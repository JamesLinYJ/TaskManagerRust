// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 进程页面
//
//   文件:       src/pages/processes/mod.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

mod actions;
mod dump;
mod model;
mod sampler;
mod tree;

// 进程页实现。
// 这里负责采集进程列表、计算每轮刷新之间的增量数据、维护排序状态，
// 并处理结束进程、调试、设置优先级和亲和性等操作。
//
// 线程模型：
//   SingleFlightWorker 工作线程在后台执行 collect_process_entries() 采样，
//   UI 线程只提交刷新意图并异步提交结果；CPU delta 基线由 worker 独占。
//   创建转储文件使用另一个 SingleFlightWorker。保存对话框留在 UI 线程，
//   MiniDumpWriteDump 在该 worker 上执行，不与采样请求合并。
//
// 缓存失效策略：
//   previous_samples (HashMap) 只在完整采样成功后由 worker 整体替换，供下轮
//   delta（CPU、内存、缺页）计算使用。失败不会推进基线。
//   DirtyColumns / DirtyRowRange 作为行/列级脏标记，避免整表重绘。
//   进程树的父子边由 worker 在同一次快照里解析；UI 线程只布局、展开和绘制。
use std::collections::{HashMap, HashSet};
use std::mem::zeroed;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    ERROR_GEN_FAILURE, ERROR_INVALID_DATA, HINSTANCE, HWND, LPARAM, POINT, RECT, SIZE, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    GetDC, GetTextExtentPoint32W, HGDIOBJ, ReleaseDC, SelectObject,
};
use windows_sys::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS,
    IDLE_PRIORITY_CLASS, REALTIME_PRIORITY_CLASS,
};
use windows_sys::Win32::UI::Controls::{
    BST_CHECKED, BST_UNCHECKED, CheckDlgButton, IsDlgButtonChecked, LVCF_FMT, LVCF_SUBITEM,
    LVCF_TEXT, LVCF_WIDTH, LVCFMT_LEFT, LVCFMT_RIGHT, LVCOLUMNW, LVIF_STATE, LVIF_TEXT,
    LVIR_BOUNDS, LVIR_LABEL, LVIS_FOCUSED, LVIS_SELECTED, LVITEMW, LVM_DELETECOLUMN,
    LVM_ENSUREVISIBLE, LVM_GETCOLUMNORDERARRAY, LVM_GETCOLUMNWIDTH, LVM_GETCOUNTPERPAGE,
    LVM_GETITEMCOUNT, LVM_GETITEMRECT, LVM_GETNEXTITEM, LVM_GETTOPINDEX, LVM_INSERTCOLUMNW,
    LVM_REDRAWITEMS, LVM_SCROLL, LVM_SETITEMCOUNT, LVM_SETITEMSTATE, LVN_COLUMNCLICK,
    LVN_GETDISPINFOW, LVN_ITEMCHANGED, LVN_KEYDOWN, LVNI_FOCUSED, LVNI_SELECTED, LVS_SHOWSELALWAYS,
    LVSICF_NOINVALIDATEALL, LVSICF_NOSCROLL, NM_CLICK, NM_DBLCLK, NMHDR, NMITEMACTIVATE,
    NMLISTVIEW, NMLVDISPINFOW, NMLVKEYDOWN,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, VK_ADD, VK_LEFT, VK_RIGHT, VK_SUBTRACT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, DeferWindowPos, EndDeferWindowPos, EndDialog, GWL_STYLE, GetClientRect,
    GetCursorPos, GetDlgItem, GetWindowLongW, IDCANCEL, IDOK, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, SendMessageW, SetWindowLongW, TPM_RETURNCMD, TrackPopupMenuEx,
    WM_COMMAND, WM_GETFONT, WM_INITDIALOG, WM_SETREDRAW,
};

use self::actions::load_debugger_path;
use self::model::{DirtyColumns, ProcEntry, column_text, compare_entries, update_process_entry};
use self::sampler::{ProcWorkerRequest, ProcWorkerResult, ProcWorkerState};
use self::tree::{
    TreeRow, expansion_ancestors, layout_process_rows, nearest_visible_identity,
    retain_live_identities, same_tree_presentation, tree_image_prefix,
};
use crate::config::options::{ColumnId, Options, UpdateSpeed};
use crate::infrastructure::native::{
    copy_text_to_callback_buffer, finish_list_view_update, get_window_userdata, loword,
    record_win32_error, set_window_userdata, subclass_list_view, to_wide_null,
    window_rect_relative_to_page,
};
use crate::infrastructure::worker::{SingleFlightWorker, replace_pending};
use crate::system::process_identity::ProcIdentity;
use crate::ui::dialogs::dialog_box;
use crate::ui::localization::{TextKey, localize_dialog, text};
use crate::ui::resource_ids::*;
use crate::ui::runtime_menu::{MenuItemState, PopupMenu};

const PROCESS_COLUMNS: [ProcessColumn; NUM_COLUMN] = [
    // 列定义和旧版 Task Manager 保持兼容，既决定标题也决定默认宽度与对齐方式。
    ProcessColumn::new(TextKey::ProcessColumnImageName, LVCFMT_LEFT, 107),
    ProcessColumn::new(TextKey::ProcessColumnPid, LVCFMT_RIGHT, 50),
    ProcessColumn::new(TextKey::ProcessColumnUserName, LVCFMT_LEFT, 107),
    ProcessColumn::new(TextKey::ProcessColumnSessionId, LVCFMT_RIGHT, 60),
    ProcessColumn::new(TextKey::ProcessColumnCpu, LVCFMT_RIGHT, 35),
    ProcessColumn::new(TextKey::ProcessColumnCpuTime, LVCFMT_RIGHT, 70),
    ProcessColumn::new(TextKey::ProcessColumnMemoryUsage, LVCFMT_RIGHT, 70),
    ProcessColumn::new(TextKey::ProcessColumnMemoryUsageDelta, LVCFMT_RIGHT, 70),
    ProcessColumn::new(TextKey::ProcessColumnPageFaults, LVCFMT_RIGHT, 70),
    ProcessColumn::new(TextKey::ProcessColumnPageFaultsDelta, LVCFMT_RIGHT, 70),
    ProcessColumn::new(TextKey::ProcessColumnVirtualMemorySize, LVCFMT_RIGHT, 70),
    ProcessColumn::new(TextKey::ProcessColumnPagedPool, LVCFMT_RIGHT, 70),
    ProcessColumn::new(TextKey::ProcessColumnNonPagedPool, LVCFMT_RIGHT, 70),
    ProcessColumn::new(TextKey::ProcessColumnBasePriority, LVCFMT_RIGHT, 60),
    ProcessColumn::new(TextKey::ProcessColumnHandleCount, LVCFMT_RIGHT, 60),
    ProcessColumn::new(TextKey::ProcessColumnThreadCount, LVCFMT_RIGHT, 60),
];

const COLUMN_DIALOG_IDS: [i32; NUM_COLUMN] = [
    // “选择列”对话框里的勾选框与列定义保持一一对应。
    IDC_IMAGENAME,
    IDC_PID,
    IDC_USERNAME,
    IDC_SESSIONID,
    IDC_CPU,
    IDC_CPUTIME,
    IDC_MEMUSAGE,
    IDC_MEMUSAGEDIFF,
    IDC_PAGEFAULTS,
    IDC_PAGEFAULTSDIFF,
    IDC_COMMITCHARGE,
    IDC_PAGEDPOOL,
    IDC_NONPAGEDPOOL,
    IDC_BASEPRIORITY,
    IDC_HANDLECOUNT,
    IDC_THREADCOUNT,
];

const DEFAULT_MARGIN: i32 = 8;

#[derive(Clone, Copy)]
struct ProcessColumn {
    // `ProcessColumn` 描述一列在 UI 层的静态元数据。
    title_key: TextKey,
    fmt: i32,
    default_width: i32,
}

impl ProcessColumn {
    const fn new(title_key: TextKey, fmt: i32, default_width: i32) -> Self {
        Self {
            title_key,
            fmt,
            default_width,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct DirtyRowRange {
    // 列表刷新时只记录真正变更的行范围，避免整表重绘。
    start: Option<usize>,
    end: usize,
}

impl DirtyRowRange {
    fn mark(&mut self, index: usize) {
        self.start = Some(self.start.map_or(index, |current| current.min(index)));
        self.end = self.end.max(index);
    }

    fn redraw_visible(self, list_hwnd: HWND, item_count: usize) {
        unsafe {
            let Some(start) = self.start else {
                return;
            };
            if item_count == 0 {
                return;
            }

            let top = SendMessageW(list_hwnd, LVM_GETTOPINDEX, 0, 0).max(0) as usize;
            let visible_count = SendMessageW(list_hwnd, LVM_GETCOUNTPERPAGE, 0, 0).max(0) as usize;
            if visible_count == 0 {
                return;
            }
            let visible_end = top
                .saturating_add(visible_count)
                .min(item_count.saturating_sub(1));
            let start = start.max(top);
            let end = self.end.min(visible_end);
            if start > end {
                return;
            }

            SendMessageW(list_hwnd, LVM_REDRAWITEMS, start, end as LPARAM);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectionScrollPolicy {
    PreserveViewport,
    RevealSelection,
}

fn refresh_selection_scroll_policy(
    requested_selection: Option<ProcIdentity>,
) -> SelectionScrollPolicy {
    if requested_selection.is_some() {
        SelectionScrollPolicy::RevealSelection
    } else {
        SelectionScrollPolicy::PreserveViewport
    }
}

#[derive(Default)]
struct ProcessStrings {
    warning: String,
    invalid_option: String,
    no_affinity_mask: String,
    kill: String,
    kill_tree: String,
    kill_tree_fail: String,
    kill_tree_fail_body: String,
    debug: String,
    prichange: String,
    cant_kill: String,
    cant_debug: String,
    cant_change_priority: String,
    cant_set_affinity: String,
    cant_open_file_location: String,
    cant_create_dump: String,
    dump_created: String,
    priority_low: String,
    priority_below_normal: String,
    priority_normal: String,
    priority_above_normal: String,
    priority_high: String,
    priority_realtime: String,
    priority_unknown: String,
}

// “选择列”对话框的上下文，通过 LPARAM 传递给 dialog proc。
struct ColumnDialogContext {
    page: *mut ProcessPageState,
    options: *mut Options,
}

// 进程优先级枚举，对应 Win32 优先级类常量。
#[derive(Clone, Copy)]
enum ProcPriority {
    Low,
    BelowNormal,
    Normal,
    AboveNormal,
    High,
    Realtime,
}

// 进程右键菜单/命令按钮支持的操作命令枚举。
#[derive(Clone, Copy)]
enum ProcCommand {
    Terminate,
    TerminateTree,
    Debug,
    OpenFileLocation,
    CreateDump,
    Affinity,
    SetPriority(ProcPriority),
    PickColumns,
    ToggleTree,
    Expand,
    Collapse,
}

/// Checked and enabled state for the process tree commands on the View menu.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcessViewCommandState {
    pub tree_mode: bool,
    pub can_expand: bool,
    pub can_collapse: bool,
}

impl ProcPriority {
    const fn command_id(self) -> u16 {
        match self {
            Self::Low => IDM_PROC_LOW,
            Self::BelowNormal => IDM_PROC_BELOWNORMAL,
            Self::Normal => IDM_PROC_NORMAL,
            Self::AboveNormal => IDM_PROC_ABOVENORMAL,
            Self::High => IDM_PROC_HIGH,
            Self::Realtime => IDM_PROC_REALTIME,
        }
    }

    const fn text_key(self) -> TextKey {
        match self {
            Self::Low => TextKey::Low,
            Self::BelowNormal => TextKey::BelowNormal,
            Self::Normal => TextKey::Normal,
            Self::AboveNormal => TextKey::AboveNormal,
            Self::High => TextKey::High,
            Self::Realtime => TextKey::Realtime,
        }
    }
}

impl ProcCommand {
    const fn command_id(self) -> u16 {
        match self {
            Self::Terminate => IDM_PROC_TERMINATE,
            Self::TerminateTree => IDM_PROC_ENDTREE,
            Self::Debug => IDM_PROC_DEBUG,
            Self::OpenFileLocation => IDM_PROC_OPENFILELOCATION,
            Self::CreateDump => IDM_PROC_DUMP,
            Self::Affinity => IDM_AFFINITY,
            Self::SetPriority(priority) => priority.command_id(),
            Self::PickColumns => IDM_PROCCOLS,
            Self::ToggleTree => IDM_PROC_TREE,
            Self::Expand => IDM_PROC_EXPAND,
            Self::Collapse => IDM_PROC_COLLAPSE,
        }
    }

    fn from_command_id(command_id: u16, terminate_button_id: u16) -> Option<Self> {
        match command_id {
            id if id == terminate_button_id || id == IDM_PROC_TERMINATE => Some(Self::Terminate),
            IDM_PROC_ENDTREE => Some(Self::TerminateTree),
            id if id == IDC_DEBUG as u16 || id == IDM_PROC_DEBUG => Some(Self::Debug),
            IDM_PROC_OPENFILELOCATION => Some(Self::OpenFileLocation),
            IDM_PROC_DUMP => Some(Self::CreateDump),
            IDM_AFFINITY => Some(Self::Affinity),
            IDM_PROC_LOW => Some(Self::SetPriority(ProcPriority::Low)),
            IDM_PROC_BELOWNORMAL => Some(Self::SetPriority(ProcPriority::BelowNormal)),
            IDM_PROC_NORMAL => Some(Self::SetPriority(ProcPriority::Normal)),
            IDM_PROC_ABOVENORMAL => Some(Self::SetPriority(ProcPriority::AboveNormal)),
            IDM_PROC_HIGH => Some(Self::SetPriority(ProcPriority::High)),
            IDM_PROC_REALTIME => Some(Self::SetPriority(ProcPriority::Realtime)),
            IDM_PROCCOLS => Some(Self::PickColumns),
            IDM_PROC_TREE => Some(Self::ToggleTree),
            IDM_PROC_EXPAND => Some(Self::Expand),
            IDM_PROC_COLLAPSE => Some(Self::Collapse),
            _ => None,
        }
    }
}

pub struct ProcessPageState {
    // 进程页状态对象持有采样缓存、排序设置、列配置以及当前选中项。
    hinstance: HINSTANCE,
    hwnd_page: HWND,
    main_hwnd: HWND,
    entries: Vec<ProcEntry>,
    visible_rows: Vec<TreeRow>,
    /// Parents the user has folded. Absent parents stay open, including ones
    /// that appear on a later refresh. Session-only; not stored in `Options`.
    collapsed: HashSet<ProcIdentity>,
    /// Session-only. The flat list is still available from the View menu.
    /// This is not stored in `Options`.
    tree_mode: bool,
    active_columns: Vec<ColumnId>,
    selected_identity: Option<ProcIdentity>,
    pending_find_identity: Option<ProcIdentity>,
    sort_column: ColumnId,
    sort_direction: i32,
    paused: bool,
    confirmations: bool,
    no_title: bool,
    processor_count: usize,
    debugger_path: Option<String>,
    debugger_error: Option<u32>,
    strings: ProcessStrings,
    pass_count: u64,
    worker: Option<SingleFlightWorker<ProcWorkerRequest, ProcWorkerResult>>,
    dump_worker: Option<SingleFlightWorker<dump::ProcessDumpRequest, dump::ProcessDumpReport>>,
    last_refresh_error: Option<u32>,
    last_row_error: Option<u32>,
}

impl Default for ProcessPageState {
    fn default() -> Self {
        Self {
            hinstance: null_mut(),
            hwnd_page: null_mut(),
            main_hwnd: null_mut(),
            entries: Vec::with_capacity(128),
            visible_rows: Vec::with_capacity(128),
            collapsed: HashSet::new(),
            tree_mode: true,
            active_columns: Vec::with_capacity(NUM_COLUMN),
            selected_identity: None,
            pending_find_identity: None,
            sort_column: ColumnId::Pid,
            sort_direction: 1,
            paused: false,
            confirmations: true,
            no_title: false,
            processor_count: 1,
            debugger_path: None,
            debugger_error: None,
            strings: ProcessStrings::default(),
            pass_count: 0,
            worker: None,
            dump_worker: None,
            last_refresh_error: None,
            last_row_error: None,
        }
    }
}

impl ProcessPageState {
    // 创建持久采样线程。线程、通道和单在途语义由 SingleFlightWorker 统一负责。
    fn start_worker_thread(&mut self) -> Result<(), u32> {
        if self.worker.is_some() {
            return Ok(());
        }
        self.worker = Some(SingleFlightWorker::spawn_initialized(
            "taskmgr-rs-process-sampler",
            PWM_PROC_WORKER_COMPLETE,
            replace_pending,
            || {
                let mut state = ProcWorkerState::default();
                move |request| state.collect(request)
            },
        )?);
        Ok(())
    }

    fn stop_worker_thread(&mut self) {
        self.worker = None;
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub fn no_title(&self) -> bool {
        self.no_title
    }

    pub fn initialize(
        &mut self,
        hinstance: HINSTANCE,
        hwnd_page: HWND,
        main_hwnd: HWND,
    ) -> Result<(), u32> {
        unsafe {
            // 进程页初始化主要做三件事：
            // 加载文案、准备调试器路径、并把 ListView 切到更适合频繁刷新的显示模式。
            self.hinstance = hinstance;
            self.hwnd_page = hwnd_page;
            self.main_hwnd = main_hwnd;
            self.load_strings();
            match load_debugger_path() {
                Ok(path) => {
                    self.debugger_path = path;
                    self.debugger_error = None;
                }
                Err(error) => {
                    self.debugger_path = None;
                    self.debugger_error = Some(error);
                    record_win32_error("debugger configuration", error);
                }
            }

            let list_hwnd = self.list_hwnd();
            if list_hwnd.is_null() {
                return Err(windows_sys::Win32::Foundation::ERROR_INVALID_WINDOW_HANDLE);
            }
            self.start_worker_thread()?;
            self.start_dump_worker()?;
            subclass_list_view(list_hwnd);
            let current_style = GetWindowLongW(list_hwnd, GWL_STYLE) as u32;
            SetWindowLongW(
                list_hwnd,
                GWL_STYLE,
                (current_style | LVS_SHOWSELALWAYS) as i32,
            );
            self.update_ui_state();
            Ok(())
        }
    }

    pub fn apply_options(&mut self, options: &Options, processor_count: usize) {
        // 进程页的选项既影响行为，也影响列结构。
        // 当列配置发生变化时，直接重建列和数据比做局部修补更可靠。
        self.no_title = options.no_title();
        self.confirmations = options.confirmations();
        self.processor_count = processor_count.max(1);

        let desired_columns = columns_from_options(options);
        if desired_columns != self.active_columns {
            self.active_columns = desired_columns;
            let visible_columns = DirtyColumns::from_columns(&self.active_columns);
            for entry in &mut self.entries {
                entry.rebuild_display_columns(&self.active_columns);
                entry.dirty_columns = visible_columns;
            }
            self.setup_columns(options);
        }
    }

    pub fn timer_event(&mut self, options: &Options, force: bool) {
        // 每一轮刷新都走“采样 -> 合并旧状态 -> 排序/重绘”这条统一链路。
        self.paused = options.update_speed == UpdateSpeed::Paused as i32;
        if force || !self.paused {
            self.refresh_processes();
        }
    }

    pub fn deactivate(&mut self, options: &mut Options) {
        if let Err(error) = self.save_column_layout(options) {
            record_win32_error("process column layout persistence", error);
        }
    }

    pub fn destroy(&mut self) {
        self.stop_worker_thread();
        self.stop_dump_worker();
        self.entries.clear();
        self.visible_rows.clear();
        self.collapsed.clear();
    }

    /// Handles a notification forwarded by the process page dialog procedure.
    ///
    /// # Safety
    ///
    /// `lparam` must be the live `WM_NOTIFY` payload for this page's ListView. The payload must
    /// have the structure implied by its `NMHDR::code` and remain readable (and, for
    /// `LVN_GETDISPINFOW`, writable) for the duration of this synchronous call.
    pub unsafe fn handle_notify(&mut self, lparam: LPARAM) -> isize {
        unsafe {
            // ListView 处于 owner-data 风格，因此文本、排序和选择同步都靠通知消息驱动。
            let Some(notify_header) = (lparam as *const NMHDR).as_ref() else {
                return 0;
            };
            if notify_header.idFrom != IDC_PROCLIST as usize
                || notify_header.hwndFrom != self.list_hwnd()
            {
                return 0;
            }
            match notify_header.code {
                code if code == LVN_GETDISPINFOW => {
                    let display_info = &mut *(lparam as *mut NMLVDISPINFOW);
                    self.fill_display_info(&mut display_info.item);
                    1
                }
                code if code == LVN_ITEMCHANGED => {
                    let notify = &*(lparam as *const NMLISTVIEW);
                    if (notify.uChanged & LVIF_STATE) != 0 {
                        self.selected_identity = self.current_selected_identity();
                        self.update_ui_state();
                    }
                    1
                }
                code if code == NM_CLICK || code == NM_DBLCLK => {
                    let activate = &*(lparam as *const NMITEMACTIVATE);
                    self.handle_tree_click(
                        activate.iItem,
                        activate.iSubItem,
                        activate.ptAction.x,
                        code == NM_DBLCLK,
                    );
                    0
                }
                code if code == LVN_KEYDOWN => {
                    let key = &*(lparam as *const NMLVKEYDOWN);
                    self.handle_tree_key(key.wVKey);
                    0
                }
                code if code == LVN_COLUMNCLICK => {
                    let notify = &*(lparam as *const NMLISTVIEW);
                    let clicked = self
                        .active_columns
                        .get(notify.iSubItem as usize)
                        .copied()
                        .unwrap_or(ColumnId::Pid);
                    if self.sort_column == clicked {
                        self.sort_direction *= -1;
                    } else {
                        self.sort_column = clicked;
                        self.sort_direction = -1;
                    }
                    self.selected_identity =
                        self.current_selected_identity().or(self.selected_identity);
                    self.resort_entries();
                    self.rebuild_listview(SelectionScrollPolicy::PreserveViewport);
                    self.refresh_processes();
                    1
                }
                _ => 0,
            }
        }
    }

    // 将命令 ID 分派到具体的进程操作（结束、调试、优先级、亲和性等）。
    pub fn handle_command(&mut self, command_id: u16, options: Option<&mut Options>) {
        let Some(command) = ProcCommand::from_command_id(command_id, IDC_TERMINATE as u16) else {
            return;
        };

        match command {
            ProcCommand::PickColumns => {
                if let Some(options) = options {
                    self.pick_columns(options);
                }
            }
            ProcCommand::Terminate => {
                if let Some(identity) = self.current_selected_identity() {
                    self.kill_process(identity);
                }
            }
            ProcCommand::TerminateTree => {
                if let Some(identity) = self.current_selected_identity() {
                    self.kill_process_tree(identity);
                }
            }
            ProcCommand::Debug => {
                if let Some(identity) = self.current_selected_identity() {
                    self.attach_debugger(identity);
                }
            }
            ProcCommand::OpenFileLocation => {
                if let Some(identity) = self.current_selected_identity() {
                    self.open_file_location(identity);
                }
            }
            ProcCommand::CreateDump => {
                if let Some(identity) = self.current_selected_identity() {
                    self.create_dump_file(identity);
                }
            }
            ProcCommand::Affinity => {
                if let Some(identity) = self.current_selected_identity() {
                    self.set_affinity(identity);
                }
            }
            ProcCommand::SetPriority(priority) => {
                if let Some(identity) = self.current_selected_identity() {
                    self.set_priority(identity, priority);
                }
            }
            ProcCommand::ToggleTree => {
                self.tree_mode = !self.tree_mode;
                self.rebuild_listview(SelectionScrollPolicy::PreserveViewport);
            }
            ProcCommand::Expand => {
                if let Some(identity) = self.current_selected_identity().or(self.selected_identity)
                {
                    self.set_row_expanded(identity, true);
                }
            }
            ProcCommand::Collapse => {
                if let Some(identity) = self.current_selected_identity().or(self.selected_identity)
                {
                    self.set_row_expanded(identity, false);
                }
            }
        }
    }

    pub fn show_context_menu(&mut self, x: i32, y: i32) {
        unsafe {
            // 右键菜单会按当前选中进程和系统能力动态裁剪。
            self.selected_identity = self.current_selected_identity();
            let Some(entry) = self.selected_entry() else {
                return;
            };

            let popup = match self.build_context_menu(entry) {
                Ok(popup) => popup,
                Err(error) => {
                    record_win32_error("process popup menu creation", error);
                    return;
                }
            };

            self.paused = true;
            let mut cursor = POINT { x, y };
            if cursor.x == -1 && cursor.y == -1 {
                GetCursorPos(&mut cursor);
            }

            SendMessageW(self.main_hwnd, crate::ui::resource_ids::PWM_INPOPUP, 1, 0);
            let command = TrackPopupMenuEx(
                popup.as_raw(),
                TPM_RETURNCMD,
                cursor.x,
                cursor.y,
                self.hwnd_page,
                null(),
            );
            SendMessageW(self.main_hwnd, crate::ui::resource_ids::PWM_INPOPUP, 0, 0);
            self.paused = false;

            if command != 0 {
                self.handle_command(command as u16, None);
            }
        }
    }

    // 构造进程右键菜单，包含结束进程、调试、打开文件位置、优先级和亲和性子菜单。
    fn build_context_menu(&self, entry: &ProcEntry) -> Result<PopupMenu, u32> {
        let identity_verified = entry.identity.is_verified();
        let mut priority_menu = PopupMenu::new()?;
        let checked_priority = match entry.priority_class {
            value if value == IDLE_PRIORITY_CLASS => ProcPriority::Low.command_id(),
            value if value == BELOW_NORMAL_PRIORITY_CLASS => ProcPriority::BelowNormal.command_id(),
            value if value == ABOVE_NORMAL_PRIORITY_CLASS => ProcPriority::AboveNormal.command_id(),
            value if value == HIGH_PRIORITY_CLASS => ProcPriority::High.command_id(),
            value if value == REALTIME_PRIORITY_CLASS => ProcPriority::Realtime.command_id(),
            _ => ProcPriority::Normal.command_id(),
        };
        for priority in [
            ProcPriority::Realtime,
            ProcPriority::High,
            ProcPriority::AboveNormal,
            ProcPriority::Normal,
            ProcPriority::BelowNormal,
            ProcPriority::Low,
        ] {
            priority_menu.append_item(
                priority.command_id(),
                text(priority.text_key()),
                if !identity_verified {
                    MenuItemState {
                        enabled: false,
                        checked: priority.command_id() == checked_priority,
                    }
                } else if priority.command_id() == checked_priority {
                    MenuItemState::checked()
                } else {
                    MenuItemState::ENABLED
                },
            )?;
        }

        let mut popup = PopupMenu::new()?;
        for (command, label_key, state) in [
            (
                ProcCommand::Terminate,
                TextKey::EndProcess,
                if identity_verified {
                    MenuItemState::ENABLED
                } else {
                    MenuItemState::disabled()
                },
            ),
            (
                ProcCommand::TerminateTree,
                TextKey::EndProcessTree,
                if identity_verified {
                    MenuItemState::ENABLED
                } else {
                    MenuItemState::disabled()
                },
            ),
            (
                ProcCommand::OpenFileLocation,
                TextKey::OpenFileLocation,
                if identity_verified {
                    MenuItemState::ENABLED
                } else {
                    MenuItemState::disabled()
                },
            ),
            (
                ProcCommand::CreateDump,
                TextKey::CreateDumpFile,
                if identity_verified {
                    MenuItemState::ENABLED
                } else {
                    MenuItemState::disabled()
                },
            ),
            (
                ProcCommand::Debug,
                TextKey::Debug,
                if identity_verified && self.debugger_path.is_some() {
                    MenuItemState::ENABLED
                } else {
                    MenuItemState::disabled()
                },
            ),
        ] {
            popup.append_item(command.command_id(), text(label_key), state)?;
        }

        popup.append_separator()?;
        let view_state = self.view_command_state();
        popup.append_item(
            ProcCommand::Expand.command_id(),
            text(TextKey::ExpandProcess),
            if view_state.can_expand {
                MenuItemState::ENABLED
            } else {
                MenuItemState::disabled()
            },
        )?;
        popup.append_item(
            ProcCommand::Collapse.command_id(),
            text(TextKey::CollapseProcess),
            if view_state.can_collapse {
                MenuItemState::ENABLED
            } else {
                MenuItemState::disabled()
            },
        )?;
        popup.append_separator()?;
        popup.append_submenu(text(TextKey::SetPriority), priority_menu)?;

        if self.processor_count > 1 {
            popup.append_item(
                ProcCommand::Affinity.command_id(),
                text(TextKey::SetAffinity),
                if identity_verified {
                    MenuItemState::ENABLED
                } else {
                    MenuItemState::disabled()
                },
            )?;
        }

        Ok(popup)
    }

    pub fn size_page(&self) {
        unsafe {
            // 进程页布局以“列表吃掉剩余空间，按钮贴右下角”为核心规则。
            let mut parent_rect = zeroed::<RECT>();
            GetClientRect(self.hwnd_page, &mut parent_rect);

            let mut hdwp = BeginDeferWindowPos(10);
            if hdwp.is_null() {
                return;
            }

            let terminate_hwnd = GetDlgItem(self.hwnd_page, IDC_TERMINATE);
            let list_hwnd = self.list_hwnd();
            if terminate_hwnd.is_null() || list_hwnd.is_null() {
                EndDeferWindowPos(hdwp);
                return;
            }

            let terminate_rect = window_rect_relative_to_page(terminate_hwnd, self.hwnd_page);
            let dx = (parent_rect.right - DEFAULT_MARGIN * 2) - terminate_rect.right;
            let dy = (parent_rect.bottom - DEFAULT_MARGIN * 2) - terminate_rect.bottom;

            hdwp = DeferWindowPos(
                hdwp,
                terminate_hwnd,
                null_mut(),
                terminate_rect.left + dx,
                terminate_rect.top + dy,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );

            let list_rect = window_rect_relative_to_page(list_hwnd, self.hwnd_page);
            hdwp = DeferWindowPos(
                hdwp,
                list_hwnd,
                null_mut(),
                0,
                0,
                (terminate_rect.right - list_rect.left + dx).max(0),
                (terminate_rect.top - list_rect.top + dy - DEFAULT_MARGIN).max(0),
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );

            EndDeferWindowPos(hdwp);
        }
    }

    // 从进程页跳转到指定 PID 的行并高亮选中。由任务页的“转到进程”命令触发。
    pub fn find_process(&mut self, identity: ProcIdentity) -> bool {
        if !identity.is_verified() {
            return false;
        }
        let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.identity == identity)
        else {
            self.pending_find_identity = Some(identity);
            self.refresh_processes();
            return true;
        };

        self.expand_ancestors_of(identity);
        self.selected_identity = Some(self.entries[index].identity);
        self.rebuild_listview(SelectionScrollPolicy::RevealSelection);
        true
    }

    fn list_hwnd(&self) -> HWND {
        unsafe { GetDlgItem(self.hwnd_page, IDC_PROCLIST) }
    }

    fn load_strings(&mut self) {
        // 常用错误文案和优先级文本在这里集中缓存，避免命令执行路径上反复查资源。
        self.strings.warning = text(TextKey::WarningTitle).to_string();
        self.strings.invalid_option = text(TextKey::InvalidOptionTitle).to_string();
        self.strings.no_affinity_mask = text(TextKey::NoAffinityMaskMessage).to_string();
        self.strings.kill = text(TextKey::KillProcessWarning).to_string();
        self.strings.kill_tree = text(TextKey::KillProcessTreePrompt).to_string();
        self.strings.kill_tree_fail = text(TextKey::KillProcessTreeFailed).to_string();
        self.strings.kill_tree_fail_body = text(TextKey::KillProcessTreeFailedBody).to_string();
        self.strings.debug = text(TextKey::DebugProcessWarning).to_string();
        self.strings.prichange = text(TextKey::PriorityChangeWarning).to_string();
        self.strings.cant_kill = text(TextKey::UnableToTerminateProcess).to_string();
        self.strings.cant_debug = text(TextKey::UnableToAttachDebugger).to_string();
        self.strings.cant_change_priority = text(TextKey::UnableToChangePriority).to_string();
        self.strings.cant_set_affinity = text(TextKey::UnableToSetAffinity).to_string();
        self.strings.cant_open_file_location = text(TextKey::UnableToOpenFileLocation).to_string();
        self.strings.cant_create_dump = text(TextKey::UnableToCreateDumpFile).to_string();
        self.strings.dump_created = text(TextKey::DumpFileCreated).to_string();
        self.strings.priority_low = text(TextKey::Low).to_string();
        self.strings.priority_below_normal = text(TextKey::BelowNormal).to_string();
        self.strings.priority_normal = text(TextKey::Normal).to_string();
        self.strings.priority_above_normal = text(TextKey::AboveNormal).to_string();
        self.strings.priority_high = text(TextKey::High).to_string();
        self.strings.priority_realtime = text(TextKey::Realtime).to_string();
        self.strings.priority_unknown = text(TextKey::Unknown).to_string();
    }

    fn update_ui_state(&self) {
        unsafe {
            // 当前实现里只有“结束进程”按钮依赖选择状态，
            // 但统一收口在这里，后续扩展其它按钮更容易。
            let has_selection = self
                .current_selected_identity()
                .is_some_and(ProcIdentity::is_verified);
            let terminate_button = GetDlgItem(self.hwnd_page, IDC_TERMINATE);
            if !terminate_button.is_null() {
                EnableWindow(terminate_button, i32::from(has_selection));
            }
        }
    }

    fn refresh_processes(&mut self) {
        self.drain_worker_results();
        self.schedule_process_collection();
    }

    fn schedule_process_collection(&mut self) {
        let Some(worker) = self.worker.as_mut() else {
            self.set_refresh_error(windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE);
            return;
        };
        let request = ProcWorkerRequest {
            processor_count: self.processor_count,
        };
        if let Err(error) = worker.request(request, self.hwnd_page) {
            self.set_refresh_error(error);
        }
    }

    fn drain_worker_results(&mut self) {
        let drain = match self.worker.as_mut() {
            Some(worker) => worker.drain(self.hwnd_page),
            None => return,
        };
        for completion in drain.completions {
            crate::infrastructure::diagnostics::with_operation_id(completion.operation_id, || {
                self.apply_worker_completion(completion.value)
            });
        }
        if let Some(error) = drain.error {
            self.set_refresh_error(error);
        }
    }

    fn apply_worker_completion(&mut self, completion: ProcWorkerResult) {
        match completion {
            Ok(snapshot) => {
                self.last_refresh_error = None;
                if self.last_row_error != snapshot.row_error
                    && let Some(error) = snapshot.row_error
                {
                    record_win32_error("process row metadata", error);
                }
                self.last_row_error = snapshot.row_error;
                self.apply_process_snapshot(snapshot.entries);
            }
            Err(error) => {
                self.set_refresh_error(error);
            }
        }
    }

    fn apply_process_snapshot(&mut self, entries: Vec<ProcEntry>) {
        let previous_selection = self.current_selected_identity().or(self.selected_identity);
        let current_pass = self.pass_count;
        let visible_columns = DirtyColumns::from_columns(&self.active_columns);
        let mut sort_dirty = false;
        let mut existing_by_identity = HashMap::with_capacity(self.entries.len());
        for (index, entry) in self.entries.iter_mut().enumerate() {
            existing_by_identity.insert(entry.identity, index);
        }

        for snapshot in entries {
            if let Some(&index) = existing_by_identity.get(&snapshot.identity) {
                let existing = &mut self.entries[index];
                let changed =
                    update_process_entry(existing, &snapshot, current_pass, visible_columns);
                sort_dirty |= changed.contains(self.sort_column);
            } else {
                self.entries.push(snapshot.with_pass_count(
                    current_pass,
                    &self.active_columns,
                    visible_columns,
                ));
                sort_dirty = true;
            }
        }

        sort_dirty |= self.remove_stale_entries(current_pass);
        if sort_dirty {
            self.resort_entries();
        }
        let requested_selection = self
            .pending_find_identity
            .take()
            .filter(|identity| self.entries.iter().any(|entry| entry.identity == *identity));
        if let Some(identity) = requested_selection {
            self.expand_ancestors_of(identity);
        }
        let selection_scroll_policy = refresh_selection_scroll_policy(requested_selection);
        self.selected_identity = requested_selection.or(previous_selection);
        self.rebuild_listview(selection_scroll_policy);
        self.pass_count = self.pass_count.wrapping_add(1);
    }

    fn set_refresh_error(&mut self, error: u32) {
        if self.last_refresh_error != Some(error) {
            record_win32_error("process refresh", error);
        }
        self.last_refresh_error = Some(error);
    }

    pub fn handle_worker_completion(&mut self) {
        self.drain_worker_results();
    }

    // 按当前排序列和方向重排 entries；文本列直接比较预先缓存的小写字符串。
    fn resort_entries(&mut self) {
        self.entries.sort_by(|left, right| {
            compare_entries(left, right, self.sort_column, self.sort_direction)
        });
    }

    fn remove_stale_entries(&mut self, current_pass: u64) -> bool {
        let previous_len = self.entries.len();
        self.entries
            .retain(|entry| entry.pass_count == current_pass);
        previous_len != self.entries.len()
    }

    fn rebuild_listview(&mut self, selection_scroll_policy: SelectionScrollPolicy) {
        let next_rows = self.layout_visible_rows();
        let list_hwnd = self.list_hwnd();
        if list_hwnd.is_null() {
            self.publish_visible_rows(next_rows);
            return;
        }
        unsafe {
            self.present_visible_rows(list_hwnd, next_rows, selection_scroll_policy);
        }
    }

    fn layout_visible_rows(&mut self) -> Vec<TreeRow> {
        let identities: Vec<ProcIdentity> =
            self.entries.iter().map(|entry| entry.identity).collect();
        let parents: Vec<Option<ProcIdentity>> =
            self.entries.iter().map(|entry| entry.parent).collect();
        let live = identities.iter().copied().collect::<HashSet<_>>();
        retain_live_identities(&mut self.collapsed, &live);
        layout_process_rows(&identities, &parents, &self.collapsed, self.tree_mode)
    }

    fn publish_visible_rows(&mut self, next_rows: Vec<TreeRow>) {
        self.selected_identity = self.retarget_visible_identity(self.selected_identity, &next_rows);
        self.visible_rows = next_rows;
    }

    fn retarget_visible_identity(
        &self,
        identity: Option<ProcIdentity>,
        rows: &[TreeRow],
    ) -> Option<ProcIdentity> {
        let identity = identity?;
        if !self.entries.iter().any(|entry| entry.identity == identity) {
            return None;
        }
        let visible = rows.iter().map(|row| row.identity).collect::<HashSet<_>>();
        let identities: Vec<_> = self.entries.iter().map(|entry| entry.identity).collect();
        let parents: Vec<_> = self.entries.iter().map(|entry| entry.parent).collect();
        nearest_visible_identity(identity, &identities, &parents, &visible)
    }

    /// Presents an already computed row order on the virtual list.
    ///
    /// # Safety
    ///
    /// `list_hwnd` must be this page's live process ListView.
    unsafe fn present_visible_rows(
        &mut self,
        list_hwnd: HWND,
        next_rows: Vec<TreeRow>,
        selection_scroll_policy: SelectionScrollPolicy,
    ) {
        unsafe {
            // 进程列表使用 LVS_OWNERDATA。树形刷新只改虚拟项数量和可见身份，
            // 不在 UI 线程打开进程。
            let anchor = self.identity_at_top(list_hwnd);
            let focused = self.identity_for_list_state(LVNI_FOCUSED);
            let presentation_changed = !same_tree_presentation(&self.visible_rows, &next_rows);
            let existing_count = SendMessageW(list_hwnd, LVM_GETITEMCOUNT, 0, 0) as usize;
            let count_changed = existing_count != next_rows.len();
            let bulk_update = presentation_changed || count_changed;
            self.selected_identity =
                self.retarget_visible_identity(self.selected_identity, &next_rows);
            let selected_identity = self.selected_identity;
            let focused_identity = focused
                .filter(|identity| next_rows.iter().any(|row| row.identity == *identity))
                .or(selected_identity);

            if bulk_update {
                SendMessageW(list_hwnd, WM_SETREDRAW, 0, 0);
            }

            let mut dirty_entries = HashSet::new();
            for (index, entry) in self.entries.iter_mut().enumerate() {
                if entry.dirty_columns.any() {
                    entry.dirty_columns = DirtyColumns::default();
                    dirty_entries.insert(index);
                }
            }

            self.visible_rows = next_rows;
            if count_changed {
                SendMessageW(
                    list_hwnd,
                    LVM_SETITEMCOUNT,
                    self.visible_rows.len(),
                    (LVSICF_NOINVALIDATEALL | LVSICF_NOSCROLL) as LPARAM,
                );
            }

            let selected_index =
                selected_identity.and_then(|identity| self.visible_index(identity));
            let focused_index = focused_identity.and_then(|identity| self.visible_index(identity));
            if bulk_update {
                // Dynamic sort keys can move the selected process on every sample. Refreshes must
                // preserve the user's wheel position; only an explicit navigation may reveal it.
                self.set_list_selection(
                    list_hwnd,
                    selected_index,
                    focused_index,
                    selection_scroll_policy,
                );
                if selection_scroll_policy == SelectionScrollPolicy::PreserveViewport
                    && let Some(anchor) = anchor
                {
                    self.scroll_identity_to_top(list_hwnd, anchor);
                }
                finish_list_view_update(list_hwnd);
            } else {
                let mut dirty_rows = DirtyRowRange::default();
                for (visible_index, row) in self.visible_rows.iter().enumerate() {
                    if dirty_entries.contains(&row.entry_index) {
                        dirty_rows.mark(visible_index);
                    }
                }
                dirty_rows.redraw_visible(list_hwnd, self.visible_rows.len());
            }

            self.selected_identity = selected_identity.filter(|_| selected_index.is_some());
            self.update_ui_state();
        }
    }

    fn visible_index(&self, identity: ProcIdentity) -> Option<usize> {
        self.visible_rows
            .iter()
            .position(|row| row.identity == identity)
    }

    fn identity_at_top(&self, list_hwnd: HWND) -> Option<ProcIdentity> {
        unsafe {
            if list_hwnd.is_null() || self.visible_rows.is_empty() {
                return None;
            }
            let top = SendMessageW(list_hwnd, LVM_GETTOPINDEX, 0, 0);
            if top < 0 {
                return None;
            }
            self.visible_rows.get(top as usize).map(|row| row.identity)
        }
    }

    fn scroll_identity_to_top(&self, list_hwnd: HWND, identity: ProcIdentity) {
        let Some(index) = self.visible_index(identity) else {
            return;
        };
        unsafe {
            let top = SendMessageW(list_hwnd, LVM_GETTOPINDEX, 0, 0).max(0) as usize;
            if top == index {
                return;
            }
            let mut rect = RECT {
                left: LVIR_BOUNDS as i32,
                ..zeroed()
            };
            if SendMessageW(
                list_hwnd,
                LVM_GETITEMRECT,
                0,
                &mut rect as *mut RECT as LPARAM,
            ) == 0
            {
                SendMessageW(list_hwnd, LVM_ENSUREVISIBLE, index, 0);
                return;
            }
            let height = (rect.bottom - rect.top).max(1);
            let delta = (index as i32 - top as i32).saturating_mul(height);
            SendMessageW(list_hwnd, LVM_SCROLL, 0, delta as LPARAM);
        }
    }

    fn set_list_selection(
        &self,
        list_hwnd: HWND,
        selected_index: Option<usize>,
        focused_index: Option<usize>,
        scroll_policy: SelectionScrollPolicy,
    ) {
        unsafe {
            // Clearing all virtual item states is one ListView operation instead of an O(n) loop.
            let mut item = LVITEMW {
                stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                state: 0,
                ..zeroed()
            };
            SendMessageW(
                list_hwnd,
                LVM_SETITEMSTATE,
                usize::MAX,
                &mut item as *mut _ as LPARAM,
            );

            if let Some(index) = selected_index {
                let focus_here = focused_index.is_none_or(|focused| focused == index);
                item.state = if focus_here {
                    LVIS_SELECTED | LVIS_FOCUSED
                } else {
                    LVIS_SELECTED
                };
                SendMessageW(
                    list_hwnd,
                    LVM_SETITEMSTATE,
                    index,
                    &mut item as *mut _ as LPARAM,
                );
                if scroll_policy == SelectionScrollPolicy::RevealSelection {
                    SendMessageW(list_hwnd, LVM_ENSUREVISIBLE, index, 0);
                }
            }
            if let Some(index) = focused_index.filter(|index| Some(*index) != selected_index) {
                item.stateMask = LVIS_FOCUSED;
                item.state = LVIS_FOCUSED;
                SendMessageW(
                    list_hwnd,
                    LVM_SETITEMSTATE,
                    index,
                    &mut item as *mut _ as LPARAM,
                );
            }
        }
    }

    /// Fills an `LVN_GETDISPINFOW` callback buffer.
    ///
    /// # Safety
    ///
    /// When `LVIF_TEXT` is set, `item.pszText` must be writable for at least
    /// `item.cchTextMax` UTF-16 code units and remain exclusively available for this call.
    unsafe fn fill_display_info(&self, item: &mut LVITEMW) {
        unsafe {
            if (item.mask & LVIF_TEXT) == 0
                || item.iItem < 0
                || item.pszText.is_null()
                || item.cchTextMax <= 0
            {
                return;
            }

            let row = self.visible_rows.get(item.iItem as usize).copied();
            let entry = row.and_then(|row| self.entries.get(row.entry_index));
            let Some(entry) = entry else {
                *item.pszText = 0;
                return;
            };
            let Some(column_id) = self.active_columns.get(item.iSubItem as usize).copied() else {
                *item.pszText = 0;
                return;
            };

            if self.tree_mode
                && column_id == ColumnId::ImageName
                && let Some(row) = row
            {
                let prefix = tree_image_prefix(&row);
                if !prefix.is_empty() {
                    let presented =
                        format!("{prefix}{}", column_text(entry, column_id, &self.strings));
                    copy_text_to_callback_buffer(
                        item.pszText,
                        item.cchTextMax as usize,
                        &presented,
                    );
                    return;
                }
            }
            let text = column_text(entry, column_id, &self.strings);
            copy_text_to_callback_buffer(item.pszText, item.cchTextMax as usize, text);
        }
    }

    // 销毁现有列并按照 active_columns 重建所有列头。列宽优先从 options 读取，否则用默认值。
    fn setup_columns(&self, options: &Options) {
        unsafe {
            let list_hwnd = self.list_hwnd();
            while SendMessageW(list_hwnd, LVM_DELETECOLUMN, 0, 0) != 0 {}

            for (index, column_id) in self.active_columns.iter().enumerate() {
                let column = PROCESS_COLUMNS[*column_id as usize];
                let width = options
                    .column_widths
                    .get(index)
                    .copied()
                    .filter(|value| *value > 0)
                    .unwrap_or(column.default_width);
                let title = text(column.title_key).to_string();
                let mut title_wide = to_wide_null(&title);
                let mut lv_column = LVCOLUMNW {
                    mask: LVCF_FMT | LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM,
                    fmt: column.fmt,
                    cx: width,
                    pszText: title_wide.as_mut_ptr(),
                    cchTextMax: title_wide.len() as i32,
                    iSubItem: index as i32,
                    ..zeroed()
                };
                SendMessageW(
                    list_hwnd,
                    LVM_INSERTCOLUMNW,
                    index,
                    &mut lv_column as *mut _ as LPARAM,
                );
            }

            if !self.visible_rows.is_empty() {
                SendMessageW(
                    list_hwnd,
                    LVM_REDRAWITEMS,
                    0,
                    self.visible_rows.len().saturating_sub(1) as LPARAM,
                );
            }
        }
    }

    fn save_column_layout(&self, options: &mut Options) -> Result<(), u32> {
        unsafe {
            let column_count = self.active_columns.len();
            if column_count == 0 {
                return Err(ERROR_INVALID_DATA);
            }

            let list = self.list_hwnd();
            let mut display_order = vec![0i32; column_count];
            if SendMessageW(
                list,
                LVM_GETCOLUMNORDERARRAY,
                column_count,
                display_order.as_mut_ptr() as LPARAM,
            ) == 0
            {
                return Err(ERROR_GEN_FAILURE);
            }

            let ordered_columns = reorder_process_columns(&self.active_columns, &display_order)
                .ok_or(ERROR_INVALID_DATA)?;
            let widths = self
                .active_columns
                .iter()
                .enumerate()
                .map(|(index, column)| {
                    (
                        *column as i32,
                        SendMessageW(list, LVM_GETCOLUMNWIDTH, index, 0) as i32,
                    )
                })
                .collect::<HashMap<_, _>>();
            write_process_column_layout(options, &ordered_columns, &widths);
            Ok(())
        }
    }

    fn current_selected_identity(&self) -> Option<ProcIdentity> {
        self.identity_for_list_state(LVNI_SELECTED)
    }

    fn identity_for_list_state(&self, state: u32) -> Option<ProcIdentity> {
        unsafe {
            let list_hwnd = self.list_hwnd();
            if list_hwnd.is_null() {
                return None;
            }
            let index = SendMessageW(list_hwnd, LVM_GETNEXTITEM, usize::MAX, state as LPARAM);
            if index < 0 {
                return None;
            }
            self.visible_rows
                .get(index as usize)
                .map(|row| row.identity)
        }
    }

    pub fn view_command_state(&self) -> ProcessViewCommandState {
        let row = self.selected_identity.and_then(|identity| {
            self.visible_rows
                .iter()
                .find(|row| row.identity == identity)
        });
        let has_children = row.is_some_and(|row| row.has_children);
        let expanded = row.is_some_and(|row| row.expanded);
        ProcessViewCommandState {
            tree_mode: self.tree_mode,
            can_expand: self.tree_mode && has_children && !expanded,
            can_collapse: self.tree_mode && has_children && expanded,
        }
    }

    fn expand_ancestors_of(&mut self, identity: ProcIdentity) {
        let identities: Vec<_> = self.entries.iter().map(|entry| entry.identity).collect();
        let parents: Vec<_> = self.entries.iter().map(|entry| entry.parent).collect();
        for ancestor in expansion_ancestors(&identities, &parents, identity) {
            self.collapsed.remove(&ancestor);
        }
    }

    fn set_row_expanded(&mut self, identity: ProcIdentity, expanded: bool) {
        if !self.tree_mode {
            return;
        }
        let changed = if expanded {
            self.collapsed.remove(&identity)
        } else {
            self.collapsed.insert(identity)
        };
        if changed {
            self.rebuild_listview(SelectionScrollPolicy::PreserveViewport);
        }
    }

    fn select_tree_identity(&mut self, identity: ProcIdentity) {
        if !self.entries.iter().any(|entry| entry.identity == identity) {
            return;
        }
        self.expand_ancestors_of(identity);
        self.selected_identity = Some(identity);
        self.rebuild_listview(SelectionScrollPolicy::RevealSelection);
    }

    fn handle_tree_key(&mut self, key: u16) {
        if !self.tree_mode {
            return;
        }
        let Some(identity) = self.current_selected_identity().or(self.selected_identity) else {
            return;
        };
        let Some(row) = self
            .visible_rows
            .iter()
            .copied()
            .find(|row| row.identity == identity)
        else {
            return;
        };
        match key {
            VK_RIGHT | VK_ADD => {
                if row.has_children && !row.expanded {
                    self.set_row_expanded(identity, true);
                } else if row.expanded {
                    self.select_first_visible_child(identity);
                }
            }
            VK_LEFT | VK_SUBTRACT => {
                if row.has_children && row.expanded {
                    self.set_row_expanded(identity, false);
                } else if let Some(parent) = self
                    .entries
                    .iter()
                    .find(|entry| entry.identity == identity)
                    .and_then(|entry| entry.parent)
                {
                    self.select_tree_identity(parent);
                }
            }
            _ => {}
        }
    }

    fn select_first_visible_child(&mut self, identity: ProcIdentity) {
        let Some(index) = self.visible_index(identity) else {
            return;
        };
        let depth = self.visible_rows[index].depth;
        let Some(child) = self.visible_rows.get(index + 1).copied() else {
            return;
        };
        if child.depth > depth {
            self.select_tree_identity(child.identity);
        }
    }

    fn handle_tree_click(&mut self, item: i32, subitem: i32, x: i32, double_click: bool) {
        if !self.tree_mode || item < 0 || subitem != 0 {
            return;
        }
        let Some(row) = self.visible_rows.get(item as usize).copied() else {
            return;
        };
        if !row.has_children {
            return;
        }
        let on_prefix = self.prefix_contains_click(item as usize, &row, x);
        let expand = !row.expanded;
        if double_click {
            if !on_prefix {
                self.set_row_expanded(row.identity, expand);
            }
        } else if on_prefix {
            self.set_row_expanded(row.identity, expand);
        }
    }

    fn prefix_contains_click(&self, visible_index: usize, row: &TreeRow, x: i32) -> bool {
        let prefix = tree_image_prefix(row);
        if prefix.is_empty() {
            return false;
        }
        let list_hwnd = self.list_hwnd();
        if list_hwnd.is_null() {
            return false;
        }
        let wide = to_wide_null(&prefix);
        let mut size = SIZE { cx: 0, cy: 0 };
        unsafe {
            let hdc = GetDC(list_hwnd);
            if hdc.is_null() {
                return false;
            }
            let font = SendMessageW(list_hwnd, WM_GETFONT, 0, 0);
            let previous = if font == 0 {
                null_mut()
            } else {
                SelectObject(hdc, font as HGDIOBJ)
            };
            let measured = GetTextExtentPoint32W(
                hdc,
                wide.as_ptr(),
                i32::try_from(wide.len().saturating_sub(1)).unwrap_or(i32::MAX),
                &mut size,
            );
            if font != 0 {
                SelectObject(hdc, previous);
            }
            ReleaseDC(list_hwnd, hdc);
            if measured == 0 {
                return false;
            }
            let mut rect = RECT {
                left: LVIR_LABEL as i32,
                ..zeroed()
            };
            if SendMessageW(
                list_hwnd,
                LVM_GETITEMRECT,
                visible_index,
                &mut rect as *mut RECT as LPARAM,
            ) == 0
            {
                return false;
            }
            x >= rect.left && x < rect.left.saturating_add(size.cx)
        }
    }

    fn selected_entry(&self) -> Option<&ProcEntry> {
        let identity = self.selected_identity?;
        self.entries.iter().find(|entry| entry.identity == identity)
    }

    // 打开“选择列”对话框。通过 ColumnDialogContext 传递页面和选项指针给 dialog proc。
    fn pick_columns(&mut self, options: &mut Options) {
        let mut context = ColumnDialogContext {
            page: self as *mut ProcessPageState,
            options: options as *mut Options,
        };
        if let Err(error) = dialog_box(
            self.hinstance,
            IDD_SELECTPROCCOLS,
            self.main_hwnd,
            Some(column_select_dialog_proc),
            &mut context as *mut ColumnDialogContext as LPARAM,
        ) {
            record_win32_error("process column dialog creation", error);
            self.show_failure_message(text(TextKey::SelectColumnsMenu), error);
        }
    }
}

// “选择列”对话框过程。初始化时同步选项状态，确认时将选中的列写回 options。
unsafe extern "system" fn column_select_dialog_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    unsafe {
        match msg {
            WM_INITDIALOG => {
                set_window_userdata(hwnd, lparam);
                localize_dialog(hwnd, IDD_SELECTPROCCOLS);
                let context = &*(lparam as *const ColumnDialogContext);
                let options = &*context.options;

                for &control_id in &COLUMN_DIALOG_IDS {
                    CheckDlgButton(hwnd, control_id, BST_UNCHECKED);
                }
                CheckDlgButton(hwnd, IDC_IMAGENAME, BST_CHECKED);

                for column in columns_from_options(options) {
                    CheckDlgButton(hwnd, COLUMN_DIALOG_IDS[column as usize], BST_CHECKED);
                }
                1
            }
            WM_COMMAND => match i32::from(loword(wparam)) {
                IDOK => {
                    let context = &mut *(get_window_userdata(hwnd) as *mut ColumnDialogContext);
                    let page = &mut *context.page;
                    let options = &mut *context.options;

                    if let Err(error) = page.save_column_layout(options) {
                        record_win32_error("process column layout persistence", error);
                        return 1;
                    }
                    apply_selected_columns(hwnd, options);
                    page.active_columns = columns_from_options(options);
                    page.setup_columns(options);
                    page.refresh_processes();
                    EndDialog(hwnd, IDOK as isize);
                    1
                }
                IDCANCEL => {
                    EndDialog(hwnd, IDCANCEL as isize);
                    1
                }
                _ => 0,
            },
            _ => 0,
        }
    }
}

fn reorder_process_columns(active: &[ColumnId], display_order: &[i32]) -> Option<Vec<ColumnId>> {
    if active.is_empty() || active.len() != display_order.len() {
        return None;
    }

    let mut seen = vec![false; active.len()];
    let mut ordered = Vec::with_capacity(active.len());
    for &logical_index in display_order {
        let logical_index = usize::try_from(logical_index).ok()?;
        if logical_index >= active.len() || seen[logical_index] {
            return None;
        }
        seen[logical_index] = true;
        ordered.push(active[logical_index]);
    }
    (ordered.first() == Some(&ColumnId::ImageName)).then_some(ordered)
}

fn write_process_column_layout(
    options: &mut Options,
    columns: &[ColumnId],
    widths: &HashMap<i32, i32>,
) {
    options.active_process_columns.fill(-1);
    options.column_widths.fill(-1);
    for (index, &column) in columns.iter().take(NUM_COLUMN).enumerate() {
        options.active_process_columns[index] = column as i32;
        options.column_widths[index] = widths
            .get(&(column as i32))
            .copied()
            .filter(|width| *width > 0)
            .unwrap_or(PROCESS_COLUMNS[column as usize].default_width);
    }
}

// 将对话框中的列勾选状态持久化到 options。保留已有顺序和列宽，新增列追加到末尾。
fn apply_selected_columns(hwnd: HWND, options: &mut Options) {
    unsafe {
        let existing_columns = columns_from_options(options);
        let mut existing_widths = HashMap::with_capacity(NUM_COLUMN);
        for (index, value) in options.active_process_columns.iter().copied().enumerate() {
            let Some(column) = column_id_from_i32(value) else {
                break;
            };
            existing_widths.insert(column as i32, options.column_widths[index]);
        }

        let mut selected = [false; NUM_COLUMN];
        selected[ColumnId::ImageName as usize] = true;
        for (column_index, &control_id) in COLUMN_DIALOG_IDS
            .iter()
            .enumerate()
            .take(NUM_COLUMN)
            .skip(1)
        {
            if IsDlgButtonChecked(hwnd, control_id) == BST_CHECKED {
                selected[column_index] = true;
            }
        }

        let mut columns = Vec::with_capacity(NUM_COLUMN);
        columns.push(ColumnId::ImageName);
        for column in existing_columns.into_iter().skip(1) {
            if selected[column as usize] {
                columns.push(column);
                selected[column as usize] = false;
            }
        }
        for (column_index, is_selected) in selected.iter().copied().enumerate().skip(1) {
            if is_selected {
                columns.push(column_id_from_i32(column_index as i32).unwrap_or(ColumnId::Pid));
            }
        }
        write_process_column_layout(options, &columns, &existing_widths);
    }
}

fn columns_from_options(options: &Options) -> Vec<ColumnId> {
    options
        .active_process_columns
        .iter()
        .copied()
        .filter_map(column_id_from_i32)
        .collect()
}

// 将 i32 列 ID 映射回 ColumnId 枚举。超出范围的值返回 None。
fn column_id_from_i32(value: i32) -> Option<ColumnId> {
    match value {
        x if x == ColumnId::ImageName as i32 => Some(ColumnId::ImageName),
        x if x == ColumnId::Pid as i32 => Some(ColumnId::Pid),
        x if x == ColumnId::Username as i32 => Some(ColumnId::Username),
        x if x == ColumnId::SessionId as i32 => Some(ColumnId::SessionId),
        x if x == ColumnId::Cpu as i32 => Some(ColumnId::Cpu),
        x if x == ColumnId::CpuTime as i32 => Some(ColumnId::CpuTime),
        x if x == ColumnId::MemUsage as i32 => Some(ColumnId::MemUsage),
        x if x == ColumnId::MemUsageDiff as i32 => Some(ColumnId::MemUsageDiff),
        x if x == ColumnId::PageFaults as i32 => Some(ColumnId::PageFaults),
        x if x == ColumnId::PageFaultsDiff as i32 => Some(ColumnId::PageFaultsDiff),
        x if x == ColumnId::CommitCharge as i32 => Some(ColumnId::CommitCharge),
        x if x == ColumnId::PagedPool as i32 => Some(ColumnId::PagedPool),
        x if x == ColumnId::NonPagedPool as i32 => Some(ColumnId::NonPagedPool),
        x if x == ColumnId::BasePriority as i32 => Some(ColumnId::BasePriority),
        x if x == ColumnId::HandleCount as i32 => Some(ColumnId::HandleCount),
        x if x == ColumnId::ThreadCount as i32 => Some(ColumnId::ThreadCount),
        _ => None,
    }
}

// 进程表的通用比较函数。排序键由 sort_column 决定，兜底使用 PID 保证稳定性。
#[cfg(test)]
mod tests {
    use super::actions::{
        DescendantProcessOutcome, FailedTerminationOutcome, affinity_cpu_mask,
        affinity_is_within_selection, affinity_target_for_thread,
        classify_descendant_process_result, classify_failed_termination,
        extract_first_command_token, is_valid_process_tree_edge, normalize_debugger_command_with,
        prepare_process_tree_termination, terminate_prepared_process_tree,
        validate_snapshot_root_identity,
    };
    use super::model::{DirtyColumns, ProcEntry, update_process_entry};
    use super::sampler::{
        ProcWorkerRequest, ProcWorkerState, WtsProcessIdentity, cpu_percent_from_delta,
        merge_wts_process_identity, signed_kb_delta, system_time_delta, wts_identity_matches,
    };
    use super::{
        ProcIdentity, ProcessPageState, SelectionScrollPolicy, refresh_selection_scroll_policy,
        reorder_process_columns, write_process_column_layout,
    };
    use crate::config::options::{ColumnId, Options};
    use crate::system::process_identity::query_process_identity_for_pid;
    use std::collections::{HashMap, HashSet};
    use std::env;
    use std::fs;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Registry::{REG_EXPAND_SZ, REG_SZ};
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, NORMAL_PRIORITY_CLASS, WaitForSingleObject,
    };

    const PROCESS_TREE_CHURN_WORKER_ENV: &str = "TASKMGR_RS_PROCESS_TREE_CHURN_WORKER";
    const PROCESS_TREE_CHURN_READY_ENV: &str = "TASKMGR_RS_PROCESS_TREE_CHURN_READY";

    struct ChurnWorkerGuard {
        child: Child,
        ready_path: PathBuf,
    }

    impl Drop for ChurnWorkerGuard {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = fs::remove_file(&self.ready_path);
        }
    }

    fn empty_process_entry(image_name: &str) -> ProcEntry {
        ProcEntry {
            identity: ProcIdentity::new(1234, 10),
            parent: None,
            pid: 1234,
            image_name: image_name.to_string(),
            image_name_lower: image_name.to_lowercase(),
            architecture: None,
            user_name: String::new(),
            user_name_lower: String::new(),
            session_id: None,
            cpu: 0,
            cpu_time_100ns: 0,
            display_cpu_time_100ns: 0,
            mem_usage_kb: 0,
            mem_diff_kb: 0,
            page_faults: 0,
            page_faults_diff: 0,
            commit_charge_kb: 0,
            paged_pool_kb: 0,
            nonpaged_pool_kb: 0,
            priority_class: NORMAL_PRIORITY_CLASS,
            handle_count: 0,
            thread_count: 0,
            display_text: std::array::from_fn(|_| String::new()),
            pass_count: 0,
            dirty_columns: DirtyColumns::default(),
        }
    }

    #[test]
    fn architecture_updates_only_the_display_name_and_its_dirty_column() {
        use crate::infrastructure::native::{ExecutionMode, ProcessArchitecture};
        let mut entry = empty_process_entry("Editor.exe");
        let mut next = entry.clone();
        next.architecture = Some(ProcessArchitecture {
            process_machine: 0x8664,
            native_machine: 0xaa64,
            mode: ExecutionMode::EmulatedX64,
        });
        let changed = update_process_entry(
            &mut entry,
            &next,
            2,
            DirtyColumns::from_column(ColumnId::ImageName),
        );
        assert!(changed.contains(ColumnId::ImageName));
        assert_eq!(entry.image_name, "Editor.exe");
        assert_eq!(entry.image_name_lower, "editor.exe");
        assert!(entry.display_text[ColumnId::ImageName as usize].contains("x86-64"));
        assert!(
            !update_process_entry(
                &mut entry,
                &next,
                3,
                DirtyColumns::from_column(ColumnId::ImageName)
            )
            .any()
        );
    }

    #[test]
    #[ignore = "requires Windows ARM64 and TASKMGR_ARCH_FIXTURES containing the four prebuilt GUI fixtures"]
    fn live_architecture_fixtures_use_the_real_process_sampler() {
        use crate::infrastructure::native::{ExecutionMode, query_process_architecture_handle};
        use crate::system::process_identity::query_process_architecture;
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, WaitForInputIdle};
        // SAFETY: this pseudo handle is borrowed for the current process lifetime.
        let observer = query_process_architecture_handle(unsafe { GetCurrentProcess() }).unwrap();
        assert_eq!(
            observer.native_machine, 0xaa64,
            "this acceptance test requires an ARM64 host"
        );
        let directory =
            PathBuf::from(env::var_os("TASKMGR_ARCH_FIXTURES").expect("fixture directory"));
        struct Fixture(Child);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let fixtures: Vec<_> = [
            ("arm64", ExecutionMode::Native),
            ("x86", ExecutionMode::EmulatedX86),
            ("x64", ExecutionMode::EmulatedX64),
            ("arm64ec", ExecutionMode::Arm64Ec),
        ]
        .into_iter()
        .map(|(name, mode)| {
            let child = Fixture(
                Command::new(directory.join(format!("fixture-{name}.exe")))
                    .spawn()
                    .unwrap(),
            );
            // SAFETY: the child owns a live process handle. Waiting for its GUI input queue
            // establishes readiness without a timing-based sleep or a production test hook.
            assert_eq!(
                unsafe { WaitForInputIdle(child.0.as_raw_handle().cast(), 10_000) },
                0
            );
            let identity = query_process_identity_for_pid(child.0.id()).unwrap();
            assert_eq!(
                query_process_architecture(identity).unwrap().mode,
                mode,
                "{name}"
            );
            assert!(
                query_process_architecture(ProcIdentity::new(
                    identity.pid,
                    identity.creation_time_100ns + 1
                ))
                .is_err()
            );
            (child, identity, mode)
        })
        .collect();
        let mut worker = ProcWorkerState::default();
        for pass in 0..2 {
            let snapshot = worker
                .collect(ProcWorkerRequest { processor_count: 1 })
                .unwrap();
            for (_, identity, mode) in &fixtures {
                let entry = snapshot
                    .entries
                    .iter()
                    .find(|entry| entry.identity == *identity)
                    .expect("fixture in process snapshot");
                assert_eq!(entry.architecture.unwrap().mode, *mode);
                println!(
                    "observer={:?} pass={pass} pid={} name={} architecture={:?}",
                    observer.mode, identity.pid, entry.image_name, entry.architecture
                );
            }
        }
        let identities: Vec<_> = fixtures.iter().map(|(_, identity, _)| *identity).collect();
        drop(fixtures);
        let snapshot = worker
            .collect(ProcWorkerRequest { processor_count: 1 })
            .unwrap();
        for identity in identities {
            assert!(
                snapshot
                    .entries
                    .iter()
                    .all(|entry| entry.identity != identity)
            );
            assert!(query_process_architecture(identity).is_err());
        }
    }

    #[test]
    fn dragged_process_columns_map_display_order_to_stable_ids() {
        let active = [
            ColumnId::ImageName,
            ColumnId::Username,
            ColumnId::Cpu,
            ColumnId::MemUsage,
        ];
        assert_eq!(
            reorder_process_columns(&active, &[0, 2, 1, 3]),
            Some(vec![
                ColumnId::ImageName,
                ColumnId::Cpu,
                ColumnId::Username,
                ColumnId::MemUsage,
            ])
        );
        assert_eq!(reorder_process_columns(&active, &[1, 0, 2, 3]), None);
        assert_eq!(reorder_process_columns(&active, &[0, 2, 2, 3]), None);
    }

    #[test]
    fn periodic_refresh_preserves_the_process_list_viewport() {
        assert_eq!(
            refresh_selection_scroll_policy(None),
            SelectionScrollPolicy::PreserveViewport
        );
    }

    #[test]
    fn explicit_find_reveals_the_requested_process() {
        assert_eq!(
            refresh_selection_scroll_policy(Some(ProcIdentity::new(1234, 10))),
            SelectionScrollPolicy::RevealSelection
        );
    }

    #[test]
    fn persisted_process_layout_keeps_widths_attached_to_column_ids() {
        let mut options = Options::default();
        let widths = HashMap::from([
            (ColumnId::ImageName as i32, 120),
            (ColumnId::Cpu as i32, 44),
            (ColumnId::Username as i32, 160),
        ]);
        write_process_column_layout(
            &mut options,
            &[ColumnId::ImageName, ColumnId::Cpu, ColumnId::Username],
            &widths,
        );
        assert_eq!(
            &options.active_process_columns[..4],
            &[
                ColumnId::ImageName as i32,
                ColumnId::Cpu as i32,
                ColumnId::Username as i32,
                -1,
            ]
        );
        assert_eq!(&options.column_widths[..3], &[120, 44, 160]);
    }

    #[test]
    fn extracts_quoted_absolute_debugger_path() {
        let command = r#""C:\Tools\Debugger\dbg.exe" -p %ld -e %ld -g"#;
        let debugger = normalize_debugger_command_with(command, REG_SZ, |value| value.to_string());
        assert_eq!(debugger.as_deref(), Some(r"C:\Tools\Debugger\dbg.exe"));
    }

    #[test]
    fn expands_environment_variables_before_extracting_debugger_path() {
        let command = r#""%SystemRoot%\System32\vsjitdebugger.exe" -p %ld"#;
        let debugger = normalize_debugger_command_with(command, REG_EXPAND_SZ, |_| {
            r#""C:\Windows\System32\vsjitdebugger.exe" -p %ld"#.to_string()
        });
        assert_eq!(
            debugger.as_deref(),
            Some(r"C:\Windows\System32\vsjitdebugger.exe")
        );
    }

    #[test]
    fn rejects_legacy_drwtsn32_debugger_commands() {
        let debugger =
            normalize_debugger_command_with(r"drwtsn32 -p %ld -e %ld -g", REG_SZ, |value| {
                value.to_string()
            });
        assert!(debugger.is_none());
    }

    #[test]
    fn extracts_unquoted_debugger_path_with_parameters() {
        let command = r"C:\Debuggers\dbg.exe -p %ld -e %ld";
        assert_eq!(
            extract_first_command_token(command),
            r"C:\Debuggers\dbg.exe"
        );
    }

    #[test]
    fn proc_identity_distinguishes_pid_reuse() {
        let first = ProcIdentity::new(1234, 10);
        let reused = ProcIdentity::new(1234, 20);
        let mut samples = HashMap::new();
        samples.insert(first, "old");
        samples.insert(reused, "new");

        assert_ne!(first, reused);
        assert_eq!(samples.get(&first), Some(&"old"));
        assert_eq!(samples.get(&reused), Some(&"new"));
    }

    #[test]
    fn signed_kb_delta_saturates_to_i64_bounds() {
        assert_eq!(signed_kb_delta(12, 5), 7);
        assert_eq!(signed_kb_delta(5, 12), -7);
        assert_eq!(signed_kb_delta(u64::MAX, 0), i64::MAX);
        assert_eq!(signed_kb_delta(0, u64::MAX), -i64::MAX);
    }

    #[test]
    fn affinity_mask_supports_the_full_native_kaffinity_width() {
        let highest_processor = i32::try_from(usize::BITS - 1).unwrap();
        assert_eq!(affinity_cpu_mask(0), 1);
        assert_eq!(
            affinity_cpu_mask(highest_processor),
            1usize << (usize::BITS - 1)
        );
        assert_eq!(affinity_cpu_mask(highest_processor + 1), 0);
    }

    #[test]
    fn multigroup_affinity_plan_keeps_group_and_mask_together() {
        let groups = [(0, 0b0011), (1, 0b1100)];
        assert_eq!(affinity_target_for_thread(0, &groups), Some((0, 0b0011)));
        assert_eq!(affinity_target_for_thread(1, &groups), Some((1, 0b1100)));
        assert_eq!(affinity_target_for_thread(2, &groups), Some((0, 0b0011)));
        assert_eq!(affinity_target_for_thread(0, &[]), None);
        assert!(affinity_is_within_selection(0, 0b0010, &groups));
        assert!(affinity_is_within_selection(1, 0b1000, &groups));
        assert!(!affinity_is_within_selection(0, 0b0100, &groups));
        assert!(!affinity_is_within_selection(2, 0b0001, &groups));
        assert!(!affinity_is_within_selection(0, 0, &groups));
    }

    #[test]
    fn process_tree_edges_reject_stale_parent_pid_reuse() {
        let old_child = ProcIdentity::new(20, 100);
        let reused_parent = ProcIdentity::new(10, 200);
        assert!(!is_valid_process_tree_edge(reused_parent, old_child, 300));

        let current_child = ProcIdentity::new(20, 250);
        assert!(is_valid_process_tree_edge(
            reused_parent,
            current_child,
            300
        ));
        assert!(!is_valid_process_tree_edge(
            reused_parent,
            current_child,
            225
        ));
    }

    #[test]
    fn process_tree_snapshot_rejects_reused_root_pid() {
        let expected = ProcIdentity::new(10, 100);

        assert!(validate_snapshot_root_identity(expected, expected).is_ok());
        assert!(validate_snapshot_root_identity(ProcIdentity::pid_only(10), expected).is_err());
        assert!(validate_snapshot_root_identity(expected, ProcIdentity::new(10, 200)).is_err());
    }

    #[test]
    fn descendant_process_outcomes_preserve_fatal_errors() {
        assert_eq!(
            classify_descendant_process_result(Ok(7), |_| true),
            DescendantProcessOutcome::Verified(7)
        );
        assert_eq!(
            classify_descendant_process_result(Ok(7), |_| false),
            DescendantProcessOutcome::GoneOrReused
        );
        assert_eq!(
            classify_descendant_process_result::<(), _>(Err(ERROR_INVALID_PARAMETER), |_| true),
            DescendantProcessOutcome::GoneOrReused
        );
        assert_eq!(
            classify_descendant_process_result::<(), _>(Err(ERROR_ACCESS_DENIED), |_| true),
            DescendantProcessOutcome::Fatal(ERROR_ACCESS_DENIED)
        );
    }

    #[test]
    fn failed_termination_of_an_exited_process_is_benign() {
        assert_eq!(
            classify_failed_termination(ERROR_ACCESS_DENIED, WAIT_OBJECT_0),
            FailedTerminationOutcome::AlreadyTerminated
        );
        assert_eq!(
            classify_failed_termination(ERROR_ACCESS_DENIED, WAIT_TIMEOUT),
            FailedTerminationOutcome::Failed(ERROR_ACCESS_DENIED)
        );
    }

    #[test]
    fn process_tree_churn_worker() {
        if env::var_os(PROCESS_TREE_CHURN_WORKER_ENV).is_none() {
            return;
        }

        let ready_path = PathBuf::from(
            env::var_os(PROCESS_TREE_CHURN_READY_ENV)
                .expect("the churn worker requires a readiness path"),
        );
        let completed_children = Arc::new(AtomicUsize::new(0));
        for _ in 0..4 {
            let completed_children = Arc::clone(&completed_children);
            thread::spawn(move || {
                loop {
                    if Command::new("cmd.exe")
                        .args(["/D", "/Q", "/C", "exit"])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .creation_flags(CREATE_NO_WINDOW)
                        .status()
                        .is_ok()
                    {
                        completed_children.fetch_add(1, Ordering::Release);
                    }
                }
            });
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        while completed_children.load(Ordering::Acquire) < 16 {
            assert!(
                Instant::now() < deadline,
                "the churn worker could not launch short-lived children"
            );
            thread::yield_now();
        }
        fs::write(&ready_path, b"ready").expect("the churn worker should signal readiness");

        loop {
            thread::park();
        }
    }

    #[test]
    fn process_tree_termination_survives_rapidly_exiting_descendants() {
        let ready_path = env::temp_dir().join(format!(
            "taskmgr-rs-process-tree-churn-{}.ready",
            std::process::id()
        ));
        let _ = fs::remove_file(&ready_path);
        let worker = Command::new(env::current_exe().expect("the test executable should exist"))
            .args([
                "--exact",
                "pages::processes::tests::process_tree_churn_worker",
            ])
            .env(PROCESS_TREE_CHURN_WORKER_ENV, "1")
            .env(PROCESS_TREE_CHURN_READY_ENV, &ready_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("the churn worker should start");
        let mut worker = ChurnWorkerGuard {
            child: worker,
            ready_path,
        };

        let deadline = Instant::now() + Duration::from_secs(10);
        while !worker.ready_path.exists() {
            assert!(
                worker
                    .child
                    .try_wait()
                    .expect("the churn worker state should be readable")
                    .is_none(),
                "the churn worker exited before becoming ready"
            );
            assert!(
                Instant::now() < deadline,
                "the churn worker did not become ready"
            );
            thread::sleep(Duration::from_millis(5));
        }

        let identity = query_process_identity_for_pid(worker.child.id())
            .expect("the churn worker identity should be queryable");
        let prepared = prepare_process_tree_termination(identity)
            .expect("short-lived descendants must not abort tree preparation");
        let outcome = terminate_prepared_process_tree(prepared);

        assert!(outcome.any_success());
        assert!(!outcome.any_failure());
        let wait = unsafe { WaitForSingleObject(worker.child.as_raw_handle(), 5_000) };
        assert_eq!(wait, WAIT_OBJECT_0, "the verified root must terminate");
    }

    #[test]
    fn wts_identity_source_rejects_stale_image_or_session() {
        let identity = WtsProcessIdentity {
            user_name: Some("SYSTEM".to_string()),
            session_id: 0,
            image_name_lower: "services.exe".to_string(),
        };

        assert!(wts_identity_matches("services.exe", None, &identity));
        assert!(wts_identity_matches("services.exe", Some(0), &identity));
        assert!(!wts_identity_matches("other.exe", Some(0), &identity));
        assert!(!wts_identity_matches("services.exe", Some(1), &identity));
    }

    #[test]
    fn missing_wts_user_name_remains_retryable() {
        let mut entry = empty_process_entry("services.exe");
        let identity = WtsProcessIdentity {
            user_name: None,
            session_id: 0,
            image_name_lower: "services.exe".to_string(),
        };

        assert!(!merge_wts_process_identity(&mut entry, Some(&identity)));
        assert!(entry.user_name.is_empty());
        assert_eq!(entry.session_id, Some(0));
    }

    #[test]
    fn direct_token_account_name_is_not_overwritten_by_wts() {
        let mut entry = empty_process_entry("service.exe");
        entry.user_name = "DIRECT".to_string();
        entry.user_name_lower = "direct".to_string();
        let identity = WtsProcessIdentity {
            user_name: Some("BATCH".to_string()),
            session_id: 0,
            image_name_lower: "service.exe".to_string(),
        };

        assert!(merge_wts_process_identity(&mut entry, Some(&identity)));
        assert_eq!(entry.user_name, "DIRECT");
        assert_eq!(entry.user_name_lower, "direct");
    }

    #[test]
    fn failed_process_worker_keeps_the_last_visible_snapshot() {
        let mut state = ProcessPageState {
            entries: vec![empty_process_entry("trusted.exe")],
            pass_count: 7,
            ..ProcessPageState::default()
        };

        state.apply_worker_completion(Err(5));

        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0].image_name, "trusted.exe");
        assert_eq!(state.pass_count, 7);
        assert_eq!(state.last_refresh_error, Some(5));
    }

    fn tree_entry(
        image_name: &str,
        identity: ProcIdentity,
        parent: Option<ProcIdentity>,
    ) -> ProcEntry {
        let mut entry = empty_process_entry(image_name);
        entry.identity = identity;
        entry.pid = identity.pid;
        entry.parent = parent;
        entry
    }

    fn visible_pids(state: &ProcessPageState) -> Vec<u32> {
        state
            .visible_rows
            .iter()
            .map(|row| row.identity.pid)
            .collect()
    }

    #[test]
    fn process_tree_refresh_keeps_expansion_and_drops_only_the_exited_row() {
        let parent = ProcIdentity::new(4, 50);
        let child = ProcIdentity::new(8, 80);
        let sibling = ProcIdentity::new(9, 90);
        let mut state = ProcessPageState {
            tree_mode: true,
            collapsed: HashSet::new(),
            selected_identity: Some(child),
            active_columns: vec![ColumnId::ImageName],
            ..ProcessPageState::default()
        };

        state.apply_process_snapshot(vec![
            tree_entry("child.exe", child, Some(parent)),
            tree_entry("parent.exe", parent, None),
            tree_entry("sibling.exe", sibling, None),
        ]);
        assert_eq!(visible_pids(&state), vec![4, 8, 9]);
        assert_eq!(state.selected_identity, Some(child));
        assert!(!state.collapsed.contains(&parent));

        state.collapsed.insert(parent);
        state.apply_process_snapshot(vec![
            tree_entry("child.exe", child, Some(parent)),
            tree_entry("parent.exe", parent, None),
            tree_entry("sibling.exe", sibling, None),
        ]);
        assert_eq!(visible_pids(&state), vec![4, 9]);
        assert_eq!(state.selected_identity, Some(parent));

        state.apply_process_snapshot(vec![
            tree_entry("parent.exe", parent, None),
            tree_entry("sibling.exe", sibling, None),
        ]);
        assert_eq!(visible_pids(&state), vec![4, 9]);
        assert!(state.collapsed.contains(&parent));
        assert!(!state.entries.iter().any(|entry| entry.identity == child));
    }

    #[test]
    fn list_mode_stays_flat_and_find_process_expands_ancestors() {
        let parent = ProcIdentity::new(4, 50);
        let child = ProcIdentity::new(2, 80);
        let mut state = ProcessPageState {
            tree_mode: false,
            active_columns: vec![ColumnId::Pid],
            ..ProcessPageState::default()
        };
        state.apply_process_snapshot(vec![
            tree_entry("parent.exe", parent, None),
            tree_entry("child.exe", child, Some(parent)),
        ]);
        assert_eq!(visible_pids(&state), vec![2, 4]);
        assert!(state.visible_rows.iter().all(|row| row.depth == 0));

        state.collapsed.insert(parent);
        state.tree_mode = true;
        state.rebuild_listview(SelectionScrollPolicy::PreserveViewport);
        assert_eq!(visible_pids(&state), vec![4]);
        assert!(state.find_process(child));
        assert_eq!(visible_pids(&state), vec![4, 2]);
        assert_eq!(state.selected_identity, Some(child));
        assert!(!state.collapsed.contains(&parent));
    }

    #[test]
    fn system_time_delta_requires_a_monotonic_baseline() {
        assert_eq!(system_time_delta(100, None), None);
        assert_eq!(system_time_delta(90, Some(100)), None);
        assert_eq!(system_time_delta(125, Some(100)), Some(25));
    }

    #[test]
    fn process_cpu_percentage_uses_exact_integer_rounding() {
        assert_eq!(cpu_percent_from_delta(0, 100), 0);
        assert_eq!(cpu_percent_from_delta(1, 3), 33);
        assert_eq!(cpu_percent_from_delta(2, 3), 67);
        assert_eq!(cpu_percent_from_delta(100, 100), 100);
    }
}

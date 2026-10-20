// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 启动项页面
//
//   文件:       src/pages/startup/mod.rs
//
//   日期:       2026年10月20日
//   环境:       Linux 6.12.94+ x86_64；Rust 1.99.1
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

mod model;
mod sampler;

// 启动项页列出 Run 键和“启动”文件夹中的项目，并在后台 worker 上启用或禁用。
// UI 线程只提交请求、维护列表选择和绘制。刷新保留来源加名称对应的选择、焦点和滚动位置。
// 启用和禁用不删除原项；写入结果读回不一致、项目已消失或 API 失败时立即显示错误。

use std::mem::zeroed;

use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, HWND, LPARAM, RECT};
use windows_sys::Win32::System::Diagnostics::Debug::MessageBeep;
use windows_sys::Win32::UI::Controls::{
    LVCF_FMT, LVCF_SUBITEM, LVCF_TEXT, LVCF_WIDTH, LVCFMT_LEFT, LVCOLUMNW, LVIF_STATE, LVIF_TEXT,
    LVIS_FOCUSED, LVIS_SELECTED, LVITEMW, LVM_DELETECOLUMN, LVM_GETCOUNTPERPAGE, LVM_GETITEMCOUNT,
    LVM_GETNEXTITEM, LVM_GETTOPINDEX, LVM_INSERTCOLUMNW, LVM_REDRAWITEMS, LVM_SETITEMCOUNT,
    LVM_SETITEMSTATE, LVN_COLUMNCLICK, LVN_GETDISPINFOW, LVN_ITEMCHANGED, LVNI_SELECTED,
    LVS_EX_FULLROWSELECT, LVS_SHOWSELALWAYS, LVSICF_NOINVALIDATEALL, LVSICF_NOSCROLL, NMHDR,
    NMLISTVIEW, NMLVDISPINFOW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, DeferWindowPos, EndDeferWindowPos, GWL_STYLE, GetClientRect,
    GetDialogBaseUnits, GetDlgItem, GetWindowLongW, HMENU, MB_ICONERROR, MB_OK, MF_BYCOMMAND,
    MF_ENABLED, MF_GRAYED, MessageBoxW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SendMessageW, SetWindowLongW, TPM_RETURNCMD, TrackPopupMenuEx, WM_SETREDRAW,
};

use crate::config::options::Options;
use crate::infrastructure::diagnostics::Field;
use crate::infrastructure::native::{
    copy_text_to_callback_buffer, finish_list_view_update, loword,
    record_hresult_error_with_fields, record_win32_error, record_win32_error_with_fields,
    subclass_list_view, to_wide_null, window_rect_relative_to_page,
};
use crate::infrastructure::worker::SingleFlightWorker;
use crate::ui::localization::{TextKey, startup_column_titles, text};
use crate::ui::menus::build_popup_menu;
use crate::ui::resource_ids::{
    IDC_STARTUPLIST, IDM_STARTUP_DISABLE, IDM_STARTUP_ENABLE, IDR_STARTUP_CONTEXT,
    PWM_STARTUP_WORKER_COMPLETE,
};

pub(crate) use self::model::StartupCommandState;
use self::model::{
    StartupActionFailure, StartupEnabledState, StartupEntryId, StartupRequest, StartupScrollPolicy,
    StartupToggle, command_state, display_name, merge_startup_requests, publisher_or_command,
    refresh_scroll_policy, retained_startup_id,
};
use self::sampler::{
    StartupCollection, StartupQueryFailure, StartupRefreshFailure, StartupWorkerCompletion, perform,
};

const DEFSPACING_BASE: i32 = 3;
const DLG_SCALE_X: i32 = 4;
const COLUMN_COUNT: usize = 4;

struct StartupRow {
    id: StartupEntryId,
    name: String,
    name_sort: String,
    detail: String,
    detail_sort: String,
    location: String,
    location_sort: String,
    state: StartupEnabledState,
    state_text: String,
    state_sort: String,
}

pub struct StartupPageState {
    hwnd: HWND,
    no_title: bool,
    rows: Vec<StartupRow>,
    selected: Option<StartupEntryId>,
    sort_column: usize,
    sort_ascending: bool,
    updating_list: bool,
    worker: Option<SingleFlightWorker<StartupRequest, StartupWorkerCompletion>>,
    last_refresh_error: Option<StartupQueryFailure>,
    last_row_errors: Vec<self::sampler::StartupRowError>,
    last_source_errors: Vec<self::sampler::StartupSourceError>,
}

impl Default for StartupPageState {
    fn default() -> Self {
        Self {
            hwnd: std::ptr::null_mut(),
            no_title: false,
            rows: Vec::new(),
            selected: None,
            sort_column: 0,
            sort_ascending: true,
            updating_list: false,
            worker: None,
            last_refresh_error: None,
            last_row_errors: Vec::new(),
            last_source_errors: Vec::new(),
        }
    }
}

impl StartupPageState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn initialize(&mut self, hwnd: HWND) -> Result<(), u32> {
        self.hwnd = hwnd;
        self.start_worker()?;
        // SAFETY: initialization runs on the UI thread against this page dialog and its list.
        unsafe {
            let list = self.list_hwnd();
            if list.is_null() {
                return Err(windows_sys::Win32::Foundation::ERROR_INVALID_WINDOW_HANDLE);
            }
            subclass_list_view(list);
            let extended = LVS_EX_FULLROWSELECT;
            SendMessageW(
                list,
                windows_sys::Win32::UI::Controls::LVM_SETEXTENDEDLISTVIEWSTYLE,
                extended as usize,
                extended as isize,
            );
            let style = GetWindowLongW(list, GWL_STYLE) as u32;
            SetWindowLongW(list, GWL_STYLE, (style | LVS_SHOWSELALWAYS) as i32);
            self.configure_columns();
            self.size_page();
            self.update_ui_state();
        }
        Ok(())
    }

    pub fn apply_options(&mut self, options: &Options) {
        self.no_title = options.no_title();
    }

    pub fn no_title(&self) -> bool {
        self.no_title
    }

    pub fn timer_event(&mut self) {
        self.refresh();
    }

    pub fn destroy(&mut self) {
        self.worker = None;
        self.rows.clear();
    }

    pub fn command_state(&self) -> StartupCommandState {
        command_state(self.selected_state())
    }

    pub fn handle_command(&mut self, command_id: u16) -> bool {
        match command_id {
            IDM_STARTUP_ENABLE => {
                self.request_toggle(StartupToggle::Enable);
                true
            }
            IDM_STARTUP_DISABLE => {
                self.request_toggle(StartupToggle::Disable);
                true
            }
            _ => false,
        }
    }

    /// Handles a notification for the startup list.
    ///
    /// # Safety
    ///
    /// `lparam` must be the live `WM_NOTIFY` payload for this page. `LVN_GETDISPINFOW` requires
    /// the text buffer in that payload to be writable for this synchronous call.
    pub unsafe fn handle_notify(&mut self, lparam: LPARAM) -> isize {
        unsafe {
            let Some(header) = (lparam as *const NMHDR).as_ref() else {
                return 0;
            };
            if header.idFrom != IDC_STARTUPLIST as usize || header.hwndFrom != self.list_hwnd() {
                return 0;
            }
            match header.code {
                code if code == LVN_GETDISPINFOW => {
                    let display = &mut *(lparam as *mut NMLVDISPINFOW);
                    self.fill_display_info(&mut display.item);
                    1
                }
                code if code == LVN_ITEMCHANGED => {
                    if !self.updating_list {
                        let notify = &*(lparam as *const NMLISTVIEW);
                        if (notify.uChanged & LVIF_STATE) != 0 {
                            self.selected = self.current_selected();
                            self.update_ui_state();
                        }
                    }
                    1
                }
                code if code == LVN_COLUMNCLICK => {
                    let notify = &*(lparam as *const NMLISTVIEW);
                    let column = usize::try_from(notify.iSubItem).unwrap_or(0);
                    if column >= COLUMN_COUNT {
                        return 1;
                    }
                    if self.sort_column == column {
                        self.sort_ascending = !self.sort_ascending;
                    } else {
                        self.sort_column = column;
                        self.sort_ascending = true;
                    }
                    self.sort_rows();
                    self.rebuild_list(refresh_scroll_policy());
                    1
                }
                _ => 0,
            }
        }
    }

    pub fn show_context_menu(&mut self, x: i32, y: i32) {
        if self.selected.is_none() {
            return;
        }
        let popup = match build_popup_menu(IDR_STARTUP_CONTEXT) {
            Ok(popup) => popup,
            Err(error) => {
                record_win32_error("startup popup menu creation", error);
                return;
            }
        };
        let state = self.command_state();
        // SAFETY: the popup is owned for this call and the page HWND is the menu owner.
        unsafe {
            enable_command(popup.as_raw(), IDM_STARTUP_ENABLE, state.enable);
            enable_command(popup.as_raw(), IDM_STARTUP_DISABLE, state.disable);
            let command = TrackPopupMenuEx(
                popup.as_raw(),
                TPM_RETURNCMD,
                x,
                y,
                self.hwnd,
                std::ptr::null_mut(),
            );
            if command != 0 {
                self.handle_command(command as u16);
            }
        }
    }

    pub fn size_page(&self) {
        // SAFETY: layout queries and moves child controls of this page HWND.
        unsafe {
            if self.hwnd.is_null() {
                return;
            }
            let mut parent_rect = zeroed::<RECT>();
            GetClientRect(self.hwnd, &mut parent_rect);
            let units = GetDialogBaseUnits() as usize;
            let def_spacing = (DEFSPACING_BASE * i32::from(loword(units))) / DLG_SCALE_X;
            let mut hdwp = BeginDeferWindowPos(8);
            if hdwp.is_null() {
                return;
            }
            let anchor = GetDlgItem(self.hwnd, i32::from(IDM_STARTUP_DISABLE));
            let list = self.list_hwnd();
            if anchor.is_null() || list.is_null() {
                EndDeferWindowPos(hdwp);
                return;
            }
            let anchor_rect = window_rect_relative_to_page(anchor, self.hwnd);
            let dx = (parent_rect.right - def_spacing * 2) - anchor_rect.right;
            let dy = (parent_rect.bottom - def_spacing * 2) - anchor_rect.bottom;
            let list_rect = window_rect_relative_to_page(list, self.hwnd);
            hdwp = DeferWindowPos(
                hdwp,
                list,
                std::ptr::null_mut(),
                0,
                0,
                (anchor_rect.right - list_rect.left + dx).max(0),
                (anchor_rect.top - list_rect.top + dy - def_spacing).max(0),
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            for control_id in [
                i32::from(IDM_STARTUP_ENABLE),
                i32::from(IDM_STARTUP_DISABLE),
            ] {
                let control = GetDlgItem(self.hwnd, control_id);
                if control.is_null() {
                    continue;
                }
                let control_rect = window_rect_relative_to_page(control, self.hwnd);
                hdwp = DeferWindowPos(
                    hdwp,
                    control,
                    std::ptr::null_mut(),
                    control_rect.left + dx,
                    control_rect.top + dy,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            EndDeferWindowPos(hdwp);
        }
    }

    pub fn handle_worker_completion(&mut self) {
        self.drain_worker();
    }

    fn start_worker(&mut self) -> Result<(), u32> {
        if self.worker.is_some() {
            return Ok(());
        }
        self.worker = Some(SingleFlightWorker::spawn(
            "taskmgr-rs-startup-sampler",
            PWM_STARTUP_WORKER_COMPLETE,
            merge_startup_requests,
            perform,
        )?);
        Ok(())
    }

    fn refresh(&mut self) {
        self.drain_worker();
        self.submit(StartupRequest::Refresh);
    }

    fn request_toggle(&mut self, toggle: StartupToggle) {
        let state = self.command_state();
        let allowed = match toggle {
            StartupToggle::Enable => state.enable,
            StartupToggle::Disable => state.disable,
        };
        let Some(id) = self.selected.clone() else {
            self.beep();
            return;
        };
        if !allowed {
            self.beep();
            return;
        }
        self.submit(StartupRequest::SetEnabled { id, toggle });
    }

    fn submit(&mut self, request: StartupRequest) {
        let toggle = match &request {
            StartupRequest::SetEnabled { toggle, .. } => Some(*toggle),
            StartupRequest::Refresh => None,
        };
        let Some(worker) = self.worker.as_mut() else {
            self.report_submit_failure(StartupQueryFailure::Win32(ERROR_BROKEN_PIPE), toggle);
            return;
        };
        if let Err(error) = worker.request(request, self.hwnd) {
            self.report_submit_failure(StartupQueryFailure::Win32(error), toggle);
        }
    }

    fn report_submit_failure(
        &mut self,
        failure: StartupQueryFailure,
        toggle: Option<StartupToggle>,
    ) {
        if let Some(toggle) = toggle {
            let action_failure = match failure {
                StartupQueryFailure::Win32(code) => StartupActionFailure::Win32(code),
                StartupQueryFailure::Hresult(code) => StartupActionFailure::Hresult(code),
            };
            self.show_action_failure(toggle, action_failure);
        } else {
            self.set_refresh_error(failure);
        }
    }

    fn drain_worker(&mut self) {
        let drained = match self.worker.as_mut() {
            Some(worker) => worker.drain(self.hwnd),
            None => return,
        };
        for completion in drained.completions {
            crate::infrastructure::diagnostics::with_operation_id(completion.operation_id, || {
                self.apply_completion(completion.value);
            });
        }
        if let Some(error) = drained.error {
            self.set_refresh_error(StartupQueryFailure::Win32(error));
        }
    }

    fn apply_completion(&mut self, completion: StartupWorkerCompletion) {
        match completion {
            StartupWorkerCompletion::Snapshot(result) => match result {
                Ok(collection) => self.apply_collection(collection),
                Err(failure) => self.apply_refresh_failure(failure),
            },
            StartupWorkerCompletion::Action(report) => {
                match report.outcome {
                    Ok(state) => self.apply_toggled_state(&report.id, state),
                    Err(failure) => self.show_action_failure(report.toggle, failure),
                }
                self.submit(StartupRequest::Refresh);
            }
        }
    }

    fn apply_toggled_state(&mut self, id: &StartupEntryId, state: StartupEnabledState) {
        let Some(row) = self.rows.iter_mut().find(|row| row.id.matches(id)) else {
            return;
        };
        let state_text = state_text(state).to_string();
        row.state = state;
        row.state_sort = state_text.to_lowercase();
        row.state_text = state_text;
        self.sort_rows();
        self.rebuild_list(refresh_scroll_policy());
    }

    fn apply_collection(&mut self, collection: StartupCollection) {
        self.last_refresh_error = None;
        for error in &collection.row_errors {
            if !self.last_row_errors.contains(error) {
                record_query_failure(
                    "startup row metadata",
                    error.failure,
                    &[
                        Field::text("source", format!("{:?}", error.id.source)),
                        Field::text("name", &error.id.name),
                        Field::text("field", error.field),
                    ],
                );
            }
        }
        self.record_source_errors(&collection.source_errors);
        self.last_row_errors = collection.row_errors;
        self.last_source_errors = collection.source_errors;
        let previous = self.selected.clone();
        self.rows = collection.entries.into_iter().map(startup_row).collect();
        self.sort_rows();
        self.selected =
            retained_startup_id(previous.as_ref(), self.rows.iter().map(|row| &row.id)).cloned();
        self.rebuild_list(refresh_scroll_policy());
    }

    fn sort_rows(&mut self) {
        let column = self.sort_column;
        let ascending = self.sort_ascending;
        self.rows.sort_by(|left, right| {
            let ordering = match column {
                1 => left.detail_sort.cmp(&right.detail_sort),
                2 => left.location_sort.cmp(&right.location_sort),
                3 => left.state_sort.cmp(&right.state_sort),
                _ => left.name_sort.cmp(&right.name_sort),
            };
            if ascending {
                ordering
            } else {
                ordering.reverse()
            }
        });
    }

    fn rebuild_list(&mut self, scroll_policy: StartupScrollPolicy) {
        // SAFETY: the list belongs to this page. Item count uses LVSICF_NOSCROLL so a refresh does
        // not move the viewport. Selection is restored without LVM_ENSUREVISIBLE.
        let StartupScrollPolicy::PreserveViewport = scroll_policy;
        unsafe {
            let list = self.list_hwnd();
            if list.is_null() {
                return;
            }
            let selected_index = self
                .selected
                .as_ref()
                .and_then(|selected| self.rows.iter().position(|row| row.id.matches(selected)));
            let existing = SendMessageW(list, LVM_GETITEMCOUNT, 0, 0).max(0) as usize;
            let count_changed = existing != self.rows.len();
            self.updating_list = true;
            if count_changed {
                SendMessageW(list, WM_SETREDRAW, 0, 0);
                SendMessageW(
                    list,
                    LVM_SETITEMCOUNT,
                    self.rows.len(),
                    (LVSICF_NOINVALIDATEALL | LVSICF_NOSCROLL) as LPARAM,
                );
            }
            let mut item = LVITEMW {
                stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                state: 0,
                ..zeroed()
            };
            SendMessageW(
                list,
                LVM_SETITEMSTATE,
                usize::MAX,
                &mut item as *mut _ as LPARAM,
            );
            if let Some(index) = selected_index {
                item.state = LVIS_SELECTED | LVIS_FOCUSED;
                SendMessageW(list, LVM_SETITEMSTATE, index, &mut item as *mut _ as LPARAM);
            } else {
                self.selected = None;
            }
            redraw_visible(list, self.rows.len());
            if count_changed {
                finish_list_view_update(list);
            }
            self.updating_list = false;
            self.update_ui_state();
        }
    }

    fn fill_display_info(&self, item: &mut LVITEMW) {
        if (item.mask & LVIF_TEXT) == 0 || item.pszText.is_null() || item.cchTextMax <= 0 {
            return;
        }
        let Some(row) = usize::try_from(item.iItem)
            .ok()
            .and_then(|index| self.rows.get(index))
        else {
            // SAFETY: the caller provided a writable callback buffer of `cchTextMax` units.
            unsafe { *item.pszText = 0 };
            return;
        };
        let text = match item.iSubItem {
            1 => row.detail.as_str(),
            2 => row.location.as_str(),
            3 => row.state_text.as_str(),
            _ => row.name.as_str(),
        };
        // SAFETY: LVN_GETDISPINFOW guarantees `pszText` is writable for `cchTextMax` units.
        unsafe { copy_text_to_callback_buffer(item.pszText, item.cchTextMax as usize, text) };
    }

    fn configure_columns(&self) {
        // SAFETY: column setup targets this page's list on the UI thread.
        unsafe {
            let list = self.list_hwnd();
            if list.is_null() {
                return;
            }
            while SendMessageW(list, LVM_DELETECOLUMN, 0, 0) != 0 {}
            let titles = startup_column_titles();
            let widths = [160, 220, 260, 90];
            for (index, (title, width)) in titles.iter().zip(widths).enumerate() {
                let mut title_wide = to_wide_null(title);
                let mut column = LVCOLUMNW {
                    mask: LVCF_FMT | LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM,
                    fmt: LVCFMT_LEFT,
                    cx: width,
                    pszText: title_wide.as_mut_ptr(),
                    cchTextMax: title_wide.len() as i32,
                    iSubItem: index as i32,
                    ..zeroed()
                };
                SendMessageW(
                    list,
                    LVM_INSERTCOLUMNW,
                    index,
                    &mut column as *mut _ as isize,
                );
            }
        }
    }

    fn current_selected(&self) -> Option<StartupEntryId> {
        // SAFETY: selection query targets this page's list.
        unsafe {
            let list = self.list_hwnd();
            if list.is_null() {
                return None;
            }
            let index = SendMessageW(list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize);
            let index = usize::try_from(index).ok()?;
            self.rows.get(index).map(|row| row.id.clone())
        }
    }

    fn selected_state(&self) -> Option<StartupEnabledState> {
        let selected = self.selected.as_ref()?;
        self.rows
            .iter()
            .find(|row| row.id.matches(selected))
            .map(|row| row.state)
    }

    fn update_ui_state(&self) {
        let state = self.command_state();
        enable_button(self.hwnd, IDM_STARTUP_ENABLE, state.enable);
        enable_button(self.hwnd, IDM_STARTUP_DISABLE, state.disable);
    }

    fn show_action_failure(&self, toggle: StartupToggle, failure: StartupActionFailure) {
        let summary = match toggle {
            StartupToggle::Enable => text(TextKey::StartupEnableFailed),
            StartupToggle::Disable => text(TextKey::StartupDisableFailed),
        };
        let body = match failure {
            StartupActionFailure::Win32(code) => {
                format!("{summary}\n\n{} {code}", text(TextKey::Win32ErrorPrefix))
            }
            StartupActionFailure::Hresult(code) => {
                format!("{summary}\n\n{} {code:#010X}", text(TextKey::HresultPrefix))
            }
            StartupActionFailure::Missing => {
                format!("{summary}\n\n{}", text(TextKey::StartupEntryMissing))
            }
            StartupActionFailure::StateUnchanged { .. } => {
                format!("{summary}\n\n{}", text(TextKey::StartupStateUnchanged))
            }
        };
        // SAFETY: both strings are NUL-terminated and live for the modal message box.
        unsafe {
            let body_wide = to_wide_null(&body);
            let title_wide = to_wide_null(text(TextKey::AppTitle));
            MessageBoxW(
                self.hwnd,
                body_wide.as_ptr(),
                title_wide.as_ptr(),
                MB_OK | MB_ICONERROR,
            );
        }
    }

    fn apply_refresh_failure(&mut self, failure: StartupRefreshFailure) {
        self.record_source_errors(&failure.sources);
        self.last_source_errors = failure.sources;
        if let Some(error) = self.last_source_errors.first().copied() {
            self.last_refresh_error = Some(error.failure);
        }
    }

    fn record_source_errors(&self, errors: &[self::sampler::StartupSourceError]) {
        for error in errors {
            if !self.last_source_errors.contains(error) {
                record_query_failure(
                    "startup source",
                    error.failure,
                    &[Field::text("source", format!("{:?}", error.source))],
                );
            }
        }
    }

    fn set_refresh_error(&mut self, failure: StartupQueryFailure) {
        if self.last_refresh_error != Some(failure) {
            record_query_failure("startup refresh", failure, &[]);
        }
        self.last_refresh_error = Some(failure);
    }

    fn beep(&self) {
        // SAFETY: MessageBeep has no pointer inputs.
        unsafe { MessageBeep(0) };
    }

    fn list_hwnd(&self) -> HWND {
        // SAFETY: this queries a child of the page dialog. Null means the control is absent.
        unsafe { GetDlgItem(self.hwnd, IDC_STARTUPLIST) }
    }
}

fn startup_row(record: self::sampler::StartupRecord) -> StartupRow {
    let name = display_name(record.id.source, &record.id.name);
    let detail = publisher_or_command(record.publisher.as_deref(), &record.command);
    let state_text = state_text(record.state).to_string();
    StartupRow {
        name_sort: name.to_lowercase(),
        name,
        detail_sort: detail.to_lowercase(),
        detail,
        location_sort: record.location.to_lowercase(),
        location: record.location,
        state: record.state,
        state_sort: state_text.to_lowercase(),
        state_text,
        id: record.id,
    }
}

fn state_text(state: StartupEnabledState) -> &'static str {
    match state {
        StartupEnabledState::Enabled => text(TextKey::StartupStatusEnabled),
        StartupEnabledState::Disabled => text(TextKey::StartupStatusDisabled),
        StartupEnabledState::Unknown => text(TextKey::Unknown),
    }
}

fn record_query_failure(component: &str, failure: StartupQueryFailure, fields: &[Field]) {
    match failure {
        StartupQueryFailure::Win32(code) => record_win32_error_with_fields(component, code, fields),
        StartupQueryFailure::Hresult(code) => {
            record_hresult_error_with_fields(component, code, fields)
        }
    }
}

fn enable_button(hwnd: HWND, command_id: u16, enabled: bool) {
    let control = unsafe { GetDlgItem(hwnd, i32::from(command_id)) };
    if !control.is_null() {
        unsafe { EnableWindow(control, i32::from(enabled)) };
    }
}

fn enable_command(menu: HMENU, command_id: u16, enabled: bool) {
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::EnableMenuItem(
            menu,
            u32::from(command_id),
            MF_BYCOMMAND | if enabled { MF_ENABLED } else { MF_GRAYED },
        );
    }
}

fn redraw_visible(list: HWND, count: usize) {
    if count == 0 {
        return;
    }
    let top = unsafe { SendMessageW(list, LVM_GETTOPINDEX, 0, 0).max(0) as usize };
    let visible = unsafe { SendMessageW(list, LVM_GETCOUNTPERPAGE, 0, 0).max(0) as usize };
    let end = top.saturating_add(visible).saturating_add(1).min(count);
    if top < end {
        unsafe { SendMessageW(list, LVM_REDRAWITEMS, top, (end - 1) as LPARAM) };
    }
}

// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 服务页面
//
//   文件:       src/pages/services/mod.rs
//
//   日期:       2026年10月19日
//   环境:       Linux 6.12.94+ x86_64；Rust 1.99.1
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

mod model;
mod sampler;

// 服务页列出本机 Win32 服务，并在后台 worker 上启动、停止、暂停或继续服务。
// UI 线程只提交请求、维护列表选择和绘制。刷新保留服务名对应的选择、焦点和滚动位置。
// “转到进程”只使用带创建时间的 ProcIdentity；没有校验过的进程时该命令保持禁用。

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
    copy_text_to_callback_buffer, finish_list_view_update, loword, record_win32_error,
    record_win32_error_with_fields, subclass_list_view, to_wide_null, window_rect_relative_to_page,
};
use crate::infrastructure::worker::SingleFlightWorker;
use crate::system::process_identity::{FindProcPayload, ProcIdentity, find_proc_lparam};
use crate::ui::localization::{TextKey, service_column_titles, text};
use crate::ui::menus::build_popup_menu;
use crate::ui::resource_ids::{
    IDC_SERVICELIST, IDM_SERVICE_CONTINUE, IDM_SERVICE_GOTOPROCESS, IDM_SERVICE_PAUSE,
    IDM_SERVICE_START, IDM_SERVICE_STOP, IDR_SERVICE_CONTEXT, PWM_SERVICE_WORKER_COMPLETE,
    WM_FINDPROC,
};

pub(crate) use self::model::ServiceCommandState;
use self::model::{
    ServiceActionFailure, ServiceCommandInput, ServiceControlAction, ServiceRequest,
    ServiceRunState, ServiceScrollPolicy, ServiceStartupType, merge_service_requests,
    owning_process_label, refresh_scroll_policy, retained_service_name,
};
use self::sampler::{ServiceCollection, ServiceWorkerCompletion, perform};

const DEFSPACING_BASE: i32 = 3;
const DLG_SCALE_X: i32 = 4;
const COLUMN_COUNT: usize = 5;

struct ServiceRow {
    service_name: String,
    name_sort: String,
    description: String,
    description_sort: String,
    status: ServiceRunState,
    status_text: String,
    status_sort: String,
    startup: ServiceStartupType,
    accepts_stop: bool,
    accepts_pause_continue: bool,
    startup_text: String,
    startup_sort: String,
    process: Option<ProcIdentity>,
    process_text: String,
    process_sort: String,
}

pub struct ServicePageState {
    hwnd: HWND,
    main_hwnd: HWND,
    no_title: bool,
    rows: Vec<ServiceRow>,
    selected_service: Option<String>,
    sort_column: usize,
    sort_ascending: bool,
    updating_list: bool,
    worker: Option<SingleFlightWorker<ServiceRequest, ServiceWorkerCompletion>>,
    pending_control: Option<ServiceControlAction>,
    last_refresh_error: Option<u32>,
    last_row_errors: Vec<self::sampler::ServiceRowError>,
}

impl Default for ServicePageState {
    fn default() -> Self {
        Self {
            hwnd: std::ptr::null_mut(),
            main_hwnd: std::ptr::null_mut(),
            no_title: false,
            rows: Vec::new(),
            selected_service: None,
            sort_column: 0,
            sort_ascending: true,
            updating_list: false,
            worker: None,
            pending_control: None,
            last_refresh_error: None,
            last_row_errors: Vec::new(),
        }
    }
}

impl ServicePageState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn initialize(&mut self, hwnd: HWND, main_hwnd: HWND) -> Result<(), u32> {
        self.hwnd = hwnd;
        self.main_hwnd = main_hwnd;
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
        self.pending_control = None;
        self.rows.clear();
    }

    pub fn command_state(&self) -> ServiceCommandState {
        let mut state = model::command_state(self.selected_input());
        if self.pending_control.is_some() {
            state.start = false;
            state.stop = false;
        }
        state
    }

    pub fn handle_command(&mut self, command_id: u16) -> bool {
        match command_id {
            IDM_SERVICE_START => {
                self.request_control(ServiceControlAction::Start);
                true
            }
            IDM_SERVICE_STOP => {
                self.request_control(ServiceControlAction::Stop);
                true
            }
            IDM_SERVICE_PAUSE => {
                self.request_control(ServiceControlAction::Pause);
                true
            }
            IDM_SERVICE_CONTINUE => {
                self.request_control(ServiceControlAction::Continue);
                true
            }
            IDM_SERVICE_GOTOPROCESS => {
                self.go_to_process();
                true
            }
            _ => false,
        }
    }

    /// Handles a notification for the services list.
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
            if header.idFrom != IDC_SERVICELIST as usize || header.hwndFrom != self.list_hwnd() {
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
                            self.selected_service = self.current_selected_service();
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
        if self.selected_service.is_none() {
            return;
        }
        let popup = match build_popup_menu(IDR_SERVICE_CONTEXT) {
            Ok(popup) => popup,
            Err(error) => {
                record_win32_error("service popup menu creation", error);
                return;
            }
        };
        let state = self.command_state();
        // SAFETY: the popup is owned for this call and the page HWND is the menu owner.
        unsafe {
            enable_command(popup.as_raw(), IDM_SERVICE_START, state.start);
            enable_command(popup.as_raw(), IDM_SERVICE_STOP, state.stop);
            enable_command(popup.as_raw(), IDM_SERVICE_PAUSE, state.pause);
            enable_command(popup.as_raw(), IDM_SERVICE_CONTINUE, state.continue_service);
            enable_command(popup.as_raw(), IDM_SERVICE_GOTOPROCESS, state.go_to_process);
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
            let anchor = GetDlgItem(self.hwnd, i32::from(IDM_SERVICE_GOTOPROCESS));
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
                i32::from(IDM_SERVICE_START),
                i32::from(IDM_SERVICE_STOP),
                i32::from(IDM_SERVICE_PAUSE),
                i32::from(IDM_SERVICE_CONTINUE),
                i32::from(IDM_SERVICE_GOTOPROCESS),
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
            "taskmgr-rs-service-sampler",
            PWM_SERVICE_WORKER_COMPLETE,
            merge_service_requests,
            perform,
        )?);
        Ok(())
    }

    fn refresh(&mut self) {
        self.drain_worker();
        // A control owns the one coalesced slot until its completion. Do not queue another
        // refresh behind it: an action panic must leave the worker idle so it can be reported
        // as a failed action, rather than being mistaken for a failed background refresh.
        if self.pending_control.is_none() {
            self.submit(ServiceRequest::Refresh);
        }
    }

    fn request_control(&mut self, action: ServiceControlAction) {
        let state = self.command_state();
        let allowed = match action {
            ServiceControlAction::Start => state.start,
            ServiceControlAction::Stop => state.stop,
            ServiceControlAction::Pause => state.pause,
            ServiceControlAction::Continue => state.continue_service,
        };
        let Some(service_name) = self.selected_service.clone() else {
            self.beep();
            return;
        };
        if !allowed {
            self.beep();
            return;
        }
        self.submit(ServiceRequest::Control {
            service_name,
            action,
        });
    }

    fn submit(&mut self, request: ServiceRequest) {
        let action = match &request {
            ServiceRequest::Control { action, .. } => Some(*action),
            ServiceRequest::Refresh => None,
        };
        let Some(worker) = self.worker.as_mut() else {
            self.report_submit_failure(ERROR_BROKEN_PIPE, action);
            return;
        };
        if let Err(error) = worker.request(request, self.hwnd) {
            self.report_submit_failure(error, action);
        } else if let Some(action) = action {
            self.pending_control = Some(action);
            self.update_ui_state();
        }
    }

    fn report_submit_failure(&mut self, error: u32, action: Option<ServiceControlAction>) {
        if let Some(action) = action {
            self.show_action_failure(action, ServiceActionFailure::Api(error));
        } else {
            self.set_refresh_error(error);
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
            self.set_refresh_error(error);
            if self
                .worker
                .as_ref()
                .is_some_and(|worker| !worker.is_in_flight() && !worker.has_pending())
                && let Some(action) = self.pending_control.take()
            {
                self.update_ui_state();
                self.show_action_failure(action, ServiceActionFailure::Api(error));
            }
        }
    }

    fn apply_completion(&mut self, completion: ServiceWorkerCompletion) {
        match completion {
            ServiceWorkerCompletion::Snapshot(result) => match result {
                Ok(collection) => self.apply_collection(collection),
                Err(error) => self.set_refresh_error(error),
            },
            ServiceWorkerCompletion::Action(report) => {
                self.pending_control = None;
                if let Ok(status) = report.outcome
                    && let Some(row) = self
                        .rows
                        .iter_mut()
                        .find(|row| row.service_name.eq_ignore_ascii_case(&report.service_name))
                {
                    row.status = status;
                    row.status_text = status_text(status).to_string();
                    row.status_sort = row.status_text.to_lowercase();
                    if !model::service_has_valid_process_id(status) {
                        row.process = None;
                        row.process_text.clear();
                        row.process_sort.clear();
                    }
                    self.sort_rows();
                    self.rebuild_list(refresh_scroll_policy());
                }
                self.update_ui_state();
                if let Err(failure) = report.outcome {
                    self.show_action_failure(report.action, failure);
                }
                self.submit(ServiceRequest::Refresh);
            }
        }
    }

    fn apply_collection(&mut self, collection: ServiceCollection) {
        self.last_refresh_error = None;
        for error in &collection.row_errors {
            if !self.last_row_errors.contains(error) {
                record_win32_error_with_fields(
                    "service row metadata",
                    error.code,
                    &[
                        Field::text("service", &error.service_name),
                        Field::text("field", error.field),
                    ],
                );
            }
        }
        self.last_row_errors = collection.row_errors;
        let previous = self.selected_service.clone();
        let pid_label = text(TextKey::ServiceProcessPid);
        self.rows = collection
            .services
            .into_iter()
            .map(|record| service_row(record, pid_label))
            .collect();
        self.sort_rows();
        self.selected_service = retained_service_name(
            previous.as_deref(),
            self.rows.iter().map(|row| row.service_name.as_str()),
        )
        .map(str::to_string);
        self.rebuild_list(refresh_scroll_policy());
    }

    fn sort_rows(&mut self) {
        let column = self.sort_column;
        let ascending = self.sort_ascending;
        self.rows.sort_by(|left, right| {
            let ordering = match column {
                1 => left.description_sort.cmp(&right.description_sort),
                2 => left.status_sort.cmp(&right.status_sort),
                3 => left.startup_sort.cmp(&right.startup_sort),
                4 => left.process_sort.cmp(&right.process_sort),
                _ => left.name_sort.cmp(&right.name_sort),
            };
            if ascending {
                ordering
            } else {
                ordering.reverse()
            }
        });
    }

    fn rebuild_list(&mut self, scroll_policy: ServiceScrollPolicy) {
        // SAFETY: the list belongs to this page. Item count uses LVSICF_NOSCROLL so a refresh does
        // not move the viewport. Selection is restored without LVM_ENSUREVISIBLE.
        let ServiceScrollPolicy::PreserveViewport = scroll_policy;
        unsafe {
            let list = self.list_hwnd();
            if list.is_null() {
                return;
            }
            let selected_index = self.selected_service.as_deref().and_then(|name| {
                self.rows
                    .iter()
                    .position(|row| row.service_name.eq_ignore_ascii_case(name))
            });
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
                self.selected_service = None;
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
            1 => row.description.as_str(),
            2 => row.status_text.as_str(),
            3 => row.startup_text.as_str(),
            4 => row.process_text.as_str(),
            _ => row.service_name.as_str(),
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
            let titles = service_column_titles();
            let widths = [140, 220, 96, 110, 120];
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

    fn current_selected_service(&self) -> Option<String> {
        // SAFETY: selection query targets this page's list.
        unsafe {
            let list = self.list_hwnd();
            if list.is_null() {
                return None;
            }
            let index = SendMessageW(list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize);
            let index = usize::try_from(index).ok()?;
            self.rows.get(index).map(|row| row.service_name.clone())
        }
    }

    fn selected_input(&self) -> Option<ServiceCommandInput> {
        let name = self.selected_service.as_deref()?;
        let row = self
            .rows
            .iter()
            .find(|row| row.service_name.eq_ignore_ascii_case(name))?;
        Some(ServiceCommandInput {
            status: row.status,
            startup: row.startup,
            accepts_stop: row.accepts_stop,
            accepts_pause_continue: row.accepts_pause_continue,
            process: row.process,
        })
    }

    fn update_ui_state(&self) {
        let state = self.command_state();
        enable_button(self.hwnd, IDM_SERVICE_START, state.start);
        enable_button(self.hwnd, IDM_SERVICE_STOP, state.stop);
        enable_button(self.hwnd, IDM_SERVICE_PAUSE, state.pause);
        enable_button(self.hwnd, IDM_SERVICE_CONTINUE, state.continue_service);
        enable_button(self.hwnd, IDM_SERVICE_GOTOPROCESS, state.go_to_process);
    }

    fn go_to_process(&self) {
        let state = self.command_state();
        let Some(identity) = self
            .selected_input()
            .and_then(|input| input.process.filter(|identity| identity.is_verified()))
        else {
            self.beep();
            return;
        };
        if !state.go_to_process || self.main_hwnd.is_null() {
            self.beep();
            return;
        }
        let payload = FindProcPayload {
            pid: identity.pid,
            creation_time_100ns: identity.creation_time_100ns,
        };
        // SAFETY: the payload lives until the synchronous find-process message returns. The main
        // window reads both fields through that pointer and does not keep it.
        unsafe {
            SendMessageW(
                self.main_hwnd,
                WM_FINDPROC,
                identity.pid as usize,
                find_proc_lparam(&payload),
            );
        }
    }

    fn show_action_failure(&self, action: ServiceControlAction, failure: ServiceActionFailure) {
        let summary = match action {
            ServiceControlAction::Start => text(TextKey::ServiceStartFailed),
            ServiceControlAction::Stop => text(TextKey::ServiceStopFailed),
            ServiceControlAction::Pause => text(TextKey::ServicePauseFailed),
            ServiceControlAction::Continue => text(TextKey::ServiceContinueFailed),
        };
        let body = match failure {
            ServiceActionFailure::Api(code) => {
                format!("{summary}\n\n{} {code}", text(TextKey::Win32ErrorPrefix))
            }
            ServiceActionFailure::StateUnchanged { .. } => {
                format!("{summary}\n\n{}", text(TextKey::ServiceStateUnchanged))
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

    fn set_refresh_error(&mut self, error: u32) {
        if self.last_refresh_error != Some(error) {
            record_win32_error("service refresh", error);
        }
        self.last_refresh_error = Some(error);
    }

    fn beep(&self) {
        // SAFETY: MessageBeep has no pointer inputs.
        unsafe { MessageBeep(0) };
    }

    fn list_hwnd(&self) -> HWND {
        // SAFETY: this queries a child of the page dialog. Null means the control is absent.
        unsafe { GetDlgItem(self.hwnd, IDC_SERVICELIST) }
    }
}

fn service_row(record: self::sampler::ServiceRecord, pid_label: &str) -> ServiceRow {
    let status_text = status_text(record.status).to_string();
    let startup_text = startup_text(record.startup).to_string();
    let process_text = owning_process_label(record.process.as_ref(), pid_label);
    let process = record
        .process
        .as_ref()
        .and_then(|process| process.identity.is_verified().then_some(process.identity));
    ServiceRow {
        name_sort: record.service_name.to_lowercase(),
        service_name: record.service_name,
        description_sort: record.description.to_lowercase(),
        description: record.description,
        status: record.status,
        status_sort: status_text.to_lowercase(),
        status_text,
        startup: record.startup,
        accepts_stop: record.accepts_stop,
        accepts_pause_continue: record.accepts_pause_continue,
        startup_sort: startup_text.to_lowercase(),
        startup_text,
        process_sort: process_text.to_lowercase(),
        process_text,
        process,
    }
}

fn status_text(status: ServiceRunState) -> &'static str {
    match status {
        ServiceRunState::Stopped => text(TextKey::ServiceStatusStopped),
        ServiceRunState::StartPending => text(TextKey::ServiceStatusStartPending),
        ServiceRunState::StopPending => text(TextKey::ServiceStatusStopPending),
        ServiceRunState::Running => text(TextKey::Running),
        ServiceRunState::ContinuePending => text(TextKey::ServiceStatusContinuePending),
        ServiceRunState::PausePending => text(TextKey::ServiceStatusPausePending),
        ServiceRunState::Paused => text(TextKey::ServiceStatusPaused),
        ServiceRunState::Unknown(_) => text(TextKey::Unknown),
    }
}

fn startup_text(startup: ServiceStartupType) -> &'static str {
    match startup {
        ServiceStartupType::Boot => text(TextKey::ServiceStartupBoot),
        ServiceStartupType::System => text(TextKey::ServiceStartupSystem),
        ServiceStartupType::Automatic => text(TextKey::ServiceStartupAutomatic),
        ServiceStartupType::Manual => text(TextKey::ServiceStartupManual),
        ServiceStartupType::Disabled => text(TextKey::ServiceStartupDisabled),
        ServiceStartupType::Unknown(_) => text(TextKey::Unknown),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    #[test]
    fn a_confirmed_stop_clears_the_old_owner_even_if_the_refresh_cannot_start() {
        let mut page = ServicePageState {
            rows: vec![service_row(
                self::sampler::ServiceRecord {
                    service_name: "test-service".to_string(),
                    description: String::new(),
                    status: ServiceRunState::Running,
                    startup: ServiceStartupType::Manual,
                    accepts_stop: true,
                    accepts_pause_continue: true,
                    process: Some(model::OwningProcess {
                        identity: ProcIdentity::new(44, 100),
                        image_name: Some("service.exe".to_string()),
                    }),
                },
                "PID",
            )],
            selected_service: Some("test-service".to_string()),
            pending_control: Some(ServiceControlAction::Stop),
            // No worker is installed: the follow-up refresh must fail without changing rows.
            ..ServicePageState::default()
        };
        page.apply_completion(ServiceWorkerCompletion::Action(
            self::sampler::ServiceActionReport {
                service_name: "test-service".to_string(),
                action: ServiceControlAction::Stop,
                outcome: Ok(ServiceRunState::Stopped),
            },
        ));
        assert_eq!(page.rows[0].status, ServiceRunState::Stopped);
        assert_eq!(page.rows[0].process, None);
        assert!(page.rows[0].process_text.is_empty());
        assert!(page.rows[0].process_sort.is_empty());
        assert!(!page.command_state().go_to_process);
        assert_eq!(page.last_refresh_error, Some(ERROR_BROKEN_PIPE));
    }

    #[test]
    fn a_queued_service_control_cannot_be_overwritten_by_another_selection() {
        let (started_sender, started_receiver) = crossbeam_channel::bounded(1);
        let (resume_sender, resume_receiver) = crossbeam_channel::bounded(1);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let worker_observed = Arc::clone(&observed);
        // All service work is synthetic. No SCM handle is opened and no service is changed.
        let worker = SingleFlightWorker::spawn(
            "taskmgr-service-control-queue-test",
            0,
            merge_service_requests,
            move |request: ServiceRequest| {
                let first = {
                    let mut observed = worker_observed.lock().unwrap();
                    let first = observed.is_empty();
                    observed.push(request.clone());
                    first
                };
                if first {
                    started_sender.send(()).unwrap();
                    resume_receiver
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap();
                }
                match request {
                    ServiceRequest::Refresh => {
                        ServiceWorkerCompletion::Snapshot(Ok(ServiceCollection {
                            services: Vec::new(),
                            row_errors: Vec::new(),
                        }))
                    }
                    ServiceRequest::Control {
                        service_name,
                        action,
                    } => ServiceWorkerCompletion::Action(self::sampler::ServiceActionReport {
                        service_name,
                        action,
                        outcome: Ok(ServiceRunState::StartPending),
                    }),
                }
            },
        )
        .unwrap();
        let mut page = ServicePageState {
            worker: Some(worker),
            rows: vec![service_row(
                self::sampler::ServiceRecord {
                    service_name: "first".to_string(),
                    description: String::new(),
                    status: ServiceRunState::Stopped,
                    startup: ServiceStartupType::Manual,
                    accepts_stop: false,
                    accepts_pause_continue: false,
                    process: None,
                },
                "PID",
            )],
            selected_service: Some("first".to_string()),
            ..ServicePageState::default()
        };
        page.submit(ServiceRequest::Refresh);
        started_receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        page.request_control(ServiceControlAction::Start);
        assert_eq!(page.pending_control, Some(ServiceControlAction::Start));

        page.rows[0].service_name = "second".to_string();
        page.selected_service = Some("second".to_string());
        assert!(!page.command_state().start);
        assert!(!page.command_state().stop);
        page.request_control(ServiceControlAction::Start);
        resume_sender.send(()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while page.pending_control.is_some() {
            assert!(
                Instant::now() < deadline,
                "control completion was not delivered"
            );
            page.drain_worker();
            std::thread::yield_now();
        }
        let observed = observed.lock().unwrap();
        let controls = observed
            .iter()
            .filter(|request| matches!(request, ServiceRequest::Control { .. }))
            .collect::<Vec<_>>();
        assert_eq!(
            controls,
            vec![&ServiceRequest::Control {
                service_name: "first".to_string(),
                action: ServiceControlAction::Start,
            }]
        );
    }
}

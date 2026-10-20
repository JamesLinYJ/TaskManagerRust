// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 新建任务对话框
//
//   文件:       src/app/new_task.rs
//
//   日期:       2026年10月20日
//   环境:       Linux 6.12.94+ x86_64；Rust 1.99.1（58cf8f9c3，2026-10-13）
//   作者:       Grok 4.7
// --------------------------------------------------------------------------

//! “文件 -> 新建任务”模态对话框。
//!
//! 对话框收集命令行，以及是否通过标准提升路径启动。`ShellExecuteExW` 在独立线程
//! 执行，避免 UAC 同意界面卡住主消息循环。成功只表示外壳接受了请求，不表示新
//! 任务仍然存活；失败立刻在这个对话框上报告。

use std::ffi::OsString;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStringExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::null;
use std::sync::{Arc, Mutex};
use std::thread;

use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows_sys::Win32::Foundation::{
    ERROR_GEN_FAILURE, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER, ERROR_NOT_ENOUGH_MEMORY,
    GetLastError, HINSTANCE, HWND, LPARAM, SetLastError, WPARAM,
};
use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows_sys::Win32::UI::Controls::Dialogs::{
    CommDlgExtendedError, GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY,
    OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::Controls::{BST_CHECKED, EM_SETLIMITTEXT, IsDlgButtonChecked};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::Shell::{
    SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EndDialog, GetDlgItem, GetWindowTextLengthW, GetWindowTextW, IDCANCEL, IDOK,
    MB_ICONEXCLAMATION, MB_ICONSTOP, MB_OK, MessageBoxW, PostMessageW, SW_SHOWNORMAL,
    SetDlgItemTextW, WM_CLOSE, WM_COMMAND, WM_INITDIALOG,
};

use crate::infrastructure::diagnostics::{self, Field, Level};
use crate::infrastructure::native::{
    get_window_userdata, loword, record_hresult_error, record_win32_error,
    record_win32_error_with_fields, set_window_userdata, to_wide_null,
};
use crate::ui::dialogs::dialog_box;
use crate::ui::localization::{TextKey, localize_dialog, text};
use crate::ui::resource_ids::*;

const PROGRAM_PATTERN: &str = "*.exe;*.com;*.bat;*.cmd;*.msc";
const ALL_FILES_PATTERN: &str = "*.*";
const MAX_EXPANDED_COMMAND: usize = 32_768;
const COMMAND_LIMIT: usize = 32_767;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NewTaskError {
    Win32(u32),
    Shell(i32),
    Hresult(i32),
}

#[derive(Debug, Eq, PartialEq)]
struct ShellTarget {
    file: String,
    parameters: String,
}

struct NewTaskDialog {
    result: Arc<Mutex<Option<Result<(), NewTaskError>>>>,
    busy: bool,
}

struct WorkerCom;

impl WorkerCom {
    fn initialize() -> Result<Self, i32> {
        // A fresh worker thread has no apartment. S_FALSE still owns one CoUninitialize.
        let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if result.is_ok() {
            Ok(Self)
        } else {
            Err(result.0)
        }
    }
}

impl Drop for WorkerCom {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

pub(super) fn show(hinstance: HINSTANCE, parent: HWND) -> Result<(), u32> {
    let mut context = NewTaskDialog {
        result: Arc::new(Mutex::new(None)),
        busy: false,
    };
    // DialogBox runs a nested loop. Drop the ambient command id so timers are not attributed
    // to the menu command that opened this dialog.
    diagnostics::without_operation_id(|| {
        dialog_box(
            hinstance,
            IDD_NEWTASK,
            parent,
            Some(new_task_dialog_proc),
            &mut context as *mut NewTaskDialog as LPARAM,
        )
    })?;
    Ok(())
}

unsafe extern "system" fn new_task_dialog_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    // Safety: the context pointer is the stack value owned by `show` for the modal call.
    unsafe {
        match message {
            WM_INITDIALOG => {
                set_window_userdata(hwnd, lparam);
                localize_dialog(hwnd, IDD_NEWTASK);
                let edit = GetDlgItem(hwnd, IDC_NEWTASK_COMMAND);
                if !edit.is_null() {
                    limit_command_text(edit);
                }
                1
            }
            WM_CLOSE => {
                if context(hwnd).busy {
                    1
                } else {
                    0
                }
            }
            WM_COMMAND => {
                if context(hwnd).busy {
                    return 1;
                }
                match i32::from(loword(wparam)) {
                    IDCANCEL => {
                        EndDialog(hwnd, IDCANCEL as isize);
                        1
                    }
                    IDOK => {
                        begin_launch(hwnd);
                        1
                    }
                    IDC_NEWTASK_BROWSE => {
                        browse_for_program(hwnd);
                        1
                    }
                    _ => 0,
                }
            }
            PWM_NEWTASK_COMPLETE => {
                complete_launch(hwnd);
                1
            }
            _ => 0,
        }
    }
}

fn limit_command_text(edit: HWND) {
    // Safety: `edit` is the live command control. EM_SETLIMITTEXT only stores a length.
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW(
            edit,
            EM_SETLIMITTEXT,
            COMMAND_LIMIT,
            0,
        );
    }
}

fn begin_launch(hwnd: HWND) {
    let command = match control_text(hwnd, IDC_NEWTASK_COMMAND) {
        Ok(command) => command,
        Err(error) => {
            record_win32_error("new task command", error);
            show_stop(hwnd, &failure_text(NewTaskError::Win32(error)));
            return;
        }
    };
    if command.trim().is_empty() {
        show_exclamation(hwnd, text(TextKey::NewTaskEmpty));
        focus_command(hwnd);
        return;
    }
    let administrative = unsafe { IsDlgButtonChecked(hwnd, IDC_NEWTASK_ADMIN) } == BST_CHECKED;
    let slot = {
        let dialog = context(hwnd);
        dialog.busy = true;
        Arc::clone(&dialog.result)
    };
    set_controls_enabled(hwnd, false);
    let command = command.trim().to_string();
    let window = hwnd as isize;
    let spawn = thread::Builder::new()
        .name("taskmgr-rs-new-task".to_string())
        .spawn(move || {
            let owner = window as HWND;
            let outcome = match catch_unwind(AssertUnwindSafe(|| {
                launch_new_task(&command, administrative, owner)
            })) {
                Ok(outcome) => outcome,
                Err(_) => {
                    record_win32_error("new task worker panic", ERROR_GEN_FAILURE);
                    Err(NewTaskError::Win32(ERROR_GEN_FAILURE))
                }
            };
            if let Ok(mut guard) = slot.lock() {
                *guard = Some(outcome);
            }
            // Safety: the HWND is an opaque target. A failure means the dialog is already gone.
            unsafe {
                PostMessageW(window as HWND, PWM_NEWTASK_COMPLETE, 0, 0);
            }
        });
    if let Err(error) = spawn {
        context(hwnd).busy = false;
        set_controls_enabled(hwnd, true);
        let code = error
            .raw_os_error()
            .unwrap_or(ERROR_NOT_ENOUGH_MEMORY as i32) as u32;
        record_win32_error("new task worker", code);
        show_stop(hwnd, &failure_text(NewTaskError::Win32(code)));
        focus_command(hwnd);
    }
}

fn complete_launch(hwnd: HWND) {
    let outcome = {
        let dialog = context(hwnd);
        dialog.busy = false;
        match dialog.result.lock() {
            Ok(mut slot) => slot.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        }
    };
    match outcome {
        Some(Ok(())) => {
            // Acceptance is not evidence that a process is still running.
            unsafe { EndDialog(hwnd, IDOK as isize) };
        }
        Some(Err(error)) => {
            set_controls_enabled(hwnd, true);
            show_stop(hwnd, &failure_text(error));
            focus_command(hwnd);
        }
        None => {
            set_controls_enabled(hwnd, true);
            show_stop(hwnd, &failure_text(NewTaskError::Win32(ERROR_GEN_FAILURE)));
            focus_command(hwnd);
        }
    }
}

fn launch_new_task(
    command_line: &str,
    administrative: bool,
    owner: HWND,
) -> Result<(), NewTaskError> {
    let _apartment = WorkerCom::initialize().map_err(|error| {
        record_hresult_error("new task COM initialization", error);
        NewTaskError::Hresult(error)
    })?;
    let expanded = expand_environment(command_line).map_err(|error| {
        record_win32_error("new task environment expansion", error);
        NewTaskError::Win32(error)
    })?;
    let Some(target) = split_shell_command(&expanded) else {
        record_win32_error("new task command line", ERROR_INVALID_PARAMETER);
        return Err(NewTaskError::Win32(ERROR_INVALID_PARAMETER));
    };
    let outcome = shell_execute(&target, administrative, owner);
    match outcome {
        Ok(()) => {
            diagnostics::event(
                Level::Info,
                "new_task.launch_accepted",
                "new-task",
                "the shell accepted the new task; its lifetime is not tracked",
                &[Field::boolean("administrative", administrative)],
            );
            Ok(())
        }
        Err(NewTaskError::Win32(code)) => {
            record_win32_error_with_fields(
                "new task launch",
                code,
                &[Field::boolean("administrative", administrative)],
            );
            Err(NewTaskError::Win32(code))
        }
        Err(NewTaskError::Shell(code)) => {
            diagnostics::event(
                Level::Error,
                "new_task.launch_failed",
                "new-task",
                "ShellExecuteExW returned a documented failure code",
                &[
                    Field::boolean("administrative", administrative),
                    Field::signed("shell_execute_code", i64::from(code)),
                ],
            );
            Err(NewTaskError::Shell(code))
        }
        Err(error) => Err(error),
    }
}

fn shell_execute(
    target: &ShellTarget,
    administrative: bool,
    owner: HWND,
) -> Result<(), NewTaskError> {
    let verb = to_wide_null(if administrative { "runas" } else { "open" });
    let file = to_wide_null(&target.file);
    let parameters = to_wide_null(&target.parameters);
    let mut info = unsafe { zeroed::<SHELLEXECUTEINFOW>() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    // NOASYNC keeps the call on this worker, which has no message loop. NO_UI suppresses the
    // shell's own error dialog so this task dialog can report the failure itself. The call does
    // not request a process handle and does not wait for the target to exit. `owner` is only the
    // elevation UI owner.
    info.fMask = SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
    info.hwnd = owner;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = if target.parameters.is_empty() {
        null()
    } else {
        parameters.as_ptr()
    };
    info.nShow = SW_SHOWNORMAL;
    let (returned, last_error, instance) = unsafe {
        SetLastError(0);
        let returned = ShellExecuteExW(&mut info);
        let last_error = if returned == 0 { GetLastError() } else { 0 };
        // hInstApp is a legacy status code, not an owned module handle.
        (returned != 0, last_error, info.hInstApp as isize)
    };
    classify_shell_execute(returned, instance, last_error)
}

fn expand_environment(command: &str) -> Result<String, u32> {
    let source = to_wide_null(command);
    let mut capacity = 1024usize;
    loop {
        let mut buffer = vec![0u16; capacity];
        let written = unsafe {
            ExpandEnvironmentStringsW(source.as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32)
        };
        if written == 0 {
            let error = unsafe { GetLastError() };
            return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
        }
        let written = written as usize;
        if written <= buffer.len() {
            let end = written.saturating_sub(1);
            return Ok(String::from_utf16_lossy(&buffer[..end]));
        }
        if written > MAX_EXPANDED_COMMAND {
            return Err(ERROR_INSUFFICIENT_BUFFER);
        }
        capacity = written;
    }
}

fn browse_for_program(hwnd: HWND) {
    match choose_program(hwnd) {
        Ok(Some(path)) => {
            set_control_text(hwnd, IDC_NEWTASK_COMMAND, &command_text_for_path(&path));
            focus_command(hwnd);
        }
        Ok(None) => {}
        Err(error) => {
            record_win32_error("new task browse", error);
            show_stop(hwnd, &failure_text(NewTaskError::Win32(error)));
        }
    }
}

fn choose_program(hwnd: HWND) -> Result<Option<String>, u32> {
    let mut file_buffer = vec![0u16; MAX_EXPANDED_COMMAND];
    let filter = browse_filter();
    let title = to_wide_null(text(TextKey::NewTaskTitle));
    let mut dialog = unsafe { zeroed::<OPENFILENAMEW>() };
    dialog.lStructSize = size_of::<OPENFILENAMEW>() as u32;
    dialog.hwndOwner = hwnd;
    dialog.lpstrFilter = filter.as_ptr();
    dialog.nFilterIndex = 1;
    dialog.lpstrFile = file_buffer.as_mut_ptr();
    dialog.nMaxFile = file_buffer.len() as u32;
    dialog.lpstrTitle = title.as_ptr();
    dialog.Flags = OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY;

    if unsafe { GetOpenFileNameW(&mut dialog) } == 0 {
        let error = unsafe { CommDlgExtendedError() };
        return if error == 0 { Ok(None) } else { Err(error) };
    }
    let length = file_buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(file_buffer.len());
    if length == 0 {
        return Err(ERROR_GEN_FAILURE);
    }
    Ok(Some(
        OsString::from_wide(&file_buffer[..length])
            .to_string_lossy()
            .into_owned(),
    ))
}

fn browse_filter() -> Vec<u16> {
    let mut filter = Vec::new();
    append_cstr(
        &mut filter,
        &format!(
            "{} ({PROGRAM_PATTERN})",
            text(TextKey::NewTaskProgramFilter)
        ),
    );
    append_cstr(&mut filter, PROGRAM_PATTERN);
    append_cstr(
        &mut filter,
        &format!(
            "{} ({ALL_FILES_PATTERN})",
            text(TextKey::NewTaskAllFilesFilter)
        ),
    );
    append_cstr(&mut filter, ALL_FILES_PATTERN);
    filter.push(0);
    filter
}

fn append_cstr(buffer: &mut Vec<u16>, value: &str) {
    buffer.extend(value.encode_utf16());
    buffer.push(0);
}

fn set_controls_enabled(hwnd: HWND, enabled: bool) {
    let enabled = i32::from(enabled);
    unsafe {
        for id in [
            IDC_NEWTASK_COMMAND,
            IDC_NEWTASK_BROWSE,
            IDC_NEWTASK_ADMIN,
            IDOK,
            IDCANCEL,
        ] {
            EnableWindow(GetDlgItem(hwnd, id), enabled);
        }
    }
}

fn focus_command(hwnd: HWND) {
    let edit = unsafe { GetDlgItem(hwnd, IDC_NEWTASK_COMMAND) };
    if !edit.is_null() {
        unsafe { SetFocus(edit) };
    }
}

fn control_text(hwnd: HWND, control_id: i32) -> Result<String, u32> {
    let control = unsafe { GetDlgItem(hwnd, control_id) };
    if control.is_null() {
        return Err(ERROR_GEN_FAILURE);
    }
    unsafe { SetLastError(0) };
    let length = unsafe { GetWindowTextLengthW(control) };
    if length <= 0 {
        let error = unsafe { GetLastError() };
        return if error == 0 {
            Ok(String::new())
        } else {
            Err(error)
        };
    }
    let mut buffer = vec![0u16; usize::try_from(length).unwrap_or(0).saturating_add(1)];
    let copied = unsafe { GetWindowTextW(control, buffer.as_mut_ptr(), buffer.len() as i32) };
    if copied <= 0 {
        let error = unsafe { GetLastError() };
        return if error == 0 {
            Ok(String::new())
        } else {
            Err(error)
        };
    }
    let copied = usize::try_from(copied).unwrap_or(0).min(buffer.len());
    Ok(String::from_utf16_lossy(&buffer[..copied]))
}

fn set_control_text(hwnd: HWND, control_id: i32, value: &str) {
    let wide = to_wide_null(value);
    unsafe { SetDlgItemTextW(hwnd, control_id, wide.as_ptr()) };
}

fn show_stop(hwnd: HWND, body: &str) {
    show_message(hwnd, body, MB_ICONSTOP);
}

fn show_exclamation(hwnd: HWND, body: &str) {
    show_message(hwnd, body, MB_ICONEXCLAMATION);
}

fn show_message(hwnd: HWND, body: &str, icon: u32) {
    let title = to_wide_null(text(TextKey::NewTaskTitle));
    let body = to_wide_null(body);
    unsafe {
        MessageBoxW(hwnd, body.as_ptr(), title.as_ptr(), MB_OK | icon);
    }
}

fn failure_text(error: NewTaskError) -> String {
    let detail = match error {
        NewTaskError::Win32(code) => format!("{code:#010X}"),
        NewTaskError::Shell(code) => format!("ShellExecuteEx {code}"),
        NewTaskError::Hresult(code) => format!("HRESULT {code:#010X}"),
    };
    format!("{} ({detail})", text(TextKey::NewTaskFailed))
}

fn context<'a>(hwnd: HWND) -> &'a mut NewTaskDialog {
    // Safety: WM_INITDIALOG stored the pointer from `show`, and that stack frame outlives the
    // modal dialog. Callers drop the reference before EndDialog or MessageBox can re-enter.
    unsafe { &mut *(get_window_userdata(hwnd) as *mut NewTaskDialog) }
}

/// Splits a command the way `PathGetArgs` / `PathUnquoteSpaces` do.
///
/// Spaces inside quotes stay with the program path. The returned file has one surrounding quote
/// pair removed. Parameters keep the user's quoting.
fn split_shell_command(command_line: &str) -> Option<ShellTarget> {
    let trimmed = command_line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut in_quotes = false;
    let mut split_at = None;
    for (index, ch) in trimmed.char_indices() {
        if ch == '"' {
            in_quotes = !in_quotes;
        } else if ch == ' ' && !in_quotes {
            split_at = Some(index);
            break;
        }
    }
    let (file, parameters) = match split_at {
        Some(index) => (&trimmed[..index], trimmed[index..].trim_start()),
        None => (trimmed, ""),
    };
    let file = unquote_spaces(file);
    if file.is_empty() {
        None
    } else {
        Some(ShellTarget {
            file,
            parameters: parameters.to_string(),
        })
    }
}

fn unquote_spaces(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

fn command_text_for_path(path: &str) -> String {
    let trimmed = path.trim();
    let already_quoted = trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"');
    if !already_quoted && trimmed.contains([' ', '\t']) {
        format!("\"{trimmed}\"")
    } else {
        trimmed.to_string()
    }
}

/// `Ok(())` means the shell accepted the launch. It carries no process handle or pid.
fn classify_shell_execute(
    api_succeeded: bool,
    instance_code: isize,
    last_error: u32,
) -> Result<(), NewTaskError> {
    // hInstApp values 1 through 32 are the documented shell errors. A successful BOOL with no
    // such code is acceptance only: this result has no process id or handle.
    let shell_error = i32::try_from(instance_code)
        .ok()
        .filter(|code| (1..=32).contains(code));
    if api_succeeded && shell_error.is_none() {
        return Ok(());
    }
    if last_error != 0 {
        return Err(NewTaskError::Win32(last_error));
    }
    match shell_error {
        Some(code) => Err(NewTaskError::Shell(code)),
        None => Err(NewTaskError::Win32(ERROR_GEN_FAILURE)),
    }
}

#[cfg(test)]
mod tests {
    use super::{NewTaskError, classify_shell_execute, command_text_for_path, split_shell_command};

    #[test]
    fn command_lines_split_on_unquoted_spaces() {
        assert!(split_shell_command("   ").is_none());
        assert!(split_shell_command("\"\"").is_none());

        let simple = split_shell_command("notepad").unwrap();
        assert_eq!(simple.file, "notepad");
        assert!(simple.parameters.is_empty());

        let args = split_shell_command("  notepad   file.txt  ").unwrap();
        assert_eq!(args.file, "notepad");
        assert_eq!(args.parameters, "file.txt");

        let quoted =
            split_shell_command(r#""C:\Program Files\app.exe" /foo "my file.txt""#).unwrap();
        assert_eq!(quoted.file, r"C:\Program Files\app.exe");
        assert_eq!(quoted.parameters, r#"/foo "my file.txt""#);

        let quoted_only = split_shell_command(r#""C:\Program Files\app.exe""#).unwrap();
        assert_eq!(quoted_only.file, r"C:\Program Files\app.exe");
        assert!(quoted_only.parameters.is_empty());
    }

    #[test]
    fn browse_paths_quote_only_when_needed() {
        assert_eq!(
            command_text_for_path(r"C:\Windows\notepad.exe"),
            r"C:\Windows\notepad.exe"
        );
        assert_eq!(
            command_text_for_path(r"C:\Program Files\app.exe"),
            r#""C:\Program Files\app.exe""#
        );
        assert_eq!(
            command_text_for_path(r#""C:\Program Files\app.exe""#),
            r#""C:\Program Files\app.exe""#
        );
    }

    #[test]
    fn shell_acceptance_does_not_describe_a_live_process() {
        assert_eq!(classify_shell_execute(true, 42, 0), Ok(()));
        assert_eq!(classify_shell_execute(true, 0, 0), Ok(()));
        assert_eq!(classify_shell_execute(true, 33, 5), Ok(()));
        assert_eq!(
            classify_shell_execute(true, 32, 0),
            Err(NewTaskError::Shell(32))
        );
        assert_eq!(
            classify_shell_execute(false, 42, 1223),
            Err(NewTaskError::Win32(1223))
        );
        assert_eq!(
            classify_shell_execute(false, 31, 0),
            Err(NewTaskError::Shell(31))
        );
        assert_eq!(
            classify_shell_execute(false, 33, 0),
            Err(NewTaskError::Win32(
                windows_sys::Win32::Foundation::ERROR_GEN_FAILURE
            ))
        );
    }
}

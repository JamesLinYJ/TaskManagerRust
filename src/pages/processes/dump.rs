// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 进程转储
//
//   文件:       src/pages/processes/dump.rs
//
//   日期:       2026年10月20日
//   环境:       Ubuntu 24.04.5 LTS x86_64；Linux 内核 6.12.94+；Rust 1.99.1
//   作者:       Grok 4.7
// --------------------------------------------------------------------------

//! Create Dump File for one verified process.
//!
//! The save dialog stays on the UI thread. `MiniDumpWriteDump` runs on a
//! persistent worker and only after `open_process_for_identity` confirms the
//! pid and creation time. A pid-only identity never reaches `OpenProcess`.

use std::ffi::{OsStr, OsString};
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_BUSY, ERROR_FILE_NOT_FOUND, ERROR_GEN_FAILURE,
    ERROR_INVALID_PARAMETER, ERROR_PATH_NOT_FOUND, GENERIC_READ, GENERIC_WRITE, GetLastError,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CREATE_ALWAYS, CreateFileW, DeleteFileW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_SHARE_READ, FlushFileBuffers, GetFileAttributesW, GetFileInformationByHandle,
    INVALID_FILE_ATTRIBUTES, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows_sys::Win32::System::Diagnostics::Debug::{MiniDumpWithFullMemory, MiniDumpWriteDump};
use windows_sys::Win32::System::Threading::{PROCESS_QUERY_INFORMATION, PROCESS_VM_READ};
use windows_sys::Win32::UI::Controls::Dialogs::{
    CommDlgExtendedError, GetSaveFileNameW, OFN_EXPLORER, OFN_HIDEREADONLY, OFN_OVERWRITEPROMPT,
    OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MessageBoxW};

use super::ProcessPageState;
use crate::infrastructure::native::{OwnedHandle, record_win32_error, to_wide_null};
use crate::infrastructure::worker::{SingleFlightWorker, keep_pending};
use crate::system::process_identity::{ProcIdentity, open_process_for_identity};
use crate::ui::localization::{TextKey, text};
use crate::ui::resource_ids::PWM_PROC_DUMP_COMPLETE;

/// Process rights documented for `MiniDumpWriteDump`.
///
/// `PROCESS_QUERY_INFORMATION` also covers `GetProcessTimes`, which
/// `open_process_for_identity` uses to reject a reused PID.
pub(super) const PROCESS_DUMP_ACCESS: u32 = PROCESS_QUERY_INFORMATION | PROCESS_VM_READ;

pub(super) struct ProcessDumpRequest {
    identity: ProcIdentity,
    path: PathBuf,
}

pub(super) struct ProcessDumpReport {
    path: PathBuf,
    error: Option<u32>,
}

enum ExistingPath {
    Absent,
    File,
    Directory,
    Reparse,
}

struct PartialDumpFile {
    path: PathBuf,
    handle: Option<OwnedHandle>,
    delete_on_drop: bool,
}

impl PartialDumpFile {
    fn handle(&self) -> &OwnedHandle {
        self.handle.as_ref().expect("partial dump handle is open")
    }

    fn close(&mut self) {
        self.handle.take();
    }
}

impl Drop for PartialDumpFile {
    fn drop(&mut self) {
        self.handle.take();
        if self.delete_on_drop {
            delete_path(&self.path);
        }
    }
}

impl ProcessPageState {
    pub(super) fn start_dump_worker(&mut self) -> Result<(), u32> {
        if self.dump_worker.is_some() {
            return Ok(());
        }
        self.dump_worker = Some(SingleFlightWorker::spawn(
            "taskmgr-rs-process-dump",
            PWM_PROC_DUMP_COMPLETE,
            keep_pending,
            |request: ProcessDumpRequest| perform_dump(request),
        )?);
        Ok(())
    }

    pub(super) fn stop_dump_worker(&mut self) {
        self.dump_worker = None;
    }

    pub(super) fn create_dump_file(&mut self, identity: ProcIdentity) {
        if !identity.is_verified() {
            self.show_failure_message(&self.strings.cant_create_dump, ERROR_INVALID_PARAMETER);
            return;
        }
        if self
            .dump_worker
            .as_ref()
            .is_some_and(SingleFlightWorker::is_in_flight)
        {
            self.show_failure_message(&self.strings.cant_create_dump, ERROR_BUSY);
            return;
        }

        let image_name = self
            .entries
            .iter()
            .find(|entry| entry.identity == identity)
            .map(|entry| entry.image_name.as_str())
            .unwrap_or("process");
        let default_name = suggested_dump_file_name(image_name);
        let path = match choose_dump_path(self.main_hwnd, &default_name) {
            Ok(Some(path)) => path,
            Ok(None) => return,
            Err(error) => {
                record_win32_error("process dump save dialog", error);
                self.show_failure_message(&self.strings.cant_create_dump, error);
                return;
            }
        };
        self.submit_dump(identity, path);
    }

    pub fn handle_dump_worker_completion(&mut self) {
        let drained = match self.dump_worker.as_mut() {
            Some(worker) => worker.drain(self.hwnd_page),
            None => return,
        };
        for completion in drained.completions {
            match completion.value.error {
                Some(error) => {
                    self.show_failure_message(&self.strings.cant_create_dump, error);
                }
                None => self.show_dump_created(&completion.value.path),
            }
        }
        if let Some(error) = drained.error {
            record_win32_error("process dump worker", error);
            self.show_failure_message(&self.strings.cant_create_dump, error);
        }
    }

    fn submit_dump(&mut self, identity: ProcIdentity, path: PathBuf) {
        let Some(worker) = self.dump_worker.as_mut() else {
            self.show_failure_message(&self.strings.cant_create_dump, ERROR_GEN_FAILURE);
            return;
        };
        if worker.is_in_flight() {
            self.show_failure_message(&self.strings.cant_create_dump, ERROR_BUSY);
            return;
        }
        match worker.request(ProcessDumpRequest { identity, path }, self.hwnd_page) {
            Ok(disposition) if disposition.was_coalesced() => {
                self.show_failure_message(&self.strings.cant_create_dump, ERROR_BUSY);
            }
            Ok(_) => {}
            Err(error) => {
                record_win32_error("process dump submit", error);
                self.show_failure_message(&self.strings.cant_create_dump, error);
            }
        }
    }

    fn show_dump_created(&self, path: &Path) {
        let title = to_wide_null(text(TextKey::AppTitle));
        let message = format!("{}\r\n\r\n{}", self.strings.dump_created, path.display());
        let message_wide = to_wide_null(&message);
        // SAFETY: both UTF-16 buffers are terminated and remain live for the synchronous call.
        // The main window HWND is borrowed and no ownership is transferred.
        unsafe {
            MessageBoxW(
                self.main_hwnd,
                message_wide.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONINFORMATION,
            );
        }
    }
}

fn perform_dump(request: ProcessDumpRequest) -> ProcessDumpReport {
    let error = match write_process_dump(request.identity, &request.path) {
        Ok(()) => None,
        Err(error) => {
            record_win32_error("process dump", error);
            Some(error)
        }
    };
    ProcessDumpReport {
        path: request.path,
        error,
    }
}

pub(super) fn suggested_dump_file_name(image_name: &str) -> String {
    let base = image_name
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(image_name)
        .trim();
    let mut cleaned = String::new();
    for ch in base.chars() {
        if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            cleaned.push('_');
        } else {
            cleaned.push(ch);
        }
    }
    let cleaned = cleaned.trim_matches(|ch: char| ch == ' ' || ch == '.');
    let cleaned = if cleaned.is_empty() {
        "process"
    } else {
        cleaned
    };
    if has_dmp_extension(cleaned) {
        cleaned.to_string()
    } else {
        format!("{cleaned}.dmp")
    }
}

fn has_dmp_extension(name: &str) -> bool {
    name.get(name.len().saturating_sub(4)..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".dmp"))
}

pub(super) fn write_process_dump(identity: ProcIdentity, path: &Path) -> Result<(), u32> {
    if !identity.is_verified() {
        return Err(ERROR_INVALID_PARAMETER);
    }
    validate_dump_path(path)?;
    match existing_path_kind(path)? {
        ExistingPath::Reparse => return Err(ERROR_ACCESS_DENIED),
        ExistingPath::Directory => return Err(ERROR_INVALID_PARAMETER),
        ExistingPath::Absent | ExistingPath::File => {}
    }

    let process = open_process_for_identity(identity, PROCESS_DUMP_ACCESS)?;
    let partial_path = partial_dump_path(path, identity);
    match existing_path_kind(&partial_path)? {
        ExistingPath::Reparse | ExistingPath::Directory => return Err(ERROR_ACCESS_DENIED),
        ExistingPath::Absent | ExistingPath::File => {}
    }

    let mut partial = create_partial_dump_file(&partial_path)?;
    if handle_is_reparse(partial.handle())? {
        partial.delete_on_drop = false;
        return Err(ERROR_ACCESS_DENIED);
    }

    // SAFETY: `process` was opened for this verified identity with the rights
    // MiniDumpWriteDump documents. `identity.pid` is that same process, not a
    // later PID reuse. The file handle is a normal file this call owns.
    // Exception, user-stream, and callback pointers are intentionally null.
    let wrote = unsafe {
        MiniDumpWriteDump(
            process.as_raw(),
            identity.pid,
            partial.handle().as_raw(),
            MiniDumpWithFullMemory,
            null(),
            null(),
            null(),
        )
    };
    if wrote == 0 {
        return Err(last_error_or_gen_failure());
    }
    // SAFETY: the partial file handle is still open and uniquely owned.
    if unsafe { FlushFileBuffers(partial.handle().as_raw()) } == 0 {
        return Err(last_error_or_gen_failure());
    }
    partial.close();
    drop(process);

    match existing_path_kind(path)? {
        ExistingPath::Reparse | ExistingPath::Directory => return Err(ERROR_ACCESS_DENIED),
        ExistingPath::Absent | ExistingPath::File => {}
    }
    move_partial_into_place(&partial_path, path)?;
    partial.delete_on_drop = false;
    Ok(())
}

pub(super) fn validate_dump_path(path: &Path) -> Result<(), u32> {
    let raw = path.as_os_str().to_string_lossy();
    if raw.is_empty() || raw.ends_with(['\\', '/']) {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let Some(name) = path.file_name() else {
        return Err(ERROR_INVALID_PARAMETER);
    };
    if name.is_empty() || name == OsStr::new(".") || name == OsStr::new("..") {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let name = name.to_string_lossy();
    if name.contains('\0') || name.ends_with([' ', '.']) {
        return Err(ERROR_INVALID_PARAMETER);
    }
    Ok(())
}

pub(super) fn partial_dump_path(path: &Path, identity: ProcIdentity) -> PathBuf {
    let file_name = path.file_name().unwrap_or(OsStr::new("process.dmp"));
    let mut partial_name = OsString::from(file_name);
    partial_name.push(format!(
        ".{:x}.{:x}.partial",
        identity.pid, identity.creation_time_100ns
    ));
    path.with_file_name(partial_name)
}

fn choose_dump_path(
    owner: windows_sys::Win32::Foundation::HWND,
    default_name: &str,
) -> Result<Option<PathBuf>, u32> {
    let mut file_buffer = vec![0u16; 32_768];
    let default_wide = to_wide_null(default_name);
    let copy_length = default_wide.len().min(file_buffer.len());
    file_buffer[..copy_length].copy_from_slice(&default_wide[..copy_length]);

    let filter_label = format!("{} (*.dmp)", text(TextKey::DumpFileFilter));
    let mut filter = filter_label.encode_utf16().collect::<Vec<_>>();
    filter.push(0);
    filter.extend("*.dmp".encode_utf16());
    filter.extend([0, 0]);
    let title = to_wide_null(&strip_menu_accelerator(text(TextKey::CreateDumpFile)));
    let extension = to_wide_null("dmp");
    let mut dialog = unsafe { zeroed::<OPENFILENAMEW>() };
    dialog.lStructSize = size_of_open_file_name();
    dialog.hwndOwner = owner;
    dialog.lpstrFilter = filter.as_ptr();
    dialog.nFilterIndex = 1;
    dialog.lpstrFile = file_buffer.as_mut_ptr();
    dialog.nMaxFile = file_buffer.len() as u32;
    dialog.lpstrTitle = title.as_ptr();
    dialog.Flags = OFN_EXPLORER | OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY;
    dialog.lpstrDefExt = extension.as_ptr();

    // SAFETY: OPENFILENAMEW references buffers that remain alive for the modal call.
    if unsafe { GetSaveFileNameW(&mut dialog) } == 0 {
        // SAFETY: this query immediately follows the common-dialog failure or cancel.
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
    let path = PathBuf::from(OsString::from_wide(&file_buffer[..length]));
    validate_dump_path(&path)?;
    Ok(Some(path))
}

fn size_of_open_file_name() -> u32 {
    size_of::<OPENFILENAMEW>() as u32
}

fn strip_menu_accelerator(label: &str) -> String {
    let without_ampersand = label.replace('&', "");
    let trimmed = without_ampersand.trim();
    let Some(open) = trimmed.rfind('(') else {
        return trimmed.to_string();
    };
    let suffix = &trimmed[open..];
    let accelerator = suffix.chars().filter(|ch| ch.is_ascii_alphabetic()).count() == 1
        && suffix.ends_with(')')
        && suffix
            .chars()
            .all(|ch| ch.is_ascii_alphabetic() || matches!(ch, '(' | ')' | ' '));
    if accelerator {
        trimmed[..open].trim().to_string()
    } else {
        trimmed.to_string()
    }
}

fn existing_path_kind(path: &Path) -> Result<ExistingPath, u32> {
    let wide = wide_path(path);
    // SAFETY: `wide` is NUL-terminated and remains live for this synchronous query.
    let attributes = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if attributes == INVALID_FILE_ATTRIBUTES {
        let error = last_error_or_gen_failure();
        return if error == ERROR_FILE_NOT_FOUND || error == ERROR_PATH_NOT_FOUND {
            Ok(ExistingPath::Absent)
        } else {
            Err(error)
        };
    }
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Ok(ExistingPath::Reparse);
    }
    if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Ok(ExistingPath::Directory);
    }
    Ok(ExistingPath::File)
}

fn create_partial_dump_file(path: &Path) -> Result<PartialDumpFile, u32> {
    let wide = wide_path(path);
    // SAFETY: `wide` is NUL-terminated and remains live for this synchronous call.
    // A successful return is one uniquely owned kernel file handle.
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ,
            null(),
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    let handle = unsafe { OwnedHandle::from_raw(raw) }.ok_or_else(last_error_or_gen_failure)?;
    Ok(PartialDumpFile {
        path: path.to_path_buf(),
        handle: Some(handle),
        delete_on_drop: true,
    })
}

fn handle_is_reparse(handle: &OwnedHandle) -> Result<bool, u32> {
    let mut info = unsafe { zeroed::<BY_HANDLE_FILE_INFORMATION>() };
    // SAFETY: `handle` is a live file handle and `info` is writable for the call.
    if unsafe { GetFileInformationByHandle(handle.as_raw(), &mut info) } == 0 {
        return Err(last_error_or_gen_failure());
    }
    Ok(info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

fn move_partial_into_place(partial: &Path, destination: &Path) -> Result<(), u32> {
    let partial_wide = wide_path(partial);
    let destination_wide = wide_path(destination);
    // SAFETY: both paths are NUL-terminated and remain live for this synchronous rename.
    if unsafe {
        MoveFileExW(
            partial_wide.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(last_error_or_gen_failure());
    }
    Ok(())
}

fn delete_path(path: &Path) {
    let wide = wide_path(path);
    // SAFETY: `wide` is NUL-terminated and remains live for this synchronous delete.
    unsafe {
        DeleteFileW(wide.as_ptr());
    }
}

fn wide_path(path: &Path) -> Vec<u16> {
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    wide
}

fn last_error_or_gen_failure() -> u32 {
    // SAFETY: GetLastError has no preconditions.
    let error = unsafe { GetLastError() };
    if error == 0 { ERROR_GEN_FAILURE } else { error }
}

#[cfg(test)]
mod tests {
    use super::{
        PROCESS_DUMP_ACCESS, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ, partial_dump_path,
        strip_menu_accelerator, suggested_dump_file_name, validate_dump_path, write_process_dump,
    };
    use crate::system::process_identity::ProcIdentity;
    use std::path::Path;
    use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
    use windows_sys::Win32::System::Threading::{
        PROCESS_SET_INFORMATION, PROCESS_TERMINATE, PROCESS_VM_WRITE,
    };

    #[test]
    fn dump_access_is_query_and_read_only() {
        assert_eq!(
            PROCESS_DUMP_ACCESS,
            PROCESS_QUERY_INFORMATION | PROCESS_VM_READ
        );
        assert_eq!(PROCESS_DUMP_ACCESS & PROCESS_VM_WRITE, 0);
        assert_eq!(PROCESS_DUMP_ACCESS & PROCESS_TERMINATE, 0);
        assert_eq!(PROCESS_DUMP_ACCESS & PROCESS_SET_INFORMATION, 0);
    }

    #[test]
    fn unverified_identity_never_reaches_open_process() {
        let error = write_process_dump(ProcIdentity::pid_only(44), Path::new("editor.dmp"))
            .expect_err("a pid-only identity must be rejected");
        assert_eq!(error, ERROR_INVALID_PARAMETER);
    }

    #[test]
    fn suggested_name_strips_directories_and_reserved_characters() {
        assert_eq!(
            suggested_dump_file_name(r"C:\Apps\Editor.exe"),
            "Editor.exe.dmp"
        );
        assert_eq!(
            suggested_dump_file_name("bad:name?.exe"),
            "bad_name_.exe.dmp"
        );
        assert_eq!(suggested_dump_file_name("already.DMP"), "already.DMP");
        assert_eq!(suggested_dump_file_name("   "), "process.dmp");
        assert_eq!(suggested_dump_file_name(""), "process.dmp");
    }

    #[test]
    fn dump_path_rejects_directories_and_trailing_dots() {
        assert!(validate_dump_path(Path::new(r"C:\Dumps\editor.dmp")).is_ok());
        assert_eq!(
            validate_dump_path(Path::new(r"C:\Dumps\")).unwrap_err(),
            ERROR_INVALID_PARAMETER
        );
        assert_eq!(
            validate_dump_path(Path::new("editor.dmp.")).unwrap_err(),
            ERROR_INVALID_PARAMETER
        );
    }

    #[test]
    fn partial_file_stays_beside_the_chosen_path() {
        let identity = ProcIdentity::new(7, 99);
        let chosen = Path::new(r"C:\Dumps\editor.dmp");
        let partial = partial_dump_path(chosen, identity);
        assert_eq!(partial.parent(), chosen.parent());
        assert_ne!(partial.file_name(), chosen.file_name());
        let name = partial.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("editor.dmp."));
        assert!(name.ends_with(".partial"));
    }

    #[test]
    fn dialog_title_drops_the_menu_accelerator() {
        assert_eq!(
            strip_menu_accelerator("Create Dump &File"),
            "Create Dump File"
        );
        assert_eq!(strip_menu_accelerator("创建转储文件(&F)"), "创建转储文件");
        assert_eq!(
            strip_menu_accelerator("Abbilddatei erstellen (&F)"),
            "Abbilddatei erstellen"
        );
    }
}

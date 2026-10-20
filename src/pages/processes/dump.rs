// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 进程转储
//
//   文件:       src/pages/processes/dump.rs
//
//   日期:       2026年10月20日
//   环境:       Ubuntu 24.04.5 LTS x86_64；Linux 内核 6.12.94+；Rust 1.99.1
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Create Dump File for one verified process.
//!
//! The save dialog stays on the UI thread. `MiniDumpWriteDump` runs on a
//! detached worker and only after `open_process_for_identity` confirms the
//! pid and creation time. Dropping the worker does not join that thread, and
//! a destroyed page does not receive its result. A pid-only identity never
//! reaches `OpenProcess`.

use std::ffi::{OsStr, OsString};
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_BUSY, ERROR_FILE_EXISTS, ERROR_FILE_NOT_FOUND,
    ERROR_GEN_FAILURE, ERROR_INVALID_PARAMETER, ERROR_NOT_ENOUGH_MEMORY, ERROR_PATH_NOT_FOUND,
    GENERIC_READ, GENERIC_WRITE, GetLastError, HWND,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateFileW, DeleteFileW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO,
    FILE_FLAG_DELETE_ON_CLOSE, FILE_FLAG_OPEN_REPARSE_POINT, FileDispositionInfo, FlushFileBuffers,
    GetFileAttributesW, GetFileInformationByHandle, INVALID_FILE_ATTRIBUTES,
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW, SetFileInformationByHandle,
};
use windows_sys::Win32::System::Diagnostics::Debug::{MiniDumpWithFullMemory, MiniDumpWriteDump};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::{PROCESS_QUERY_INFORMATION, PROCESS_VM_READ};
use windows_sys::Win32::UI::Controls::Dialogs::{
    CommDlgExtendedError, GetSaveFileNameW, OFN_EXPLORER, OFN_HIDEREADONLY, OFN_OVERWRITEPROMPT,
    OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MB_ICONINFORMATION, MB_OK, MessageBoxW, PostMessageW,
};

use super::ProcessPageState;
use crate::infrastructure::diagnostics::{release_dbghelp_dump, try_acquire_dbghelp_dump};
use crate::infrastructure::native::{OwnedHandle, record_win32_error, to_wide_null};
use crate::system::process_identity::{ProcIdentity, open_process_for_identity};
use crate::ui::localization::{TextKey, text};
use crate::ui::resource_ids::PWM_PROC_DUMP_COMPLETE;

/// Process rights documented for `MiniDumpWriteDump`.
///
/// `PROCESS_QUERY_INFORMATION` also covers `GetProcessTimes`, which
/// `open_process_for_identity` uses to reject a reused PID.
pub(super) const PROCESS_DUMP_ACCESS: u32 = PROCESS_QUERY_INFORMATION | PROCESS_VM_READ;

const MAX_PARTIAL_ATTEMPTS: u32 = 32;

static PARTIAL_UNIQUE: AtomicU64 = AtomicU64::new(1);

enum WorkerCommand {
    Dump {
        request: ProcessDumpRequest,
        notify_hwnd: isize,
    },
    #[cfg(test)]
    Block {
        entered: mpsc::Sender<()>,
        release: Receiver<()>,
    },
}

struct ProcessDumpRequest {
    identity: ProcIdentity,
    path: PathBuf,
}

struct ProcessDumpReport {
    path: PathBuf,
    failure: Option<DumpFailure>,
}

struct DumpFailure {
    error: u32,
    cleanup_error: Option<u32>,
    leftover_path: Option<PathBuf>,
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
    delete_on_close: bool,
}

impl Drop for PartialDumpFile {
    fn drop(&mut self) {
        self.handle.take();
        if !self.delete_on_close {
            return;
        }
        if let Err(error) = delete_file_if_present(&self.path) {
            record_win32_error("process dump temporary file cleanup", error);
        }
    }
}

/// Dump worker whose destructor detaches instead of joining.
///
/// A full-memory dump can run for minutes. Closing Task Manager must not wait
/// for that thread on the UI thread, and the worker must not post a result
/// after the page stops accepting one.
pub(super) struct DumpWorker {
    command_tx: Option<SyncSender<WorkerCommand>>,
    completion_rx: Option<Receiver<ProcessDumpReport>>,
    thread: Option<JoinHandle<()>>,
    accepting: Arc<AtomicBool>,
    in_flight: bool,
}

impl DumpWorker {
    fn spawn() -> Result<Self, u32> {
        let (command_tx, command_rx) = mpsc::sync_channel(1);
        let (completion_tx, completion_rx) = mpsc::sync_channel(1);
        let accepting = Arc::new(AtomicBool::new(true));
        let accepting_worker = Arc::clone(&accepting);
        let thread = thread::Builder::new()
            .name("taskmgr-rs-process-dump".to_string())
            .spawn(move || dump_worker_loop(command_rx, completion_tx, accepting_worker))
            .map_err(thread_spawn_error)?;
        Ok(Self {
            command_tx: Some(command_tx),
            completion_rx: Some(completion_rx),
            thread: Some(thread),
            accepting,
            in_flight: false,
        })
    }

    fn is_in_flight(&self) -> bool {
        self.in_flight
    }

    fn request(&mut self, request: ProcessDumpRequest, notify_hwnd: isize) -> Result<(), u32> {
        if self.in_flight || !self.accepting.load(Ordering::Acquire) {
            return Err(ERROR_BUSY);
        }
        let sender = self.command_tx.as_ref().ok_or(ERROR_GEN_FAILURE)?;
        match sender.try_send(WorkerCommand::Dump {
            request,
            notify_hwnd,
        }) {
            Ok(()) => {
                self.in_flight = true;
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(ERROR_BUSY),
            Err(TrySendError::Disconnected(_)) => Err(ERROR_GEN_FAILURE),
        }
    }

    fn drain(&mut self) -> Vec<ProcessDumpReport> {
        let mut reports = Vec::new();
        let Some(receiver) = self.completion_rx.as_ref() else {
            return reports;
        };
        while let Ok(report) = receiver.try_recv() {
            self.in_flight = false;
            reports.push(report);
        }
        reports
    }

    #[cfg(test)]
    fn request_block(
        &mut self,
        entered: mpsc::Sender<()>,
        release: Receiver<()>,
    ) -> Result<(), u32> {
        let sender = self.command_tx.as_ref().ok_or(ERROR_GEN_FAILURE)?;
        sender
            .try_send(WorkerCommand::Block { entered, release })
            .map_err(|_| ERROR_BUSY)
    }
}

impl Drop for DumpWorker {
    fn drop(&mut self) {
        self.accepting.store(false, Ordering::Release);
        // Dropping the completion receiver unblocks a worker blocked on send.
        // Dropping the command sender lets the loop exit after the current dump.
        // The join handle is detached: waiting here would freeze window destruction.
        self.completion_rx.take();
        self.command_tx.take();
        self.thread.take();
    }
}

fn dump_worker_loop(
    command_rx: Receiver<WorkerCommand>,
    completion_tx: SyncSender<ProcessDumpReport>,
    accepting: Arc<AtomicBool>,
) {
    while let Ok(command) = command_rx.recv() {
        match command {
            WorkerCommand::Dump {
                request,
                notify_hwnd,
            } => {
                let report = perform_dump(request);
                if !accepting.load(Ordering::Acquire) {
                    break;
                }
                if completion_tx.send(report).is_err() {
                    break;
                }
                if accepting.load(Ordering::Acquire) && notify_hwnd != 0 {
                    // SAFETY: `notify_hwnd` is the process page. The page sets `accepting`
                    // false before it is destroyed, and the posted handler ignores a worker
                    // that has already been dropped.
                    unsafe {
                        PostMessageW(notify_hwnd as HWND, PWM_PROC_DUMP_COMPLETE, 0, 0);
                    }
                }
            }
            #[cfg(test)]
            WorkerCommand::Block { entered, release } => {
                let _ = entered.send(());
                let _ = release.recv();
            }
        }
    }
}

fn thread_spawn_error(error: std::io::Error) -> u32 {
    error
        .raw_os_error()
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(ERROR_NOT_ENOUGH_MEMORY)
}

impl ProcessPageState {
    pub(super) fn start_dump_worker(&mut self) -> Result<(), u32> {
        if self.dump_worker.is_some() {
            return Ok(());
        }
        self.dump_worker = Some(DumpWorker::spawn()?);
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
            .is_some_and(DumpWorker::is_in_flight)
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
        let Some(worker) = self.dump_worker.as_mut() else {
            return;
        };
        if !worker.accepting.load(Ordering::Acquire) {
            return;
        }
        for report in worker.drain() {
            match report.failure {
                Some(failure) => self.show_dump_failure(&report.path, &failure),
                None => self.show_dump_created(&report.path),
            }
        }
    }

    fn submit_dump(&mut self, identity: ProcIdentity, path: PathBuf) {
        let Some(worker) = self.dump_worker.as_mut() else {
            self.show_failure_message(&self.strings.cant_create_dump, ERROR_GEN_FAILURE);
            return;
        };
        if let Err(error) = worker.request(
            ProcessDumpRequest { identity, path },
            self.hwnd_page as isize,
        ) {
            record_win32_error("process dump submit", error);
            self.show_failure_message(&self.strings.cant_create_dump, error);
        }
    }

    fn show_dump_failure(&self, destination: &Path, failure: &DumpFailure) {
        let mut body = self.strings.cant_create_dump.clone();
        if let Some(cleanup_error) = failure.cleanup_error {
            let leftover = failure
                .leftover_path
                .as_deref()
                .unwrap_or(destination)
                .display();
            body.push_str(&format!(
                "\r\n\r\n{}\r\n{leftover}\r\nWin32 error: {cleanup_error}",
                self.strings.dump_cleanup_failed
            ));
        }
        self.show_failure_message(&body, failure.error);
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
    let failure = match write_process_dump(request.identity, &request.path) {
        Ok(()) => None,
        Err(failure) => {
            record_win32_error("process dump", failure.error);
            if let Some(cleanup_error) = failure.cleanup_error {
                record_win32_error("process dump temporary file cleanup", cleanup_error);
            }
            Some(failure)
        }
    };
    ProcessDumpReport {
        path: request.path,
        failure,
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

fn write_process_dump(identity: ProcIdentity, path: &Path) -> Result<(), DumpFailure> {
    if !identity.is_verified() {
        return Err(DumpFailure::primary(ERROR_INVALID_PARAMETER));
    }
    validate_dump_path(path).map_err(DumpFailure::primary)?;
    match existing_path_kind(path).map_err(DumpFailure::primary)? {
        ExistingPath::Reparse => return Err(DumpFailure::primary(ERROR_ACCESS_DENIED)),
        ExistingPath::Directory => return Err(DumpFailure::primary(ERROR_INVALID_PARAMETER)),
        ExistingPath::Absent | ExistingPath::File => {}
    }

    let process =
        open_process_for_identity(identity, PROCESS_DUMP_ACCESS).map_err(DumpFailure::primary)?;
    let mut partial = create_unique_partial(path, identity).map_err(DumpFailure::primary)?;
    let reparse = partial
        .handle
        .as_ref()
        .ok_or(ERROR_GEN_FAILURE)
        .and_then(handle_is_reparse);
    let reparse = match reparse {
        Ok(reparse) => reparse,
        Err(error) => return Err(fail_partial(&mut partial, error)),
    };
    if reparse {
        return Err(fail_partial(&mut partial, ERROR_ACCESS_DENIED));
    }
    let Some(file) = partial.handle.as_ref().map(OwnedHandle::as_raw) else {
        return Err(fail_partial(&mut partial, ERROR_GEN_FAILURE));
    };

    if !try_acquire_dbghelp_dump() {
        return Err(fail_partial(&mut partial, ERROR_BUSY));
    }
    // SAFETY: `process` was opened for this verified identity with the rights
    // MiniDumpWriteDump documents. `identity.pid` is that same process, not a
    // later PID reuse. `file` is a copy of the normal file this call created.
    // The process-wide gate is held, so the crash filter will not enter DbgHelp.
    // Exception, user-stream, and callback pointers are intentionally null.
    let wrote = unsafe {
        MiniDumpWriteDump(
            process.as_raw(),
            identity.pid,
            file,
            MiniDumpWithFullMemory,
            null(),
            null(),
            null(),
        )
    };
    let write_error = if wrote == 0 {
        Some(last_error_or_gen_failure())
    } else {
        None
    };
    release_dbghelp_dump();
    if let Some(error) = write_error {
        return Err(fail_partial(&mut partial, error));
    }

    // SAFETY: the partial file handle is still open and uniquely owned.
    if unsafe { FlushFileBuffers(file) } == 0 {
        return Err(fail_partial(&mut partial, last_error_or_gen_failure()));
    }
    drop(process);
    commit_partial(&mut partial, path)
}

impl DumpFailure {
    fn primary(error: u32) -> Self {
        Self {
            error,
            cleanup_error: None,
            leftover_path: None,
        }
    }
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

pub(super) fn partial_dump_candidate(
    destination: &Path,
    identity: ProcIdentity,
    unique: u64,
) -> PathBuf {
    let file_name = destination.file_name().unwrap_or(OsStr::new("process.dmp"));
    let mut partial_name = OsString::from(file_name);
    partial_name.push(format!(
        ".{:x}.{:x}.{unique:x}.partial",
        identity.pid, identity.creation_time_100ns
    ));
    destination.with_file_name(partial_name)
}

fn next_partial_unique() -> u64 {
    let counter = PARTIAL_UNIQUE.fetch_add(1, Ordering::Relaxed);
    // SAFETY: GetTickCount64 has no preconditions and does not allocate.
    let tick = unsafe { GetTickCount64() };
    tick ^ counter.rotate_left(17)
}

fn create_unique_partial(
    destination: &Path,
    identity: ProcIdentity,
) -> Result<PartialDumpFile, u32> {
    let mut last_error = ERROR_FILE_EXISTS;
    for _ in 0..MAX_PARTIAL_ATTEMPTS {
        let path = partial_dump_candidate(destination, identity, next_partial_unique());
        match create_partial_dump_file(&path) {
            Ok(file) => return Ok(file),
            Err(error) if is_create_collision(error) => last_error = error,
            Err(error) => return Err(error),
        }
    }
    Err(last_error)
}

fn is_create_collision(error: u32) -> bool {
    error == ERROR_FILE_EXISTS || error == ERROR_ALREADY_EXISTS
}

fn create_partial_dump_file(path: &Path) -> Result<PartialDumpFile, u32> {
    let wide = wide_path(path);
    // SAFETY: `wide` is NUL-terminated and remains live for this synchronous call.
    // CREATE_NEW refuses an existing name instead of truncating it. DELETE_ON_CLOSE
    // removes the new file when this handle closes unless the disposition is cleared.
    // Share mode is zero so another reader cannot keep the memory dump alive.
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            null(),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_DELETE_ON_CLOSE,
            null_mut(),
        )
    };
    let handle = unsafe { OwnedHandle::from_raw(raw) }.ok_or_else(last_error_or_gen_failure)?;
    Ok(PartialDumpFile {
        path: path.to_path_buf(),
        handle: Some(handle),
        delete_on_close: true,
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

fn commit_partial(partial: &mut PartialDumpFile, destination: &Path) -> Result<(), DumpFailure> {
    let cleared = partial
        .handle
        .as_ref()
        .ok_or(ERROR_GEN_FAILURE)
        .and_then(clear_delete_on_close);
    if let Err(error) = cleared {
        return Err(fail_partial(partial, error));
    }
    partial.delete_on_close = false;
    partial.handle.take();
    if let Err(error) = move_partial_into_place(&partial.path, destination) {
        return Err(cleanup_after(partial, error));
    }
    Ok(())
}

fn clear_delete_on_close(handle: &OwnedHandle) -> Result<(), u32> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: false };
    // SAFETY: `handle` is the file this call created. `info` stays live for the call
    // and matches `FileDispositionInfo`.
    if unsafe {
        SetFileInformationByHandle(
            handle.as_raw(),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        return Err(last_error_or_gen_failure());
    }
    Ok(())
}

fn fail_partial(partial: &mut PartialDumpFile, error: u32) -> DumpFailure {
    partial.handle.take();
    cleanup_after(partial, error)
}

fn cleanup_after(partial: &mut PartialDumpFile, error: u32) -> DumpFailure {
    let cleanup = delete_file_if_present(&partial.path);
    partial.delete_on_close = false;
    match cleanup {
        Ok(()) => DumpFailure::primary(error),
        Err(cleanup_error) => DumpFailure {
            error,
            cleanup_error: Some(cleanup_error),
            leftover_path: Some(partial.path.clone()),
        },
    }
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

fn delete_file_if_present(path: &Path) -> Result<(), u32> {
    match existing_path_kind(path) {
        Ok(ExistingPath::Absent) => Ok(()),
        Err(error) if error == ERROR_FILE_NOT_FOUND || error == ERROR_PATH_NOT_FOUND => Ok(()),
        Ok(_) | Err(_) => delete_file(path),
    }
}

fn delete_file(path: &Path) -> Result<(), u32> {
    let wide = wide_path(path);
    // SAFETY: `wide` is NUL-terminated and remains live for this synchronous delete.
    if unsafe { DeleteFileW(wide.as_ptr()) } == 0 {
        let error = last_error_or_gen_failure();
        if error == ERROR_FILE_NOT_FOUND || error == ERROR_PATH_NOT_FOUND {
            Ok(())
        } else {
            Err(error)
        }
    } else {
        Ok(())
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

fn choose_dump_path(owner: HWND, default_name: &str) -> Result<Option<PathBuf>, u32> {
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
        DumpWorker, PROCESS_DUMP_ACCESS, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
        is_create_collision, partial_dump_candidate, strip_menu_accelerator,
        suggested_dump_file_name, validate_dump_path, write_process_dump,
    };
    use crate::system::process_identity::ProcIdentity;
    use std::path::Path;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, ERROR_INVALID_PARAMETER,
    };
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
        assert_eq!(error.error, ERROR_INVALID_PARAMETER);
        assert_eq!(error.cleanup_error, None);
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
    fn partial_candidates_are_unique_siblings() {
        let identity = ProcIdentity::new(7, 99);
        let chosen = Path::new(r"C:\Dumps\editor.dmp");
        let first = partial_dump_candidate(chosen, identity, 1);
        let second = partial_dump_candidate(chosen, identity, 2);
        assert_ne!(first, second);
        assert_eq!(first.parent(), chosen.parent());
        assert_ne!(first.file_name(), chosen.file_name());
        let name = first.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("editor.dmp."));
        assert!(name.ends_with(".1.partial"));
        assert!(is_create_collision(ERROR_FILE_EXISTS));
        assert!(is_create_collision(ERROR_ALREADY_EXISTS));
        assert!(!is_create_collision(ERROR_ACCESS_DENIED));
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

    #[test]
    fn dropping_a_blocked_dump_worker_does_not_join() {
        let mut worker = DumpWorker::spawn().expect("dump worker should start");
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        worker
            .request_block(entered_tx, release_rx)
            .expect("block request should queue");
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker should enter the block");

        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            drop(worker);
            let _ = done_tx.send(());
        });
        let finished = done_rx.recv_timeout(Duration::from_secs(1));
        let _ = release_tx.send(());
        assert!(
            finished.is_ok(),
            "shutdown must not wait for the dump thread"
        );
    }
}

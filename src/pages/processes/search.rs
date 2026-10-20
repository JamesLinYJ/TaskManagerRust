// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 进程映像联机搜索
//
//   文件:       src/pages/processes/search.rs
//
//   日期:       2026年10月20日
//   环境:       Linux 6.12.94+；Rust 1.99.1
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Searches the web for one verified process image name.
//!
//! The UI thread only decides whether the selection is allowed and submits the
//! request. Opening the process, building the query, and `ShellExecuteExW` run
//! on a persistent worker. An unverified identity is rejected before any
//! process open, so there is no PID-only fallback.

use std::mem::size_of;
use std::ptr::{null, null_mut};

use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_GEN_FAILURE, ERROR_INVALID_DATA, ERROR_INVALID_PARAMETER,
    GetLastError, HANDLE, SetLastError,
};
use windows_sys::Win32::System::Threading::{
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::Shell::{SEE_MASK_FLAG_NO_UI, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::infrastructure::native::to_wide_null;
use crate::infrastructure::worker::{SingleFlightWorker, replace_pending};
use crate::system::process_identity::{ProcIdentity, open_process_for_identity};
use crate::ui::resource_ids::PWM_PROC_SEARCH_COMPLETE;

const RPC_E_CHANGED_MODE: i32 = 0x8001_0106_u32 as i32;
const SEARCH_URL_PREFIX: &str = "https://www.bing.com/search?q=";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ProcessSearchRequest {
    pub(super) identity: ProcIdentity,
    pub(super) image_name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProcessSearchFailure {
    Win32(u32),
    Shell(i32),
}

pub(super) fn search_online_menu_enabled(identity: ProcIdentity, image_name: &str) -> bool {
    validate_search_target(identity, image_name).is_ok()
}

pub(super) fn validate_search_target(identity: ProcIdentity, image_name: &str) -> Result<(), u32> {
    if identity.pid != 0 && identity.is_verified() && is_real_process_image_name(image_name) {
        Ok(())
    } else {
        Err(ERROR_INVALID_PARAMETER)
    }
}

pub(super) fn is_real_process_image_name(name: &str) -> bool {
    let name = name.trim();
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    if name.contains(['\\', '/', '\0']) || name.chars().any(char::is_control) {
        return false;
    }
    name.chars().any(|ch| ch.is_ascii_alphanumeric())
}

pub(super) fn search_url_for_image_name(image_name: &str) -> Option<String> {
    let name = image_name.trim();
    if !is_real_process_image_name(name) {
        return None;
    }
    let mut encoded = String::with_capacity(name.len());
    for byte in name.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    Some(format!("{SEARCH_URL_PREFIX}{encoded}"))
}

/// Chooses the image name after an identity-checked open.
///
/// `Ok(Some(live))` is the file name from the verified process. `Ok(None)` means
/// the open succeeded but the live name was missing. `Err(ERROR_ACCESS_DENIED)`
/// keeps the snapshot name already bound to that identity. Any other open error,
/// including a creation-time mismatch, fails the search.
pub(super) fn image_name_after_identity_open(
    snapshot_name: &str,
    opened: Result<Option<String>, u32>,
) -> Result<String, u32> {
    let snapshot_name = snapshot_name.trim();
    if !is_real_process_image_name(snapshot_name) {
        return Err(ERROR_INVALID_PARAMETER);
    }
    match opened {
        Ok(Some(live)) if is_real_process_image_name(&live) => Ok(live),
        Ok(_) => Ok(snapshot_name.to_string()),
        Err(ERROR_ACCESS_DENIED) => Ok(snapshot_name.to_string()),
        Err(error) => Err(if error == 0 { ERROR_GEN_FAILURE } else { error }),
    }
}

pub(super) fn shell_execute_ex_failure(last_error: u32) -> ProcessSearchFailure {
    if last_error == 0 {
        ProcessSearchFailure::Shell(0)
    } else {
        ProcessSearchFailure::Win32(last_error)
    }
}

pub(super) fn spawn_search_worker()
-> Result<SingleFlightWorker<ProcessSearchRequest, Result<(), ProcessSearchFailure>>, u32> {
    SingleFlightWorker::spawn_initialized(
        "taskmgr-rs-process-search",
        PWM_PROC_SEARCH_COMPLETE,
        replace_pending,
        || {
            let mut state = SearchWorkerState::new();
            move |request| state.run(request)
        },
    )
}

pub(super) fn perform_process_image_search(
    identity: ProcIdentity,
    image_name: &str,
) -> Result<(), ProcessSearchFailure> {
    validate_search_target(identity, image_name).map_err(ProcessSearchFailure::Win32)?;
    let resolved =
        resolve_search_image_name(identity, image_name).map_err(ProcessSearchFailure::Win32)?;
    let url = search_url_for_image_name(&resolved)
        .ok_or(ProcessSearchFailure::Win32(ERROR_INVALID_PARAMETER))?;
    launch_search_url(&url)
}

fn resolve_search_image_name(identity: ProcIdentity, snapshot_name: &str) -> Result<String, u32> {
    // `open_process_for_identity` refuses an unverified identity itself. This
    // function is only reached after `validate_search_target`, and it never
    // opens the PID on its own.
    let opened = match open_process_for_identity(identity, PROCESS_QUERY_LIMITED_INFORMATION) {
        Ok(handle) => Ok(query_image_file_name(handle.as_raw()).ok()),
        Err(error) => Err(error),
    };
    image_name_after_identity_open(snapshot_name, opened)
}

fn query_image_file_name(handle: HANDLE) -> Result<String, u32> {
    let mut capacity = 32768u32;
    let mut buffer = vec![0u16; capacity as usize];
    // SAFETY: `handle` is a live identity-checked process handle. `buffer` is
    // writable for `capacity` UTF-16 units, and the API writes the initialized
    // length back into `capacity`.
    let success =
        unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut capacity) };
    if success == 0 {
        let error = unsafe { GetLastError() };
        return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
    }
    let path = String::from_utf16_lossy(&buffer[..capacity as usize]);
    image_file_name(&path)
        .map(str::to_string)
        .ok_or(ERROR_INVALID_DATA)
}

fn image_file_name(path: &str) -> Option<&str> {
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path).trim();
    is_real_process_image_name(name).then_some(name)
}

fn launch_search_url(url: &str) -> Result<(), ProcessSearchFailure> {
    let verb = to_wide_null("open");
    let target = to_wide_null(url);
    let mut execute_info = SHELLEXECUTEINFOW::default();
    execute_info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    execute_info.fMask = SEE_MASK_FLAG_NO_UI;
    execute_info.hwnd = null_mut();
    execute_info.lpVerb = verb.as_ptr();
    execute_info.lpFile = target.as_ptr();
    execute_info.lpParameters = null();
    execute_info.lpDirectory = null();
    execute_info.nShow = SW_SHOWNORMAL;
    // SAFETY: the verb and URL are NUL-terminated and live for this call.
    // SEE_MASK_FLAG_NO_UI keeps the shell from opening its own error dialog on
    // the worker thread; the UI thread shows the failure dialog instead.
    let succeeded = unsafe {
        SetLastError(0);
        ShellExecuteExW(&mut execute_info)
    };
    if succeeded != 0 {
        Ok(())
    } else {
        let last_error = unsafe { GetLastError() };
        Err(shell_execute_ex_failure(last_error))
    }
}

struct SearchWorkerState {
    _com: Option<SearchComApartment>,
    com_error: Option<u32>,
}

impl SearchWorkerState {
    fn new() -> Self {
        match SearchComApartment::initialize() {
            Ok(com) => Self {
                _com: com,
                com_error: None,
            },
            Err(error) => Self {
                _com: None,
                com_error: Some(error),
            },
        }
    }

    fn run(&mut self, request: ProcessSearchRequest) -> Result<(), ProcessSearchFailure> {
        if let Some(error) = self.com_error {
            return Err(ProcessSearchFailure::Win32(error));
        }
        perform_process_image_search(request.identity, &request.image_name)
    }
}

struct SearchComApartment;

impl SearchComApartment {
    fn initialize() -> Result<Option<Self>, u32> {
        // SAFETY: this runs on the search worker thread before any shell call.
        // A successful result, including S_FALSE, is balanced by `Drop`.
        let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if result.is_ok() {
            Ok(Some(Self))
        } else if result.0 == RPC_E_CHANGED_MODE {
            Ok(None)
        } else {
            Err(hresult_to_win32(result.0))
        }
    }
}

impl Drop for SearchComApartment {
    fn drop(&mut self) {
        // SAFETY: `initialize` created this guard only after CoInitializeEx succeeded
        // on this same worker thread.
        unsafe { CoUninitialize() };
    }
}

fn hresult_to_win32(hr: i32) -> u32 {
    let raw = hr as u32;
    if raw & 0xFFFF_0000 == 0x8007_0000 {
        let code = raw & 0xFFFF;
        if code == 0 { ERROR_GEN_FAILURE } else { code }
    } else {
        ERROR_GEN_FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_image_names_are_file_names() {
        assert!(is_real_process_image_name("notepad.exe"));
        assert!(is_real_process_image_name(" System "));
        assert!(is_real_process_image_name("foo bar.exe"));
        assert!(!is_real_process_image_name(""));
        assert!(!is_real_process_image_name("   "));
        assert!(!is_real_process_image_name("."));
        assert!(!is_real_process_image_name(".."));
        assert!(!is_real_process_image_name(r"C:\Windows\notepad.exe"));
        assert!(!is_real_process_image_name("bin/notepad.exe"));
        assert!(!is_real_process_image_name("a\u{0001}b"));
        assert!(!is_real_process_image_name("---"));
    }

    #[test]
    fn search_url_encodes_the_image_name_only() {
        assert_eq!(
            search_url_for_image_name("notepad.exe").as_deref(),
            Some("https://www.bing.com/search?q=notepad.exe")
        );
        assert_eq!(
            search_url_for_image_name(" foo bar&.exe ").as_deref(),
            Some("https://www.bing.com/search?q=foo%20bar%26.exe")
        );
        assert_eq!(search_url_for_image_name(r"C:\Windows\notepad.exe"), None);
        assert_eq!(search_url_for_image_name(""), None);
    }

    #[test]
    fn unverified_or_idle_identity_cannot_search() {
        let verified = ProcIdentity::new(42, 100);
        assert!(search_online_menu_enabled(verified, "notepad.exe"));
        assert!(!search_online_menu_enabled(
            ProcIdentity::pid_only(42),
            "notepad.exe"
        ));
        assert!(!search_online_menu_enabled(verified, ""));
        assert!(!search_online_menu_enabled(
            verified,
            r"C:\Windows\notepad.exe"
        ));
        assert!(!search_online_menu_enabled(
            ProcIdentity::new(0, 100),
            "System Idle Process"
        ));
        assert_eq!(
            validate_search_target(ProcIdentity::pid_only(42), "notepad.exe"),
            Err(ERROR_INVALID_PARAMETER)
        );
    }

    #[test]
    fn identity_open_keeps_snapshot_name_only_for_access_denied() {
        let snapshot = "csrss.exe";
        assert_eq!(
            image_name_after_identity_open(snapshot, Ok(Some("csrss.exe".to_string()))).unwrap(),
            "csrss.exe"
        );
        assert_eq!(
            image_name_after_identity_open(snapshot, Ok(Some("live.exe".to_string()))).unwrap(),
            "live.exe"
        );
        assert_eq!(
            image_name_after_identity_open(snapshot, Ok(None)).unwrap(),
            snapshot
        );
        assert_eq!(
            image_name_after_identity_open(snapshot, Ok(Some(r"C:\nope".to_string()))).unwrap(),
            snapshot
        );
        assert_eq!(
            image_name_after_identity_open(snapshot, Err(ERROR_ACCESS_DENIED)).unwrap(),
            snapshot
        );
        assert_eq!(
            image_name_after_identity_open(snapshot, Err(ERROR_INVALID_PARAMETER)),
            Err(ERROR_INVALID_PARAMETER)
        );
        assert_eq!(
            image_name_after_identity_open("", Err(ERROR_ACCESS_DENIED)),
            Err(ERROR_INVALID_PARAMETER)
        );
    }

    #[test]
    fn image_file_name_drops_the_directory() {
        assert_eq!(
            image_file_name(r"C:\Windows\System32\notepad.exe"),
            Some("notepad.exe")
        );
        assert_eq!(image_file_name("/usr/bin/notepad.exe"), Some("notepad.exe"));
        assert_eq!(image_file_name(r"C:\Windows\"), None);
    }

    #[test]
    fn shell_failure_preserves_last_error() {
        assert_eq!(shell_execute_ex_failure(5), ProcessSearchFailure::Win32(5));
        assert_eq!(shell_execute_ex_failure(0), ProcessSearchFailure::Shell(0));
    }

    #[test]
    fn hresult_win32_facility_keeps_the_code() {
        assert_eq!(hresult_to_win32(0x8007_0005_u32 as i32), 5);
        assert_eq!(hresult_to_win32(0x8000_4005_u32 as i32), ERROR_GEN_FAILURE);
        assert_eq!(hresult_to_win32(0x8007_0000_u32 as i32), ERROR_GEN_FAILURE);
    }
}

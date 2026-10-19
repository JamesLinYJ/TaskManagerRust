// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 进程身份与句柄验证
//
//   文件:       src/system/process_identity.rs
//
//   日期:       2026年07月16日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Stable process identities and creation-time-verified process handles.
//!
//! Windows can reuse process identifiers. Any cached sample or action that outlives a single
//! enumeration therefore carries both the PID and process creation time. Opening a process through
//! this module verifies that pair before exposing the owned handle to the caller.

use std::mem::zeroed;

use windows_sys::Win32::Foundation::{
    ERROR_GEN_FAILURE, ERROR_INVALID_PARAMETER, FILETIME, GetLastError,
};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::infrastructure::native::{
    OwnedHandle, ProcessArchitecture, query_process_architecture_handle,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct ProcIdentity {
    pub(crate) pid: u32,
    pub(crate) creation_time_100ns: u64,
}

impl ProcIdentity {
    pub(crate) const fn new(pid: u32, creation_time_100ns: u64) -> Self {
        Self {
            pid,
            creation_time_100ns,
        }
    }

    pub(crate) const fn pid_only(pid: u32) -> Self {
        Self::new(pid, 0)
    }

    pub(crate) const fn is_verified(self) -> bool {
        self.creation_time_100ns != 0
    }
}

/// Payload for `WM_FINDPROC`.
///
/// `LPARAM` is 32 bits on i686. The creation time stays in this struct and the message
/// carries only its address, which is pointer-sized on both i686 and x86_64.
#[repr(C)]
pub(crate) struct FindProcPayload {
    pub(crate) pid: u32,
    pub(crate) creation_time_100ns: u64,
}

pub(crate) fn find_proc_lparam(payload: &FindProcPayload) -> isize {
    std::ptr::from_ref(payload) as isize
}

pub(crate) fn find_proc_identity(pid_param: usize, lparam: isize) -> Option<ProcIdentity> {
    let pid = u32::try_from(pid_param).ok()?;
    if lparam == 0 {
        return None;
    }
    // SAFETY: `lparam` is the address produced by `find_proc_lparam` for a payload the sender
    // keeps alive until the synchronous `SendMessageW` returns. This copies the two fields
    // before that payload goes out of scope. The pointer is aligned because it came from a
    // reference to `FindProcPayload`.
    let payload = unsafe { (lparam as *const FindProcPayload).read() };
    if payload.pid != pid || payload.pid == 0 || payload.creation_time_100ns == 0 {
        return None;
    }
    Some(ProcIdentity::new(payload.pid, payload.creation_time_100ns))
}

pub(crate) fn query_process_identity_for_pid(pid: u32) -> Result<ProcIdentity, u32> {
    let handle = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let creation_time_100ns = query_process_creation_time(handle.as_raw())?;
    Ok(ProcIdentity::new(pid, creation_time_100ns))
}

pub(crate) fn query_process_architecture(
    identity: ProcIdentity,
) -> Result<ProcessArchitecture, u32> {
    let handle = open_process_for_identity(identity, PROCESS_QUERY_LIMITED_INFORMATION)?;
    query_process_architecture_handle(handle.as_raw())
}

pub(crate) fn open_process_for_identity(
    identity: ProcIdentity,
    access: u32,
) -> Result<OwnedHandle, u32> {
    if !identity.is_verified() {
        return Err(ERROR_INVALID_PARAMETER);
    }

    let handle = open_process(identity.pid, access)?;
    let creation_time_100ns = query_process_creation_time(handle.as_raw())?;
    if creation_time_100ns != identity.creation_time_100ns {
        return Err(ERROR_INVALID_PARAMETER);
    }

    Ok(handle)
}

fn open_process(pid: u32, access: u32) -> Result<OwnedHandle, u32> {
    // SAFETY: OpenProcess takes scalar values only.
    let raw_handle = unsafe { OpenProcess(access, 0, pid) };
    // SAFETY: a successful OpenProcess call returns one owned kernel handle whose release function
    // is CloseHandle; ownership is transferred immediately and never duplicated.
    unsafe { OwnedHandle::from_raw(raw_handle) }.ok_or_else(last_error_or_gen_failure)
}

fn query_process_creation_time(handle: windows_sys::Win32::Foundation::HANDLE) -> Result<u64, u32> {
    // SAFETY: `handle` is owned by the caller for the duration of this synchronous query and each
    // FILETIME output points to valid initialized storage.
    unsafe {
        let mut creation = zeroed::<FILETIME>();
        let mut exit = zeroed::<FILETIME>();
        let mut kernel = zeroed::<FILETIME>();
        let mut user = zeroed::<FILETIME>();
        if GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) == 0 {
            return Err(last_error_or_gen_failure());
        }

        Ok(filetime_to_u64(creation))
    }
}

fn last_error_or_gen_failure() -> u32 {
    // SAFETY: GetLastError has no preconditions.
    let error = unsafe { GetLastError() };
    if error == 0 { ERROR_GEN_FAILURE } else { error }
}

const fn filetime_to_u64(filetime: FILETIME) -> u64 {
    ((filetime.dwHighDateTime as u64) << 32) | filetime.dwLowDateTime as u64
}

#[cfg(test)]
mod tests {
    use super::ProcIdentity;

    #[test]
    fn pid_only_identity_is_never_verified() {
        assert!(!ProcIdentity::pid_only(42).is_verified());
        assert!(ProcIdentity::new(42, 1).is_verified());
    }

    #[test]
    fn creation_time_distinguishes_reused_pid() {
        assert_ne!(ProcIdentity::new(42, 100), ProcIdentity::new(42, 200));
    }

    #[test]
    fn find_proc_creation_time_survives_i686_lparam_width() {
        // An i686 LPARAM keeps 32 bits. The old cast drops the high half, then
        // sign-extends bit 31 on the way back to u64.
        let corrupted = [
            (0x0000_0001_0000_0001, 0x0000_0000_0000_0001),
            (0x0000_0000_8000_0000, 0xFFFF_FFFF_8000_0000),
            (0x0000_0001_8000_0001, 0xFFFF_FFFF_8000_0001),
        ];
        for (creation_time_100ns, narrowed) in corrupted {
            assert_eq!(i686_lparam_creation_time(creation_time_100ns), narrowed);
            assert_ne!(narrowed, creation_time_100ns);
            assert_find_proc_round_trip(creation_time_100ns);
        }
        // All-ones survives the old sign-extend by accident. The payload path
        // still has to return that exact creation time.
        assert_eq!(i686_lparam_creation_time(u64::MAX), u64::MAX);
        assert_find_proc_round_trip(u64::MAX);
    }

    fn assert_find_proc_round_trip(creation_time_100ns: u64) {
        let payload = super::FindProcPayload {
            pid: 0x89AB_CDEF,
            creation_time_100ns,
        };
        let lparam = super::find_proc_lparam(&payload);
        #[cfg(target_pointer_width = "32")]
        let lparam = (lparam as u32) as isize;
        assert_eq!(
            super::find_proc_identity(payload.pid as usize, lparam),
            Some(ProcIdentity::new(payload.pid, creation_time_100ns))
        );
    }

    #[test]
    fn find_proc_rejects_unverified_or_mismatched_identity() {
        let payload = super::FindProcPayload {
            pid: 4,
            creation_time_100ns: 0x1_0000_0001,
        };
        let lparam = super::find_proc_lparam(&payload);
        assert_eq!(super::find_proc_identity(4, 0), None);
        assert_eq!(super::find_proc_identity(5, lparam), None);

        let unverified = super::FindProcPayload {
            pid: 4,
            creation_time_100ns: 0,
        };
        assert_eq!(
            super::find_proc_identity(4, super::find_proc_lparam(&unverified)),
            None
        );
        let idle = super::FindProcPayload {
            pid: 0,
            creation_time_100ns: 0x1_0000_0001,
        };
        assert_eq!(
            super::find_proc_identity(0, super::find_proc_lparam(&idle)),
            None
        );
    }

    fn i686_lparam_creation_time(creation_time_100ns: u64) -> u64 {
        let lparam = creation_time_100ns as u32 as i32;
        lparam as i64 as u64
    }
}

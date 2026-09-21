// +-------------------------------------------------------------------------
//
//   taskmgr-rs - Windows 安全与进程能力
//
//   文件:       src/infrastructure/native/safety.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Contains privilege and elevation checks with explicit Win32 failures.

use std::mem::zeroed;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{ERROR_NOT_ALL_ASSIGNED, GetLastError, SetLastError};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, GetTokenInformation, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
    SE_DEBUG_NAME, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_ELEVATION,
    TOKEN_PRIVILEGES, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::handles::OwnedHandle;

pub fn enable_debug_privilege() -> Result<(), u32> {
    // Task Manager needs SeDebugPrivilege to query process tokens owned by services and SYSTEM.
    // AdjustTokenPrivileges may return success while reporting ERROR_NOT_ALL_ASSIGNED, so both
    // return channels must be checked.
    unsafe {
        let mut raw_token = null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut raw_token,
        ) == 0
        {
            return Err(GetLastError());
        }
        // 安全性: successful OpenProcessToken returns one owned token handle released by
        // CloseHandle; ownership moves directly into this guard.
        let Some(token) = OwnedHandle::from_raw(raw_token) else {
            return Err(ERROR_NOT_ALL_ASSIGNED);
        };

        let mut luid = zeroed();
        if LookupPrivilegeValueW(null(), SE_DEBUG_NAME, &mut luid) == 0 {
            return Err(GetLastError());
        }

        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        SetLastError(0);
        if AdjustTokenPrivileges(token.as_raw(), 0, &privileges, 0, null_mut(), null_mut()) == 0 {
            return Err(GetLastError());
        }

        let error = GetLastError();
        if error == 0 { Ok(()) } else { Err(error) }
    }
}

pub fn process_is_elevated() -> Result<bool, u32> {
    unsafe {
        let mut raw_token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) == 0 {
            let error = GetLastError();
            return Err(if error == 0 {
                ERROR_NOT_ALL_ASSIGNED
            } else {
                error
            });
        }
        // 安全性: successful OpenProcessToken returns one owned token handle released by
        // CloseHandle; ownership moves directly into this guard.
        let Some(token) = OwnedHandle::from_raw(raw_token) else {
            return Err(ERROR_NOT_ALL_ASSIGNED);
        };

        let mut elevation = zeroed::<TOKEN_ELEVATION>();
        let mut returned = 0u32;
        if GetTokenInformation(
            token.as_raw(),
            TokenElevation,
            &mut elevation as *mut _ as *mut _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        ) == 0
        {
            let error = GetLastError();
            return Err(if error == 0 {
                ERROR_NOT_ALL_ASSIGNED
            } else {
                error
            });
        }

        Ok(elevation.TokenIsElevated != 0)
    }
}

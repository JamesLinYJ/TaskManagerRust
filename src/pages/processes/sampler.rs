// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 进程快照采样
//
//   文件:       src/pages/processes/sampler.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Owns process enumeration, account/session enrichment, and successful-snapshot delta baselines.
//! Failed collections leave CPU and per-process baselines untouched.
//!
//! CPU, creation time, memory, and I/O counters come from one `SystemProcessInformation` query
//! on the worker thread. That query covers the Idle process and processes that refuse `OpenProcess`.
//! A limited, creation-time-checked open is used only for architecture, the primary token, and,
//! when those columns are visible, the command line and GDI/USER counts. A process that exits
//! during one of those queries keeps its snapshot row; the missing value stays unknown and
//! `row_error` is set. Unknown counters are not stored as zero.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::mem::{align_of, size_of};
use std::ptr::null_mut;
use std::slice;

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_GEN_FAILURE, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_DATA,
    ERROR_INVALID_PARAMETER, ERROR_NONE_MAPPED, GetLastError, HANDLE, LocalFree,
    RtlNtStatusToDosError, SetLastError,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    GetLengthSid, GetTokenInformation, IsValidSid, LookupAccountSidW, SID_NAME_USE, TOKEN_QUERY,
    TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTS_PROCESS_INFOW, WTSEnumerateProcessesW,
};
use windows_sys::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, GR_GDIOBJECTS, GR_USEROBJECTS,
    GetGuiResources, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS,
    OpenProcessToken, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    REALTIME_PRIORITY_CLASS,
};

use super::model::{DirtyColumns, ProcEntry, ProcStaticMetadata};
use super::tree::{ProcessLink, resolve_parent_identities};
use crate::infrastructure::native::{
    OwnedHandle, OwnedWtsMemory, query_process_architecture_handle, widestr_ptr_to_string,
};
use crate::system::cpu_sampler::{
    ProcessorPerformance, checked_summed_processor_times, query_processor_performance,
};
use crate::system::process_identity::{
    ProcIdentity, open_process_for_identity, query_process_identity_for_pid,
};

#[derive(Clone, Default)]
struct PreviousProcSample {
    // 上一轮采样值用于计算 CPU、内存增量和缺页增量。
    raw_cpu_time_100ns: u64,
    mem_usage_kb: u64,
    page_faults: u32,
}

// 列级脏标记位图。每轮刷新时标记变更列，rebuild_listview 只重绘被标记的行。
pub(super) struct WtsProcessIdentity {
    pub(super) user_name: Option<String>,
    pub(super) session_id: u32,
    pub(super) image_name_lower: String,
}

struct WtsProcessIdentitySnapshot {
    identities: HashMap<u32, WtsProcessIdentity>,
    row_error: Option<u32>,
}

#[derive(Clone)]
struct CachedAccountName {
    name: String,
    last_used: u64,
}

#[derive(Default)]
struct AccountNameCache {
    entries: HashMap<Vec<u8>, CachedAccountName>,
    generation: u64,
}

impl AccountNameCache {
    const MAX_ENTRIES: usize = 256;

    fn begin_refresh(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn get(&mut self, sid: &[u8]) -> Option<String> {
        let entry = self.entries.get_mut(sid)?;
        entry.last_used = self.generation;
        Some(entry.name.clone())
    }

    fn insert(&mut self, sid: Vec<u8>, name: String) {
        debug_assert!(!name.is_empty());
        if !self.entries.contains_key(sid.as_slice())
            && self.entries.len() >= Self::MAX_ENTRIES
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(sid, _)| sid.clone())
        {
            self.entries.remove(oldest.as_slice());
        }
        self.entries.insert(
            sid,
            CachedAccountName {
                name,
                last_used: self.generation,
            },
        );
    }
}

#[derive(Default)]
struct ProcWorkerCache {
    metadata: HashMap<ProcIdentity, ProcStaticMetadata>,
    account_names: AccountNameCache,
    processor_info: Vec<ProcessorPerformance>,
    // Reused across refreshes. Valid bytes are only those returned by the latest query.
    process_info: Vec<u8>,
}

// 常用文案缓存，避免命令执行路径上反复查本地化资源。
#[derive(Default)]
pub(super) struct ProcWorkerSnapshot {
    pub(super) entries: Vec<ProcEntry>,
    pub(super) row_error: Option<u32>,
}

struct CollectedProcessEntries {
    entries: Vec<ProcEntry>,
    next_samples: HashMap<ProcIdentity, PreviousProcSample>,
    row_error: Option<u32>,
}

pub(super) type ProcWorkerResult = Result<ProcWorkerSnapshot, u32>;

pub(super) struct ProcWorkerRequest {
    pub(super) processor_count: usize,
    pub(super) query_command_line: bool,
    pub(super) query_gui_objects: bool,
}

#[derive(Clone, Copy)]
struct ProcessDetailQuery {
    command_line: bool,
    gui_objects: bool,
}

#[derive(Default)]
pub(super) struct ProcWorkerState {
    cache: ProcWorkerCache,
    previous_samples: HashMap<ProcIdentity, PreviousProcSample>,
    previous_system_time: Option<u64>,
}

impl ProcWorkerState {
    pub(super) fn collect(&mut self, request: ProcWorkerRequest) -> ProcWorkerResult {
        let system_time =
            current_system_time(request.processor_count, &mut self.cache.processor_info)?;
        let total_delta = system_time_delta(system_time, self.previous_system_time);
        let collected = collect_process_entries(
            &self.previous_samples,
            total_delta,
            &mut self.cache,
            ProcessDetailQuery {
                command_line: request.query_command_line,
                gui_objects: request.query_gui_objects,
            },
        )?;

        // CPU and per-process deltas must share the same successful snapshot boundary. A failed
        // process walk leaves both baselines untouched so the next sample remains comparable.
        self.previous_samples = collected.next_samples;
        self.previous_system_time = Some(system_time);
        Ok(ProcWorkerSnapshot {
            entries: collected.entries,
            row_error: collected.row_error,
        })
    }
}

// 通过 LookupAccountSidW 将 SID 解析为经典 Task Manager 风格的账户名。
unsafe fn lookup_account_name_from_sid(sid: *mut core::ffi::c_void) -> Result<String, u32> {
    unsafe {
        if sid.is_null() || IsValidSid(sid) == 0 {
            return Err(ERROR_INVALID_DATA);
        }

        let mut name_len = 0u32;
        let mut domain_len = 0u32;
        let mut sid_use = 0 as SID_NAME_USE;
        if LookupAccountSidW(
            null_mut(),
            sid,
            null_mut(),
            &mut name_len,
            null_mut(),
            &mut domain_len,
            &mut sid_use,
        ) == 0
        {
            let error = GetLastError();
            if error == ERROR_NONE_MAPPED {
                return sid_string(sid);
            }
            if error != ERROR_INSUFFICIENT_BUFFER {
                return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
            }
        }

        if name_len == 0 {
            return Err(ERROR_INVALID_DATA);
        }

        let mut name = vec![0u16; name_len as usize];
        let mut domain = vec![0u16; domain_len as usize];
        let domain_ptr = if domain.is_empty() {
            null_mut()
        } else {
            domain.as_mut_ptr()
        };
        if LookupAccountSidW(
            null_mut(),
            sid,
            name.as_mut_ptr(),
            &mut name_len,
            domain_ptr,
            &mut domain_len,
            &mut sid_use,
        ) == 0
        {
            let error = GetLastError();
            if error == ERROR_NONE_MAPPED {
                return sid_string(sid);
            }
            return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
        }

        let available = (name_len as usize).min(name.len());
        let length = name[..available]
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(available);
        let name = String::from_utf16_lossy(&name[..length]);
        if name.is_empty() {
            Err(ERROR_INVALID_DATA)
        } else {
            Ok(name)
        }
    }
}

unsafe fn lookup_account_name_from_sid_cached(
    sid: *mut core::ffi::c_void,
    account_cache: &mut AccountNameCache,
) -> Result<String, u32> {
    unsafe {
        if sid.is_null() || IsValidSid(sid) == 0 {
            return lookup_account_name_from_sid(sid);
        }
        let length = GetLengthSid(sid) as usize;
        if length == 0 {
            return lookup_account_name_from_sid(sid);
        }
        let cache_key = slice::from_raw_parts(sid as *const u8, length);
        if let Some(name) = account_cache.get(cache_key) {
            return Ok(name);
        }

        let name = lookup_account_name_from_sid(sid)?;
        account_cache.insert(cache_key.to_vec(), name.clone());
        Ok(name)
    }
}

unsafe fn query_process_account_name(
    process_handle: HANDLE,
    account_cache: &mut AccountNameCache,
) -> Result<String, u32> {
    unsafe {
        let mut raw_token = null_mut();
        if OpenProcessToken(process_handle, TOKEN_QUERY, &mut raw_token) == 0 {
            let error = GetLastError();
            return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
        }
        // 安全性: successful OpenProcessToken returns one owned token handle released by
        // CloseHandle; no other owner is retained.
        let token = OwnedHandle::from_raw(raw_token).ok_or(ERROR_INVALID_DATA)?;

        let mut required_bytes = 0u32;
        if GetTokenInformation(
            token.as_raw(),
            TokenUser,
            null_mut(),
            0,
            &mut required_bytes,
        ) != 0
        {
            return Err(ERROR_INVALID_DATA);
        }
        let error = GetLastError();
        if error != ERROR_INSUFFICIENT_BUFFER || required_bytes < size_of::<TOKEN_USER>() as u32 {
            return Err(if error == 0 {
                ERROR_INVALID_DATA
            } else {
                error
            });
        }

        let word_count = (required_bytes as usize).div_ceil(size_of::<usize>());
        let mut storage = vec![0usize; word_count];
        let storage_bytes = storage
            .len()
            .checked_mul(size_of::<usize>())
            .and_then(|bytes| u32::try_from(bytes).ok())
            .ok_or(ERROR_INVALID_DATA)?;
        let mut returned_bytes = 0u32;
        if GetTokenInformation(
            token.as_raw(),
            TokenUser,
            storage.as_mut_ptr() as *mut _,
            storage_bytes,
            &mut returned_bytes,
        ) == 0
        {
            let error = GetLastError();
            return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
        }
        if returned_bytes < size_of::<TOKEN_USER>() as u32 || returned_bytes > storage_bytes {
            return Err(ERROR_INVALID_DATA);
        }

        let token_user = &*(storage.as_ptr() as *const TOKEN_USER);
        lookup_account_name_from_sid_cached(token_user.User.Sid, account_cache)
    }
}

unsafe fn sid_string(sid: *mut core::ffi::c_void) -> Result<String, u32> {
    unsafe {
        let mut string_sid = null_mut::<u16>();
        if ConvertSidToStringSidW(sid, &mut string_sid) == 0 || string_sid.is_null() {
            let error = GetLastError();
            return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
        }

        let value = widestr_ptr_to_string(string_sid);
        if !LocalFree(string_sid as _).is_null() {
            let error = GetLastError();
            return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
        }
        Ok(value)
    }
}

unsafe fn collect_process_identity_map(
    required_pids: &HashSet<u32>,
    required_user_pids: &HashSet<u32>,
    account_cache: &mut AccountNameCache,
) -> Result<WtsProcessIdentitySnapshot, u32> {
    unsafe {
        if required_pids.is_empty() {
            return Ok(WtsProcessIdentitySnapshot {
                identities: HashMap::new(),
                row_error: None,
            });
        }
        // WTS_PROCESS_INFO documents pUserSid as the SID from the process primary token. WTS is
        // also the batch source for session IDs; direct token results remain authoritative when
        // both sources are available because they are tied to a creation-time-verified handle.
        let mut process_info = null_mut::<WTS_PROCESS_INFOW>();
        let mut count = 0u32;

        let enumeration_result = WTSEnumerateProcessesW(
            WTS_CURRENT_SERVER_HANDLE,
            0,
            1,
            &mut process_info,
            &mut count,
        );
        if enumeration_result == 0 {
            let error = windows_sys::Win32::Foundation::GetLastError();
            return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
        }
        // 安全性: successful WTSEnumerateProcessesW transfers a uniquely owned array whose
        // documented release function is WTSFreeMemory.
        let process_info = OwnedWtsMemory::from_raw(process_info).ok_or(ERROR_INVALID_DATA)?;

        let mut identities = HashMap::with_capacity(count as usize);
        let mut row_error = None;
        let processes = slice::from_raw_parts(process_info.as_ptr(), count as usize);
        for process in processes {
            let pid = process.ProcessId;
            if !required_pids.contains(&pid) {
                continue;
            }
            let user_name = if !required_user_pids.contains(&pid) {
                None
            } else if pid == 0 {
                Some("SYSTEM".to_string())
            } else {
                match lookup_account_name_from_sid_cached(process.pUserSid, account_cache) {
                    Ok(name) => Some(name),
                    Err(error) => {
                        row_error.get_or_insert(error);
                        None
                    }
                }
            };
            identities.insert(
                pid,
                WtsProcessIdentity {
                    user_name,
                    session_id: process.SessionId,
                    image_name_lower: widestr_ptr_to_string(process.pProcessName).to_lowercase(),
                },
            );
        }

        Ok(WtsProcessIdentitySnapshot {
            identities,
            row_error,
        })
    }
}

/// Whether a WTS row, which is keyed only by PID, still names `expected`.
///
/// `live` is `None` only when this identity has already refused `PROCESS_QUERY_LIMITED_INFORMATION`
/// and must not be opened again. Access denied cannot prove the PID was reused, so the image and
/// session check remains. Any other open failure, or a different creation time, means the row
/// belongs to a process that exited or was replaced.
pub(super) fn wts_live_identity_allows_merge(
    expected: ProcIdentity,
    live: Option<Result<ProcIdentity, u32>>,
) -> bool {
    if !expected.is_verified() {
        return false;
    }
    match live {
        None => true,
        Some(Ok(live)) => live == expected,
        Some(Err(ERROR_ACCESS_DENIED)) => true,
        Some(Err(_)) => false,
    }
}

pub(super) fn merge_wts_process_identity(
    entry: &mut ProcEntry,
    wts_identity: Option<&WtsProcessIdentity>,
) -> bool {
    let Some(wts_identity) = wts_identity else {
        return false;
    };

    // WTS only keys rows by PID. The caller rejects a live identity that is gone or reused
    // before this image and session check copies an account name onto the row.
    if !wts_identity_matches(&entry.image_name_lower, entry.session_id, wts_identity) {
        return false;
    }

    let user_name = wts_identity
        .user_name
        .as_ref()
        .filter(|user_name| !user_name.is_empty());
    if entry.user_name.is_empty()
        && let Some(user_name) = user_name
    {
        entry.user_name.clone_from(user_name);
        entry.user_name_lower = user_name.to_lowercase();
    }
    if entry.session_id.is_none() {
        entry.session_id = Some(wts_identity.session_id);
    }
    user_name.is_some()
}

pub(super) fn wts_identity_matches(
    image_name_lower: &str,
    session_id: Option<u32>,
    wts_identity: &WtsProcessIdentity,
) -> bool {
    wts_identity.image_name_lower == image_name_lower
        && session_id.is_none_or(|session_id| session_id == wts_identity.session_id)
}

// 从 Options 中解析当前激活的列列表，过滤无效列 ID。
fn kb_from_bytes(value: usize) -> u64 {
    (value as u64) / 1024
}

pub(super) fn signed_kb_delta(current: u64, previous: u64) -> i64 {
    if current >= previous {
        (current - previous).min(i64::MAX as u64) as i64
    } else {
        -((previous - current).min(i64::MAX as u64) as i64)
    }
}

// SystemProcessInformation class 5. Times sit at fixed offsets on both pointer widths;
// pointer-sized fields below ImageName follow the Windows 7+ layout.
const SYSTEM_PROCESS_INFORMATION_CLASS: i32 = 5;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004u32 as i32;
const STATUS_BUFFER_TOO_SMALL: i32 = 0xC000_0023u32 as i32;
const STATUS_INVALID_BUFFER_SIZE: i32 = 0xC000_0206u32 as i32;
const PROCESS_INFO_INITIAL_BYTES: usize = 256 * 1024;
const PROCESS_INFO_MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_PARSED_PROCESSES: usize = 100_000;
const MAX_IMAGE_NAME_BYTES: usize = 32_768;

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySystemInformation(
        system_information_class: i32,
        system_information: *mut c_void,
        system_information_length: u32,
        return_length: *mut u32,
    ) -> i32;

    fn NtQueryInformationProcess(
        process_handle: HANDLE,
        process_information_class: i32,
        process_information: *mut c_void,
        process_information_length: u32,
        return_length: *mut u32,
    ) -> i32;
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SystemProcessInformation {
    next_entry_offset: u32,
    number_of_threads: u32,
    working_set_private_size: i64,
    hard_fault_count: u32,
    number_of_threads_high_watermark: u32,
    cycle_time: u64,
    create_time: i64,
    user_time: i64,
    kernel_time: i64,
    image_name: UnicodeString,
    base_priority: i32,
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
    handle_count: u32,
    session_id: u32,
    unique_process_key: usize,
    peak_virtual_size: usize,
    virtual_size: usize,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_page_count: usize,
    read_operation_count: i64,
    write_operation_count: i64,
    other_operation_count: i64,
    read_transfer_count: i64,
    write_transfer_count: i64,
    other_transfer_count: i64,
}

const _: () = {
    assert!(std::mem::offset_of!(SystemProcessInformation, create_time) == 0x20);
    assert!(std::mem::offset_of!(SystemProcessInformation, user_time) == 0x28);
    assert!(std::mem::offset_of!(SystemProcessInformation, kernel_time) == 0x30);
    assert!(std::mem::offset_of!(SystemProcessInformation, image_name) == 0x38);
};
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<SystemProcessInformation>() == 0x100);
    assert!(std::mem::offset_of!(SystemProcessInformation, unique_process_id) == 0x50);
    assert!(std::mem::offset_of!(SystemProcessInformation, working_set_size) == 0x90);
    assert!(std::mem::offset_of!(SystemProcessInformation, pagefile_usage) == 0xB8);
};
#[cfg(target_pointer_width = "32")]
const _: () = {
    assert!(size_of::<SystemProcessInformation>() == 0xB8);
    assert!(std::mem::offset_of!(SystemProcessInformation, unique_process_id) == 0x44);
    assert!(std::mem::offset_of!(SystemProcessInformation, working_set_size) == 0x68);
    assert!(std::mem::offset_of!(SystemProcessInformation, pagefile_usage) == 0x7C);
};

#[derive(Clone)]
struct ParsedProcess {
    pid: u32,
    creation_time_100ns: u64,
    /// Parent PID from this same `SystemProcessInformation` buffer.
    parent_pid: Option<u32>,
    cpu_time_100ns: Option<u64>,
    image_name: String,
    session_id: u32,
    thread_count: u32,
    handle_count: u32,
    base_priority: i32,
    working_set_bytes: usize,
    page_faults: u32,
    commit_bytes: usize,
    paged_pool_bytes: usize,
    nonpaged_pool_bytes: usize,
    io_read_bytes: Option<u64>,
    io_write_bytes: Option<u64>,
    io_read_count: Option<u64>,
    io_write_count: Option<u64>,
}

fn query_system_process_information(buffer: &mut Vec<u8>) -> Result<usize, u32> {
    if buffer.len() < PROCESS_INFO_INITIAL_BYTES {
        buffer.resize(PROCESS_INFO_INITIAL_BYTES, 0);
    }
    loop {
        let Ok(length) = u32::try_from(buffer.len()) else {
            return Err(ntstatus_to_win32(STATUS_INVALID_BUFFER_SIZE));
        };
        let mut returned = 0u32;
        // SAFETY: the worker owns `buffer` for `length` bytes and does not share it. The call
        // writes at most `length` bytes and reports the used size in `returned`.
        let status = unsafe {
            NtQuerySystemInformation(
                SYSTEM_PROCESS_INFORMATION_CLASS,
                buffer.as_mut_ptr().cast(),
                length,
                &mut returned,
            )
        };
        if status >= 0 {
            let returned = returned as usize;
            if returned == 0 || returned > buffer.len() {
                return Err(ERROR_INVALID_DATA);
            }
            return Ok(returned);
        }
        if status != STATUS_INFO_LENGTH_MISMATCH && status != STATUS_BUFFER_TOO_SMALL {
            return Err(ntstatus_to_win32(status));
        }
        let next = if returned as usize > buffer.len() {
            returned as usize
        } else {
            buffer.len().saturating_mul(2)
        };
        if next <= buffer.len() || next > PROCESS_INFO_MAX_BYTES {
            return Err(ntstatus_to_win32(status));
        }
        buffer.resize(next, 0);
    }
}

fn parse_system_process_information(
    buffer: &[u8],
) -> Result<(Vec<ParsedProcess>, Option<u32>), u32> {
    if buffer.len() < size_of::<SystemProcessInformation>() {
        return Err(ERROR_INVALID_DATA);
    }
    let mut rows = Vec::new();
    let mut row_error = None;
    let mut offset = 0usize;
    let header_size = size_of::<SystemProcessInformation>();
    loop {
        if rows.len() >= MAX_PARSED_PROCESSES {
            return Err(ERROR_INVALID_DATA);
        }
        let header = read_process_header(buffer, offset)?;
        if let Some(row) = parsed_process(buffer, &header, &mut row_error) {
            rows.push(row);
        }
        if header.next_entry_offset == 0 {
            break;
        }
        let step = usize::try_from(header.next_entry_offset).map_err(|_| ERROR_INVALID_DATA)?;
        if step < header_size {
            return Err(ERROR_INVALID_DATA);
        }
        let Some(next) = offset.checked_add(step) else {
            return Err(ERROR_INVALID_DATA);
        };
        if next <= offset || buffer.len().saturating_sub(next) < header_size {
            return Err(ERROR_INVALID_DATA);
        }
        offset = next;
    }
    if rows.is_empty() {
        return Err(ERROR_INVALID_DATA);
    }
    Ok((rows, row_error))
}

fn read_process_header(buffer: &[u8], offset: usize) -> Result<SystemProcessInformation, u32> {
    let header_size = size_of::<SystemProcessInformation>();
    let Some(end) = offset.checked_add(header_size) else {
        return Err(ERROR_INVALID_DATA);
    };
    if end > buffer.len() {
        return Err(ERROR_INVALID_DATA);
    }
    // SAFETY: `offset..end` is inside `buffer`. `read_unaligned` copies the header without
    // requiring the struct's natural alignment. The buffer is not mutated during the copy.
    let header = unsafe {
        buffer
            .as_ptr()
            .add(offset)
            .cast::<SystemProcessInformation>()
            .read_unaligned()
    };
    Ok(header)
}

fn parsed_process(
    buffer: &[u8],
    header: &SystemProcessInformation,
    row_error: &mut Option<u32>,
) -> Option<ParsedProcess> {
    let Ok(pid) = u32::try_from(header.unique_process_id) else {
        row_error.get_or_insert(ERROR_INVALID_DATA);
        return None;
    };
    let creation_time_100ns = if header.create_time < 0 {
        row_error.get_or_insert(ERROR_INVALID_DATA);
        0
    } else {
        header.create_time as u64
    };
    let parent_pid = u32::try_from(header.inherited_from_unique_process_id).ok();
    if parent_pid.is_none() {
        row_error.get_or_insert(ERROR_INVALID_DATA);
    }
    let image_name = match image_name_from_process_buffer(buffer, header.image_name) {
        Ok(name) => name,
        Err(error) => {
            row_error.get_or_insert(error);
            String::new()
        }
    };
    Some(ParsedProcess {
        pid,
        creation_time_100ns,
        parent_pid,
        cpu_time_100ns: cpu_time_from_counters(header.kernel_time, header.user_time, row_error),
        image_name,
        session_id: header.session_id,
        thread_count: header.number_of_threads,
        handle_count: header.handle_count,
        base_priority: header.base_priority,
        working_set_bytes: header.working_set_size,
        page_faults: header.page_fault_count,
        commit_bytes: header.pagefile_usage,
        paged_pool_bytes: header.quota_paged_pool_usage,
        nonpaged_pool_bytes: header.quota_non_paged_pool_usage,
        io_read_bytes: counted_u64(header.read_transfer_count, row_error),
        io_write_bytes: counted_u64(header.write_transfer_count, row_error),
        io_read_count: counted_u64(header.read_operation_count, row_error),
        io_write_count: counted_u64(header.write_operation_count, row_error),
    })
}

fn counted_u64(value: i64, row_error: &mut Option<u32>) -> Option<u64> {
    if value < 0 {
        row_error.get_or_insert(ERROR_INVALID_DATA);
        None
    } else {
        Some(value as u64)
    }
}

fn cpu_time_from_counters(kernel: i64, user: i64, row_error: &mut Option<u32>) -> Option<u64> {
    if kernel < 0 || user < 0 {
        row_error.get_or_insert(ERROR_INVALID_DATA);
        return None;
    }
    match u64::try_from(kernel).and_then(|kernel| u64::try_from(user).map(|user| (kernel, user))) {
        Ok((kernel, user)) => match kernel.checked_add(user) {
            Some(sum) => Some(sum),
            None => {
                row_error.get_or_insert(ERROR_INVALID_DATA);
                None
            }
        },
        Err(_) => {
            row_error.get_or_insert(ERROR_INVALID_DATA);
            None
        }
    }
}

fn image_name_from_process_buffer(buffer: &[u8], name: UnicodeString) -> Result<String, u32> {
    if name.length == 0 {
        return Ok(String::new());
    }
    if name.buffer.is_null() || !name.length.is_multiple_of(2) {
        return Err(ERROR_INVALID_DATA);
    }
    let bytes = usize::from(name.length);
    if bytes > MAX_IMAGE_NAME_BYTES {
        return Err(ERROR_INVALID_DATA);
    }
    let buffer_start = buffer.as_ptr() as usize;
    let Some(buffer_end) = buffer_start.checked_add(buffer.len()) else {
        return Err(ERROR_INVALID_DATA);
    };
    let name_start = name.buffer as usize;
    if !name_start.is_multiple_of(align_of::<u16>()) {
        return Err(ERROR_INVALID_DATA);
    }
    let Some(name_end) = name_start.checked_add(bytes) else {
        return Err(ERROR_INVALID_DATA);
    };
    if name_start < buffer_start || name_end > buffer_end {
        return Err(ERROR_INVALID_DATA);
    }
    // SAFETY: `name_start` is 2-aligned and `name_start..name_end` lies inside `buffer`, which
    // outlives this read. The kernel, or the test builder, placed the UTF-16 units there.
    let units = unsafe { slice::from_raw_parts(name.buffer, bytes / 2) };
    let end = units
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(units.len());
    Ok(String::from_utf16_lossy(&units[..end]))
}

fn priority_class_from_base_priority(base_priority: i32) -> u32 {
    match base_priority {
        4 => IDLE_PRIORITY_CLASS,
        6 => BELOW_NORMAL_PRIORITY_CLASS,
        8 => NORMAL_PRIORITY_CLASS,
        10 => ABOVE_NORMAL_PRIORITY_CLASS,
        13 => HIGH_PRIORITY_CLASS,
        24 => REALTIME_PRIORITY_CLASS,
        _ => 0,
    }
}

fn build_process_entries(
    rows: &[ParsedProcess],
    previous_samples: &HashMap<ProcIdentity, PreviousProcSample>,
    total_delta: Option<u64>,
) -> CollectedProcessEntries {
    let mut entries = Vec::with_capacity(rows.len());
    let mut next_samples = HashMap::with_capacity(rows.len());
    let mut row_error = None;
    for row in rows {
        let identity = if row.creation_time_100ns == 0 {
            ProcIdentity::pid_only(row.pid)
        } else {
            ProcIdentity::new(row.pid, row.creation_time_100ns)
        };
        let identity_usable = row.pid == 0 || row.creation_time_100ns != 0;
        let image_name = if row.pid == 0 && row.image_name.is_empty() {
            "System Idle Process".to_string()
        } else {
            row.image_name.clone()
        };
        let mut entry = ProcEntry {
            identity,
            parent: None,
            pid: row.pid,
            image_name: image_name.clone(),
            image_name_lower: image_name.to_lowercase(),
            architecture: None,
            user_name: if row.pid == 0 {
                "SYSTEM".to_string()
            } else {
                String::new()
            },
            user_name_lower: if row.pid == 0 {
                "system".to_string()
            } else {
                String::new()
            },
            session_id: if row.pid == 0 {
                Some(0)
            } else {
                Some(row.session_id)
            },
            cpu: 0,
            cpu_time_100ns: 0,
            display_cpu_time_100ns: 0,
            mem_usage_kb: kb_from_bytes(row.working_set_bytes),
            mem_diff_kb: 0,
            page_faults: row.page_faults,
            page_faults_diff: 0,
            commit_charge_kb: kb_from_bytes(row.commit_bytes),
            paged_pool_kb: kb_from_bytes(row.paged_pool_bytes),
            nonpaged_pool_kb: kb_from_bytes(row.nonpaged_pool_bytes),
            priority_class: priority_class_from_base_priority(row.base_priority),
            handle_count: row.handle_count,
            thread_count: row.thread_count,
            command_line: None,
            io_read_bytes: row.io_read_bytes,
            io_write_bytes: row.io_write_bytes,
            io_read_count: row.io_read_count,
            io_write_count: row.io_write_count,
            gdi_objects: None,
            user_objects: None,
            display_text: std::array::from_fn(|_| String::new()),
            pass_count: 0,
            dirty_columns: DirtyColumns::default(),
        };
        if !identity_usable || row.cpu_time_100ns.is_none() {
            row_error.get_or_insert(ERROR_INVALID_DATA);
        }
        if row.io_read_bytes.is_none()
            || row.io_write_bytes.is_none()
            || row.io_read_count.is_none()
            || row.io_write_count.is_none()
        {
            row_error.get_or_insert(ERROR_INVALID_DATA);
        }
        if identity_usable && let Some(previous) = previous_samples.get(&identity) {
            entry.mem_diff_kb = signed_kb_delta(entry.mem_usage_kb, previous.mem_usage_kb);
            entry.page_faults_diff = i64::from(entry.page_faults) - i64::from(previous.page_faults);
            if let Some(cpu_time) = row.cpu_time_100ns
                && let Some(total_delta) = total_delta
                && let Some(process_delta) = cpu_time.checked_sub(previous.raw_cpu_time_100ns)
            {
                entry.cpu = cpu_percent_from_delta(process_delta, total_delta);
            }
        }
        if identity_usable && let Some(cpu_time) = row.cpu_time_100ns {
            entry.cpu_time_100ns = cpu_time;
            entry.display_cpu_time_100ns = cpu_time;
            next_samples.insert(
                identity,
                PreviousProcSample {
                    raw_cpu_time_100ns: cpu_time,
                    mem_usage_kb: entry.mem_usage_kb,
                    page_faults: entry.page_faults,
                },
            );
        }
        entries.push(entry);
    }
    // Parent links come only from this snapshot. A row that failed to parse was
    // already omitted, so its children stay roots and the other rows remain.
    let links = rows
        .iter()
        .zip(entries.iter())
        .map(|(row, entry)| ProcessLink {
            identity: entry.identity,
            parent_pid: row.parent_pid,
        })
        .collect::<Vec<_>>();
    for (entry, parent) in entries.iter_mut().zip(resolve_parent_identities(&links)) {
        entry.parent = parent;
    }
    CollectedProcessEntries {
        entries,
        next_samples,
        row_error,
    }
}

fn collect_process_entries(
    previous_samples: &HashMap<ProcIdentity, PreviousProcSample>,
    total_delta: Option<u64>,
    cache: &mut ProcWorkerCache,
    details: ProcessDetailQuery,
) -> Result<CollectedProcessEntries, u32> {
    let returned = query_system_process_information(&mut cache.process_info)?;
    let (rows, mut row_error) = parse_system_process_information(&cache.process_info[..returned])?;
    let mut collected = build_process_entries(&rows, previous_samples, total_delta);
    if row_error.is_none() {
        row_error = collected.row_error;
    }

    cache.account_names.begin_refresh();
    let mut resolved_users = HashSet::with_capacity(collected.entries.len());
    let mut unresolved_user_errors = HashMap::new();
    let mut denied = HashSet::new();
    enrich_process_entries(
        &mut collected.entries,
        cache,
        &mut resolved_users,
        &mut unresolved_user_errors,
        &mut denied,
        &mut row_error,
    );

    let required_user_pids = collected
        .entries
        .iter()
        .filter(|entry| {
            entry.pid != 0
                && (!entry.identity.is_verified() || !resolved_users.contains(&entry.identity))
        })
        .map(|entry| entry.pid)
        .collect::<HashSet<_>>();
    let required_identity_pids = collected
        .entries
        .iter()
        .filter(|entry| {
            entry.pid != 0
                && (entry.session_id.is_none() || required_user_pids.contains(&entry.pid))
        })
        .map(|entry| entry.pid)
        .collect::<HashSet<_>>();
    // SAFETY: the PID sets and account cache are owned by this worker call. The callee does not
    // retain WTS pointers after it returns.
    match unsafe {
        collect_process_identity_map(
            &required_identity_pids,
            &required_user_pids,
            &mut cache.account_names,
        )
    } {
        Ok(identity_snapshot) => {
            row_error = row_error.or(identity_snapshot.row_error);
            for entry in collected.entries.iter_mut().filter(|entry| entry.pid != 0) {
                let Some(wts_identity) = identity_snapshot.identities.get(&entry.pid) else {
                    continue;
                };
                if !wts_identity_matches(&entry.image_name_lower, entry.session_id, wts_identity) {
                    continue;
                }
                let open_already_denied = denied.contains(&entry.identity)
                    || cache
                        .metadata
                        .get(&entry.identity)
                        .is_some_and(|metadata| metadata.query_access_denied);
                let live = if open_already_denied {
                    None
                } else {
                    Some(query_process_identity_for_pid(entry.pid))
                };
                if let Some(Err(error)) = live {
                    if error == ERROR_ACCESS_DENIED && entry.identity.is_verified() {
                        denied.insert(entry.identity);
                    } else if error != ERROR_ACCESS_DENIED && error != ERROR_INVALID_PARAMETER {
                        row_error.get_or_insert(error);
                    }
                }
                if !wts_live_identity_allows_merge(entry.identity, live) {
                    continue;
                }
                if merge_wts_process_identity(entry, Some(wts_identity))
                    && entry.identity.is_verified()
                {
                    resolved_users.insert(entry.identity);
                    unresolved_user_errors.remove(&entry.identity);
                }
            }
        }
        Err(error) => {
            row_error.get_or_insert(error);
        }
    }
    if let Some(error) = collected
        .entries
        .iter()
        .filter(|entry| entry.identity.is_verified() && !resolved_users.contains(&entry.identity))
        .find_map(|entry| unresolved_user_errors.get(&entry.identity).copied())
    {
        row_error.get_or_insert(error);
    }

    let current_identities = collected
        .entries
        .iter()
        .map(|entry| entry.identity)
        .filter(|identity| identity.is_verified())
        .collect::<HashSet<_>>();
    cache
        .metadata
        .retain(|identity, _| current_identities.contains(identity));
    for entry in collected
        .entries
        .iter()
        .filter(|entry| entry.identity.is_verified())
    {
        let resolved = resolved_users.contains(&entry.identity);
        let access_denied = denied.contains(&entry.identity);
        if let Some(metadata) = cache.metadata.get_mut(&entry.identity) {
            if metadata.architecture.is_none() && entry.architecture.is_some() {
                metadata.architecture = entry.architecture;
            }
            if metadata.session_id.is_none() && entry.session_id.is_some() {
                metadata.session_id = entry.session_id;
            }
            if resolved && !metadata.user_identity_resolved {
                metadata.user_name.clone_from(&entry.user_name);
                metadata.user_name_lower.clone_from(&entry.user_name_lower);
                metadata.user_identity_resolved = true;
            }
            if access_denied {
                metadata.query_access_denied = true;
            }
        } else {
            cache.metadata.insert(
                entry.identity,
                ProcStaticMetadata {
                    architecture: entry.architecture,
                    user_name: entry.user_name.clone(),
                    user_name_lower: entry.user_name_lower.clone(),
                    session_id: entry.session_id,
                    user_identity_resolved: resolved,
                    query_access_denied: access_denied,
                },
            );
        }
    }

    query_process_details(&mut collected.entries, details, &mut row_error);
    collected.row_error = row_error;
    Ok(collected)
}

fn query_process_details(
    entries: &mut [ProcEntry],
    query: ProcessDetailQuery,
    row_error: &mut Option<u32>,
) {
    if !query.command_line && !query.gui_objects {
        return;
    }
    for entry in entries.iter_mut() {
        if entry.pid == 0 || !entry.identity.is_verified() {
            continue;
        }
        // GDI/USER counts need PROCESS_QUERY_INFORMATION. Command line is available with
        // PROCESS_QUERY_LIMITED_INFORMATION, so an access-denied full query still tries that.
        let mut command_line_done = false;
        if query.gui_objects {
            match open_process_for_identity(
                entry.identity,
                PROCESS_QUERY_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION,
            ) {
                Ok(handle) => {
                    if query.command_line {
                        apply_command_line(entry, handle.as_raw(), row_error);
                        command_line_done = true;
                    }
                    apply_gui_objects(entry, handle.as_raw(), row_error);
                }
                Err(error) => remember_row_error(row_error, error),
            }
        }
        if query.command_line && !command_line_done {
            match open_process_for_identity(entry.identity, PROCESS_QUERY_LIMITED_INFORMATION) {
                Ok(handle) => apply_command_line(entry, handle.as_raw(), row_error),
                Err(error) => remember_row_error(row_error, error),
            }
        }
    }
}

fn apply_command_line(entry: &mut ProcEntry, handle: HANDLE, row_error: &mut Option<u32>) {
    match query_command_line(handle) {
        Ok(command_line) => entry.command_line = Some(command_line),
        Err(error) => {
            entry.command_line = None;
            remember_row_error(row_error, error);
        }
    }
}

fn apply_gui_objects(entry: &mut ProcEntry, handle: HANDLE, row_error: &mut Option<u32>) {
    match query_gui_objects(handle) {
        Ok((gdi_objects, user_objects)) => {
            entry.gdi_objects = Some(gdi_objects);
            entry.user_objects = Some(user_objects);
        }
        Err(error) => {
            entry.gdi_objects = None;
            entry.user_objects = None;
            remember_row_error(row_error, error);
        }
    }
}

fn remember_row_error(row_error: &mut Option<u32>, error: u32) {
    row_error.get_or_insert(if error == 0 { ERROR_GEN_FAILURE } else { error });
}

const PROCESS_COMMAND_LINE_INFORMATION: i32 = 60;
const COMMAND_LINE_INITIAL_BYTES: usize = 512;
/// `UNICODE_STRING::Length` counts bytes in a `u16`. A UTF-16 command line is even, so
/// 32,767 code units is the longest value that field can describe.
const MAX_COMMAND_LINE_UNITS: usize = 32_767;
/// Room for the `UnicodeString` header plus the longest legal command line. The header
/// size depends on pointer width, so it is not a fixed 64 KiB.
const COMMAND_LINE_MAX_BYTES: usize =
    size_of::<UnicodeString>() + MAX_COMMAND_LINE_UNITS * size_of::<u16>();
const STATUS_BUFFER_OVERFLOW: i32 = 0x8000_0005u32 as i32;

fn query_command_line(handle: HANDLE) -> Result<String, u32> {
    let mut buffer = vec![0u8; COMMAND_LINE_INITIAL_BYTES];
    loop {
        let Ok(length) = u32::try_from(buffer.len()) else {
            return Err(ERROR_INVALID_DATA);
        };
        let mut returned = 0u32;
        // SAFETY: `handle` is a creation-time-checked process handle owned by the caller. The
        // worker owns `buffer` for this call and does not share it. The kernel writes at most
        // `length` bytes and reports the used size in `returned`.
        let status = unsafe {
            NtQueryInformationProcess(
                handle,
                PROCESS_COMMAND_LINE_INFORMATION,
                buffer.as_mut_ptr().cast(),
                length,
                &mut returned,
            )
        };
        if status >= 0 {
            let returned = returned as usize;
            if returned == 0 || returned > buffer.len() {
                return Err(ERROR_INVALID_DATA);
            }
            return parse_process_command_line(&buffer[..returned]);
        }
        if status != STATUS_INFO_LENGTH_MISMATCH
            && status != STATUS_BUFFER_TOO_SMALL
            && status != STATUS_BUFFER_OVERFLOW
        {
            return Err(ntstatus_to_win32(status));
        }
        let Some(next) = next_command_line_capacity(buffer.len(), returned as usize) else {
            return Err(ntstatus_to_win32(status));
        };
        buffer.resize(next, 0);
    }
}

fn parse_process_command_line(buffer: &[u8]) -> Result<String, u32> {
    if buffer.len() < size_of::<UnicodeString>() {
        return Err(ERROR_INVALID_DATA);
    }
    // SAFETY: `buffer` holds at least one `UnicodeString`. `read_unaligned` copies the header
    // without requiring the struct alignment. The buffer is not mutated during the copy.
    let header = unsafe { buffer.as_ptr().cast::<UnicodeString>().read_unaligned() };
    if header.length == 0 {
        return Ok(String::new());
    }
    if !header.length.is_multiple_of(2) {
        return Err(ERROR_INVALID_DATA);
    }
    let bytes = usize::from(header.length);
    if bytes > MAX_COMMAND_LINE_UNITS * size_of::<u16>() {
        return Err(ERROR_INVALID_DATA);
    }
    let units = command_line_units(buffer, header.buffer, bytes)?;
    let end = units
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(units.len());
    Ok(String::from_utf16_lossy(&units[..end]))
}

fn next_command_line_capacity(current: usize, returned: usize) -> Option<usize> {
    let next = if returned > current {
        returned
    } else {
        // Doubling can jump past the header-inclusive cap. Try that cap once before failing,
        // so a legal 32,767-unit command line still fits.
        current.saturating_mul(2).min(COMMAND_LINE_MAX_BYTES)
    };
    if next > current && next <= COMMAND_LINE_MAX_BYTES {
        Some(next)
    } else {
        None
    }
}

fn command_line_units(buffer: &[u8], raw_buffer: *const u16, bytes: usize) -> Result<&[u16], u32> {
    let buffer_start = buffer.as_ptr() as usize;
    let Some(buffer_end) = buffer_start.checked_add(buffer.len()) else {
        return Err(ERROR_INVALID_DATA);
    };
    let pointer = raw_buffer as usize;
    let absolute = pointer >= buffer_start
        && pointer
            .checked_add(bytes)
            .is_some_and(|end| end <= buffer_end)
        && pointer.is_multiple_of(align_of::<u16>());
    let start = if absolute {
        pointer
    } else if pointer != 0
        && pointer.is_multiple_of(align_of::<u16>())
        && pointer
            .checked_add(bytes)
            .is_some_and(|end| end <= buffer.len())
    {
        buffer_start + pointer
    } else {
        return Err(ERROR_INVALID_DATA);
    };
    // SAFETY: `start` is aligned for `u16` and `start..start + bytes` lies inside `buffer`,
    // which outlives this slice. The kernel, or the test builder, placed the UTF-16 units there.
    let units = unsafe { slice::from_raw_parts(start as *const u16, bytes / 2) };
    Ok(units)
}

fn classify_gui_count(count: u32, last_error: u32) -> Result<u32, u32> {
    if count == 0 && last_error != 0 {
        Err(last_error)
    } else {
        Ok(count)
    }
}

fn query_gui_objects(handle: HANDLE) -> Result<(u32, u32), u32> {
    Ok((
        query_gui_count(handle, GR_GDIOBJECTS)?,
        query_gui_count(handle, GR_USEROBJECTS)?,
    ))
}

fn query_gui_count(handle: HANDLE, flag: u32) -> Result<u32, u32> {
    // SAFETY: `handle` is a creation-time-checked process handle owned by the caller.
    // GetGuiResources documents GetLastError when it returns zero, so the last-error slot is
    // cleared first and read only for that zero result. A zero count with no error is a real zero.
    unsafe {
        SetLastError(0);
        let count = GetGuiResources(handle, flag);
        let error = if count == 0 { GetLastError() } else { 0 };
        classify_gui_count(count, error)
    }
}

fn enrich_process_entries(
    entries: &mut [ProcEntry],
    cache: &mut ProcWorkerCache,
    resolved_users: &mut HashSet<ProcIdentity>,
    unresolved_user_errors: &mut HashMap<ProcIdentity, u32>,
    denied: &mut HashSet<ProcIdentity>,
    row_error: &mut Option<u32>,
) {
    for entry in entries {
        if entry.pid == 0 {
            resolved_users.insert(entry.identity);
            continue;
        }
        let mut access_was_denied = false;
        if let Some(metadata) = cache.metadata.get(&entry.identity) {
            access_was_denied = metadata.query_access_denied;
            let queried_session = entry.session_id;
            entry.apply_static_metadata(metadata);
            if entry.session_id.is_none() {
                entry.session_id = queried_session;
            }
            if metadata.user_identity_resolved {
                resolved_users.insert(entry.identity);
            }
        }
        // Architecture and the primary token are not in the system snapshot. Open once per
        // verified identity. Access-denied identities keep their CPU row and are not opened again.
        if access_was_denied || !entry.identity.is_verified() || entry.architecture.is_some() {
            continue;
        }
        match open_process_for_identity(entry.identity, PROCESS_QUERY_LIMITED_INFORMATION) {
            Ok(handle) => {
                entry.architecture = match query_process_architecture_handle(handle.as_raw()) {
                    Ok(architecture) => Some(architecture),
                    Err(error) => {
                        row_error.get_or_insert(if error == 0 { ERROR_GEN_FAILURE } else { error });
                        None
                    }
                };
                if !resolved_users.contains(&entry.identity) {
                    // SAFETY: `handle` is the creation-time-checked process handle owned for this call.
                    match unsafe {
                        query_process_account_name(handle.as_raw(), &mut cache.account_names)
                    } {
                        Ok(user_name) => {
                            entry.user_name_lower = user_name.to_lowercase();
                            entry.user_name = user_name;
                            resolved_users.insert(entry.identity);
                        }
                        Err(error) => {
                            unresolved_user_errors.insert(entry.identity, error);
                        }
                    }
                }
            }
            Err(error) => {
                if error == ERROR_ACCESS_DENIED {
                    denied.insert(entry.identity);
                }
                row_error.get_or_insert(error);
            }
        }
    }
}

// 读取系统总 CPU 时间（kernel + user），用于计算每轮刷新的 CPU 时间增量。
fn current_system_time(
    processor_count: usize,
    processor_info: &mut Vec<ProcessorPerformance>,
) -> Result<u64, u32> {
    query_processor_performance(processor_count.max(1), processor_info)
        .map_err(ntstatus_to_win32)?;
    let (_, kernel, user) =
        checked_summed_processor_times(processor_info).ok_or(ERROR_INVALID_DATA)?;
    kernel.checked_add(user).ok_or(ERROR_INVALID_DATA)
}

fn ntstatus_to_win32(status: i32) -> u32 {
    let error = unsafe { RtlNtStatusToDosError(status) };
    if error == 0 { ERROR_GEN_FAILURE } else { error }
}

pub(super) fn system_time_delta(current: u64, previous: Option<u64>) -> Option<u64> {
    previous.and_then(|previous| current.checked_sub(previous))
}

// 根据进程 CPU 时间增量与系统总时间增量计算 CPU 使用率百分比。
pub(super) fn cpu_percent_from_delta(delta_100ns: u64, total_delta_100ns: u64) -> u8 {
    if total_delta_100ns == 0 {
        return 0;
    }
    let rounded = (u128::from(delta_100ns) * 100 + u128::from(total_delta_100ns) / 2)
        / u128::from(total_delta_100ns);
    rounded.min(100) as u8
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::mem::size_of;

    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_INVALID_DATA, ERROR_INVALID_PARAMETER,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, HIGH_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS,
        OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };

    use super::{
        AccountNameCache, COMMAND_LINE_INITIAL_BYTES, COMMAND_LINE_MAX_BYTES,
        MAX_COMMAND_LINE_UNITS, ParsedProcess, ProcessDetailQuery, SystemProcessInformation,
        UnicodeString, build_process_entries, classify_gui_count, next_command_line_capacity,
        parse_process_command_line, parse_system_process_information, query_process_account_name,
        query_process_details, query_system_process_information, wts_live_identity_allows_merge,
    };
    use crate::config::options::ColumnId;
    use crate::infrastructure::native::OwnedHandle;
    use crate::system::process_identity::{ProcIdentity, query_process_identity_for_pid};
    use crate::ui::localization::{TextKey, text};

    #[test]
    fn current_process_primary_token_resolves_to_a_non_empty_account_label() {
        let mut cache = AccountNameCache::default();
        let account_name = unsafe { query_process_account_name(GetCurrentProcess(), &mut cache) }
            .expect("the current process primary token should expose TokenUser");

        assert!(!account_name.is_empty());
    }

    #[test]
    fn wts_account_requires_the_same_live_process() {
        let expected = ProcIdentity::new(77, 1_000);
        let reused = ProcIdentity::new(77, 2_000);

        assert!(wts_live_identity_allows_merge(expected, Some(Ok(expected))));
        assert!(!wts_live_identity_allows_merge(expected, Some(Ok(reused))));
        assert!(!wts_live_identity_allows_merge(
            expected,
            Some(Err(ERROR_INVALID_PARAMETER))
        ));
        assert!(wts_live_identity_allows_merge(
            expected,
            Some(Err(ERROR_ACCESS_DENIED))
        ));
        assert!(wts_live_identity_allows_merge(expected, None));
        assert!(!wts_live_identity_allows_merge(
            ProcIdentity::pid_only(77),
            Some(Ok(ProcIdentity::pid_only(77)))
        ));
        assert!(!wts_live_identity_allows_merge(
            ProcIdentity::pid_only(77),
            None
        ));
    }

    #[test]
    fn idle_and_inaccessible_rows_get_cpu_from_system_process_counters() {
        let first_rows = [
            sample_row(0, 0, Some(5_000), "", 0),
            sample_row(4, 100, Some(1_000), "System", 8),
            sample_row(99, 200, Some(2_000), "secure.exe", 13),
        ];
        let first = build_process_entries(&first_rows, &HashMap::new(), Some(1_000));
        assert!(first.row_error.is_none());
        assert!(first.entries.iter().all(|entry| entry.cpu == 0));
        assert!(!first.entries[0].identity.is_verified());
        assert_eq!(first.entries[0].image_name, "System Idle Process");
        assert_eq!(first.entries[0].user_name, "SYSTEM");
        assert!(first.entries[2].identity.is_verified());
        assert_eq!(first.entries[1].priority_class, NORMAL_PRIORITY_CLASS);
        assert_eq!(first.entries[2].priority_class, HIGH_PRIORITY_CLASS);
        assert_eq!(first.entries[0].priority_class, 0);
        assert!(first.next_samples.contains_key(&ProcIdentity::new(0, 0)));

        let second_rows = [
            sample_row(0, 0, Some(5_700), "", 0),
            sample_row(4, 100, Some(1_100), "System", 8),
            sample_row(99, 200, Some(2_200), "secure.exe", 13),
        ];
        let second = build_process_entries(&second_rows, &first.next_samples, Some(1_000));
        assert_eq!(cpu_of(&second, 0), 70);
        assert_eq!(cpu_of(&second, 4), 10);
        assert_eq!(cpu_of(&second, 99), 20);
        assert!(second.row_error.is_none());
        assert_eq!(second.entries[2].identity, ProcIdentity::new(99, 200));
    }

    #[test]
    fn corrupt_counter_keeps_sibling_rows_and_does_not_seed_a_zero_baseline() {
        let idle = sample_row(0, 0, Some(1_000), "", 0);
        let broken = sample_row(50, 10, None, "bad.exe", 8);
        let protected = sample_row(99, 20, Some(2_000), "secure.exe", 8);
        let first = build_process_entries(
            &[idle.clone(), broken.clone(), protected.clone()],
            &HashMap::new(),
            Some(100),
        );
        assert_eq!(first.entries.len(), 3);
        assert!(first.row_error.is_some());
        assert!(!first.next_samples.contains_key(&ProcIdentity::new(50, 10)));
        assert!(first.next_samples.contains_key(&ProcIdentity::new(0, 0)));
        assert_eq!(first.entries[1].cpu, 0);
        assert_eq!(first.entries[1].cpu_time_100ns, 0);

        let mut repaired = broken;
        repaired.cpu_time_100ns = Some(10_000);
        let idle_later = sample_row(0, 0, Some(1_080), "", 0);
        let protected_later = sample_row(99, 20, Some(2_030), "secure.exe", 8);
        let second = build_process_entries(
            &[idle_later, repaired, protected_later],
            &first.next_samples,
            Some(100),
        );
        assert_eq!(second.entries.len(), 3);
        assert_eq!(cpu_of(&second, 0), 80);
        assert_eq!(cpu_of(&second, 50), 0);
        assert_eq!(cpu_of(&second, 99), 30);
    }

    #[test]
    fn reused_pid_does_not_inherit_the_previous_cpu_baseline() {
        let first = build_process_entries(
            &[sample_row(77, 999, Some(500), "app.exe", 8)],
            &HashMap::new(),
            Some(1_000),
        );
        let second = build_process_entries(
            &[sample_row(77, 1_000, Some(500_000), "app.exe", 8)],
            &first.next_samples,
            Some(100),
        );
        assert_eq!(second.entries[0].identity, ProcIdentity::new(77, 1_000));
        assert_eq!(second.entries[0].cpu, 0);
        assert!(second.row_error.is_none());
    }

    #[test]
    fn snapshot_parent_links_reject_a_reused_pid_and_a_missing_parent() {
        let rows = [
            sample_row(10, 200, Some(1), "new-parent.exe", 8),
            sample_row(20, 100, Some(1), "old-child.exe", 8),
            sample_row(30, 250, Some(1), "new-child.exe", 8),
            sample_row(40, 260, Some(1), "orphan.exe", 8),
        ];
        let mut rows = rows;
        rows[0].parent_pid = None;
        rows[1].parent_pid = Some(10);
        rows[2].parent_pid = Some(10);
        rows[3].parent_pid = Some(99);
        let built = build_process_entries(&rows, &HashMap::new(), None);
        let parent_of = |pid: u32| {
            built
                .entries
                .iter()
                .find(|entry| entry.pid == pid)
                .expect("row")
                .parent
        };
        assert_eq!(parent_of(20), None);
        assert_eq!(parent_of(30), Some(ProcIdentity::new(10, 200)));
        assert_eq!(parent_of(40), None);
        assert_eq!(built.entries.len(), 4);

        let buffer = system_process_buffer(&[
            BufferSpec {
                pid: 4,
                creation_time: 50,
                name: Some("System".to_string()),
                ..BufferSpec::default()
            },
            BufferSpec {
                pid: 100,
                creation_time: 80,
                parent_pid: 4,
                name: Some("smss.exe".to_string()),
                ..BufferSpec::default()
            },
        ]);
        let (parsed, _) =
            parse_system_process_information(&buffer).expect("parent pid should round-trip");
        assert_eq!(parsed[1].parent_pid, Some(4));
        let built = build_process_entries(&parsed, &HashMap::new(), None);
        assert_eq!(built.entries[1].parent, Some(ProcIdentity::new(4, 50)));
    }

    #[test]
    fn system_process_buffer_reports_idle_counters_and_keeps_a_bad_sibling() {
        let buffer = system_process_buffer(&[
            BufferSpec {
                pid: 0,
                creation_time: 0,
                kernel_time: 50,
                user_time: 0,
                name: Some(String::new()),
                threads: 8,
                ..BufferSpec::default()
            },
            BufferSpec {
                pid: 44,
                creation_time: 123_456,
                kernel_time: 10,
                user_time: 5,
                name: Some("app.exe".to_string()),
                session_id: 2,
                threads: 3,
                handles: 11,
                base_priority: 8,
                working_set: 8_192,
                page_faults: 9,
                commit: 2_048,
                paged_pool: 1_024,
                nonpaged_pool: 3_072,
                parent_pid: 0,
                ..BufferSpec::default()
            },
            BufferSpec {
                pid: 55,
                creation_time: 99,
                kernel_time: 1,
                user_time: 1,
                name: None,
                ..BufferSpec::default()
            },
        ]);
        let (rows, row_error) =
            parse_system_process_information(&buffer).expect("synthetic buffer should parse");
        assert_eq!(rows.len(), 3);
        assert!(row_error.is_some());
        assert_eq!(rows[0].pid, 0);
        assert_eq!(rows[0].cpu_time_100ns, Some(50));
        assert!(rows[0].image_name.is_empty());
        assert_eq!(rows[1].image_name, "app.exe");
        assert_eq!(rows[1].creation_time_100ns, 123_456);
        assert_eq!(rows[1].cpu_time_100ns, Some(15));
        assert_eq!(rows[2].pid, 55);
        assert!(rows[2].image_name.is_empty());

        let built = build_process_entries(&rows, &HashMap::new(), None);
        assert_eq!(built.entries.len(), 3);
        assert_eq!(built.entries[0].image_name, "System Idle Process");
        assert_eq!(built.entries[0].cpu_time_100ns, 50);
        assert!(!built.entries[0].identity.is_verified());
        assert_eq!(built.entries[1].mem_usage_kb, 8);
        assert_eq!(built.entries[1].commit_charge_kb, 2);
        assert_eq!(built.entries[1].paged_pool_kb, 1);
        assert_eq!(built.entries[1].nonpaged_pool_kb, 3);
        assert_eq!(built.entries[1].page_faults, 9);
        assert_eq!(built.entries[1].thread_count, 3);
        assert_eq!(built.entries[1].handle_count, 11);
        assert_eq!(built.entries[1].session_id, Some(2));
        assert_eq!(built.entries[1].identity, ProcIdentity::new(44, 123_456));
        assert!(built.entries[2].identity.is_verified());
    }

    #[test]
    fn negative_io_counter_stays_unknown_and_keeps_the_sibling_row() {
        let buffer = system_process_buffer(&[
            BufferSpec {
                pid: 10,
                creation_time: 1,
                name: Some("bad.exe".to_string()),
                read_bytes: -1,
                write_bytes: 4,
                read_count: 2,
                write_count: 3,
                ..BufferSpec::default()
            },
            BufferSpec {
                pid: 11,
                creation_time: 2,
                name: Some("ok.exe".to_string()),
                read_bytes: 8,
                write_bytes: 9,
                read_count: 1,
                write_count: 1,
                ..BufferSpec::default()
            },
        ]);
        let (rows, row_error) =
            parse_system_process_information(&buffer).expect("synthetic buffer should parse");
        assert!(row_error.is_some());
        assert!(rows[0].io_read_bytes.is_none());
        assert_eq!(rows[0].io_write_bytes, Some(4));
        assert_eq!(rows[0].io_read_count, Some(2));
        assert_eq!(rows[1].io_read_bytes, Some(8));
        assert_ne!(rows[0].io_read_bytes, Some(0));

        let built = build_process_entries(&rows, &HashMap::new(), None);
        assert_eq!(built.entries.len(), 2);
        assert!(built.row_error.is_some());
        assert!(built.entries[0].io_read_bytes.is_none());
        assert_eq!(built.entries[0].io_write_bytes, Some(4));
        assert_eq!(built.entries[1].io_read_bytes, Some(8));
        assert_eq!(built.entries[1].image_name, "ok.exe");
    }

    #[test]
    fn command_line_bytes_outside_the_returned_buffer_are_not_an_empty_success() {
        assert_eq!(
            parse_process_command_line(&[]).err(),
            Some(ERROR_INVALID_DATA)
        );
        assert_eq!(
            parse_process_command_line(&command_line_buffer(0x1000, "nope")).err(),
            Some(ERROR_INVALID_DATA)
        );
        assert_eq!(
            parse_process_command_line(&command_line_buffer_inside("taskmgr.exe")).unwrap(),
            "taskmgr.exe"
        );
        assert_eq!(
            parse_process_command_line(&command_line_buffer_inside("")).unwrap(),
            ""
        );
    }

    #[test]
    fn command_line_accepts_the_unicode_string_length_limit() {
        assert_eq!(
            COMMAND_LINE_MAX_BYTES,
            size_of::<UnicodeString>() + MAX_COMMAND_LINE_UNITS * 2
        );
        const {
            assert!(COMMAND_LINE_MAX_BYTES > 64 * 1024);
        }

        let full = next_command_line_capacity(64 * 1024, 0).expect("cap is reachable");
        assert_eq!(full, COMMAND_LINE_MAX_BYTES);
        assert!(next_command_line_capacity(COMMAND_LINE_MAX_BYTES, 0).is_none());
        assert!(
            next_command_line_capacity(COMMAND_LINE_INITIAL_BYTES, COMMAND_LINE_MAX_BYTES + 1)
                .is_none()
        );

        let mut built = build_process_entries(
            &[sample_row(40, 10, Some(10), "long.exe", 8)],
            &HashMap::new(),
            None,
        );
        let unknown = text(TextKey::NotAvailable);
        for units in [16_385_usize, MAX_COMMAND_LINE_UNITS] {
            let invocation = "A".repeat(units);
            let parsed =
                parse_process_command_line(&command_line_buffer_inside(&invocation)).unwrap();
            assert_eq!(parsed.chars().count(), units);
            built.entries[0].command_line = Some(parsed);
            built.entries[0].rebuild_display_columns(&[ColumnId::CommandLine]);
            let shown = &built.entries[0].display_text[ColumnId::CommandLine as usize];
            assert_eq!(shown.chars().count(), units);
            assert_ne!(shown, unknown);
        }
    }

    #[test]
    fn gui_object_zero_is_success_only_when_the_call_reports_no_error() {
        assert_eq!(classify_gui_count(0, 0), Ok(0));
        assert_eq!(classify_gui_count(0, 5), Err(5));
        assert_eq!(classify_gui_count(4, 5), Ok(4));
    }

    #[test]
    fn departed_process_keeps_sibling_rows_and_records_row_error() {
        // SAFETY: GetCurrentProcessId has no preconditions.
        let self_pid = unsafe { GetCurrentProcessId() };
        let self_identity = query_process_identity_for_pid(self_pid).expect("current identity");
        let mut collected = build_process_entries(
            &[
                sample_row(u32::MAX, 1, Some(10), "gone.exe", 8),
                sample_row(
                    self_identity.pid,
                    self_identity.creation_time_100ns,
                    Some(10),
                    "self.exe",
                    8,
                ),
            ],
            &HashMap::new(),
            None,
        );
        let mut row_error = None;
        query_process_details(
            &mut collected.entries,
            ProcessDetailQuery {
                command_line: true,
                gui_objects: true,
            },
            &mut row_error,
        );
        assert_eq!(collected.entries.len(), 2);
        assert!(row_error.is_some());
        assert!(collected.entries[0].command_line.is_none());
        assert!(collected.entries[0].gdi_objects.is_none());
        assert!(collected.entries[0].user_objects.is_none());
        assert_eq!(collected.entries[0].io_read_bytes, Some(0));
        assert!(
            collected.entries[1]
                .command_line
                .as_ref()
                .is_some_and(|line| !line.is_empty())
        );
        assert!(collected.entries[1].gdi_objects.is_some());
        assert!(collected.entries[1].user_objects.is_some());
        assert_eq!(collected.entries[1].identity, self_identity);
    }

    fn command_line_buffer_inside(text: &str) -> Vec<u8> {
        let units: Vec<u16> = text.encode_utf16().collect();
        let mut buffer = vec![0u8; size_of::<UnicodeString>() + units.len() * 2];
        let string_offset = size_of::<UnicodeString>();
        for (index, unit) in units.iter().enumerate() {
            let offset = string_offset + index * 2;
            buffer[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
        }
        let header = UnicodeString {
            length: u16::try_from(units.len() * 2).unwrap(),
            maximum_length: u16::try_from(units.len() * 2).unwrap(),
            buffer: unsafe { buffer.as_ptr().add(string_offset).cast() },
        };
        // SAFETY: the header occupies the reserved prefix of `buffer` and does not alias a live
        // reference to that range.
        unsafe {
            buffer
                .as_mut_ptr()
                .cast::<UnicodeString>()
                .write_unaligned(header);
        }
        buffer
    }

    fn command_line_buffer(pointer: usize, text: &str) -> Vec<u8> {
        let mut buffer = command_line_buffer_inside(text);
        // SAFETY: the prefix of `buffer` is a UnicodeString written above. This only replaces the
        // pointer field with an address that is outside the allocation.
        unsafe {
            let mut header = buffer.as_ptr().cast::<UnicodeString>().read_unaligned();
            header.buffer = pointer as *const u16;
            buffer
                .as_mut_ptr()
                .cast::<UnicodeString>()
                .write_unaligned(header);
        }
        buffer
    }

    #[test]
    fn live_system_process_information_includes_idle_without_a_process_handle() {
        let mut buffer = Vec::new();
        let returned = query_system_process_information(&mut buffer).expect("system process query");
        let (rows, _) = parse_system_process_information(&buffer[..returned]).expect("parse");

        let idle = rows.iter().find(|row| row.pid == 0).expect("idle process");
        assert!(idle.cpu_time_100ns.is_some_and(|time| time > 0));
        // SAFETY: OpenProcess returns null or one owned process handle. Pid 0 has no process
        // object that this access can open; a non-null result would be closed by OwnedHandle.
        let idle_handle =
            unsafe { OwnedHandle::from_raw(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, 0)) };
        assert!(idle_handle.is_none());

        // SAFETY: GetCurrentProcessId has no preconditions.
        let self_pid = unsafe { GetCurrentProcessId() };
        let self_row = rows
            .iter()
            .find(|row| row.pid == self_pid)
            .expect("current process");
        let self_identity =
            query_process_identity_for_pid(self_pid).expect("current process identity");
        assert_eq!(
            self_row.creation_time_100ns,
            self_identity.creation_time_100ns
        );
        assert!(self_row.cpu_time_100ns.is_some());

        for row in &rows {
            if row.pid == 0 {
                continue;
            }
            // SAFETY: each successful OpenProcess returns one owned handle released by OwnedHandle.
            let full = unsafe {
                OwnedHandle::from_raw(OpenProcess(
                    PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
                    0,
                    row.pid,
                ))
            };
            let limited = unsafe {
                OwnedHandle::from_raw(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, row.pid))
            };
            if full.is_none() && limited.is_none() {
                assert_ne!(
                    row.creation_time_100ns, 0,
                    "pid {} refused OpenProcess but still needs a creation time",
                    row.pid
                );
                assert!(
                    row.cpu_time_100ns.is_some(),
                    "pid {} refused OpenProcess but still needs CPU counters",
                    row.pid
                );
                break;
            }
        }
    }

    fn sample_row(
        pid: u32,
        creation_time_100ns: u64,
        cpu_time_100ns: Option<u64>,
        image_name: &str,
        base_priority: i32,
    ) -> ParsedProcess {
        ParsedProcess {
            pid,
            creation_time_100ns,
            parent_pid: None,
            cpu_time_100ns,
            image_name: image_name.to_string(),
            session_id: 1,
            thread_count: 1,
            handle_count: 1,
            base_priority,
            working_set_bytes: 0,
            page_faults: 0,
            commit_bytes: 0,
            paged_pool_bytes: 0,
            nonpaged_pool_bytes: 0,
            io_read_bytes: Some(0),
            io_write_bytes: Some(0),
            io_read_count: Some(0),
            io_write_count: Some(0),
        }
    }

    fn cpu_of(collected: &super::CollectedProcessEntries, pid: u32) -> u8 {
        collected
            .entries
            .iter()
            .find(|entry| entry.pid == pid)
            .expect("row")
            .cpu
    }

    #[derive(Clone)]
    struct BufferSpec {
        pid: u32,
        creation_time: i64,
        kernel_time: i64,
        user_time: i64,
        name: Option<String>,
        session_id: u32,
        threads: u32,
        handles: u32,
        base_priority: i32,
        working_set: usize,
        page_faults: u32,
        commit: usize,
        paged_pool: usize,
        nonpaged_pool: usize,
        parent_pid: u32,
        read_bytes: i64,
        write_bytes: i64,
        read_count: i64,
        write_count: i64,
    }

    impl Default for BufferSpec {
        fn default() -> Self {
            Self {
                pid: 0,
                creation_time: 0,
                kernel_time: 0,
                user_time: 0,
                name: Some(String::new()),
                session_id: 0,
                threads: 1,
                handles: 0,
                base_priority: 0,
                working_set: 0,
                page_faults: 0,
                commit: 0,
                paged_pool: 0,
                nonpaged_pool: 0,
                parent_pid: 0,
                read_bytes: 0,
                write_bytes: 0,
                read_count: 0,
                write_count: 0,
            }
        }
    }

    fn system_process_buffer(specs: &[BufferSpec]) -> Vec<u8> {
        struct Placed {
            entry_offset: usize,
            name_offset: Option<usize>,
            name_bytes: u16,
        }
        let mut buffer = Vec::new();
        let mut placed = Vec::new();
        let header_size = size_of::<SystemProcessInformation>();
        for spec in specs {
            while !buffer.len().is_multiple_of(8) {
                buffer.push(0);
            }
            let entry_offset = buffer.len();
            buffer.resize(entry_offset + header_size, 0);
            let (name_offset, name_bytes) = if let Some(name) = &spec.name {
                let units: Vec<u16> = name.encode_utf16().collect();
                let name_offset = buffer.len();
                for unit in units {
                    buffer.extend_from_slice(&unit.to_le_bytes());
                }
                let name_bytes = u16::try_from(buffer.len() - name_offset).unwrap();
                (Some(name_offset), name_bytes)
            } else {
                (None, 4)
            };
            let header = SystemProcessInformation {
                number_of_threads: spec.threads,
                create_time: spec.creation_time,
                user_time: spec.user_time,
                kernel_time: spec.kernel_time,
                image_name: UnicodeString {
                    length: name_bytes,
                    maximum_length: name_bytes,
                    buffer: std::ptr::null(),
                },
                base_priority: spec.base_priority,
                unique_process_id: usize::try_from(spec.pid).unwrap(),
                inherited_from_unique_process_id: usize::try_from(spec.parent_pid).unwrap(),
                handle_count: spec.handles,
                session_id: spec.session_id,
                page_fault_count: spec.page_faults,
                working_set_size: spec.working_set,
                quota_paged_pool_usage: spec.paged_pool,
                quota_non_paged_pool_usage: spec.nonpaged_pool,
                pagefile_usage: spec.commit,
                read_operation_count: spec.read_count,
                write_operation_count: spec.write_count,
                read_transfer_count: spec.read_bytes,
                write_transfer_count: spec.write_bytes,
                ..SystemProcessInformation::default()
            };
            // SAFETY: `entry_offset` addresses `header_size` bytes reserved above, and this write
            // does not alias another live reference to that range.
            unsafe {
                buffer
                    .as_mut_ptr()
                    .add(entry_offset)
                    .cast::<SystemProcessInformation>()
                    .write_unaligned(header);
            }
            placed.push(Placed {
                entry_offset,
                name_offset,
                name_bytes,
            });
        }
        for (index, entry) in placed.iter().enumerate() {
            // SAFETY: every `entry_offset` still addresses a header inside this buffer. Name
            // pointers are assigned only after the Vec has finished growing.
            unsafe {
                let header_ptr = buffer
                    .as_mut_ptr()
                    .add(entry.entry_offset)
                    .cast::<SystemProcessInformation>();
                let mut header = header_ptr.read_unaligned();
                header.next_entry_offset = if index + 1 == placed.len() {
                    0
                } else {
                    u32::try_from(placed[index + 1].entry_offset - entry.entry_offset).unwrap()
                };
                if let Some(name_offset) = entry.name_offset {
                    if entry.name_bytes > 0 {
                        header.image_name.buffer = buffer.as_ptr().add(name_offset).cast();
                    }
                    header.image_name.length = entry.name_bytes;
                    header.image_name.maximum_length = entry.name_bytes;
                }
                header_ptr.write_unaligned(header);
            }
        }
        buffer
    }
}

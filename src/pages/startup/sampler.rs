// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 启动项枚举与启用状态
//
//   文件:       src/pages/startup/sampler.rs
//
//   日期:       2026年10月20日
//   环境:       Linux 6.12.94+ x86_64；Rust 1.99.1
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Registry Run keys, Startup folders, and `StartupApproved` toggles.
//!
//! The page worker owns every registry key and find handle. Enumeration reads the native
//! 64-bit registry view, including the explicit WOW6432Node Run key. Enable and disable rewrite
//! only the 12-byte StartupApproved value and then read it back. The Run value and the Startup
//! folder file are checked before and after that write so a missing entry is never reported as
//! a successful change.

use std::mem::zeroed;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_GEN_FAILURE, ERROR_INVALID_DATA, ERROR_INVALID_PARAMETER,
    ERROR_MORE_DATA, ERROR_NO_MORE_FILES, ERROR_PATH_NOT_FOUND, ERROR_RESOURCE_DATA_NOT_FOUND,
    ERROR_RESOURCE_TYPE_NOT_FOUND, ERROR_SUCCESS, FILETIME, GetLastError, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FIND_FIRST_EX_LARGE_FETCH, FindClose, FindExInfoBasic,
    FindExSearchNameMatch, FindFirstFileExW, FindNextFileW, GetFileAttributesW,
    GetFileVersionInfoSizeW, GetFileVersionInfoW, INVALID_FILE_ATTRIBUTES, VerQueryValueW,
    WIN32_FIND_DATAW,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_READ, KEY_SET_VALUE,
    KEY_WOW64_64KEY, REG_BINARY, REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegEnumValueW, RegOpenKeyExW, RegQueryInfoKeyW, RegQueryValueExW,
    RegSetValueExW,
};
use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;
use windows_sys::core::GUID;

use crate::infrastructure::native::{to_wide_null, widestr_ptr_to_string};

use super::model::{
    StartupActionFailure, StartupEnabledState, StartupEntryId, StartupSource, StartupToggle,
    approved_blob, classify_startup_result, executable_candidate, has_version_resource_extension,
    is_absolute_win32_path, registry_value_name_is_safe, startup_file_name_is_safe,
    state_from_approved,
};

const VALUE_ENUM_LIMIT: u32 = 4_096;
const FILE_ENUM_LIMIT: usize = 4_096;
const MAX_VALUE_BYTES: usize = 1_048_576;
const MAX_VERSION_BYTES: u32 = 16 * 1_024 * 1_024;
const KF_FLAG_DONT_VERIFY: u32 = 0x0000_4000;
const FOLDERID_STARTUP: GUID = GUID {
    data1: 0xB97D20BB,
    data2: 0xF46A,
    data3: 0x4C97,
    data4: [0xBA, 0x10, 0x5E, 0x36, 0x08, 0x43, 0x08, 0x54],
};
const FOLDERID_COMMON_STARTUP: GUID = GUID {
    data1: 0x82A5EA35,
    data2: 0xD9CD,
    data3: 0x47C5,
    data4: [0x96, 0x29, 0xE1, 0x5D, 0x2F, 0x71, 0x4E, 0x6E],
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StartupQueryFailure {
    Win32(u32),
    Hresult(i32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StartupRecord {
    pub(super) id: StartupEntryId,
    pub(super) command: String,
    pub(super) publisher: Option<String>,
    pub(super) location: String,
    pub(super) state: StartupEnabledState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StartupRowError {
    pub(super) id: StartupEntryId,
    pub(super) field: &'static str,
    pub(super) failure: StartupQueryFailure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct StartupSourceError {
    pub(super) source: StartupSource,
    pub(super) failure: StartupQueryFailure,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StartupCollection {
    pub(super) entries: Vec<StartupRecord>,
    pub(super) row_errors: Vec<StartupRowError>,
    pub(super) source_errors: Vec<StartupSourceError>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StartupActionReport {
    pub(super) id: StartupEntryId,
    pub(super) toggle: StartupToggle,
    pub(super) outcome: Result<StartupEnabledState, StartupActionFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StartupRefreshFailure {
    pub(super) sources: Vec<StartupSourceError>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum StartupWorkerCompletion {
    Snapshot(Result<StartupCollection, StartupRefreshFailure>),
    Action(StartupActionReport),
}

struct RegKey {
    key: HKEY,
}

impl RegKey {
    fn raw(&self) -> HKEY {
        self.key
    }
}

impl Drop for RegKey {
    fn drop(&mut self) {
        if !self.key.is_null() {
            // SAFETY: `key` was opened or created by this worker and has not been closed.
            // RegCloseKey is the documented release. Predefined hive roots are never stored here.
            unsafe {
                RegCloseKey(self.key);
            }
            self.key = null_mut();
        }
    }
}

struct FindHandle {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

impl Drop for FindHandle {
    fn drop(&mut self) {
        if self.handle != INVALID_HANDLE_VALUE {
            // SAFETY: the handle was returned by FindFirstFileExW and has not been closed.
            unsafe {
                FindClose(self.handle);
            }
            self.handle = INVALID_HANDLE_VALUE;
        }
    }
}

struct SourcePart {
    entries: Vec<StartupRecord>,
    row_errors: Vec<StartupRowError>,
    source_errors: Vec<StartupSourceError>,
}

impl SourcePart {
    fn empty() -> Self {
        Self {
            entries: Vec::new(),
            row_errors: Vec::new(),
            source_errors: Vec::new(),
        }
    }
}

pub(super) fn perform(request: super::model::StartupRequest) -> StartupWorkerCompletion {
    match request {
        super::model::StartupRequest::Refresh => {
            StartupWorkerCompletion::Snapshot(collect_startup_entries())
        }
        super::model::StartupRequest::SetEnabled { id, toggle } => {
            StartupWorkerCompletion::Action(set_enabled(&id, toggle))
        }
    }
}

fn collect_startup_entries() -> Result<StartupCollection, StartupRefreshFailure> {
    let mut entries = Vec::new();
    let mut row_errors = Vec::new();
    let mut source_errors = Vec::new();
    let mut any_success = false;

    for source in StartupSource::ALL {
        match collect_source(source) {
            Ok(mut part) => {
                any_success = true;
                entries.append(&mut part.entries);
                row_errors.append(&mut part.row_errors);
                source_errors.append(&mut part.source_errors);
            }
            Err(failure) => source_errors.push(StartupSourceError { source, failure }),
        }
    }

    if !any_success {
        if source_errors.is_empty() {
            source_errors.push(StartupSourceError {
                source: StartupSource::CurrentUserRun,
                failure: StartupQueryFailure::Win32(ERROR_GEN_FAILURE),
            });
        }
        return Err(StartupRefreshFailure {
            sources: source_errors,
        });
    }

    Ok(StartupCollection {
        entries,
        row_errors,
        source_errors,
    })
}

fn collect_source(source: StartupSource) -> Result<SourcePart, StartupQueryFailure> {
    if source.is_registry() {
        collect_registry(source)
    } else {
        collect_folder(source)
    }
}

fn collect_registry(source: StartupSource) -> Result<SourcePart, StartupQueryFailure> {
    let Some(run_path) = source.run_key() else {
        return Err(StartupQueryFailure::Win32(ERROR_INVALID_PARAMETER));
    };
    let root = registry_root(source);
    let run_key = match open_key(root, run_path, KEY_READ | KEY_WOW64_64KEY) {
        Ok(key) => key,
        Err(code) if is_absent(code) => return Ok(SourcePart::empty()),
        Err(code) => return Err(StartupQueryFailure::Win32(code)),
    };
    let values = enum_registry_values(run_key.raw()).map_err(StartupQueryFailure::Win32)?;
    let approved = match open_key(root, source.approved_key(), KEY_READ | KEY_WOW64_64KEY) {
        Ok(key) => ApprovedRead::open(key),
        Err(code) if is_absent(code) => ApprovedRead::missing(),
        Err(code) => {
            let mut part = entries_from_registry(source, &values, &ApprovedRead::unreadable());
            part.source_errors.push(StartupSourceError {
                source,
                failure: StartupQueryFailure::Win32(code),
            });
            return Ok(part);
        }
    };
    Ok(entries_from_registry(source, &values, &approved))
}

fn entries_from_registry(
    source: StartupSource,
    values: &[RegistryValue],
    approved: &ApprovedRead,
) -> SourcePart {
    let location = source.location_label().unwrap_or("").to_string();
    let mut part = SourcePart::empty();
    for value in values {
        if !registry_value_name_is_safe(&value.name) {
            part.row_errors.push(StartupRowError {
                id: StartupEntryId {
                    source,
                    name: String::new(),
                },
                field: "name",
                failure: StartupQueryFailure::Win32(ERROR_INVALID_PARAMETER),
            });
            continue;
        }
        let id = StartupEntryId {
            source,
            name: value.name.clone(),
        };
        let (command, command_error) = command_from_registry_value(value);
        if let Some(failure) = command_error {
            part.row_errors.push(StartupRowError {
                id: id.clone(),
                field: "command",
                failure,
            });
        }
        let (state, state_error) = approved_state(approved, &value.name);
        if let Some(failure) = state_error {
            part.row_errors.push(StartupRowError {
                id: id.clone(),
                field: "state",
                failure,
            });
        }
        let (publisher, publisher_error) = publisher_for_command(&command);
        if let Some(failure) = publisher_error {
            part.row_errors.push(StartupRowError {
                id: id.clone(),
                field: "publisher",
                failure,
            });
        }
        part.entries.push(StartupRecord {
            id,
            command,
            publisher,
            location: location.clone(),
            state,
        });
    }
    part
}

fn collect_folder(source: StartupSource) -> Result<SourcePart, StartupQueryFailure> {
    let folder = known_folder_path(folder_guid(source))?;
    let names = list_startup_files(&folder).map_err(StartupQueryFailure::Win32)?;
    let root = registry_root(source);
    let approved = match open_key(root, source.approved_key(), KEY_READ | KEY_WOW64_64KEY) {
        Ok(key) => ApprovedRead::open(key),
        Err(code) if is_absent(code) => ApprovedRead::missing(),
        Err(code) => {
            let mut part =
                entries_from_folder(source, &folder, &names, &ApprovedRead::unreadable());
            part.source_errors.push(StartupSourceError {
                source,
                failure: StartupQueryFailure::Win32(code),
            });
            return Ok(part);
        }
    };
    Ok(entries_from_folder(source, &folder, &names, &approved))
}

fn entries_from_folder(
    source: StartupSource,
    folder: &str,
    names: &[String],
    approved: &ApprovedRead,
) -> SourcePart {
    let mut part = SourcePart::empty();
    for name in names {
        if !startup_file_name_is_safe(name) {
            part.row_errors.push(StartupRowError {
                id: StartupEntryId {
                    source,
                    name: name.clone(),
                },
                field: "name",
                failure: StartupQueryFailure::Win32(ERROR_INVALID_PARAMETER),
            });
            continue;
        }
        let id = StartupEntryId {
            source,
            name: name.clone(),
        };
        let command = join_startup_file(folder, name).unwrap_or_default();
        let (state, state_error) = approved_state(approved, name);
        if let Some(failure) = state_error {
            part.row_errors.push(StartupRowError {
                id: id.clone(),
                field: "state",
                failure,
            });
        }
        let (publisher, publisher_error) = if has_version_resource_extension(&command) {
            publisher_for_path(&command)
        } else {
            (None, None)
        };
        if let Some(failure) = publisher_error {
            part.row_errors.push(StartupRowError {
                id: id.clone(),
                field: "publisher",
                failure,
            });
        }
        part.entries.push(StartupRecord {
            id,
            command,
            publisher,
            location: folder.to_string(),
            state,
        });
    }
    part
}

fn set_enabled(id: &StartupEntryId, toggle: StartupToggle) -> StartupActionReport {
    let outcome = if id.source.is_registry() {
        set_registry_enabled(id, toggle)
    } else {
        set_folder_enabled(id, toggle)
    };
    StartupActionReport {
        id: id.clone(),
        toggle,
        outcome,
    }
}

fn set_registry_enabled(
    id: &StartupEntryId,
    toggle: StartupToggle,
) -> Result<StartupEnabledState, StartupActionFailure> {
    if !registry_value_name_is_safe(&id.name) {
        return Err(StartupActionFailure::Win32(ERROR_INVALID_PARAMETER));
    }
    if !registry_entry_exists(id)? {
        return Err(StartupActionFailure::Missing);
    }
    let observed = write_and_read_approved(id, toggle)?;
    if !registry_entry_exists(id)? {
        return Err(StartupActionFailure::Missing);
    }
    classify_startup_result(toggle, Ok(observed))
}

fn set_folder_enabled(
    id: &StartupEntryId,
    toggle: StartupToggle,
) -> Result<StartupEnabledState, StartupActionFailure> {
    if !startup_file_name_is_safe(&id.name) {
        return Err(StartupActionFailure::Win32(ERROR_INVALID_PARAMETER));
    }
    let path = startup_file_path(id)?;
    if !startup_file_exists(&path)? {
        return Err(StartupActionFailure::Missing);
    }
    let observed = write_and_read_approved(id, toggle)?;
    if !startup_file_exists(&path)? {
        return Err(StartupActionFailure::Missing);
    }
    classify_startup_result(toggle, Ok(observed))
}

fn registry_entry_exists(id: &StartupEntryId) -> Result<bool, StartupActionFailure> {
    let path = id
        .source
        .run_key()
        .ok_or(StartupActionFailure::Win32(ERROR_INVALID_PARAMETER))?;
    let key = match open_key(registry_root(id.source), path, KEY_READ | KEY_WOW64_64KEY) {
        Ok(key) => key,
        Err(code) if is_absent(code) => return Ok(false),
        Err(code) => return Err(StartupActionFailure::Win32(code)),
    };
    match query_value_bytes(key.raw(), &id.name) {
        Ok(value) => Ok(value.is_some()),
        Err(code) => Err(StartupActionFailure::Win32(code)),
    }
}

fn startup_file_path(id: &StartupEntryId) -> Result<String, StartupActionFailure> {
    let folder = known_folder_path(folder_guid(id.source)).map_err(failure_to_action)?;
    join_startup_file(&folder, &id.name).ok_or(StartupActionFailure::Win32(ERROR_INVALID_PARAMETER))
}

fn startup_file_exists(path: &str) -> Result<bool, StartupActionFailure> {
    let wide = to_wide_null(path);
    // SAFETY: `wide` is NUL-terminated and lives for this call. The attribute query does not
    // retain the path pointer.
    let attributes = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if attributes == INVALID_FILE_ATTRIBUTES {
        let code = last_error();
        return if is_absent(code) {
            Ok(false)
        } else {
            Err(StartupActionFailure::Win32(code))
        };
    }
    Ok(attributes & FILE_ATTRIBUTE_DIRECTORY == 0)
}

fn write_and_read_approved(
    id: &StartupEntryId,
    toggle: StartupToggle,
) -> Result<StartupEnabledState, StartupActionFailure> {
    let key = create_key(registry_root(id.source), id.source.approved_key())
        .map_err(StartupActionFailure::Win32)?;
    let filetime = if toggle.enables() {
        0
    } else {
        system_filetime_100ns()
    };
    let blob = approved_blob(toggle, filetime);
    set_binary_value(key.raw(), &id.name, &blob).map_err(StartupActionFailure::Win32)?;
    match query_value_bytes(key.raw(), &id.name) {
        Ok(Some(bytes)) => Ok(state_from_approved(Some(&bytes))),
        Ok(None) => Ok(StartupEnabledState::Unknown),
        Err(code) => Err(StartupActionFailure::Win32(code)),
    }
}

fn registry_root(source: StartupSource) -> HKEY {
    if source.uses_current_user_hive() {
        HKEY_CURRENT_USER
    } else {
        HKEY_LOCAL_MACHINE
    }
}

fn folder_guid(source: StartupSource) -> &'static GUID {
    match source {
        StartupSource::CommonStartupFolder => &FOLDERID_COMMON_STARTUP,
        _ => &FOLDERID_STARTUP,
    }
}

fn known_folder_path(folder_id: &GUID) -> Result<String, StartupQueryFailure> {
    let mut path_ptr: *mut u16 = null_mut();
    // SAFETY: `folder_id` is a static GUID. A null token selects the current user. On success the
    // API allocates `path_ptr` with CoTaskMemAlloc; this function copies it and frees it before
    // returning. On failure a non-null pointer is still freed.
    let hr =
        unsafe { SHGetKnownFolderPath(folder_id, KF_FLAG_DONT_VERIFY, null_mut(), &mut path_ptr) };
    if hr != 0 {
        if !path_ptr.is_null() {
            unsafe { CoTaskMemFree(path_ptr.cast()) };
        }
        return Err(StartupQueryFailure::Hresult(hr));
    }
    if path_ptr.is_null() {
        return Err(StartupQueryFailure::Win32(ERROR_INVALID_DATA));
    }
    let path = unsafe { widestr_ptr_to_string(path_ptr) };
    unsafe { CoTaskMemFree(path_ptr.cast()) };
    if path.is_empty() || path.contains('\0') {
        return Err(StartupQueryFailure::Win32(ERROR_INVALID_DATA));
    }
    Ok(path)
}

fn list_startup_files(folder: &str) -> Result<Vec<String>, u32> {
    if folder.is_empty() || folder.contains('\0') {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let pattern = if folder.ends_with(['\\', '/']) {
        format!("{folder}*")
    } else {
        format!("{folder}\\*")
    };
    if pattern.encode_utf16().count() >= 32_767 {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let pattern_wide = to_wide_null(&pattern);
    // SAFETY: WIN32_FIND_DATAW is a plain Win32 record. FindFirstFileExW initializes it before
    // any field is read, and a failed first call returns before those fields are used.
    let mut data = unsafe { zeroed::<WIN32_FIND_DATAW>() };
    // SAFETY: `pattern_wide` is NUL-terminated. `data` is writable for one WIN32_FIND_DATAW.
    // FindExInfoBasic still fills the file name and attributes used below.
    let handle = unsafe {
        FindFirstFileExW(
            pattern_wide.as_ptr(),
            FindExInfoBasic,
            (&mut data as *mut WIN32_FIND_DATAW).cast(),
            FindExSearchNameMatch,
            null(),
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = last_error();
        return if is_absent(error) {
            Ok(Vec::new())
        } else {
            Err(error)
        };
    }
    let _finder = FindHandle { handle };
    let mut names = Vec::new();
    loop {
        if let Some(name) = file_name_from_find(&data) {
            names.push(name);
        }
        if names.len() > FILE_ENUM_LIMIT {
            return Err(ERROR_MORE_DATA);
        }
        // SAFETY: `handle` is the live find handle and `data` is writable for the next record.
        let success = unsafe { FindNextFileW(handle, &mut data) };
        if success == 0 {
            let error = last_error();
            if error == ERROR_NO_MORE_FILES {
                return Ok(names);
            }
            return Err(error);
        }
    }
}

fn file_name_from_find(data: &WIN32_FIND_DATAW) -> Option<String> {
    if data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return None;
    }
    let length = data
        .cFileName
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(data.cFileName.len());
    let name = String::from_utf16_lossy(&data.cFileName[..length]);
    if name.is_empty() || name.eq_ignore_ascii_case("desktop.ini") {
        None
    } else {
        Some(name)
    }
}

fn join_startup_file(folder: &str, name: &str) -> Option<String> {
    if !startup_file_name_is_safe(name) || folder.is_empty() || folder.contains('\0') {
        return None;
    }
    let folder = folder.trim_end_matches(['\\', '/']);
    let path = format!("{folder}\\{name}");
    (path.encode_utf16().count() < 32_767).then_some(path)
}

struct RegistryValue {
    name: String,
    kind: RegistryValueKind,
    text: String,
}

enum RegistryValueKind {
    Text,
    Expandable,
    Unsupported,
}

fn enum_registry_values(key: HKEY) -> Result<Vec<RegistryValue>, u32> {
    let mut value_count = 0u32;
    let mut max_name = 0u32;
    let mut max_data = 0u32;
    // SAFETY: `key` is a live registry key. The count and size outputs are writable locals.
    // Class, subkey, and time outputs are unused.
    let status = unsafe {
        RegQueryInfoKeyW(
            key,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            &mut value_count,
            &mut max_name,
            &mut max_data,
            null_mut(),
            null_mut(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(status);
    }
    if value_count > VALUE_ENUM_LIMIT {
        return Err(ERROR_MORE_DATA);
    }
    let name_cap = usize::try_from(max_name)
        .unwrap_or(usize::MAX)
        .saturating_add(1)
        .clamp(1, 16_384);
    let data_cap = usize::try_from(max_data)
        .unwrap_or(usize::MAX)
        .saturating_add(2)
        .min(MAX_VALUE_BYTES);
    if usize::try_from(max_data).unwrap_or(usize::MAX) > MAX_VALUE_BYTES {
        return Err(ERROR_INVALID_DATA);
    }
    let mut values = Vec::with_capacity(value_count as usize);
    for index in 0..value_count {
        values.push(enum_one_value(key, index, name_cap, data_cap)?);
    }
    Ok(values)
}

fn enum_one_value(
    key: HKEY,
    index: u32,
    name_cap: usize,
    data_cap: usize,
) -> Result<RegistryValue, u32> {
    let mut name = vec![0u16; name_cap];
    let mut data = vec![0u8; data_cap];
    let mut name_len = u32::try_from(name.len()).unwrap_or(u32::MAX);
    let mut data_len = u32::try_from(data.len()).unwrap_or(u32::MAX);
    let mut kind = 0u32;
    // SAFETY: `name` and `data` are writable for the lengths passed in. On success the API writes
    // the value name without its NUL into `name_len` characters and `data_len` bytes of data.
    let status = unsafe {
        RegEnumValueW(
            key,
            index,
            name.as_mut_ptr(),
            &mut name_len,
            null_mut(),
            &mut kind,
            data.as_mut_ptr(),
            &mut data_len,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(status);
    }
    let name_chars = usize::try_from(name_len).unwrap_or(0).min(name.len());
    let name = String::from_utf16_lossy(&name[..name_chars]);
    let data_bytes = usize::try_from(data_len).unwrap_or(0).min(data.len());
    let (value_kind, text) = decode_registry_text(kind, &data[..data_bytes])?;
    Ok(RegistryValue {
        name,
        kind: value_kind,
        text,
    })
}

fn decode_registry_text(kind: u32, data: &[u8]) -> Result<(RegistryValueKind, String), u32> {
    match kind {
        REG_SZ => Ok((RegistryValueKind::Text, decode_utf16(data)?)),
        REG_EXPAND_SZ => Ok((RegistryValueKind::Expandable, decode_utf16(data)?)),
        _ => Ok((RegistryValueKind::Unsupported, String::new())),
    }
}

fn decode_utf16(data: &[u8]) -> Result<String, u32> {
    if !data.len().is_multiple_of(2) {
        return Err(ERROR_INVALID_DATA);
    }
    let mut units = Vec::with_capacity(data.len() / 2);
    for chunk in data.as_chunks::<2>().0 {
        units.push(u16::from_le_bytes(*chunk));
    }
    while units.last() == Some(&0) {
        units.pop();
    }
    String::from_utf16(&units).map_err(|_| ERROR_INVALID_DATA)
}

fn command_from_registry_value(value: &RegistryValue) -> (String, Option<StartupQueryFailure>) {
    match value.kind {
        RegistryValueKind::Text => (value.text.clone(), None),
        RegistryValueKind::Expandable => match expand_command(&value.text) {
            Ok(expanded) => (expanded, None),
            Err(code) => (value.text.clone(), Some(StartupQueryFailure::Win32(code))),
        },
        RegistryValueKind::Unsupported => (
            String::new(),
            Some(StartupQueryFailure::Win32(ERROR_INVALID_DATA)),
        ),
    }
}

fn expand_command(raw: &str) -> Result<String, u32> {
    if !raw.contains('%') {
        return Ok(raw.to_string());
    }
    let source = to_wide_null(raw);
    let mut capacity = 512u32;
    for _ in 0..4 {
        let mut buffer = vec![0u16; usize::try_from(capacity).unwrap_or(usize::MAX)];
        // SAFETY: `source` and `buffer` are NUL-sized writable UTF-16 buffers for this call.
        let written =
            unsafe { ExpandEnvironmentStringsW(source.as_ptr(), buffer.as_mut_ptr(), capacity) };
        if written == 0 {
            return Err(last_error());
        }
        if written <= capacity {
            let length = usize::try_from(written)
                .unwrap_or(1)
                .saturating_sub(1)
                .min(buffer.len());
            return Ok(String::from_utf16_lossy(&buffer[..length]));
        }
        if written > 32_768 {
            return Err(ERROR_INVALID_DATA);
        }
        capacity = written;
    }
    Err(ERROR_INVALID_DATA)
}

struct ApprovedRead {
    key: Option<RegKey>,
    unreadable: bool,
}

impl ApprovedRead {
    fn open(key: RegKey) -> Self {
        Self {
            key: Some(key),
            unreadable: false,
        }
    }

    fn missing() -> Self {
        Self {
            key: None,
            unreadable: false,
        }
    }

    fn unreadable() -> Self {
        Self {
            key: None,
            unreadable: true,
        }
    }
}

fn approved_state(
    approved: &ApprovedRead,
    name: &str,
) -> (StartupEnabledState, Option<StartupQueryFailure>) {
    if approved.unreadable {
        return (StartupEnabledState::Unknown, None);
    }
    let Some(key) = approved.key.as_ref() else {
        return (StartupEnabledState::Enabled, None);
    };
    match query_value_bytes(key.raw(), name) {
        Ok(None) => (StartupEnabledState::Enabled, None),
        Ok(Some(bytes)) => (state_from_approved(Some(&bytes)), None),
        Err(code) => (
            StartupEnabledState::Unknown,
            Some(StartupQueryFailure::Win32(code)),
        ),
    }
}

fn publisher_for_command(command: &str) -> (Option<String>, Option<StartupQueryFailure>) {
    let Some(path) = executable_candidate(command) else {
        return (None, None);
    };
    if !is_absolute_win32_path(&path) || !has_version_resource_extension(&path) {
        return (None, None);
    }
    publisher_for_path(&path)
}

fn publisher_for_path(path: &str) -> (Option<String>, Option<StartupQueryFailure>) {
    match company_name(path) {
        Ok(publisher) => (publisher, None),
        Err(code) if publisher_absence(code) => (None, None),
        Err(code) => (None, Some(StartupQueryFailure::Win32(code))),
    }
}

fn publisher_absence(code: u32) -> bool {
    matches!(
        code,
        ERROR_FILE_NOT_FOUND
            | ERROR_PATH_NOT_FOUND
            | ERROR_RESOURCE_DATA_NOT_FOUND
            | ERROR_RESOURCE_TYPE_NOT_FOUND
    )
}

fn company_name(path: &str) -> Result<Option<String>, u32> {
    if path.is_empty() || path.contains('\0') {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let path_wide = to_wide_null(path);
    // SAFETY: `path_wide` is NUL-terminated and lives for both version calls. The size query's
    // unused handle argument is null, which the API documents as ignored.
    let size = unsafe { GetFileVersionInfoSizeW(path_wide.as_ptr(), null_mut()) };
    if size == 0 {
        return Err(last_error());
    }
    if size > MAX_VERSION_BYTES {
        return Err(ERROR_INVALID_DATA);
    }
    let mut block = vec![0u8; usize::try_from(size).unwrap_or(usize::MAX)];
    // SAFETY: `block` is writable for `size` bytes. Version strings queried below point into this
    // allocation and are copied before it is dropped.
    let success =
        unsafe { GetFileVersionInfoW(path_wide.as_ptr(), 0, size, block.as_mut_ptr().cast()) };
    if success == 0 {
        return Err(last_error());
    }
    let translations = version_translations(&block);
    let mut order = Vec::new();
    if let Some(index) = translations.iter().position(|item| item.0 == 0x0409) {
        order.push(translations[index]);
    }
    order.extend(translations.into_iter().filter(|item| item.0 != 0x0409));
    if order.is_empty() {
        order.push((0x0409, 0x04B0));
    }
    for (language, codepage) in order {
        let query = format!("\\StringFileInfo\\{language:04X}{codepage:04X}\\CompanyName");
        if let Some(company) = version_string(&block, &query) {
            return Ok(Some(company));
        }
    }
    Ok(None)
}

fn version_translations(block: &[u8]) -> Vec<(u16, u16)> {
    let mut pointer = null_mut();
    let mut length = 0u32;
    let query = to_wide_null("\\VarFileInfo\\Translation");
    // SAFETY: `block` is the live version-info allocation. On success `pointer` addresses memory
    // inside that allocation for `length` bytes. The pairs are copied before the next query.
    let success = unsafe {
        VerQueryValueW(
            block.as_ptr().cast(),
            query.as_ptr(),
            &mut pointer,
            &mut length,
        )
    };
    if success == 0 || pointer.is_null() || length < 4 {
        return Vec::new();
    }
    let byte_len = usize::try_from(length).unwrap_or(0) / 4 * 4;
    if byte_len == 0 || byte_len > 256 {
        return Vec::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), byte_len) };
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| {
            (
                u16::from_le_bytes([chunk[0], chunk[1]]),
                u16::from_le_bytes([chunk[2], chunk[3]]),
            )
        })
        .collect()
}

fn version_string(block: &[u8], query: &str) -> Option<String> {
    let mut pointer = null_mut();
    let mut length = 0u32;
    let query_wide = to_wide_null(query);
    // SAFETY: same version block as `version_translations`. A successful string query points into
    // that block for `length` UTF-16 code units, which are copied immediately.
    let success = unsafe {
        VerQueryValueW(
            block.as_ptr().cast(),
            query_wide.as_ptr(),
            &mut pointer,
            &mut length,
        )
    };
    if success == 0 || pointer.is_null() || length == 0 {
        return None;
    }
    let chars = usize::try_from(length).unwrap_or(0).min(4_096);
    if chars == 0 {
        return None;
    }
    let units = unsafe { std::slice::from_raw_parts(pointer.cast::<u16>(), chars) };
    let end = units
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(units.len());
    let text = String::from_utf16_lossy(&units[..end]).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn query_value_bytes(key: HKEY, name: &str) -> Result<Option<Vec<u8>>, u32> {
    if !registry_value_name_is_safe(name) && !startup_file_name_is_safe(name) {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let name_wide = to_wide_null(name);
    let mut kind = 0u32;
    let mut size = 0u32;
    // SAFETY: `key` is live and `name_wide` is NUL-terminated. A null data pointer asks for the
    // size only.
    let status = unsafe {
        RegQueryValueExW(
            key,
            name_wide.as_ptr(),
            null_mut(),
            &mut kind,
            null_mut(),
            &mut size,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if status != ERROR_SUCCESS {
        return Err(status);
    }
    let size_usize = usize::try_from(size).unwrap_or(usize::MAX);
    if size_usize > MAX_VALUE_BYTES {
        return Err(ERROR_INVALID_DATA);
    }
    let mut buffer = vec![0u8; size_usize];
    let mut got = size;
    // SAFETY: `buffer` is writable for `got` bytes. The second call fills that prefix.
    let status = unsafe {
        RegQueryValueExW(
            key,
            name_wide.as_ptr(),
            null_mut(),
            &mut kind,
            buffer.as_mut_ptr(),
            &mut got,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if status != ERROR_SUCCESS {
        return Err(status);
    }
    let length = usize::try_from(got).unwrap_or(0).min(buffer.len());
    buffer.truncate(length);
    Ok(Some(buffer))
}

fn set_binary_value(key: HKEY, name: &str, bytes: &[u8]) -> Result<(), u32> {
    let name_wide = to_wide_null(name);
    let size = u32::try_from(bytes.len()).map_err(|_| ERROR_INVALID_PARAMETER)?;
    // SAFETY: `key` is open for KEY_SET_VALUE. `name_wide` and `bytes` live for the call. This
    // writes the StartupApproved blob and does not delete the Run value or startup file.
    let status =
        unsafe { RegSetValueExW(key, name_wide.as_ptr(), 0, REG_BINARY, bytes.as_ptr(), size) };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(status)
    }
}

fn open_key(root: HKEY, path: &str, access: u32) -> Result<RegKey, u32> {
    let path_wide = to_wide_null(path);
    let mut key = null_mut();
    // SAFETY: `root` is a predefined hive. `path_wide` is NUL-terminated. On success the API
    // returns an owned key that `RegKey` closes.
    let status = unsafe { RegOpenKeyExW(root, path_wide.as_ptr(), 0, access, &mut key) };
    if status != ERROR_SUCCESS {
        return Err(status);
    }
    if key.is_null() {
        return Err(ERROR_GEN_FAILURE);
    }
    Ok(RegKey { key })
}

fn create_key(root: HKEY, path: &str) -> Result<RegKey, u32> {
    let path_wide = to_wide_null(path);
    let mut key = null_mut();
    // SAFETY: `root` is a predefined hive and `path_wide` is NUL-terminated. The new or existing
    // key is owned by the returned `RegKey`. No security descriptor is supplied, so the key keeps
    // the parent key's default DACL.
    let status = unsafe {
        RegCreateKeyExW(
            root,
            path_wide.as_ptr(),
            0,
            null_mut(),
            REG_OPTION_NON_VOLATILE,
            KEY_QUERY_VALUE | KEY_SET_VALUE | KEY_WOW64_64KEY,
            null(),
            &mut key,
            null_mut(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(status);
    }
    if key.is_null() {
        return Err(ERROR_GEN_FAILURE);
    }
    Ok(RegKey { key })
}

fn system_filetime_100ns() -> u64 {
    // SAFETY: `time` is a writable FILETIME. The API has no further lifetime requirements.
    let mut time = unsafe { zeroed::<FILETIME>() };
    unsafe { GetSystemTimeAsFileTime(&mut time) };
    u64::from(time.dwLowDateTime) | (u64::from(time.dwHighDateTime) << 32)
}

fn failure_to_action(failure: StartupQueryFailure) -> StartupActionFailure {
    match failure {
        StartupQueryFailure::Win32(code) => StartupActionFailure::Win32(code),
        StartupQueryFailure::Hresult(code) => StartupActionFailure::Hresult(code),
    }
}

fn is_absent(code: u32) -> bool {
    code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND
}

fn last_error() -> u32 {
    // SAFETY: called immediately after the failing API that documents GetLastError.
    let code = unsafe { GetLastError() };
    if code == 0 { ERROR_GEN_FAILURE } else { code }
}

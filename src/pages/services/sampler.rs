// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 服务控制管理器采样
//
//   文件:       src/pages/services/sampler.rs
//
//   日期:       2026年10月19日
//   环境:       Linux 6.12.94+ x86_64；Rust 1.99.1
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Service Control Manager queries and start, stop, pause, and continue calls.
//!
//! The page worker owns every SCM handle. Enumeration asks only for connect and enumerate rights.
//! Each service is opened with query rights, and a control opens that one service with only the
//! right the action needs plus status query so the result can be checked.

use std::collections::HashMap;
use std::mem::{size_of, zeroed};

use windows_sys::Win32::Foundation::{
    ERROR_GEN_FAILURE, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA,
    GetLastError,
};
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, ControlService, ENUM_SERVICE_STATUS_PROCESSW, EnumServicesStatusExW,
    OpenSCManagerW, OpenServiceW, QueryServiceConfig2W, QueryServiceConfigW, QueryServiceStatusEx,
    SC_ENUM_PROCESS_INFO, SC_MANAGER_CONNECT, SC_MANAGER_ENUMERATE_SERVICE, SC_STATUS_PROCESS_INFO,
    SERVICE_CONFIG_DESCRIPTION, SERVICE_CONTROL_CONTINUE, SERVICE_CONTROL_PAUSE,
    SERVICE_CONTROL_STOP, SERVICE_DESCRIPTIONW, SERVICE_PAUSE_CONTINUE, SERVICE_QUERY_CONFIG,
    SERVICE_QUERY_STATUS, SERVICE_START, SERVICE_STATE_ALL, SERVICE_STATUS, SERVICE_STATUS_PROCESS,
    SERVICE_STOP, SERVICE_WIN32, StartServiceW,
};
use windows_sys::Win32::System::Threading::{
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

use crate::infrastructure::native::to_wide_null;
use crate::system::process_identity::{
    ProcIdentity, open_process_for_identity, query_process_identity_for_pid,
};

use super::model::{
    OwningProcess, ServiceActionFailure, ServiceControlAction, ServiceRequest, ServiceRunState,
    ServiceStartupType, classify_control_result, file_name_from_win32_path, startup_from_scm,
    state_from_scm,
};

const ENUM_ATTEMPT_LIMIT: usize = 10_000;
const CONFIG_GROW_LIMIT: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ServiceRecord {
    pub(super) service_name: String,
    pub(super) description: String,
    pub(super) status: ServiceRunState,
    pub(super) startup: ServiceStartupType,
    pub(super) process: Option<OwningProcess>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ServiceRowError {
    pub(super) service_name: String,
    pub(super) field: &'static str,
    pub(super) code: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ServiceCollection {
    pub(super) services: Vec<ServiceRecord>,
    pub(super) row_errors: Vec<ServiceRowError>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ServiceActionReport {
    pub(super) service_name: String,
    pub(super) action: ServiceControlAction,
    pub(super) outcome: Result<ServiceRunState, ServiceActionFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ServiceWorkerCompletion {
    Snapshot(Result<ServiceCollection, u32>),
    Action(ServiceActionReport),
}

struct ScHandle {
    handle: windows_sys::Win32::System::Services::SC_HANDLE,
}

impl ScHandle {
    fn new(handle: windows_sys::Win32::System::Services::SC_HANDLE) -> Result<Self, u32> {
        if handle.is_null() {
            Err(last_error())
        } else {
            Ok(Self { handle })
        }
    }

    fn as_raw(&self) -> windows_sys::Win32::System::Services::SC_HANDLE {
        self.handle
    }
}

impl Drop for ScHandle {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: the handle was returned by OpenSCManagerW or OpenServiceW and has not been
            // closed. CloseServiceHandle is the documented release for both.
            unsafe {
                CloseServiceHandle(self.handle);
            }
            self.handle = std::ptr::null_mut();
        }
    }
}

pub(super) fn perform(request: ServiceRequest) -> ServiceWorkerCompletion {
    match request {
        ServiceRequest::Refresh => ServiceWorkerCompletion::Snapshot(collect_services()),
        ServiceRequest::Control {
            service_name,
            action,
        } => ServiceWorkerCompletion::Action(control_service(&service_name, action)),
    }
}

fn collect_services() -> Result<ServiceCollection, u32> {
    // SAFETY: null machine and database select the local active database. The access mask is
    // connect plus enumerate, which is enough for EnumServicesStatusExW.
    let manager = ScHandle::new(unsafe {
        OpenSCManagerW(
            std::ptr::null(),
            std::ptr::null(),
            SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE,
        )
    })?;

    let enumerated = enumerate_win32_services(manager.as_raw())?;
    let mut services = Vec::with_capacity(enumerated.len());
    let mut row_errors = Vec::new();
    let mut processes = HashMap::<u32, Result<ProcessLookup, u32>>::new();

    for entry in enumerated {
        if entry.service_name.is_empty() {
            row_errors.push(ServiceRowError {
                service_name: String::new(),
                field: "name",
                code: ERROR_INVALID_PARAMETER,
            });
            continue;
        }

        let (startup, startup_error) =
            match query_startup_type(manager.as_raw(), &entry.service_name) {
                Ok(startup) => (startup, None),
                Err(code) => (ServiceStartupType::Unknown(0), Some(code)),
            };
        if let Some(code) = startup_error {
            row_errors.push(ServiceRowError {
                service_name: entry.service_name.clone(),
                field: "startup",
                code,
            });
        }

        let (description, description_error) =
            match query_description(manager.as_raw(), &entry.service_name) {
                Ok(description) => (description, None),
                Err(code) => (String::new(), Some(code)),
            };
        if let Some(code) = description_error {
            row_errors.push(ServiceRowError {
                service_name: entry.service_name.clone(),
                field: "description",
                code,
            });
        }

        let process = resolve_process(
            entry.process_id,
            &mut processes,
            &mut row_errors,
            &entry.service_name,
        );

        services.push(ServiceRecord {
            service_name: entry.service_name,
            description,
            status: entry.status,
            startup,
            process,
        });
    }

    Ok(ServiceCollection {
        services,
        row_errors,
    })
}

struct EnumeratedService {
    service_name: String,
    status: ServiceRunState,
    process_id: u32,
}

fn enumerate_win32_services(
    manager: windows_sys::Win32::System::Services::SC_HANDLE,
) -> Result<Vec<EnumeratedService>, u32> {
    let mut services = Vec::new();
    let mut resume = 0u32;
    let mut buffer = vec![0u64; 8 * 1024];

    for _ in 0..ENUM_ATTEMPT_LIMIT {
        let mut bytes_needed = 0u32;
        let mut returned = 0u32;
        let buffer_bytes =
            u32::try_from(buffer.len().saturating_mul(size_of::<u64>())).unwrap_or(u32::MAX);
        // SAFETY: `manager` is a live SCM handle. `buffer` is u64-aligned and writable for
        // `buffer_bytes`. The API writes `returned` records at the front and points their wide
        // strings into this same buffer until the next call.
        let success = unsafe {
            EnumServicesStatusExW(
                manager,
                SC_ENUM_PROCESS_INFO,
                SERVICE_WIN32,
                SERVICE_STATE_ALL,
                buffer.as_mut_ptr().cast(),
                buffer_bytes,
                &mut bytes_needed,
                &mut returned,
                &mut resume,
                std::ptr::null(),
            )
        };
        let error = if success == 0 { last_error() } else { 0 };
        if success == 0 && error != ERROR_MORE_DATA {
            return Err(error);
        }

        let count = usize::try_from(returned).map_err(|_| ERROR_INVALID_PARAMETER)?;
        if count > 0 {
            let bytes_available = buffer.len().saturating_mul(size_of::<u64>());
            let needed_bytes = count.saturating_mul(size_of::<ENUM_SERVICE_STATUS_PROCESSW>());
            if needed_bytes > bytes_available {
                return Err(ERROR_INVALID_PARAMETER);
            }
            // SAFETY: EnumServicesStatusExW reported `count` records in this aligned buffer, and
            // the size check above keeps the slice inside that allocation. Strings are copied
            // before the buffer is reused.
            let records = unsafe {
                std::slice::from_raw_parts(
                    buffer.as_ptr().cast::<ENUM_SERVICE_STATUS_PROCESSW>(),
                    count,
                )
            };
            for record in records {
                // SAFETY: each name pointer is either null or a NUL-terminated wide string inside
                // the live enumeration buffer.
                let service_name = unsafe {
                    crate::infrastructure::native::widestr_ptr_to_string(record.lpServiceName)
                };
                services.push(EnumeratedService {
                    service_name,
                    status: state_from_scm(record.ServiceStatusProcess.dwCurrentState),
                    process_id: record.ServiceStatusProcess.dwProcessId,
                });
            }
        }

        if success != 0 {
            return Ok(services);
        }
        if count == 0 {
            let next_bytes = usize::try_from(bytes_needed).unwrap_or(usize::MAX);
            let next_words = next_bytes.div_ceil(size_of::<u64>()).saturating_add(1);
            if next_words <= buffer.len() {
                return Err(ERROR_INVALID_PARAMETER);
            }
            buffer.resize(next_words, 0);
        }
    }

    Err(ERROR_MORE_DATA)
}

fn query_startup_type(
    manager: windows_sys::Win32::System::Services::SC_HANDLE,
    service_name: &str,
) -> Result<ServiceStartupType, u32> {
    let service = open_service(manager, service_name, SERVICE_QUERY_CONFIG)?;
    let buffer = query_service_blob(|ptr, bytes, needed| {
        // SAFETY: `service` is open for SERVICE_QUERY_CONFIG. The callback buffer is writable for
        // `bytes` and lives until this call returns.
        unsafe { QueryServiceConfigW(service.as_raw(), ptr.cast(), bytes, needed) }
    })?;
    if buffer.len().saturating_mul(size_of::<u64>())
        < size_of::<windows_sys::Win32::System::Services::QUERY_SERVICE_CONFIGW>()
    {
        return Err(ERROR_INVALID_PARAMETER);
    }
    // SAFETY: a successful QueryServiceConfigW writes a QUERY_SERVICE_CONFIGW at the start of the
    // buffer. Only the start-type integer is read; embedded string pointers are not dereferenced.
    let start_type = unsafe {
        (*buffer
            .as_ptr()
            .cast::<windows_sys::Win32::System::Services::QUERY_SERVICE_CONFIGW>())
        .dwStartType
    };
    Ok(startup_from_scm(start_type))
}

fn query_description(
    manager: windows_sys::Win32::System::Services::SC_HANDLE,
    service_name: &str,
) -> Result<String, u32> {
    let service = open_service(manager, service_name, SERVICE_QUERY_CONFIG)?;
    let buffer = query_service_blob(|ptr, bytes, needed| {
        // SAFETY: same handle and buffer contract as the config query, at description level.
        unsafe {
            QueryServiceConfig2W(
                service.as_raw(),
                SERVICE_CONFIG_DESCRIPTION,
                ptr,
                bytes,
                needed,
            )
        }
    })?;
    if buffer.len().saturating_mul(size_of::<u64>()) < size_of::<SERVICE_DESCRIPTIONW>() {
        return Err(ERROR_INVALID_PARAMETER);
    }
    // SAFETY: a successful description query writes SERVICE_DESCRIPTIONW at the front of `buffer`.
    // Its string pointer, when non-null, addresses memory inside this same allocation. The string
    // is copied before `buffer` is dropped.
    let description = unsafe {
        let header = &*buffer.as_ptr().cast::<SERVICE_DESCRIPTIONW>();
        crate::infrastructure::native::widestr_ptr_to_string(header.lpDescription)
    };
    Ok(description)
}

fn query_service_blob(
    mut query: impl FnMut(*mut u8, u32, *mut u32) -> i32,
) -> Result<Vec<u64>, u32> {
    let mut words = 64usize;
    for _ in 0..CONFIG_GROW_LIMIT {
        let mut buffer = vec![0u64; words];
        let mut bytes_needed = 0u32;
        let bytes =
            u32::try_from(buffer.len().saturating_mul(size_of::<u64>())).unwrap_or(u32::MAX);
        let success = query(buffer.as_mut_ptr().cast(), bytes, &mut bytes_needed);
        if success != 0 {
            // Keep the original aligned allocation. SCM string pointers address this buffer, so a
            // byte copy would leave those pointers aimed at freed memory.
            return Ok(buffer);
        }
        let error = last_error();
        if error != ERROR_INSUFFICIENT_BUFFER {
            return Err(error);
        }
        let needed = usize::try_from(bytes_needed).unwrap_or(usize::MAX);
        let next_words = needed.div_ceil(size_of::<u64>()).saturating_add(1);
        if next_words <= words {
            return Err(ERROR_INVALID_PARAMETER);
        }
        words = next_words;
    }
    Err(ERROR_INSUFFICIENT_BUFFER)
}

struct ProcessLookup {
    process: OwningProcess,
    name_error: Option<u32>,
}

fn resolve_process(
    process_id: u32,
    cache: &mut HashMap<u32, Result<ProcessLookup, u32>>,
    row_errors: &mut Vec<ServiceRowError>,
    service_name: &str,
) -> Option<OwningProcess> {
    if process_id == 0 {
        return None;
    }
    let first_lookup = !cache.contains_key(&process_id);
    if first_lookup {
        cache.insert(process_id, query_owning_process(process_id));
    }
    match cache.get(&process_id) {
        Some(Ok(lookup)) => {
            if first_lookup && let Some(code) = lookup.name_error {
                row_errors.push(ServiceRowError {
                    service_name: service_name.to_string(),
                    field: "process-name",
                    code,
                });
            }
            Some(lookup.process.clone())
        }
        Some(Err(code)) => {
            row_errors.push(ServiceRowError {
                service_name: service_name.to_string(),
                field: "process",
                code: *code,
            });
            None
        }
        None => None,
    }
}

fn query_owning_process(process_id: u32) -> Result<ProcessLookup, u32> {
    let identity = query_process_identity_for_pid(process_id)?;
    if !identity.is_verified() {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let (image_name, name_error) = match query_process_file_name(identity) {
        Ok(name) => (Some(name), None),
        Err(code) => (None, Some(code)),
    };
    Ok(ProcessLookup {
        process: OwningProcess {
            identity,
            image_name,
        },
        name_error,
    })
}

fn query_process_file_name(identity: ProcIdentity) -> Result<String, u32> {
    let handle = open_process_for_identity(identity, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut capacity = 32_768u32;
    let mut buffer = vec![0u16; capacity as usize];
    // SAFETY: the identity-checked handle is live. `buffer` has `capacity` writable UTF-16 units
    // and the API stores the initialized length back into `capacity`.
    let success = unsafe {
        QueryFullProcessImageNameW(handle.as_raw(), 0, buffer.as_mut_ptr(), &mut capacity)
    };
    if success == 0 {
        return Err(last_error());
    }
    let length = usize::try_from(capacity).unwrap_or(0).min(buffer.len());
    let path = String::from_utf16_lossy(&buffer[..length]);
    Ok(file_name_from_win32_path(&path).to_string())
}

fn control_service(service_name: &str, action: ServiceControlAction) -> ServiceActionReport {
    let outcome = if service_name.is_empty() {
        Err(ServiceActionFailure::Api(ERROR_INVALID_PARAMETER))
    } else {
        classify_control_result(action, control_service_and_read_state(service_name, action))
    };
    ServiceActionReport {
        service_name: service_name.to_string(),
        action,
        outcome,
    }
}

fn control_service_and_read_state(
    service_name: &str,
    action: ServiceControlAction,
) -> Result<ServiceRunState, u32> {
    // SAFETY: null names select the local SCM. Connect is the only manager right required to open
    // one service; the service handle itself carries the action right.
    let manager = ScHandle::new(unsafe {
        OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT)
    })?;
    let access = match action {
        ServiceControlAction::Start => SERVICE_START | SERVICE_QUERY_STATUS,
        ServiceControlAction::Stop => SERVICE_STOP | SERVICE_QUERY_STATUS,
        ServiceControlAction::Pause | ServiceControlAction::Continue => {
            SERVICE_PAUSE_CONTINUE | SERVICE_QUERY_STATUS
        }
    };
    let service = open_service(manager.as_raw(), service_name, access)?;
    let control = match action {
        ServiceControlAction::Start => {
            // SAFETY: the service is open for SERVICE_START. No start arguments are supplied.
            let success = unsafe { StartServiceW(service.as_raw(), 0, std::ptr::null()) };
            if success == 0 {
                return Err(last_error());
            }
            None
        }
        ServiceControlAction::Stop => Some(SERVICE_CONTROL_STOP),
        ServiceControlAction::Pause => Some(SERVICE_CONTROL_PAUSE),
        ServiceControlAction::Continue => Some(SERVICE_CONTROL_CONTINUE),
    };
    if let Some(control) = control {
        let mut status = unsafe { zeroed::<SERVICE_STATUS>() };
        // SAFETY: the service is open for this one control right. `status` is a writable output
        // of the control call and is not used as proof of the final state.
        let success = unsafe { ControlService(service.as_raw(), control, &mut status) };
        if success == 0 {
            return Err(last_error());
        }
    }
    query_service_state(service.as_raw())
}

fn query_service_state(
    service: windows_sys::Win32::System::Services::SC_HANDLE,
) -> Result<ServiceRunState, u32> {
    let mut status = unsafe { zeroed::<SERVICE_STATUS_PROCESS>() };
    let mut bytes_needed = 0u32;
    // SAFETY: the service is open for SERVICE_QUERY_STATUS. The status object is writable and
    // sized for SC_STATUS_PROCESS_INFO.
    let success = unsafe {
        QueryServiceStatusEx(
            service,
            SC_STATUS_PROCESS_INFO,
            (&mut status as *mut SERVICE_STATUS_PROCESS).cast(),
            u32::try_from(size_of::<SERVICE_STATUS_PROCESS>()).unwrap_or(u32::MAX),
            &mut bytes_needed,
        )
    };
    if success == 0 {
        return Err(last_error());
    }
    Ok(state_from_scm(status.dwCurrentState))
}

fn open_service(
    manager: windows_sys::Win32::System::Services::SC_HANDLE,
    service_name: &str,
    access: u32,
) -> Result<ScHandle, u32> {
    let name = to_wide_null(service_name);
    // SAFETY: `manager` is live and `name` is NUL-terminated for this synchronous call.
    ScHandle::new(unsafe { OpenServiceW(manager, name.as_ptr(), access) })
}

fn last_error() -> u32 {
    // SAFETY: GetLastError is read immediately after the failing API.
    let error = unsafe { GetLastError() };
    if error == 0 { ERROR_GEN_FAILURE } else { error }
}

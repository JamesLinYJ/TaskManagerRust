// +-------------------------------------------------------------------------
//
//   taskmgr-rs - CPU 固件与 WMI 采集
//
//   文件:       src/pages/cpu/firmware.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 在独立 MTA worker 中初始化并复用 WMI 服务，读取固件报告的处理器身份。
//!
//! WMI 失败只影响固件来源；用户主动刷新才重建失败的 provider。

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::RPC_E_TOO_LATE;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    CoInitializeSecurity, CoSetProxyBlanket, CoUninitialize, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL,
    RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
use windows::Win32::System::Variant::{VARIANT, VT_BSTR, VT_EMPTY, VT_I4, VT_NULL, VariantClear};
use windows::Win32::System::Wmi::{
    CIM_STRING, CIM_UINT16, CIM_UINT32, IWbemClassObject, IWbemLocator, IWbemServices,
    WBEM_E_NOT_FOUND, WBEM_FLAG_CONNECT_USE_MAX_WAIT, WBEM_FLAG_FORWARD_ONLY,
    WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_S_FALSE, WBEM_S_TIMEDOUT, WbemLocator,
};
use windows::core::{BSTR, PCWSTR};
use windows_sys::Win32::Foundation::{ERROR_OPERATION_ABORTED, ERROR_TIMEOUT};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;

use super::model::{
    CpuComponentUpdate, CpuDetailError, CpuDetailRefresh, CpuDetailRequest, CpuFirmwareProcessor,
    CpuFirmwareSnapshot, CpuTopologyKey, invalid, invalid_error,
};
use crate::infrastructure::diagnostics::{self, Field, Level};
use crate::infrastructure::native::{record_hresult_error, record_startup_timing, to_wide_null};
use crate::infrastructure::worker::WorkerStopSignal;

const WMI_NEXT_WAIT_MS: i32 = 100;
const WMI_QUERY_DEADLINE: Duration = Duration::from_secs(5);

pub(crate) struct CpuFirmwareCollector {
    provider: Result<CpuWmiProvider, CpuDetailError>,
    topology_key: Option<CpuTopologyKey>,
    firmware_done: bool,
    firmware_failed: bool,
    startup_started_ms: u64,
    timing_recorded: bool,
    stop: WorkerStopSignal,
}

impl CpuFirmwareCollector {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::with_stop(WorkerStopSignal::default())
    }

    pub(crate) fn with_stop(stop: WorkerStopSignal) -> Self {
        let startup_started_ms = unsafe { GetTickCount64() };
        let provider = if stop.is_cancelled() {
            Err(cancelled_query())
        } else {
            CpuWmiProvider::connect()
        };
        record_startup_timing(
            "CPU WMI initialization",
            unsafe { GetTickCount64() }.wrapping_sub(startup_started_ms),
        );
        Self {
            provider,
            topology_key: None,
            firmware_done: false,
            firmware_failed: false,
            startup_started_ms,
            timing_recorded: false,
            stop,
        }
    }

    pub(crate) fn collect(&mut self, request: CpuDetailRequest) -> CpuFirmwareSnapshot {
        if self.stop.is_cancelled() {
            return CpuFirmwareSnapshot {
                topology_key: request.topology_key,
                firmware: CpuComponentUpdate::Failed(cancelled_query()),
            };
        }
        if self.topology_key.as_ref() != Some(&request.topology_key) {
            self.topology_key = Some(request.topology_key.clone());
            self.firmware_done = false;
            self.firmware_failed = false;
            self.timing_recorded = false;
        }
        let retry_failed = request.refresh == CpuDetailRefresh::User && self.firmware_failed;
        if retry_failed {
            self.provider = CpuWmiProvider::connect();
            self.firmware_done = false;
            self.firmware_failed = false;
        }
        let firmware = if !self.firmware_done {
            let query_started_ms = unsafe { GetTickCount64() };
            let result = self
                .provider
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|provider| provider.query_processors(&self.stop));
            if !self.timing_recorded {
                record_startup_timing(
                    "CPU WMI firmware query",
                    unsafe { GetTickCount64() }.wrapping_sub(query_started_ms),
                );
                record_startup_timing(
                    "CPU firmware",
                    unsafe { GetTickCount64() }.wrapping_sub(self.startup_started_ms),
                );
                self.timing_recorded = true;
            }
            self.firmware_done = true;
            match result {
                Ok(value) => {
                    self.firmware_failed = false;
                    CpuComponentUpdate::Success(value)
                }
                Err(error) => {
                    self.firmware_failed = true;
                    CpuComponentUpdate::Failed(error)
                }
            }
        } else {
            CpuComponentUpdate::Unchanged
        };
        CpuFirmwareSnapshot {
            topology_key: request.topology_key,
            firmware,
        }
    }
}

struct ComApartment;

impl ComApartment {
    fn initialize() -> Result<Self, CpuDetailError> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|error| CpuDetailError::HResult {
                context: "CoInitializeEx for CPU WMI worker",
                code: error.code().0,
            })?;
        match unsafe {
            CoInitializeSecurity(
                None,
                -1,
                None,
                None,
                RPC_C_AUTHN_LEVEL_CALL,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
                None,
            )
        } {
            Ok(()) => Ok(Self),
            Err(error) if error.code() == RPC_E_TOO_LATE => Ok(Self),
            Err(error) => {
                unsafe { CoUninitialize() };
                Err(CpuDetailError::HResult {
                    context: "CoInitializeSecurity for CPU WMI worker",
                    code: error.code().0,
                })
            }
        }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct CpuWmiProvider {
    services: IWbemServices,
    _apartment: ComApartment,
}

impl CpuWmiProvider {
    fn connect() -> Result<Self, CpuDetailError> {
        let apartment = ComApartment::initialize()?;
        let locator: IWbemLocator =
            unsafe { CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER) }.map_err(
                |error| CpuDetailError::HResult {
                    context: "CoCreateInstance IWbemLocator",
                    code: error.code().0,
                },
            )?;
        let empty = BSTR::new();
        let services = unsafe {
            locator.ConnectServer(
                &BSTR::from("ROOT\\CIMV2"),
                &empty,
                &empty,
                &empty,
                WBEM_FLAG_CONNECT_USE_MAX_WAIT.0,
                &empty,
                None,
            )
        }
        .map_err(|error| CpuDetailError::HResult {
            context: "IWbemLocator::ConnectServer for CPU details",
            code: error.code().0,
        })?;
        unsafe {
            CoSetProxyBlanket(
                &services,
                RPC_C_AUTHN_WINNT,
                RPC_C_AUTHZ_NONE,
                PCWSTR::null(),
                RPC_C_AUTHN_LEVEL_CALL,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
            )
        }
        .map_err(|error| CpuDetailError::HResult {
            context: "CoSetProxyBlanket for CPU WMI service",
            code: error.code().0,
        })?;
        Ok(Self {
            services,
            _apartment: apartment,
        })
    }

    fn query_processors(
        &self,
        stop: &WorkerStopSignal,
    ) -> Result<Vec<CpuFirmwareProcessor>, CpuDetailError> {
        let started = Instant::now();
        check_query_budget(stop, started.elapsed(), WMI_QUERY_DEADLINE)?;
        let query = BSTR::from(
            "SELECT DeviceID, Name, Manufacturer, SocketDesignation, ProcessorId, Family, Level, \
         Revision, Stepping, AddressWidth, DataWidth, MaxClockSpeed FROM Win32_Processor",
        );
        let enumerator = unsafe {
            self.services.ExecQuery(
                &BSTR::from("WQL"),
                &query,
                WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                None,
            )
        }
        .map_err(|error| CpuDetailError::HResult {
            context: "IWbemServices::ExecQuery Win32_Processor",
            code: error.code().0,
        })?;

        let mut processors = Vec::new();
        let mut row_index = 0u64;
        loop {
            check_query_budget(stop, started.elapsed(), WMI_QUERY_DEADLINE)?;
            let mut objects: [Option<IWbemClassObject>; 1] = [None];
            let mut returned = 0u32;
            let result = unsafe { enumerator.Next(WMI_NEXT_WAIT_MS, &mut objects, &mut returned) };
            check_query_budget(stop, started.elapsed(), WMI_QUERY_DEADLINE)?;
            let finished = match classify_next(result.0, returned)? {
                NextStep::Retry => continue,
                NextStep::Finished => break,
                NextStep::Row { finished } => finished,
            };
            let object = objects[0]
                .take()
                .ok_or_else(|| invalid_error("Win32_Processor null object"))?;
            let current_row = row_index;
            row_index = row_index
                .checked_add(1)
                .ok_or_else(|| invalid_error("Win32_Processor row index overflow"))?;
            let Some(device_id) =
                get_wmi_string(&object, "DeviceID", current_row)?.filter(|value| !value.is_empty())
            else {
                diagnostics::event(
                    Level::Warn,
                    "cpu.wmi_processor_skipped",
                    "cpu",
                    "Win32_Processor row was skipped because its required identity is unavailable",
                    &[
                        Field::text("wmi_class", "Win32_Processor"),
                        Field::text("property", "DeviceID"),
                        Field::unsigned("row_index", current_row),
                    ],
                );
                if finished {
                    break;
                }
                continue;
            };
            processors.push(CpuFirmwareProcessor {
                device_id,
                name: get_wmi_string(&object, "Name", current_row)?,
                manufacturer: get_wmi_string(&object, "Manufacturer", current_row)?,
                socket: get_wmi_string(&object, "SocketDesignation", current_row)?,
                processor_id: get_wmi_string(&object, "ProcessorId", current_row)?,
                family: get_wmi_u16(&object, "Family", current_row)?,
                level: get_wmi_u16(&object, "Level", current_row)?,
                revision: get_wmi_u16(&object, "Revision", current_row)?,
                stepping: get_wmi_string(&object, "Stepping", current_row)?,
                address_width: get_wmi_u16(&object, "AddressWidth", current_row)?,
                data_width: get_wmi_u16(&object, "DataWidth", current_row)?,
                max_clock_mhz: get_wmi_u32(&object, "MaxClockSpeed", current_row)?,
            });
            if finished {
                break;
            }
        }
        if processors.is_empty() {
            return invalid("Win32_Processor empty result");
        }
        processors.sort_by(|left, right| left.device_id.cmp(&right.device_id));
        if processors
            .windows(2)
            .any(|pair| pair[0].device_id == pair[1].device_id)
        {
            return invalid("Win32_Processor duplicate DeviceID");
        }
        Ok(processors)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum NextStep {
    Retry,
    Finished,
    Row { finished: bool },
}

fn cancelled_query() -> CpuDetailError {
    CpuDetailError::Win32 {
        context: "CPU WMI query cancelled",
        code: ERROR_OPERATION_ABORTED,
    }
}

fn check_query_budget(
    stop: &WorkerStopSignal,
    elapsed: Duration,
    budget: Duration,
) -> Result<(), CpuDetailError> {
    if stop.is_cancelled() {
        Err(cancelled_query())
    } else if elapsed >= budget {
        Err(CpuDetailError::Win32 {
            context: "CPU WMI query deadline",
            code: ERROR_TIMEOUT,
        })
    } else {
        Ok(())
    }
}

fn classify_next(status: i32, returned: u32) -> Result<NextStep, CpuDetailError> {
    if status < 0 {
        return Err(CpuDetailError::HResult {
            context: "IEnumWbemClassObject::Next Win32_Processor",
            code: status,
        });
    }
    if returned > 1 {
        return invalid("Win32_Processor enumerator returned count");
    }
    if returned == 1 {
        return Ok(NextStep::Row {
            finished: status == WBEM_S_FALSE.0,
        });
    }
    if status == WBEM_S_TIMEDOUT.0 {
        Ok(NextStep::Retry)
    } else if status == WBEM_S_FALSE.0 {
        Ok(NextStep::Finished)
    } else {
        invalid("Win32_Processor zero count without completion or timeout")
    }
}

struct OwnedVariant(VARIANT);

impl OwnedVariant {
    fn new() -> Self {
        Self(VARIANT::default())
    }

    fn as_mut_ptr(&mut self) -> *mut VARIANT {
        &mut self.0
    }

    fn value(&self) -> &windows::Win32::System::Variant::VARIANT_0_0 {
        unsafe { &self.0.Anonymous.Anonymous }
    }
}

impl Drop for OwnedVariant {
    fn drop(&mut self) {
        if let Err(error) = unsafe { VariantClear(&mut self.0) } {
            record_hresult_error("VariantClear for CPU WMI property", error.code().0);
        }
    }
}

struct WmiProperty {
    value: OwnedVariant,
    cim_type: i32,
}

fn get_wmi_property(
    object: &IWbemClassObject,
    name: &'static str,
    row_index: u64,
) -> Result<Option<WmiProperty>, CpuDetailError> {
    let wide_name = to_wide_null(name);
    let mut value = OwnedVariant::new();
    let mut cim_type = 0;
    let result = unsafe {
        object.Get(
            PCWSTR(wide_name.as_ptr()),
            0,
            value.as_mut_ptr(),
            Some(&mut cim_type),
            None,
        )
    };
    match result {
        Ok(()) => Ok(Some(WmiProperty { value, cim_type })),
        Err(error) if wmi_property_is_missing(error.code().0) => {
            record_wmi_property_unavailable(name, row_index, "not_found", &[]);
            Ok(None)
        }
        Err(error) => Err(CpuDetailError::HResultProperty {
            context: "IWbemClassObject::Get CPU property",
            code: error.code().0,
            property: name,
        }),
    }
}

fn get_wmi_string(
    object: &IWbemClassObject,
    name: &'static str,
    row_index: u64,
) -> Result<Option<String>, CpuDetailError> {
    let Some(property) = get_wmi_property(object, name, row_index)? else {
        return Ok(None);
    };
    if property.cim_type != CIM_STRING.0 {
        record_wmi_property_unavailable(
            name,
            row_index,
            "unexpected_cim_type",
            &[Field::signed("cim_type", i64::from(property.cim_type))],
        );
        return Ok(None);
    }
    let inner = property.value.value();
    match inner.vt {
        VT_EMPTY | VT_NULL => {
            record_wmi_property_unavailable(name, row_index, "empty_value", &[]);
            Ok(None)
        }
        VT_BSTR => {
            let bstr: &ManuallyDrop<BSTR> = unsafe { &inner.Anonymous.bstrVal };
            // `VT_BSTR` proves the active union member; `ManuallyDrop<T>` has the same layout as
            // `T`, while `VariantClear` remains the sole owner responsible for releasing it.
            let bstr = unsafe { &*(bstr as *const ManuallyDrop<BSTR>).cast::<BSTR>() };
            let string = match String::from_utf16(bstr) {
                Ok(value) => value,
                Err(_) => {
                    record_wmi_property_unavailable(name, row_index, "invalid_utf16", &[]);
                    return Ok(None);
                }
            };
            let trimmed = string.trim();
            Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
        }
        variant_type => {
            record_wmi_property_unavailable(
                name,
                row_index,
                "unexpected_variant_type",
                &[Field::unsigned("variant_type", u64::from(variant_type.0))],
            );
            Ok(None)
        }
    }
}

fn get_wmi_u16(
    object: &IWbemClassObject,
    name: &'static str,
    row_index: u64,
) -> Result<Option<u16>, CpuDetailError> {
    let Some(property) = get_wmi_property(object, name, row_index)? else {
        return Ok(None);
    };
    if property.cim_type != CIM_UINT16.0 {
        record_wmi_property_unavailable(
            name,
            row_index,
            "unexpected_cim_type",
            &[Field::signed("cim_type", i64::from(property.cim_type))],
        );
        return Ok(None);
    }
    let inner = property.value.value();
    match inner.vt {
        VT_EMPTY | VT_NULL => {
            record_wmi_property_unavailable(name, row_index, "empty_value", &[]);
            Ok(None)
        }
        VT_I4 => match wmi_i4_to_u16(unsafe { inner.Anonymous.lVal }) {
            Ok(value) => Ok(Some(value)),
            Err(_) => {
                record_wmi_property_unavailable(name, row_index, "integer_overflow", &[]);
                Ok(None)
            }
        },
        variant_type => {
            record_wmi_property_unavailable(
                name,
                row_index,
                "unexpected_variant_type",
                &[Field::unsigned("variant_type", u64::from(variant_type.0))],
            );
            Ok(None)
        }
    }
}

fn get_wmi_u32(
    object: &IWbemClassObject,
    name: &'static str,
    row_index: u64,
) -> Result<Option<u32>, CpuDetailError> {
    let Some(property) = get_wmi_property(object, name, row_index)? else {
        return Ok(None);
    };
    if property.cim_type != CIM_UINT32.0 {
        record_wmi_property_unavailable(
            name,
            row_index,
            "unexpected_cim_type",
            &[Field::signed("cim_type", i64::from(property.cim_type))],
        );
        return Ok(None);
    }
    let inner = property.value.value();
    match inner.vt {
        VT_EMPTY | VT_NULL => {
            record_wmi_property_unavailable(name, row_index, "empty_value", &[]);
            Ok(None)
        }
        VT_I4 => Ok(Some(wmi_i4_to_u32(unsafe { inner.Anonymous.lVal }))),
        variant_type => {
            record_wmi_property_unavailable(
                name,
                row_index,
                "unexpected_variant_type",
                &[Field::unsigned("variant_type", u64::from(variant_type.0))],
            );
            Ok(None)
        }
    }
}

#[track_caller]
fn record_wmi_property_unavailable(
    property: &'static str,
    row_index: u64,
    reason: &'static str,
    extra_fields: &[Field],
) {
    let mut fields = vec![
        Field::text("wmi_class", "Win32_Processor"),
        Field::text("property", property),
        Field::unsigned("row_index", row_index),
        Field::text("reason", reason),
    ];
    fields.extend_from_slice(extra_fields);
    diagnostics::event(
        Level::Warn,
        "cpu.wmi_property_unavailable",
        "cpu",
        "optional Win32_Processor property is unavailable",
        &fields,
    );
}

pub(super) fn wmi_property_is_missing(code: i32) -> bool {
    code == WBEM_E_NOT_FOUND.0
}

pub(super) fn wmi_i4_to_u16(value: i32) -> Result<u16, CpuDetailError> {
    u16::try_from(value).map_err(|_| invalid_error("Win32_Processor u16 property overflow"))
}

pub(super) fn wmi_i4_to_u32(value: i32) -> u32 {
    u32::from_ne_bytes(value.to_ne_bytes())
}

#[cfg(test)]
mod polling_tests {
    use super::*;

    #[test]
    fn timeouts_are_not_the_end_of_a_successful_enumeration() {
        assert_eq!(
            classify_next(WBEM_S_TIMEDOUT.0, 0).unwrap(),
            NextStep::Retry
        );
        assert_eq!(
            classify_next(0, 1).unwrap(),
            NextStep::Row { finished: false }
        );
        assert_eq!(
            classify_next(WBEM_S_FALSE.0, 1).unwrap(),
            NextStep::Row { finished: true }
        );
        assert_eq!(
            classify_next(WBEM_S_FALSE.0, 0).unwrap(),
            NextStep::Finished
        );
        assert!(classify_next(0, 0).is_err());
        assert!(classify_next(0, 2).is_err());
        assert!(matches!(classify_next(0x80004005u32 as i32, 0),
            Err(CpuDetailError::HResult { code, .. }) if code == 0x80004005u32 as i32));
    }

    #[test]
    fn cancelled_and_timed_out_queries_cannot_commit_partial_rows() {
        let stop = WorkerStopSignal::default();
        assert!(check_query_budget(&stop, Duration::ZERO, WMI_QUERY_DEADLINE).is_ok());
        assert!(matches!(
            check_query_budget(&stop, WMI_QUERY_DEADLINE, WMI_QUERY_DEADLINE),
            Err(CpuDetailError::Win32 {
                code: ERROR_TIMEOUT,
                ..
            })
        ));
        stop.cancel();
        assert!(matches!(
            check_query_budget(&stop, Duration::ZERO, WMI_QUERY_DEADLINE),
            Err(CpuDetailError::Win32 {
                code: ERROR_OPERATION_ABORTED,
                ..
            })
        ));
    }
}

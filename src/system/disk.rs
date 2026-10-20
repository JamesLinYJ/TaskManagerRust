// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 磁盘活动采样
//
//   文件:       src/system/disk.rs
//
//   日期:       2026年10月20日
//   环境:       Linux 6.12.94+，rustc 1.99.1 (x86_64-unknown-linux-gnu)
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Physical-disk activity for the performance page.
//!
//! The system sampler worker owns one handle per `\\.\PhysicalDriveN` and reads
//! `IOCTL_DISK_PERFORMANCE`. Microsoft documents that each successful call
//! increments a reference count that enables disk statistics. This collector
//! counts those successes. It keeps one enable while the handle is open, and
//! issues one `IOCTL_DISK_PERFORMANCE_OFF` for every other success before the
//! next sample. Dropping the drive — the worker stopping, or the drive
//! disappearing — issues the remaining offs before `CloseHandle`. A failed
//! IOCTL does not add a reference. Refresh never opens a process. Cumulative
//! byte and timing counters stay on the worker. A failed read leaves those
//! baselines in place. The first sample, a counter reset, and a rate that does
//! not fit in `u64` replace the baseline without publishing a point. A zero
//! query interval publishes nothing and keeps the previous baseline.

use std::mem::{offset_of, size_of, zeroed};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_GEN_FAILURE, ERROR_INVALID_DATA, ERROR_INVALID_HANDLE,
    ERROR_MEDIA_CHANGED, ERROR_NOT_READY, ERROR_PATH_NOT_FOUND, GetLastError, HANDLE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::infrastructure::native::{OwnedHandle, record_win32_error, to_wide_null};

/// `IOCTL_DISK_PERFORMANCE` = `CTL_CODE(FILE_DEVICE_DISK, 0x0008, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
const IOCTL_DISK_PERFORMANCE: u32 = 0x0007_0020;
/// `IOCTL_DISK_PERFORMANCE_OFF` = `CTL_CODE(FILE_DEVICE_DISK, 0x0018, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
const IOCTL_DISK_PERFORMANCE_OFF: u32 = 0x0007_0060;
const MAX_PHYSICAL_DRIVES: u32 = 32;
const RESCAN_INTERVAL: u32 = 30;
const HUNDRED_NS_PER_SECOND: u64 = 10_000_000;
/// `ERROR_DEV_NOT_EXIST`.
const ERROR_DEVICE_GONE: u32 = 55;
/// `ERROR_NO_MEDIA_IN_DRIVE`.
const ERROR_NO_MEDIA_IN_DRIVE: u32 = 1112;

#[repr(C)]
#[derive(Clone, Copy)]
struct DiskPerformanceRaw {
    bytes_read: i64,
    bytes_written: i64,
    read_time: i64,
    write_time: i64,
    idle_time: i64,
    read_count: u32,
    write_count: u32,
    queue_depth: u32,
    split_count: u32,
    query_time: i64,
    storage_device_number: u32,
    storage_manager_name: [u16; 8],
}

const MIN_DISK_PERFORMANCE_BYTES: u32 =
    (offset_of!(DiskPerformanceRaw, query_time) + size_of::<i64>()) as u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DiskCounters {
    pub(crate) bytes_read: u64,
    pub(crate) bytes_written: u64,
    pub(crate) read_time_100ns: u64,
    pub(crate) write_time_100ns: u64,
    pub(crate) idle_time_100ns: u64,
    pub(crate) query_time_100ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DiskRate {
    pub(crate) read_bytes_per_second: u64,
    pub(crate) write_bytes_per_second: u64,
    pub(crate) active_percent: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiskActivityUpdate {
    /// The read failed. Baselines are unchanged.
    Failed,
    /// Counters were read, but this tick has no publishable rate.
    NoRate,
    Rate(DiskRate),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiskDelta {
    Baseline,
    Reset,
    ZeroInterval,
    Overflow,
    Rate(DiskRate),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ByteRateScale {
    Bytes,
    Kilobytes,
    Megabytes,
    Gigabytes,
    Terabytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScaledByteRate {
    pub(crate) whole: u64,
    pub(crate) tenth: Option<u8>,
    pub(crate) scale: ByteRateScale,
}

pub(crate) struct DiskCollector {
    drives: Vec<OpenDrive>,
    samples_since_rescan: u32,
    last_error: Option<u32>,
}

struct OpenDrive {
    index: u32,
    handle: OwnedHandle,
    baseline: Option<DiskCounters>,
    enables: PerformanceEnables,
}

/// Outstanding `IOCTL_DISK_PERFORMANCE` enables for one drive handle.
///
/// A successful performance IOCTL increments the driver's reference count even
/// when the returned payload is later rejected. `release_until` sends one
/// `IOCTL_DISK_PERFORMANCE_OFF` per counted enable above `keep`, and stops at
/// the first disable failure so the remainder stays outstanding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PerformanceEnables {
    outstanding: u32,
}

impl PerformanceEnables {
    fn note_successful_ioctl(&mut self) {
        self.outstanding = self.outstanding.saturating_add(1);
    }

    fn release_until(&mut self, keep: u32, mut issue_off: impl FnMut() -> bool) -> u32 {
        let mut sent = 0u32;
        while self.outstanding > keep {
            if !issue_off() {
                break;
            }
            self.outstanding -= 1;
            sent = sent.saturating_add(1);
        }
        sent
    }
}

impl Drop for OpenDrive {
    fn drop(&mut self) {
        let handle = self.handle.as_raw();
        let mut logged = false;
        self.enables
            .release_until(0, || match disable_disk_performance_once(handle) {
                Ok(()) => true,
                Err(error) => {
                    if !logged {
                        record_win32_error("disk performance disable", error);
                        logged = true;
                    }
                    false
                }
            });
    }
}

impl DiskCollector {
    pub(crate) fn new() -> Self {
        Self {
            drives: Vec::new(),
            samples_since_rescan: RESCAN_INTERVAL,
            last_error: None,
        }
    }

    pub(crate) fn sample(&mut self) -> DiskActivityUpdate {
        self.refresh_drives();
        if self.drives.is_empty() {
            self.note_error(ERROR_FILE_NOT_FOUND);
            return DiskActivityUpdate::Failed;
        }

        let reads: Vec<PerformanceRead> = self
            .drives
            .iter()
            .map(|drive| read_disk_counters(drive.handle.as_raw()))
            .collect();
        for (index, read) in reads.iter().enumerate() {
            if read.enabled {
                self.retain_one_enable(index);
            }
        }

        let mut drop_drives = Vec::new();
        let mut errors = Vec::new();
        for (index, read) in reads.iter().enumerate() {
            if let Err(error) = read.counters {
                errors.push(error);
                if drive_is_gone(error) {
                    drop_drives.push(index);
                }
            }
        }
        for error in errors {
            self.note_error(error);
        }

        if reads.iter().any(|read| read.counters.is_err()) {
            for index in drop_drives.into_iter().rev() {
                self.drives.swap_remove(index);
            }
            return DiskActivityUpdate::Failed;
        }

        let mut baselines: Vec<Option<DiskCounters>> =
            self.drives.iter().map(|drive| drive.baseline).collect();
        let readings: Vec<Result<DiskCounters, ()>> = reads
            .iter()
            .map(|read| read.counters.map_err(|_| ()))
            .collect();
        let update = commit_disk_readings(&mut baselines, &readings);
        for (drive, baseline) in self.drives.iter_mut().zip(baselines) {
            drive.baseline = baseline;
        }
        if !matches!(update, DiskActivityUpdate::Failed) {
            self.last_error = None;
        }
        update
    }

    fn refresh_drives(&mut self) {
        let due = self.drives.is_empty() || self.samples_since_rescan >= RESCAN_INTERVAL;
        if !due {
            self.samples_since_rescan = self.samples_since_rescan.saturating_add(1);
            return;
        }
        self.samples_since_rescan = 0;
        for index in 0..MAX_PHYSICAL_DRIVES {
            if self.drives.iter().any(|drive| drive.index == index) {
                continue;
            }
            match open_physical_drive(index) {
                OpenAttempt::Opened(handle) => self.drives.push(OpenDrive {
                    index,
                    handle,
                    baseline: None,
                    enables: PerformanceEnables::default(),
                }),
                OpenAttempt::Absent => {}
                OpenAttempt::Failed(error) => self.note_error(error),
            }
        }
        self.drives.sort_by_key(|drive| drive.index);
    }

    fn note_error(&mut self, error: u32) {
        if self.last_error == Some(error) {
            return;
        }
        record_win32_error("disk performance counters", error);
        self.last_error = Some(error);
    }

    fn retain_one_enable(&mut self, index: usize) {
        let handle = self.drives[index].handle.as_raw();
        let mut failure = None;
        self.drives[index].enables.note_successful_ioctl();
        self.drives[index].enables.release_until(1, || {
            match disable_disk_performance_once(handle) {
                Ok(()) => true,
                Err(error) => {
                    failure = Some(error);
                    false
                }
            }
        });
        if let Some(error) = failure {
            self.note_error(error);
        }
    }
}

enum OpenAttempt {
    Opened(OwnedHandle),
    Absent,
    Failed(u32),
}

fn open_physical_drive(index: u32) -> OpenAttempt {
    let path = to_wide_null(&format!("\\\\.\\PhysicalDrive{index}"));
    // Safety: `path` is NUL-terminated and lives for the call. A successful handle is uniquely
    // owned and released with `CloseHandle`.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    // Safety: `CreateFileW` failed, so this is the call's `GetLastError`.
    let owned = unsafe { OwnedHandle::from_raw(handle) };
    if let Some(owned) = owned {
        return OpenAttempt::Opened(owned);
    }
    let error = last_error_or_gen_failure();
    if matches!(error, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
        OpenAttempt::Absent
    } else {
        OpenAttempt::Failed(error)
    }
}

struct PerformanceRead {
    /// The driver incremented its enable reference. This stays set when the
    /// payload is rejected, because the IOCTL itself succeeded.
    enabled: bool,
    counters: Result<DiskCounters, u32>,
}

fn read_disk_counters(handle: HANDLE) -> PerformanceRead {
    let mut raw = unsafe { zeroed::<DiskPerformanceRaw>() };
    let mut returned = 0u32;
    // Safety: `handle` is a live physical-drive handle owned by the collector. `raw` is writable
    // for this synchronous buffered IOCTL, and the length is the struct size.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_PERFORMANCE,
            null(),
            0,
            &mut raw as *mut DiskPerformanceRaw as *mut _,
            size_of::<DiskPerformanceRaw>() as u32,
            &mut returned,
            null_mut(),
        )
    };
    if ok == 0 {
        return PerformanceRead {
            enabled: false,
            counters: Err(last_error_or_gen_failure()),
        };
    }
    if returned < MIN_DISK_PERFORMANCE_BYTES {
        return PerformanceRead {
            enabled: true,
            counters: Err(ERROR_INVALID_DATA),
        };
    }
    PerformanceRead {
        enabled: true,
        counters: counters_from_raw(&raw).ok_or(ERROR_INVALID_DATA),
    }
}

fn disable_disk_performance_once(handle: HANDLE) -> Result<(), u32> {
    if handle.is_null() {
        return Err(ERROR_INVALID_HANDLE);
    }
    let mut returned = 0u32;
    // Safety: `handle` is still owned by `OpenDrive`. This synchronous IOCTL has no buffers.
    // The worker calls it at passive level, before `CloseHandle`.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_PERFORMANCE_OFF,
            null(),
            0,
            null_mut(),
            0,
            &mut returned,
            null_mut(),
        )
    };
    if ok == 0 {
        Err(last_error_or_gen_failure())
    } else {
        Ok(())
    }
}

fn counters_from_raw(raw: &DiskPerformanceRaw) -> Option<DiskCounters> {
    Some(DiskCounters {
        bytes_read: u64::try_from(raw.bytes_read).ok()?,
        bytes_written: u64::try_from(raw.bytes_written).ok()?,
        read_time_100ns: u64::try_from(raw.read_time).ok()?,
        write_time_100ns: u64::try_from(raw.write_time).ok()?,
        idle_time_100ns: u64::try_from(raw.idle_time).ok()?,
        query_time_100ns: u64::try_from(raw.query_time).ok()?,
    })
}

fn drive_is_gone(error: u32) -> bool {
    matches!(
        error,
        ERROR_FILE_NOT_FOUND
            | ERROR_PATH_NOT_FOUND
            | ERROR_INVALID_HANDLE
            | ERROR_NOT_READY
            | ERROR_DEVICE_GONE
            | ERROR_MEDIA_CHANGED
            | ERROR_NO_MEDIA_IN_DRIVE
    )
}

fn last_error_or_gen_failure() -> u32 {
    // Safety: the caller invokes this immediately after a failed Win32 call.
    let error = unsafe { GetLastError() };
    if error == 0 { ERROR_GEN_FAILURE } else { error }
}

pub(crate) fn commit_disk_readings(
    baselines: &mut [Option<DiskCounters>],
    readings: &[Result<DiskCounters, ()>],
) -> DiskActivityUpdate {
    if readings.len() != baselines.len() || readings.is_empty() {
        return DiskActivityUpdate::Failed;
    }

    let mut currents = Vec::with_capacity(readings.len());
    for reading in readings {
        match reading {
            Ok(counters) => currents.push(*counters),
            Err(()) => return DiskActivityUpdate::Failed,
        }
    }
    let deltas: Vec<DiskDelta> = baselines
        .iter()
        .zip(currents.iter())
        .map(|(baseline, current)| disk_delta(*baseline, *current))
        .collect();

    if deltas.iter().any(|delta| {
        matches!(
            delta,
            DiskDelta::Baseline | DiskDelta::Reset | DiskDelta::Overflow
        )
    }) {
        for (baseline, current) in baselines.iter_mut().zip(currents) {
            *baseline = Some(current);
        }
        return DiskActivityUpdate::NoRate;
    }

    let mut read_sum = 0u64;
    let mut write_sum = 0u64;
    let mut active_percent = 0u8;
    let mut rate_indexes = Vec::new();
    for (index, delta) in deltas.iter().enumerate() {
        match delta {
            DiskDelta::Rate(rate) => {
                let Some(next_read) = read_sum.checked_add(rate.read_bytes_per_second) else {
                    replace_baselines(baselines, &currents);
                    return DiskActivityUpdate::NoRate;
                };
                let Some(next_write) = write_sum.checked_add(rate.write_bytes_per_second) else {
                    replace_baselines(baselines, &currents);
                    return DiskActivityUpdate::NoRate;
                };
                read_sum = next_read;
                write_sum = next_write;
                active_percent = active_percent.max(rate.active_percent);
                rate_indexes.push(index);
            }
            DiskDelta::ZeroInterval => {}
            DiskDelta::Baseline | DiskDelta::Reset | DiskDelta::Overflow => {
                replace_baselines(baselines, &currents);
                return DiskActivityUpdate::NoRate;
            }
        }
    }

    if rate_indexes.is_empty() {
        return DiskActivityUpdate::NoRate;
    }
    for index in rate_indexes {
        baselines[index] = Some(currents[index]);
    }
    DiskActivityUpdate::Rate(DiskRate {
        read_bytes_per_second: read_sum,
        write_bytes_per_second: write_sum,
        active_percent,
    })
}

fn replace_baselines(baselines: &mut [Option<DiskCounters>], currents: &[DiskCounters]) {
    for (baseline, current) in baselines.iter_mut().zip(currents) {
        *baseline = Some(*current);
    }
}

fn disk_delta(previous: Option<DiskCounters>, current: DiskCounters) -> DiskDelta {
    let Some(previous) = previous else {
        return DiskDelta::Baseline;
    };
    if current.bytes_read < previous.bytes_read
        || current.bytes_written < previous.bytes_written
        || current.read_time_100ns < previous.read_time_100ns
        || current.write_time_100ns < previous.write_time_100ns
        || current.idle_time_100ns < previous.idle_time_100ns
        || current.query_time_100ns < previous.query_time_100ns
    {
        return DiskDelta::Reset;
    }

    let Some(elapsed) = current
        .query_time_100ns
        .checked_sub(previous.query_time_100ns)
    else {
        return DiskDelta::Overflow;
    };
    if elapsed == 0 {
        return DiskDelta::ZeroInterval;
    }

    let read_bytes = current.bytes_read - previous.bytes_read;
    let write_bytes = current.bytes_written - previous.bytes_written;
    let Some(read_bytes_per_second) = bytes_per_second(read_bytes, elapsed) else {
        return DiskDelta::Overflow;
    };
    let Some(write_bytes_per_second) = bytes_per_second(write_bytes, elapsed) else {
        return DiskDelta::Overflow;
    };
    let idle_reported = previous.idle_time_100ns != 0 || current.idle_time_100ns != 0;
    let read_time = current.read_time_100ns - previous.read_time_100ns;
    let write_time = current.write_time_100ns - previous.write_time_100ns;
    let idle_time = current.idle_time_100ns - previous.idle_time_100ns;
    if idle_reported && idle_time > elapsed {
        return DiskDelta::Reset;
    }
    let Some(active_percent) =
        active_percent(elapsed, read_time, write_time, idle_time, idle_reported)
    else {
        return DiskDelta::Overflow;
    };
    DiskDelta::Rate(DiskRate {
        read_bytes_per_second,
        write_bytes_per_second,
        active_percent,
    })
}

fn bytes_per_second(bytes: u64, elapsed_100ns: u64) -> Option<u64> {
    if elapsed_100ns == 0 {
        return None;
    }
    let scaled = u128::from(bytes).checked_mul(u128::from(HUNDRED_NS_PER_SECOND))?;
    let elapsed = u128::from(elapsed_100ns);
    let rounded = scaled.checked_add(elapsed / 2)?;
    u64::try_from(rounded / elapsed).ok()
}

fn active_percent(
    elapsed: u64,
    read_time: u64,
    write_time: u64,
    idle_time: u64,
    idle_reported: bool,
) -> Option<u8> {
    let busy = if idle_reported {
        elapsed.checked_sub(idle_time)?
    } else {
        read_time.checked_add(write_time)?.min(elapsed)
    };
    let percent = u128::from(busy)
        .checked_mul(100)?
        .checked_div(u128::from(elapsed))?;
    let percent = u8::try_from(percent.min(100)).ok()?;
    if busy > 0 && percent == 0 {
        Some(1)
    } else {
        Some(percent)
    }
}

const BYTE_RATE_SCALES: [ByteRateScale; 5] = [
    ByteRateScale::Bytes,
    ByteRateScale::Kilobytes,
    ByteRateScale::Megabytes,
    ByteRateScale::Gigabytes,
    ByteRateScale::Terabytes,
];

pub(crate) fn scale_byte_rate(bytes_per_second: u64) -> ScaledByteRate {
    const STEP: u64 = 1024;
    let mut scale_index = 0usize;
    let mut divisor = 1u64;
    while scale_index < 4 && bytes_per_second / divisor >= STEP {
        scale_index += 1;
        divisor = divisor.saturating_mul(STEP);
    }
    let mut whole = bytes_per_second / divisor;
    let mut tenth = 0u64;
    if scale_index > 0 {
        let remainder = bytes_per_second % divisor;
        tenth = remainder.saturating_mul(10).saturating_add(divisor / 2) / divisor;
        if tenth >= 10 {
            whole = whole.saturating_add(1);
            tenth = 0;
        }
        if whole >= STEP && scale_index < 4 {
            scale_index += 1;
            whole /= STEP;
            tenth = 0;
        }
    }
    ScaledByteRate {
        whole,
        tenth: (scale_index > 0 && whole < 10 && tenth > 0).then_some(tenth as u8),
        scale: BYTE_RATE_SCALES[scale_index],
    }
}

const _: () = assert!(size_of::<DiskPerformanceRaw>() == 88);
const _: () = assert!(offset_of!(DiskPerformanceRaw, query_time) == 56);

#[cfg(test)]
mod tests {
    use super::*;

    fn counters(
        bytes_read: u64,
        bytes_written: u64,
        read_time: u64,
        write_time: u64,
        idle_time: u64,
        query_time: u64,
    ) -> DiskCounters {
        DiskCounters {
            bytes_read,
            bytes_written,
            read_time_100ns: read_time,
            write_time_100ns: write_time,
            idle_time_100ns: idle_time,
            query_time_100ns: query_time,
        }
    }

    #[test]
    fn first_sample_only_establishes_a_baseline() {
        let mut baselines = [None];
        let current = counters(10, 4, 0, 0, 1, 1_000);
        let update = commit_disk_readings(&mut baselines, &[Ok(current)]);
        assert_eq!(update, DiskActivityUpdate::NoRate);
        assert_eq!(baselines, [Some(current)]);
    }

    #[test]
    fn reset_replaces_the_baseline_without_a_spike() {
        let previous = counters(1_000, 0, 0, 0, 5, 1_000);
        let mut baselines = [Some(previous)];
        let reset = counters(10, 0, 0, 0, 5, 2_000);
        assert_eq!(
            commit_disk_readings(&mut baselines, &[Ok(reset)]),
            DiskActivityUpdate::NoRate
        );
        assert_eq!(baselines, [Some(reset)]);

        let next = counters(10 + 5_000_000, 0, 0, 0, 5 + 10_000_000, 2_000 + 10_000_000);
        assert_eq!(
            commit_disk_readings(&mut baselines, &[Ok(next)]),
            DiskActivityUpdate::Rate(DiskRate {
                read_bytes_per_second: 5_000_000,
                write_bytes_per_second: 0,
                active_percent: 0,
            })
        );
    }

    #[test]
    fn zero_interval_keeps_the_baseline_and_counts_bytes_later() {
        let previous = counters(0, 0, 0, 0, 1, 1_000);
        let mut baselines = [Some(previous)];
        let frozen = counters(1_000, 250, 0, 0, 1, 1_000);
        assert_eq!(
            commit_disk_readings(&mut baselines, &[Ok(frozen)]),
            DiskActivityUpdate::NoRate
        );
        assert_eq!(baselines, [Some(previous)]);

        let next = counters(1_000, 250, 0, 0, 1 + 10_000_000, 1_000 + 10_000_000);
        assert_eq!(
            commit_disk_readings(&mut baselines, &[Ok(next)]),
            DiskActivityUpdate::Rate(DiskRate {
                read_bytes_per_second: 1_000,
                write_bytes_per_second: 250,
                active_percent: 0,
            })
        );
    }

    #[test]
    fn overflow_does_not_publish_a_rate_and_replaces_the_baseline() {
        let previous = counters(0, 0, 0, 0, 1, 0);
        let mut baselines = [Some(previous)];
        let overflow = counters(u64::MAX, 0, 0, 0, 1, 1);
        assert_eq!(
            commit_disk_readings(&mut baselines, &[Ok(overflow)]),
            DiskActivityUpdate::NoRate
        );
        assert_eq!(baselines, [Some(overflow)]);

        let next = counters(u64::MAX, 0, 0, 0, 1 + 10_000_000, 1 + 10_000_000);
        assert_eq!(
            commit_disk_readings(&mut baselines, &[Ok(next)]),
            DiskActivityUpdate::Rate(DiskRate {
                read_bytes_per_second: 0,
                write_bytes_per_second: 0,
                active_percent: 0,
            })
        );
    }

    #[test]
    fn failed_read_does_not_advance_any_baseline() {
        let previous = counters(40, 5, 0, 0, 8, 100);
        let mut baselines = [Some(previous), Some(previous)];
        let advanced = counters(90, 5, 0, 0, 8 + 10_000_000, 100 + 10_000_000);
        assert_eq!(
            commit_disk_readings(&mut baselines, &[Ok(advanced), Err(())]),
            DiskActivityUpdate::Failed
        );
        assert_eq!(baselines, [Some(previous), Some(previous)]);
    }

    #[test]
    fn mismatched_or_empty_readings_fail_without_writing_baselines() {
        let previous = counters(1, 1, 0, 0, 1, 1);
        let mut baselines = [Some(previous)];
        assert_eq!(
            commit_disk_readings(&mut baselines, &[]),
            DiskActivityUpdate::Failed
        );
        assert_eq!(
            commit_disk_readings(&mut baselines, &[Ok(previous), Ok(previous)]),
            DiskActivityUpdate::Failed
        );
        assert_eq!(baselines, [Some(previous)]);
    }

    #[test]
    fn aggregate_uses_total_bytes_and_the_busiest_disk() {
        let previous = counters(0, 0, 0, 0, 10, 0);
        let mut baselines = [Some(previous), Some(previous)];
        let quiet = counters(100, 0, 0, 0, 10 + 10_000_000, 10_000_000);
        let busy = counters(50, 25, 7_000_000, 0, 10 + 3_000_000, 10_000_000);
        assert_eq!(
            commit_disk_readings(&mut baselines, &[Ok(quiet), Ok(busy)]),
            DiskActivityUpdate::Rate(DiskRate {
                read_bytes_per_second: 150,
                write_bytes_per_second: 25,
                active_percent: 70,
            })
        );
    }

    #[test]
    fn summed_rates_that_overflow_u64_do_not_publish() {
        let previous = counters(0, 0, 0, 0, 1, 0);
        let mut baselines = [Some(previous), Some(previous)];
        let saturated = counters(u64::MAX, 0, 0, 0, 1 + 10_000_000, 10_000_000);
        assert_eq!(
            commit_disk_readings(&mut baselines, &[Ok(saturated), Ok(saturated)]),
            DiskActivityUpdate::NoRate
        );
        assert_eq!(baselines, [Some(saturated), Some(saturated)]);
    }

    #[test]
    fn unreported_idle_time_uses_read_and_write_time() {
        let previous = counters(0, 0, 0, 0, 0, 0);
        let mut baselines = [Some(previous)];
        let busy = counters(0, 0, 10_000_000, 0, 0, 10_000_000);
        assert_eq!(
            commit_disk_readings(&mut baselines, &[Ok(busy)]),
            DiskActivityUpdate::Rate(DiskRate {
                read_bytes_per_second: 0,
                write_bytes_per_second: 0,
                active_percent: 100,
            })
        );

        let idle = counters(0, 0, 10_000_000, 0, 0, 20_000_000);
        assert_eq!(
            commit_disk_readings(&mut baselines, &[Ok(idle)]),
            DiskActivityUpdate::Rate(DiskRate {
                read_bytes_per_second: 0,
                write_bytes_per_second: 0,
                active_percent: 0,
            })
        );
    }

    #[test]
    fn idle_time_past_the_interval_is_a_reset() {
        let previous = counters(0, 0, 0, 0, 5, 0);
        let mut baselines = [Some(previous)];
        let skewed = counters(10, 0, 0, 0, 5 + 20_000, 10_000);
        assert_eq!(
            commit_disk_readings(&mut baselines, &[Ok(skewed)]),
            DiskActivityUpdate::NoRate
        );
        assert_eq!(baselines, [Some(skewed)]);
    }

    #[test]
    fn successful_samples_keep_one_enable_and_shutdown_releases_it() {
        let mut enables = PerformanceEnables::default();
        let mut offs = 0u32;
        for _ in 0..4 {
            enables.note_successful_ioctl();
            offs += enables.release_until(1, || true);
        }
        assert_eq!(enables.outstanding, 1);
        assert_eq!(offs, 3);
        assert_eq!(enables.release_until(0, || true), 1);
        assert_eq!(enables.outstanding, 0);
        assert_eq!(enables.release_until(0, || true), 0);
    }

    #[test]
    fn a_failed_ioctl_does_not_need_a_disable() {
        let mut enables = PerformanceEnables::default();
        assert_eq!(
            enables.release_until(0, || panic!("no enable to release")),
            0
        );
    }

    #[test]
    fn a_rejected_payload_still_counts_as_an_enable() {
        let mut enables = PerformanceEnables::default();
        let read = PerformanceRead {
            enabled: true,
            counters: Err(ERROR_INVALID_DATA),
        };
        if read.enabled {
            enables.note_successful_ioctl();
        }
        assert!(read.counters.is_err());
        assert_eq!(enables.release_until(0, || true), 1);
    }

    #[test]
    fn a_failed_disable_leaves_the_reference_outstanding() {
        let mut enables = PerformanceEnables::default();
        enables.note_successful_ioctl();
        enables.note_successful_ioctl();
        assert_eq!(enables.release_until(1, || false), 0);
        assert_eq!(enables.outstanding, 2);
        let mut attempts = 0u32;
        let sent = enables.release_until(0, || {
            attempts += 1;
            false
        });
        assert_eq!(sent, 0);
        assert_eq!(attempts, 1);
        assert_eq!(enables.outstanding, 2);
    }

    #[test]
    fn removing_one_drive_balances_only_that_drive() {
        let mut kept = PerformanceEnables::default();
        let mut gone = PerformanceEnables::default();
        kept.note_successful_ioctl();
        gone.note_successful_ioctl();
        gone.note_successful_ioctl();
        assert_eq!(gone.release_until(1, || true), 1);
        assert_eq!(gone.outstanding, 1);
        assert_eq!(gone.release_until(0, || true), 1);
        assert_eq!(gone.outstanding, 0);
        assert_eq!(kept.outstanding, 1);
        assert_eq!(kept.release_until(0, || true), 1);
    }

    #[test]
    fn byte_rates_scale_at_1024_boundaries() {
        assert_eq!(
            scale_byte_rate(0),
            ScaledByteRate {
                whole: 0,
                tenth: None,
                scale: ByteRateScale::Bytes,
            }
        );
        assert_eq!(
            scale_byte_rate(1023),
            ScaledByteRate {
                whole: 1023,
                tenth: None,
                scale: ByteRateScale::Bytes,
            }
        );
        assert_eq!(
            scale_byte_rate(1024),
            ScaledByteRate {
                whole: 1,
                tenth: None,
                scale: ByteRateScale::Kilobytes,
            }
        );
        assert_eq!(
            scale_byte_rate(1536),
            ScaledByteRate {
                whole: 1,
                tenth: Some(5),
                scale: ByteRateScale::Kilobytes,
            }
        );
        assert_eq!(
            scale_byte_rate(1024 * 1024),
            ScaledByteRate {
                whole: 1,
                tenth: None,
                scale: ByteRateScale::Megabytes,
            }
        );
        assert_eq!(scale_byte_rate(u64::MAX).scale, ByteRateScale::Terabytes);
        assert!(scale_byte_rate(u64::MAX).whole >= 10);
    }
}

// +-------------------------------------------------------------------------
//
//   taskmgr-rs - CPU 原始计数采样
//
//   文件:       src/system/cpu_sampler.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

use std::ffi::c_void;
use std::mem::size_of;

const PROCESSOR_PERFORMANCE_INFORMATION_CLASS: i32 = 8;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004u32 as i32;
const STATUS_INVALID_BUFFER_SIZE: i32 = 0xC000_0206u32 as i32;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct ProcessorPerformance {
    pub idle_time: i64,
    pub kernel_time: i64,
    pub user_time: i64,
    pub dpc_time: i64,
    pub interrupt_time: i64,
    pub interrupt_count: u32,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySystemInformation(
        system_information_class: i32,
        system_information: *mut c_void,
        system_information_length: u32,
        return_length: *mut u32,
    ) -> i32;
}

pub(crate) fn query_processor_performance(
    expected_count: usize,
    output: &mut Vec<ProcessorPerformance>,
) -> Result<(), i32> {
    let item_size = size_of::<ProcessorPerformance>();
    let mut count = expected_count.max(1);

    loop {
        output.resize(count, ProcessorPerformance::default());
        let Some(byte_len) = count
            .checked_mul(item_size)
            .and_then(|value| u32::try_from(value).ok())
        else {
            return Err(STATUS_INVALID_BUFFER_SIZE);
        };
        let mut returned = 0u32;
        let status = unsafe {
            NtQuerySystemInformation(
                PROCESSOR_PERFORMANCE_INFORMATION_CLASS,
                output.as_mut_ptr() as *mut c_void,
                byte_len,
                &mut returned,
            )
        };

        match take_processor_performance_query(output, status, returned, byte_len, count) {
            Ok(ProcessorPerformanceQuery::Complete) => return Ok(()),
            Ok(ProcessorPerformanceQuery::GrowTo(next_count)) => count = next_count,
            Err(error) => return Err(error),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessorPerformanceQuery {
    Complete,
    GrowTo(usize),
}

/// Classifies one processor-performance query.
///
/// A success status with a zero returned length is an error. A nonzero length is
/// accepted only when it is an exact multiple of the struct and matches `count`.
fn decide_processor_performance_query(
    status: i32,
    returned: u32,
    buffer_bytes: u32,
    count: usize,
) -> Result<ProcessorPerformanceQuery, i32> {
    let item_size = size_of::<ProcessorPerformance>();
    if status >= 0 {
        if returned == 0 {
            return Err(STATUS_INVALID_BUFFER_SIZE);
        }
        let returned_len = returned as usize;
        let buffer_len = buffer_bytes as usize;
        if !returned_len.is_multiple_of(item_size) || returned_len > buffer_len {
            return Err(STATUS_INVALID_BUFFER_SIZE);
        }
        if returned_len / item_size != count {
            return Err(STATUS_INFO_LENGTH_MISMATCH);
        }
        return Ok(ProcessorPerformanceQuery::Complete);
    }

    if status != STATUS_INFO_LENGTH_MISMATCH || returned as usize <= buffer_bytes as usize {
        return Err(status);
    }
    Ok(ProcessorPerformanceQuery::GrowTo(
        (returned as usize).div_ceil(item_size),
    ))
}

fn take_processor_performance_query(
    output: &mut Vec<ProcessorPerformance>,
    status: i32,
    returned: u32,
    buffer_bytes: u32,
    count: usize,
) -> Result<ProcessorPerformanceQuery, i32> {
    match decide_processor_performance_query(status, returned, buffer_bytes, count) {
        // A successful query that wrote nothing leaves the caller's default-zero
        // rows in place. Drop them so they cannot be read as idle counters.
        Err(error) if status >= 0 && returned == 0 => {
            output.clear();
            Err(error)
        }
        decision => decision,
    }
}

pub(crate) fn checked_summed_processor_times(
    processors: &[ProcessorPerformance],
) -> Option<(u64, u64, u64)> {
    processors
        .iter()
        .try_fold((0u64, 0u64, 0u64), |sums, processor| {
            let idle = u64::try_from(processor.idle_time).ok()?;
            let kernel = u64::try_from(processor.kernel_time).ok()?;
            let user = u64::try_from(processor.user_time).ok()?;
            Some((
                sums.0.checked_add(idle)?,
                sums.1.checked_add(kernel)?,
                sums.2.checked_add(user)?,
            ))
        })
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use super::{
        ProcessorPerformance, ProcessorPerformanceQuery, STATUS_INFO_LENGTH_MISMATCH,
        STATUS_INVALID_BUFFER_SIZE, checked_summed_processor_times,
        decide_processor_performance_query, query_processor_performance,
        take_processor_performance_query,
    };
    use windows_sys::Win32::System::Threading::{ALL_PROCESSOR_GROUPS, GetActiveProcessorCount};

    #[test]
    fn processor_query_returns_every_active_logical_processor() {
        let expected = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) } as usize;
        assert!(expected > 0);
        let mut processors = Vec::new();
        query_processor_performance(expected, &mut processors).unwrap();
        assert_eq!(processors.len(), expected);
    }

    #[test]
    fn summed_processor_times_reject_negative_values() {
        let processors = [
            ProcessorPerformance {
                idle_time: 10,
                kernel_time: 30,
                user_time: 20,
                ..ProcessorPerformance::default()
            },
            ProcessorPerformance {
                idle_time: -1,
                kernel_time: i64::MAX,
                user_time: 5,
                ..ProcessorPerformance::default()
            },
        ];

        assert_eq!(checked_summed_processor_times(&processors), None);
    }

    #[test]
    fn summed_processor_times_preserve_valid_counters() {
        let processors = [
            ProcessorPerformance {
                idle_time: 10,
                kernel_time: 30,
                user_time: 20,
                ..ProcessorPerformance::default()
            },
            ProcessorPerformance {
                idle_time: 5,
                kernel_time: 40,
                user_time: 7,
                ..ProcessorPerformance::default()
            },
        ];
        assert_eq!(
            checked_summed_processor_times(&processors),
            Some((15, 70, 27))
        );
    }

    #[test]
    fn success_with_zero_returned_length_is_an_error() {
        let bytes = size_of::<ProcessorPerformance>() as u32;
        assert!(decide_processor_performance_query(0, 0, bytes, 1).is_err());
        assert!(decide_processor_performance_query(0, 0, bytes, 0).is_err());
    }

    #[test]
    fn success_with_exact_multiple_is_ok() {
        let item_size = size_of::<ProcessorPerformance>();
        let count = 3;
        let bytes = u32::try_from(item_size * count).unwrap();
        assert_eq!(
            decide_processor_performance_query(0, bytes, bytes, count),
            Ok(ProcessorPerformanceQuery::Complete)
        );
    }

    #[test]
    fn non_multiple_returned_length_is_an_error() {
        let bytes = size_of::<ProcessorPerformance>() as u32;
        assert_eq!(
            decide_processor_performance_query(0, bytes - 1, bytes, 1),
            Err(STATUS_INVALID_BUFFER_SIZE)
        );
    }

    #[test]
    fn short_buffer_keeps_invalid_buffer_size() {
        let one = size_of::<ProcessorPerformance>() as u32;
        assert_eq!(
            decide_processor_performance_query(0, one * 2, one, 2),
            Err(STATUS_INVALID_BUFFER_SIZE)
        );
    }

    #[test]
    fn count_mismatch_keeps_length_mismatch() {
        let one = size_of::<ProcessorPerformance>() as u32;
        let bytes = one * 2;
        assert_eq!(
            decide_processor_performance_query(0, bytes, bytes, 1),
            Err(STATUS_INFO_LENGTH_MISMATCH)
        );
    }

    #[test]
    fn length_mismatch_grows_when_the_kernel_reports_a_larger_buffer() {
        let one = size_of::<ProcessorPerformance>() as u32;
        assert_eq!(
            decide_processor_performance_query(STATUS_INFO_LENGTH_MISMATCH, one * 3, one, 1),
            Ok(ProcessorPerformanceQuery::GrowTo(3))
        );
    }

    #[test]
    fn length_mismatch_without_a_larger_return_keeps_the_status() {
        let bytes = size_of::<ProcessorPerformance>() as u32;
        assert_eq!(
            decide_processor_performance_query(STATUS_INFO_LENGTH_MISMATCH, bytes, bytes, 1),
            Err(STATUS_INFO_LENGTH_MISMATCH)
        );
    }

    #[test]
    fn zero_returned_length_drops_default_processor_times() {
        let mut processors = vec![ProcessorPerformance::default(); 2];
        let bytes = size_of::<ProcessorPerformance>() as u32;
        assert!(take_processor_performance_query(&mut processors, 0, 0, bytes, 2).is_err());
        assert!(processors.is_empty());
    }

    #[test]
    fn failed_zero_return_keeps_existing_rows() {
        let mut processors = vec![ProcessorPerformance {
            idle_time: 4,
            ..ProcessorPerformance::default()
        }];
        let bytes = size_of::<ProcessorPerformance>() as u32;
        assert_eq!(
            take_processor_performance_query(
                &mut processors,
                STATUS_INFO_LENGTH_MISMATCH,
                0,
                bytes,
                1
            ),
            Err(STATUS_INFO_LENGTH_MISMATCH)
        );
        assert_eq!(processors[0].idle_time, 4);
    }
}

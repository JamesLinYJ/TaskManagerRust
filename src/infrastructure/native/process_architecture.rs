// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 进程架构与 ARM 仿真识别
//
//   文件:       src/infrastructure/native/process_architecture.rs
//
//   日期:       2026年09月21日
//   环境:       macOS 27.2 ARM64；Rust 1.98.0；Windows MSVC 目标
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Queries a borrowed, identity-verified process handle on a background worker.
//! Architecture is immutable for that process lifetime; callers cache only successful results
//! by PID AND creation time. None/Err means unknown, never native. Hybrid detection describes
//! the main executable, not the fraction of emulated instructions in loaded DLLs or JIT code.

use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::ffi::OsStringExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{
    ERROR_CALL_NOT_IMPLEMENTED, ERROR_GEN_FAILURE, ERROR_INVALID_DATA, ERROR_INVALID_HANDLE,
    ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED, GetLastError, HANDLE,
};
use windows_sys::Win32::System::Diagnostics::Debug::IMAGE_LOAD_CONFIG_DIRECTORY64;
use windows_sys::Win32::System::SystemInformation::{
    IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_ARM, IMAGE_FILE_MACHINE_ARM64,
    IMAGE_FILE_MACHINE_ARMNT, IMAGE_FILE_MACHINE_I386, IMAGE_FILE_MACHINE_IA64,
    IMAGE_FILE_MACHINE_THUMB, IMAGE_FILE_MACHINE_UNKNOWN,
};
use windows_sys::Win32::System::Threading::{
    GetProcessInformation, IsWow64Process2, PROCESS_MACHINE_INFORMATION, ProcessMachineTypeInfo,
    QueryFullProcessImageNameW,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecutionMode {
    Native,
    Compatibility32,
    EmulatedX86,
    EmulatedX64,
    Arm64Ec,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProcessArchitecture {
    pub(crate) process_machine: u16,
    pub(crate) native_machine: u16,
    pub(crate) mode: ExecutionMode,
}

pub(crate) fn query_process_architecture_handle(
    handle: HANDLE,
) -> Result<ProcessArchitecture, u32> {
    if handle.is_null() {
        return Err(ERROR_INVALID_HANDLE);
    }
    let mut wow_machine = IMAGE_FILE_MACHINE_UNKNOWN;
    let mut native_machine = IMAGE_FILE_MACHINE_UNKNOWN;
    // SAFETY: caller retains the query-capable handle; both output pointers are live u16 values.
    if unsafe { IsWow64Process2(handle, &mut wow_machine, &mut native_machine) } == 0 {
        return Err(last_error());
    }
    let mut info = PROCESS_MACHINE_INFORMATION::default();
    // SAFETY: the information class, aligned output type, and byte size match the Windows API.
    let reported_machine = if unsafe {
        GetProcessInformation(
            handle,
            ProcessMachineTypeInfo,
            (&mut info as *mut PROCESS_MACHINE_INFORMATION).cast(),
            size_of::<PROCESS_MACHINE_INFORMATION>() as u32,
        )
    } != 0
    {
        Some(info.ProcessMachine)
    } else {
        let error = last_error();
        match error {
            // This information class requires Windows 11 (build 22000). Windows 10 keeps
            // only facts established by IsWow64Process2. Other failures are not hidden.
            ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED | ERROR_CALL_NOT_IMPLEMENTED => None,
            _ => return Err(error),
        }
    };
    let mut architecture = classify_machines(wow_machine, native_machine, reported_machine)?;
    if architecture.mode == ExecutionMode::EmulatedX64 && main_image_is_arm64ec(handle)? {
        architecture.mode = ExecutionMode::Arm64Ec;
    }
    Ok(architecture)
}

fn machine_is_32_bit(machine: u16) -> Result<bool, u32> {
    match machine {
        IMAGE_FILE_MACHINE_I386
        | IMAGE_FILE_MACHINE_ARM
        | IMAGE_FILE_MACHINE_ARMNT
        | IMAGE_FILE_MACHINE_THUMB => Ok(true),
        IMAGE_FILE_MACHINE_AMD64 | IMAGE_FILE_MACHINE_ARM64 | IMAGE_FILE_MACHINE_IA64 => Ok(false),
        _ => Err(ERROR_INVALID_DATA),
    }
}

fn classify_machines(
    wow_machine: u16,
    native_machine: u16,
    reported_machine: Option<u16>,
) -> Result<ProcessArchitecture, u32> {
    let native_is_32_bit = machine_is_32_bit(native_machine)?;
    let process_machine = match reported_machine {
        Some(machine) => machine,
        None if wow_machine != IMAGE_FILE_MACHINE_UNKNOWN => wow_machine,
        // On ARM64, both native ARM64 and x86-64/ARM64EC can report UNKNOWN. The old API
        // alone cannot distinguish them, including on Windows 10 preview builds.
        None if native_machine == IMAGE_FILE_MACHINE_ARM64 => return Err(ERROR_NOT_SUPPORTED),
        None => native_machine,
    };
    let process_is_32_bit = machine_is_32_bit(process_machine)?;
    let mode = match (native_machine, process_machine) {
        (IMAGE_FILE_MACHINE_ARM64, IMAGE_FILE_MACHINE_I386) => ExecutionMode::EmulatedX86,
        (IMAGE_FILE_MACHINE_ARM64, IMAGE_FILE_MACHINE_AMD64) => ExecutionMode::EmulatedX64,
        _ if process_is_32_bit && !native_is_32_bit => ExecutionMode::Compatibility32,
        _ if process_machine == native_machine => ExecutionMode::Native,
        _ => return Err(ERROR_INVALID_DATA),
    };
    Ok(ProcessArchitecture {
        process_machine,
        native_machine,
        mode,
    })
}

fn main_image_is_arm64ec(handle: HANDLE) -> Result<bool, u32> {
    let mut path = vec![0u16; 32768];
    let mut length = path.len() as u32;
    // SAFETY: the live buffer has length UTF-16 units and the borrowed handle permits querying.
    if unsafe { QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut length) } == 0 {
        return Err(last_error());
    }
    if length == 0 || length as usize >= path.len() {
        return Err(ERROR_INVALID_DATA);
    }
    let path = OsString::from_wide(&path[..length as usize]);
    let mut image = open_main_image(Path::new(&path))?;
    image_is_arm64ec(&mut image)
}

fn open_main_image(path: &Path) -> Result<File, u32> {
    #[cfg(target_arch = "x86")]
    {
        use windows_sys::Win32::Storage::FileSystem::{
            Wow64DisableWow64FsRedirection, Wow64RevertWow64FsRedirection,
        };
        let mut previous = std::ptr::null_mut();
        // SAFETY: only the current worker thread is affected. Revert immediately after open,
        // before parsing, returning, or invoking other application code. Otherwise a 32-bit
        // observer could silently open SysWOW64's different EXE for a System32 target.
        if unsafe { Wow64DisableWow64FsRedirection(&mut previous) } == 0 {
            return Err(last_error());
        }
        let file = File::open(path).map_err(io_error);
        // SAFETY: previous is the exact token returned on this same thread above.
        if unsafe { Wow64RevertWow64FsRedirection(previous) } == 0 {
            return Err(last_error());
        }
        file
    }
    #[cfg(not(target_arch = "x86"))]
    File::open(path).map_err(io_error)
}

fn last_error() -> u32 {
    // SAFETY: reads this thread's error immediately after a failed Win32 call.
    let error = unsafe { GetLastError() };
    if error == 0 { ERROR_GEN_FAILURE } else { error }
}

fn io_error(error: std::io::Error) -> u32 {
    if error.kind() == std::io::ErrorKind::UnexpectedEof {
        ERROR_INVALID_DATA
    } else {
        error
            .raw_os_error()
            .filter(|value| *value > 0)
            .map_or(ERROR_GEN_FAILURE, |value| value as u32)
    }
}

fn read_at<R: Read + Seek>(
    reader: &mut R,
    length: u64,
    offset: u64,
    data: &mut [u8],
) -> Result<(), u32> {
    if offset
        .checked_add(data.len() as u64)
        .is_none_or(|end| end > length)
    {
        return Err(ERROR_INVALID_DATA);
    }
    reader.seek(SeekFrom::Start(offset)).map_err(io_error)?;
    reader.read_exact(data).map_err(io_error)
}

fn u16_at(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

fn u32_at(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        data[offset..offset + 4]
            .try_into()
            .expect("validated PE field"),
    )
}

fn u64_at(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        data[offset..offset + 8]
            .try_into()
            .expect("validated PE field"),
    )
}

/// Read bounded headers and load configuration, never load/execute the target image or read
/// the entire file. PE32+ and the on-disk CHPE pointer are independent of observer pointer size.
/// Final ARM64EC EXEs use AMD64, NOT the MSVC intermediate-object machine value 0xA641.
fn image_is_arm64ec<R: Read + Seek>(reader: &mut R) -> Result<bool, u32> {
    let length = reader.seek(SeekFrom::End(0)).map_err(io_error)?;
    let mut dos = [0u8; 64];
    read_at(reader, length, 0, &mut dos)?;
    if &dos[..2] != b"MZ" {
        return Err(ERROR_INVALID_DATA);
    }
    let pe_offset = u64::from(u32_at(&dos, 60));
    if pe_offset < 64 {
        return Err(ERROR_INVALID_DATA);
    }
    let mut coff = [0u8; 24];
    read_at(reader, length, pe_offset, &mut coff)?;
    if &coff[..4] != b"PE\0\0" {
        return Err(ERROR_INVALID_DATA);
    }
    let section_count = usize::from(u16_at(&coff, 6));
    // The Windows loader limits PE images to 96 sections.
    if section_count == 0 || section_count > 96 {
        return Err(ERROR_INVALID_DATA);
    }
    let optional_size = usize::from(u16_at(&coff, 20));
    if optional_size < 96 {
        return Err(ERROR_INVALID_DATA);
    }
    let mut optional = vec![0u8; optional_size]; // bounded by the u16 COFF field
    read_at(reader, length, pe_offset + 24, &mut optional)?;
    let directory_start = match (u16_at(&coff, 4), u16_at(&optional, 0)) {
        (IMAGE_FILE_MACHINE_AMD64, 0x20b) if optional_size >= 112 => 112,
        (IMAGE_FILE_MACHINE_I386, 0x10b) => 96,
        _ => return Err(ERROR_INVALID_DATA),
    };
    let header_size = u32_at(&optional, 60);
    let directory_count = u32_at(&optional, directory_start - 4);
    if directory_count as u64 > ((optional_size - directory_start) / 8) as u64 {
        return Err(ERROR_INVALID_DATA);
    }
    let mut sections = vec![0u8; section_count * 40];
    read_at(
        reader,
        length,
        pe_offset + 24 + optional_size as u64,
        &mut sections,
    )?;
    if directory_start == 96 {
        // An AnyCPU managed EXE can have an I386/PE32 disk header while its actual CLR
        // process is x86-64. Use the queried runtime architecture; do not call it ARM64EC
        // or mistake the disk header for a running x86 process.
        if directory_count <= 14 {
            return Err(ERROR_INVALID_DATA);
        }
        let clr_rva = u32_at(&optional, directory_start + 14 * 8);
        let clr_size = u32_at(&optional, directory_start + 14 * 8 + 4);
        if clr_rva == 0 || clr_size < 72 {
            return Err(ERROR_INVALID_DATA);
        }
        let clr_offset = rva_offset(clr_rva, clr_size, header_size, &sections, length)?;
        let mut clr = [0u8; 20];
        read_at(reader, length, clr_offset, &mut clr)?;
        let flags = u32_at(&clr, 16);
        if u32_at(&clr, 0) < 72 || u32_at(&clr, 0) > clr_size || flags & 3 != 1 {
            return Err(ERROR_INVALID_DATA);
        }
        return Ok(false);
    }
    let image_base = u64_at(&optional, 24);
    if directory_count <= 10 {
        return Ok(false);
    }
    let config_rva = u32_at(&optional, 112 + 10 * 8);
    let config_size = u32_at(&optional, 116 + 10 * 8);
    if config_rva == 0 && config_size == 0 {
        return Ok(false);
    }
    if config_rva == 0 || config_size < 4 {
        return Err(ERROR_INVALID_DATA);
    }
    let config_offset = rva_offset(config_rva, config_size, header_size, &sections, length)?;
    let mut size_bytes = [0u8; 4];
    read_at(reader, length, config_offset, &mut size_bytes)?;
    let declared_size = u32_at(&size_bytes, 0);
    if declared_size < 4 || declared_size > config_size {
        return Err(ERROR_INVALID_DATA);
    }
    const CHPE_OFFSET: usize =
        std::mem::offset_of!(IMAGE_LOAD_CONFIG_DIRECTORY64, CHPEMetadataPointer);
    const CHPE_END: u32 = (CHPE_OFFSET + size_of::<u64>()) as u32;
    if declared_size < CHPE_END {
        return Ok(false);
    }
    let mut pointer_bytes = [0u8; 8];
    read_at(
        reader,
        length,
        config_offset + CHPE_OFFSET as u64,
        &mut pointer_bytes,
    )?;
    let pointer = u64_at(&pointer_bytes, 0);
    if pointer == 0 {
        return Ok(false);
    }
    let metadata_rva = pointer
        .checked_sub(image_base)
        .and_then(|rva| u32::try_from(rva).ok())
        .ok_or(ERROR_INVALID_DATA)?;
    rva_offset(metadata_rva, 4, header_size, &sections, length)?;
    Ok(true)
}

fn rva_offset(
    rva: u32,
    size: u32,
    header_size: u32,
    sections: &[u8],
    length: u64,
) -> Result<u64, u32> {
    let rva = u64::from(rva);
    let end = rva + u64::from(size);
    let mut found = if end <= u64::from(header_size) && end <= length {
        Some(rva)
    } else {
        None
    };
    for section in sections.as_chunks::<40>().0 {
        let address = u64::from(u32_at(section, 12));
        let raw_size = u64::from(u32_at(section, 16));
        let raw_offset = u64::from(u32_at(section, 20));
        if rva >= address && end <= address + raw_size {
            let offset = raw_offset + rva - address;
            if found.is_some() || offset + u64::from(size) > length {
                return Err(ERROR_INVALID_DATA);
            }
            found = Some(offset);
        }
    }
    found.ok_or(ERROR_INVALID_DATA)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn classifies_host_and_process_independently_of_observer() {
        for (host, process, mode) in [
            (
                IMAGE_FILE_MACHINE_ARM64,
                IMAGE_FILE_MACHINE_I386,
                ExecutionMode::EmulatedX86,
            ),
            (
                IMAGE_FILE_MACHINE_ARM64,
                IMAGE_FILE_MACHINE_AMD64,
                ExecutionMode::EmulatedX64,
            ),
            (
                IMAGE_FILE_MACHINE_ARM64,
                IMAGE_FILE_MACHINE_ARM64,
                ExecutionMode::Native,
            ),
            (
                IMAGE_FILE_MACHINE_ARM64,
                IMAGE_FILE_MACHINE_ARMNT,
                ExecutionMode::Compatibility32,
            ),
            (
                IMAGE_FILE_MACHINE_AMD64,
                IMAGE_FILE_MACHINE_I386,
                ExecutionMode::Compatibility32,
            ),
            (
                IMAGE_FILE_MACHINE_AMD64,
                IMAGE_FILE_MACHINE_AMD64,
                ExecutionMode::Native,
            ),
            (
                IMAGE_FILE_MACHINE_I386,
                IMAGE_FILE_MACHINE_I386,
                ExecutionMode::Native,
            ),
        ] {
            let result =
                classify_machines(IMAGE_FILE_MACHINE_UNKNOWN, host, Some(process)).unwrap();
            assert_eq!(result.mode, mode);
            assert_eq!(result.process_machine, process);
            assert_eq!(result.native_machine, host);
        }
    }

    #[test]
    fn old_api_keeps_known_bitness_but_never_guesses_arm64_native() {
        assert_eq!(
            classify_machines(0, IMAGE_FILE_MACHINE_ARM64, None),
            Err(ERROR_NOT_SUPPORTED)
        );
        assert_eq!(
            classify_machines(IMAGE_FILE_MACHINE_I386, IMAGE_FILE_MACHINE_ARM64, None)
                .unwrap()
                .mode,
            ExecutionMode::EmulatedX86
        );
        assert_eq!(
            classify_machines(0, IMAGE_FILE_MACHINE_I386, None)
                .unwrap()
                .mode,
            ExecutionMode::Native
        );
        assert_eq!(
            classify_machines(IMAGE_FILE_MACHINE_I386, IMAGE_FILE_MACHINE_AMD64, None)
                .unwrap()
                .mode,
            ExecutionMode::Compatibility32
        );
        assert_eq!(
            classify_machines(0, IMAGE_FILE_MACHINE_ARM64, Some(0)),
            Err(ERROR_INVALID_DATA)
        );
        assert_eq!(
            classify_machines(0, 0xffff, Some(IMAGE_FILE_MACHINE_I386)),
            Err(ERROR_INVALID_DATA)
        );
    }

    fn put16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }
    fn put32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn put64(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn image(hybrid: bool) -> Vec<u8> {
        let mut bytes = vec![0u8; 1024];
        bytes[..2].copy_from_slice(b"MZ");
        put32(&mut bytes, 60, 64);
        bytes[64..68].copy_from_slice(b"PE\0\0");
        put16(&mut bytes, 68, IMAGE_FILE_MACHINE_AMD64);
        put16(&mut bytes, 70, 1);
        put16(&mut bytes, 84, 240);
        put16(&mut bytes, 88, 0x20b);
        put64(&mut bytes, 112, 0x140000000);
        put32(&mut bytes, 148, 512);
        put32(&mut bytes, 196, 16);
        put32(&mut bytes, 280, 4096);
        put32(&mut bytes, 284, 208);
        put32(&mut bytes, 340, 4096);
        put32(&mut bytes, 344, 512);
        put32(&mut bytes, 348, 512);
        put32(&mut bytes, 512, 208);
        if hybrid {
            put64(&mut bytes, 712, 0x140001100);
        }
        bytes
    }

    #[test]
    fn final_amd64_images_are_distinguished_by_hybrid_metadata() {
        assert_eq!(image_is_arm64ec(&mut Cursor::new(image(false))), Ok(false));
        assert_eq!(image_is_arm64ec(&mut Cursor::new(image(true))), Ok(true));
        let mut old = image(false);
        put32(&mut old, 512, 192);
        assert_eq!(image_is_arm64ec(&mut Cursor::new(old)), Ok(false));
    }

    #[test]
    fn anycpu_disk_headers_do_not_override_the_queried_runtime_architecture() {
        let mut bytes = image(false);
        put16(&mut bytes, 68, IMAGE_FILE_MACHINE_I386);
        put16(&mut bytes, 88, 0x10b);
        put32(&mut bytes, 180, 16);
        put32(&mut bytes, 296, 4096);
        put32(&mut bytes, 300, 72);
        put32(&mut bytes, 512, 72);
        put32(&mut bytes, 528, 1); // COMIMAGE_FLAGS_ILONLY
        assert_eq!(image_is_arm64ec(&mut Cursor::new(&bytes)), Ok(false));
        put32(&mut bytes, 528, 3); // COMIMAGE_FLAGS_32BITREQUIRED is incompatible with x86-64
        assert_eq!(
            image_is_arm64ec(&mut Cursor::new(&bytes)),
            Err(ERROR_INVALID_DATA)
        );
        put32(&mut bytes, 296, 0);
        assert_eq!(
            image_is_arm64ec(&mut Cursor::new(&bytes)),
            Err(ERROR_INVALID_DATA)
        );
    }

    #[test]
    fn truncated_or_malformed_images_never_claim_emulation() {
        let valid = image(true);
        for length in [0, 63, 87, 199, 327, 367, 511, 719, 771] {
            assert_eq!(
                image_is_arm64ec(&mut Cursor::new(&valid[..length])),
                Err(ERROR_INVALID_DATA)
            );
        }
        for (offset, value) in [
            (60, u32::MAX),
            (196, u32::MAX),
            (280, u32::MAX),
            (284, u32::MAX),
            (512, 209),
            (344, 1),
            (348, u32::MAX),
        ] {
            let mut bytes = image(true);
            put32(&mut bytes, offset, value);
            assert_eq!(
                image_is_arm64ec(&mut Cursor::new(bytes)),
                Err(ERROR_INVALID_DATA)
            );
        }
        let mut object_machine = image(true);
        put16(&mut object_machine, 68, 0xa641);
        assert_eq!(
            image_is_arm64ec(&mut Cursor::new(object_machine)),
            Err(ERROR_INVALID_DATA)
        );
        let mut bad_pointer = image(true);
        put64(&mut bad_pointer, 712, 1);
        assert_eq!(
            image_is_arm64ec(&mut Cursor::new(bad_pointer)),
            Err(ERROR_INVALID_DATA)
        );
    }
}

// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 配置校验与迁移
//
//   文件:       src/config/options.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

use std::mem::{size_of, zeroed};

// 持久化配置模块。
// 该模块维护与历史 Task Manager 注册表格式兼容的选项结构，并负责默认值、
// 数据合法性校验以及注册表的读写边界。
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_INVALID_DATA, ERROR_REVISION_MISMATCH, ERROR_SUCCESS, RECT,
};
use windows_sys::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromRect};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_BINARY, REG_OPTION_NON_VOLATILE, RegCloseKey,
    RegCreateKeyExW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_MENU, VK_SHIFT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXMAXIMIZED, SM_CXVIRTUALSCREEN, SM_CYMAXIMIZED, SM_CYVIRTUALSCREEN,
    SPI_GETSCREENREADER, SystemParametersInfoW,
};

use crate::app::page_registry::PageId;
use crate::infrastructure::native::{record_win32_error, to_wide_null};
use crate::ui::resource_ids::NUM_COLUMN;

const OPTIONS_KEY: &str = "Software\\taskmgr-rs\\TaskManager";
const OPTIONS_VALUE: &str = "OptionsV1";
const LEGACY_TASKMAN_KEY: &str = "Software\\Microsoft\\Windows NT\\CurrentVersion\\TaskManager";
const LEGACY_OPTIONS_VALUE: &str = "Preferences";
const OPTIONS_STORAGE_MAGIC: [u8; 8] = *b"TMGRRS01";
const OPTIONS_STORAGE_VERSION: u32 = 1;
const OPTIONS_SCHEMA_VERSION: i32 = 3;
const SCHEMA_2_VERSION: i32 = 2;
// Schema 2 stored 16 process columns plus the terminating sentinel. Those two arrays stay at
// this length so later fields keep their offsets. Schema 3 appends the extra slots below.
const SCHEMA2_COLUMN_SLOTS: usize = 17;
const EXTENDED_COLUMN_SLOTS: usize = (NUM_COLUMN + 1) - SCHEMA2_COLUMN_SLOTS;
const SCHEMA_0_NETWORK_PAGE: i32 = 3;
const SCHEMA_0_USERS_PAGE: i32 = 4;
const SCHEMA_1_GPU_PAGE: i32 = 3;
const SCHEMA_1_NETWORK_PAGE: i32 = 4;
const SCHEMA_1_USERS_PAGE: i32 = 5;

// 这些 flag 会按历史二进制格式打包到 `Options.flags`。
const FLAG_MINIMIZE_ON_USE: u32 = 1 << 0;
const FLAG_CONFIRMATIONS: u32 = 1 << 1;
const FLAG_ALWAYS_ON_TOP: u32 = 1 << 2;
const FLAG_KERNEL_TIMES: u32 = 1 << 3;
const FLAG_NO_TITLE: u32 = 1 << 4;
const FLAG_HIDE_WHEN_MIN: u32 = 1 << 5;
const ALL_VALID_FLAGS: u32 = FLAG_MINIMIZE_ON_USE
    | FLAG_CONFIRMATIONS
    | FLAG_ALWAYS_ON_TOP
    | FLAG_KERNEL_TIMES
    | FLAG_NO_TITLE
    | FLAG_HIDE_WHEN_MIN;

#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    // 应用程序页 ListView 视图模式。
    LargeIcon = 0,
    SmallIcon = 1,
    Details = 2,
}

#[repr(i32)]
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CpuHistoryMode {
    // 性能页 CPU 历史图的两种经典显示模式。
    Sum = 0,
    Panes = 1,
}

#[repr(i32)]
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UpdateSpeed {
    // 刷新速度同时影响定时器间隔和是否暂停自动刷新。
    High = 0,
    Normal = 1,
    Low = 2,
    Paused = 3,
}

impl UpdateSpeed {
    const fn from_raw(value: i32) -> Option<Self> {
        match value {
            x if x == Self::High as i32 => Some(Self::High),
            x if x == Self::Normal as i32 => Some(Self::Normal),
            x if x == Self::Low as i32 => Some(Self::Low),
            x if x == Self::Paused as i32 => Some(Self::Paused),
            _ => None,
        }
    }

    const fn timer_interval(self) -> u32 {
        match self {
            Self::High => 500,
            Self::Normal => 2_000,
            Self::Low => 4_000,
            Self::Paused => 0,
        }
    }
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnId {
    // 进程页列枚举，顺序必须和持久化格式保持稳定。
    ImageName = 0,
    Pid = 1,
    Username = 2,
    SessionId = 3,
    Cpu = 4,
    CpuTime = 5,
    MemUsage = 6,
    MemUsageDiff = 7,
    PageFaults = 8,
    PageFaultsDiff = 9,
    CommitCharge = 10,
    PagedPool = 11,
    NonPagedPool = 12,
    BasePriority = 13,
    HandleCount = 14,
    ThreadCount = 15,
    CommandLine = 16,
    IoReadBytes = 17,
    IoWriteBytes = 18,
    IoReadCount = 19,
    IoWriteCount = 20,
    GdiObjects = 21,
    UserObjects = 22,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Options {
    // 该结构体会按二进制整体落盘到注册表，因此字段顺序和类型都需要保持稳定。
    pub cb_size: u32,
    pub timer_interval: u32,
    pub view_mode: i32,
    pub cpu_history_mode: i32,
    pub update_speed: i32,
    pub window_rect: RECT,
    pub current_page: i32,
    pub active_process_columns: [i32; SCHEMA2_COLUMN_SLOTS],
    pub column_widths: [i32; SCHEMA2_COLUMN_SLOTS],
    flags: u32,
    pub unused: i32,
    pub unused2: i32,
    pub extended_process_columns: [i32; EXTENDED_COLUMN_SLOTS],
    pub extended_column_widths: [i32; EXTENDED_COLUMN_SLOTS],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct OptionsV2 {
    cb_size: u32,
    timer_interval: u32,
    view_mode: i32,
    cpu_history_mode: i32,
    update_speed: i32,
    window_rect: RECT,
    current_page: i32,
    active_process_columns: [i32; SCHEMA2_COLUMN_SLOTS],
    column_widths: [i32; SCHEMA2_COLUMN_SLOTS],
    flags: u32,
    unused: i32,
    unused2: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct StoredOptions {
    magic: [u8; 8],
    storage_version: u32,
    payload_size: u32,
    options: Options,
}

impl StoredOptions {
    fn new(options: Options) -> Self {
        Self {
            magic: OPTIONS_STORAGE_MAGIC,
            storage_version: OPTIONS_STORAGE_VERSION,
            payload_size: size_of::<Options>() as u32,
            options,
        }
    }

    fn into_options(self) -> Result<Options, u32> {
        if self.magic != OPTIONS_STORAGE_MAGIC || self.payload_size != size_of::<Options>() as u32 {
            return Err(ERROR_INVALID_DATA);
        }
        if self.storage_version != OPTIONS_STORAGE_VERSION {
            return Err(ERROR_REVISION_MISMATCH);
        }
        Ok(self.options)
    }
}

/// Marker for fixed-layout registry values whose every bit pattern is a valid Rust value.
///
/// # Safety
///
/// Implementors must be `Copy`, have a stable layout, contain no references, and permit every
/// possible byte pattern. This lets the registry reader initialize the value as zeroed storage
/// before Windows fills every byte.
unsafe trait RegistryPod: Copy {}

unsafe impl RegistryPod for Options {}
unsafe impl RegistryPod for OptionsV2 {}
unsafe impl RegistryPod for StoredOptions {}

const _: () = {
    assert!(SCHEMA2_COLUMN_SLOTS == 17);
    assert!(EXTENDED_COLUMN_SLOTS == 7);
    assert!(ColumnId::ThreadCount as i32 == 15);
    assert!(ColumnId::UserObjects as i32 == 22);
    assert!(size_of::<Options>() == size_of::<OptionsV2>() + EXTENDED_COLUMN_SLOTS * 2 * 4);
};

impl Default for Options {
    fn default() -> Self {
        // 默认值尽量贴近经典任务管理器的首次启动体验。
        let mut options = Self {
            cb_size: size_of::<Self>() as u32,
            timer_interval: UpdateSpeed::Normal.timer_interval(),
            view_mode: ViewMode::Details as i32,
            cpu_history_mode: CpuHistoryMode::Panes as i32,
            update_speed: UpdateSpeed::Normal as i32,
            window_rect: RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            current_page: -1,
            active_process_columns: [-1; SCHEMA2_COLUMN_SLOTS],
            column_widths: [-1; SCHEMA2_COLUMN_SLOTS],
            flags: 0,
            unused: OPTIONS_SCHEMA_VERSION,
            unused2: 0,
            extended_process_columns: [-1; EXTENDED_COLUMN_SLOTS],
            extended_column_widths: [-1; EXTENDED_COLUMN_SLOTS],
        };

        options.set_minimize_on_use(true);
        options.set_confirmations(true);
        options.active_process_columns[0] = ColumnId::ImageName as i32;
        options.active_process_columns[1] = ColumnId::Username as i32;
        options.active_process_columns[2] = ColumnId::SessionId as i32;
        options.active_process_columns[3] = ColumnId::Cpu as i32;
        options.active_process_columns[4] = ColumnId::MemUsage as i32;

        options
    }
}

impl Options {
    pub fn set_default_values(&mut self, min_width: i32, min_height: i32) {
        // 默认窗口位置基于当前屏幕可用最大化区域居中生成。
        *self = Self::default();

        if screen_reader_enabled() {
            self.timer_interval = 0;
        }

        // 安全性: querying system metrics has no pointer inputs or lifetime requirements.
        let screen_width = unsafe { GetSystemMetrics(SM_CXMAXIMIZED) };
        // 安全性: querying system metrics has no pointer inputs or lifetime requirements.
        let screen_height = unsafe { GetSystemMetrics(SM_CYMAXIMIZED) };

        self.window_rect.left = (screen_width - min_width) / 2;
        self.window_rect.top = (screen_height - min_height) / 2;
        self.window_rect.right = self.window_rect.left + min_width;
        self.window_rect.bottom = self.window_rect.top + min_height;
    }

    pub fn load(&mut self, min_width: i32, min_height: i32) -> bool {
        // taskmgr-rs owns a versioned application-specific value. The Microsoft Task Manager key
        // is read only for a one-time migration of values that carry taskmgr-rs' exact schema
        // marker; it is never written or deleted.
        if modifiers_force_defaults() {
            self.set_default_values(min_width, min_height);
            return false;
        }

        match read_registry_bytes(OPTIONS_KEY, OPTIONS_VALUE) {
            Ok(Some(bytes)) => {
                let loaded = match decode_stored_options(&bytes) {
                    Ok(loaded) => loaded,
                    Err(error) => {
                        record_win32_error("taskmgr-rs options envelope", error);
                        self.set_default_values(min_width, min_height);
                        return false;
                    }
                };
                return self.apply_loaded_options(loaded, min_width, min_height, false);
            }
            Ok(None) => {}
            Err(error) => {
                record_win32_error("reading taskmgr-rs options", error);
                self.set_default_values(min_width, min_height);
                return false;
            }
        }

        match read_registry_bytes(LEGACY_TASKMAN_KEY, LEGACY_OPTIONS_VALUE) {
            Ok(Some(bytes)) => match decode_legacy_taskmgr_rs_options(&bytes) {
                Some(legacy) => self.apply_loaded_options(legacy, min_width, min_height, true),
                None => {
                    self.set_default_values(min_width, min_height);
                    false
                }
            },
            Ok(None) => {
                self.set_default_values(min_width, min_height);
                false
            }
            Err(error) => {
                record_win32_error("reading legacy taskmgr-rs options", error);
                self.set_default_values(min_width, min_height);
                false
            }
        }
    }

    fn apply_loaded_options(
        &mut self,
        mut loaded: Options,
        min_width: i32,
        min_height: i32,
        persist_migration: bool,
    ) -> bool {
        let migrated = match loaded.migrate_schema() {
            Ok(migrated) => migrated,
            Err(()) => {
                record_win32_error("unsupported options schema", ERROR_REVISION_MISMATCH);
                self.set_default_values(min_width, min_height);
                return false;
            }
        };
        let loaded_was_valid = loaded.is_valid(min_width, min_height);
        if !loaded_was_valid {
            loaded.normalize(min_width, min_height);
        }
        *self = loaded;
        if (persist_migration || migrated || !loaded_was_valid)
            && let Err(error) = self.save()
        {
            record_win32_error("normalized options persistence", error);
        }
        loaded_was_valid
    }

    pub fn save(&self) -> Result<(), u32> {
        let stored = StoredOptions::new(*self);
        write_registry_binary(OPTIONS_KEY, OPTIONS_VALUE, &stored)
    }

    pub fn minimize_on_use(&self) -> bool {
        self.flags & FLAG_MINIMIZE_ON_USE != 0
    }

    pub fn set_minimize_on_use(&mut self, value: bool) {
        self.set_flag(FLAG_MINIMIZE_ON_USE, value);
    }

    pub fn confirmations(&self) -> bool {
        self.flags & FLAG_CONFIRMATIONS != 0
    }

    pub fn set_confirmations(&mut self, value: bool) {
        self.set_flag(FLAG_CONFIRMATIONS, value);
    }

    pub fn always_on_top(&self) -> bool {
        self.flags & FLAG_ALWAYS_ON_TOP != 0
    }

    pub fn set_always_on_top(&mut self, value: bool) {
        self.set_flag(FLAG_ALWAYS_ON_TOP, value);
    }

    pub fn kernel_times(&self) -> bool {
        self.flags & FLAG_KERNEL_TIMES != 0
    }

    pub fn set_kernel_times(&mut self, value: bool) {
        self.set_flag(FLAG_KERNEL_TIMES, value);
    }

    pub fn no_title(&self) -> bool {
        self.flags & FLAG_NO_TITLE != 0
    }

    pub fn set_no_title(&mut self, value: bool) {
        self.set_flag(FLAG_NO_TITLE, value);
    }

    pub fn hide_when_minimized(&self) -> bool {
        self.flags & FLAG_HIDE_WHEN_MIN != 0
    }

    pub fn set_hide_when_minimized(&mut self, value: bool) {
        self.set_flag(FLAG_HIDE_WHEN_MIN, value);
    }

    fn is_valid(&self, min_width: i32, min_height: i32) -> bool {
        // 这里显式校验所有会影响数组索引或窗口状态的字段，
        // 防止损坏的注册表值在后续刷新路径里触发越界或错误状态。
        // 安全性: querying system metrics has no pointer inputs or lifetime requirements.
        let max_width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
        // 安全性: querying system metrics has no pointer inputs or lifetime requirements.
        let max_height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };

        self.cb_size == size_of::<Self>() as u32
            && window_rect_is_valid(
                &self.window_rect,
                min_width,
                min_height,
                max_width,
                max_height,
            )
            && self.current_page >= -1
            && self.current_page < PageId::COUNT as i32
            && is_valid_view_mode(self.view_mode)
            && is_valid_cpu_history_mode(self.cpu_history_mode)
            && is_valid_update_speed(self.update_speed)
            && timer_interval_is_valid(self.update_speed, self.timer_interval)
            && self.flags & !ALL_VALID_FLAGS == 0
            && self.unused == OPTIONS_SCHEMA_VERSION
            && self.unused2 == 0
            && self.process_columns_are_valid()
    }

    fn migrate_schema(&mut self) -> Result<bool, ()> {
        match self.unused {
            0 => {
                self.current_page = match self.current_page {
                    SCHEMA_0_NETWORK_PAGE => 5,
                    SCHEMA_0_USERS_PAGE => 6,
                    page => page,
                };
                self.finish_column_schema_migration();
                Ok(true)
            }
            1 => {
                if matches!(
                    self.current_page,
                    SCHEMA_1_GPU_PAGE | SCHEMA_1_NETWORK_PAGE | SCHEMA_1_USERS_PAGE
                ) {
                    self.current_page += 1;
                }
                self.finish_column_schema_migration();
                Ok(true)
            }
            SCHEMA_2_VERSION => {
                self.finish_column_schema_migration();
                Ok(true)
            }
            OPTIONS_SCHEMA_VERSION => Ok(false),
            _ => Err(()),
        }
    }

    fn finish_column_schema_migration(&mut self) {
        self.cb_size = size_of::<Self>() as u32;
        self.unused = OPTIONS_SCHEMA_VERSION;
        self.unused2 = 0;
        self.extended_process_columns = [-1; EXTENDED_COLUMN_SLOTS];
        self.extended_column_widths = [-1; EXTENDED_COLUMN_SLOTS];
    }

    fn normalize(&mut self, min_width: i32, min_height: i32) {
        let mut defaults = Self::default();
        defaults.set_default_values(min_width, min_height);

        self.cb_size = size_of::<Self>() as u32;
        self.unused = OPTIONS_SCHEMA_VERSION;
        self.unused2 = 0;
        if !is_valid_view_mode(self.view_mode) {
            self.view_mode = defaults.view_mode;
        }
        if !is_valid_cpu_history_mode(self.cpu_history_mode) {
            self.cpu_history_mode = defaults.cpu_history_mode;
        }
        let update_speed = match UpdateSpeed::from_raw(self.update_speed) {
            Some(update_speed) => update_speed,
            None => {
                self.update_speed = UpdateSpeed::Normal as i32;
                UpdateSpeed::Normal
            }
        };
        if !timer_interval_is_valid(self.update_speed, self.timer_interval) {
            self.timer_interval = if screen_reader_enabled() {
                0
            } else {
                update_speed.timer_interval()
            };
        }
        if self.current_page < -1 || self.current_page >= PageId::COUNT as i32 {
            self.current_page = defaults.current_page;
        }
        self.flags &= ALL_VALID_FLAGS;

        // 安全性: querying system metrics has no pointer inputs or lifetime requirements.
        let max_width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
        // 安全性: querying system metrics has no pointer inputs or lifetime requirements.
        let max_height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
        if !window_rect_is_valid(
            &self.window_rect,
            min_width,
            min_height,
            max_width,
            max_height,
        ) {
            self.window_rect = defaults.window_rect;
        }

        let (mut columns, mut widths) = self.logical_process_columns();
        normalize_process_columns(&mut columns, &mut widths);
        self.store_logical_process_columns(&columns, &widths);
    }

    pub(crate) fn clear_process_column_slots(&mut self) {
        self.active_process_columns = [-1; SCHEMA2_COLUMN_SLOTS];
        self.column_widths = [-1; SCHEMA2_COLUMN_SLOTS];
        self.extended_process_columns = [-1; EXTENDED_COLUMN_SLOTS];
        self.extended_column_widths = [-1; EXTENDED_COLUMN_SLOTS];
    }

    pub(crate) fn process_column_id(&self, index: usize) -> i32 {
        self.column_slot(index)
            .map(|(column, _)| column)
            .unwrap_or(-1)
    }

    pub(crate) fn process_column_width(&self, index: usize) -> i32 {
        self.column_slot(index)
            .map(|(_, width)| width)
            .unwrap_or(-1)
    }

    pub(crate) fn set_process_column_slot(&mut self, index: usize, column: i32, width: i32) {
        if index < SCHEMA2_COLUMN_SLOTS {
            self.active_process_columns[index] = column;
            self.column_widths[index] = width;
        } else if index - SCHEMA2_COLUMN_SLOTS < EXTENDED_COLUMN_SLOTS {
            let extended = index - SCHEMA2_COLUMN_SLOTS;
            self.extended_process_columns[extended] = column;
            self.extended_column_widths[extended] = width;
        }
    }

    fn column_slot(&self, index: usize) -> Option<(i32, i32)> {
        if index < SCHEMA2_COLUMN_SLOTS {
            Some((
                self.active_process_columns[index],
                self.column_widths[index],
            ))
        } else {
            let extended = index - SCHEMA2_COLUMN_SLOTS;
            (extended < EXTENDED_COLUMN_SLOTS).then_some((
                self.extended_process_columns[extended],
                self.extended_column_widths[extended],
            ))
        }
    }

    fn process_columns_are_valid(&self) -> bool {
        let (columns, widths) = self.logical_process_columns();
        process_columns_are_valid(&columns, &widths)
    }

    fn logical_process_columns(&self) -> ([i32; NUM_COLUMN + 1], [i32; NUM_COLUMN + 1]) {
        let mut columns = [-1; NUM_COLUMN + 1];
        let mut widths = [-1; NUM_COLUMN + 1];
        columns[..SCHEMA2_COLUMN_SLOTS].copy_from_slice(&self.active_process_columns);
        widths[..SCHEMA2_COLUMN_SLOTS].copy_from_slice(&self.column_widths);
        columns[SCHEMA2_COLUMN_SLOTS..].copy_from_slice(&self.extended_process_columns);
        widths[SCHEMA2_COLUMN_SLOTS..].copy_from_slice(&self.extended_column_widths);
        (columns, widths)
    }

    fn store_logical_process_columns(
        &mut self,
        columns: &[i32; NUM_COLUMN + 1],
        widths: &[i32; NUM_COLUMN + 1],
    ) {
        self.active_process_columns
            .copy_from_slice(&columns[..SCHEMA2_COLUMN_SLOTS]);
        self.column_widths
            .copy_from_slice(&widths[..SCHEMA2_COLUMN_SLOTS]);
        self.extended_process_columns
            .copy_from_slice(&columns[SCHEMA2_COLUMN_SLOTS..]);
        self.extended_column_widths
            .copy_from_slice(&widths[SCHEMA2_COLUMN_SLOTS..]);
    }

    fn from_v2(old: OptionsV2) -> Self {
        Self {
            cb_size: size_of::<Self>() as u32,
            timer_interval: old.timer_interval,
            view_mode: old.view_mode,
            cpu_history_mode: old.cpu_history_mode,
            update_speed: old.update_speed,
            window_rect: old.window_rect,
            current_page: old.current_page,
            active_process_columns: old.active_process_columns,
            column_widths: old.column_widths,
            flags: old.flags,
            unused: old.unused,
            unused2: old.unused2,
            extended_process_columns: [-1; EXTENDED_COLUMN_SLOTS],
            extended_column_widths: [-1; EXTENDED_COLUMN_SLOTS],
        }
    }

    fn set_flag(&mut self, mask: u32, value: bool) {
        if value {
            self.flags |= mask;
        } else {
            self.flags &= !mask;
        }
    }
}

fn legacy_options_is_taskmgr_rs(options: &Options) -> bool {
    options.cb_size == size_of::<Options>() as u32
        && options.unused == OPTIONS_SCHEMA_VERSION
        && options.unused2 == 0
        && options.current_page >= -1
        && options.current_page < PageId::COUNT as i32
        && is_valid_view_mode(options.view_mode)
        && is_valid_cpu_history_mode(options.cpu_history_mode)
        && is_valid_update_speed(options.update_speed)
        && options.flags & !ALL_VALID_FLAGS == 0
        && options.process_columns_are_valid()
}

fn legacy_v2_options_are_taskmgr_rs(options: &OptionsV2) -> bool {
    let migrated = Options::from_v2(*options);
    options.cb_size == size_of::<OptionsV2>() as u32
        && options.unused == SCHEMA_2_VERSION
        && options.unused2 == 0
        && migrated.current_page >= -1
        && migrated.current_page < PageId::COUNT as i32
        && is_valid_view_mode(migrated.view_mode)
        && is_valid_cpu_history_mode(migrated.cpu_history_mode)
        && is_valid_update_speed(migrated.update_speed)
        && migrated.flags & !ALL_VALID_FLAGS == 0
        && migrated.process_columns_are_valid()
}

fn decode_stored_options(bytes: &[u8]) -> Result<Options, u32> {
    if bytes.len() < 16 {
        return Err(ERROR_INVALID_DATA);
    }
    let mut magic = [0u8; 8];
    magic.copy_from_slice(&bytes[..8]);
    if magic != OPTIONS_STORAGE_MAGIC {
        return Err(ERROR_INVALID_DATA);
    }
    let storage_version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if storage_version != OPTIONS_STORAGE_VERSION {
        return Err(ERROR_REVISION_MISMATCH);
    }
    let payload_size = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    let payload_len = usize::try_from(payload_size).map_err(|_| ERROR_INVALID_DATA)?;
    if bytes.len() != 16 + payload_len {
        return Err(ERROR_INVALID_DATA);
    }
    let payload = &bytes[16..];
    if payload_len == size_of::<OptionsV2>() {
        Ok(Options::from_v2(read_pod::<OptionsV2>(payload)?))
    } else if payload_len == size_of::<Options>() && bytes.len() == size_of::<StoredOptions>() {
        read_pod::<StoredOptions>(bytes)?.into_options()
    } else {
        Err(ERROR_INVALID_DATA)
    }
}

fn decode_legacy_taskmgr_rs_options(bytes: &[u8]) -> Option<Options> {
    if bytes.len() == size_of::<Options>() {
        let options = read_pod::<Options>(bytes).ok()?;
        legacy_options_is_taskmgr_rs(&options).then_some(options)
    } else if bytes.len() == size_of::<OptionsV2>() {
        let old = read_pod::<OptionsV2>(bytes).ok()?;
        legacy_v2_options_are_taskmgr_rs(&old).then_some(Options::from_v2(old))
    } else {
        None
    }
}

fn read_pod<T: RegistryPod>(bytes: &[u8]) -> Result<T, u32> {
    if bytes.len() != size_of::<T>() {
        return Err(ERROR_INVALID_DATA);
    }
    // SAFETY: RegistryPod permits every bit pattern. `bytes` has the exact size of `T`, and the
    // copy lands in aligned owned storage before any field is read.
    unsafe {
        let mut value = zeroed::<T>();
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            (&mut value as *mut T).cast::<u8>(),
            bytes.len(),
        );
        Ok(value)
    }
}

fn read_registry_bytes(key_path: &str, value_name: &str) -> Result<Option<Vec<u8>>, u32> {
    unsafe {
        let key_path = to_wide_null(key_path);
        let value_name = to_wide_null(value_name);
        let mut key: HKEY = null_mut();
        let open_status =
            RegOpenKeyExW(HKEY_CURRENT_USER, key_path.as_ptr(), 0, KEY_READ, &mut key);
        if open_status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if open_status != ERROR_SUCCESS {
            return Err(open_status);
        }

        let mut value_type = 0u32;
        let mut value_size = 0u32;
        let size_status = RegQueryValueExW(
            key,
            value_name.as_ptr(),
            null_mut(),
            &mut value_type,
            null_mut(),
            &mut value_size,
        );
        if size_status == ERROR_FILE_NOT_FOUND {
            let close_status = RegCloseKey(key);
            return if close_status == ERROR_SUCCESS {
                Ok(None)
            } else {
                Err(close_status)
            };
        }
        if size_status != ERROR_SUCCESS {
            RegCloseKey(key);
            return Err(size_status);
        }
        if value_type != REG_BINARY || value_size == 0 || value_size > 1_048_576 {
            RegCloseKey(key);
            return Err(ERROR_INVALID_DATA);
        }

        let mut value = vec![0u8; value_size as usize];
        let mut actual_size = value_size;
        let read_status = RegQueryValueExW(
            key,
            value_name.as_ptr(),
            null_mut(),
            &mut value_type,
            value.as_mut_ptr(),
            &mut actual_size,
        );
        let close_status = RegCloseKey(key);
        if read_status != ERROR_SUCCESS {
            return Err(read_status);
        }
        if close_status != ERROR_SUCCESS {
            return Err(close_status);
        }
        if value_type != REG_BINARY || actual_size != value_size {
            return Err(ERROR_INVALID_DATA);
        }
        value.truncate(actual_size as usize);
        Ok(Some(value))
    }
}

fn write_registry_binary<T: RegistryPod>(
    key_path: &str,
    value_name: &str,
    value: &T,
) -> Result<(), u32> {
    unsafe {
        let key_path = to_wide_null(key_path);
        let value_name = to_wide_null(value_name);
        let mut key: HKEY = null_mut();
        let mut disposition = 0u32;
        let create_status = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            key_path.as_ptr(),
            0,
            null_mut(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            null_mut(),
            &mut key,
            &mut disposition,
        );
        if create_status != ERROR_SUCCESS {
            return Err(create_status);
        }

        // SAFETY: RegistryPod has a fixed initialized representation and contains no references.
        let bytes = std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>());
        let set_status = RegSetValueExW(
            key,
            value_name.as_ptr(),
            0,
            REG_BINARY,
            bytes.as_ptr(),
            bytes.len() as u32,
        );
        let close_status = RegCloseKey(key);
        if set_status != ERROR_SUCCESS {
            Err(set_status)
        } else if close_status != ERROR_SUCCESS {
            Err(close_status)
        } else {
            Ok(())
        }
    }
}

fn modifiers_force_defaults() -> bool {
    // 安全性: `GetKeyState` only reads current keyboard state for virtual-key codes.
    unsafe {
        GetKeyState(i32::from(VK_SHIFT)) < 0
            && GetKeyState(i32::from(VK_MENU)) < 0
            && GetKeyState(i32::from(VK_CONTROL)) < 0
    }
}

fn window_rect_is_valid(
    rect: &RECT,
    min_width: i32,
    min_height: i32,
    max_width: i32,
    max_height: i32,
) -> bool {
    window_rect_dimensions_are_valid(rect, min_width, min_height, max_width, max_height)
        // 安全性: `rect` is a live RECT and MonitorFromRect only reads it during the call.
        && !unsafe { MonitorFromRect(rect, MONITOR_DEFAULTTONULL) }.is_null()
}

fn window_rect_dimensions_are_valid(
    rect: &RECT,
    min_width: i32,
    min_height: i32,
    max_width: i32,
    max_height: i32,
) -> bool {
    let width = i64::from(rect.right) - i64::from(rect.left);
    let height = i64::from(rect.bottom) - i64::from(rect.top);
    width >= i64::from(min_width.max(1))
        && height >= i64::from(min_height.max(1))
        && width <= i64::from(max_width.max(min_width).max(1))
        && height <= i64::from(max_height.max(min_height).max(1))
}

fn is_valid_view_mode(value: i32) -> bool {
    matches!(
        value,
        x if x == ViewMode::LargeIcon as i32
            || x == ViewMode::SmallIcon as i32
            || x == ViewMode::Details as i32
    )
}

fn is_valid_cpu_history_mode(value: i32) -> bool {
    matches!(
        value,
        x if x == CpuHistoryMode::Sum as i32 || x == CpuHistoryMode::Panes as i32
    )
}

fn is_valid_update_speed(value: i32) -> bool {
    UpdateSpeed::from_raw(value).is_some()
}

pub const fn update_speed_timer_interval(value: i32) -> Option<u32> {
    match UpdateSpeed::from_raw(value) {
        Some(update_speed) => Some(update_speed.timer_interval()),
        None => None,
    }
}

fn timer_interval_is_valid(update_speed: i32, timer_interval: u32) -> bool {
    let Some(expected) = update_speed_timer_interval(update_speed) else {
        return false;
    };

    timer_interval == expected || (timer_interval == 0 && expected != 0 && screen_reader_enabled())
}

fn process_columns_are_valid(
    columns: &[i32; NUM_COLUMN + 1],
    widths: &[i32; NUM_COLUMN + 1],
) -> bool {
    if columns[0] != ColumnId::ImageName as i32 {
        return false;
    }

    let mut seen = [false; NUM_COLUMN];
    let mut reached_end = false;
    for (&column, &width) in columns.iter().zip(widths.iter()) {
        if column == -1 {
            reached_end = true;
            if width != -1 {
                return false;
            }
            continue;
        }
        if reached_end || !(0..NUM_COLUMN as i32).contains(&column) || !(width == -1 || width > 0) {
            return false;
        }

        let index = column as usize;
        if seen[index] {
            return false;
        }
        seen[index] = true;
    }

    true
}

fn normalize_process_columns(
    columns: &mut [i32; NUM_COLUMN + 1],
    widths: &mut [i32; NUM_COLUMN + 1],
) {
    let original_columns = *columns;
    let original_widths = *widths;
    let mut seen = [false; NUM_COLUMN];
    let mut next_columns = [-1; NUM_COLUMN + 1];
    let mut next_widths = [-1; NUM_COLUMN + 1];

    next_columns[0] = ColumnId::ImageName as i32;
    seen[ColumnId::ImageName as usize] = true;
    next_widths[0] = original_columns
        .iter()
        .position(|column| *column == ColumnId::ImageName as i32)
        .and_then(|index| original_widths.get(index).copied())
        .filter(|width| *width > 0)
        .unwrap_or(-1);

    let mut next = 1usize;
    for (&column, &width) in original_columns.iter().zip(original_widths.iter()) {
        if column == -1 {
            break;
        }
        if !(0..NUM_COLUMN as i32).contains(&column) {
            continue;
        }
        let index = column as usize;
        if seen[index] {
            continue;
        }
        seen[index] = true;
        next_columns[next] = column;
        next_widths[next] = if width > 0 { width } else { -1 };
        next += 1;
    }

    *columns = next_columns;
    *widths = next_widths;
}

fn screen_reader_enabled() -> bool {
    let mut enabled = 0i32;
    // 安全性: `enabled` is a valid writable out parameter for `SPI_GETSCREENREADER`.
    unsafe {
        SystemParametersInfoW(
            SPI_GETSCREENREADER,
            0,
            &mut enabled as *mut i32 as *mut _,
            0,
        ) != 0
            && enabled != 0
    }
}

#[cfg(test)]
fn schema2_stored_bytes(schema: i32, page: i32) -> Vec<u8> {
    let current = Options::default();
    let old = OptionsV2 {
        cb_size: size_of::<OptionsV2>() as u32,
        timer_interval: current.timer_interval,
        view_mode: current.view_mode,
        cpu_history_mode: current.cpu_history_mode,
        update_speed: current.update_speed,
        window_rect: current.window_rect,
        current_page: page,
        active_process_columns: current.active_process_columns,
        column_widths: current.column_widths,
        flags: current.flags,
        unused: schema,
        unused2: 0,
    };
    let mut bytes = Vec::with_capacity(16 + size_of::<OptionsV2>());
    bytes.extend_from_slice(&OPTIONS_STORAGE_MAGIC);
    bytes.extend_from_slice(&OPTIONS_STORAGE_VERSION.to_le_bytes());
    bytes.extend_from_slice(&(size_of::<OptionsV2>() as u32).to_le_bytes());
    // SAFETY: OptionsV2 is RegistryPod. This copies the initialized value and does not retain the pointer.
    let raw = unsafe {
        std::slice::from_raw_parts(
            (&old as *const OptionsV2).cast::<u8>(),
            size_of::<OptionsV2>(),
        )
    };
    bytes.extend_from_slice(raw);
    bytes
}

#[cfg(test)]
mod tests {
    use super::{
        ColumnId, ERROR_INVALID_DATA, ERROR_REVISION_MISMATCH, LEGACY_OPTIONS_VALUE,
        LEGACY_TASKMAN_KEY, NUM_COLUMN, OPTIONS_KEY, OPTIONS_SCHEMA_VERSION, OPTIONS_STORAGE_MAGIC,
        OPTIONS_STORAGE_VERSION, OPTIONS_VALUE, Options, SCHEMA_0_NETWORK_PAGE,
        SCHEMA_0_USERS_PAGE, SCHEMA_1_GPU_PAGE, SCHEMA_1_NETWORK_PAGE, SCHEMA_1_USERS_PAGE,
        StoredOptions, UpdateSpeed, decode_stored_options, legacy_options_is_taskmgr_rs,
        normalize_process_columns, process_columns_are_valid, schema2_stored_bytes,
        update_speed_timer_interval, window_rect_dimensions_are_valid, window_rect_is_valid,
    };
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::RECT;

    #[test]
    fn stored_options_envelope_round_trips_and_rejects_foreign_data() {
        let options = Options::default();
        assert_eq!(
            StoredOptions::new(options).into_options().unwrap().unused,
            options.unused
        );

        let mut foreign = StoredOptions::new(options);
        foreign.magic = *b"NATIVE00";
        assert_eq!(foreign.into_options().err(), Some(ERROR_INVALID_DATA));

        let mut future = StoredOptions::new(options);
        future.storage_version = OPTIONS_STORAGE_VERSION + 1;
        assert_eq!(future.into_options().err(), Some(ERROR_REVISION_MISMATCH));

        let mut wrong_size = StoredOptions::new(options);
        wrong_size.payload_size = 0;
        assert_eq!(wrong_size.into_options().err(), Some(ERROR_INVALID_DATA));
        assert_eq!(OPTIONS_STORAGE_MAGIC, *b"TMGRRS01");
    }

    #[test]
    fn legacy_migration_requires_the_exact_taskmgr_rs_schema_marker() {
        let current = Options::default();
        assert!(legacy_options_is_taskmgr_rs(&current));

        for schema in [0, 1, OPTIONS_SCHEMA_VERSION + 1] {
            let candidate = Options {
                unused: schema,
                ..current
            };
            assert!(!legacy_options_is_taskmgr_rs(&candidate));
        }

        let wrong_size = Options {
            cb_size: 0,
            ..current
        };
        assert!(!legacy_options_is_taskmgr_rs(&wrong_size));
    }

    #[test]
    fn application_options_namespace_is_distinct_from_windows_task_manager() {
        assert_ne!(OPTIONS_KEY, LEGACY_TASKMAN_KEY);
        assert_ne!(OPTIONS_VALUE, LEGACY_OPTIONS_VALUE);
    }

    #[test]
    fn process_columns_reject_missing_primary_and_duplicates() {
        let mut columns = [-1; NUM_COLUMN + 1];
        let widths = [-1; NUM_COLUMN + 1];
        columns[0] = ColumnId::Pid as i32;
        assert!(!process_columns_are_valid(&columns, &widths));

        columns[0] = ColumnId::ImageName as i32;
        columns[1] = ColumnId::Pid as i32;
        columns[2] = ColumnId::Pid as i32;
        assert!(!process_columns_are_valid(&columns, &widths));
    }

    #[test]
    fn normalize_process_columns_restores_primary_and_sentinel_shape() {
        let mut columns = [-1; NUM_COLUMN + 1];
        let mut widths = [-1; NUM_COLUMN + 1];
        columns[0] = ColumnId::Pid as i32;
        widths[0] = 42;
        columns[1] = ColumnId::Pid as i32;
        widths[1] = 43;
        columns[2] = ColumnId::Cpu as i32;
        widths[2] = 0;
        columns[4] = ColumnId::ThreadCount as i32;
        widths[4] = 99;

        normalize_process_columns(&mut columns, &mut widths);

        assert_eq!(columns[0], ColumnId::ImageName as i32);
        assert_eq!(columns[1], ColumnId::Pid as i32);
        assert_eq!(columns[2], ColumnId::Cpu as i32);
        assert_eq!(columns[3], -1);
        assert_eq!(widths[0], -1);
        assert_eq!(widths[1], 42);
        assert_eq!(widths[2], -1);
        assert!(process_columns_are_valid(&columns, &widths));
    }

    #[test]
    fn default_options_have_valid_process_columns() {
        let options = Options::default();
        assert_eq!(options.unused, OPTIONS_SCHEMA_VERSION);
        assert_eq!(
            options.timer_interval,
            update_speed_timer_interval(UpdateSpeed::Normal as i32).unwrap()
        );
        assert!(options.process_columns_are_valid());
        assert!(
            options
                .extended_process_columns
                .iter()
                .all(|column| *column == -1)
        );
    }

    #[test]
    fn schema2_options_grow_into_schema3_without_reordering_saved_columns() {
        let mut options = decode_stored_options(&schema2_stored_bytes(2, 2)).unwrap();
        assert_eq!(options.migrate_schema(), Ok(true));
        assert_eq!(options.unused, OPTIONS_SCHEMA_VERSION);
        assert_eq!(options.cb_size, size_of::<Options>() as u32);
        assert_eq!(options.current_page, 2);
        assert_eq!(
            options.active_process_columns[0],
            ColumnId::ImageName as i32
        );
        assert_eq!(options.active_process_columns[1], ColumnId::Username as i32);
        assert_eq!(options.active_process_columns[4], ColumnId::MemUsage as i32);
        assert!(
            options
                .extended_process_columns
                .iter()
                .all(|column| *column == -1)
        );
        assert!(
            options
                .extended_column_widths
                .iter()
                .all(|width| *width == -1)
        );
        assert!(options.process_columns_are_valid());

        let mut network =
            decode_stored_options(&schema2_stored_bytes(0, SCHEMA_0_NETWORK_PAGE)).unwrap();
        assert_eq!(network.migrate_schema(), Ok(true));
        assert_eq!(network.current_page, 5);
        assert_eq!(network.unused, OPTIONS_SCHEMA_VERSION);
        assert_eq!(
            network.active_process_columns[0],
            ColumnId::ImageName as i32
        );
    }

    #[test]
    fn stored_options_reject_a_payload_size_from_neither_schema() {
        let mut bytes = schema2_stored_bytes(2, 0);
        bytes.push(0);
        let payload_size = (bytes.len() - 16) as u32;
        bytes[12..16].copy_from_slice(&payload_size.to_le_bytes());
        assert_eq!(
            decode_stored_options(&bytes).err(),
            Some(ERROR_INVALID_DATA)
        );
    }

    #[test]
    fn added_process_columns_keep_primary_duplicate_and_sentinel_rules() {
        let mut columns = [-1; NUM_COLUMN + 1];
        let mut widths = [-1; NUM_COLUMN + 1];
        columns[0] = ColumnId::ImageName as i32;
        columns[1] = ColumnId::CommandLine as i32;
        widths[1] = 180;
        columns[2] = ColumnId::IoReadBytes as i32;
        columns[3] = ColumnId::GdiObjects as i32;
        columns[4] = ColumnId::UserObjects as i32;
        assert!(process_columns_are_valid(&columns, &widths));

        columns[5] = ColumnId::CommandLine as i32;
        assert!(!process_columns_are_valid(&columns, &widths));

        columns[5] = NUM_COLUMN as i32;
        assert!(!process_columns_are_valid(&columns, &widths));

        columns[5] = -1;
        widths[5] = 8;
        assert!(!process_columns_are_valid(&columns, &widths));

        widths[5] = -1;
        widths[2] = 0;
        normalize_process_columns(&mut columns, &mut widths);
        assert_eq!(columns[0], ColumnId::ImageName as i32);
        assert_eq!(columns[1], ColumnId::CommandLine as i32);
        assert_eq!(columns[2], ColumnId::IoReadBytes as i32);
        assert_eq!(columns[3], ColumnId::GdiObjects as i32);
        assert_eq!(columns[4], ColumnId::UserObjects as i32);
        assert_eq!(columns[5], -1);
        assert_eq!(widths[1], 180);
        assert_eq!(widths[2], -1);
        assert!(process_columns_are_valid(&columns, &widths));
    }

    #[test]
    fn schema_zero_page_indices_migrate_across_gpu_and_cpu_insertions() {
        let mut network = Options {
            unused: 0,
            current_page: SCHEMA_0_NETWORK_PAGE,
            ..Options::default()
        };
        assert_eq!(network.migrate_schema(), Ok(true));
        assert_eq!(network.current_page, 5);

        let mut users = Options {
            unused: 0,
            current_page: SCHEMA_0_USERS_PAGE,
            ..Options::default()
        };
        assert_eq!(users.migrate_schema(), Ok(true));
        assert_eq!(users.current_page, 6);

        let mut performance = Options {
            unused: 0,
            current_page: 2,
            ..Options::default()
        };
        assert_eq!(performance.migrate_schema(), Ok(true));
        assert_eq!(performance.current_page, 2);
    }

    #[test]
    fn schema_one_page_indices_migrate_around_the_inserted_cpu_page() {
        for (old_page, expected_page) in [
            (SCHEMA_1_GPU_PAGE, 4),
            (SCHEMA_1_NETWORK_PAGE, 5),
            (SCHEMA_1_USERS_PAGE, 6),
        ] {
            let mut options = Options {
                unused: 1,
                current_page: old_page,
                ..Options::default()
            };
            assert_eq!(options.migrate_schema(), Ok(true));
            assert_eq!(options.current_page, expected_page);
            assert_eq!(options.unused, OPTIONS_SCHEMA_VERSION);
        }
    }

    #[test]
    fn current_and_unknown_options_schemas_are_not_guessed() {
        let mut current = Options::default();
        assert_eq!(current.migrate_schema(), Ok(false));

        let mut unknown = Options {
            unused: OPTIONS_SCHEMA_VERSION + 1,
            ..Options::default()
        };
        assert_eq!(unknown.migrate_schema(), Err(()));
        assert_eq!(unknown.unused, OPTIONS_SCHEMA_VERSION + 1);
    }

    #[test]
    fn update_speed_intervals_have_one_canonical_mapping() {
        assert_eq!(
            update_speed_timer_interval(UpdateSpeed::High as i32),
            Some(500)
        );
        assert_eq!(
            update_speed_timer_interval(UpdateSpeed::Normal as i32),
            Some(2_000)
        );
        assert_eq!(
            update_speed_timer_interval(UpdateSpeed::Low as i32),
            Some(4_000)
        );
        assert_eq!(
            update_speed_timer_interval(UpdateSpeed::Paused as i32),
            Some(0)
        );
        assert_eq!(update_speed_timer_interval(i32::MAX), None);
    }

    #[test]
    fn saved_window_rect_must_meet_the_minimum_size_without_overflowing() {
        assert!(window_rect_dimensions_are_valid(
            &RECT {
                left: 10,
                top: 20,
                right: 410,
                bottom: 320,
            },
            300,
            200,
            1920,
            1080,
        ));
        assert!(!window_rect_dimensions_are_valid(
            &RECT {
                left: 10,
                top: 20,
                right: 10,
                bottom: 320,
            },
            300,
            200,
            1920,
            1080,
        ));
        assert!(!window_rect_dimensions_are_valid(
            &RECT {
                left: i32::MIN,
                top: 0,
                right: i32::MAX,
                bottom: 300,
            },
            300,
            200,
            1920,
            1080,
        ));
    }

    #[test]
    fn saved_window_rect_must_intersect_an_attached_monitor() {
        assert!(!window_rect_is_valid(
            &RECT {
                left: i32::MAX - 400,
                top: i32::MAX - 300,
                right: i32::MAX,
                bottom: i32::MAX,
            },
            300,
            200,
            i32::MAX,
            i32::MAX,
        ));
    }
}

// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 启动项页纯模型
//
//   文件:       src/pages/startup/model.rs
//
//   日期:       2026年10月20日
//   环境:       Linux 6.12.94+ x86_64；Rust 1.99.1
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Startup-page decisions that do not touch the registry or the file system.
//!
//! An entry stays in its Run key or Startup folder. Enable and disable only describe the
//! 12-byte `StartupApproved` value Task Manager writes. A missing value means enabled. A blob
//! that is too short, or whose first dword is not one of the documented states, stays unknown
//! and cannot be reported as a successful change.

pub(crate) const APPROVED_DISABLED_BY_POLICY: u32 = 1;
pub(crate) const APPROVED_ENABLED: u32 = 2;
pub(crate) const APPROVED_DISABLED_BY_USER: u32 = 3;
pub(crate) const APPROVED_ENABLED_BY_USER: u32 = 6;
pub(crate) const APPROVED_BLOB_LEN: usize = 12;

pub(crate) const HKCU_RUN: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
pub(crate) const HKLM_RUN: &str = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run";
pub(crate) const HKLM_WOW_RUN: &str =
    "SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Run";
pub(crate) const HKCU_APPROVED_RUN: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run";
pub(crate) const HKLM_APPROVED_RUN: &str =
    "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run";
pub(crate) const HKLM_APPROVED_RUN32: &str =
    "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run32";
pub(crate) const HKCU_APPROVED_STARTUP_FOLDER: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\StartupFolder";
pub(crate) const HKLM_APPROVED_STARTUP_FOLDER: &str =
    "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\StartupFolder";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartupSource {
    CurrentUserRun,
    LocalMachineRun,
    LocalMachineWowRun,
    CurrentUserStartupFolder,
    CommonStartupFolder,
}

impl StartupSource {
    pub(crate) const ALL: [Self; 5] = [
        Self::CurrentUserRun,
        Self::LocalMachineRun,
        Self::LocalMachineWowRun,
        Self::CurrentUserStartupFolder,
        Self::CommonStartupFolder,
    ];

    pub(crate) const fn is_registry(self) -> bool {
        matches!(
            self,
            Self::CurrentUserRun | Self::LocalMachineRun | Self::LocalMachineWowRun
        )
    }

    pub(crate) const fn run_key(self) -> Option<&'static str> {
        match self {
            Self::CurrentUserRun => Some(HKCU_RUN),
            Self::LocalMachineRun => Some(HKLM_RUN),
            Self::LocalMachineWowRun => Some(HKLM_WOW_RUN),
            Self::CurrentUserStartupFolder | Self::CommonStartupFolder => None,
        }
    }

    pub(crate) const fn approved_key(self) -> &'static str {
        match self {
            Self::CurrentUserRun => HKCU_APPROVED_RUN,
            Self::LocalMachineRun => HKLM_APPROVED_RUN,
            Self::LocalMachineWowRun => HKLM_APPROVED_RUN32,
            Self::CurrentUserStartupFolder => HKCU_APPROVED_STARTUP_FOLDER,
            Self::CommonStartupFolder => HKLM_APPROVED_STARTUP_FOLDER,
        }
    }

    pub(crate) const fn location_label(self) -> Option<&'static str> {
        match self {
            Self::CurrentUserRun => Some("HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
            Self::LocalMachineRun => {
                Some("HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run")
            }
            Self::LocalMachineWowRun => {
                Some("HKLM\\SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Run")
            }
            Self::CurrentUserStartupFolder | Self::CommonStartupFolder => None,
        }
    }

    pub(crate) const fn uses_current_user_hive(self) -> bool {
        matches!(self, Self::CurrentUserRun | Self::CurrentUserStartupFolder)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StartupEntryId {
    pub(crate) source: StartupSource,
    pub(crate) name: String,
}

impl StartupEntryId {
    pub(crate) fn matches(&self, other: &Self) -> bool {
        self.source == other.source && self.name.eq_ignore_ascii_case(&other.name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartupEnabledState {
    Enabled,
    Disabled,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartupToggle {
    Enable,
    Disable,
}

impl StartupToggle {
    pub(crate) const fn enables(self) -> bool {
        matches!(self, Self::Enable)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StartupRequest {
    Refresh,
    SetEnabled {
        id: StartupEntryId,
        toggle: StartupToggle,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StartupCommandState {
    pub(crate) enable: bool,
    pub(crate) disable: bool,
}

impl StartupCommandState {
    pub(crate) const fn disabled() -> Self {
        Self {
            enable: false,
            disable: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartupScrollPolicy {
    PreserveViewport,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartupActionFailure {
    Win32(u32),
    Hresult(i32),
    Missing,
    StateUnchanged { observed: StartupEnabledState },
}

pub(crate) fn state_from_approved(bytes: Option<&[u8]>) -> StartupEnabledState {
    let Some(bytes) = bytes else {
        return StartupEnabledState::Enabled;
    };
    if bytes.len() < 4 {
        return StartupEnabledState::Unknown;
    }
    let flag = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    match flag {
        APPROVED_ENABLED | APPROVED_ENABLED_BY_USER => StartupEnabledState::Enabled,
        APPROVED_DISABLED_BY_POLICY | APPROVED_DISABLED_BY_USER => StartupEnabledState::Disabled,
        _ => StartupEnabledState::Unknown,
    }
}

pub(crate) fn approved_blob(
    toggle: StartupToggle,
    disabled_filetime_100ns: u64,
) -> [u8; APPROVED_BLOB_LEN] {
    let mut bytes = [0u8; APPROVED_BLOB_LEN];
    let flag = if toggle.enables() {
        APPROVED_ENABLED
    } else {
        APPROVED_DISABLED_BY_USER
    };
    bytes[..4].copy_from_slice(&flag.to_le_bytes());
    if !toggle.enables() {
        bytes[4..].copy_from_slice(&disabled_filetime_100ns.to_le_bytes());
    }
    bytes
}

pub(crate) fn state_matches_request(toggle: StartupToggle, state: StartupEnabledState) -> bool {
    match state {
        StartupEnabledState::Enabled => toggle.enables(),
        StartupEnabledState::Disabled => !toggle.enables(),
        StartupEnabledState::Unknown => false,
    }
}

pub(crate) fn classify_startup_result(
    toggle: StartupToggle,
    observed: Result<StartupEnabledState, u32>,
) -> Result<StartupEnabledState, StartupActionFailure> {
    match observed {
        Err(code) => Err(StartupActionFailure::Win32(code)),
        Ok(state) if state_matches_request(toggle, state) => Ok(state),
        Ok(state) => Err(StartupActionFailure::StateUnchanged { observed: state }),
    }
}

pub(crate) fn command_state(selected: Option<StartupEnabledState>) -> StartupCommandState {
    let Some(state) = selected else {
        return StartupCommandState::disabled();
    };
    StartupCommandState {
        enable: state != StartupEnabledState::Enabled,
        disable: state != StartupEnabledState::Disabled,
    }
}

/// Keeps a pending enable or disable ahead of a coalesced refresh.
pub(crate) fn merge_startup_requests(pending: &mut StartupRequest, incoming: StartupRequest) {
    match (&*pending, &incoming) {
        (StartupRequest::SetEnabled { .. }, StartupRequest::Refresh) => {}
        _ => *pending = incoming,
    }
}

pub(crate) const fn refresh_scroll_policy() -> StartupScrollPolicy {
    StartupScrollPolicy::PreserveViewport
}

pub(crate) fn retained_startup_id<'a>(
    previous: Option<&StartupEntryId>,
    ids: impl IntoIterator<Item = &'a StartupEntryId>,
) -> Option<&'a StartupEntryId> {
    let previous = previous?;
    if previous.name.is_empty() {
        return None;
    }
    ids.into_iter().find(|id| id.matches(previous))
}

pub(crate) fn display_name(source: StartupSource, stored_name: &str) -> String {
    if source.is_registry() {
        return stored_name.to_string();
    }
    strip_lnk_suffix(stored_name).to_string()
}

pub(crate) fn publisher_or_command(publisher: Option<&str>, command: &str) -> String {
    publisher
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| command.trim().to_string())
}

pub(crate) fn executable_candidate(command: &str) -> Option<String> {
    let command = command.trim();
    if command.is_empty() {
        return None;
    }
    if let Some(rest) = command.strip_prefix('"') {
        let end = rest.find('"')?;
        let path = rest[..end].trim();
        return (!path.is_empty()).then(|| path.to_string());
    }
    const EXTENSIONS: [&str; 6] = [".exe", ".dll", ".bat", ".cmd", ".com", ".scr"];
    let lower = command.to_ascii_lowercase();
    let mut search_from = 0usize;
    while search_from < lower.len() {
        let Some(relative) = EXTENSIONS
            .iter()
            .filter_map(|extension| {
                lower[search_from..]
                    .find(extension)
                    .map(|index| (index, extension.len()))
            })
            .min_by_key(|(index, _)| *index)
        else {
            break;
        };
        let end = search_from + relative.0 + relative.1;
        let boundary_ok = lower
            .as_bytes()
            .get(end)
            .is_none_or(|byte| byte.is_ascii_whitespace() || *byte == b'"' || *byte == b',');
        if boundary_ok {
            let path = command[..end].trim();
            return (!path.is_empty()).then(|| path.to_string());
        }
        search_from = end;
    }
    command
        .split_whitespace()
        .next()
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

pub(crate) fn is_absolute_win32_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    drive || path.starts_with("\\\\")
}

pub(crate) fn startup_file_name_is_safe(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['\\', '/', ':', '\0'])
        && !name.ends_with('.')
        && !name.ends_with(' ')
}

pub(crate) fn registry_value_name_is_safe(name: &str) -> bool {
    !name.is_empty() && !name.contains('\0') && name.len() <= 16_383
}

pub(crate) fn has_version_resource_extension(path: &str) -> bool {
    let file_name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    [".exe", ".dll", ".bat", ".cmd", ".com", ".scr"]
        .iter()
        .any(|extension| {
            file_name.len() > extension.len()
                && file_name
                    .get(file_name.len() - extension.len()..)
                    .is_some_and(|suffix| suffix.eq_ignore_ascii_case(extension))
        })
}

fn strip_lnk_suffix(name: &str) -> &str {
    let Some(stem) = name.get(..name.len().saturating_sub(4)) else {
        return name;
    };
    if name[stem.len()..].eq_ignore_ascii_case(".lnk") {
        stem
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(source: StartupSource, name: &str) -> StartupEntryId {
        StartupEntryId {
            source,
            name: name.to_string(),
        }
    }

    #[test]
    fn approved_bytes_keep_unknown_distinct_from_enabled_and_disabled() {
        assert_eq!(state_from_approved(None), StartupEnabledState::Enabled);
        assert_eq!(state_from_approved(Some(&[])), StartupEnabledState::Unknown);
        assert_eq!(
            state_from_approved(Some(&[0x02, 0x00, 0x00])),
            StartupEnabledState::Unknown
        );
        assert_eq!(
            state_from_approved(Some(&approved_blob(StartupToggle::Enable, 99))),
            StartupEnabledState::Enabled
        );
        let disabled = approved_blob(StartupToggle::Disable, 0x01D0_1234_5678_9ABC);
        assert_eq!(
            state_from_approved(Some(&disabled)),
            StartupEnabledState::Disabled
        );
        assert_eq!(&disabled[4..], &0x01D0_1234_5678_9ABCu64.to_le_bytes());
        assert_eq!(
            state_from_approved(Some(&[0x06, 0x00, 0x00, 0x00])),
            StartupEnabledState::Enabled
        );
        assert_eq!(
            state_from_approved(Some(&[0x01, 0x00, 0x00, 0x00])),
            StartupEnabledState::Disabled
        );
        assert_eq!(
            state_from_approved(Some(&[0x00, 0x00, 0x00, 0x00])),
            StartupEnabledState::Unknown
        );
        assert_eq!(approved_blob(StartupToggle::Enable, 7)[4..], [0; 8]);
    }

    #[test]
    fn a_change_is_successful_only_when_the_observed_state_matches() {
        assert_eq!(
            classify_startup_result(StartupToggle::Enable, Ok(StartupEnabledState::Enabled)),
            Ok(StartupEnabledState::Enabled)
        );
        assert_eq!(
            classify_startup_result(StartupToggle::Disable, Ok(StartupEnabledState::Disabled)),
            Ok(StartupEnabledState::Disabled)
        );
        assert_eq!(
            classify_startup_result(StartupToggle::Enable, Ok(StartupEnabledState::Disabled)),
            Err(StartupActionFailure::StateUnchanged {
                observed: StartupEnabledState::Disabled
            })
        );
        assert_eq!(
            classify_startup_result(StartupToggle::Disable, Ok(StartupEnabledState::Unknown)),
            Err(StartupActionFailure::StateUnchanged {
                observed: StartupEnabledState::Unknown
            })
        );
        assert_eq!(
            classify_startup_result(StartupToggle::Enable, Err(5)),
            Err(StartupActionFailure::Win32(5))
        );
    }

    #[test]
    fn commands_follow_the_selected_state() {
        assert_eq!(command_state(None), StartupCommandState::disabled());
        assert_eq!(
            command_state(Some(StartupEnabledState::Enabled)),
            StartupCommandState {
                enable: false,
                disable: true
            }
        );
        assert_eq!(
            command_state(Some(StartupEnabledState::Disabled)),
            StartupCommandState {
                enable: true,
                disable: false
            }
        );
        assert_eq!(
            command_state(Some(StartupEnabledState::Unknown)),
            StartupCommandState {
                enable: true,
                disable: true
            }
        );
    }

    #[test]
    fn pending_toggle_survives_a_coalesced_refresh() {
        let original = StartupRequest::SetEnabled {
            id: id(StartupSource::CurrentUserRun, "OneDrive"),
            toggle: StartupToggle::Disable,
        };
        let mut pending = original.clone();
        merge_startup_requests(&mut pending, StartupRequest::Refresh);
        assert_eq!(pending, original);

        merge_startup_requests(
            &mut pending,
            StartupRequest::SetEnabled {
                id: id(StartupSource::LocalMachineRun, "SecurityHealth"),
                toggle: StartupToggle::Enable,
            },
        );
        assert!(matches!(
            pending,
            StartupRequest::SetEnabled {
                toggle: StartupToggle::Enable,
                ..
            }
        ));
    }

    #[test]
    fn selection_keeps_the_same_source_and_name_across_refresh() {
        let previous = id(StartupSource::LocalMachineRun, "SecurityHealth");
        let rows = [
            id(StartupSource::CurrentUserRun, "SecurityHealth"),
            id(StartupSource::LocalMachineRun, "securityhealth"),
        ];
        let retained = retained_startup_id(Some(&previous), &rows).unwrap();
        assert!(retained.matches(&rows[1]));
        assert!(retained_startup_id(Some(&id(StartupSource::CurrentUserRun, "")), &rows).is_none());
        assert!(retained_startup_id(None, &rows).is_none());
    }

    #[test]
    fn publisher_falls_back_to_the_command_and_paths_parse() {
        assert_eq!(
            publisher_or_command(Some("  Contoso  "), "ignored.exe"),
            "Contoso"
        );
        assert_eq!(
            publisher_or_command(Some("   "), "  C:\\App\\app.exe /s  "),
            "C:\\App\\app.exe /s"
        );
        assert_eq!(
            executable_candidate("\"C:\\Program Files\\App\\app.exe\" --flag"),
            Some("C:\\Program Files\\App\\app.exe".to_string())
        );
        assert_eq!(
            executable_candidate("C:\\Program Files\\App\\app.exe /silent"),
            Some("C:\\Program Files\\App\\app.exe".to_string())
        );
        assert_eq!(
            executable_candidate("rundll32.exe C:\\path\\plugin.dll,Entry"),
            Some("rundll32.exe".to_string())
        );
        assert_eq!(executable_candidate("\"\""), None);
        assert_eq!(executable_candidate("   "), None);
        assert_eq!(executable_candidate("notepad"), Some("notepad".to_string()));
        assert!(is_absolute_win32_path("C:\\Windows\\notepad.exe"));
        assert!(is_absolute_win32_path("\\\\server\\share\\app.exe"));
        assert!(!is_absolute_win32_path("notepad.exe"));
        assert!(!is_absolute_win32_path("\\Windows\\notepad.exe"));
    }

    #[test]
    fn shortcut_display_names_drop_only_a_trailing_lnk_suffix() {
        assert_eq!(
            display_name(StartupSource::CurrentUserStartupFolder, "OneDrive.lnk"),
            "OneDrive"
        );
        assert_eq!(
            display_name(StartupSource::CommonStartupFolder, "Widget.LNK"),
            "Widget"
        );
        assert_eq!(
            display_name(StartupSource::CurrentUserRun, "OneDrive.lnk"),
            "OneDrive.lnk"
        );
        assert_eq!(
            display_name(StartupSource::CurrentUserStartupFolder, "note.lnk.txt"),
            "note.lnk.txt"
        );
        assert!(startup_file_name_is_safe("OneDrive.lnk"));
        assert!(!startup_file_name_is_safe("..\\OneDrive.lnk"));
        assert!(!startup_file_name_is_safe(""));
        assert!(!startup_file_name_is_safe("OneDrive.lnk "));
        assert!(registry_value_name_is_safe("Security Health"));
        assert!(!registry_value_name_is_safe(""));
        assert!(!registry_value_name_is_safe("bad\0name"));
        assert!(has_version_resource_extension(
            "C:\\Program Files\\App\\app.exe"
        ));
        assert!(!has_version_resource_extension(
            "C:\\Users\\Public\\Startup\\Widget.lnk"
        ));
    }
}

// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 服务页纯模型
//
//   文件:       src/pages/services/model.rs
//
//   日期:       2026年10月19日
//   环境:       Linux 6.12.94+ x86_64；Rust 1.99.1
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Service-page decisions that do not touch the Service Control Manager.
//!
//! Startup and status values use the documented SCM constants. A process is actionable only when
//! its `ProcIdentity` carries a non-zero creation time. PID-only values stay invisible and cannot
//! enable Go to Process.

use crate::system::process_identity::ProcIdentity;

pub(crate) const SERVICE_STOPPED: u32 = 1;
pub(crate) const SERVICE_START_PENDING: u32 = 2;
pub(crate) const SERVICE_STOP_PENDING: u32 = 3;
pub(crate) const SERVICE_RUNNING: u32 = 4;
pub(crate) const SERVICE_CONTINUE_PENDING: u32 = 5;
pub(crate) const SERVICE_PAUSE_PENDING: u32 = 6;
pub(crate) const SERVICE_PAUSED: u32 = 7;

pub(crate) const SERVICE_BOOT_START: u32 = 0;
pub(crate) const SERVICE_SYSTEM_START: u32 = 1;
pub(crate) const SERVICE_AUTO_START: u32 = 2;
pub(crate) const SERVICE_DEMAND_START: u32 = 3;
pub(crate) const SERVICE_DISABLED: u32 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServiceRunState {
    Stopped,
    StartPending,
    StopPending,
    Running,
    ContinuePending,
    PausePending,
    Paused,
    Unknown(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServiceStartupType {
    Boot,
    System,
    Automatic,
    Manual,
    Disabled,
    Unknown(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServiceControlAction {
    Start,
    Stop,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ServiceRequest {
    Refresh,
    Control {
        service_name: String,
        action: ServiceControlAction,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ServiceCommandState {
    pub(crate) start: bool,
    pub(crate) stop: bool,
    pub(crate) go_to_process: bool,
}

impl ServiceCommandState {
    pub(crate) const fn disabled() -> Self {
        Self {
            start: false,
            stop: false,
            go_to_process: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ServiceCommandInput {
    pub(crate) status: ServiceRunState,
    pub(crate) startup: ServiceStartupType,
    pub(crate) process: Option<ProcIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OwningProcess {
    pub(crate) identity: ProcIdentity,
    pub(crate) image_name: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServiceScrollPolicy {
    PreserveViewport,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ServiceActionFailure {
    Api(u32),
    StateUnchanged { observed: ServiceRunState },
}

pub(crate) fn state_from_scm(value: u32) -> ServiceRunState {
    match value {
        SERVICE_STOPPED => ServiceRunState::Stopped,
        SERVICE_START_PENDING => ServiceRunState::StartPending,
        SERVICE_STOP_PENDING => ServiceRunState::StopPending,
        SERVICE_RUNNING => ServiceRunState::Running,
        SERVICE_CONTINUE_PENDING => ServiceRunState::ContinuePending,
        SERVICE_PAUSE_PENDING => ServiceRunState::PausePending,
        SERVICE_PAUSED => ServiceRunState::Paused,
        other => ServiceRunState::Unknown(other),
    }
}

pub(crate) fn startup_from_scm(value: u32) -> ServiceStartupType {
    match value {
        SERVICE_BOOT_START => ServiceStartupType::Boot,
        SERVICE_SYSTEM_START => ServiceStartupType::System,
        SERVICE_AUTO_START => ServiceStartupType::Automatic,
        SERVICE_DEMAND_START => ServiceStartupType::Manual,
        SERVICE_DISABLED => ServiceStartupType::Disabled,
        other => ServiceStartupType::Unknown(other),
    }
}

pub(crate) fn command_state(selected: Option<ServiceCommandInput>) -> ServiceCommandState {
    let Some(selected) = selected else {
        return ServiceCommandState::disabled();
    };
    ServiceCommandState {
        start: selected.status == ServiceRunState::Stopped
            && selected.startup != ServiceStartupType::Disabled,
        stop: matches!(
            selected.status,
            ServiceRunState::Running | ServiceRunState::Paused
        ),
        go_to_process: selected.process.is_some_and(ProcIdentity::is_verified),
    }
}

pub(crate) fn control_reached_requested_state(
    action: ServiceControlAction,
    state: ServiceRunState,
) -> bool {
    match action {
        ServiceControlAction::Start => matches!(
            state,
            ServiceRunState::Running | ServiceRunState::StartPending
        ),
        ServiceControlAction::Stop => matches!(
            state,
            ServiceRunState::Stopped | ServiceRunState::StopPending
        ),
    }
}

pub(crate) fn classify_control_result(
    action: ServiceControlAction,
    api_result: Result<ServiceRunState, u32>,
) -> Result<ServiceRunState, ServiceActionFailure> {
    match api_result {
        Err(code) => Err(ServiceActionFailure::Api(code)),
        Ok(state) if control_reached_requested_state(action, state) => Ok(state),
        Ok(state) => Err(ServiceActionFailure::StateUnchanged { observed: state }),
    }
}

/// Keeps a pending start or stop ahead of a coalesced refresh.
pub(crate) fn merge_service_requests(pending: &mut ServiceRequest, incoming: ServiceRequest) {
    match (&*pending, &incoming) {
        (ServiceRequest::Control { .. }, ServiceRequest::Refresh) => {}
        _ => *pending = incoming,
    }
}

pub(crate) const fn refresh_scroll_policy() -> ServiceScrollPolicy {
    ServiceScrollPolicy::PreserveViewport
}

pub(crate) fn retained_service_name<'a>(
    previous: Option<&str>,
    names: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    let previous = previous?;
    if previous.is_empty() {
        return None;
    }
    names
        .into_iter()
        .find(|name| name.eq_ignore_ascii_case(previous))
}

pub(crate) fn file_name_from_win32_path(path: &str) -> &str {
    path.rsplit(['\\', '/'])
        .find(|part| !part.is_empty())
        .unwrap_or(path)
}

pub(crate) fn owning_process_label(process: Option<&OwningProcess>, pid_label: &str) -> String {
    let Some(process) = process else {
        return String::new();
    };
    if !process.identity.is_verified() {
        return String::new();
    }
    if let Some(name) = process
        .image_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        return file_name_from_win32_path(name).to_string();
    }
    format!("{pid_label} {}", process.identity.pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(
        status: ServiceRunState,
        startup: ServiceStartupType,
        process: Option<ProcIdentity>,
    ) -> ServiceCommandInput {
        ServiceCommandInput {
            status,
            startup,
            process,
        }
    }

    #[test]
    fn documented_scm_values_map_without_collapsing_unknowns() {
        assert_eq!(state_from_scm(SERVICE_STOPPED), ServiceRunState::Stopped);
        assert_eq!(state_from_scm(SERVICE_RUNNING), ServiceRunState::Running);
        assert_eq!(state_from_scm(SERVICE_PAUSED), ServiceRunState::Paused);
        assert_eq!(state_from_scm(0), ServiceRunState::Unknown(0));
        assert_eq!(state_from_scm(99), ServiceRunState::Unknown(99));

        assert_eq!(
            startup_from_scm(SERVICE_BOOT_START),
            ServiceStartupType::Boot
        );
        assert_eq!(
            startup_from_scm(SERVICE_DEMAND_START),
            ServiceStartupType::Manual
        );
        assert_eq!(
            startup_from_scm(SERVICE_DISABLED),
            ServiceStartupType::Disabled
        );
        assert_eq!(startup_from_scm(9), ServiceStartupType::Unknown(9));
    }

    #[test]
    fn commands_require_a_real_selection_and_a_verified_process() {
        assert_eq!(command_state(None), ServiceCommandState::disabled());

        let stopped = command_state(Some(input(
            ServiceRunState::Stopped,
            ServiceStartupType::Manual,
            None,
        )));
        assert!(stopped.start);
        assert!(!stopped.stop);
        assert!(!stopped.go_to_process);

        let disabled = command_state(Some(input(
            ServiceRunState::Stopped,
            ServiceStartupType::Disabled,
            None,
        )));
        assert!(!disabled.start);

        let running = command_state(Some(input(
            ServiceRunState::Running,
            ServiceStartupType::Automatic,
            Some(ProcIdentity::pid_only(44)),
        )));
        assert!(!running.start);
        assert!(running.stop);
        assert!(!running.go_to_process);

        let verified = command_state(Some(input(
            ServiceRunState::Paused,
            ServiceStartupType::Automatic,
            Some(ProcIdentity::new(44, 100)),
        )));
        assert!(verified.stop);
        assert!(verified.go_to_process);

        let pending = command_state(Some(input(
            ServiceRunState::StartPending,
            ServiceStartupType::Automatic,
            Some(ProcIdentity::new(44, 100)),
        )));
        assert!(!pending.start);
        assert!(!pending.stop);
        assert!(pending.go_to_process);
    }

    #[test]
    fn control_success_requires_the_requested_state() {
        assert!(control_reached_requested_state(
            ServiceControlAction::Start,
            ServiceRunState::StartPending
        ));
        assert!(control_reached_requested_state(
            ServiceControlAction::Start,
            ServiceRunState::Running
        ));
        assert!(!control_reached_requested_state(
            ServiceControlAction::Start,
            ServiceRunState::Stopped
        ));
        assert!(control_reached_requested_state(
            ServiceControlAction::Stop,
            ServiceRunState::StopPending
        ));
        assert!(!control_reached_requested_state(
            ServiceControlAction::Stop,
            ServiceRunState::Running
        ));

        assert_eq!(
            classify_control_result(ServiceControlAction::Start, Err(5)),
            Err(ServiceActionFailure::Api(5))
        );
        assert_eq!(
            classify_control_result(ServiceControlAction::Start, Ok(ServiceRunState::Stopped)),
            Err(ServiceActionFailure::StateUnchanged {
                observed: ServiceRunState::Stopped
            })
        );
        assert_eq!(
            classify_control_result(ServiceControlAction::Stop, Ok(ServiceRunState::Stopped)),
            Ok(ServiceRunState::Stopped)
        );
    }

    #[test]
    fn refresh_does_not_replace_a_pending_control() {
        let mut pending = ServiceRequest::Control {
            service_name: "wuauserv".to_string(),
            action: ServiceControlAction::Start,
        };
        merge_service_requests(&mut pending, ServiceRequest::Refresh);
        assert_eq!(
            pending,
            ServiceRequest::Control {
                service_name: "wuauserv".to_string(),
                action: ServiceControlAction::Start,
            }
        );

        merge_service_requests(
            &mut pending,
            ServiceRequest::Control {
                service_name: "wuauserv".to_string(),
                action: ServiceControlAction::Stop,
            },
        );
        assert_eq!(
            pending,
            ServiceRequest::Control {
                service_name: "wuauserv".to_string(),
                action: ServiceControlAction::Stop,
            }
        );

        let mut refresh = ServiceRequest::Refresh;
        merge_service_requests(
            &mut refresh,
            ServiceRequest::Control {
                service_name: "eventlog".to_string(),
                action: ServiceControlAction::Start,
            },
        );
        assert!(matches!(refresh, ServiceRequest::Control { .. }));
    }

    #[test]
    fn selection_and_scroll_survive_a_refresh() {
        assert_eq!(
            refresh_scroll_policy(),
            ServiceScrollPolicy::PreserveViewport
        );
        let names = ["EventLog", "wuauserv"];
        assert_eq!(
            retained_service_name(Some("WUAUSERV"), names),
            Some("wuauserv")
        );
        assert_eq!(retained_service_name(Some("gone"), names), None);
        assert_eq!(retained_service_name(None, names), None);
        assert_eq!(retained_service_name(Some(""), names), None);
    }

    #[test]
    fn owning_process_text_hides_unverified_identities() {
        assert_eq!(owning_process_label(None, "PID"), "");
        let pid_only = OwningProcess {
            identity: ProcIdentity::pid_only(90),
            image_name: Some(r"C:\Windows\System32\svchost.exe".to_string()),
        };
        assert_eq!(owning_process_label(Some(&pid_only), "PID"), "");

        let named = OwningProcess {
            identity: ProcIdentity::new(90, 7),
            image_name: Some(r"C:\Windows\System32\svchost.exe".to_string()),
        };
        assert_eq!(owning_process_label(Some(&named), "PID"), "svchost.exe");

        let unnamed = OwningProcess {
            identity: ProcIdentity::new(90, 7),
            image_name: None,
        };
        assert_eq!(owning_process_label(Some(&unnamed), "PID"), "PID 90");
    }
}

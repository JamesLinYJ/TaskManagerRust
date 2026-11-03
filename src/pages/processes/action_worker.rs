// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 进程结束、优先级与亲和性
//
//   文件:       src/pages/processes/action_worker.rs
//
//   日期:       2026年11月03日
//   环境:       Ubuntu 24.04.5 LTS x86_64；Linux 内核 6.12.94+；Rust 1.99.1
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! End Process, End Process Tree, Set Priority, and Set Affinity after the UI
//! confirmation.
//!
//! One persistent [`SingleFlightWorker`] owns every confirmed action. The page
//! posts no extra window message: completion reuses `PWM_PROC_WORKER_COMPLETE`
//! and is drained beside the process sampler. Dropping the worker retires it
//! without joining on the UI thread, including while a tree walk or an affinity
//! update is still inside the collector.
//!
//! Each action re-opens its target with `open_process_for_identity` and requires
//! both the pid and `creation_time_100ns`. An unknown or mismatched identity
//! returns before `TerminateProcess`, the tree walk, `SetPriorityClass`,
//! `SetProcessAffinityMask`, or `SetProcessDefaultCpuSets`. A group or mask that
//! the native `KAFFINITY` cannot express fails before those calls.

#[cfg(test)]
use std::sync::mpsc::{Receiver, Sender};

use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, GetLastError};
use windows_sys::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS,
    IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SET_INFORMATION, PROCESS_TERMINATE, REALTIME_PRIORITY_CLASS, SetPriorityClass,
    TerminateProcess,
};

use super::ProcPriority;
use super::actions::{
    ProcessTreePrepareError, ProcessTreeTerminationOutcome, apply_confirmed_affinity,
    prepare_process_tree_termination, terminate_prepared_process_tree,
};
use crate::infrastructure::native::OwnedHandle;
use crate::infrastructure::worker::SingleFlightWorker;
use crate::system::process_identity::{ProcIdentity, open_process_for_identity};
use crate::ui::resource_ids::PWM_PROC_WORKER_COMPLETE;

pub(super) type ProcessActionWorker =
    SingleFlightWorker<Vec<ProcessAction>, Vec<ProcessActionReport>>;

/// Group-qualified affinity bits chosen in the dialog.
///
/// `mask` is the full group mask. A later conversion that would keep only the
/// low bits of `KAFFINITY` is rejected instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AffinityGroupMask {
    pub(super) group: u16,
    pub(super) mask: u64,
}

pub(super) enum ProcessAction {
    End(ProcIdentity),
    EndTree(ProcIdentity),
    SetPriority {
        identity: ProcIdentity,
        priority: ProcPriority,
    },
    SetAffinity {
        identity: ProcIdentity,
        masks: Vec<AffinityGroupMask>,
    },
    #[cfg(test)]
    Block {
        entered: Sender<()>,
        release: Receiver<()>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ProcessActionReport {
    /// Unpause and refresh. Set when a terminate or priority change landed,
    /// including a tree that ended only in part.
    pub(super) refresh: bool,
    pub(super) feedback: ProcessActionFeedback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProcessActionFeedback {
    None,
    CantKill(u32),
    CantChangePriority(u32),
    CantSetAffinity(u32),
    /// `show_failure_message` with the existing tree-failure body.
    KillTreePrepare(u32),
    /// Existing warning dialog. This is not a full success.
    PartialTree,
}

pub(super) fn spawn_process_action_worker() -> Result<ProcessActionWorker, u32> {
    // Asynchronous retirement closes completion posts and does not join a worker
    // that is still inside TerminateProcess, Toolhelp, SetPriorityClass, or an
    // affinity update.
    SingleFlightWorker::spawn_cancellable_initialized(
        "taskmgr-rs-process-actions",
        PWM_PROC_WORKER_COMPLETE,
        append_process_actions,
        |_stop| |actions| actions.into_iter().map(run_process_action).collect(),
    )
}

pub(super) fn append_process_actions(
    pending: &mut Vec<ProcessAction>,
    incoming: Vec<ProcessAction>,
) {
    pending.extend(incoming);
}

pub(super) fn require_action_identity(identity: ProcIdentity) -> Result<(), u32> {
    if identity.pid != 0 && identity.is_verified() {
        Ok(())
    } else {
        Err(ERROR_INVALID_PARAMETER)
    }
}

pub(super) fn report_end_tree_outcome(
    outcome: ProcessTreeTerminationOutcome,
) -> ProcessActionReport {
    let feedback = if outcome.root_error != 0 && !outcome.any_success() {
        ProcessActionFeedback::CantKill(outcome.root_error)
    } else if outcome.any_failure() {
        ProcessActionFeedback::PartialTree
    } else {
        ProcessActionFeedback::None
    };
    ProcessActionReport {
        refresh: outcome.any_completed,
        feedback,
    }
}

pub(super) fn report_end_tree_prepare_error(error: ProcessTreePrepareError) -> ProcessActionReport {
    match error {
        ProcessTreePrepareError::Root(error) => kill_failure(error),
        ProcessTreePrepareError::Tree(error) => ProcessActionReport {
            refresh: false,
            feedback: ProcessActionFeedback::KillTreePrepare(error),
        },
    }
}

fn run_process_action(action: ProcessAction) -> ProcessActionReport {
    match action {
        ProcessAction::End(identity) => execute_end(identity),
        ProcessAction::EndTree(identity) => execute_end_tree(identity),
        ProcessAction::SetPriority { identity, priority } => {
            execute_set_priority(identity, priority)
        }
        ProcessAction::SetAffinity { identity, masks } => execute_set_affinity(identity, masks),
        #[cfg(test)]
        ProcessAction::Block { entered, release } => {
            let _ = entered.send(());
            let _ = release.recv();
            ProcessActionReport {
                refresh: false,
                feedback: ProcessActionFeedback::None,
            }
        }
    }
}

fn execute_end(identity: ProcIdentity) -> ProcessActionReport {
    let handle = match open_action_process(
        identity,
        PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
    ) {
        Ok(handle) => handle,
        Err(error) => return kill_failure(error),
    };
    // SAFETY: `open_process_for_identity` returned a live handle with PROCESS_TERMINATE
    // access for this pid and creation time. The handle is owned for this call.
    if unsafe { TerminateProcess(handle.as_raw(), 1) } == 0 {
        let error = unsafe { GetLastError() };
        kill_failure(error)
    } else {
        succeeded()
    }
}

fn execute_end_tree(identity: ProcIdentity) -> ProcessActionReport {
    // Unknown identities never reach the tree walk. `prepare_process_tree_termination`
    // re-opens the root before it enumerates children or calls TerminateProcess.
    if let Err(error) = require_action_identity(identity) {
        return kill_failure(error);
    }
    match prepare_process_tree_termination(identity) {
        Ok(prepared) => report_end_tree_outcome(terminate_prepared_process_tree(prepared)),
        Err(error) => report_end_tree_prepare_error(error),
    }
}

fn execute_set_affinity(
    identity: ProcIdentity,
    masks: Vec<AffinityGroupMask>,
) -> ProcessActionReport {
    // Reject an unknown identity before `apply_confirmed_affinity` opens the
    // process and reaches either affinity API.
    if let Err(error) = require_action_identity(identity) {
        return affinity_failure(error);
    }
    match apply_confirmed_affinity(identity, &masks) {
        Ok(()) => succeeded(),
        Err(error) => affinity_failure(error),
    }
}

fn execute_set_priority(identity: ProcIdentity, priority: ProcPriority) -> ProcessActionReport {
    let handle = match open_action_process(
        identity,
        PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION,
    ) {
        Ok(handle) => handle,
        Err(error) => return priority_failure(error),
    };
    let priority_class = priority_class(priority);
    // SAFETY: the identity-validated handle has PROCESS_SET_INFORMATION access.
    if unsafe { SetPriorityClass(handle.as_raw(), priority_class) } == 0 {
        let error = unsafe { GetLastError() };
        priority_failure(error)
    } else {
        succeeded()
    }
}

fn open_action_process(identity: ProcIdentity, access: u32) -> Result<OwnedHandle, u32> {
    require_action_identity(identity)?;
    open_process_for_identity(identity, access)
}

fn priority_class(priority: ProcPriority) -> u32 {
    match priority {
        ProcPriority::Low => IDLE_PRIORITY_CLASS,
        ProcPriority::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
        ProcPriority::Normal => NORMAL_PRIORITY_CLASS,
        ProcPriority::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
        ProcPriority::High => HIGH_PRIORITY_CLASS,
        ProcPriority::Realtime => REALTIME_PRIORITY_CLASS,
    }
}

const fn kill_failure(error: u32) -> ProcessActionReport {
    ProcessActionReport {
        refresh: false,
        feedback: ProcessActionFeedback::CantKill(error),
    }
}

const fn priority_failure(error: u32) -> ProcessActionReport {
    ProcessActionReport {
        refresh: false,
        feedback: ProcessActionFeedback::CantChangePriority(error),
    }
}

const fn affinity_failure(error: u32) -> ProcessActionReport {
    ProcessActionReport {
        refresh: false,
        feedback: ProcessActionFeedback::CantSetAffinity(error),
    }
}

const fn succeeded() -> ProcessActionReport {
    ProcessActionReport {
        refresh: true,
        feedback: ProcessActionFeedback::None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::actions::ProcessTreePrepareError;
    use super::{
        AffinityGroupMask, ProcessAction, ProcessActionFeedback, ProcessTreeTerminationOutcome,
        affinity_failure, append_process_actions, report_end_tree_outcome,
        report_end_tree_prepare_error, require_action_identity, run_process_action,
        spawn_process_action_worker,
    };
    use crate::system::process_identity::ProcIdentity;
    use std::ptr::null_mut;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;

    #[test]
    fn unknown_identity_is_rejected_before_an_action() {
        assert_eq!(
            require_action_identity(ProcIdentity::pid_only(4)),
            Err(ERROR_INVALID_PARAMETER)
        );
        assert_eq!(
            require_action_identity(ProcIdentity::new(0, 100)),
            Err(ERROR_INVALID_PARAMETER)
        );
        assert!(require_action_identity(ProcIdentity::new(4, 100)).is_ok());
    }

    #[test]
    fn unverified_actions_fail_without_a_refresh() {
        for action in [
            ProcessAction::End(ProcIdentity::pid_only(4)),
            ProcessAction::EndTree(ProcIdentity::pid_only(8)),
            ProcessAction::SetPriority {
                identity: ProcIdentity::new(0, 50),
                priority: super::super::ProcPriority::High,
            },
            ProcessAction::SetAffinity {
                identity: ProcIdentity::pid_only(12),
                masks: vec![AffinityGroupMask {
                    group: 0,
                    mask: 0b1111,
                }],
            },
        ] {
            let report = run_process_action(action);
            assert!(!report.refresh);
            assert_ne!(report.feedback, ProcessActionFeedback::None);
            assert_ne!(report.feedback, ProcessActionFeedback::PartialTree);
        }
    }

    #[test]
    fn partial_tree_is_not_full_success() {
        let partial = report_end_tree_outcome(ProcessTreeTerminationOutcome {
            any_success: true,
            any_completed: true,
            any_failure: true,
            root_error: 5,
        });
        assert!(partial.refresh);
        assert_eq!(partial.feedback, ProcessActionFeedback::PartialTree);

        let root_only = report_end_tree_outcome(ProcessTreeTerminationOutcome {
            any_success: false,
            any_completed: true,
            any_failure: true,
            root_error: 5,
        });
        assert!(root_only.refresh);
        assert_eq!(root_only.feedback, ProcessActionFeedback::CantKill(5));

        let succeeded = report_end_tree_outcome(ProcessTreeTerminationOutcome {
            any_success: true,
            any_completed: true,
            any_failure: false,
            root_error: 0,
        });
        assert!(succeeded.refresh);
        assert_eq!(succeeded.feedback, ProcessActionFeedback::None);
    }

    #[test]
    fn tree_prepare_errors_keep_their_dialogs() {
        assert_eq!(
            report_end_tree_prepare_error(ProcessTreePrepareError::Root(87)),
            super::kill_failure(87)
        );
        assert_eq!(
            report_end_tree_prepare_error(ProcessTreePrepareError::Tree(5)).feedback,
            ProcessActionFeedback::KillTreePrepare(5)
        );
    }

    #[test]
    fn confirmed_actions_stay_queued_while_one_is_in_flight() {
        let first = ProcIdentity::new(1, 10);
        let second = ProcIdentity::new(2, 20);
        let mut pending = vec![ProcessAction::End(first)];
        append_process_actions(
            &mut pending,
            vec![
                ProcessAction::EndTree(second),
                ProcessAction::SetPriority {
                    identity: first,
                    priority: super::super::ProcPriority::Normal,
                },
                ProcessAction::SetAffinity {
                    identity: second,
                    masks: vec![
                        AffinityGroupMask {
                            group: 0,
                            mask: 0b0011,
                        },
                        AffinityGroupMask {
                            group: 1,
                            mask: 1u64 << 63,
                        },
                    ],
                },
            ],
        );
        assert!(matches!(pending[0], ProcessAction::End(identity) if identity == first));
        assert!(matches!(pending[1], ProcessAction::EndTree(identity) if identity == second));
        assert!(matches!(
            pending[2],
            ProcessAction::SetPriority { identity, priority: super::super::ProcPriority::Normal }
                if identity == first
        ));
        assert!(matches!(
            &pending[3],
            ProcessAction::SetAffinity { identity, masks }
                if *identity == second
                    && masks.len() == 2
                    && masks[0] == AffinityGroupMask { group: 0, mask: 0b0011 }
                    && masks[1] == AffinityGroupMask { group: 1, mask: 1u64 << 63 }
        ));
    }

    #[test]
    fn affinity_failure_does_not_refresh_or_count_as_success() {
        let report = affinity_failure(ERROR_INVALID_PARAMETER);
        assert!(!report.refresh);
        assert_eq!(
            report.feedback,
            ProcessActionFeedback::CantSetAffinity(ERROR_INVALID_PARAMETER)
        );
    }

    #[test]
    fn worker_completion_rejects_an_unverified_process() {
        let mut worker = spawn_process_action_worker().expect("action worker should start");
        worker
            .request(
                vec![ProcessAction::End(ProcIdentity::pid_only(4))],
                null_mut(),
            )
            .expect("request should submit");
        let deadline = Instant::now() + Duration::from_secs(2);
        let reports = loop {
            let drained = worker.drain(null_mut());
            assert_eq!(drained.error, None);
            if let Some(completion) = drained.completions.into_iter().next() {
                break completion.value;
            }
            assert!(Instant::now() < deadline, "action completion timed out");
            thread::yield_now();
        };
        assert_eq!(reports.len(), 1);
        assert!(!reports[0].refresh);
        assert_eq!(
            reports[0].feedback,
            ProcessActionFeedback::CantKill(ERROR_INVALID_PARAMETER)
        );
    }

    #[test]
    fn worker_completion_does_not_apply_affinity_for_an_unverified_process() {
        let mut worker = spawn_process_action_worker().expect("action worker should start");
        worker
            .request(
                vec![ProcessAction::SetAffinity {
                    identity: ProcIdentity::new(0, 80),
                    masks: vec![AffinityGroupMask {
                        group: 1,
                        mask: 1u64 << 63,
                    }],
                }],
                null_mut(),
            )
            .expect("request should submit");
        let deadline = Instant::now() + Duration::from_secs(2);
        let reports = loop {
            let drained = worker.drain(null_mut());
            assert_eq!(drained.error, None);
            if let Some(completion) = drained.completions.into_iter().next() {
                break completion.value;
            }
            assert!(Instant::now() < deadline, "affinity completion timed out");
            thread::yield_now();
        };
        assert_eq!(reports.len(), 1);
        assert!(!reports[0].refresh);
        assert_eq!(
            reports[0].feedback,
            ProcessActionFeedback::CantSetAffinity(ERROR_INVALID_PARAMETER)
        );
    }

    #[test]
    fn dropping_a_blocked_action_worker_does_not_join() {
        let mut worker = spawn_process_action_worker().expect("action worker should start");
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        worker
            .request(
                vec![ProcessAction::Block {
                    entered: entered_tx,
                    release: release_rx,
                }],
                null_mut(),
            )
            .expect("block request should queue");
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker should enter the block");

        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            drop(worker);
            let _ = done_tx.send(());
        });
        let finished = done_rx.recv_timeout(Duration::from_secs(1));
        let _ = release_tx.send(());
        assert!(
            finished.is_ok(),
            "shutdown must not wait for the action thread"
        );
    }
}

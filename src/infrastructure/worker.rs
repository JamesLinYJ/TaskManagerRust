// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 持久后台任务执行器
//
//   文件:       src/infrastructure/worker.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 页面采样使用的持久单消费者 worker。
//!
//! `BackgroundWorker` 只拥有线程、容量为一的通道和完成通知；`SingleFlightWorker`
//! 在其上保证最多一个在途请求，并把刷新期间的请求合并为一个后续请求。UI 仍负责
//! 判断完成快照能否提交，worker 不接触页面状态。

use std::thread::{self, JoinHandle};
use std::time::Instant;
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded};
use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_GEN_FAILURE, ERROR_NOT_ENOUGH_MEMORY, GetLastError, HWND,
};
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::infrastructure::diagnostics::{self, Field, Level};
use crate::infrastructure::native::record_win32_error;

enum WorkerCommand<Request> {
    Run {
        request: Request,
        notify_hwnd: isize,
        operation_id: u64,
    },
    Shutdown,
    #[cfg(test)]
    Disconnect,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CorrelatedCompletion<Completion> {
    pub(crate) operation_id: u64,
    pub(crate) value: Completion,
}

#[derive(Debug, PartialEq, Eq)]
enum WorkerEvent<Completion> {
    Completed(CorrelatedCompletion<Completion>),
    CollectorPanicked { operation_id: u64 },
}

struct SubmitError<Request> {
    code: u32,
    request: Request,
}

/// Cooperative cancellation for collectors that may wait inside external providers.
/// The notification gate prevents any message to a page after its owner retires the worker.
#[derive(Clone, Default)]
pub(crate) struct WorkerStopSignal {
    cancelled: Arc<AtomicBool>,
    notifications_closed: Arc<Mutex<bool>>,
}

impl WorkerStopSignal {
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub(crate) fn cancel(&self) {
        self.close_notifications(true);
    }

    fn close_notifications(&self, cancel: bool) {
        let mut closed = self
            .notifications_closed
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *closed = true;
        if cancel {
            self.cancelled.store(true, Ordering::Release);
        }
    }

    pub(crate) fn notify(&self, hwnd: HWND, message: u32) -> Option<u32> {
        let closed = self
            .notifications_closed
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if *closed || hwnd.is_null() {
            return None;
        }
        // SAFETY: the owner closes this gate before page destruction; no pointer payload is sent.
        if unsafe { PostMessageW(hwnd, message, 0, 0) } == 0 {
            let error = unsafe { GetLastError() };
            Some(if error == 0 { ERROR_GEN_FAILURE } else { error })
        } else {
            None
        }
    }
}

pub(crate) struct BackgroundWorker<Request, Completion> {
    command_sender: Sender<WorkerCommand<Request>>,
    completion_receiver: Option<Receiver<WorkerEvent<Completion>>>,
    thread: Option<JoinHandle<()>>,
    name: String,
    stop: WorkerStopSignal,
    retire_async: bool,
}

impl<Request, Completion> BackgroundWorker<Request, Completion>
where
    Request: Send + 'static,
    Completion: Send + 'static,
{
    #[cfg(test)]
    pub(crate) fn spawn<Collect>(
        thread_name: &str,
        completion_message: u32,
        collect: Collect,
    ) -> Result<Self, u32>
    where
        Collect: Fn(Request) -> Completion + Send + Sync + 'static,
    {
        let collect = Arc::new(collect);
        Self::spawn_initialized(
            thread_name,
            completion_message,
            Arc::new(move || {
                let collect = Arc::clone(&collect);
                move |request| collect(request)
            }),
        )
    }

    /// Constructs collector state on the worker thread before receiving requests.
    #[cfg(test)]
    fn spawn_initialized<Initialize, Collect>(
        thread_name: &str,
        completion_message: u32,
        initialize: Arc<Initialize>,
    ) -> Result<Self, u32>
    where
        Initialize: Fn() -> Collect + Send + Sync + 'static,
        Collect: FnMut(Request) -> Completion + 'static,
    {
        Self::spawn_with_stop(
            thread_name,
            completion_message,
            Arc::new(move |_| initialize()),
            false,
        )
    }

    fn spawn_with_stop<Initialize, Collect>(
        thread_name: &str,
        completion_message: u32,
        initialize: Arc<Initialize>,
        retire_async: bool,
    ) -> Result<Self, u32>
    where
        Initialize: Fn(WorkerStopSignal) -> Collect + Send + Sync + 'static,
        Collect: FnMut(Request) -> Completion + 'static,
    {
        let (command_sender, command_receiver) = bounded::<WorkerCommand<Request>>(1);
        let (completion_sender, completion_receiver) = bounded::<WorkerEvent<Completion>>(1);
        let name = thread_name.to_string();
        let worker_name = name.clone();
        let stop = WorkerStopSignal::default();
        let worker_stop = stop.clone();
        let thread = thread::Builder::new()
            .name(name.clone())
            .spawn(move || {
                diagnostics::event(
                    Level::Debug,
                    "worker.started",
                    "worker",
                    "background worker started",
                    &[Field::text("worker", &worker_name)],
                );
                let mut collect = None;
                let mut restarting = false;
                while let Ok(command) = command_receiver.recv() {
                    match command {
                        WorkerCommand::Run {
                            request,
                            notify_hwnd,
                            operation_id,
                        } => {
                            if worker_stop.is_cancelled() {
                                break;
                            }
                            let started = Instant::now();
                            diagnostics::event_with(
                                Level::Trace,
                                "worker.request_started",
                                "worker",
                                "background worker request started",
                                Some(operation_id),
                                None,
                                &[Field::text("worker", &worker_name)],
                            );
                            let completion = catch_unwind(AssertUnwindSafe(|| {
                                let collect = collect.get_or_insert_with(|| {
                                    let initialized = initialize(worker_stop.clone());
                                    if restarting {
                                        diagnostics::event_with(
                                            Level::Info,
                                            "worker.collector_restarted",
                                            "worker",
                                            "background worker collector restarted after a panic",
                                            Some(operation_id),
                                            None,
                                            &[Field::text("worker", &worker_name)],
                                        );
                                    }
                                    initialized
                                });
                                (!worker_stop.is_cancelled()).then(|| {
                                    diagnostics::with_operation_id(operation_id, || collect(request))
                                })
                            }));
                            let duration_ms =
                                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                            if worker_stop.is_cancelled() {
                                break;
                            }
                            let event = match completion {
                                Ok(Some(value)) => {
                                    restarting = false;
                                    diagnostics::event_with(
                                        Level::Trace,
                                        "worker.request_completed",
                                        "worker",
                                        "background worker request completed",
                                        Some(operation_id),
                                        Some(duration_ms),
                                        &[Field::text("worker", &worker_name)],
                                    );
                                    WorkerEvent::Completed(CorrelatedCompletion {
                                        operation_id,
                                        value,
                                    })
                                }
                                Ok(None) => break,
                                Err(_) => {
                                    collect = None;
                                    restarting = true;
                                    diagnostics::event_with(
                                        Level::Error,
                                        "worker.collector_panicked",
                                        "worker",
                                        "background worker collector panicked; its state was discarded",
                                        Some(operation_id),
                                        Some(duration_ms),
                                        &[Field::text("worker", &worker_name)],
                                    );
                                    WorkerEvent::CollectorPanicked { operation_id }
                                }
                            };
                            if completion_sender.send(event).is_err() {
                                break;
                            }
                            let notify_hwnd = notify_hwnd as HWND;
                            if let Some(error) = worker_stop.notify(notify_hwnd, completion_message) {
                                record_win32_error("background worker completion notification", error);
                            }
                        }
                        WorkerCommand::Shutdown => {
                            diagnostics::event(
                                Level::Debug,
                                "worker.stopping",
                                "worker",
                                "background worker stopping",
                                &[Field::text("worker", &worker_name)],
                            );
                            break;
                        }
                        #[cfg(test)]
                        WorkerCommand::Disconnect => break,
                    }
                }
            })
            .map_err(thread_spawn_error)?;

        Ok(Self {
            command_sender,
            completion_receiver: Some(completion_receiver),
            thread: Some(thread),
            name,
            stop,
            retire_async,
        })
    }

    #[cfg(test)]
    pub(crate) fn submit(&self, request: Request, notify_hwnd: HWND) -> Result<u64, u32> {
        let operation_id = diagnostics::next_operation_id();
        self.submit_with_operation_id(request, notify_hwnd, operation_id)
            .map_err(|error| error.code)?;
        Ok(operation_id)
    }

    fn submit_with_operation_id(
        &self,
        request: Request,
        notify_hwnd: HWND,
        operation_id: u64,
    ) -> Result<(), SubmitError<Request>> {
        self.command_sender
            .send(WorkerCommand::Run {
                request,
                notify_hwnd: notify_hwnd as isize,
                operation_id,
            })
            .map_err(|error| {
                let WorkerCommand::Run { request, .. } = error.0 else {
                    unreachable!("only a run command is submitted through this path");
                };
                SubmitError {
                    code: ERROR_BROKEN_PIPE,
                    request,
                }
            })?;
        let parent_operation_id = diagnostics::current_operation_id();
        let mut fields = vec![Field::text("worker", &self.name)];
        if let Some(parent_operation_id) =
            parent_operation_id.filter(|parent| *parent != operation_id)
        {
            fields.push(Field::unsigned("parent_operation_id", parent_operation_id));
        }
        diagnostics::event_with(
            Level::Trace,
            "worker.request_submitted",
            "worker",
            "background worker request submitted",
            Some(operation_id),
            None,
            &fields,
        );
        Ok(())
    }

    fn try_recv(&self) -> Result<WorkerEvent<Completion>, TryRecvError> {
        self.completion_receiver
            .as_ref()
            .ok_or(TryRecvError::Disconnected)?
            .try_recv()
    }

    #[cfg(test)]
    fn disconnect(&self) {
        self.command_sender
            .send(WorkerCommand::Disconnect)
            .expect("test worker should accept disconnect");
    }
}

impl<Request, Completion> Drop for BackgroundWorker<Request, Completion> {
    fn drop(&mut self) {
        // Disconnect completion first so a worker blocked on the capacity-one result channel can
        // leave before `join`. This makes teardown independent of whether the UI drained the last
        // completion.
        self.stop.close_notifications(self.retire_async);
        self.completion_receiver.take();
        if self.retire_async {
            // A provider may still be inside ConnectServer or an RPC call. Its state is owned
            // entirely by that worker, and cancellation closes notifications before UI teardown.
            // Reap it outside the UI; never force-terminate a thread that owns COM resources.
            let _ = self.command_sender.try_send(WorkerCommand::Shutdown);
            if let Some(worker) = self.thread.take() {
                if worker.is_finished() {
                    if worker.join().is_err() {
                        record_win32_error("background worker shutdown join", ERROR_GEN_FAILURE);
                    }
                } else {
                    let name = self.name.clone();
                    if let Err(error) = thread::Builder::new()
                        .name("taskmgr-rs-worker-reaper".to_string())
                        .spawn(move || {
                            if worker.join().is_err() {
                                record_win32_error(
                                    "retired worker shutdown join",
                                    ERROR_GEN_FAILURE,
                                );
                            }
                            diagnostics::event(
                                Level::Debug,
                                "worker.stopped",
                                "worker",
                                "retired background worker released its collector",
                                &[Field::text("worker", &name)],
                            );
                        })
                    {
                        record_win32_error("retired worker reaper", thread_spawn_error(error));
                    }
                }
            }
        } else {
            let _ = self.command_sender.send(WorkerCommand::Shutdown);
            if let Some(thread) = self.thread.take()
                && thread.join().is_err()
            {
                record_win32_error("background worker shutdown join", ERROR_GEN_FAILURE);
            }
        }
        diagnostics::event(
            Level::Debug,
            if self.retire_async {
                "worker.retired"
            } else {
                "worker.stopped"
            },
            "worker",
            if self.retire_async {
                "provider worker no longer accepts work or UI notifications"
            } else {
                "background worker stopped"
            },
            &[Field::text("worker", &self.name)],
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestDisposition {
    Submitted(u64),
    Coalesced(u64),
}

impl RequestDisposition {
    pub(crate) const fn operation_id(self) -> u64 {
        match self {
            Self::Submitted(operation_id) | Self::Coalesced(operation_id) => operation_id,
        }
    }

    pub(crate) const fn was_coalesced(self) -> bool {
        matches!(self, Self::Coalesced(_))
    }
}

pub(crate) struct WorkerDrain<Completion> {
    pub(crate) completions: Vec<CorrelatedCompletion<Completion>>,
    pub(crate) error: Option<u32>,
}

type WorkerSpawner<Request, Completion> =
    Box<dyn Fn() -> Result<BackgroundWorker<Request, Completion>, u32> + Send + Sync>;

pub(crate) struct SingleFlightWorker<Request, Completion> {
    worker: Option<BackgroundWorker<Request, Completion>>,
    restart: WorkerSpawner<Request, Completion>,
    name: String,
    merge_pending: fn(&mut Request, Request),
    pending: Option<Request>,
    pending_operation_id: Option<u64>,
    in_flight: bool,
}

impl<Request, Completion> SingleFlightWorker<Request, Completion>
where
    Request: Send + 'static,
    Completion: Send + 'static,
{
    pub(crate) fn spawn<Collect>(
        thread_name: &str,
        completion_message: u32,
        merge_pending: fn(&mut Request, Request),
        collect: Collect,
    ) -> Result<Self, u32>
    where
        Collect: Fn(Request) -> Completion + Send + Sync + 'static,
    {
        let collect = Arc::new(collect);
        Self::spawn_initialized(thread_name, completion_message, merge_pending, move || {
            let collect = Arc::clone(&collect);
            move |request| collect(request)
        })
    }

    pub(crate) fn spawn_initialized<Initialize, Collect>(
        thread_name: &str,
        completion_message: u32,
        merge_pending: fn(&mut Request, Request),
        initialize: Initialize,
    ) -> Result<Self, u32>
    where
        Initialize: Fn() -> Collect + Send + Sync + 'static,
        Collect: FnMut(Request) -> Completion + 'static,
    {
        Self::spawn_initialized_policy(
            thread_name,
            completion_message,
            merge_pending,
            move |_| initialize(),
            false,
        )
    }

    /// Only provider collectors with cooperative cancellation use asynchronous retirement.
    pub(crate) fn spawn_cancellable_initialized<Initialize, Collect>(
        thread_name: &str,
        completion_message: u32,
        merge_pending: fn(&mut Request, Request),
        initialize: Initialize,
    ) -> Result<Self, u32>
    where
        Initialize: Fn(WorkerStopSignal) -> Collect + Send + Sync + 'static,
        Collect: FnMut(Request) -> Completion + 'static,
    {
        Self::spawn_initialized_policy(
            thread_name,
            completion_message,
            merge_pending,
            initialize,
            true,
        )
    }

    fn spawn_initialized_policy<Initialize, Collect>(
        thread_name: &str,
        completion_message: u32,
        merge_pending: fn(&mut Request, Request),
        initialize: Initialize,
        retire_async: bool,
    ) -> Result<Self, u32>
    where
        Initialize: Fn(WorkerStopSignal) -> Collect + Send + Sync + 'static,
        Collect: FnMut(Request) -> Completion + 'static,
    {
        let initialize = Arc::new(initialize);
        let name = thread_name.to_string();
        let restart_name = name.clone();
        let restart_initialize = Arc::clone(&initialize);
        let restart: WorkerSpawner<Request, Completion> = Box::new(move || {
            BackgroundWorker::spawn_with_stop(
                &restart_name,
                completion_message,
                Arc::clone(&restart_initialize),
                retire_async,
            )
        });
        let worker = BackgroundWorker::spawn_with_stop(
            thread_name,
            completion_message,
            initialize,
            retire_async,
        )?;
        Ok(Self::new(worker, restart, name, merge_pending))
    }

    fn new(
        worker: BackgroundWorker<Request, Completion>,
        restart: WorkerSpawner<Request, Completion>,
        name: String,
        merge_pending: fn(&mut Request, Request),
    ) -> Self {
        Self {
            worker: Some(worker),
            restart,
            name,
            merge_pending,
            pending: None,
            pending_operation_id: None,
            in_flight: false,
        }
    }

    fn ensure_worker(&mut self) -> Result<(), u32> {
        if self.worker.is_some() {
            return Ok(());
        }
        self.worker = Some((self.restart)()?);
        diagnostics::event(
            Level::Info,
            "worker.thread_restarted",
            "worker",
            "background worker thread restarted after channel disconnection",
            &[Field::text("worker", &self.name)],
        );
        Ok(())
    }

    fn submit_request(
        &mut self,
        request: Request,
        notify_hwnd: HWND,
        operation_id: u64,
    ) -> Result<(), SubmitError<Request>> {
        if let Err(code) = self.ensure_worker() {
            return Err(SubmitError { code, request });
        }
        let result = self
            .worker
            .as_ref()
            .expect("worker was ensured")
            .submit_with_operation_id(request, notify_hwnd, operation_id);
        match result {
            Ok(()) => Ok(()),
            Err(first_error) => {
                self.worker = None;
                if let Err(code) = self.ensure_worker() {
                    return Err(SubmitError {
                        code,
                        request: first_error.request,
                    });
                }
                self.worker
                    .as_ref()
                    .expect("worker was restarted")
                    .submit_with_operation_id(first_error.request, notify_hwnd, operation_id)
            }
        }
    }

    pub(crate) fn request(
        &mut self,
        request: Request,
        notify_hwnd: HWND,
    ) -> Result<RequestDisposition, u32> {
        if self.in_flight {
            let already_pending = self.pending.is_some();
            if let Some(pending) = self.pending.as_mut() {
                (self.merge_pending)(pending, request);
            } else {
                self.pending = Some(request);
                self.pending_operation_id = Some(diagnostics::next_operation_id());
            }
            let operation_id = self
                .pending_operation_id
                .unwrap_or_else(diagnostics::next_operation_id);
            let mut fields = vec![
                Field::text("worker", &self.name),
                Field::boolean("replaced_existing_pending", already_pending),
            ];
            if let Some(parent_operation_id) =
                diagnostics::current_operation_id().filter(|parent| *parent != operation_id)
            {
                fields.push(Field::unsigned("parent_operation_id", parent_operation_id));
            }
            diagnostics::event_with(
                Level::Trace,
                "worker.request_coalesced",
                "worker",
                "background worker request coalesced",
                Some(operation_id),
                None,
                &fields,
            );
            return Ok(RequestDisposition::Coalesced(operation_id));
        }

        let operation_id = diagnostics::next_operation_id();
        self.submit_request(request, notify_hwnd, operation_id)
            .map_err(|error| error.code)?;
        self.in_flight = true;
        Ok(RequestDisposition::Submitted(operation_id))
    }

    pub(crate) fn drain(&mut self, notify_hwnd: HWND) -> WorkerDrain<Completion> {
        let mut completions = Vec::new();
        let mut error = None;
        loop {
            let event = match self.worker.as_ref() {
                Some(worker) => worker.try_recv(),
                None => Err(TryRecvError::Disconnected),
            };
            match event {
                Ok(WorkerEvent::Completed(completion)) => {
                    self.in_flight = false;
                    completions.push(completion);
                }
                Ok(WorkerEvent::CollectorPanicked { operation_id }) => {
                    self.in_flight = false;
                    error.get_or_insert(ERROR_GEN_FAILURE);
                    diagnostics::event_with(
                        Level::Warn,
                        "worker.request_failed_after_panic",
                        "worker",
                        "background worker request failed after a collector panic",
                        Some(operation_id),
                        None,
                        &[Field::text("worker", &self.name)],
                    );
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.in_flight = false;
                    self.worker = None;
                    error.get_or_insert(ERROR_BROKEN_PIPE);
                    if let Err(restart_error) = self.ensure_worker() {
                        error = Some(restart_error);
                    }
                    break;
                }
            }
        }

        if !self.in_flight
            && let Some(request) = self.pending.take()
        {
            let operation_id = self
                .pending_operation_id
                .take()
                .unwrap_or_else(diagnostics::next_operation_id);
            match self.submit_request(request, notify_hwnd, operation_id) {
                Ok(()) => self.in_flight = true,
                Err(submit_error) => {
                    self.pending = Some(submit_error.request);
                    self.pending_operation_id = Some(operation_id);
                    error = Some(submit_error.code);
                }
            }
        }

        if !completions.is_empty() || error.is_some() {
            let operation_id = (completions.len() == 1).then(|| completions[0].operation_id);
            diagnostics::event_with(
                if error.is_some() {
                    Level::Warn
                } else {
                    Level::Trace
                },
                "worker.completions_drained",
                "worker",
                "background worker completions drained",
                operation_id,
                None,
                &[
                    Field::text("worker", &self.name),
                    Field::unsigned("completion_count", completions.len() as u64),
                    Field::boolean("request_failed", error.is_some()),
                    Field::boolean("follow_up_in_flight", self.in_flight),
                ],
            );
        }

        WorkerDrain { completions, error }
    }

    pub(crate) fn is_in_flight(&self) -> bool {
        self.in_flight
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    #[cfg(test)]
    fn disconnect(&self) {
        self.worker
            .as_ref()
            .expect("test worker should exist")
            .disconnect();
    }
}

pub(crate) fn replace_pending<Request>(current: &mut Request, incoming: Request) {
    *current = incoming;
}

pub(crate) fn keep_pending<Request>(_current: &mut Request, _incoming: Request) {}

fn thread_spawn_error(error: std::io::Error) -> u32 {
    match error
        .raw_os_error()
        .and_then(|value| u32::try_from(value).ok())
    {
        Some(error) => error,
        None => ERROR_NOT_ENOUGH_MEMORY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr::null_mut;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::time::{Duration, Instant};

    #[test]
    fn worker_delivers_completion_even_when_notification_window_is_gone() {
        let worker = BackgroundWorker::spawn("taskmgr-rs-worker-test", 0, |value: u32| value * 2)
            .expect("worker should start");

        let operation_id = worker.submit(21, null_mut()).expect("request should queue");

        assert_eq!(
            worker
                .completion_receiver
                .as_ref()
                .expect("receiver should exist")
                .recv_timeout(Duration::from_secs(2))
                .expect("completion should arrive"),
            WorkerEvent::Completed(CorrelatedCompletion {
                operation_id,
                value: 42,
            })
        );
    }

    #[test]
    fn single_flight_coalesces_to_one_follow_up_request() {
        let mut worker = SingleFlightWorker::spawn(
            "taskmgr-rs-single-flight-test",
            0,
            replace_pending,
            |value: u32| value,
        )
        .expect("worker should start");

        let submitted = worker
            .request(1, null_mut())
            .expect("request should submit");
        assert!(matches!(submitted, RequestDisposition::Submitted(_)));
        let coalesced = worker
            .request(2, null_mut())
            .expect("request should coalesce");
        assert!(matches!(coalesced, RequestDisposition::Coalesced(_)));
        let coalesced_again = worker
            .request(3, null_mut())
            .expect("request should coalesce");
        assert_eq!(coalesced_again.operation_id(), coalesced.operation_id());
        assert!(worker.has_pending());

        let deadline = Instant::now() + Duration::from_secs(2);
        let first = loop {
            let drained = worker.drain(null_mut());
            assert_eq!(drained.error, None);
            if !drained.completions.is_empty() {
                break drained.completions;
            }
            assert!(Instant::now() < deadline, "first completion timed out");
            thread::yield_now();
        };
        assert_eq!(
            first,
            vec![CorrelatedCompletion {
                operation_id: submitted.operation_id(),
                value: 1,
            }]
        );
        assert_eq!(diagnostics::current_operation_id(), None);
        diagnostics::with_operation_id(first[0].operation_id, || {
            assert_eq!(
                diagnostics::current_operation_id(),
                Some(submitted.operation_id())
            );
        });
        assert_eq!(diagnostics::current_operation_id(), None);

        let deadline = Instant::now() + Duration::from_secs(2);
        let second = loop {
            let drained = worker.drain(null_mut());
            assert_eq!(drained.error, None);
            if !drained.completions.is_empty() {
                break drained.completions;
            }
            assert!(Instant::now() < deadline, "follow-up completion timed out");
            thread::yield_now();
        };
        assert_eq!(
            second,
            vec![CorrelatedCompletion {
                operation_id: coalesced.operation_id(),
                value: 3,
            }]
        );
        assert!(!worker.is_in_flight());
        assert!(!worker.has_pending());
    }

    #[test]
    fn collector_panic_discards_state_and_runs_the_pending_request_after_reinitializing() {
        let initialize_count = Arc::new(AtomicUsize::new(0));
        let panic_once = Arc::new(AtomicBool::new(true));
        let initialize_count_by_worker = Arc::clone(&initialize_count);
        let panic_once_by_worker = Arc::clone(&panic_once);
        let mut worker = SingleFlightWorker::spawn_initialized(
            "taskmgr-rs-worker-panic-test",
            0,
            replace_pending,
            move || {
                initialize_count_by_worker.fetch_add(1, Ordering::AcqRel);
                let panic_once = Arc::clone(&panic_once_by_worker);
                move |value: u32| {
                    assert!(
                        !panic_once.swap(false, Ordering::AcqRel),
                        "synthetic collector panic"
                    );
                    value * 2
                }
            },
        )
        .expect("worker should start");
        worker.request(1, null_mut()).expect("request should queue");
        worker
            .request(2, null_mut())
            .expect("follow-up request should coalesce");
        assert!(worker.has_pending());

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let drained = worker.drain(null_mut());
            if drained.error == Some(ERROR_GEN_FAILURE) {
                assert!(drained.completions.is_empty());
                assert!(worker.is_in_flight());
                assert!(!worker.has_pending());
                break;
            }
            assert!(Instant::now() < deadline, "panic report timed out");
            thread::yield_now();
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let drained = worker.drain(null_mut());
            assert_eq!(drained.error, None);
            if let Some(completion) = drained.completions.first() {
                assert_eq!(completion.value, 4);
                assert_eq!(initialize_count.load(Ordering::Acquire), 2);
                assert!(!worker.is_in_flight());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "reinitialized collector timed out"
            );
            thread::yield_now();
        }
    }

    #[test]
    fn disconnected_worker_thread_is_recreated_before_the_next_request() {
        let mut worker = SingleFlightWorker::spawn(
            "taskmgr-rs-worker-restart-test",
            0,
            keep_pending,
            |value: u32| value + 1,
        )
        .expect("worker should start");
        worker.disconnect();

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let drained = worker.drain(null_mut());
            if drained.error == Some(ERROR_BROKEN_PIPE) {
                break;
            }
            assert!(Instant::now() < deadline, "disconnect timed out");
            thread::yield_now();
        }

        worker
            .request(41, null_mut())
            .expect("replacement worker should accept a request");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let drained = worker.drain(null_mut());
            assert_eq!(drained.error, None);
            if let Some(completion) = drained.completions.first() {
                assert_eq!(completion.value, 42);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "replacement worker completion timed out"
            );
            thread::yield_now();
        }
    }

    #[test]
    fn dropping_worker_joins_an_accepted_request() {
        let completed = Arc::new(AtomicBool::new(false));
        {
            let completed_by_worker = Arc::clone(&completed);
            let mut worker = SingleFlightWorker::spawn(
                "taskmgr-rs-worker-close-test",
                0,
                keep_pending,
                move |(): ()| completed_by_worker.store(true, Ordering::Release),
            )
            .expect("worker should start");
            worker
                .request((), null_mut())
                .expect("request should be accepted");
        }

        assert!(completed.load(Ordering::Acquire));
    }

    #[test]
    fn cancelled_provider_is_reaped_off_ui_and_cannot_notify_the_destroyed_page() {
        use crate::infrastructure::native::to_wide_null;
        use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
        use windows_sys::Win32::System::Threading::GetCurrentThreadId;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, HWND_MESSAGE,
        };
        let class = to_wide_null("STATIC");
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                1,
                1,
                HWND_MESSAGE,
                null_mut(),
                null_mut(),
                null_mut(),
            )
        };
        assert!(!hwnd.is_null());
        struct Window(HWND);
        impl Drop for Window {
            fn drop(&mut self) {
                unsafe {
                    DestroyWindow(self.0);
                }
            }
        }
        let window = Window(hwnd);
        let (started_tx, started_rx) = bounded(1);
        let (release_tx, release_rx) = bounded(1);
        let (dropped_tx, dropped_rx) = bounded(1);
        let (signal_tx, signal_rx) = bounded(1);
        struct Owner {
            thread: u32,
            dropped: Sender<()>,
        }
        impl Drop for Owner {
            fn drop(&mut self) {
                assert_eq!(unsafe { GetCurrentThreadId() }, self.thread);
                let _ = self.dropped.send(());
            }
        }
        let mut worker = SingleFlightWorker::spawn_cancellable_initialized(
            "taskmgr-provider-stop-test",
            0x8001,
            keep_pending,
            move |stop| {
                signal_tx.send(stop.clone()).unwrap();
                let owner = Owner {
                    thread: unsafe { GetCurrentThreadId() },
                    dropped: dropped_tx.clone(),
                };
                let started = started_tx.clone();
                let release = release_rx.clone();
                move |(): ()| {
                    let _owned_on_worker = &owner;
                    started.send(()).unwrap();
                    // A bounded fixture models a native call that is still blocked at teardown.
                    let _ = release.recv_timeout(Duration::from_secs(3));
                }
            },
        )
        .unwrap();
        worker.request((), hwnd).unwrap();
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let signal = signal_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let started = Instant::now();
        drop(worker);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "UI waited for the blocked provider"
        );
        drop(window);
        assert!(signal.is_cancelled());
        unsafe {
            SetLastError(0x1234);
        }
        assert_eq!(signal.notify(hwnd, 0x8001), None);
        assert_eq!(
            unsafe { GetLastError() },
            0x1234,
            "closed gate called PostMessageW"
        );
        release_tx.send(()).unwrap();
        dropped_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("collector resources must be reclaimed on its own thread");
    }

    #[test]
    fn cancellation_during_provider_initialization_skips_collection() {
        let (started_tx, started_rx) = bounded(1);
        let (release_tx, release_rx) = bounded(1);
        let (dropped_tx, dropped_rx) = bounded(1);
        let collected = Arc::new(AtomicBool::new(false));
        let captured = Arc::clone(&collected);
        struct Owner(Sender<()>);
        impl Drop for Owner {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let mut worker = SingleFlightWorker::spawn_cancellable_initialized(
            "taskmgr-provider-init-stop-test",
            0,
            keep_pending,
            move |_| {
                started_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(3));
                let owner = Owner(dropped_tx.clone());
                let captured = Arc::clone(&captured);
                move |(): ()| {
                    let _owner = &owner;
                    captured.store(true, Ordering::Release);
                }
            },
        )
        .unwrap();
        worker.request((), null_mut()).unwrap();
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(worker);
        release_tx.send(()).unwrap();
        dropped_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!collected.load(Ordering::Acquire));
    }
}

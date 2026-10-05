// A write operation on its own thread, for a UI that must not block
// (orchestration, Core layer). No GUI toolkit: only `std::thread` and
// `std::sync::mpsc`.
//
// The UI never builds or holds any part of the operation. It hands a
// `WriteOperationRequest` (only choices: a target reference, an image path,
// a Verify mode) to `spawn_write_worker`; the worker thread runs the same
// `run_write_operation` sequence the CLI runs -- every snapshot, selection,
// image source, confirmation token, FD and authorization is created, used
// and dropped on that thread. What crosses back is owned data only:
//
//   UI thread                                   worker thread
//   WriteOperationRequest (moved in) ---------> run_write_operation(...)
//   WriteWorker::request_cancel()  --------->   the operation's CancelHandle
//                         <-- WorkerMessage::Event(WorkerEvent)
//                         <-- WorkerMessage::ConfirmationRequested(..)
//   submit_confirmation(ConfirmationDecision) -> ChannelObserver
//                         <-- WorkerMessage::Finished(Box<OperationOutcome>)
//
// `WorkerEvent` / `WorkerConfirmationRequest` are owned copies of the
// operation's borrowed `OperationEvent` / `ConfirmationRequest`: the data a
// UI shows, with no capability of any kind. The UI's answer -- its explicit
// approval of the request as shown, or typed text -- is only passed along:
// the operation acts on it (`approve()` / `confirm()`, which compares typed
// text), not the UI and not this module. `OperationOutcome` crosses as it is: it is owned data
// (the steps' own structured errors), with no image source, token, FD or
// authorization in it.
//
// Cancellation is the operation's own `CancelHandle`, requested from the UI
// thread; the operation checks it only at its existing cancel points. While
// a confirmation is pending, the worker waits for the UI's decision the way
// the CLI waits for its prompt: checking the handle before each wait and
// after a decision arrives, so a cancellation always wins (the operation's
// confirmation cancel point).
//
// This module is the library's Production API entry point (see lib.rs).
// The CLI binary, which compiles the same modules as its own crate, does not
// use it, and a UI reads most message fields only for display; hence the
// module-wide `dead_code` allowance.
#![allow(dead_code)]

use std::io;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::candidates::DeviceDisplay;
use super::events::{ConfirmationDecision, OpenPurpose, OperationEvent, OperationObserver};
use super::operation::{ConfirmationRequest, WriteOperationRequest, run_on};
use super::outcome::OperationOutcome;
use super::platform::{LinuxPlatform, Platform};
use super::removal::{RemovalTarget, removal_allowed};
use crate::device::DeviceSnapshot;
use crate::execution::core::{SelectionState, VerifyMode, VerifyTargetDiagnostics};
use crate::execution::linux_access::FdMetadata;
use crate::execution::write_job::{CancelHandle, CancelReason, VerifyProgress};
use crate::image_source::CompressionFormat;
use crate::image_source::compressed::PreflightProgress;
use crate::safety::SafetyAssessment;
use crate::writer::{WritePlan, WriteProgress, WritebackProgress};

// How often a pending confirmation re-checks for cancellation while no
// decision has arrived (the same bound the CLI prompt uses).
const CONFIRMATION_CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);

// An owned copy of an `OperationEvent`, one variant each. Where the event
// borrows, this owns a copy; the one exception is `DeviceOpenFailed`, whose
// `OpenDeviceError` travels in the outcome (`WriteDeviceRejected` /
// `VerifyNotStarted::Start`) instead of being copied here.
/// A step of the operation, as it happens (owned data).
#[derive(Debug)]
pub enum WorkerEvent {
    TargetSelected {
        target: DeviceDisplay,
        block_path: String,
        diskseq: Option<u64>,
        assessment: SafetyAssessment,
    },
    CompressedImageDetected {
        format: CompressionFormat,
    },
    PreflightProgress(PreflightProgress),
    ImageSelected {
        image_size: u64,
    },
    Confirmed {
        target_block_path: String,
        image_size: u64,
        verify_mode: VerifyMode,
    },
    WriteGatePassed {
        plan: WritePlan,
    },
    OpeningDevice {
        purpose: OpenPurpose,
        block_path: String,
    },
    DeviceOpened {
        purpose: OpenPurpose,
        metadata: Option<FdMetadata>,
    },
    DeviceOpenFailed {
        purpose: OpenPurpose,
    },
    FdBound {
        purpose: OpenPurpose,
    },
    WriteAuthorized {
        target_block_path: String,
        target_size: u64,
        image_size: u64,
        verify_mode: VerifyMode,
    },
    ImageBound,
    WriteStarted,
    WriteProgress(WriteProgress),
    WritebackProgress(WritebackProgress),
    WriteSucceeded {
        bytes_written: u64,
        image_size: u64,
    },
    // The write stopped at a cancellation and its pending data is being
    // written back before the FD is closed (not cancellable).
    CancelDrainStarted {
        bytes_written: u64,
    },
    // `io::Error` cannot be copied; its kind and message are.
    CancelDrainOnCallingThread {
        kind: io::ErrorKind,
        message: String,
    },
    SyncStarted,
    // `io::Error` cannot be copied; its kind and message are.
    SyncOnCallingThread {
        kind: io::ErrorKind,
        message: String,
    },
    SyncSucceeded {
        bytes_written: u64,
    },
    VerifyPending {
        mode: VerifyMode,
    },
    VerifySnapshotRequested {
        block_path: String,
    },
    // Boxed: two full snapshots, much larger than every other event.
    VerifyTargetChecked {
        diagnostics: Box<VerifyTargetDiagnostics>,
    },
    VerifyStarted,
    VerifyProgress(VerifyProgress),
}

impl WorkerEvent {
    fn from_operation(event: OperationEvent<'_>) -> Self {
        match event {
            OperationEvent::TargetSelected { state } => match state {
                SelectionState::Selected {
                    baseline,
                    baseline_assessment,
                    ..
                } => WorkerEvent::TargetSelected {
                    target: DeviceDisplay::from_snapshot(baseline),
                    block_path: baseline.block_path.clone(),
                    diskseq: baseline.diskseq,
                    assessment: baseline_assessment.clone(),
                },
                _ => unreachable!("TargetSelected is only reported for a selected target"),
            },
            OperationEvent::CompressedImageDetected { format } => {
                WorkerEvent::CompressedImageDetected { format }
            }
            OperationEvent::PreflightProgress(progress) => WorkerEvent::PreflightProgress(progress),
            OperationEvent::ImageSelected { image_size } => {
                WorkerEvent::ImageSelected { image_size }
            }
            OperationEvent::Confirmed(confirmed) => WorkerEvent::Confirmed {
                target_block_path: confirmed.target_block_path.to_string(),
                image_size: confirmed.image_size,
                verify_mode: confirmed.verify_mode,
            },
            OperationEvent::WriteGatePassed { plan } => WorkerEvent::WriteGatePassed { plan },
            OperationEvent::OpeningDevice {
                purpose,
                block_path,
            } => WorkerEvent::OpeningDevice {
                purpose,
                block_path: block_path.to_string(),
            },
            OperationEvent::DeviceOpened { purpose, metadata } => WorkerEvent::DeviceOpened {
                purpose,
                metadata: metadata.map(|metadata| FdMetadata {
                    major: metadata.major,
                    minor: metadata.minor,
                    size: metadata.size,
                    diskseq: metadata.diskseq,
                    proc_fd_target: metadata.proc_fd_target.clone(),
                }),
            },
            OperationEvent::DeviceOpenFailed { purpose, error: _ } => {
                WorkerEvent::DeviceOpenFailed { purpose }
            }
            OperationEvent::FdBound { purpose } => WorkerEvent::FdBound { purpose },
            OperationEvent::WriteAuthorized {
                target_block_path,
                target_size,
                image_size,
                verify_mode,
            } => WorkerEvent::WriteAuthorized {
                target_block_path: target_block_path.to_string(),
                target_size,
                image_size,
                verify_mode,
            },
            OperationEvent::ImageBound => WorkerEvent::ImageBound,
            OperationEvent::WriteStarted => WorkerEvent::WriteStarted,
            OperationEvent::WriteProgress(progress) => WorkerEvent::WriteProgress(progress),
            OperationEvent::WritebackProgress(progress) => WorkerEvent::WritebackProgress(progress),
            OperationEvent::WriteSucceeded {
                bytes_written,
                image_size,
            } => WorkerEvent::WriteSucceeded {
                bytes_written,
                image_size,
            },
            OperationEvent::CancelDrainStarted { bytes_written } => {
                WorkerEvent::CancelDrainStarted { bytes_written }
            }
            OperationEvent::CancelDrainOnCallingThread { error } => {
                WorkerEvent::CancelDrainOnCallingThread {
                    kind: error.kind(),
                    message: error.to_string(),
                }
            }
            OperationEvent::SyncStarted => WorkerEvent::SyncStarted,
            OperationEvent::SyncOnCallingThread { error } => WorkerEvent::SyncOnCallingThread {
                kind: error.kind(),
                message: error.to_string(),
            },
            OperationEvent::SyncSucceeded { bytes_written } => {
                WorkerEvent::SyncSucceeded { bytes_written }
            }
            OperationEvent::VerifyPending { mode } => WorkerEvent::VerifyPending { mode },
            OperationEvent::VerifySnapshotRequested { block_path } => {
                WorkerEvent::VerifySnapshotRequested {
                    block_path: block_path.to_string(),
                }
            }
            OperationEvent::VerifyTargetChecked { diagnostics } => {
                WorkerEvent::VerifyTargetChecked {
                    diagnostics: Box::new(diagnostics.clone()),
                }
            }
            OperationEvent::VerifyStarted => WorkerEvent::VerifyStarted,
            OperationEvent::VerifyProgress(progress) => WorkerEvent::VerifyProgress(progress),
        }
    }
}

// An owned copy of the operation's `ConfirmationRequest`: what the user
// must be shown, and the text a typed confirmation must match. Showing it
// and returning the user's answer is all a UI does; acting on the answer is
// the operation's.
/// What the user must be shown before writing. The user confirms it by
/// explicitly approving it ([`ConfirmationDecision::Approved`]) or by typing
/// `expected_text` ([`ConfirmationDecision::Submitted`]).
#[derive(Debug, Clone)]
pub struct WorkerConfirmationRequest {
    pub target: DeviceDisplay,
    pub block_path: String,
    pub diskseq: Option<u64>,
    pub assessment: SafetyAssessment,
    pub image_size: u64,
    pub verify_mode: VerifyMode,
    pub expected_text: String,
}

impl WorkerConfirmationRequest {
    fn from_operation(request: &ConfirmationRequest<'_>) -> Self {
        WorkerConfirmationRequest {
            target: request.target.clone(),
            block_path: request.block_path.to_string(),
            diskseq: request.diskseq,
            assessment: request.assessment.clone(),
            image_size: request.image_size,
            verify_mode: request.verify_mode,
            expected_text: request.expected_text.to_string(),
        }
    }
}

// What actually crosses the worker's channel: the public messages, and at
// the end the outcome together with Safe Removal's reference when this
// outcome offers one. Private: a UI only ever receives `WorkerMessage`.
enum Delivery {
    Message(WorkerMessage),
    Finished {
        outcome: Box<OperationOutcome>,
        removal: Option<RemovalTarget>,
    },
}

// Everything the worker sends to the UI, in the order it happens. After
// `Finished` the worker sends nothing more and its thread ends.
/// Everything the worker sends to the UI, in order; `Finished` is last.
#[derive(Debug)]
pub enum WorkerMessage {
    Event(WorkerEvent),
    ConfirmationRequested(WorkerConfirmationRequest),
    // Boxed: an outcome is far larger than any other message, and is sent
    // once.
    Finished(Box<OperationOutcome>),
}

// The UI/worker communication lifecycle as the UI side has seen it -- not
// the operation's own state, which only the operation knows. ("Idle" is
// simply having no `WriteWorker`.)
/// The UI/worker communication lifecycle, as the UI side has seen it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    // Spawned; nothing received yet.
    Started,
    Running,
    // A `ConfirmationRequested` arrived and no decision was submitted yet.
    WaitingForConfirmation,
    Completed,
    Failed,
    Cancelled,
    // The worker's channel closed without an outcome (the worker thread
    // panicked); `join` reports the panic.
    Lost,
}

// Why `submit_confirmation` did not deliver the decision.
/// Why a confirmation decision was not delivered.
#[derive(Debug)]
pub enum SubmitError {
    // No confirmation is pending; the decision is handed back.
    NotWaiting(ConfirmationDecision),
    // The worker is gone.
    WorkerGone(ConfirmationDecision),
}

// The UI side of one write operation running on its own thread.
/// The UI side of one write operation running on its own thread.
pub struct WriteWorker {
    messages: Receiver<Delivery>,
    decisions: Option<Sender<ConfirmationDecision>>,
    cancel: CancelHandle,
    thread: Option<JoinHandle<()>>,
    state: WorkerState,
    // Set only when `Finished` is received (see `removal_target`).
    removal: Option<RemovalTarget>,
}

// Starts `request` on a new thread with the production platform (the
// sequence `run_write_operation` runs). A thread that cannot be created
// is an error before anything has started.
/// Starts the write operation described by `request` on a new thread.
pub fn spawn_write_worker(request: WriteOperationRequest) -> io::Result<WriteWorker> {
    spawn_on(LinuxPlatform, request)
}

fn spawn_on<P: Platform + Send + 'static>(
    platform: P,
    request: WriteOperationRequest,
) -> io::Result<WriteWorker> {
    let (message_sender, messages) = mpsc::channel();
    let (decision_sender, decisions) = mpsc::channel();
    let cancel = CancelHandle::new();
    let worker_cancel = cancel.clone();

    let thread = thread::Builder::new()
        .name("write-operation".into())
        .spawn(move || {
            let mut observer = ChannelObserver {
                messages: message_sender.clone(),
                decisions,
                cancel: worker_cancel.clone(),
                bound: None,
            };
            let outcome = run_on(&platform, request, &worker_cancel, &mut observer);
            #[cfg(debug_assertions)]
            crate::cancel_diag::outcome(&outcome);
            // `run_on` has returned: every device FD the operation opened is
            // closed. Only now may Safe Removal's reference exist.
            let removal = removal_for(observer.bound.take(), &outcome);
            // The UI may already be gone; the operation has ended either way.
            let _ = message_sender.send(Delivery::Finished {
                outcome: Box::new(outcome),
                removal,
            });
        })?;

    Ok(WriteWorker {
        messages,
        decisions: Some(decision_sender),
        cancel,
        thread: Some(thread),
        state: WorkerState::Started,
        removal: None,
    })
}

// Safe Removal's reference for a finished operation: only if the write
// started on a bound target (`bound`) and the outcome offers removal
// (`removal::removal_allowed`, the one place that rule is written).
fn removal_for(bound: Option<DeviceSnapshot>, outcome: &OperationOutcome) -> Option<RemovalTarget> {
    bound
        .filter(|_| removal_allowed(outcome))
        .map(RemovalTarget::from_bound_snapshot)
}

impl WriteWorker {
    /// The lifecycle state as of the last message received.
    pub fn state(&self) -> WorkerState {
        self.state
    }

    // A handle a UI can keep (e.g. for a Cancel button); cancelling through
    // it is the same as `request_cancel`.
    /// A handle that cancels this operation (e.g. for a Cancel button).
    pub fn cancel_handle(&self) -> CancelHandle {
        self.cancel.clone()
    }

    // Safe Removal's reference to the device this operation wrote to:
    // `None` until `Finished` has been received, and after it `Some` only if
    // the write started and the outcome offers removal. Take it before
    // `join`, which consumes the worker.
    /// The device this operation wrote to, for safe removal: `None` until
    /// [`WorkerMessage::Finished`] has been received, then `Some` only if
    /// the write started and the outcome allows removal.
    pub fn removal_target(&self) -> Option<RemovalTarget> {
        self.removal.clone()
    }

    /// Asks the operation to stop at its next cancel point.
    pub fn request_cancel(&self) {
        self.cancel.request_cancel(CancelReason::UserRequested);
    }

    // Waits for the next message; `None` once the worker has finished (or
    // was lost) and nothing more will come.
    /// Waits for the next message; `None` once nothing more will come.
    pub fn recv(&mut self) -> Option<WorkerMessage> {
        match self.messages.recv() {
            Ok(delivery) => Some(self.observe(delivery)),
            Err(_) => {
                self.channel_closed();
                None
            }
        }
    }

    // For a UI event loop: the next message if one is waiting.
    /// The next message if one is waiting.
    pub fn try_recv(&mut self) -> Result<WorkerMessage, TryRecvError> {
        match self.messages.try_recv() {
            Ok(delivery) => Ok(self.observe(delivery)),
            Err(TryRecvError::Disconnected) => {
                self.channel_closed();
                Err(TryRecvError::Disconnected)
            }
            Err(TryRecvError::Empty) => Err(TryRecvError::Empty),
        }
    }

    /// Waits up to `timeout` for the next message.
    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<WorkerMessage, RecvTimeoutError> {
        match self.messages.recv_timeout(timeout) {
            Ok(delivery) => Ok(self.observe(delivery)),
            Err(RecvTimeoutError::Disconnected) => {
                self.channel_closed();
                Err(RecvTimeoutError::Disconnected)
            }
            Err(RecvTimeoutError::Timeout) => Err(RecvTimeoutError::Timeout),
        }
    }

    // Answers the pending confirmation: the user's approval, what the user
    // typed, or why there is no answer. Only valid while
    // `WaitingForConfirmation`; any other time the decision is handed back.
    /// Answers the pending confirmation request.
    pub fn submit_confirmation(
        &mut self,
        decision: ConfirmationDecision,
    ) -> Result<(), SubmitError> {
        if self.state != WorkerState::WaitingForConfirmation {
            return Err(SubmitError::NotWaiting(decision));
        }
        let Some(decisions) = &self.decisions else {
            return Err(SubmitError::WorkerGone(decision));
        };
        match decisions.send(decision) {
            Ok(()) => {
                self.state = WorkerState::Running;
                Ok(())
            }
            Err(mpsc::SendError(decision)) => Err(SubmitError::WorkerGone(decision)),
        }
    }

    // Waits for the worker thread to end. After `Finished` this returns
    // promptly; `Err` is a panic on the worker thread.
    /// Waits for the worker thread to end; `Err` if it panicked.
    pub fn join(mut self) -> thread::Result<()> {
        match self.thread.take() {
            Some(thread) => thread.join(),
            None => Ok(()),
        }
    }

    fn observe(&mut self, delivery: Delivery) -> WorkerMessage {
        let message = match delivery {
            Delivery::Message(message) => message,
            Delivery::Finished { outcome, removal } => {
                self.removal = removal;
                WorkerMessage::Finished(outcome)
            }
        };
        self.state = match &message {
            WorkerMessage::Event(_) => match self.state {
                WorkerState::Started => WorkerState::Running,
                state => state,
            },
            WorkerMessage::ConfirmationRequested(_) => WorkerState::WaitingForConfirmation,
            WorkerMessage::Finished(outcome) => match **outcome {
                OperationOutcome::Completed { .. } => WorkerState::Completed,
                OperationOutcome::Failed(_) => WorkerState::Failed,
                OperationOutcome::Cancelled(_) => WorkerState::Cancelled,
            },
        };
        message
    }

    fn channel_closed(&mut self) {
        if !matches!(
            self.state,
            WorkerState::Completed | WorkerState::Failed | WorkerState::Cancelled
        ) {
            self.state = WorkerState::Lost;
        }
    }
}

// The operation's observer on the worker thread: forwards owned copies of
// events and confirmation requests, and waits for the UI's decision.
struct ChannelObserver {
    messages: Sender<Delivery>,
    decisions: Receiver<ConfirmationDecision>,
    cancel: CancelHandle,
    // The snapshot the write FD was bound to, once the write started.
    bound: Option<DeviceSnapshot>,
}

impl OperationObserver for ChannelObserver {
    fn on_event(&mut self, event: OperationEvent<'_>) {
        // A UI that went away does not stop the operation mid-step; its own
        // cancel points and the confirmation (below) still apply.
        let _ = self.messages.send(Delivery::Message(WorkerMessage::Event(
            WorkerEvent::from_operation(event),
        )));
    }

    // Kept, not sent: it becomes Safe Removal's reference only after the
    // operation ended (`removal_for`).
    fn write_started_on(&mut self, bound: DeviceSnapshot) {
        self.bound = Some(bound);
    }

    fn request_confirmation(&mut self, request: &ConfirmationRequest<'_>) -> ConfirmationDecision {
        let request = WorkerConfirmationRequest::from_operation(request);
        if self
            .messages
            .send(Delivery::Message(WorkerMessage::ConfirmationRequested(
                request,
            )))
            .is_err()
        {
            // Nobody can answer: never an implicit yes.
            return ConfirmationDecision::InputClosed;
        }

        loop {
            if self.cancel.is_requested() {
                return ConfirmationDecision::Cancelled;
            }
            match self
                .decisions
                .recv_timeout(CONFIRMATION_CANCEL_POLL_INTERVAL)
            {
                Ok(decision) => {
                    if self.cancel.is_requested() {
                        return ConfirmationDecision::Cancelled;
                    }
                    return decision;
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    if self.cancel.is_requested() {
                        return ConfirmationDecision::Cancelled;
                    }
                    return ConfirmationDecision::InputClosed;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::operation::ConfirmError;
    use super::super::outcome::{CancelledAt, OperationError, VerifyNotStarted};
    use super::super::test_support::*;
    use super::*;
    use crate::device::SnapshotFetchOutcome;
    use crate::execution::core::WriteGateError;
    use crate::execution::linux_access::{OpenAccess, OpenDeviceError};
    use crate::execution::write_job::{VerifyFailureReason, VerifySucceeded};
    use crate::orchestration::candidates::TargetRef;

    fn request(image: &TempImage, verify_mode: VerifyMode) -> WriteOperationRequest {
        WriteOperationRequest {
            target: TargetRef::from_block_path(usb_stick().block_path),
            image_path: image.path().to_string(),
            verify_mode,
        }
    }

    fn device(target: &TempImage) -> Device {
        Device {
            path: target.0.clone(),
            ..Device::default()
        }
    }

    // select, re-check, fresh gate, and Verify's re-check when verifying.
    fn snapshots(verify: bool) -> Vec<SnapshotFetchOutcome> {
        (0..3 + usize::from(verify))
            .map(|_| found(usb_stick()))
            .collect()
    }

    // Plays the UI: collects every event, answers the one confirmation with
    // `answer` (after checking what it asks for), and returns the events,
    // the outcome and the states seen along the way.
    struct Run {
        events: Vec<WorkerEvent>,
        outcome: OperationOutcome,
        states: Vec<WorkerState>,
    }

    fn drive(
        worker: &mut WriteWorker,
        answer: impl FnOnce(&WorkerConfirmationRequest) -> ConfirmationDecision,
    ) -> Run {
        let mut answer = Some(answer);
        let mut events = Vec::new();
        let mut states = vec![worker.state()];
        assert!(worker.removal_target().is_none(), "before anything arrived");
        loop {
            let message = worker.recv().expect("the worker ended without an outcome");
            states.push(worker.state());
            // Safe Removal's reference never exists before `Finished`.
            if !matches!(message, WorkerMessage::Finished(_)) {
                assert!(worker.removal_target().is_none(), "before Finished");
            }
            match message {
                WorkerMessage::Event(event) => events.push(event),
                WorkerMessage::ConfirmationRequested(request) => {
                    let decide = answer.take().expect("asked for confirmation twice");
                    worker.submit_confirmation(decide(&request)).unwrap();
                    states.push(worker.state());
                }
                WorkerMessage::Finished(outcome) => {
                    return Run {
                        events,
                        outcome: *outcome,
                        states,
                    };
                }
            }
        }
    }

    // The worker ended: its thread joins, and its channel is closed.
    fn assert_ended(mut worker: WriteWorker, final_state: WorkerState) {
        assert_eq!(worker.state(), final_state);
        assert!(worker.recv().is_none(), "nothing after Finished");
        assert_eq!(worker.state(), final_state, "a finished worker is not Lost");
        worker.join().expect("the worker thread ended normally");
    }

    fn opened(events: &[WorkerEvent], purpose: OpenPurpose) -> bool {
        events.iter().any(
            |event| matches!(event, WorkerEvent::OpeningDevice { purpose: p, .. } if *p == purpose),
        )
    }

    // D (compile time): everything that crosses between the UI thread and
    // the worker is owned and can be sent; the operation's own image source
    // still cannot (it is built and used on the worker thread only).
    #[test]
    fn only_owned_sendable_values_cross_the_thread_boundary() {
        fn crosses<T: Send + 'static>() {}
        crosses::<WriteOperationRequest>();
        crosses::<WorkerMessage>();
        crosses::<WorkerEvent>();
        crosses::<WorkerConfirmationRequest>();
        crosses::<ConfirmationDecision>();
        crosses::<OperationOutcome>();
        crosses::<CancelHandle>();
        crosses::<RemovalTarget>();

        trait AmbiguousIfSend<A> {
            fn check() {}
        }
        impl<T: ?Sized> AmbiguousIfSend<()> for T {}
        struct IsSend;
        impl<T: ?Sized + Send> AmbiguousIfSend<IsSend> for T {}
        <crate::image_source::SelectedImage as AmbiguousIfSend<_>>::check();
    }

    // A, B, D: a raw image with Full Verify, run on the worker thread:
    // events arrive owned and in order, the confirmation goes to the UI and
    // the UI's typed text comes back, and the outcome arrives last. The
    // UI-side state follows Started -> Running -> WaitingForConfirmation ->
    // Running -> Completed.
    #[test]
    fn a_worker_runs_the_operation_and_reports_its_outcome() {
        let data = payload();
        let image = temp_image("worker", "img", &data);
        let target = temp_image("worker-target", "device", b"");
        let mut worker = spawn_on(
            ScriptedPlatform::with_device(snapshots(true), device(&target)),
            request(&image, VerifyMode::Full),
        )
        .unwrap();

        let run = drive(&mut worker, |request| {
            assert_eq!(request.expected_text, "/dev/sdx");
            assert_eq!(request.block_path, usb_stick().block_path);
            assert_eq!(request.image_size, data.len() as u64);
            assert_eq!(request.verify_mode, VerifyMode::Full);
            assert!(request.assessment.writable);
            ConfirmationDecision::Submitted("/dev/sdx\n".to_string())
        });

        match &run.outcome {
            OperationOutcome::Completed { verify, image_size } => {
                assert_eq!(verify.mode, VerifyMode::Full);
                assert_eq!(verify.verified_bytes, data.len() as u64);
                assert_eq!(*image_size, data.len() as u64);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(std::fs::read(&target.0).unwrap(), data);

        let mut states = run.states.clone();
        states.dedup();
        assert_eq!(
            states,
            [
                WorkerState::Started,
                WorkerState::Running,
                WorkerState::WaitingForConfirmation,
                WorkerState::Running,
                WorkerState::Completed,
            ]
        );

        // The events are plain owned data, still readable after the worker
        // thread (and everything the operation borrowed) is gone.
        assert_ended(worker, WorkerState::Completed);
        match &run.events[0] {
            WorkerEvent::TargetSelected {
                target,
                diskseq,
                assessment,
                ..
            } => {
                assert_eq!(target.device, "/dev/sdx");
                assert_eq!(*diskseq, Some(12));
                assert!(assessment.writable);
            }
            other => panic!("{other:?}"),
        }
        assert!(run.events.iter().any(|event| matches!(
            event,
            WorkerEvent::DeviceOpened {
                purpose: OpenPurpose::Write,
                metadata: Some(FdMetadata {
                    major: 8,
                    minor: 0,
                    ..
                }),
            }
        )));
        assert!(
            run.events
                .iter()
                .any(|event| matches!(event, WorkerEvent::VerifyTargetChecked { .. }))
        );
        assert!(opened(&run.events, OpenPurpose::Write));
        assert!(opened(&run.events, OpenPurpose::Verify));
        assert!(matches!(
            run.events.last(),
            Some(WorkerEvent::VerifyProgress(_))
        ));
    }

    // B: the UI only returns text; the operation decides. A wrong device
    // name ends as the operation's own `Mismatch`, before anything is
    // opened, and a decision nobody asked for is refused by the worker
    // handle.
    #[test]
    fn a_wrong_confirmation_is_refused_by_the_operation() {
        let image = temp_image("worker-wrong", "img", &payload());
        let target = temp_image("worker-wrong-target", "device", b"");
        let mut worker = spawn_on(
            ScriptedPlatform::with_device(snapshots(false), device(&target)),
            request(&image, VerifyMode::None),
        )
        .unwrap();
        assert!(matches!(
            worker.submit_confirmation(ConfirmationDecision::Submitted("/dev/sdx".to_string())),
            Err(SubmitError::NotWaiting(_))
        ));

        let run = drive(&mut worker, |_| {
            ConfirmationDecision::Submitted("/dev/sdy".to_string())
        });
        assert!(matches!(
            run.outcome,
            OperationOutcome::Failed(OperationError::Confirmation(ConfirmError::Mismatch))
        ));
        assert!(!opened(&run.events, OpenPurpose::Write));
        assert!(std::fs::read(&target.0).unwrap().is_empty());
        assert_ended(worker, WorkerState::Failed);
    }

    // B2: a UI's explicit approval is only an answer to a pending request:
    // sent before one was shown it is refused by the worker handle; given to
    // the request, the operation continues through the same Write Gate,
    // OpenDevice and FD binding, writes and verifies. An approval that
    // arrives together with a cancellation loses to it.
    #[test]
    fn an_approval_answers_only_the_pending_request() {
        let image = temp_image("worker-approve", "img", &payload());
        let target = temp_image("worker-approve-target", "device", b"");
        let mut worker = spawn_on(
            ScriptedPlatform::with_device(snapshots(true), device(&target)),
            request(&image, VerifyMode::Full),
        )
        .unwrap();
        assert!(matches!(
            worker.submit_confirmation(ConfirmationDecision::Approved),
            Err(SubmitError::NotWaiting(ConfirmationDecision::Approved))
        ));

        let run = drive(&mut worker, |request| {
            assert_eq!(request.block_path, usb_stick().block_path);
            ConfirmationDecision::Approved
        });
        assert!(
            matches!(
                run.outcome,
                OperationOutcome::Completed {
                    verify: VerifySucceeded { skipped: false, .. },
                    ..
                }
            ),
            "{:?}",
            run.outcome
        );
        assert!(
            run.events
                .iter()
                .any(|event| matches!(event, WorkerEvent::WriteGatePassed { .. }))
        );
        assert!(run.events.iter().any(|event| matches!(
            event,
            WorkerEvent::FdBound {
                purpose: OpenPurpose::Write
            }
        )));
        assert_eq!(std::fs::read(&target.0).unwrap(), payload());
        assert_ended(worker, WorkerState::Completed);

        let target = temp_image("worker-approve-cancel", "device", b"");
        let mut worker = spawn_on(
            ScriptedPlatform::with_device(snapshots(false), device(&target)),
            request(&image, VerifyMode::None),
        )
        .unwrap();
        let cancel = worker.cancel_handle();
        let run = drive(&mut worker, move |_| {
            cancel.request_cancel(CancelReason::UserRequested);
            ConfirmationDecision::Approved
        });
        assert!(matches!(
            run.outcome,
            OperationOutcome::Cancelled(CancelledAt::Confirmation)
        ));
        assert!(std::fs::read(&target.0).unwrap().is_empty());
        assert_ended(worker, WorkerState::Cancelled);
    }

    // C: a cancellation requested from the UI thread while the worker
    // waits for the confirmation wins, as it does at the CLI prompt: the
    // operation's own `Cancelled(Confirmation)` comes back, nothing opened.
    #[test]
    fn a_cancellation_from_the_ui_during_confirmation_stops_the_operation() {
        let image = temp_image("worker-cancel", "img", &payload());
        let target = temp_image("worker-cancel-target", "device", b"");
        let mut worker = spawn_on(
            ScriptedPlatform::with_device(snapshots(false), device(&target)),
            request(&image, VerifyMode::Full),
        )
        .unwrap();

        let mut events = Vec::new();
        let outcome = loop {
            match worker.recv().unwrap() {
                WorkerMessage::Event(event) => events.push(event),
                WorkerMessage::ConfirmationRequested(_) => {
                    let cancel = worker.cancel_handle();
                    std::thread::spawn(move || cancel.request_cancel(CancelReason::UserRequested))
                        .join()
                        .unwrap();
                }
                WorkerMessage::Finished(outcome) => break *outcome,
            }
        };
        assert!(matches!(
            outcome,
            OperationOutcome::Cancelled(CancelledAt::Confirmation)
        ));
        assert!(!opened(&events, OpenPurpose::Write));
        assert_ended(worker, WorkerState::Cancelled);

        // A decision that arrives together with a cancellation loses to it.
        let target = temp_image("worker-cancel-target", "device", b"");
        let mut worker = spawn_on(
            ScriptedPlatform::with_device(snapshots(false), device(&target)),
            request(&image, VerifyMode::Full),
        )
        .unwrap();
        let cancel = worker.cancel_handle();
        let run = drive(&mut worker, move |_| {
            cancel.request_cancel(CancelReason::UserRequested);
            ConfirmationDecision::Submitted("/dev/sdx".to_string())
        });
        assert!(matches!(
            run.outcome,
            OperationOutcome::Cancelled(CancelledAt::Confirmation)
        ));
        assert!(std::fs::read(&target.0).unwrap().is_empty());
        assert_ended(worker, WorkerState::Cancelled);
    }

    // C: a cancellation from the UI after the write has begun is honoured
    // at the operation's own points (a write chunk, or right after sync if
    // the write finished first); Verify never starts.
    #[test]
    fn a_cancellation_from_the_ui_during_the_write_stops_before_verify() {
        let data: Vec<u8> = (0..16 * 1024 * 1024u32).map(|i| (i % 247) as u8).collect();
        let image = temp_image("worker-write-cancel", "img", &data);
        let target = temp_image("worker-write-cancel-target", "device", b"");
        let mut worker = spawn_on(
            ScriptedPlatform::with_device(snapshots(true), device(&target)),
            request(&image, VerifyMode::Full),
        )
        .unwrap();

        let mut events = Vec::new();
        let outcome = loop {
            match worker.recv().unwrap() {
                WorkerMessage::Event(event) => {
                    if matches!(event, WorkerEvent::WriteStarted) {
                        worker.request_cancel();
                    }
                    events.push(event);
                }
                WorkerMessage::ConfirmationRequested(_) => worker
                    .submit_confirmation(ConfirmationDecision::Submitted("/dev/sdx".to_string()))
                    .unwrap(),
                WorkerMessage::Finished(outcome) => break *outcome,
            }
        };
        assert!(
            matches!(
                outcome,
                OperationOutcome::Cancelled(CancelledAt::Write { .. } | CancelledAt::AfterSync)
            ),
            "{outcome:?}"
        );
        assert!(!opened(&events, OpenPurpose::Verify));
        assert_ended(worker, WorkerState::Cancelled);
    }

    // E: structured failures reach the UI intact -- the FD binding verdict,
    // OpenDevice's own error, and Verify's first mismatching byte.
    #[test]
    fn failures_reach_the_ui_with_their_structure() {
        let data = payload();
        let image = temp_image("worker-fail", "img", &data);

        let target = temp_image("worker-fail-target", "device", b"");
        let mut worker = spawn_on(
            ScriptedPlatform::with_device(
                snapshots(false),
                Device {
                    wrong_metadata: true,
                    ..device(&target)
                },
            ),
            request(&image, VerifyMode::None),
        )
        .unwrap();
        let run = drive(&mut worker, |_| {
            ConfirmationDecision::Submitted("/dev/sdx".to_string())
        });
        assert!(matches!(
            run.outcome,
            OperationOutcome::Failed(OperationError::WriteDeviceRejected {
                error: WriteGateError::FdBindingMismatch,
                open_device: None,
            })
        ));
        assert_ended(worker, WorkerState::Failed);

        let target = temp_image("worker-fail-target", "device", b"");
        let mut worker = spawn_on(
            ScriptedPlatform::with_device(
                snapshots(false),
                Device {
                    fail: vec![OpenAccess::WriteExclusive],
                    ..device(&target)
                },
            ),
            request(&image, VerifyMode::None),
        )
        .unwrap();
        let run = drive(&mut worker, |_| {
            ConfirmationDecision::Submitted("/dev/sdx".to_string())
        });
        match run.outcome {
            OperationOutcome::Failed(OperationError::WriteDeviceRejected {
                error: WriteGateError::OpenDeviceFailed,
                open_device: Some(OpenDeviceError::Connection(message)),
            }) => assert_eq!(message, "simulated"),
            other => panic!("{other:?}"),
        }
        assert!(run.events.iter().any(|event| matches!(
            event,
            WorkerEvent::DeviceOpenFailed {
                purpose: OpenPurpose::Write
            }
        )));
        assert_ended(worker, WorkerState::Failed);

        let target = temp_image("worker-fail-target", "device", b"");
        let mut tampered = data.clone();
        tampered[4096] ^= 0x5a;
        let other = temp_image("worker-fail-read", "device", &tampered);
        let mut worker = spawn_on(
            ScriptedPlatform::with_device(
                snapshots(true),
                Device {
                    verify_from: Some(other.0.clone()),
                    ..device(&target)
                },
            ),
            request(&image, VerifyMode::Full),
        )
        .unwrap();
        let run = drive(&mut worker, |_| {
            ConfirmationDecision::Submitted("/dev/sdx".to_string())
        });
        match run.outcome {
            OperationOutcome::Failed(OperationError::Verify { failed, .. }) => assert!(matches!(
                failed.reason,
                VerifyFailureReason::Mismatch { offset: 4096, .. }
            )),
            other => panic!("{other:?}"),
        }
        assert_ended(worker, WorkerState::Failed);
    }

    // F: a UI that stops answering (its decision channel closes) is never
    // taken as a yes: the operation ends with no answer, nothing opened,
    // and the worker thread ends.
    #[test]
    fn a_vanished_ui_is_never_a_confirmation() {
        let image = temp_image("worker-gone", "img", &payload());
        let target = temp_image("worker-gone-target", "device", b"");
        let mut worker = spawn_on(
            ScriptedPlatform::with_device(snapshots(false), device(&target)),
            request(&image, VerifyMode::Full),
        )
        .unwrap();

        let mut events = Vec::new();
        let outcome = loop {
            match worker.recv().unwrap() {
                WorkerMessage::Event(event) => events.push(event),
                WorkerMessage::ConfirmationRequested(_) => {
                    worker.decisions = None;
                }
                WorkerMessage::Finished(outcome) => break *outcome,
            }
        };
        assert!(matches!(
            outcome,
            OperationOutcome::Failed(OperationError::ConfirmationInputClosed)
        ));
        assert!(!opened(&events, OpenPurpose::Write));
        assert!(std::fs::read(&target.0).unwrap().is_empty());
        assert_ended(worker, WorkerState::Failed);
    }

    // F: a worker thread that dies without an outcome is reported as Lost,
    // and the panic comes back from `join`.
    #[test]
    fn a_worker_that_dies_is_reported_lost() {
        let image = temp_image("worker-lost", "img", &payload());
        // No scripted snapshot at all: the first fetch panics.
        let mut worker = spawn_on(
            ScriptedPlatform::new(Vec::new()),
            request(&image, VerifyMode::None),
        )
        .unwrap();
        assert!(worker.recv().is_none());
        assert_eq!(worker.state(), WorkerState::Lost);
        assert!(
            worker.removal_target().is_none(),
            "no outcome, no reference"
        );
        assert!(worker.join().is_err());
    }

    // ---- Safe Removal's reference ----

    // Runs one operation to its end (the UI types the device name) and
    // returns the outcome and what `removal_target` gives after `Finished`
    // -- taken before `join`, which then still ends the worker normally.
    fn removal_after(
        platform: ScriptedPlatform,
        image: &TempImage,
        verify_mode: VerifyMode,
    ) -> (OperationOutcome, Option<RemovalTarget>) {
        let mut worker = spawn_on(platform, request(image, verify_mode)).unwrap();
        let run = drive(&mut worker, |_| {
            ConfirmationDecision::Submitted("/dev/sdx".to_string())
        });
        let removal = worker.removal_target();
        let final_state = worker.state();
        assert_ended(worker, final_state);
        (run.outcome, removal)
    }

    // The reference is the device the write started on: the same device
    // and instance pass Safe Removal's check, a recreated one does not.
    fn assert_anchored_to_the_written_device(removal: Option<RemovalTarget>) {
        use super::super::removal::decide;
        use crate::device::{RemovalFacts, RemovalFactsOutcome, UsbTopology};
        let removal = removal.expect("a reference after Finished");
        let facts = |snapshot| {
            RemovalFactsOutcome::Found(Box::new(RemovalFacts {
                snapshot,
                can_power_off: true,
                sibling_id: "/sys/devices/usb2/2-1/2-1:1.0".to_string(),
                other_siblings: 0,
                usb: UsbTopology::Bound { interfaces: 1 },
                filesystems: Vec::new(),
            }))
        };
        assert!(decide(&removal, facts(usb_stick())).is_ok());
        assert!(decide(&removal, facts(recreated())).is_err());
    }

    // After `Finished`, a completed operation offers removal whatever its
    // Verify mode, as do a failed write and a failed Verify -- the write
    // started in each. The reference is taken, then the worker joins.
    #[test]
    fn removal_is_offered_after_an_operation_that_wrote() {
        let data = payload();
        let image = temp_image("removal-offered", "img", &data);
        for mode in [VerifyMode::Quick, VerifyMode::Full, VerifyMode::None] {
            let target = temp_image("removal-offered-target", "device", b"");
            let (outcome, removal) = removal_after(
                ScriptedPlatform::with_device(snapshots(mode != VerifyMode::None), device(&target)),
                &image,
                mode,
            );
            assert!(
                matches!(outcome, OperationOutcome::Completed { .. }),
                "{outcome:?}"
            );
            assert_anchored_to_the_written_device(removal);
        }

        let target = temp_image("removal-offered-target", "device", b"");
        let (outcome, removal) = removal_after(
            ScriptedPlatform::with_device(
                snapshots(false),
                Device {
                    read_only_write_fd: true,
                    ..device(&target)
                },
            ),
            &image,
            VerifyMode::None,
        );
        assert!(matches!(
            outcome,
            OperationOutcome::Failed(OperationError::Write { .. })
        ));
        assert_anchored_to_the_written_device(removal);

        let target = temp_image("removal-offered-target", "device", b"");
        let mut tampered = data.clone();
        tampered[100] ^= 0x5a;
        let other = temp_image("removal-offered-read", "device", &tampered);
        let (outcome, removal) = removal_after(
            ScriptedPlatform::with_device(
                snapshots(true),
                Device {
                    verify_from: Some(other.0.clone()),
                    ..device(&target)
                },
            ),
            &image,
            VerifyMode::Full,
        );
        assert!(matches!(
            outcome,
            OperationOutcome::Failed(OperationError::Verify { .. })
        ));
        assert_anchored_to_the_written_device(removal);
    }

    // No reference when nothing was written (a refused confirmation, a
    // failed FD binding), and none when Verify's own re-check found the
    // target changed -- although the write did start there.
    #[test]
    fn no_removal_without_a_write_or_after_a_changed_verify_target() {
        let image = temp_image("removal-refused", "img", &payload());

        let target = temp_image("removal-refused-target", "device", b"");
        let mut worker = spawn_on(
            ScriptedPlatform::with_device(snapshots(false), device(&target)),
            request(&image, VerifyMode::None),
        )
        .unwrap();
        let run = drive(&mut worker, |_| {
            ConfirmationDecision::Submitted("/dev/sda".to_string())
        });
        assert!(matches!(
            run.outcome,
            OperationOutcome::Failed(OperationError::Confirmation(ConfirmError::Mismatch))
        ));
        assert!(worker.removal_target().is_none());
        assert_ended(worker, WorkerState::Failed);

        let target = temp_image("removal-refused-target", "device", b"");
        let (outcome, removal) = removal_after(
            ScriptedPlatform::with_device(
                snapshots(false),
                Device {
                    wrong_metadata: true,
                    ..device(&target)
                },
            ),
            &image,
            VerifyMode::None,
        );
        assert!(matches!(
            outcome,
            OperationOutcome::Failed(OperationError::WriteDeviceRejected { .. })
        ));
        assert!(removal.is_none());

        let target = temp_image("removal-refused-target", "device", b"");
        let (outcome, removal) = removal_after(
            ScriptedPlatform::with_device(
                vec![
                    found(usb_stick()),
                    found(usb_stick()),
                    found(usb_stick()),
                    found(recreated()),
                ],
                device(&target),
            ),
            &image,
            VerifyMode::Full,
        );
        assert!(
            matches!(
                outcome,
                OperationOutcome::Failed(OperationError::VerifyNotStarted(
                    VerifyNotStarted::TargetCheck { .. }
                ))
            ),
            "{outcome:?}"
        );
        assert!(removal.is_none());
    }

    // A cancellation during the write ends as Cancelled(Write) or, if the
    // write finished first, Cancelled(AfterSync): both offer removal.
    #[test]
    fn removal_is_offered_after_a_cancelled_write() {
        let data: Vec<u8> = (0..16 * 1024 * 1024u32).map(|i| (i % 241) as u8).collect();
        let image = temp_image("removal-cancel", "img", &data);
        let target = temp_image("removal-cancel-target", "device", b"");
        let mut worker = spawn_on(
            ScriptedPlatform::with_device(snapshots(true), device(&target)),
            request(&image, VerifyMode::Full),
        )
        .unwrap();
        let outcome = loop {
            let message = worker.recv().unwrap();
            if !matches!(message, WorkerMessage::Finished(_)) {
                assert!(worker.removal_target().is_none(), "before Finished");
            }
            match message {
                WorkerMessage::Event(event) => {
                    if matches!(event, WorkerEvent::WriteStarted) {
                        worker.request_cancel();
                    }
                }
                WorkerMessage::ConfirmationRequested(_) => worker
                    .submit_confirmation(ConfirmationDecision::Submitted("/dev/sdx".to_string()))
                    .unwrap(),
                WorkerMessage::Finished(outcome) => break *outcome,
            }
        };
        assert!(
            matches!(
                outcome,
                OperationOutcome::Cancelled(CancelledAt::Write { .. } | CancelledAt::AfterSync)
            ),
            "{outcome:?}"
        );
        assert_anchored_to_the_written_device(worker.removal_target());
        assert_ended(worker, WorkerState::Cancelled);
    }

    // What the worker hands out after `Finished` is exactly the shared rule
    // (`removal_allowed`) for every outcome -- including those a scripted
    // run cannot produce (a Verify cancelled mid-way, a failed sync, a sync
    // worker panic, the CLI's test pause) -- and nothing without a started
    // write.
    #[test]
    fn the_worker_applies_the_shared_rule_to_every_outcome() {
        for (name, outcome, offered) in super::super::removal::tests::every_outcome() {
            assert_eq!(
                removal_for(Some(usb_stick()), &outcome).is_some(),
                offered,
                "{name}"
            );
            assert!(removal_for(None, &outcome).is_none(), "{name}");
        }
    }

    // The public message set is unchanged: this match lists every variant
    // and has no wildcard, so an added one fails to compile here.
    #[test]
    fn the_public_messages_are_unchanged() {
        fn every_variant(message: WorkerMessage) {
            match message {
                WorkerMessage::Event(_)
                | WorkerMessage::ConfirmationRequested(_)
                | WorkerMessage::Finished(_) => {}
            }
        }
        let _ = every_variant;
    }
}

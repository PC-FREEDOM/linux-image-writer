// The Production API exactly as an outside crate (a GUI) sees it. Nothing
// here touches a device or D-Bus: the flow from the device list to a
// worker is only type-checked, and the runtime tests use values that need
// no system access. What must stay unreachable is checked by the
// `compile_fail` doc tests in src/lib.rs.

use linux_usb_writer::report::{
    DeviceSnapshot, FdMetadata, SelectionState, VerifyTargetDiagnostics, WriteGateError,
};
use linux_usb_writer::{
    CancelHandle, CancelReason, CancelledAt, ConfirmationDecision, DeviceCandidate, OperationError,
    OperationOutcome, Selectability, SubmitError, TargetRef, VerifyMode, VerifyNotStarted,
    WorkerConfirmationRequest, WorkerEvent, WorkerMessage, WorkerState, WriteOperationRequest,
    WriteWorker, list_candidates, spawn_write_worker,
};

// The whole path a GUI takes, type-checked but never run: list devices,
// take a selectable candidate's opaque reference, build a request, start
// the worker, handle its messages, answer the confirmation with what the
// user typed, cancel, join.
#[allow(dead_code)]
fn gui_flow(image_path: &str, typed: String) -> Option<OperationOutcome> {
    let candidates: Vec<DeviceCandidate> = list_candidates().ok()?;
    let candidate = candidates
        .iter()
        .find(|candidate| candidate.selectability().is_selectable())?;
    let _shown = (
        candidate.display(),
        candidate.assessment(),
        candidate.target().block_path(),
    );

    let target: TargetRef = candidate.target().clone();
    let request = WriteOperationRequest::new(target, image_path, VerifyMode::Full);
    let mut worker: WriteWorker = spawn_write_worker(request).ok()?;
    let cancel: CancelHandle = worker.cancel_handle();

    let mut outcome = None;
    let mut typed = Some(typed);
    while let Some(message) = worker.recv() {
        match message {
            WorkerMessage::Event(_) => {}
            WorkerMessage::ConfirmationRequested(request) => {
                let _expected: &str = &request.expected_text;
                let decision = match typed.take() {
                    Some(text) => ConfirmationDecision::Submitted(text),
                    None => ConfirmationDecision::Cancelled,
                };
                match worker.submit_confirmation(decision) {
                    Ok(()) => {}
                    Err(SubmitError::NotWaiting(_) | SubmitError::WorkerGone(_)) => {
                        cancel.request_cancel(CancelReason::UserRequested)
                    }
                }
            }
            WorkerMessage::Finished(finished) => outcome = Some(*finished),
        }
    }
    worker.join().ok()?;
    outcome
}

// Every message, event, state and outcome is nameable and matchable from
// outside -- no wildcard arms, so a variant whose payload type were not
// public would fail to compile here.
fn describe_message(message: &WorkerMessage) -> &'static str {
    match message {
        WorkerMessage::Event(event) => describe_event(event),
        WorkerMessage::ConfirmationRequested(WorkerConfirmationRequest {
            target,
            block_path,
            diskseq,
            assessment,
            image_size,
            verify_mode,
            expected_text,
        }) => {
            let _ = (
                target,
                block_path,
                diskseq,
                assessment,
                image_size,
                verify_mode,
                expected_text,
            );
            "confirmation"
        }
        WorkerMessage::Finished(outcome) => describe_outcome(outcome),
    }
}

fn describe_event(event: &WorkerEvent) -> &'static str {
    match event {
        WorkerEvent::TargetSelected { .. } => "target",
        WorkerEvent::CompressedImageDetected { .. } => "compressed",
        WorkerEvent::PreflightProgress(_) => "preflight",
        WorkerEvent::ImageSelected { .. } => "image",
        WorkerEvent::Confirmed { .. } => "confirmed",
        WorkerEvent::WriteGatePassed { .. } => "gate",
        WorkerEvent::OpeningDevice { .. } => "opening",
        WorkerEvent::DeviceOpened {
            metadata: Some(FdMetadata { .. }),
            ..
        }
        | WorkerEvent::DeviceOpened { metadata: None, .. } => "opened",
        WorkerEvent::DeviceOpenFailed { .. } => "open failed",
        WorkerEvent::FdBound { .. } => "bound",
        WorkerEvent::WriteAuthorized { .. } => "authorized",
        WorkerEvent::ImageBound => "image bound",
        WorkerEvent::WriteStarted => "write",
        WorkerEvent::WriteProgress(_) => "write progress",
        WorkerEvent::WriteSucceeded { .. } => "written",
        WorkerEvent::SyncStarted => "sync",
        WorkerEvent::SyncOnCallingThread { .. } => "sync here",
        WorkerEvent::SyncSucceeded { .. } => "synced",
        WorkerEvent::VerifyPending { .. } => "verify pending",
        WorkerEvent::VerifySnapshotRequested { .. } => "verify snapshot",
        WorkerEvent::VerifyTargetChecked { diagnostics } => {
            let diagnostics: &VerifyTargetDiagnostics = diagnostics;
            let _: (&DeviceSnapshot, &DeviceSnapshot) =
                (diagnostics.baseline(), diagnostics.current());
            "verify checked"
        }
        WorkerEvent::VerifyStarted => "verify",
        WorkerEvent::VerifyProgress(_) => "verify progress",
    }
}

fn describe_outcome(outcome: &OperationOutcome) -> &'static str {
    match outcome {
        OperationOutcome::Completed { .. } => "completed",
        OperationOutcome::Cancelled(at) => match at {
            CancelledAt::Preflight
            | CancelledAt::BeforeConfirmation
            | CancelledAt::Confirmation
            | CancelledAt::Write { .. }
            | CancelledAt::AfterSync
            | CancelledAt::BeforeVerify
            | CancelledAt::Verify { .. } => "cancelled",
        },
        OperationOutcome::Failed(error) => match error {
            OperationError::Target(_)
            | OperationError::Image(_)
            | OperationError::Confirmation(_)
            | OperationError::ConfirmationInputClosed
            | OperationError::ConfirmationInputFailed(_)
            | OperationError::WriteGate(_)
            | OperationError::WriteDeviceRejected { .. }
            | OperationError::ImageBinding(_)
            | OperationError::ReaderOpen(_)
            | OperationError::Write { .. }
            | OperationError::SyncWorkerPanicked { .. }
            | OperationError::Sync { .. }
            | OperationError::Verify { .. } => "failed",
            OperationError::VerifyNotStarted(
                VerifyNotStarted::TestPauseEnded
                | VerifyNotStarted::TargetCheck { .. }
                | VerifyNotStarted::Start { .. },
            ) => "verify not started",
        },
    }
}

#[test]
fn the_gui_flow_is_expressible_with_the_public_api() {
    let flow: fn(&str, String) -> Option<OperationOutcome> = gui_flow;
    let _ = flow;
}

#[test]
fn outcomes_and_messages_are_plain_values_a_ui_can_inspect() {
    let cancelled = OperationOutcome::Cancelled(CancelledAt::AfterSync);
    assert!(cancelled.is_cancelled());
    assert_eq!(describe_outcome(&cancelled), "cancelled");

    let refused = OperationOutcome::Failed(OperationError::WriteGate(
        WriteGateError::SnapshotRefreshFailed,
    ));
    assert!(!refused.is_cancelled());
    assert_eq!(
        describe_message(&WorkerMessage::Finished(Box::new(refused))),
        "failed"
    );
    assert_eq!(describe_event(&WorkerEvent::SyncStarted), "sync");

    // Read-only data a UI may match on; nothing in the API accepts it.
    assert!(matches!(
        SelectionState::NoSelection,
        SelectionState::NoSelection
    ));
    assert!(matches!(
        ConfirmationDecision::InputClosed,
        ConfirmationDecision::InputClosed
    ));
    assert_ne!(WorkerState::Running, WorkerState::WaitingForConfirmation);
    let _ = Selectability::Selectable;
}

#[test]
fn a_cancel_handle_is_shared_and_one_way() {
    let cancel = CancelHandle::new();
    let shared = cancel.clone();
    assert!(!cancel.is_requested());
    std::thread::spawn(move || shared.request_cancel(CancelReason::UserRequested))
        .join()
        .unwrap();
    assert!(cancel.is_requested());
}

// The values that cross between the UI thread and the worker can be sent.
#[test]
fn boundary_values_can_cross_threads() {
    fn crosses<T: Send + 'static>() {}
    crosses::<WriteOperationRequest>();
    crosses::<WorkerMessage>();
    crosses::<ConfirmationDecision>();
    crosses::<CancelHandle>();
    crosses::<OperationOutcome>();
}

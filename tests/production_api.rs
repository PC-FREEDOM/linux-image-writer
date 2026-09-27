// The Production API exactly as an outside crate (a GUI) sees it. Nothing
// here touches a device or D-Bus: the flow from the device list to a
// worker is only type-checked, and the runtime tests use values that need
// no system access. What must stay unreachable is checked by the
// `compile_fail` doc tests in src/lib.rs.

use linux_usb_writer::report::{
    CompressionFormat, DeviceSnapshot, FdMetadata, ImageSourceError, SelectionState,
    UnsupportedCompression, VerifyTargetDiagnostics, WriteGateError,
};
use linux_usb_writer::{
    CancelHandle, CancelReason, CancelledAt, ConfirmationDecision, DeviceCandidate, ImageAccess,
    ImageInfo, OperationError, OperationOutcome, Selectability, SubmitError, TargetRef,
    VerifyAvailability, VerifyMode, VerifyNotStarted, VerifyUnavailableReason,
    WorkerConfirmationRequest, WorkerEvent, WorkerMessage, WorkerState, WriteOperationRequest,
    WriteWorker, inspect_image, list_candidates, spawn_write_worker,
};

// The whole path a GUI takes, type-checked but never run: list devices,
// take a selectable candidate's opaque reference, keep it across a refresh
// only while the refreshed entry is the same device and instance, inspect
// the image and pick a Verify mode its format allows, build a request,
// start the worker, handle its messages, answer the confirmation (the
// user's explicit approval, or typed text), cancel, join.
#[allow(dead_code)]
fn gui_flow(image_path: &str, typed: Option<String>) -> Option<OperationOutcome> {
    let candidates: Vec<DeviceCandidate> = list_candidates().ok()?;
    let candidate = candidates
        .iter()
        .find(|candidate| candidate.selectability().is_selectable())?;
    let _shown = (
        candidate.display(),
        candidate.assessment(),
        candidate.target().block_path(),
        candidate.diskseq(),
        candidate.major_minor(),
    );
    let mut target: TargetRef = candidate.target().clone();

    let refreshed: Vec<DeviceCandidate> = list_candidates().ok()?;
    target = refreshed
        .iter()
        .find(|entry| entry.is_same_device_as(&target))?
        .target()
        .clone();

    let image: ImageInfo = inspect_image(image_path).ok()?;
    let verify_mode = match image.verify_availability(VerifyMode::Quick) {
        VerifyAvailability::Available => VerifyMode::Quick,
        VerifyAvailability::Unavailable(VerifyUnavailableReason::NeedsRandomAccess) => {
            VerifyMode::Full
        }
    };
    let request = WriteOperationRequest::new(target, image_path, verify_mode);
    let mut worker: WriteWorker = spawn_write_worker(request).ok()?;
    let cancel: CancelHandle = worker.cancel_handle();

    let mut outcome = None;
    let mut typed = typed;
    while let Some(message) = worker.recv() {
        match message {
            WorkerMessage::Event(_) => {}
            WorkerMessage::ConfirmationRequested(request) => {
                let _expected: &str = &request.expected_text;
                let decision = match typed.take() {
                    Some(text) => ConfirmationDecision::Submitted(text),
                    None => ConfirmationDecision::Approved,
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
    let flow: fn(&str, Option<String>) -> Option<OperationOutcome> = gui_flow;
    let _ = flow;
}

// Every answer a UI can give is nameable from outside.
#[allow(dead_code)]
fn describe_decision(decision: &ConfirmationDecision) -> &'static str {
    match decision {
        ConfirmationDecision::Submitted(_) => "typed",
        ConfirmationDecision::Approved => "approved",
        ConfirmationDecision::InputClosed => "closed",
        ConfirmationDecision::InputFailed(_) => "failed",
        ConfirmationDecision::Cancelled => "cancelled",
    }
}

// A temporary file for inspection, removed when dropped.
struct TempFile(std::path::PathBuf);

impl TempFile {
    fn new(name: &str, contents: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!(
            "linux-usb-writer-api-{}-{name}",
            std::process::id()
        ));
        std::fs::write(&path, contents).unwrap();
        TempFile(path)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// Inspection from outside: plain data a UI can show and match on. (Only
// the leading bytes decide the format; nothing is decoded.)
#[test]
fn an_image_can_be_inspected_for_display() {
    let raw = TempFile::new("raw.img", &[0u8; 4096]);
    let info = inspect_image(&raw.0).unwrap();
    assert_eq!(info.file_size(), 4096);
    assert_eq!(info.logical_size(), Some(4096));
    assert_eq!(info.compression(), None);
    assert_eq!(info.access(), ImageAccess::RandomAccess);
    for mode in [VerifyMode::None, VerifyMode::Quick, VerifyMode::Full] {
        assert_eq!(
            info.verify_availability(mode),
            VerifyAvailability::Available
        );
    }

    let gzip = TempFile::new("image.img.gz", &[0x1F, 0x8B, 0x08, 0x00, 0x00, 0x00]);
    let info = inspect_image(&gzip.0).unwrap();
    assert_eq!(info.compression(), Some(CompressionFormat::Gzip));
    assert_eq!(info.access(), ImageAccess::SequentialReplay);
    assert_eq!(info.logical_size(), None);
    assert_eq!(
        info.verify_availability(VerifyMode::Quick),
        VerifyAvailability::Unavailable(VerifyUnavailableReason::NeedsRandomAccess)
    );
    assert_eq!(
        info.verify_availability(VerifyMode::Full),
        VerifyAvailability::Available
    );

    let zstd = TempFile::new("image.img.zst", &[0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x00]);
    assert!(matches!(
        inspect_image(&zstd.0),
        Err(ImageSourceError::UnsupportedFormat(
            UnsupportedCompression::Zstd
        ))
    ));
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
    crosses::<ImageInfo>();
}

// What a write operation tells its caller while it runs, and the one
// question it asks (orchestration, Core layer). `run_write_operation` owns
// the whole safe sequence; a caller (the CLI today, a GUI later) only
// observes it through `OperationObserver`:
//
//   - `on_event`: one-way notices, in the order the steps happen. They carry
//     the data a caller needs to show progress, borrowed for the duration of
//     the call; nothing in them can be used to act on the device.
//   - `request_confirmation`: the human confirmation. The observer shows the
//     request and returns what the user typed, or that the user explicitly
//     approved it (a GUI's button), or why there is no answer; whether a
//     typed answer matches is decided by the operation, never by the
//     observer.
//
// Events and the observer are synchronous and carry no `Send` bound: the
// operation runs on the caller's thread, and a GUI is expected to run the
// whole operation on a worker thread of its own.

use super::operation::{ConfirmationRequest, ConfirmedSummary};
use crate::device::DeviceSnapshot;
use crate::execution::core::{SelectionState, VerifyMode, VerifyTargetDiagnostics};
use crate::execution::linux_access::{FdMetadata, OpenDeviceError};
use crate::execution::write_job::VerifyProgress;
use crate::image_source::CompressionFormat;
use crate::image_source::compressed::PreflightProgress;
use crate::writer::{WritePlan, WriteProgress, WritebackProgress};

// Which of the operation's two device opens an event is about.
/// Which of the operation's two device opens an event is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenPurpose {
    // `OpenAccess::WriteExclusive` (read-write, O_EXCL).
    Write,
    // `OpenAccess::ReadOnlyDirect` (read-only, O_DIRECT, no O_EXCL).
    Verify,
}

#[derive(Debug)]
pub(crate) enum OperationEvent<'a> {
    // The target was selected and passed the immediate re-verification.
    TargetSelected {
        state: &'a SelectionState,
    },
    // The image is gzip or xz; Preflight (a full decode) starts next.
    CompressedImageDetected {
        format: CompressionFormat,
    },
    PreflightProgress(PreflightProgress),
    // The image is opened (and, if compressed, validated); `image_size` is
    // its logical (decoded) size.
    ImageSelected {
        image_size: u64,
    },
    // The typed confirmation matched; this is what was confirmed.
    Confirmed(ConfirmedSummary<'a>),
    // The fresh Write Gate (before OpenDevice) passed with this plan.
    WriteGatePassed {
        plan: WritePlan,
    },
    // OpenDevice is about to be requested (a polkit prompt may appear).
    OpeningDevice {
        purpose: OpenPurpose,
        block_path: &'a str,
    },
    // OpenDevice returned an FD; `metadata` is what the kernel reports for
    // it, which the FD binding check uses next.
    DeviceOpened {
        purpose: OpenPurpose,
        metadata: Option<&'a FdMetadata>,
    },
    DeviceOpenFailed {
        purpose: OpenPurpose,
        // Read by the CLI binary's observer; the worker (the library) takes
        // the same error from the outcome instead.
        #[allow(dead_code)]
        error: &'a OpenDeviceError,
    },
    // The FD is bound to the re-verified device (major:minor, size, diskseq).
    FdBound {
        purpose: OpenPurpose,
    },
    // The Write Gate authorized this write (`PreparedWrite` -> `AuthorizedWrite`).
    WriteAuthorized {
        target_block_path: &'a str,
        target_size: u64,
        image_size: u64,
        verify_mode: VerifyMode,
    },
    // The authorization is bound to the confirmed image.
    ImageBound,
    WriteStarted,
    // Bytes the kernel accepted (`write()` returned success); not
    // necessarily written back to the device.
    WriteProgress(WriteProgress),
    // Bytes an explicit sync confirmed as written back to the device. Sent
    // only from a sync result, never from `write()` returns: after the
    // final `fsync()` (before `SyncSucceeded`), and after a cancelled
    // write's drain (before its FD is closed). Absent until then.
    WritebackProgress(WritebackProgress),
    WriteSucceeded {
        bytes_written: u64,
        image_size: u64,
    },
    // The write stopped at a cancellation; the data already handed to the
    // kernel is now being written back on the still-open FD (O_EXCL held),
    // before the FD is closed. Not cancellable; the outcome follows once
    // it is done.
    CancelDrainStarted {
        bytes_written: u64,
    },
    // No worker thread could be started for the drain; it runs on the
    // calling thread instead (it is never skipped).
    CancelDrainOnCallingThread {
        error: &'a std::io::Error,
    },
    SyncStarted,
    // No worker thread could be started for sync; it runs on the calling
    // thread instead (it is never skipped).
    SyncOnCallingThread {
        error: &'a std::io::Error,
    },
    SyncSucceeded {
        bytes_written: u64,
    },
    // Quick or Full Verify will follow (the write FD is already closed).
    VerifyPending {
        mode: VerifyMode,
    },
    // A fresh snapshot of the target is being read for Verify.
    VerifySnapshotRequested {
        block_path: &'a str,
    },
    // The target passed Verify's re-check; the comparison it was based on.
    VerifyTargetChecked {
        diagnostics: &'a VerifyTargetDiagnostics,
    },
    VerifyStarted,
    VerifyProgress(VerifyProgress),
}

// The observer's answer to a confirmation request. `Submitted` and
// `Approved` are the two ways a human confirms (the CLI's typed text, a
// GUI's button); either one is only an answer to the request that was
// shown, and either one continues through the same WriteIntent,
// ConfirmationToken and Write Gate.
/// The answer to a confirmation request: what the user typed (compared by the
/// operation), the user's explicit approval of the request as shown, or why
/// there is no answer.
#[derive(Debug)]
pub enum ConfirmationDecision {
    // What the user typed, as entered; the operation compares it.
    Submitted(String),
    // The user was shown this request and explicitly approved it (e.g. a
    // "Write to USB" button in the final confirmation). Not "no
    // confirmation needed": it answers only the pending request.
    // Given by the library's users (a GUI); the CLI binary asks for typed
    // text and never builds it.
    /// The user was shown the pending request and explicitly approved it
    /// (e.g. a GUI's "Write" button). It answers that request only.
    #[allow(dead_code)]
    Approved,
    // There will be no answer (e.g. the input was closed).
    InputClosed,
    // Reading the answer failed.
    InputFailed(std::io::Error),
    // The user cancelled while being asked.
    Cancelled,
}

pub(crate) trait OperationObserver {
    fn on_event(&mut self, event: OperationEvent<'_>);

    fn request_confirmation(&mut self, request: &ConfirmationRequest<'_>) -> ConfirmationDecision;

    // TEST-ONLY hook (`write-test --test-pause-before-verify`): called after
    // write + sync, strictly before Verify reads its fresh snapshot, with no
    // device FD open. `false` stops before Verify. Only the CLI's test flag
    // overrides it.
    fn pause_before_verify(&mut self) -> bool {
        true
    }

    // Called once, right after the write started (`begin_write` succeeded),
    // with the snapshot the write FD was bound to. Not a progress event:
    // the worker keeps it as the anchor of a `RemovalTarget`, handed out
    // only after the operation ended (Safe Removal). Everyone else ignores
    // it.
    fn write_started_on(&mut self, _bound: DeviceSnapshot) {}
}

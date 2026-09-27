// What a write operation tells its caller while it runs, and the one
// question it asks (orchestration, Core layer). `run_write_operation` owns
// the whole safe sequence; a caller (the CLI today, a GUI later) only
// observes it through `OperationObserver`:
//
//   - `on_event`: one-way notices, in the order the steps happen. They carry
//     the data a caller needs to show progress, borrowed for the duration of
//     the call; nothing in them can be used to act on the device.
//   - `request_confirmation`: the human confirmation. The observer shows the
//     request and returns what the user typed (or why there is no answer);
//     whether it matches is decided by the operation, never by the observer.
//
// Events and the observer are synchronous and carry no `Send` bound: the
// operation runs on the caller's thread, and a GUI is expected to run the
// whole operation on a worker thread of its own.

use super::operation::{ConfirmationRequest, ConfirmedSummary};
use crate::execution::core::{SelectionState, VerifyMode, VerifyTargetDiagnostics};
use crate::execution::linux_access::{FdMetadata, OpenDeviceError};
use crate::execution::write_job::VerifyProgress;
use crate::image_source::CompressionFormat;
use crate::image_source::compressed::PreflightProgress;
use crate::writer::{WritePlan, WriteProgress};

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
    WriteProgress(WriteProgress),
    WriteSucceeded {
        bytes_written: u64,
        image_size: u64,
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

// The observer's answer to a confirmation request.
/// The answer to a confirmation request: what the user typed (compared by the
/// operation), or why there is none.
#[derive(Debug)]
pub enum ConfirmationDecision {
    // What the user typed, as entered; the operation compares it.
    Submitted(String),
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
}

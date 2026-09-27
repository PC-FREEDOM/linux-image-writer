// How a write operation ended (orchestration, Core layer). Every variant
// keeps the existing structured payload of the step that ended it -- the
// same error types the steps themselves return -- so a caller can report it
// in its own words. Where the operation had written the image, `image_size`
// is the logical size of the image it was bound to.

use super::operation::{ConfirmError, PrepareImageError, TargetNotReady};
use crate::execution::core::{VerifyTargetDiagnostics, WriteGateError};
use crate::execution::linux_access::OpenDeviceError;
use crate::execution::write_job::{
    Cancelled as WriteCancelled, Failed, ImageBindingError, VerifyCancelled, VerifyFailed,
    VerifyStartError, VerifySucceeded,
};

#[derive(Debug)]
pub(crate) enum OperationOutcome {
    // Write and sync succeeded, and Verify succeeded (or `VerifyMode::None`
    // skipped it: `verify.skipped`).
    Completed {
        verify: VerifySucceeded,
        image_size: u64,
    },
    Cancelled(CancelledAt),
    Failed(OperationError),
}

// Where a cancellation stopped the operation -- always at one of the
// existing cancel points.
#[derive(Debug)]
pub(crate) enum CancelledAt {
    // During a compressed image's Preflight; nothing was opened on the target.
    Preflight,
    // After the image was prepared, before the confirmation was shown.
    BeforeConfirmation,
    // While the user was being asked to confirm.
    Confirmation,
    // During the write (the writer's own check, once per chunk).
    Write {
        cancelled: WriteCancelled,
        image_size: u64,
    },
    // After a successful sync; Verify was not started.
    AfterSync,
    // After write + sync, before Verify opened anything.
    BeforeVerify,
    // During Verify (the verifier's own check, once per chunk).
    Verify {
        cancelled: VerifyCancelled,
        image_size: u64,
    },
}

#[derive(Debug)]
pub(crate) enum OperationError {
    // ---- before the target is opened ----
    Target(TargetNotReady),
    Image(PrepareImageError),
    Confirmation(ConfirmError),
    ConfirmationInputClosed,
    ConfirmationInputFailed(std::io::Error),
    // The fresh Write Gate refused before OpenDevice.
    WriteGate(WriteGateError),
    // ---- the write FD ----
    // The Write Gate refused after OpenDevice (`OpenDeviceFailed`, or the
    // FD binding). `open_device` is OpenDevice's own error when that is why
    // (the CLI prints it from `OperationEvent::DeviceOpenFailed` instead).
    WriteDeviceRejected {
        error: WriteGateError,
        #[allow(dead_code)] // kept for callers that report from the outcome; tests read it
        open_device: Option<OpenDeviceError>,
    },
    ImageBinding(ImageBindingError),
    // The image reader for the write could not be opened (0 bytes written).
    ReaderOpen(std::io::Error),
    Write {
        failed: Failed,
        image_size: u64,
    },
    // The sync worker panicked: durability is not confirmed.
    SyncWorkerPanicked {
        cancel_requested: bool,
    },
    Sync {
        failed: Failed,
        cancel_requested: bool,
        image_size: u64,
    },
    // ---- Verify (write + sync succeeded) ----
    VerifyNotStarted(VerifyNotStarted),
    Verify {
        failed: VerifyFailed,
        image_size: u64,
    },
}

#[derive(Debug)]
pub(crate) enum VerifyNotStarted {
    // The test-only pause before Verify got no input.
    TestPauseEnded,
    // Verify's target re-check refused; `diagnostics` is `None` only when
    // no fresh snapshot could be read.
    TargetCheck {
        error: VerifyStartError,
        diagnostics: Option<VerifyTargetDiagnostics>,
        image_size: u64,
    },
    // OpenDevice for Verify, the Verify FD binding, or its direct-read
    // setup refused. `open_device` is OpenDevice's own error when that is
    // why (the CLI prints it from `OperationEvent::DeviceOpenFailed`).
    Start {
        error: VerifyStartError,
        #[allow(dead_code)] // kept for callers that report from the outcome; tests read it
        open_device: Option<OpenDeviceError>,
        image_size: u64,
    },
}

impl OperationOutcome {
    pub(crate) fn is_cancelled(&self) -> bool {
        matches!(self, OperationOutcome::Cancelled(_))
    }
}

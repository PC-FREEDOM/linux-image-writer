// A running write operation as the GUI shows it: what the worker's messages
// say it is doing (`Tracker`), the three user-facing steps (prepare, write,
// verify), what Cancel does at each point, and how the operation's outcome
// reads. No GTK here, so all of it is unit tested.
//
// Nothing here is a Safety decision or a copy of the operation's own state
// machine: the operation runs on the library's worker thread, decides
// everything itself, and only reports what happened (`WorkerEvent`,
// `OperationOutcome`). This module maps those reports to what is shown, and
// the user's answers (the final confirmation, Cancel) to what is sent back.

use linux_usb_writer::report::{
    CompressionFormat, OpenDeviceError, PrepareImageError, SelectTargetError, SelectionState,
    TargetNotReady, VerifyFailureReason,
};
use linux_usb_writer::{
    CancelledAt, ConfirmationDecision, DeviceDisplay, OpenPurpose, OperationError,
    OperationOutcome, VerifyMode, VerifyNotStarted, WorkerEvent,
};

// ---- What the operation is doing ----

// The operation's latest reported activity. Derived only from the worker's
// messages (and the user's answer to the confirmation), never assumed ahead
// of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    // The worker started; the target is being selected and re-checked.
    Starting,
    // The target was selected; the image is being opened.
    PreparingImage,
    // A compressed image is being validated (Preflight).
    Preflight,
    // The final confirmation is shown and awaits the user's answer.
    AwaitingConfirmation,
    // The user approved; the operation re-checks the target before opening it.
    StartingWrite,
    // OpenDevice was requested (a system authentication prompt may appear).
    OpeningDevice(OpenPurpose),
    Writing,
    // Write and sync: the data is being committed to the device.
    Syncing,
    // Write and sync are done; Verify re-checks the target and opens it.
    PreparingVerify,
    Verifying,
    // The outcome arrived.
    Finished,
}

// The steps the user sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Prepare,
    Write,
    Verify,
}

pub const STEPS: [Step; 3] = [Step::Prepare, Step::Write, Step::Verify];

// How far one step is, as shown next to it (always with words, not only an
// icon or a color).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Waiting,
    Active,
    Done,
    // Verify when the mode is None: never shown as done.
    Skipped,
    Cancelled,
    Failed,
}

// Bytes done out of a total, as a progress report gave them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transfer {
    pub done: u64,
    pub total: u64,
}

impl Transfer {
    // 0.0 ..= 1.0; an empty total counts as nothing done.
    pub fn fraction(self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            (self.done.min(self.total) as f64) / (self.total as f64)
        }
    }

    // Whole percent, rounded down: 100 only once everything is done.
    pub fn percent(self) -> u64 {
        if self.total == 0 {
            0
        } else {
            (u128::from(self.done.min(self.total)) * 100 / u128::from(self.total)) as u64
        }
    }
}

// What the worker has reported so far, for display.
#[derive(Debug, Clone)]
pub struct Tracker {
    pub activity: Activity,
    // The mode the request named (shown before the operation confirms it).
    pub verify_mode: VerifyMode,
    // From the operation's own events: the target as it selected it, the
    // image as it opened it.
    pub target: Option<DeviceDisplay>,
    pub block_path: Option<String>,
    pub diskseq: Option<u64>,
    pub major_minor: Option<(u32, u32)>,
    pub compression: Option<CompressionFormat>,
    pub compressed_size: Option<u64>,
    pub image_size: Option<u64>,
    pub preflight: Option<Transfer>,
    pub written: Option<Transfer>,
    pub verified: Option<Transfer>,
    // The user asked to stop (Cancel, or declining the confirmation); the
    // operation decides where it actually stops.
    pub cancel_requested: bool,
}

impl Tracker {
    pub fn new(verify_mode: VerifyMode) -> Self {
        Tracker {
            activity: Activity::Starting,
            verify_mode,
            target: None,
            block_path: None,
            diskseq: None,
            major_minor: None,
            compression: None,
            compressed_size: None,
            image_size: None,
            preflight: None,
            written: None,
            verified: None,
            cancel_requested: false,
        }
    }

    pub fn apply(&mut self, event: &WorkerEvent) {
        match event {
            WorkerEvent::TargetSelected {
                target,
                block_path,
                diskseq,
                ..
            } => {
                self.target = Some(target.clone());
                self.block_path = Some(block_path.clone());
                self.diskseq = *diskseq;
                self.activity = Activity::PreparingImage;
            }
            WorkerEvent::CompressedImageDetected { format } => {
                self.compression = Some(*format);
                self.activity = Activity::Preflight;
            }
            WorkerEvent::PreflightProgress(progress) => {
                self.compressed_size = Some(progress.compressed_total);
                self.preflight = Some(Transfer {
                    done: progress.compressed_consumed,
                    total: progress.compressed_total,
                });
            }
            WorkerEvent::ImageSelected { image_size } => {
                self.image_size = Some(*image_size);
                self.activity = Activity::PreparingImage;
            }
            WorkerEvent::Confirmed { verify_mode, .. } => {
                self.verify_mode = *verify_mode;
                self.activity = Activity::StartingWrite;
            }
            WorkerEvent::WriteGatePassed { .. }
            | WorkerEvent::FdBound {
                purpose: OpenPurpose::Write,
            }
            | WorkerEvent::WriteAuthorized { .. }
            | WorkerEvent::ImageBound => self.activity = Activity::StartingWrite,
            WorkerEvent::OpeningDevice { purpose, .. } => {
                self.activity = Activity::OpeningDevice(*purpose);
            }
            WorkerEvent::DeviceOpened { purpose, metadata } => {
                if let Some(metadata) = metadata {
                    self.major_minor = Some((metadata.major, metadata.minor));
                }
                self.activity = match purpose {
                    OpenPurpose::Write => Activity::StartingWrite,
                    OpenPurpose::Verify => Activity::PreparingVerify,
                };
            }
            WorkerEvent::DeviceOpenFailed { .. } => {}
            WorkerEvent::WriteStarted => {
                self.activity = Activity::Writing;
                if let Some(total) = self.image_size {
                    self.written = Some(Transfer { done: 0, total });
                }
            }
            WorkerEvent::WriteProgress(progress) => {
                self.activity = Activity::Writing;
                self.written = Some(Transfer {
                    done: progress.bytes_written,
                    total: progress.total_bytes,
                });
            }
            WorkerEvent::WriteSucceeded {
                bytes_written,
                image_size,
            } => {
                self.written = Some(Transfer {
                    done: *bytes_written,
                    total: *image_size,
                });
            }
            WorkerEvent::SyncStarted
            | WorkerEvent::SyncOnCallingThread { .. }
            | WorkerEvent::SyncSucceeded { .. } => self.activity = Activity::Syncing,
            WorkerEvent::VerifyPending { mode } => {
                self.verify_mode = *mode;
                self.activity = Activity::PreparingVerify;
            }
            WorkerEvent::VerifySnapshotRequested { .. }
            | WorkerEvent::VerifyTargetChecked { .. }
            | WorkerEvent::FdBound {
                purpose: OpenPurpose::Verify,
            } => self.activity = Activity::PreparingVerify,
            WorkerEvent::VerifyStarted => self.activity = Activity::Verifying,
            WorkerEvent::VerifyProgress(progress) => {
                self.activity = Activity::Verifying;
                self.verified = Some(Transfer {
                    done: progress.verified_bytes,
                    total: progress.total_bytes,
                });
            }
        }
    }

    // The worker asked for the final confirmation.
    pub fn confirmation_requested(&mut self) {
        self.activity = Activity::AwaitingConfirmation;
    }

    // The user's answer to the confirmation was delivered.
    pub fn answered(&mut self, approved: bool) {
        if approved {
            self.activity = Activity::StartingWrite;
        } else {
            self.cancel_requested = true;
        }
    }

    pub fn finished(&mut self) {
        self.activity = Activity::Finished;
    }

    // The step the operation is in (for a finished one, see `Ending`).
    pub fn step(&self) -> Step {
        match self.activity {
            Activity::Starting
            | Activity::PreparingImage
            | Activity::Preflight
            | Activity::AwaitingConfirmation => Step::Prepare,
            Activity::StartingWrite
            | Activity::OpeningDevice(OpenPurpose::Write)
            | Activity::Writing
            | Activity::Syncing => Step::Write,
            Activity::OpeningDevice(OpenPurpose::Verify)
            | Activity::PreparingVerify
            | Activity::Verifying
            | Activity::Finished => Step::Verify,
        }
    }

    // How each step is shown while the operation runs.
    pub fn marks(&self) -> [Mark; 3] {
        let verify_waiting = if self.verify_mode == VerifyMode::None {
            Mark::Skipped
        } else {
            Mark::Waiting
        };
        match self.step() {
            Step::Prepare => [Mark::Active, Mark::Waiting, verify_waiting],
            Step::Write => [Mark::Done, Mark::Active, verify_waiting],
            Step::Verify => [Mark::Done, Mark::Done, Mark::Active],
        }
    }

    // What the progress bar shows now, if anything.
    pub fn transfer(&self) -> Option<Transfer> {
        match self.activity {
            Activity::Preflight => self.preflight,
            Activity::Writing | Activity::Syncing => self.written,
            Activity::Verifying => self.verified,
            _ => None,
        }
    }

    pub fn cancel_action(&self) -> CancelAction {
        if self.activity == Activity::Finished || self.cancel_requested {
            return CancelAction::Unavailable;
        }
        if self.activity == Activity::AwaitingConfirmation {
            return CancelAction::Decline;
        }
        match self.step() {
            Step::Prepare | Step::Verify => CancelAction::RequestNow,
            // Once the user approved, the write may start at any moment:
            // stopping it can leave an incomplete image, so it is asked
            // once. Sync is part of this step.
            Step::Write => CancelAction::AskFirst,
        }
    }
}

// What pressing Cancel does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelAction {
    // Ask the operation to stop now (nothing destructive has started, or
    // the write already finished).
    RequestNow,
    // The confirmation is pending: answer it with a refusal.
    Decline,
    // Ask the user once whether to stop the write.
    AskFirst,
    // Already requested, or finished: the button is disabled.
    Unavailable,
}

// The final confirmation's answer for the dialog response the user chose:
// only the write button approves; anything else (Cancel, Escape, closing)
// declines.
pub const CONFIRM_RESPONSE_WRITE: &str = "write";
pub const CONFIRM_RESPONSE_CANCEL: &str = "cancel";

pub fn decision_for(response: &str) -> ConfirmationDecision {
    if response == CONFIRM_RESPONSE_WRITE {
        ConfirmationDecision::Approved
    } else {
        ConfirmationDecision::Cancelled
    }
}

// ---- How the operation ended ----

// Where a failure happened, for the steps shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    // Written, synced and verified (Quick or Full).
    Verified(VerifyMode),
    // Written and synced; Verify was not requested.
    WrittenWithoutVerify,
    // Stopped before anything was opened on the target.
    CancelledBeforeWrite,
    // Stopped during the write (sync included).
    CancelledDuringWrite {
        target_modified: bool,
    },
    // Written and synced; Verify was stopped or never started.
    CancelledDuringVerify,
    // Refused before any byte was written; `at` is the step shown as failed.
    NotStarted {
        at: Step,
        reason: Reason,
    },
    // The write or sync failed.
    WriteFailed {
        reason: Reason,
        target_modified: bool,
    },
    // Written and synced; Verify failed or could not start.
    VerifyFailed {
        reason: Reason,
    },
    // The worker ended without an outcome (it panicked).
    Lost,
}

// Why an operation failed, in the user's terms (worded in `text`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    TargetNotFound,
    TargetUnreadable,
    TargetChanged,
    TargetNotSelectable,
    ImageUnreadable,
    ImageRefused,
    QuickVerifyUnsupported,
    CompressedImageDamaged,
    CompressedImageTooLarge,
    CompressedImageTooDemanding,
    ImageChanged,
    ConfirmationFailed,
    TargetRecheckFailed,
    AccessDenied,
    AuthenticationCancelled,
    DeviceBusyOrRefused,
    SystemServiceUnavailable,
    OpenedDeviceMismatch,
    WriteError,
    ImageChangedDuringWrite,
    SyncError,
    VerifyTargetChanged,
    VerifyOpenFailed,
    VerifyDirectReadUnavailable,
    VerifyNotStarted,
    VerifyMismatch,
    VerifyReadError,
    VerifyLengthMismatch,
}

pub fn ending(outcome: &OperationOutcome) -> Ending {
    match outcome {
        OperationOutcome::Completed { verify, .. } => {
            if verify.skipped {
                Ending::WrittenWithoutVerify
            } else {
                Ending::Verified(verify.mode)
            }
        }
        OperationOutcome::Cancelled(at) => match at {
            CancelledAt::Preflight
            | CancelledAt::BeforeConfirmation
            | CancelledAt::Confirmation => Ending::CancelledBeforeWrite,
            CancelledAt::Write { cancelled, .. } => Ending::CancelledDuringWrite {
                target_modified: cancelled.target_may_be_modified,
            },
            CancelledAt::AfterSync | CancelledAt::BeforeVerify | CancelledAt::Verify { .. } => {
                Ending::CancelledDuringVerify
            }
        },
        OperationOutcome::Failed(error) => failure(error),
    }
}

fn failure(error: &OperationError) -> Ending {
    let before = |at, reason| Ending::NotStarted { at, reason };
    match error {
        OperationError::Target(not_ready) => before(Step::Prepare, target_reason(not_ready)),
        OperationError::Image(error) => before(Step::Prepare, image_reason(error)),
        OperationError::Confirmation(_)
        | OperationError::ConfirmationInputClosed
        | OperationError::ConfirmationInputFailed(_) => {
            before(Step::Prepare, Reason::ConfirmationFailed)
        }
        OperationError::WriteGate(_) => before(Step::Write, Reason::TargetRecheckFailed),
        OperationError::WriteDeviceRejected { open_device, .. } => before(
            Step::Write,
            match open_device {
                Some(error) => open_reason(error),
                None => Reason::OpenedDeviceMismatch,
            },
        ),
        OperationError::ImageBinding(_) => before(Step::Write, Reason::ConfirmationFailed),
        OperationError::ReaderOpen(_) => before(Step::Write, Reason::ImageUnreadable),
        OperationError::Write { failed, .. } => Ending::WriteFailed {
            reason: match failed.cause {
                linux_usb_writer::report::WriteJobFailureCause::SourceChanged(_) => {
                    Reason::ImageChangedDuringWrite
                }
                _ => Reason::WriteError,
            },
            target_modified: failed.target_may_be_modified,
        },
        OperationError::SyncWorkerPanicked { .. } => Ending::WriteFailed {
            reason: Reason::SyncError,
            target_modified: true,
        },
        OperationError::Sync { failed, .. } => Ending::WriteFailed {
            reason: Reason::SyncError,
            target_modified: failed.target_may_be_modified,
        },
        OperationError::VerifyNotStarted(not_started) => Ending::VerifyFailed {
            reason: match not_started {
                VerifyNotStarted::TestPauseEnded => Reason::VerifyNotStarted,
                VerifyNotStarted::TargetCheck { .. } => Reason::VerifyTargetChanged,
                VerifyNotStarted::Start { error, .. } => match error {
                    linux_usb_writer::report::VerifyStartError::DirectReadUnavailable(_) => {
                        Reason::VerifyDirectReadUnavailable
                    }
                    _ => Reason::VerifyOpenFailed,
                },
            },
        },
        OperationError::Verify { failed, .. } => Ending::VerifyFailed {
            reason: match failed.reason {
                VerifyFailureReason::Mismatch { .. } => Reason::VerifyMismatch,
                VerifyFailureReason::SourceReadError(_)
                | VerifyFailureReason::TargetReadError(_) => Reason::VerifyReadError,
                VerifyFailureReason::SourceUnexpectedEof
                | VerifyFailureReason::TargetUnexpectedEof => Reason::VerifyLengthMismatch,
                VerifyFailureReason::UnsupportedAccess => Reason::QuickVerifyUnsupported,
                VerifyFailureReason::SourceChanged(_) => Reason::ImageChanged,
            },
        },
    }
}

fn target_reason(not_ready: &TargetNotReady) -> Reason {
    match not_ready {
        TargetNotReady::Select(error) => match error {
            SelectTargetError::NotFound => Reason::TargetNotFound,
            SelectTargetError::SnapshotUnavailable(_) => Reason::TargetUnreadable,
            SelectTargetError::CandidateChanged(_) => Reason::TargetChanged,
            SelectTargetError::NotSelectable(_) => Reason::TargetNotSelectable,
        },
        TargetNotReady::NotReady(_) => Reason::TargetChanged,
    }
}

fn image_reason(error: &PrepareImageError) -> Reason {
    use linux_usb_writer::report::{CompressedImageRejection, PreflightError};
    match error {
        PrepareImageError::Image(linux_usb_writer::report::ImageSourceError::Io(_)) => {
            Reason::ImageUnreadable
        }
        PrepareImageError::Image(_) => Reason::ImageRefused,
        PrepareImageError::CompressedImage(rejection) => match rejection {
            CompressedImageRejection::QuickVerifyUnsupported(_) => Reason::QuickVerifyUnsupported,
            CompressedImageRejection::SourceChanged(_) => Reason::ImageChanged,
            CompressedImageRejection::Preflight(error) => match error {
                PreflightError::LogicalSizeLimitExceeded { .. }
                | PreflightError::LogicalSizeOverflow => Reason::CompressedImageTooLarge,
                PreflightError::DecoderMemoryLimitExceeded { .. }
                | PreflightError::CompressedInputBudgetExceeded { .. }
                | PreflightError::DecoderFailure(_) => Reason::CompressedImageTooDemanding,
                PreflightError::Io(_) => Reason::ImageUnreadable,
                _ => Reason::CompressedImageDamaged,
            },
        },
        PrepareImageError::TargetChanged(_) => Reason::TargetChanged,
    }
}

fn open_reason(error: &OpenDeviceError) -> Reason {
    use linux_usb_writer::report::AuthorizationDenial;
    match error {
        OpenDeviceError::NotAuthorized(AuthorizationDenial::Dismissed) => {
            Reason::AuthenticationCancelled
        }
        OpenDeviceError::NotAuthorized(_) => Reason::AccessDenied,
        OpenDeviceError::Rejected { .. } => Reason::DeviceBusyOrRefused,
        OpenDeviceError::Connection(_) | OpenDeviceError::Transport(_) => {
            Reason::SystemServiceUnavailable
        }
    }
}

impl Ending {
    // How each step is shown once the operation has ended.
    pub fn marks(self, verify_mode: VerifyMode) -> [Mark; 3] {
        let verify_waiting = if verify_mode == VerifyMode::None {
            Mark::Skipped
        } else {
            Mark::Waiting
        };
        match self {
            Ending::Verified(_) => [Mark::Done, Mark::Done, Mark::Done],
            Ending::WrittenWithoutVerify => [Mark::Done, Mark::Done, Mark::Skipped],
            Ending::CancelledBeforeWrite => [Mark::Cancelled, Mark::Waiting, verify_waiting],
            Ending::CancelledDuringWrite { .. } => [Mark::Done, Mark::Cancelled, verify_waiting],
            Ending::CancelledDuringVerify => [Mark::Done, Mark::Done, Mark::Cancelled],
            Ending::NotStarted {
                at: Step::Prepare, ..
            } => [Mark::Failed, Mark::Waiting, verify_waiting],
            Ending::NotStarted { .. } | Ending::WriteFailed { .. } => {
                [Mark::Done, Mark::Failed, verify_waiting]
            }
            Ending::VerifyFailed { .. } => [Mark::Done, Mark::Done, Mark::Failed],
            Ending::Lost => [Mark::Failed, Mark::Failed, Mark::Failed],
        }
    }

    // Stopped before anything was written: the window goes straight back
    // to the main view.
    pub fn returns_to_main(self) -> bool {
        self == Ending::CancelledBeforeWrite
    }
}

// The outcome for the technical details: the variant path, without the
// device snapshots some variants carry (they include the serial number,
// which the GUI never shows).
pub fn technical_detail(outcome: &OperationOutcome) -> String {
    match outcome {
        OperationOutcome::Failed(OperationError::Target(TargetNotReady::NotReady(state))) => {
            format!("Failed(Target(NotReady({})))", selection_state(state))
        }
        OperationOutcome::Failed(OperationError::Image(PrepareImageError::TargetChanged(
            state,
        ))) => format!("Failed(Image(TargetChanged({})))", selection_state(state)),
        OperationOutcome::Failed(OperationError::VerifyNotStarted(
            VerifyNotStarted::TargetCheck {
                error, image_size, ..
            },
        )) => format!(
            "Failed(VerifyNotStarted(TargetCheck {{ error: {error:?}, image_size: {image_size} }}))"
        ),
        other => format!("{other:?}"),
    }
}

fn selection_state(state: &SelectionState) -> String {
    match state {
        SelectionState::NoSelection => "NoSelection".to_string(),
        SelectionState::Selected { .. } => "Selected".to_string(),
        SelectionState::Invalidated { reason, .. } => format!("Invalidated({reason:?})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_usb_writer::report::{
        Cancelled, CompressedImageRejection, Failed, PreflightError, PreflightProgress,
        VerifyCancelled, VerifyFailed, VerifyProgress, VerifySucceeded, WriteJobFailureCause,
        WriteProgress, WriteStage,
    };
    use linux_usb_writer::{CancelReason, RiskLevel, SafetyAssessment};
    use std::io;

    fn display() -> DeviceDisplay {
        DeviceDisplay {
            device: "/dev/sdz".to_string(),
            vendor: "Vendor".to_string(),
            model: "Stick".to_string(),
            serial: "SERIAL".to_string(),
            size: 8_000_000_000,
            connection_bus: "usb".to_string(),
            removable: true,
            read_only: false,
            media_available: true,
            mount_points: Vec::new(),
        }
    }

    fn selected() -> WorkerEvent {
        WorkerEvent::TargetSelected {
            target: display(),
            block_path: "/org/freedesktop/UDisks2/block_devices/sdz".to_string(),
            diskseq: Some(42),
            assessment: SafetyAssessment {
                risk_level: RiskLevel::Normal,
                writable: true,
                reasons: Vec::new(),
            },
        }
    }

    fn written(done: u64) -> WorkerEvent {
        WorkerEvent::WriteProgress(WriteProgress {
            bytes_written: done,
            total_bytes: 1000,
        })
    }

    // Drives a tracker through a whole raw-image operation.
    fn through_write(mode: VerifyMode) -> Tracker {
        let mut tracker = Tracker::new(mode);
        for event in [selected(), WorkerEvent::ImageSelected { image_size: 1000 }] {
            tracker.apply(&event);
        }
        tracker.confirmation_requested();
        tracker.answered(true);
        tracker.apply(&WorkerEvent::WriteStarted);
        tracker.apply(&written(640));
        tracker
    }

    // ---- phase mapping ----

    #[test]
    fn preparing_and_confirmation_are_the_prepare_step() {
        let mut tracker = Tracker::new(VerifyMode::Quick);
        assert_eq!(tracker.step(), Step::Prepare);
        tracker.apply(&selected());
        assert_eq!(tracker.activity, Activity::PreparingImage);
        tracker.apply(&WorkerEvent::CompressedImageDetected {
            format: CompressionFormat::Xz,
        });
        tracker.apply(&WorkerEvent::PreflightProgress(PreflightProgress {
            compressed_consumed: 50,
            compressed_total: 200,
            logical_produced: 400,
        }));
        assert_eq!(tracker.activity, Activity::Preflight);
        // Preflight progress is the compressed file's, as reported.
        assert_eq!(tracker.transfer().map(Transfer::percent), Some(25));
        tracker.apply(&WorkerEvent::ImageSelected { image_size: 1000 });
        tracker.confirmation_requested();
        assert_eq!(tracker.activity, Activity::AwaitingConfirmation);
        assert_eq!(tracker.step(), Step::Prepare);
        assert_eq!(
            tracker.marks(),
            [Mark::Active, Mark::Waiting, Mark::Waiting]
        );
    }

    #[test]
    fn writing_and_sync_are_the_write_step() {
        let mut tracker = through_write(VerifyMode::Quick);
        assert_eq!(tracker.activity, Activity::Writing);
        assert_eq!(tracker.step(), Step::Write);
        assert_eq!(tracker.marks(), [Mark::Done, Mark::Active, Mark::Waiting]);
        assert_eq!(
            tracker.transfer(),
            Some(Transfer {
                done: 640,
                total: 1000
            })
        );
        tracker.apply(&WorkerEvent::WriteSucceeded {
            bytes_written: 1000,
            image_size: 1000,
        });
        tracker.apply(&WorkerEvent::SyncStarted);
        assert_eq!(tracker.activity, Activity::Syncing);
        assert_eq!(tracker.step(), Step::Write);
        assert_eq!(tracker.transfer().map(Transfer::percent), Some(100));
    }

    #[test]
    fn opening_the_device_may_wait_for_authentication() {
        let mut tracker = through_write(VerifyMode::Full);
        tracker.apply(&WorkerEvent::OpeningDevice {
            purpose: OpenPurpose::Write,
            block_path: String::new(),
        });
        assert_eq!(
            tracker.activity,
            Activity::OpeningDevice(OpenPurpose::Write)
        );
        assert_eq!(tracker.step(), Step::Write);
        tracker.apply(&WorkerEvent::OpeningDevice {
            purpose: OpenPurpose::Verify,
            block_path: String::new(),
        });
        assert_eq!(tracker.step(), Step::Verify);
    }

    #[test]
    fn verify_quick_and_full_are_the_verify_step() {
        for mode in [VerifyMode::Quick, VerifyMode::Full] {
            let mut tracker = through_write(mode);
            tracker.apply(&WorkerEvent::SyncSucceeded {
                bytes_written: 1000,
            });
            tracker.apply(&WorkerEvent::VerifyPending { mode });
            assert_eq!(tracker.activity, Activity::PreparingVerify);
            tracker.apply(&WorkerEvent::VerifyStarted);
            tracker.apply(&WorkerEvent::VerifyProgress(VerifyProgress {
                verified_bytes: 30,
                total_bytes: 120,
                mode,
            }));
            assert_eq!(tracker.activity, Activity::Verifying);
            assert_eq!(tracker.verify_mode, mode);
            assert_eq!(tracker.marks(), [Mark::Done, Mark::Done, Mark::Active]);
            assert_eq!(tracker.transfer().map(Transfer::percent), Some(25));
        }
    }

    #[test]
    fn verify_none_is_never_shown_as_done() {
        let tracker = Tracker::new(VerifyMode::None);
        assert_eq!(tracker.marks()[2], Mark::Skipped);
        let tracker = through_write(VerifyMode::None);
        assert_eq!(tracker.marks(), [Mark::Done, Mark::Active, Mark::Skipped]);
        assert_eq!(
            Ending::WrittenWithoutVerify.marks(VerifyMode::None),
            [Mark::Done, Mark::Done, Mark::Skipped]
        );
    }

    #[test]
    fn finished_hides_cancel() {
        let mut tracker = through_write(VerifyMode::Quick);
        tracker.finished();
        assert_eq!(tracker.activity, Activity::Finished);
        assert_eq!(tracker.cancel_action(), CancelAction::Unavailable);
    }

    // ---- cancel policy ----

    #[test]
    fn cancel_is_immediate_while_preparing() {
        let mut tracker = Tracker::new(VerifyMode::Quick);
        assert_eq!(tracker.cancel_action(), CancelAction::RequestNow);
        tracker.apply(&WorkerEvent::CompressedImageDetected {
            format: CompressionFormat::Gzip,
        });
        assert_eq!(tracker.cancel_action(), CancelAction::RequestNow);
    }

    #[test]
    fn cancel_while_confirming_declines_the_confirmation() {
        let mut tracker = Tracker::new(VerifyMode::Quick);
        tracker.confirmation_requested();
        assert_eq!(tracker.cancel_action(), CancelAction::Decline);
        tracker.answered(false);
        assert!(tracker.cancel_requested);
        assert_eq!(tracker.cancel_action(), CancelAction::Unavailable);
        // Declining never moves on to the write step.
        assert_eq!(tracker.step(), Step::Prepare);
    }

    #[test]
    fn cancel_during_write_and_sync_asks_first() {
        let mut tracker = Tracker::new(VerifyMode::Quick);
        tracker.confirmation_requested();
        tracker.answered(true);
        // Approved: the write may start at any moment.
        assert_eq!(tracker.cancel_action(), CancelAction::AskFirst);
        tracker.apply(&WorkerEvent::WriteStarted);
        assert_eq!(tracker.cancel_action(), CancelAction::AskFirst);
        tracker.apply(&WorkerEvent::SyncStarted);
        assert_eq!(tracker.cancel_action(), CancelAction::AskFirst);
    }

    #[test]
    fn cancel_during_verify_is_immediate() {
        let mut tracker = through_write(VerifyMode::Full);
        tracker.apply(&WorkerEvent::VerifyPending {
            mode: VerifyMode::Full,
        });
        assert_eq!(tracker.cancel_action(), CancelAction::RequestNow);
        tracker.apply(&WorkerEvent::VerifyStarted);
        assert_eq!(tracker.cancel_action(), CancelAction::RequestNow);
    }

    #[test]
    fn cancel_is_disabled_once_requested() {
        let mut tracker = through_write(VerifyMode::Quick);
        tracker.cancel_requested = true;
        assert_eq!(tracker.cancel_action(), CancelAction::Unavailable);
    }

    // ---- confirmation ----

    #[test]
    fn only_the_write_response_approves() {
        assert!(matches!(
            decision_for(CONFIRM_RESPONSE_WRITE),
            ConfirmationDecision::Approved
        ));
        for response in [CONFIRM_RESPONSE_CANCEL, "close", "", "Write", "approve"] {
            assert!(
                matches!(decision_for(response), ConfirmationDecision::Cancelled),
                "{response:?}"
            );
        }
    }

    // ---- result mapping ----

    fn completed(mode: VerifyMode, skipped: bool) -> OperationOutcome {
        OperationOutcome::Completed {
            verify: VerifySucceeded {
                mode,
                verified_bytes: if skipped { 0 } else { 1000 },
                skipped,
            },
            image_size: 1000,
        }
    }

    #[test]
    fn success_and_verify_none() {
        assert_eq!(
            ending(&completed(VerifyMode::Quick, false)),
            Ending::Verified(VerifyMode::Quick)
        );
        assert_eq!(
            ending(&completed(VerifyMode::Full, false)),
            Ending::Verified(VerifyMode::Full)
        );
        assert_eq!(
            ending(&completed(VerifyMode::None, true)),
            Ending::WrittenWithoutVerify
        );
    }

    #[test]
    fn cancellations_before_the_write_return_to_the_main_view() {
        for at in [
            CancelledAt::Preflight,
            CancelledAt::BeforeConfirmation,
            CancelledAt::Confirmation,
        ] {
            let ending = ending(&OperationOutcome::Cancelled(at));
            assert_eq!(ending, Ending::CancelledBeforeWrite);
            assert!(ending.returns_to_main());
        }
    }

    #[test]
    fn write_cancel() {
        let outcome = OperationOutcome::Cancelled(CancelledAt::Write {
            cancelled: Cancelled {
                image_size: 1000,
                bytes_written: 300,
                reason: CancelReason::UserRequested,
                target_may_be_modified: true,
                retry_requires_fresh_gate: true,
            },
            image_size: 1000,
        });
        let ending = ending(&outcome);
        assert_eq!(
            ending,
            Ending::CancelledDuringWrite {
                target_modified: true
            }
        );
        assert!(!ending.returns_to_main());
        assert_eq!(
            ending.marks(VerifyMode::Quick),
            [Mark::Done, Mark::Cancelled, Mark::Waiting]
        );
    }

    #[test]
    fn verify_cancel_means_written_not_verified() {
        for at in [
            CancelledAt::AfterSync,
            CancelledAt::BeforeVerify,
            CancelledAt::Verify {
                cancelled: VerifyCancelled {
                    mode: VerifyMode::Full,
                    verified_bytes: 10,
                },
                image_size: 1000,
            },
        ] {
            let ending = ending(&OperationOutcome::Cancelled(at));
            assert_eq!(ending, Ending::CancelledDuringVerify);
            assert_eq!(
                ending.marks(VerifyMode::Full),
                [Mark::Done, Mark::Done, Mark::Cancelled]
            );
        }
    }

    #[test]
    fn write_failure() {
        let outcome = OperationOutcome::Failed(OperationError::Sync {
            failed: Failed {
                image_size: 1000,
                bytes_written: 1000,
                stage: WriteStage::Syncing,
                cause: WriteJobFailureCause::Sync(io::Error::other("EIO")),
                target_may_be_modified: true,
                retry_requires_fresh_gate: true,
            },
            cancel_requested: false,
            image_size: 1000,
        });
        assert_eq!(
            ending(&outcome),
            Ending::WriteFailed {
                reason: Reason::SyncError,
                target_modified: true
            }
        );
    }

    #[test]
    fn verify_failure() {
        let outcome = OperationOutcome::Failed(OperationError::Verify {
            failed: VerifyFailed {
                mode: VerifyMode::Full,
                verified_bytes: 512,
                reason: VerifyFailureReason::Mismatch {
                    offset: 512,
                    expected: 1,
                    actual: 2,
                },
            },
            image_size: 1000,
        });
        let ending = ending(&outcome);
        assert_eq!(
            ending,
            Ending::VerifyFailed {
                reason: Reason::VerifyMismatch
            }
        );
        assert_eq!(
            ending.marks(VerifyMode::Full),
            [Mark::Done, Mark::Done, Mark::Failed]
        );
    }

    #[test]
    fn refusals_before_any_write_are_not_started() {
        let outcome = OperationOutcome::Failed(OperationError::Target(TargetNotReady::Select(
            SelectTargetError::NotFound,
        )));
        assert_eq!(
            ending(&outcome),
            Ending::NotStarted {
                at: Step::Prepare,
                reason: Reason::TargetNotFound
            }
        );
        let outcome = OperationOutcome::Failed(OperationError::Image(
            PrepareImageError::CompressedImage(CompressedImageRejection::Preflight(
                PreflightError::LogicalSizeLimitExceeded { limit: 10 },
            )),
        ));
        assert_eq!(
            ending(&outcome),
            Ending::NotStarted {
                at: Step::Prepare,
                reason: Reason::CompressedImageTooLarge
            }
        );
        let outcome = OperationOutcome::Failed(OperationError::WriteDeviceRejected {
            error: linux_usb_writer::report::WriteGateError::OpenDeviceFailed,
            open_device: Some(OpenDeviceError::NotAuthorized(
                linux_usb_writer::report::AuthorizationDenial::Dismissed,
            )),
        });
        assert_eq!(
            ending(&outcome),
            Ending::NotStarted {
                at: Step::Write,
                reason: Reason::AuthenticationCancelled
            }
        );
    }

    // ---- formatting ----

    #[test]
    fn progress_rounds_down() {
        let t = |done, total| Transfer { done, total };
        assert_eq!(t(0, 1000).percent(), 0);
        assert_eq!(t(999, 1000).percent(), 99);
        assert_eq!(t(1000, 1000).percent(), 100);
        assert_eq!(t(5, 0).percent(), 0);
        assert_eq!(t(2000, 1000).percent(), 100);
        assert!((t(640, 1000).fraction() - 0.64).abs() < 1e-9);
        assert_eq!(t(u64::MAX, u64::MAX).percent(), 100);
    }
}

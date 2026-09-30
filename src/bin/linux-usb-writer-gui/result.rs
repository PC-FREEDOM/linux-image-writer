// The result view of a finished operation, as a plain model: which case it
// is (from the operation's own `Ending`), what each step says, and which
// actions are offered -- plus the Safe Removal presentation state. No GTK
// here, so all of it is unit tested; the wording is `text`'s.
//
// Nothing here decides anything about the device. How the operation ended
// is the library's `OperationOutcome` (read through `operation::ending`);
// whether Safe Removal is offered is only whether the finished worker handed
// out a `RemovalTarget` (`WriteWorker::removal_target`). The GUI never works
// that out again from the outcome, the device or anything else.

use linux_usb_writer::{SafeRemovalOutcome, VerifyMode};

use crate::model::{MainReturn, TargetReturn};
use crate::operation::{Ending, Mark, Reason, STEPS, Step};

// ---- Safe Removal, as the result view shows it ----

// Where Safe Removal stands for the finished operation. `T` is the
// library's opaque `RemovalTarget` (a parameter only so the tests, which
// cannot make one, can use a stand-in; the window uses `RemovalTarget`).
//
// One way only: `Ready` -> `Removing` -> `Finished`, and back to
// `Removing` only from a `Finished` that kept its target for another try.
// The target has exactly one owner at any time: this state, or the request
// running in the background (`begin` hands it over, `end` gives it back).
// It is not `Clone`: no second copy exists to start a second request with.
#[derive(Debug, PartialEq, Eq)]
pub enum RemovalPresentation<T> {
    // The finished worker handed out no target: Safe Removal is not offered.
    Unavailable,
    // It did: Safe Removal can be requested for this target.
    Ready(T),
    // The request is running (off the GTK thread), holding the target.
    Removing,
    // The request ended. `retry` keeps the target only when trying again is
    // offered (`RemovalStatus::retryable`); otherwise it was dropped.
    Finished {
        status: RemovalStatus,
        retry: Option<T>,
    },
}

// How a Safe Removal request ended, as the result view says it. Read only
// from the library's `SafeRemovalOutcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalStatus {
    // Powered off: the only status that may say the drive can be removed.
    // `unmounted_filesystems`: the outcome lists filesystems this removal
    // itself unmounted.
    Removed { unmounted_filesystems: bool },
    DeviceGone,
    DeviceChanged,
    Unsupported,
    // In use; trying again later may work.
    Busy,
    NotAuthorized,
    // Could not be completed: the device's state could not be established,
    // a call failed, or the request itself ended without an outcome.
    NotCompleted,
}

impl RemovalStatus {
    pub fn from_outcome(outcome: &SafeRemovalOutcome) -> Self {
        match outcome {
            SafeRemovalOutcome::Removed { unmounted, .. } => RemovalStatus::Removed {
                unmounted_filesystems: !unmounted.is_empty(),
            },
            SafeRemovalOutcome::DeviceGone => RemovalStatus::DeviceGone,
            SafeRemovalOutcome::DeviceChanged(_) => RemovalStatus::DeviceChanged,
            SafeRemovalOutcome::Unsupported(_) => RemovalStatus::Unsupported,
            SafeRemovalOutcome::Busy { .. } => RemovalStatus::Busy,
            SafeRemovalOutcome::NotAuthorized { .. } => RemovalStatus::NotAuthorized,
            SafeRemovalOutcome::Unavailable(_) | SafeRemovalOutcome::Failed { .. } => {
                RemovalStatus::NotCompleted
            }
        }
    }

    // Only "in use" is worth trying again (with the same target, from the
    // start); every other status is final for this target.
    pub fn retryable(self) -> bool {
        self == RemovalStatus::Busy
    }
}

impl<T> RemovalPresentation<T> {
    // The one entry point: what `WriteWorker::removal_target` returned after
    // `Finished`, as it is.
    pub fn from_target(target: Option<T>) -> Self {
        match target {
            Some(target) => RemovalPresentation::Ready(target),
            None => RemovalPresentation::Unavailable,
        }
    }

    pub fn is_removing(&self) -> bool {
        matches!(self, RemovalPresentation::Removing)
    }

    // Starts a request: from `Ready`, or from a `Finished` that kept its
    // target, to `Removing`, handing the target over to the request.
    // Anything else (already removing, nothing to remove) starts nothing.
    pub fn begin(&mut self) -> Option<T> {
        match std::mem::replace(self, RemovalPresentation::Removing) {
            RemovalPresentation::Ready(target)
            | RemovalPresentation::Finished {
                retry: Some(target),
                ..
            } => Some(target),
            other => {
                *self = other;
                None
            }
        }
    }

    // The request ended with `status`, giving the target back: kept only
    // for another try.
    pub fn end(&mut self, target: T, status: RemovalStatus) {
        if self.is_removing() {
            *self = RemovalPresentation::Finished {
                status,
                retry: status.retryable().then_some(target),
            };
        }
    }

    // The request ended without an outcome (it panicked): the target went
    // with it.
    pub fn end_without_outcome(&mut self) {
        if self.is_removing() {
            *self = RemovalPresentation::Finished {
                status: RemovalStatus::NotCompleted,
                retry: None,
            };
        }
    }
}

// What the result view shows about Safe Removal, apart from the
// operation's own result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalNotice {
    Removing,
    Finished(RemovalStatus),
}

// ---- The result ----

// The four kinds of result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultKind {
    // Written, synced and verified.
    Success,
    // Written and synced; not verified (not requested, or not completed).
    WrittenNotVerified,
    Cancelled,
    Failed,
}

// What happened, finely enough to word it. Every case is read off the
// operation's `Ending`, never guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultCase {
    // Written and verified (Quick or Full).
    Verified(VerifyMode),
    // Written; Verify was not requested.
    WrittenWithoutVerify,
    // Written; Verify was stopped or not started because of a cancellation.
    VerifyCancelled,
    // Written; the data read back did not match the image.
    VerifyMismatch,
    // Written; Verify could not start or could not be completed.
    VerifyNotCompleted(Reason),
    // The write (or its sync) failed.
    WriteFailed {
        reason: Reason,
        target_modified: bool,
    },
    // Stopped during the write (or its sync).
    WriteCancelled {
        target_modified: bool,
    },
    // Stopped before anything was opened on the target.
    CancelledBeforeWrite,
    // Refused before anything was written.
    NotStarted(Reason),
    // The worker ended without an outcome.
    Lost,
}

// What the user can do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultAction {
    // Only while the presentation is `Ready`.
    SafeRemoval,
    // Only after "in use": Safe Removal again, from the start.
    RetryRemoval,
    WriteAnother,
    WriteAgain,
    Retry,
    BackToMain,
    Done,
}

impl ResultAction {
    // What the main view keeps when this action leaves the result (`None`:
    // it does not leave it). Whatever is kept is only shown as chosen: the
    // next write re-checks everything itself, as every write does.
    pub fn main_return(self) -> Option<MainReturn> {
        let keep = |target| {
            Some(MainReturn {
                keep_image: true,
                target,
            })
        };
        match self {
            // Handled on the result view itself.
            ResultAction::SafeRemoval | ResultAction::RetryRemoval => None,
            // The same image to another drive: the drive just written is
            // let go.
            ResultAction::WriteAnother => keep(TargetReturn::ChooseAnother),
            // The same image and drive again (the drive only while the
            // device list still shows the same device and instance).
            ResultAction::WriteAgain | ResultAction::Retry | ResultAction::BackToMain => {
                keep(TargetReturn::Keep)
            }
            // A new start: no image, no drive (the Verify preference stays).
            ResultAction::Done => Some(MainReturn {
                keep_image: false,
                target: TargetReturn::Reset,
            }),
        }
    }
}

// Where an action leaves the finished operation.
#[derive(Debug)]
pub enum Leaving<T> {
    // Still on the result, with its Safe Removal state.
    Stay(RemovalPresentation<T>),
    // Back to the main view: the Safe Removal state (and its target) has
    // been dropped here -- nothing of it reaches the next operation, which
    // starts `Unavailable` and gets a target only from its own worker.
    Main(MainReturn),
}

// The one way out of a result: `removal` is handed over, and kept only
// while the result stays. While Safe Removal runs, nothing leaves.
pub fn leave<T>(removal: RemovalPresentation<T>, action: ResultAction) -> Leaving<T> {
    if removal.is_removing() {
        return Leaving::Stay(removal);
    }
    match action.main_return() {
        Some(ret) => {
            drop(removal);
            Leaving::Main(ret)
        }
        // Safe Removal's own actions stay on the result (the window starts
        // the request).
        None => Leaving::Stay(removal),
    }
}

// One step's line on the result view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepLine {
    pub step: Step,
    pub mark: Mark,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultView {
    // The operation's own result: Safe Removal never changes it.
    pub case: ResultCase,
    pub kind: ResultKind,
    pub steps: [StepLine; 3],
    // Safe Removal, shown apart from the operation's result.
    pub removal: Option<RemovalNotice>,
    // In order; Safe Removal's own action first when offered.
    pub actions: Vec<ResultAction>,
    // `false` while Safe Removal runs: nothing can be pressed.
    pub actions_enabled: bool,
}

// The case for an ending, or `None` when the ending has no result view (a
// declined final confirmation goes straight back to the main view).
pub fn case(ending: Ending) -> Option<ResultCase> {
    Some(match ending {
        Ending::Verified(mode) => ResultCase::Verified(mode),
        Ending::WrittenWithoutVerify => ResultCase::WrittenWithoutVerify,
        Ending::CancelledDuringVerify => ResultCase::VerifyCancelled,
        Ending::VerifyFailed {
            reason: Reason::VerifyMismatch,
        } => ResultCase::VerifyMismatch,
        Ending::VerifyFailed { reason } => ResultCase::VerifyNotCompleted(reason),
        Ending::WriteFailed {
            reason,
            target_modified,
        } => ResultCase::WriteFailed {
            reason,
            target_modified,
        },
        Ending::CancelledDuringWrite { target_modified } => {
            ResultCase::WriteCancelled { target_modified }
        }
        Ending::CancelledBeforeWrite => ResultCase::CancelledBeforeWrite,
        Ending::NotStarted { reason, .. } => ResultCase::NotStarted(reason),
        Ending::Lost => ResultCase::Lost,
        Ending::ConfirmationDeclined => return None,
    })
}

impl ResultCase {
    pub fn kind(self) -> ResultKind {
        match self {
            ResultCase::Verified(_) => ResultKind::Success,
            ResultCase::WrittenWithoutVerify | ResultCase::VerifyCancelled => {
                ResultKind::WrittenNotVerified
            }
            ResultCase::WriteCancelled { .. } | ResultCase::CancelledBeforeWrite => {
                ResultKind::Cancelled
            }
            ResultCase::VerifyMismatch
            | ResultCase::VerifyNotCompleted(_)
            | ResultCase::WriteFailed { .. }
            | ResultCase::NotStarted(_)
            | ResultCase::Lost => ResultKind::Failed,
        }
    }

    // The next step besides Safe Removal and "Done": another USB drive after
    // a written image, the same write again after an unverified or failed
    // one, the main view when nothing was written.
    fn next(self) -> ResultAction {
        match self {
            ResultCase::Verified(_) | ResultCase::WrittenWithoutVerify => {
                ResultAction::WriteAnother
            }
            ResultCase::VerifyCancelled
            | ResultCase::VerifyMismatch
            | ResultCase::VerifyNotCompleted(_) => ResultAction::WriteAgain,
            ResultCase::WriteFailed { .. } | ResultCase::WriteCancelled { .. } => {
                ResultAction::Retry
            }
            ResultCase::CancelledBeforeWrite | ResultCase::NotStarted(_) | ResultCase::Lost => {
                ResultAction::BackToMain
            }
        }
    }
}

// The result view for an ending (`None`: no result view, see `case`).
// `verify_mode` is the mode the operation used; `removal` is the finished
// worker's own answer, and the only thing Safe Removal's action depends on.
pub fn view<T>(
    ending: Ending,
    verify_mode: VerifyMode,
    removal: &RemovalPresentation<T>,
) -> Option<ResultView> {
    let case = case(ending)?;
    let marks = ending.marks(verify_mode);
    let steps = [0, 1, 2].map(|index| StepLine {
        step: STEPS[index],
        mark: marks[index],
    });
    let (notice, first, rest) = match removal {
        RemovalPresentation::Unavailable => (None, None, case.next()),
        RemovalPresentation::Ready(_) => (None, Some(ResultAction::SafeRemoval), case.next()),
        RemovalPresentation::Removing => (Some(RemovalNotice::Removing), None, case.next()),
        RemovalPresentation::Finished { status, retry } => (
            Some(RemovalNotice::Finished(*status)),
            retry.as_ref().map(|_| ResultAction::RetryRemoval),
            case.next(),
        ),
    };
    let mut actions: Vec<ResultAction> = first.into_iter().collect();
    // After the drive was powered off, only "Done": every other way back
    // assumes the drive is still there.
    if !matches!(
        notice,
        Some(RemovalNotice::Finished(RemovalStatus::Removed { .. }))
    ) {
        actions.push(rest);
    }
    actions.push(ResultAction::Done);
    Some(ResultView {
        case,
        kind: case.kind(),
        steps,
        removal: notice,
        actions,
        actions_enabled: !removal.is_removing(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Stands in for the opaque `RemovalTarget` (which only a finished
    // worker can produce).
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Target;

    fn unavailable() -> RemovalPresentation<Target> {
        RemovalPresentation::Unavailable
    }

    fn every_ending() -> Vec<Ending> {
        vec![
            Ending::Verified(VerifyMode::Quick),
            Ending::Verified(VerifyMode::Full),
            Ending::WrittenWithoutVerify,
            Ending::CancelledDuringVerify,
            Ending::VerifyFailed {
                reason: Reason::VerifyMismatch,
            },
            Ending::VerifyFailed {
                reason: Reason::VerifyReadError,
            },
            Ending::WriteFailed {
                reason: Reason::WriteError,
                target_modified: true,
            },
            Ending::CancelledDuringWrite {
                target_modified: true,
            },
            Ending::CancelledBeforeWrite,
            Ending::NotStarted {
                at: Step::Write,
                reason: Reason::AccessDenied,
            },
            Ending::Lost,
            Ending::ConfirmationDeclined,
        ]
    }

    fn shown(ending: Ending, mode: VerifyMode) -> ResultView {
        view(ending, mode, &unavailable()).expect("a result view")
    }

    #[test]
    fn quick_and_full_success() {
        for mode in [VerifyMode::Quick, VerifyMode::Full] {
            let view = shown(Ending::Verified(mode), mode);
            assert_eq!(view.case, ResultCase::Verified(mode));
            assert_eq!(view.kind, ResultKind::Success);
            assert_eq!(view.steps.map(|line| line.mark), [Mark::Done; 3]);
            assert_eq!(
                view.actions,
                [ResultAction::WriteAnother, ResultAction::Done]
            );
        }
    }

    #[test]
    fn verify_none_is_written_not_verified() {
        let view = shown(Ending::WrittenWithoutVerify, VerifyMode::None);
        assert_eq!(view.case, ResultCase::WrittenWithoutVerify);
        assert_eq!(view.kind, ResultKind::WrittenNotVerified);
        assert_eq!(
            view.steps.map(|line| line.mark),
            [Mark::Done, Mark::Done, Mark::Skipped]
        );
        assert_eq!(
            view.actions,
            [ResultAction::WriteAnother, ResultAction::Done]
        );
    }

    #[test]
    fn a_cancelled_verify_is_written_not_verified_not_an_error() {
        let view = shown(Ending::CancelledDuringVerify, VerifyMode::Full);
        assert_eq!(view.case, ResultCase::VerifyCancelled);
        assert_eq!(view.kind, ResultKind::WrittenNotVerified);
        assert_eq!(view.steps[1].mark, Mark::Done);
        assert_eq!(view.actions, [ResultAction::WriteAgain, ResultAction::Done]);
    }

    #[test]
    fn a_mismatch_and_a_verify_error_are_told_apart() {
        let mismatch = shown(
            Ending::VerifyFailed {
                reason: Reason::VerifyMismatch,
            },
            VerifyMode::Full,
        );
        assert_eq!(mismatch.case, ResultCase::VerifyMismatch);
        for reason in [
            Reason::VerifyReadError,
            Reason::VerifyLengthMismatch,
            Reason::VerifyOpenFailed,
            Reason::VerifyTargetChanged,
        ] {
            let error = shown(Ending::VerifyFailed { reason }, VerifyMode::Quick);
            assert_eq!(error.case, ResultCase::VerifyNotCompleted(reason));
            assert_ne!(error.case, mismatch.case);
        }
        // Both say the write itself was done.
        assert_eq!(mismatch.steps[1].mark, Mark::Done);
        assert_eq!(
            mismatch.actions,
            [ResultAction::WriteAgain, ResultAction::Done]
        );
    }

    #[test]
    fn write_failure_and_write_cancellation() {
        let failed = shown(
            Ending::WriteFailed {
                reason: Reason::WriteError,
                target_modified: true,
            },
            VerifyMode::Quick,
        );
        assert_eq!(failed.kind, ResultKind::Failed);
        assert_eq!(failed.steps[1].mark, Mark::Failed);
        assert_eq!(failed.actions, [ResultAction::Retry, ResultAction::Done]);

        let cancelled = shown(
            Ending::CancelledDuringWrite {
                target_modified: true,
            },
            VerifyMode::Quick,
        );
        // A cancellation, not an error.
        assert_eq!(cancelled.kind, ResultKind::Cancelled);
        assert_eq!(cancelled.steps[1].mark, Mark::Cancelled);
        assert_eq!(cancelled.actions, [ResultAction::Retry, ResultAction::Done]);
    }

    #[test]
    fn a_cancellation_before_the_write_has_a_result_back_to_the_main_view() {
        let view = shown(Ending::CancelledBeforeWrite, VerifyMode::Quick);
        assert_eq!(view.case, ResultCase::CancelledBeforeWrite);
        assert_eq!(view.kind, ResultKind::Cancelled);
        assert_eq!(view.actions, [ResultAction::BackToMain, ResultAction::Done]);
    }

    #[test]
    fn a_declined_confirmation_has_no_result() {
        assert_eq!(case(Ending::ConfirmationDeclined), None);
        assert_eq!(
            view(
                Ending::ConfirmationDeclined,
                VerifyMode::Quick,
                &RemovalPresentation::Ready(Target)
            ),
            None
        );
    }

    #[test]
    fn the_presentation_is_exactly_the_workers_answer() {
        assert_eq!(
            RemovalPresentation::<Target>::from_target(None),
            RemovalPresentation::Unavailable
        );
        assert_eq!(
            RemovalPresentation::from_target(Some(Target)),
            RemovalPresentation::Ready(Target)
        );
    }

    // Safe Removal is offered exactly when the worker handed out a target,
    // whatever the ending: the GUI does not work it out again from the
    // outcome (the library already did, before handing it out).
    #[test]
    fn safe_removal_is_offered_only_for_a_target_and_for_every_ending() {
        for ending in every_ending() {
            let Some(without) = view(ending, VerifyMode::Quick, &unavailable()) else {
                continue;
            };
            assert!(
                !without.actions.contains(&ResultAction::SafeRemoval),
                "{ending:?}"
            );
            let with = view(
                ending,
                VerifyMode::Quick,
                &RemovalPresentation::Ready(Target),
            )
            .unwrap();
            assert_eq!(with.actions[0], ResultAction::SafeRemoval, "{ending:?}");
            assert_eq!(with.actions[1..], without.actions[..], "{ending:?}");
        }
    }

    fn ret(keep_image: bool, target: TargetReturn) -> Option<MainReturn> {
        Some(MainReturn { keep_image, target })
    }

    #[test]
    fn each_action_returns_to_the_main_view_as_specified() {
        // Write to another USB: image kept, drive let go.
        assert_eq!(
            ResultAction::WriteAnother.main_return(),
            ret(true, TargetReturn::ChooseAnother)
        );
        // Write again, retry, back to the main view: both kept.
        for action in [
            ResultAction::WriteAgain,
            ResultAction::Retry,
            ResultAction::BackToMain,
        ] {
            assert_eq!(
                action.main_return(),
                ret(true, TargetReturn::Keep),
                "{action:?}"
            );
        }
        // Done: a new start.
        assert_eq!(
            ResultAction::Done.main_return(),
            ret(false, TargetReturn::Reset)
        );
        // Safe Removal does not leave the result.
        assert_eq!(ResultAction::SafeRemoval.main_return(), None);
    }

    // A stand-in target that counts how many copies are alive.
    fn counted() -> (std::rc::Rc<()>, RemovalPresentation<std::rc::Rc<()>>) {
        let alive = std::rc::Rc::new(());
        let presentation = RemovalPresentation::from_target(Some(alive.clone()));
        (alive, presentation)
    }

    // Leaving for the main view drops the finished operation's target:
    // "Done" and "write to another USB" (and every other way back) carry
    // nothing of it into the next operation.
    #[test]
    fn leaving_a_result_drops_the_removal_target() {
        for action in [
            ResultAction::Done,
            ResultAction::WriteAnother,
            ResultAction::WriteAgain,
            ResultAction::Retry,
            ResultAction::BackToMain,
        ] {
            let (alive, presentation) = counted();
            assert_eq!(std::rc::Rc::strong_count(&alive), 2);
            let leaving = leave(presentation, action);
            assert!(matches!(leaving, Leaving::Main(_)), "{action:?}");
            assert_eq!(std::rc::Rc::strong_count(&alive), 1, "{action:?}");
        }
    }

    // ---- Safe Removal (3b) ----

    use linux_usb_writer::DeviceDisplay;
    use linux_usb_writer::report::{
        AuthorizationDenial, RemovalActionError, RemovalStage, RemovalUnavailable,
        RemovalUnsupported, TargetChange,
    };

    fn display() -> Box<DeviceDisplay> {
        Box::new(DeviceDisplay {
            device: "/dev/sdz".to_string(),
            vendor: "General".to_string(),
            model: "UDisk".to_string(),
            serial: "SERIAL-123".to_string(),
            size: 8_000_000_000,
            connection_bus: "usb".to_string(),
            removable: true,
            read_only: false,
            media_available: true,
            mount_points: Vec::new(),
        })
    }

    // Every outcome the library can return, with the status it reads as.
    fn every_outcome() -> Vec<(SafeRemovalOutcome, RemovalStatus)> {
        let mounted = || vec!["/run/media/user/STICK".to_string()];
        vec![
            (
                SafeRemovalOutcome::Removed {
                    device: display(),
                    unmounted: Vec::new(),
                },
                RemovalStatus::Removed {
                    unmounted_filesystems: false,
                },
            ),
            (
                SafeRemovalOutcome::Removed {
                    device: display(),
                    unmounted: mounted(),
                },
                RemovalStatus::Removed {
                    unmounted_filesystems: true,
                },
            ),
            (SafeRemovalOutcome::DeviceGone, RemovalStatus::DeviceGone),
            (
                SafeRemovalOutcome::DeviceChanged(TargetChange::DifferentTarget),
                RemovalStatus::DeviceChanged,
            ),
            (
                SafeRemovalOutcome::Unsupported(RemovalUnsupported::SharedPhysicalDevice),
                RemovalStatus::Unsupported,
            ),
            (
                SafeRemovalOutcome::Busy {
                    stage: RemovalStage::Unmount,
                    unmounted: mounted(),
                },
                RemovalStatus::Busy,
            ),
            (
                SafeRemovalOutcome::NotAuthorized {
                    stage: RemovalStage::PowerOff,
                    denial: AuthorizationDenial::CanObtain,
                    unmounted: Vec::new(),
                },
                RemovalStatus::NotAuthorized,
            ),
            (
                SafeRemovalOutcome::Unavailable(RemovalUnavailable::DeviceInformation(
                    "UDisks2 object /org/freedesktop/UDisks2/drives/General_UDisk_SERIAL_123 is missing"
                        .to_string(),
                )),
                RemovalStatus::NotCompleted,
            ),
            (
                SafeRemovalOutcome::Failed {
                    stage: RemovalStage::PowerOff,
                    error: RemovalActionError::Rejected {
                        name: "org.freedesktop.UDisks2.Error.Failed".to_string(),
                        message: Some("Error powering off drive".to_string()),
                    },
                    unmounted: mounted(),
                },
                RemovalStatus::NotCompleted,
            ),
        ]
    }

    #[test]
    fn every_outcome_has_its_status() {
        for (outcome, status) in every_outcome() {
            assert_eq!(RemovalStatus::from_outcome(&outcome), status, "{outcome:?}");
            // Only "in use" is tried again.
            assert_eq!(status.retryable(), status == RemovalStatus::Busy);
        }
    }

    fn quick() -> Ending {
        Ending::Verified(VerifyMode::Quick)
    }

    fn shown_with(removal: &RemovalPresentation<std::rc::Rc<()>>) -> ResultView {
        view(quick(), VerifyMode::Quick, removal).unwrap()
    }

    // Ready -> Removing, once: the target goes to the request, and a second
    // start while it runs gets nothing (no second request can start).
    #[test]
    fn starting_moves_the_target_to_the_request_once() {
        let (alive, mut removal) = counted();
        let target = removal.begin().expect("a target to remove");
        assert!(std::rc::Rc::ptr_eq(&target, &alive));
        assert!(removal.is_removing());
        assert_eq!(removal.begin(), None);
        assert_eq!(removal.begin(), None);
        assert!(removal.is_removing());
        // Only the request holds it now.
        assert_eq!(std::rc::Rc::strong_count(&alive), 2);

        // Nothing to start without a target.
        let mut none = RemovalPresentation::<std::rc::Rc<()>>::Unavailable;
        assert_eq!(none.begin(), None);
        assert_eq!(none, RemovalPresentation::Unavailable);
    }

    // While it runs: its button is gone, every other action is shown but
    // cannot be pressed, and no action leaves the result.
    #[test]
    fn while_removing_nothing_can_be_pressed_or_left() {
        let (_alive, mut removal) = counted();
        let _target = removal.begin().unwrap();
        let view = shown_with(&removal);
        assert_eq!(view.removal, Some(RemovalNotice::Removing));
        assert!(!view.actions_enabled);
        assert!(!view.actions.contains(&ResultAction::SafeRemoval));
        for action in [
            ResultAction::SafeRemoval,
            ResultAction::RetryRemoval,
            ResultAction::WriteAnother,
            ResultAction::WriteAgain,
            ResultAction::Retry,
            ResultAction::BackToMain,
            ResultAction::Done,
        ] {
            let stay = leave(RemovalPresentation::<std::rc::Rc<()>>::Removing, action);
            assert!(
                matches!(stay, Leaving::Stay(RemovalPresentation::Removing)),
                "{action:?}"
            );
        }
    }

    // Ready shows the button; Unavailable shows nothing about removal.
    #[test]
    fn the_button_is_there_only_for_a_target() {
        let (_alive, ready) = counted();
        let view = shown_with(&ready);
        assert_eq!(view.actions[0], ResultAction::SafeRemoval);
        assert!(view.actions_enabled);
        assert_eq!(view.removal, None);

        let none = shown_with(&RemovalPresentation::Unavailable);
        assert!(!none.actions.contains(&ResultAction::SafeRemoval));
        assert!(!none.actions.contains(&ResultAction::RetryRemoval));
        assert_eq!(none.removal, None);
    }

    fn finished_with(
        status: RemovalStatus,
    ) -> (std::rc::Rc<()>, RemovalPresentation<std::rc::Rc<()>>) {
        let (alive, mut removal) = counted();
        let target = removal.begin().unwrap();
        removal.end(target, status);
        (alive, removal)
    }

    // Removed: only "Done" (the drive is off); the target is dropped; the
    // operation's own result is unchanged.
    #[test]
    fn removed_leaves_only_done() {
        let status = RemovalStatus::Removed {
            unmounted_filesystems: true,
        };
        let (alive, removal) = finished_with(status);
        assert_eq!(std::rc::Rc::strong_count(&alive), 1, "target dropped");
        let view = shown_with(&removal);
        assert_eq!(view.removal, Some(RemovalNotice::Finished(status)));
        assert_eq!(view.actions, [ResultAction::Done]);
        assert!(view.actions_enabled);
        // Nothing starts again.
        let mut removal = removal;
        assert_eq!(removal.begin(), None);
        // Done then leaves for a new start.
        assert!(matches!(
            leave(removal, ResultAction::Done),
            Leaving::Main(MainReturn {
                keep_image: false,
                target: TargetReturn::Reset
            })
        ));
    }

    // Busy: "try again" with the same target, from the start (the window
    // calls the library again with it; nothing else is kept).
    #[test]
    fn busy_offers_another_try_with_the_same_target() {
        let (alive, removal) = finished_with(RemovalStatus::Busy);
        let view = shown_with(&removal);
        assert_eq!(
            view.actions,
            [
                ResultAction::RetryRemoval,
                ResultAction::WriteAnother,
                ResultAction::Done
            ]
        );
        // "Try again" stays on the result, target and all...
        let mut removal = match leave(removal, ResultAction::RetryRemoval) {
            Leaving::Stay(removal) => removal,
            other => panic!("{other:?}"),
        };
        // ...and starts the request again with that same target.
        let again = removal.begin().expect("the same target again");
        assert!(std::rc::Rc::ptr_eq(&again, &alive));
        assert!(removal.is_removing());
        assert_eq!(removal.begin(), None);
        removal.end(
            again,
            RemovalStatus::Removed {
                unmounted_filesystems: false,
            },
        );
        assert_eq!(std::rc::Rc::strong_count(&alive), 1);
    }

    // Every other failure: no retry (the target is dropped, so a stale one
    // can never be used), the original way back and "Done" stay.
    #[test]
    fn other_failures_offer_no_retry_and_keep_the_way_back() {
        for status in [
            RemovalStatus::DeviceGone,
            RemovalStatus::DeviceChanged,
            RemovalStatus::Unsupported,
            RemovalStatus::NotAuthorized,
            RemovalStatus::NotCompleted,
        ] {
            let (alive, mut removal) = finished_with(status);
            assert_eq!(std::rc::Rc::strong_count(&alive), 1, "{status:?}");
            let view = shown_with(&removal);
            assert_eq!(view.removal, Some(RemovalNotice::Finished(status)));
            assert_eq!(
                view.actions,
                [ResultAction::WriteAnother, ResultAction::Done],
                "{status:?}"
            );
            assert_eq!(removal.begin(), None, "{status:?}");
        }
        // A request that ended without an outcome is not completed either.
        let (alive, mut removal) = counted();
        let target = removal.begin().unwrap();
        drop(target);
        removal.end_without_outcome();
        assert_eq!(
            removal,
            RemovalPresentation::Finished {
                status: RemovalStatus::NotCompleted,
                retry: None
            }
        );
        assert_eq!(std::rc::Rc::strong_count(&alive), 1);
    }

    // Safe Removal never changes the operation's own result: the same case,
    // kind and steps whatever it did, for every ending.
    #[test]
    fn the_operation_result_is_independent_of_safe_removal() {
        for ending in every_ending() {
            let Some(base) = view(ending, VerifyMode::Quick, &unavailable()) else {
                continue;
            };
            for (_, status) in every_outcome() {
                let (_alive, removal) = finished_with(status);
                let after = view(ending, VerifyMode::Quick, &removal).unwrap();
                assert_eq!(
                    (after.case, after.kind, after.steps),
                    (base.case, base.kind, base.steps),
                    "{ending:?} {status:?}"
                );
            }
        }
    }

    // Leaving after a finished removal drops whatever target was kept.
    #[test]
    fn leaving_after_removal_drops_the_target() {
        let (alive, removal) = finished_with(RemovalStatus::Busy);
        assert_eq!(std::rc::Rc::strong_count(&alive), 2);
        assert!(matches!(
            leave(removal, ResultAction::WriteAnother),
            Leaving::Main(_)
        ));
        assert_eq!(std::rc::Rc::strong_count(&alive), 1);
    }

    // The library's blocking request is called in one place only, inside
    // the closure GIO runs on its blocking pool -- never on the GTK thread.
    #[test]
    fn the_request_runs_only_off_the_gtk_thread() {
        let call = concat!("request_safe", "_removal(");
        let sources = [
            ("main.rs", include_str!("main.rs")),
            ("window.rs", include_str!("window.rs")),
            ("operation.rs", include_str!("operation.rs")),
            ("model.rs", include_str!("model.rs")),
            ("text.rs", include_str!("text.rs")),
            ("expansion.rs", include_str!("expansion.rs")),
            ("result.rs", include_str!("result.rs")),
        ];
        let calls: Vec<(&str, usize)> = sources
            .iter()
            .flat_map(|(name, source)| source.match_indices(call).map(move |(at, _)| (*name, at)))
            .collect();
        assert_eq!(calls.len(), 1, "{calls:?}");
        let (name, at) = calls[0];
        assert_eq!(name, "window.rs");
        let window = include_str!("window.rs");
        let before = &window[..at];
        let pool = before
            .rfind(concat!("gio::spawn", "_blocking(move || {"))
            .expect("inside a blocking-pool closure");
        // Nothing closes that closure between its start and the call.
        assert!(!before[pool..].contains("});"), "{}", &before[pool..]);
    }

    // Safe Removal's internals stay in the library: the GUI names only the
    // public Production API.
    #[test]
    fn only_the_public_api_is_used() {
        let code = |source: &'static str| source.split("#[cfg(test)]").next().unwrap();
        for (name, source) in [
            ("window.rs", code(include_str!("window.rs"))),
            ("result.rs", code(include_str!("result.rs"))),
            ("text.rs", code(include_str!("text.rs"))),
        ] {
            for internal in [
                "RemovalFacts",
                "RemovalPlan",
                "RemovalPlatform",
                "RemovalCallError",
                "DeviceSnapshot",
                "drive_path",
            ] {
                assert!(!source.contains(internal), "{name}: {internal}");
            }
        }
    }

    #[test]
    fn every_result_can_be_left() {
        for ending in every_ending() {
            if let Some(view) = view(ending, VerifyMode::Quick, &unavailable()) {
                assert_eq!(view.actions.last(), Some(&ResultAction::Done));
                assert_eq!(view.kind, view.case.kind());
            }
        }
    }
}

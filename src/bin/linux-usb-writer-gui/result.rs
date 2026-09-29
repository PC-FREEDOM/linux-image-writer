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

use linux_usb_writer::VerifyMode;

use crate::model::{MainReturn, TargetReturn};
use crate::operation::{Ending, Mark, Reason, STEPS, Step};

// ---- Safe Removal, as the result view shows it ----

// Where Safe Removal stands for the finished operation. `T` is the
// library's opaque `RemovalTarget` (a parameter only so the tests, which
// cannot make one, can use a stand-in; the window uses `RemovalTarget`).
//
// Only the two states the result view uses today: the request itself --
// with a state while it runs, and one for each of its outcomes, of which
// only the library's `SafeRemovalOutcome::Removed` may say the drive can be
// removed -- is added with the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemovalPresentation<T> {
    // The finished worker handed out no target: Safe Removal is not offered.
    Unavailable,
    // It did: Safe Removal can be requested for this target.
    Ready(T),
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

    pub fn offers_removal(&self) -> bool {
        matches!(self, RemovalPresentation::Ready(_))
    }
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
            ResultAction::SafeRemoval => None,
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
// while the result stays.
pub fn leave<T>(removal: RemovalPresentation<T>, action: ResultAction) -> Leaving<T> {
    match action.main_return() {
        Some(ret) => {
            drop(removal);
            Leaving::Main(ret)
        }
        // Safe Removal is not requested from here yet: nothing happens.
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
    pub case: ResultCase,
    pub kind: ResultKind,
    pub steps: [StepLine; 3],
    // In order; Safe Removal first when offered.
    pub actions: Vec<ResultAction>,
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
    let mut actions = Vec::new();
    if removal.offers_removal() {
        actions.push(ResultAction::SafeRemoval);
    }
    actions.push(case.next());
    actions.push(ResultAction::Done);
    Some(ResultView {
        case,
        kind: case.kind(),
        steps,
        actions,
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
        assert!(!unavailable().offers_removal());
        assert!(RemovalPresentation::Ready(Target).offers_removal());
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

    // In 3a, Safe Removal requests nothing: the result stays as it was,
    // with the same target, and no GUI source calls the request.
    #[test]
    fn safe_removal_is_not_connected_yet() {
        let (alive, presentation) = counted();
        match leave(presentation, ResultAction::SafeRemoval) {
            Leaving::Stay(RemovalPresentation::Ready(target)) => {
                assert!(std::rc::Rc::ptr_eq(&target, &alive));
            }
            other => panic!("{other:?}"),
        }
        let request = concat!("request_safe", "_removal");
        for (name, source) in [
            ("main.rs", include_str!("main.rs")),
            ("window.rs", include_str!("window.rs")),
            ("operation.rs", include_str!("operation.rs")),
            ("model.rs", include_str!("model.rs")),
            ("text.rs", include_str!("text.rs")),
            ("expansion.rs", include_str!("expansion.rs")),
            ("result.rs", include_str!("result.rs")),
        ] {
            assert!(!source.contains(request), "{name}");
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

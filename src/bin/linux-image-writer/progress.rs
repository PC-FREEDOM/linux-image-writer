// The operation's overall progress (0..=1) as the progress bar shows it,
// computed from what the worker reported (`Tracker`) -- never from time.
// Pure logic, no GTK, so every rule is unit tested; the window only shows
// the result.
//
// The rules:
//
//   - Each phase owns a fixed share of the bar, by Verify mode (`weights`).
//   - Write is measured by the bytes confirmed as written back to the device
//     (`Tracker::writeback`), never by the bytes the kernel accepted
//     (`Tracker::written`): accepted data may still be waiting in memory.
//     Until the first write-back confirmation, Write stays at its start.
//   - Finalize (the final sync) has no measurable progress: it holds its
//     start value, shown as busy, and moves on only once the sync succeeded.
//   - Verify is measured by the bytes verified.
//   - 100% is only ever shown for a completed operation (`COMPLETED`); while
//     it runs the value stops at `RUNNING_MAX`.
//   - The value never moves backwards, and while a cancellation is under way
//     it does not move at all (`Tracker::overall`).

use linux_image_writer::{OpenPurpose, VerifyMode};

use crate::operation::{Activity, Ending, Tracker};

// The most a running operation shows: 99%, whatever its last phase reports.
pub const RUNNING_MAX: f64 = 0.99;

// Where each phase ends on the bar (it starts where the previous one ends;
// Prepare starts at 0, and Verify, if any, ends at 1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Weights {
    pub prepare_end: f64,
    pub write_end: f64,
    pub finalize_end: f64,
}

pub fn weights(mode: VerifyMode) -> Weights {
    match mode {
        VerifyMode::None => Weights {
            prepare_end: 0.03,
            write_end: 0.95,
            finalize_end: 1.0,
        },
        VerifyMode::Quick => Weights {
            prepare_end: 0.03,
            write_end: 0.92,
            finalize_end: 0.98,
        },
        VerifyMode::Full => Weights {
            prepare_end: 0.03,
            write_end: 0.75,
            finalize_end: 0.80,
        },
    }
}

// What the progress bar shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverallProgress {
    // 0.0 ..= 1.0; 1.0 only for `COMPLETED`.
    pub fraction: f64,
    // Work continues but the value cannot move (nothing measurable): the
    // bar shows activity on top of the fixed value.
    pub busy: bool,
}

// A completed operation: the only 100%.
pub const COMPLETED: OverallProgress = OverallProgress {
    fraction: 1.0,
    busy: false,
};

impl OverallProgress {
    // Whole percent, rounded down (so 99.9% reads 99%).
    pub fn percent(self) -> u8 {
        (self.fraction.clamp(0.0, 1.0) * 100.0).floor() as u8
    }
}

// What the progress bar shows: while the operation runs, its overall
// progress; once it ended, 100% for a completed operation and nothing for
// any other ending (cancelled, failed, lost).
pub fn shown(tracker: &Tracker, ending: Option<Ending>) -> Option<OverallProgress> {
    match ending {
        None => Some(tracker.overall()),
        Some(ending) if ending.is_completed() => Some(COMPLETED),
        Some(_) => None,
    }
}

// The value the reported state alone gives, before the "never backwards /
// frozen while cancelling" rules (`Tracker::overall`) apply.
pub fn measured(tracker: &Tracker) -> OverallProgress {
    let w = weights(tracker.verify_mode);
    let (fraction, busy) = match tracker.activity {
        Activity::Starting | Activity::PreparingImage => (0.0, true),
        Activity::Preflight => (
            within(0.0, w.prepare_end, tracker.preflight.map(|t| t.fraction())),
            false,
        ),
        // Waiting for the user: nothing is running.
        Activity::AwaitingConfirmation => (0.0, false),
        // The write is about to start (re-check, authentication).
        Activity::StartingWrite | Activity::OpeningDevice(OpenPurpose::Write) => {
            (w.prepare_end, true)
        }
        // Written back, not accepted: until the first confirmation the bar
        // stays at the start of Write and shows activity.
        Activity::Writing => match tracker.writeback {
            Some(writeback) => (
                within(w.prepare_end, w.write_end, Some(writeback.fraction())),
                false,
            ),
            None => (w.prepare_end, true),
        },
        // The final sync: fixed at its start until it succeeded.
        Activity::Syncing if tracker.synced => (w.finalize_end, false),
        Activity::Syncing => (w.write_end, true),
        Activity::OpeningDevice(OpenPurpose::Verify) | Activity::PreparingVerify => {
            (w.finalize_end, true)
        }
        Activity::Verifying => (
            within(w.finalize_end, 1.0, tracker.verified.map(|t| t.fraction())),
            false,
        ),
        // The outcome arrived; the result view shows it (`COMPLETED` only
        // for a completed operation).
        Activity::Finished => (0.0, false),
    };
    OverallProgress {
        fraction: fraction.min(RUNNING_MAX),
        busy: busy || tracker.cancel_requested,
    }
}

// `start + (end - start) * part`, `start` while the part is unknown.
fn within(start: f64, end: f64, part: Option<f64>) -> f64 {
    start + (end - start) * part.unwrap_or(0.0).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation::Transfer;
    use linux_image_writer::WorkerEvent;
    use linux_image_writer::report::{VerifyProgress, WriteProgress, WritebackProgress};

    const TOTAL: u64 = 1000;

    fn approx(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    fn accepted(done: u64) -> WorkerEvent {
        WorkerEvent::WriteProgress(WriteProgress {
            bytes_written: done,
            total_bytes: TOTAL,
        })
    }

    fn written_back(done: u64) -> WorkerEvent {
        WorkerEvent::WritebackProgress(WritebackProgress {
            completed_bytes: done,
            total_bytes: TOTAL,
        })
    }

    fn verified(mode: VerifyMode, done: u64, total: u64) -> WorkerEvent {
        WorkerEvent::VerifyProgress(VerifyProgress {
            mode,
            verified_bytes: done,
            total_bytes: total,
        })
    }

    // A tracker at the start of the write (approved, writing).
    fn writing(mode: VerifyMode) -> Tracker {
        let mut tracker = Tracker::new(mode);
        tracker.apply(&WorkerEvent::ImageSelected { image_size: TOTAL });
        tracker.confirmation_requested();
        tracker.answered(true);
        tracker.apply(&WorkerEvent::WriteStarted);
        tracker
    }

    // A tracker whose write and final sync are done.
    fn synced(mode: VerifyMode) -> Tracker {
        let mut tracker = writing(mode);
        tracker.apply(&accepted(TOTAL));
        tracker.apply(&WorkerEvent::WriteSucceeded {
            bytes_written: TOTAL,
            image_size: TOTAL,
        });
        tracker.apply(&WorkerEvent::SyncStarted);
        tracker.apply(&written_back(TOTAL));
        tracker.apply(&WorkerEvent::SyncSucceeded {
            bytes_written: TOTAL,
        });
        tracker
    }

    // Records the overall value after each step, checking the invariants as
    // it goes: within [0, 1), never 100% while running, never backwards.
    struct Run {
        tracker: Tracker,
        values: Vec<f64>,
    }

    impl Run {
        fn new(tracker: Tracker) -> Self {
            let value = tracker.overall().fraction;
            Run {
                tracker,
                values: vec![value],
            }
        }

        fn apply(&mut self, event: WorkerEvent) -> OverallProgress {
            self.tracker.apply(&event);
            let overall = self.tracker.overall();
            assert!((0.0..1.0).contains(&overall.fraction), "{overall:?}");
            assert!(overall.percent() < 100, "{overall:?}");
            assert!(
                overall.fraction >= *self.values.last().unwrap(),
                "went backwards after {event:?}: {overall:?}"
            );
            self.values.push(overall.fraction);
            overall
        }
    }

    // ---- weights ----

    // Each mode's phase boundaries, as specified; Verify ends at 100% when
    // there is one, Finalize does when there is none.
    #[test]
    fn weights_per_verify_mode() {
        assert_eq!(
            weights(VerifyMode::None),
            Weights {
                prepare_end: 0.03,
                write_end: 0.95,
                finalize_end: 1.0
            }
        );
        assert_eq!(
            weights(VerifyMode::Quick),
            Weights {
                prepare_end: 0.03,
                write_end: 0.92,
                finalize_end: 0.98
            }
        );
        assert_eq!(
            weights(VerifyMode::Full),
            Weights {
                prepare_end: 0.03,
                write_end: 0.75,
                finalize_end: 0.80
            }
        );
    }

    // ---- whole runs (1-3) ----

    fn whole_run(mode: VerifyMode) -> Run {
        let w = weights(mode);
        let mut run = Run::new(Tracker::new(mode));
        approx(run.tracker.overall().fraction, 0.0);
        run.apply(WorkerEvent::ImageSelected { image_size: TOTAL });
        run.tracker.confirmation_requested();
        run.tracker.answered(true);
        approx(run.tracker.overall().fraction, w.prepare_end);
        run.apply(WorkerEvent::WriteStarted);
        run.apply(accepted(400));
        approx(run.tracker.overall().fraction, w.prepare_end);
        run.apply(written_back(500));
        approx(
            run.tracker.overall().fraction,
            w.prepare_end + (w.write_end - w.prepare_end) * 0.5,
        );
        run.apply(accepted(TOTAL));
        run.apply(WorkerEvent::WriteSucceeded {
            bytes_written: TOTAL,
            image_size: TOTAL,
        });
        let finalizing = run.apply(WorkerEvent::SyncStarted);
        approx(finalizing.fraction, w.write_end);
        assert!(finalizing.busy);
        run.apply(written_back(TOTAL));
        run.apply(WorkerEvent::SyncSucceeded {
            bytes_written: TOTAL,
        });
        approx(
            run.tracker.overall().fraction,
            w.finalize_end.min(RUNNING_MAX),
        );
        run
    }

    #[test]
    fn verify_none_runs_prepare_write_finalize_then_completes() {
        let run = whole_run(VerifyMode::None);
        // Finalize ends at 100%, but a running operation shows at most 99%.
        approx(run.tracker.overall().fraction, RUNNING_MAX);
        let ending = Ending::WrittenWithoutVerify;
        assert!(ending.is_completed());
        assert_eq!(COMPLETED.percent(), 100);
    }

    #[test]
    fn quick_runs_through_verify_then_completes() {
        let mut run = whole_run(VerifyMode::Quick);
        let w = weights(VerifyMode::Quick);
        run.apply(WorkerEvent::VerifyPending {
            mode: VerifyMode::Quick,
        });
        approx(run.tracker.overall().fraction, w.finalize_end);
        run.apply(WorkerEvent::VerifyStarted);
        // 8. Quick Verify 50%.
        run.apply(verified(VerifyMode::Quick, 6, 12));
        approx(
            run.tracker.overall().fraction,
            w.finalize_end + (1.0 - w.finalize_end) * 0.5,
        );
        let last = run.apply(verified(VerifyMode::Quick, 12, 12));
        approx(last.fraction, RUNNING_MAX);
        assert!(Ending::Verified(VerifyMode::Quick).is_completed());
    }

    #[test]
    fn full_runs_through_verify_then_completes() {
        let mut run = whole_run(VerifyMode::Full);
        let w = weights(VerifyMode::Full);
        run.apply(WorkerEvent::VerifyPending {
            mode: VerifyMode::Full,
        });
        run.apply(WorkerEvent::VerifyStarted);
        // 9. Full Verify 50%.
        run.apply(verified(VerifyMode::Full, TOTAL / 2, TOTAL));
        approx(
            run.tracker.overall().fraction,
            w.finalize_end + (1.0 - w.finalize_end) * 0.5,
        );
        run.apply(verified(VerifyMode::Full, TOTAL, TOTAL));
        assert!(run.tracker.overall().percent() < 100);
        assert!(Ending::Verified(VerifyMode::Full).is_completed());
    }

    // ---- write (4-6) ----

    // 4. Every byte accepted, nothing confirmed: Write has not moved, and
    // the bar shows activity instead.
    #[test]
    fn accepted_bytes_alone_do_not_move_write() {
        for mode in [VerifyMode::None, VerifyMode::Quick, VerifyMode::Full] {
            let mut tracker = writing(mode);
            tracker.apply(&accepted(TOTAL));
            let overall = tracker.overall();
            approx(overall.fraction, weights(mode).prepare_end);
            assert!(overall.busy);
        }
    }

    // 5. Written back 50%: the middle of Write; written-back growth moves it.
    #[test]
    fn written_back_half_is_the_middle_of_write() {
        let mut tracker = writing(VerifyMode::Quick);
        tracker.apply(&accepted(800));
        tracker.apply(&written_back(500));
        let w = weights(VerifyMode::Quick);
        let half = tracker.overall();
        approx(
            half.fraction,
            w.prepare_end + (w.write_end - w.prepare_end) * 0.5,
        );
        assert!(!half.busy);
        tracker.apply(&written_back(600));
        assert!(tracker.overall().fraction > half.fraction);
    }

    // 6. Everything written back during the write is still not 100%: it is
    // the end of Write, short of Finalize (and of Verify).
    #[test]
    fn written_back_total_is_not_complete() {
        for mode in [VerifyMode::None, VerifyMode::Quick, VerifyMode::Full] {
            let mut tracker = writing(mode);
            tracker.apply(&accepted(TOTAL));
            tracker.apply(&written_back(TOTAL));
            let overall = tracker.overall();
            approx(overall.fraction, weights(mode).write_end);
            assert!(overall.percent() < 100);
        }
    }

    // 7. Finalize holds its start value, shown as busy, however long the
    // sync takes; it moves on only when the sync succeeded.
    #[test]
    fn finalizing_holds_a_fixed_value_and_shows_activity() {
        let mut tracker = writing(VerifyMode::None);
        tracker.apply(&accepted(TOTAL));
        tracker.apply(&written_back(TOTAL - 64));
        tracker.apply(&WorkerEvent::WriteSucceeded {
            bytes_written: TOTAL,
            image_size: TOTAL,
        });
        tracker.apply(&WorkerEvent::SyncStarted);
        let start = tracker.overall();
        approx(start.fraction, weights(VerifyMode::None).write_end);
        assert!(start.busy);
        assert!(start.percent() < 100);
        // Nothing reported: nothing moves (no time-based progress exists).
        assert_eq!(tracker.overall(), start);
    }

    // ---- cancel / end (10-13) ----

    // 11. While cancelling, the value stays where the cancellation found
    // it -- the drain's write-back confirmation does not move it towards
    // success -- and the bar shows activity.
    #[test]
    fn cancelling_freezes_the_value() {
        let mut tracker = writing(VerifyMode::Quick);
        tracker.apply(&accepted(700));
        tracker.apply(&written_back(300));
        let before = tracker.overall().fraction;
        tracker.cancel_requested = true;
        tracker.apply(&WorkerEvent::CancelDrainStarted { bytes_written: 700 });
        tracker.apply(&written_back(700));
        let cancelling = tracker.overall();
        approx(cancelling.fraction, before);
        assert!(cancelling.busy);
        assert_eq!(tracker.pending_writeback(), Some(0));
    }

    // 10, 12, 13. Only a completed operation is 100%: a cancelled or failed
    // one is not, and the finished tracker itself never claims it.
    #[test]
    fn only_completed_is_100_percent() {
        use crate::operation::Reason;
        for ending in [
            Ending::Verified(VerifyMode::Quick),
            Ending::Verified(VerifyMode::Full),
            Ending::WrittenWithoutVerify,
        ] {
            assert!(ending.is_completed(), "{ending:?}");
        }
        for ending in [
            Ending::CancelledBeforeWrite,
            Ending::ConfirmationDeclined,
            Ending::CancelledDuringWrite {
                target_modified: true,
            },
            Ending::CancelledDuringVerify,
            Ending::NotStarted {
                at: crate::operation::Step::Write,
                reason: Reason::WriteError,
            },
            Ending::WriteFailed {
                reason: Reason::WriteError,
                target_modified: true,
            },
            Ending::VerifyFailed {
                reason: Reason::VerifyMismatch,
            },
            Ending::Lost,
        ] {
            assert!(!ending.is_completed(), "{ending:?}");
        }

        let mut tracker = synced(VerifyMode::None);
        tracker.finished();
        assert!(tracker.overall().percent() < 100);
    }

    // 14. What the bar shows: the running value (never 100%), 100% for a
    // completed ending, nothing for a cancelled or failed one -- even when
    // the tracker had reached the end of its last phase.
    #[test]
    fn the_bar_shows_100_percent_only_for_a_completed_ending() {
        let mut tracker = synced(VerifyMode::None);
        let running = shown(&tracker, None).unwrap();
        assert!(running.percent() < 100);
        tracker.finished();
        assert_eq!(
            shown(&tracker, Some(Ending::WrittenWithoutVerify)),
            Some(COMPLETED)
        );
        assert_eq!(
            shown(&tracker, Some(Ending::Verified(VerifyMode::Full))),
            Some(COMPLETED)
        );
        assert_eq!(shown(&tracker, Some(Ending::CancelledDuringVerify)), None);
        assert_eq!(
            shown(
                &tracker,
                Some(Ending::CancelledDuringWrite {
                    target_modified: true
                })
            ),
            None
        );
        assert_eq!(
            shown(
                &tracker,
                Some(Ending::WriteFailed {
                    reason: crate::operation::Reason::WriteError,
                    target_modified: true
                })
            ),
            None
        );
        assert_eq!(shown(&tracker, Some(Ending::Lost)), None);
    }

    // Verify starting never moves the value back; Preflight moves within
    // Prepare only.
    #[test]
    fn phase_changes_never_move_backwards() {
        let mut run = Run::new(Tracker::new(VerifyMode::Full));
        run.apply(WorkerEvent::CompressedImageDetected {
            format: linux_image_writer::report::CompressionFormat::Xz,
        });
        let preflight = run.apply(WorkerEvent::PreflightProgress(
            linux_image_writer::report::PreflightProgress {
                compressed_consumed: 50,
                compressed_total: 100,
                logical_produced: 10,
            },
        ));
        approx(preflight.fraction, 0.015);
        run.tracker = synced(VerifyMode::Full);
        run.values.push(run.tracker.overall().fraction);
        run.apply(WorkerEvent::VerifyPending {
            mode: VerifyMode::Full,
        });
        run.apply(WorkerEvent::VerifyStarted);
        run.apply(verified(VerifyMode::Full, 0, TOTAL));
    }

    #[test]
    fn transfer_fraction_is_used_unchanged() {
        approx(Transfer { done: 1, total: 4 }.fraction(), 0.25);
    }
}

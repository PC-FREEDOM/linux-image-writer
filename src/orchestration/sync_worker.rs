// Runs one unit of blocking work (in production, `Syncing::sync()`) on a
// scoped worker thread (orchestration, Core layer). Moved unchanged from
// `main.rs` (Phase 3A-1).

// How a unit of blocking work handed to `run_off_main_thread` ended.
// `NotStarted` hands the input back untouched (the worker thread could not
// be created, so the work never began), letting the caller decide how to
// proceed without having lost the value.
#[derive(Debug)]
pub(crate) enum OffMainThread<T, R> {
    Finished(R),
    Panicked,
    NotStarted(T, std::io::Error),
}

// Runs `work(input)` on a dedicated, scoped worker thread and blocks the
// calling (main) thread in `join()` until it finishes. Exists for
// `Syncing::sync()`: `fsync()` on a block device keeps the calling thread
// in uninterruptible sleep, and the kernel delivers a terminal SIGINT to the
// main thread first -- so when the main thread itself was in `fsync()`,
// ctrlc's OS-level handler only ran once `fsync()` returned, and the
// `request_cancel()` its dispatch thread performs could lose the race
// against the post-sync cancel check (see reports/latest.md). Waiting in
// `join()` instead is an interruptible wait, so a Ctrl+C during sync is
// handled while sync is still running. This narrows the window to a
// Ctrl+C landing at (nearly) the same moment sync completes; it does not
// make that residual race impossible.
//
// The work itself is never interrupted: `join()` always waits for it to
// finish. A panic inside `work` is reported as `Panicked` rather than
// propagated into the main thread; the input was consumed by the worker in
// that case (its destructors ran there during unwinding).
pub(crate) fn run_off_main_thread<T: Send, R: Send>(
    input: T,
    work: impl FnOnce(T) -> R + Send,
) -> OffMainThread<T, R> {
    let mut slot = Some(input);
    let slot_ref = &mut slot;

    let joined = std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("sync-worker".into())
            .spawn_scoped(scope, move || {
                let input = slot_ref
                    .take()
                    .expect("worker input is present until the worker takes it");
                work(input)
            })
            .map(|handle| handle.join())
    });

    match joined {
        Ok(Ok(result)) => OffMainThread::Finished(result),
        Ok(Err(_panic_payload)) => OffMainThread::Panicked,
        Err(spawn_error) => match slot.take() {
            Some(input) => OffMainThread::NotStarted(input, spawn_error),
            None => OffMainThread::Panicked,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{OffMainThread, run_off_main_thread};
    use std::time::Duration;

    // ---------------------------------------------------------------------
    // Sync off the main thread (S1): `run_off_main_thread` is exercised with
    // plain closures standing in for `Syncing::sync()` -- no block device,
    // no fsync. The real call site's `Syncing`/`SyncAttemptOutcome` Send
    // bounds are enforced by the compiler at that call site itself.
    // ---------------------------------------------------------------------

    // S1a. A successful result is returned to the caller unchanged, and the
    // work ran on a thread other than the caller's.
    #[test]
    fn off_main_thread_returns_success_from_another_thread() {
        let caller = std::thread::current().id();

        match run_off_main_thread(21u32, |value| (value * 2, std::thread::current().id())) {
            OffMainThread::Finished((result, worker)) => {
                assert_eq!(result, 42);
                assert_ne!(worker, caller, "work must not run on the calling thread");
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    // S1b. An error result (the stand-in for `SyncAttemptOutcome::Failed`) is
    // returned as-is -- `Finished`, not `Panicked`: a failure the work
    // reported is a result, not a crash.
    #[test]
    fn off_main_thread_returns_error_result_unchanged() {
        let outcome = run_off_main_thread((), |()| -> Result<(), std::io::Error> {
            Err(std::io::Error::other("sync failed"))
        });

        match outcome {
            OffMainThread::Finished(Err(error)) => assert_eq!(error.to_string(), "sync failed"),
            other => panic!("expected Finished(Err), got {other:?}"),
        }
    }

    // S1c. A panic in the worker is reported as `Panicked` instead of
    // propagating into the caller. (The panic message printed to stderr by
    // the default hook is expected test output.)
    #[test]
    fn off_main_thread_reports_worker_panic() {
        let outcome = run_off_main_thread((), |()| -> u32 {
            panic!("simulated sync worker panic");
        });

        assert!(matches!(outcome, OffMainThread::Panicked));
    }

    // S1d. The caller blocks until the work has fully finished -- the work is
    // never abandoned or interrupted, even if it takes a while.
    #[test]
    fn off_main_thread_waits_for_work_to_complete() {
        let outcome = run_off_main_thread(Duration::from_millis(30), |delay| {
            std::thread::sleep(delay);
            "done"
        });

        assert!(matches!(outcome, OffMainThread::Finished("done")));
    }
}

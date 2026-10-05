// Debug-build-only diagnostics for a cancelled write: one `[CancelDiag]`
// line on stderr at each step from the Cancel request to `Cancelled`
// (CancelDrain -> fdatasync -> write-back reported -> close -> Cancelled),
// with monotonic times (`Instant`) and byte counts, for checking on real
// hardware which path a cancellation took and how long each step waited.
//
// Observation only: nothing here is read back by the operation, so no
// decision, event or I/O depends on it. Compiled only with
// `debug_assertions` (the module and every call to it), so a release build
// contains neither the code nor the `[CancelDiag]` strings.

use crate::execution::write_job::CancelSynced;
use crate::orchestration::outcome::{CancelledAt, OperationOutcome};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const PREFIX: &str = "[CancelDiag]";

struct State {
    // When the current write started (`WriteStarted`).
    write_started: Option<Instant>,
    // When the cancellation was first requested, for the current write.
    cancel_requested: Option<Instant>,
    // The write loop's latest write-back confirmation.
    writeback_completed: Option<u64>,
}

static STATE: Mutex<State> = Mutex::new(State {
    write_started: None,
    cancel_requested: None,
    writeback_completed: None,
});

fn with_state<T>(f: impl FnOnce(&mut State) -> T) -> T {
    // A poisoned lock still holds plain numbers; keep using them.
    let mut state = STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut state)
}

fn ms(duration: Duration) -> String {
    format!("{:.3}", duration.as_secs_f64() * 1000.0)
}

fn optional(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
}

// "+<ms since the Cancel request>" for every line after the request.
fn since_request(state: &State, now: Instant) -> String {
    match state.cancel_requested {
        Some(requested) => format!("since_cancel_ms={}", ms(now - requested)),
        None => "since_cancel_ms=unknown".to_owned(),
    }
}

fn log(message: &str) {
    eprintln!("{PREFIX} {message}");
}

// ---- the write loop ----

// A new write began: forget any earlier operation's request.
pub(crate) fn write_started() {
    with_state(|state| {
        state.write_started = Some(Instant::now());
        state.cancel_requested = None;
        state.writeback_completed = None;
    });
}

pub(crate) fn written_back(bytes: u64) {
    with_state(|state| state.writeback_completed = Some(bytes));
}

// ---- 1. Cancel requested ----

/// The user asked to cancel. `accepted` / `writeback_completed` /
/// `pending_writeback` are what the caller had shown (`None`: not known).
/// Only the first request of a write is timed; later ones are logged as
/// repeats.
#[doc(hidden)]
pub fn cancel_requested(
    accepted: Option<u64>,
    writeback_completed: Option<u64>,
    pending_writeback: Option<u64>,
) {
    let now = Instant::now();
    with_state(|state| {
        let repeat = state.cancel_requested.is_some();
        if !repeat {
            state.cancel_requested = Some(now);
        }
        let since_write = state
            .write_started
            .map_or_else(|| "unknown".to_owned(), |started| ms(now - started));
        log(&format!(
            "Cancel requested{}: since_write_start_ms={since_write} accepted_bytes={} \
             writeback_completed_bytes={} pending_writeback_bytes={}",
            if repeat { " (repeat)" } else { "" },
            optional(accepted),
            optional(writeback_completed),
            optional(pending_writeback),
        ));
    });
}

// The cancel flag was set, by whoever: times a request that did not come
// through `cancel_requested` (it keeps an earlier one).
pub(crate) fn cancel_flag_set() {
    let now = Instant::now();
    with_state(|state| {
        state.cancel_requested.get_or_insert(now);
    });
}

// ---- 2. CancelDrainStarted ----

pub(crate) fn cancel_drain_started(accepted: u64) {
    let now = Instant::now();
    with_state(|state| {
        let completed = state.writeback_completed;
        let pending = completed.map(|completed| accepted.saturating_sub(completed));
        log(&format!(
            "CancelDrainStarted: {} accepted_bytes={accepted} writeback_completed_bytes={} \
             pending_writeback_bytes={}",
            since_request(state, now),
            optional(completed),
            optional(pending),
        ));
    });
}

// ---- 3./4. fdatasync ----

pub(crate) fn fdatasync_started() -> Instant {
    let now = Instant::now();
    with_state(|state| log(&format!("fdatasync start: {}", since_request(state, now))));
    now
}

pub(crate) fn fdatasync_finished(started: Instant, error: Option<&std::io::Error>) {
    let now = Instant::now();
    with_state(|state| {
        let result = match error {
            None => "success".to_owned(),
            Some(error) => format!("failure ({error})"),
        };
        log(&format!(
            "fdatasync {result}: fdatasync_ms={} {}",
            ms(now - started),
            since_request(state, now),
        ));
    });
}

// ---- 5. WritebackProgress after drain ----

pub(crate) fn writeback_after_drain(synced: &CancelSynced) {
    let progress = synced.writeback_progress();
    let now = Instant::now();
    with_state(|state| {
        log(&format!(
            "WritebackProgress after drain: completed_bytes={} total_bytes={} {}",
            progress.completed_bytes,
            progress.total_bytes,
            since_request(state, now),
        ));
    });
}

// ---- 6./7. close ----

pub(crate) fn close_started() -> Instant {
    let now = Instant::now();
    with_state(|state| log(&format!("close start: {}", since_request(state, now))));
    now
}

pub(crate) fn close_finished(started: Instant) {
    let now = Instant::now();
    with_state(|state| {
        log(&format!(
            "close complete: close_ms={} {}",
            ms(now - started),
            since_request(state, now),
        ));
    });
}

// ---- 8. Cancelled outcome ----

// The operation ended: for a cancellation, where it stopped and the total
// time since the request. Every outcome then clears the state, so the next
// operation starts without this one's request.
pub(crate) fn outcome(outcome: &OperationOutcome) {
    let now = Instant::now();
    with_state(|state| {
        if let OperationOutcome::Cancelled(at) = outcome {
            let at = match at {
                CancelledAt::Preflight => "Preflight",
                CancelledAt::BeforeConfirmation => "BeforeConfirmation",
                CancelledAt::Confirmation => "Confirmation",
                CancelledAt::Write { .. } => "Write (after CancelDrain)",
                CancelledAt::AfterSync => "AfterSync (no CancelDrain)",
                CancelledAt::BeforeVerify => "BeforeVerify (no CancelDrain)",
                CancelledAt::Verify { .. } => "Verify (no CancelDrain)",
            };
            let total = state
                .cancel_requested
                .map_or_else(|| "unknown".to_owned(), |requested| ms(now - requested));
            log(&format!(
                "Cancelled outcome: at={at} total_since_cancel_request_ms={total}"
            ));
        } else if let Some(requested) = state.cancel_requested {
            // Requested, but the operation ended otherwise (e.g. a failed
            // drain, or it completed first).
            let kind = match outcome {
                OperationOutcome::Completed { .. } => "Completed",
                _ => "Failed",
            };
            log(&format!(
                "Outcome after cancel request: {kind} total_since_cancel_request_ms={}",
                ms(now - requested)
            ));
        }
        state.write_started = None;
        state.cancel_requested = None;
        state.writeback_completed = None;
    });
}

// Production orchestration (Core layer; see CLAUDE.md). The safe write
// procedure's own steps -- the order of selection, image preparation,
// confirmation, the Write Gate, write, sync and Verify -- are meant to live
// here, so that the CLI and a future GUI both drive the same path. Phase
// 3A-1 only moves the pure helpers here, unchanged; `main.rs`'s
// `run_write_test` still performs the sequence itself and calls them. Not a
// public API: nothing here is visible outside this crate.

pub(crate) mod candidates;
pub(crate) mod image;
pub(crate) mod operation;
pub(crate) mod platform;
pub(crate) mod sync_worker;

// Pure comparison used by the Human Confirmation prompt in `main.rs`'s
// `run_write_test`: the operator's raw input line, trimmed, must equal the
// target's `/dev` node string exactly -- case-sensitive, no partial/prefix
// match, no "y"/"yes" shortcut. Kept as its own small function (rather than
// inlined) purely so it can be unit tested without stdin or a real device.
pub(crate) fn confirmation_matches(input: &str, expected_device: &str) -> bool {
    input.trim() == expected_device
}

// What happens right after sync reported success: stop as cancelled if a
// cancellation was requested (sync has already finished, so the full image
// is on the target and only verification is skipped), or continue into
// Verify. Deliberately only for the success path: a sync failure is always
// reported as that failure, never masked by a cancellation that happened at
// the same time. `main.rs` turns `Cancelled` into its `WriteTestExit::
// Cancelled` (exit 130).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AfterSync {
    Cancelled,
    ContinueToVerify,
}

pub(crate) fn after_successful_sync(cancel_requested: bool) -> AfterSync {
    if cancel_requested {
        AfterSync::Cancelled
    } else {
        AfterSync::ContinueToVerify
    }
}

#[cfg(test)]
mod tests {
    use super::{AfterSync, after_successful_sync, confirmation_matches};

    // A. Exact match -> true.
    #[test]
    fn exact_match_confirms() {
        assert!(confirmation_matches("/dev/sdb", "/dev/sdb"));
    }

    // B. A trailing newline (as `read_line` always includes one) is
    // trimmed before comparing -> still true.
    #[test]
    fn trailing_newline_is_trimmed_before_comparing() {
        assert!(confirmation_matches("/dev/sdb\n", "/dev/sdb"));
        assert!(confirmation_matches("/dev/sdb\r\n", "/dev/sdb"));
    }

    // C. A different, even superficially similar, device string -> false.
    #[test]
    fn wrong_device_does_not_confirm() {
        assert!(!confirmation_matches("/dev/sdc", "/dev/sdb"));
        assert!(!confirmation_matches("/dev/sdb1", "/dev/sdb"));
    }

    // D. No "y"/"yes" shortcut -- only the exact device string confirms.
    #[test]
    fn yes_or_y_does_not_confirm() {
        assert!(!confirmation_matches("yes\n", "/dev/sdb"));
        assert!(!confirmation_matches("y\n", "/dev/sdb"));
    }

    // E. Empty input (including EOF, which this function never sees
    // directly since `run_write_test` special-cases it, but an empty
    // trimmed string must still never match a non-empty device) -> false.
    #[test]
    fn empty_input_does_not_confirm() {
        assert!(!confirmation_matches("", "/dev/sdb"));
        assert!(!confirmation_matches("\n", "/dev/sdb"));
    }

    // S1e. After a successful sync: cancellation requested -> stop as
    // cancelled; not requested -> continue into Verify.
    #[test]
    fn after_successful_sync_follows_cancel_flag() {
        assert_eq!(after_successful_sync(true), AfterSync::Cancelled);
        assert_eq!(after_successful_sync(false), AfterSync::ContinueToVerify);
    }
}

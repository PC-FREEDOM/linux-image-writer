mod device;
mod execution;
mod identity;
mod image_source;
mod linux_backend;
mod linux_monitor;
mod orchestration;
mod removal_check;
mod safe_removal_test;
mod safety;
mod writer;

use execution::{core, linux_access, write_job};
use identity::{IdentityComparison, InstanceComparison, compare_identity, compare_instance};
use linux_backend::{collect_device_snapshot, collect_device_snapshots};
use linux_monitor::{DeviceEvent, start_monitoring};
use orchestration::image::CompressedImageRejection;
use safety::assess_device;
use std::io::Write as _;

fn main() -> zbus::Result<()> {
    let mut args = std::env::args().skip(1);

    match args.next().as_deref() {
        Some("monitor") => return run_monitor(),
        Some("select") => {
            let Some(target) = args.next() else {
                eprintln!("usage: cargo run -- select <udisks2-block-object-path>");
                return Ok(());
            };

            return run_select(target);
        }
        Some("open-test") => {
            let Some(target) = args.next() else {
                eprintln!("usage: cargo run -- open-test <udisks2-block-object-path>");
                return Ok(());
            };

            return run_open_test(target);
        }
        Some("prepare-test") => {
            let Some(target) = args.next() else {
                eprintln!("usage: cargo run -- prepare-test <udisks2-block-object-path>");
                return Ok(());
            };

            return run_prepare_test(target);
        }
        Some("write-test") => {
            const USAGE: &str = "usage: cargo run -- write-test <image-path> <udisks2-block-object-path> [verify-mode] [--test-pause-before-verify]\n\
                 verify-mode: none (default) | quick | full\n\
                 --test-pause-before-verify: TEST-ONLY diagnostic option (not for normal use).\n\
                 Pauses after write+sync, before Verify fetches a fresh device snapshot, so a\n\
                 child partition can be mounted manually in another terminal -- see this flag's\n\
                 own doc comment on `pause_before_verify_for_test` for the full rationale.\n\
                 Requires verify-mode quick or full.";

            let Some(image_path) = args.next() else {
                eprintln!("{USAGE}");
                return Ok(());
            };
            let Some(target) = args.next() else {
                eprintln!("{USAGE}");
                return Ok(());
            };

            let verify_mode_arg = args.next();
            let fourth_arg = args.next();

            let (verify_mode, test_pause_before_verify) = match parse_write_test_trailing_args(
                verify_mode_arg.as_deref(),
                fourth_arg.as_deref(),
            ) {
                Ok(parsed) => parsed,
                // Present but unrecognized -> a usage error with a
                // non-zero exit, never a silent fallback to `None` or to
                // any other mode.
                Err(WriteTestArgsError::InvalidVerifyMode) => {
                    let mode_str = verify_mode_arg.as_deref().unwrap_or("");
                    eprintln!(
                        "write-test: invalid verify-mode '{mode_str}' (expected: none | quick | full)"
                    );
                    eprintln!("{USAGE}");
                    std::process::exit(1);
                }
                // Same policy for the fourth token: an unrecognized value
                // is a usage error, never silently ignored.
                Err(WriteTestArgsError::UnrecognizedFourthArgument) => {
                    let arg = fourth_arg.as_deref().unwrap_or("");
                    eprintln!(
                        "write-test: unrecognized argument '{arg}' (expected: --test-pause-before-verify)"
                    );
                    eprintln!("{USAGE}");
                    std::process::exit(1);
                }
                // `--test-pause-before-verify` with `verify-mode none` is
                // rejected outright rather than silently accepted-but-
                // ineffective: `VerifyMode::None` never produces
                // `VerifyStart::Pending`, so the flag would have no
                // observable effect at all -- see this design's own
                // report (reports/latest.md) for why "specified but does
                // nothing" was deliberately ruled out.
                Err(WriteTestArgsError::TestPauseRequiresVerification) => {
                    eprintln!(
                        "write-test: --test-pause-before-verify requires verify-mode quick or full (verify-mode none never runs Verify pre-flight)"
                    );
                    eprintln!("{USAGE}");
                    std::process::exit(1);
                }
            };

            let exit = run_write_test(image_path, target, verify_mode, test_pause_before_verify)?;
            // `std::process::exit` only here, at the very top level, and only
            // after `run_write_test` has already returned normally -- every
            // FD/state cleanup it triggers has already happened via ordinary
            // Rust `Drop` by this point (see `write_test_exit_code`'s own
            // doc comment).
            if let Some(code) = write_test_exit_code(exit) {
                std::process::exit(code);
            }
            return Ok(());
        }
        Some("writer-test") => return run_writer_test(),
        Some("removal-check") => return removal_check::run(),
        Some("safe-removal-test") => return safe_removal_test::run(args.collect()),
        _ => {}
    }

    let snapshots = collect_device_snapshots()?;

    println!("Safety assessments:");

    for snapshot in snapshots {
        let assessment = assess_device(&snapshot);

        println!();
        println!("Device:      {}", snapshot.device);
        println!("Model:       {} {}", snapshot.vendor, snapshot.model);
        println!("Size:        {} bytes", snapshot.size);
        println!("Bus:         {}", snapshot.connection_bus);
        println!("Removable:   {}", snapshot.removable);
        println!("Media avail: {}", snapshot.media_available);
        println!("Serial:      {}", snapshot.serial);
        println!("Major:minor: {}:{}", snapshot.major, snapshot.minor);
        println!("Diskseq:     {:?}", snapshot.diskseq);
        println!("Block path:  {}", snapshot.block_path);
        println!("Drive path:  {}", snapshot.drive_path);

        if snapshot.mount_points.is_empty() {
            println!("Mounts:      none");
        } else {
            println!("Mounts:");

            for mount in &snapshot.mount_points {
                println!("  {mount}");
            }
        }

        if snapshot.active_swap {
            println!("Active swap:");

            for swap in &snapshot.swap_devices {
                println!("  {swap}");
            }
        } else {
            println!("Active swap: none");
        }

        if snapshot.complex_storage {
            println!("Complex storage:");

            for detail in &snapshot.complex_storage_details {
                println!("  {detail}");
            }
        } else {
            println!("Complex storage: none");
        }

        println!("Risk:        {:?}", assessment.risk_level);
        println!("Writable:    {}", assessment.writable);
        println!("Reasons:     {:?}", assessment.reasons);

        let identity_self_check = compare_identity(&snapshot, &snapshot);
        println!("Identity self-check: {identity_self_check:?}");

        let instance_self_check = compare_instance(&snapshot, &snapshot);
        println!("Instance self-check: {instance_self_check:?}");
    }

    Ok(())
}

// PoC mode: read-only observation of UDisks2 D-Bus signals (`cargo run --
// monitor`). Prints raw structured events and, on each event, a fresh
// DeviceSnapshot re-read so diskseq changes (not carried by any signal) can
// be observed as they happen. Runs until interrupted with Ctrl+C.
fn run_monitor() -> zbus::Result<()> {
    println!("Monitoring UDisks2 D-Bus signals (read-only). Press Ctrl+C to stop.");

    let events = start_monitoring()?;

    for event in events {
        match &event {
            DeviceEvent::InterfacesAdded {
                object_path,
                interfaces,
            } => {
                println!("\n[InterfacesAdded] {object_path}");
                println!("  interfaces: {interfaces:?}");
            }
            DeviceEvent::InterfacesRemoved {
                object_path,
                interfaces,
            } => {
                println!("\n[InterfacesRemoved] {object_path}");
                println!("  interfaces: {interfaces:?}");
            }
            DeviceEvent::PropertiesChanged {
                object_path,
                interface,
                changed,
                invalidated,
            } => {
                println!("\n[PropertiesChanged] {object_path} ({interface})");

                for change in changed {
                    println!("  {} = {}", change.name, change.value);
                }

                if !invalidated.is_empty() {
                    println!("  invalidated: {invalidated:?}");
                }
            }
            DeviceEvent::WatcherFailed {
                object_path,
                reason,
            } => {
                eprintln!("\n[WatcherFailed] {object_path}: {reason}");
            }
        }

        println!("  -- current diskseq (re-fetched DeviceSnapshot) --");

        match collect_device_snapshots() {
            Ok(snapshots) => {
                for snapshot in &snapshots {
                    println!(
                        "  {} diskseq={:?} media_available={} size={}",
                        snapshot.device, snapshot.diskseq, snapshot.media_available, snapshot.size
                    );
                }
            }
            Err(error) => eprintln!("  snapshot refresh failed: {error}"),
        }
    }

    Ok(())
}

// PoC mode: Selection Continuity (`cargo run -- select <block_path>`).
// Performs one explicit selection, then watches UDisks2 signals and, on
// every event, both (a) folds the event into the SelectionState and (b)
// re-fetches just this one target and re-verifies Identity/Instance/Safety
// against it. Read-only throughout; never opens the device or writes to it.
// To demonstrate re-selection, stop this process (Ctrl+C) and run it again
// with `select` — a fresh process always starts from SelectionState::NoSelection,
// so the only way back to Selected is this explicit action, never automatic.
fn run_select(block_path: String) -> zbus::Result<()> {
    let mut state = attempt_select(&block_path);
    print_selection_state(&state);

    let events = start_monitoring()?;

    for event in events {
        state = core::apply_event(state, &event);

        if let core::SelectionState::Selected { baseline, .. } = &state {
            let outcome = collect_device_snapshot(&baseline.block_path);
            state = core::revalidate(state, outcome);
        }

        print_selection_state(&state);
    }

    Ok(())
}

// Selection goes through the same entry a UI will use
// (`orchestration::candidates::select_target`), with the CLI argument as an
// unverified block-path reference: a fresh snapshot, then `core::select()`.
// The messages are the ones this function printed before.
fn attempt_select(block_path: &str) -> core::SelectionState {
    use orchestration::candidates::{TargetRef, select_target};

    match select_target(
        &orchestration::platform::LinuxPlatform,
        &TargetRef::from_block_path(block_path),
    ) {
        Ok(state) => state,
        Err(error) => {
            print_select_error(&error, block_path);
            core::SelectionState::NoSelection
        }
    }
}

// The messages `attempt_select` has always printed when selection fails
// (also used by `write-test`, which selects through `orchestration`).
fn print_select_error(error: &orchestration::candidates::SelectTargetError, block_path: &str) {
    use orchestration::candidates::SelectTargetError;

    match error {
        SelectTargetError::NotSelectable(_) => eprintln!("select rejected: NotSelectable"),
        SelectTargetError::NotFound => eprintln!("select failed: no such target: {block_path}"),
        SelectTargetError::SnapshotUnavailable(reason) => eprintln!("select failed: {reason}"),
        // Only a reference from the candidate list can report this; a
        // block-path reference has nothing to compare with.
        SelectTargetError::CandidateChanged(change) => eprintln!("select rejected: {change:?}"),
    }
}

fn print_selection_state(state: &core::SelectionState) {
    match state {
        core::SelectionState::NoSelection => {
            println!("\n[Selection] NoSelection");
        }
        core::SelectionState::Selected {
            baseline,
            baseline_assessment,
            selection_generation,
        } => {
            println!(
                "\n[Selection] Selected  device={} risk={:?} writable={} diskseq={:?} selection_generation={:?}",
                baseline.device,
                baseline_assessment.risk_level,
                baseline_assessment.writable,
                baseline.diskseq,
                selection_generation
            );
        }
        core::SelectionState::Invalidated {
            baseline,
            baseline_assessment,
            reason,
            selection_generation,
        } => {
            println!(
                "\n[Selection] Invalidated  device={} reason={reason:?} (baseline was risk={:?} writable={}) selection_generation={:?}",
                baseline.device,
                baseline_assessment.risk_level,
                baseline_assessment.writable,
                selection_generation
            );
        }
    }
}

// PoC mode: OpenDevice safety check (`cargo run -- open-test <block_path>`).
// Runs the full pre-write safety pipeline up to — and only up to — holding an
// open file descriptor: select, one final targeted re-verification
// (Identity/Instance/Safety), OpenDevice, FD metadata inspection, an FD
// binding check against the just-re-verified snapshot, then close.
// No bytes are ever written, seeked-then-written, truncated, or otherwise
// modified through the returned descriptor — this function has no code path
// that could do so. If OpenDevice needs polkit authentication, this program
// does nothing but wait for the reply; it never falls back to sudo or any
// other bypass.
fn run_open_test(block_path: String) -> zbus::Result<()> {
    let mut state = attempt_select(&block_path);

    if let core::SelectionState::Selected { baseline, .. } = &state {
        let outcome = collect_device_snapshot(&baseline.block_path);
        state = core::revalidate(state, outcome);
    }

    print_selection_state(&state);

    if !core::is_ready_to_open(&state) {
        println!("\nSelection: invalid -- refusing to call OpenDevice.");
        return Ok(());
    }

    println!("\nSelection: valid");

    let core::SelectionState::Selected { baseline, .. } = &state else {
        unreachable!("is_ready_to_open just confirmed Selected");
    };

    println!(
        "Requesting OpenDevice(mode=\"rw\") on {}.",
        baseline.block_path
    );
    println!(
        "If a polkit authentication prompt appears, please complete it yourself -- \
         this program will not use sudo or any other privilege bypass."
    );

    let handle = match linux_access::open_device(
        &baseline.block_path,
        linux_access::OpenAccess::WriteExclusive,
    ) {
        Ok(handle) => handle,
        Err(error) => {
            println!("OpenDevice: failed ({error:?})");
            return Ok(());
        }
    };

    println!("OpenDevice: success");

    let metadata = handle.metadata();

    match &metadata {
        Some(meta) => {
            println!("FD major:minor: {}:{}", meta.major, meta.minor);
            println!(
                "Expected major:minor: {}:{}",
                baseline.major, baseline.minor
            );
            println!("FD size (BLKGETSIZE64): {:?}", meta.size);
            println!("Expected size: {}", baseline.size);
            println!("FD diskseq (BLKGETDISKSEQ): {:?}", meta.diskseq);
            println!("Expected diskseq: {:?}", baseline.diskseq);

            if let Some(target) = &meta.proc_fd_target {
                println!("/proc/self/fd target: {target}");
            }
        }
        None => println!("FD metadata: unavailable"),
    }

    match core::check_fd_binding(baseline, metadata.as_ref()) {
        core::FdBindingCheck::Match => println!("FD binding: Match"),
        core::FdBindingCheck::Mismatch => {
            println!("FD binding: Mismatch -- would abort before any write")
        }
        core::FdBindingCheck::InsufficientInformation => {
            println!("FD binding: InsufficientInformation -- would abort before any write")
        }
    }

    println!("Write performed: NO (0 bytes)");

    linux_access::close_without_writing(handle);
    println!("FD closed: yes");

    Ok(())
}

// The image size this PoC pretends to be about to write. Never actually read
// or written — only used to exercise WritePlan validation and the
// confirmation token. 4 MiB, matching writer-test's own pattern size.
const PREPARE_TEST_IMAGE_SIZE: u64 = 4 * 1024 * 1024;

// PoC mode: Write Gate (`cargo run -- prepare-test <block_path>`). Exercises
// the full pre-write gate (Selection -> final re-verification -> WritePlan ->
// OpenDevice -> FD binding -> confirmation -> PreparedWrite) end to end.
// `writer::write()` is never called and is not reachable from this function
// — establishing a `PreparedWrite` here proves the gate passed, nothing more.
// If OpenDevice needs polkit authentication, this program does nothing but
// wait for the reply; it never falls back to sudo or any other bypass.
fn run_prepare_test(block_path: String) -> zbus::Result<()> {
    let mut state = attempt_select(&block_path);

    if let core::SelectionState::Selected { baseline, .. } = &state {
        let outcome = collect_device_snapshot(&baseline.block_path);
        state = core::revalidate(state, outcome);
    }

    print_selection_state(&state);

    if !core::is_ready_to_open(&state) {
        println!("\nprepare-test: Selection invalid -- stopping before the Write Gate.");
        return Ok(());
    }

    let core::SelectionState::Selected { baseline, .. } = &state else {
        unreachable!("is_ready_to_open just confirmed Selected");
    };

    // `image` is this PoC's stand-in for an explicit image selection (the
    // currently-nonexistent GUI would call `ImageSelection::new()` once,
    // when the user picks a file). The same `ImageSelection` value is reused
    // for both the confirmation and the later Gate call below, since this
    // PoC never re-selects a different image mid-run.
    let image = core::ImageSelection::new(PREPARE_TEST_IMAGE_SIZE);

    // This PoC never implements Verify itself (see CLAUDE.md/reports) --
    // `VerifyMode::None` here only means "no verify policy was chosen yet",
    // not that a future GUI must default to it.
    let verify_mode = core::VerifyMode::None;

    // `WriteIntent::from_selection` pulls `baseline` and `selection_generation`
    // out of the same `state` value -- there is no way to build one from a
    // mismatched pairing of the two. This is what the — currently
    // nonexistent — GUI would call once, at the moment the user confirms.
    let intent = match core::WriteIntent::from_selection(&state, image, verify_mode) {
        Ok(intent) => intent,
        Err(error) => {
            println!("prepare-test: WriteIntent construction rejected: {error:?}");
            return Ok(());
        }
    };
    let confirmation = core::ConfirmationToken::confirm(intent);
    println!(
        "\nprepare-test: confirmation created for {} (image_size={} bytes, verify_mode={:?})",
        confirmation.intent().target_block_path(),
        confirmation.intent().image_size(),
        confirmation.intent().verify_mode()
    );

    let refreshed_for_gate = collect_device_snapshot(&baseline.block_path);

    let ready = match core::prepare_for_open(
        &state,
        refreshed_for_gate,
        image,
        verify_mode,
        Some(&confirmation),
    ) {
        Ok(ready) => ready,
        Err(error) => {
            println!("prepare-test: Write Gate rejected before OpenDevice: {error:?}");
            return Ok(());
        }
    };

    println!(
        "prepare-test: Write Gate (pre-open) passed. WritePlan: image_size={} target_size={} chunk_size={}",
        ready.plan().image_size,
        ready.plan().target_size,
        ready.plan().chunk_size
    );

    println!(
        "prepare-test: requesting OpenDevice(mode=\"rw\") on {}.",
        ready.current().block_path
    );
    println!(
        "If a polkit authentication prompt appears, please complete it yourself -- \
         this program will not use sudo or any other privilege bypass."
    );

    let open_result = linux_access::open_device(
        &ready.current().block_path,
        linux_access::OpenAccess::WriteExclusive,
    );

    // Metadata must be read from the handle (a genuine, if tiny, bit of
    // Linux I/O -- fstat/ioctl) *before* the handle's ownership moves into
    // `finalize_prepared_write`, since that function must never call
    // `.metadata()` itself (core.rs stays free of Linux I/O calls; it only
    // ever receives already-collected data plus an opaque value to move).
    let (handle_opt, metadata) = match open_result {
        Ok(handle) => {
            println!("prepare-test: OpenDevice: success");
            let metadata = handle.metadata();
            (Some(handle), metadata)
        }
        Err(error) => {
            println!("prepare-test: OpenDevice: failed ({error:?})");
            (None, None)
        }
    };

    if let Some(meta) = &metadata {
        println!(
            "prepare-test: FD major:minor={}:{} size={:?} diskseq={:?}",
            meta.major, meta.minor, meta.size, meta.diskseq
        );
    }

    // `handle_opt` moves into `finalize_prepared_write` here. On success, the
    // returned `PreparedWrite` is now the sole owner of that handle -- this
    // function never sees it again. On rejection, the handle was already
    // dropped (closed via RAII) inside `finalize_prepared_write` itself, so
    // there is nothing left here to close explicitly either way.
    match core::finalize_prepared_write(ready, handle_opt, metadata.as_ref()) {
        Ok(prepared) => {
            println!(
                "prepare-test: PreparedWrite established for {} (target_size={} image_size={})",
                prepared.target_block_path, prepared.target_size, prepared.image_size
            );
            println!(
                "prepare-test: WRITE NOT PERFORMED (writer::write() is not called by this PoC)"
            );

            // Demonstrate the next ownership stage: PreparedWrite -> AuthorizedWrite.
            // `begin()` consumes `prepared` by value -- the fd moves once,
            // with no dup()/try_clone(), into the new AuthorizedWrite, which
            // also now bundles the exact WritePlan/VerifyMode the Gate
            // verified. `prepared` cannot be referred to again after this
            // line; the compiler enforces that, not a runtime check. Fields
            // are captured beforehand since AuthorizedWrite exposes none of
            // them publicly -- only `write_job::start()` (via the
            // crate-private `into_parts()`) is meant to take it apart.
            let target_block_path = prepared.target_block_path.clone();
            let target_size = prepared.target_size;
            let image_size = prepared.image_size;

            let authorized = prepared.begin();
            println!(
                "prepare-test: WRITE SESSION AUTHORIZED for {target_block_path} (target_size={target_size} image_size={image_size} verify_mode={verify_mode:?})"
            );
            println!(
                "prepare-test: WRITE NOT PERFORMED (AuthorizedWrite is not connected to write_job::start() by this PoC)"
            );

            drop(authorized);
            println!("prepare-test: AuthorizedWrite dropped -- FD closed via RAII");
        }
        Err(error) => {
            println!("prepare-test: Write Gate rejected after OpenDevice: {error:?}");
            println!(
                "prepare-test: FD (if any was opened) was already closed via RAII inside the Write Gate"
            );
        }
    }

    Ok(())
}

// Simple, dependency-free human-readable size formatting for the Pre-write
// Safety Summary in `write-test` (`CliObserver`) -- not a general-purpose
// formatting utility, just enough to show e.g. "8000000000 bytes
// (7.45 GiB)" without adding a crate for it.
fn format_size(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = MIB * 1024.0;

    let bytes_f = bytes as f64;

    if bytes_f >= GIB {
        format!("{bytes} bytes ({:.2} GiB)", bytes_f / GIB)
    } else if bytes_f >= MIB {
        format!("{bytes} bytes ({:.2} MiB)", bytes_f / MIB)
    } else {
        format!("{bytes} bytes")
    }
}

// How often `wait_for_prompt_input` re-checks for cancellation while no
// input has arrived yet. Bounds how long a Ctrl+C during the Human
// Confirmation prompt can go unnoticed; small enough to feel immediate,
// large enough that the idle wait costs nothing measurable.
const PROMPT_CANCEL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

// The result of waiting for one line of prompt input while also watching for
// cancellation. `Line`/`Eof`/`Error` are exactly what a plain
// `stdin().read_line()` could report (`Ok(n > 0)`/`Ok(0)`/`Err`); `Cancelled`
// is the one outcome a blocking `read_line()` on the main thread could never
// produce here -- `ctrlc` installs its SIGINT handler with `SA_RESTART`, so
// the kernel silently restarts an in-progress `read()` after Ctrl+C instead
// of interrupting it (see reports/latest.md's D3 technical verification).
#[derive(Debug)]
enum PromptInput {
    Line(String),
    Eof,
    Error(std::io::Error),
    Cancelled,
}

// Starts a dedicated thread that performs exactly one blocking
// `stdin().read_line()` and sends its result back over the returned
// channel. The thread never observes cancellation itself -- it is simply
// left blocked in `read_line()` if the caller stops waiting (see
// `wait_for_prompt_input`); on that path the caller returns
// `WriteTestExit::Cancelled`, and `main()`'s top-level `std::process::exit`
// then ends the process, reader thread included. Stdin is never read again
// by anything else after a cancellation, so the abandoned thread's stdin
// lock can never block other code. A thread-spawn failure is returned to
// the caller rather than panicking.
fn spawn_prompt_reader() -> std::io::Result<std::sync::mpsc::Receiver<PromptInput>> {
    let (sender, receiver) = std::sync::mpsc::channel();

    std::thread::Builder::new()
        .name("prompt-reader".into())
        .spawn(move || {
            let mut line = String::new();
            let input = match std::io::stdin().read_line(&mut line) {
                Ok(0) => PromptInput::Eof,
                Ok(_) => PromptInput::Line(line),
                Err(error) => PromptInput::Error(error),
            };
            // The receiver may already be gone (the caller stopped waiting
            // because of a cancellation); nothing more to do in that case.
            let _ = sender.send(input);
        })?;

    Ok(receiver)
}

// Waits for the prompt reader's result while checking `is_cancelled` before
// every wait and after every received input. Cancellation always wins: an
// input that arrives at (nearly) the same moment as a Ctrl+C is discarded in
// favor of `Cancelled`, so a correctly typed confirmation can never carry a
// run past a cancellation that was already requested. Takes the receiver
// and the cancellation check as parameters (not `stdin`/`CancelHandle`
// directly) so every branch is unit-testable without a terminal. A
// disconnected channel without a result (the reader thread died without
// sending) is reported as `Error`, never as a confirmation.
fn wait_for_prompt_input(
    receiver: &std::sync::mpsc::Receiver<PromptInput>,
    is_cancelled: impl Fn() -> bool,
    poll_interval: std::time::Duration,
) -> PromptInput {
    loop {
        if is_cancelled() {
            return PromptInput::Cancelled;
        }

        match receiver.recv_timeout(poll_interval) {
            Ok(input) => {
                if is_cancelled() {
                    return PromptInput::Cancelled;
                }
                return input;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if is_cancelled() {
                    return PromptInput::Cancelled;
                }
                return PromptInput::Error(std::io::Error::other(
                    "prompt reader ended without reporting a result",
                ));
            }
        }
    }
}

// How `run_write_test` finished, for `main()` to turn into a process exit
// code -- see `write_test_exit_code` below. Deliberately not richer than
// this (no byte counts, no phase): every message a human needs has already
// been printed by `run_write_test` itself by the time this is returned;
// this exists only to answer the one question `main()` still needs
// answered afterward -- "did the user cancel this run?"
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteTestExit {
    Completed,
    Cancelled,
}

// Pure: no I/O, no `std::process::exit` -- `main()` is the only place that
// actually calls `std::process::exit`, and only after `run_write_test` has
// already returned normally (see the `write-test` dispatch in `main()`),
// so every FD/state cleanup `run_write_test` triggers via ordinary Rust
// `Drop` has already happened by the time this decision is acted on. `None`
// means "let `main` return `Ok(())` and exit 0 the ordinary way"; `Some`
// carries the exit code `main` should pass to `std::process::exit` instead.
// 130 (128 + SIGINT) is the conventional Unix exit code for a
// signal-interrupted process, distinguishing a deliberate user cancellation
// from both success (0) and a genuine error (1, `zbus::Result`'s own
// `Err` path).
fn write_test_exit_code(exit: WriteTestExit) -> Option<i32> {
    match exit {
        WriteTestExit::Completed => None,
        WriteTestExit::Cancelled => Some(130),
    }
}

// Installs the process-wide Ctrl+C (SIGINT) handler that lets a user
// actually cancel an in-progress `write-test` image preparation, Human
// Confirmation prompt, write, or Verify -- the
// missing "last mile" identified by the v0.1 audit: `write_job::CancelHandle`
// itself, and every write/Verify loop's use of it, already existed and were
// already unit-tested; nothing anywhere in this crate could ever trigger
// `request_cancel()` from a real user action until this function existed.
//
// The handler does exactly one thing: `cancel.request_cancel(UserRequested)`.
// Nothing else runs inside it -- no `println!`/`eprintln!`, no allocation
// beyond what capturing `cancel` itself already required, no D-Bus call, no
// file or USB I/O, no `std::process::exit`, no panic, no lock, no shell
// command, no sleep. `ctrlc` (unlike a hand-rolled `libc`/`nix` `sigaction`)
// runs this closure outside the raw OS signal context (on its own internal
// dispatch thread), which is what makes it safe to call ordinary, non-
// `async-signal-safe` Rust code such as `request_cancel()` (an `AtomicU8`
// store) here at all -- see reports/latest.md's "Cancel機能 Ctrl+C実配線 設計"
// for the fuller comparison against `signal-hook`/raw `libc` that led to
// this choice.
//
// Deliberately a small, named function rather than an inline closure at the
// call site: this is the one and only place in this crate that touches
// `ctrlc` at all, so isolating it here keeps that fact easy to audit.
//
// Because the handler only ever sets a flag, it cannot by itself end a
// blocking read: `ctrlc` registers with `SA_RESTART`, so a `read_line()`
// in progress on the main thread simply resumes after Ctrl+C. The Human
// Confirmation prompt therefore reads stdin on a separate thread and polls
// this flag instead (`spawn_prompt_reader`/`wait_for_prompt_input`), which
// keeps this closure a one-liner and keeps `std::process::exit` confined to
// `main()`. The `--test-pause-before-verify` prompt deliberately still
// blocks on the main thread (known limitation of that test-only mode).
fn install_cancel_handler(cancel: write_job::CancelHandle) -> Result<(), ctrlc::Error> {
    ctrlc::set_handler(move || {
        cancel.request_cancel(write_job::CancelReason::UserRequested);
    })
}

// TEST-ONLY diagnostic pause, enabled only by the explicit
// `--test-pause-before-verify` CLI flag (see the `write-test` dispatch in
// `main()`). Exists solely to let a real-device test manually mount a child
// partition of the target device -- in another terminal -- between write+sync
// completing and Verify fetching its fresh `DeviceSnapshot`, so the
// mount_points-allowance branch `core::verify_target_check_from_diagnostics`
// implements can actually be exercised on real hardware (see
// reports/latest.md's Mount-Allowance Real-device Test design). This is not
// a Safety bypass of any kind: every existing check (fresh snapshot fetch,
// `check_target()`'s Identity/Instance/hazard re-verification,
// `OpenDevice(mode="r", O_DIRECT)`, FD binding) still runs in full
// afterward, unchanged -- this function only delays when that sequence
// starts. It never touches the target device itself (no `udisksctl`,
// `mount`, or any other command is spawned here) -- mounting is entirely
// the user's own action in their own terminal.
//
// Blocks on `stdin`. Returns `true` only if a line was actually read (the
// user pressed Enter); `false` on EOF or an I/O error, mirroring the
// existing Human Confirmation prompt's own `Ok(0) => false` / `Err(_) =>
// false` treatment (`CliObserver::request_confirmation`) -- the operation
// never proceeds to Verify on `false`.
fn pause_before_verify_for_test() -> bool {
    println!("write-test: TEST PAUSE (--test-pause-before-verify, test-only)");
    println!("write-test: write + sync are complete.");
    println!("write-test: the write-mode device handle is already closed.");
    println!("write-test: mount only a CHILD PARTITION of the target device in another terminal.");
    println!("write-test: do NOT mount the whole-disk device.");
    println!("write-test:   e.g. udisksctl mount -b /dev/<partition>");
    println!("write-test: after mounting, return here and press Enter to continue.");
    println!(
        "write-test: Verify will then obtain a fresh device snapshot and re-check identity, instance, and hazards."
    );
    print!("> ");
    let _ = std::io::stdout().flush();

    let mut discard = String::new();
    match std::io::stdin().read_line(&mut discard) {
        Ok(0) => false, // EOF: no input was given, never treat this as an implicit "continue".
        Ok(_) => true,
        Err(_) => false,
    }
}

// `cargo run -- write-test <image-path> <block_path> [verify-mode]`: the
// production write path, run through `orchestration::operation::
// run_write_operation`, which owns the whole safe sequence (select ->
// re-verify -> open the image once -> Preflight + target re-check for gzip /
// xz -> typed confirmation -> WriteIntent / ConfirmationToken -> fresh Write
// Gate -> OpenDevice(rw, O_EXCL) -> FD binding -> AuthorizedExecution ->
// write -> sync -> Verify). None of Selection/Identity/Instance/Safety/
// Confirmation/FD-binding is skipped, and nothing here can reorder it: this
// function only
//
//   - wires Ctrl+C to the operation's `CancelHandle` (before anything else,
//     so every existing cancel point of the operation is reachable),
//   - prints what the operation reports and asks for the typed
//     confirmation (`CliObserver`),
//   - prints how it ended and turns that into the exit status
//     (`print_write_test_outcome`, `write_test_exit`).
//
// `block_path` is a UDisks2 block object path, exactly like every other CLI
// mode above (`select`/`open-test`/`prepare-test`) -- not a raw `/dev/sdX`
// string accepted with no safety checks; it is passed as an unverified
// block-path reference. If OpenDevice needs polkit authentication, this
// program does nothing but wait for the reply; it never falls back to sudo
// or any other bypass.
fn run_write_test(
    image_path: String,
    block_path: String,
    verify_mode: core::VerifyMode,
    test_pause_before_verify: bool,
) -> zbus::Result<WriteTestExit> {
    // Cancel wiring (Ctrl+C -> CancelHandle): one shared handle, wired to
    // Ctrl+C before the operation starts. The operation checks it only at
    // its existing cancel points (during a compressed image's Preflight,
    // after the image is prepared, during the confirmation prompt, per write
    // chunk, after sync, before Verify, per Verify chunk). It is one-shot:
    // there is no way to reset it, so a cancelled run can never continue.
    let cancel = write_job::CancelHandle::new();

    if let Err(error) = install_cancel_handler(cancel.clone()) {
        println!("write-test: failed to install the Ctrl+C handler: {error:?}");
        println!(
            "write-test: refusing to start a destructive write without a working cancel path."
        );
        return Ok(WriteTestExit::Completed);
    }

    let request = orchestration::operation::WriteOperationRequest::new(
        orchestration::candidates::TargetRef::from_block_path(block_path.as_str()),
        image_path.clone(),
        verify_mode,
    );
    let mut observer = CliObserver {
        image_path: &image_path,
        cancel: cancel.clone(),
        test_pause_before_verify,
        write_cancel_notice_shown: false,
        verify_cancel_notice_shown: false,
        last_verify_total_bytes: 0,
        expected_device: String::new(),
    };

    let outcome = orchestration::operation::run_write_operation(request, &cancel, &mut observer);

    print_write_test_outcome(&outcome, &observer, &block_path);
    Ok(write_test_exit(&outcome))
}

// The exit status `write-test` has always used: 130 (`Cancelled`) for a
// cancellation at any point, and `Completed` for everything else --
// including a refusal or a failure, which only the printed messages report
// (unchanged in Phase 3A).
fn write_test_exit(outcome: &orchestration::outcome::OperationOutcome) -> WriteTestExit {
    if outcome.is_cancelled() {
        WriteTestExit::Cancelled
    } else {
        WriteTestExit::Completed
    }
}

// The CLI side of a write operation: prints every step exactly as
// `write-test` always has, and asks for the typed confirmation on stdin.
struct CliObserver<'a> {
    image_path: &'a str,
    cancel: write_job::CancelHandle,
    test_pause_before_verify: bool,
    // The progress callbacks are a convenient place to tell the user their
    // Ctrl+C was seen, the *first* time it's observed -- but only a
    // best-effort, early notice: it is never what decides whether the write
    // or Verify actually stopped (that is the operation's outcome).
    write_cancel_notice_shown: bool,
    verify_cancel_notice_shown: bool,
    // The last Verify `total_bytes` shown, so a Verify cancellation can be
    // reported as "X of Y" (Quick's Y is its sampled total, not the image
    // size).
    last_verify_total_bytes: u64,
    // The `/dev` node the confirmation asked for.
    expected_device: String,
}

impl orchestration::events::OperationObserver for CliObserver<'_> {
    fn on_event(&mut self, event: orchestration::events::OperationEvent<'_>) {
        use orchestration::events::{OpenPurpose, OperationEvent};

        match event {
            OperationEvent::TargetSelected { state } => print_selection_state(state),
            OperationEvent::CompressedImageDetected { format } => println!(
                "write-test: detected format: {} (compressed image); validating it before writing (nothing is written yet)",
                format.name()
            ),
            OperationEvent::PreflightProgress(progress) => {
                println!("write-test: {}", format_preflight_progress(&progress))
            }
            OperationEvent::ImageSelected { image_size } => println!(
                "\nwrite-test: image selected from {} (image_size={image_size} bytes)",
                self.image_path
            ),
            OperationEvent::Confirmed(confirmed) => {
                println!(
                    "write-test: confirmation accepted for {}",
                    self.expected_device
                );
                println!(
                    "write-test: confirmation created for {} (image_size={} bytes, verify_mode={:?})",
                    confirmed.target_block_path, confirmed.image_size, confirmed.verify_mode
                );
            }
            OperationEvent::WriteGatePassed { plan } => println!(
                "write-test: Write Gate (pre-open) passed. WritePlan: image_size={} target_size={} chunk_size={}",
                plan.image_size, plan.target_size, plan.chunk_size
            ),
            OperationEvent::OpeningDevice {
                purpose,
                block_path,
            } => {
                match purpose {
                    OpenPurpose::Write => {
                        println!("write-test: requesting OpenDevice(mode=\"rw\") on {block_path}.")
                    }
                    OpenPurpose::Verify => println!(
                        "write-test: requesting OpenDevice(mode=\"r\", O_DIRECT) on {block_path} for verification."
                    ),
                }
                println!(
                    "If a polkit authentication prompt appears, please complete it yourself -- \
                     this program will not use sudo or any other privilege bypass."
                );
            }
            OperationEvent::DeviceOpened {
                purpose: OpenPurpose::Write,
                metadata,
            } => {
                println!("write-test: OpenDevice: success");
                if let Some(meta) = metadata {
                    println!(
                        "write-test: FD major:minor={}:{} size={:?} diskseq={:?}",
                        meta.major, meta.minor, meta.size, meta.diskseq
                    );
                }
            }
            OperationEvent::DeviceOpened {
                purpose: OpenPurpose::Verify,
                metadata,
            } => {
                println!("write-test: OpenDevice(mode=\"r\", O_DIRECT): success");
                if let Some(meta) = metadata {
                    println!(
                        "write-test: verify FD major:minor={}:{} size={:?} diskseq={:?}",
                        meta.major, meta.minor, meta.size, meta.diskseq
                    );
                }
            }
            OperationEvent::DeviceOpenFailed { purpose, error } => match purpose {
                OpenPurpose::Write => println!("write-test: OpenDevice: failed ({error:?})"),
                OpenPurpose::Verify => {
                    println!("write-test: OpenDevice(mode=\"r\", O_DIRECT): failed ({error:?})")
                }
            },
            // Reaching `FdBound` already proves `check_fd_binding` (core.rs)
            // returned `FdBindingCheck::Match`.
            OperationEvent::FdBound { purpose } => match purpose {
                OpenPurpose::Write => println!("write-test: FD binding: Match"),
                OpenPurpose::Verify => println!("write-test: verify FD binding: Match"),
            },
            OperationEvent::WriteAuthorized {
                target_block_path,
                target_size,
                image_size,
                verify_mode,
            } => {
                println!(
                    "write-test: PreparedWrite established for {target_block_path} (target_size={target_size} image_size={image_size})"
                );
                println!("write-test: WRITE SESSION AUTHORIZED (verify_mode={verify_mode:?})");
            }
            OperationEvent::ImageBound => println!(
                "write-test: AuthorizedExecution bound (image_generation/image_size match confirmed)"
            ),
            OperationEvent::WriteStarted => println!("write-test: write started"),
            OperationEvent::WriteProgress(progress) => {
                if self.cancel.is_requested() && !self.write_cancel_notice_shown {
                    self.write_cancel_notice_shown = true;
                    println!(
                        "write-test: cancellation requested -- waiting for the current operation to stop safely."
                    );
                }
                let percent = if progress.total_bytes > 0 {
                    (progress.bytes_written as f64 / progress.total_bytes as f64) * 100.0
                } else {
                    100.0
                };
                println!(
                    "write-test: progress {}/{} bytes ({percent:.1}%)",
                    progress.bytes_written, progress.total_bytes
                );
            }
            OperationEvent::WriteSucceeded {
                bytes_written,
                image_size,
            } => println!(
                "write-test: write succeeded ({bytes_written} of {image_size} bytes written)"
            ),
            OperationEvent::CancelDrainStarted { bytes_written } => println!(
                "write-test: cancelled after {bytes_written} bytes -- writing back pending data before closing the device..."
            ),
            OperationEvent::CancelDrainOnCallingThread { error } => println!(
                "write-test: could not start the drain worker thread ({error}); draining on the main thread instead"
            ),
            OperationEvent::SyncStarted => println!("write-test: syncing..."),
            OperationEvent::SyncOnCallingThread { error } => println!(
                "write-test: could not start the sync worker thread ({error}); syncing on the main thread instead"
            ),
            OperationEvent::SyncSucceeded { bytes_written } => println!(
                "write-test: sync succeeded -- write + sync completed ({bytes_written} bytes)"
            ),
            OperationEvent::VerifyPending { mode } => match mode {
                core::VerifyMode::Quick => println!(
                    "write-test: Quick verification checks selected regions only. It does not verify the entire image."
                ),
                core::VerifyMode::Full => {
                    println!("write-test: Full verification reads back the entire written image.")
                }
                core::VerifyMode::None => unreachable!(
                    "VerifyMode::None always produces VerifyStart::Skipped, never Pending"
                ),
            },
            OperationEvent::VerifySnapshotRequested { block_path } => println!(
                "write-test: requesting a fresh DeviceSnapshot for verification on {block_path}."
            ),
            // Verify Pre-flight Diagnostics: the same five-line summary the
            // rejection path shows -- "Simple by default": always shown,
            // never a full `DeviceSnapshot` dump.
            OperationEvent::VerifyTargetChecked { diagnostics } => {
                println!("write-test: verify target re-check: OK");
                for line in format_verify_diagnostics_summary(diagnostics) {
                    println!("write-test:   {line}");
                }
            }
            OperationEvent::VerifyStarted => println!("write-test: verification started"),
            OperationEvent::VerifyProgress(progress) => {
                self.last_verify_total_bytes = progress.total_bytes;
                if self.cancel.is_requested() && !self.verify_cancel_notice_shown {
                    self.verify_cancel_notice_shown = true;
                    println!(
                        "write-test: cancellation requested -- waiting for the current operation to stop safely."
                    );
                }
                let percent = if progress.total_bytes > 0 {
                    (progress.verified_bytes as f64 / progress.total_bytes as f64) * 100.0
                } else {
                    100.0
                };
                println!(
                    "write-test: verify progress {}/{} bytes ({percent:.1}%)",
                    progress.verified_bytes, progress.total_bytes
                );
            }
        }
    }

    // ---- Pre-write Safety Summary / Destructive Warning / Human Confirmation ----
    // Shows the re-verified selection and image the confirmation will be
    // bound to, then reads one line. The operation compares it.
    fn request_confirmation(
        &mut self,
        request: &orchestration::operation::ConfirmationRequest<'_>,
    ) -> orchestration::events::ConfirmationDecision {
        use orchestration::events::ConfirmationDecision;

        println!("\nwrite-test: Pre-write Safety Summary");
        println!("Target:");
        println!("  Device:      {}", request.target.device);
        println!(
            "  Model:       {} {}",
            request.target.vendor, request.target.model
        );
        println!("  Serial:      {}", request.target.serial);
        println!("  Size:        {}", format_size(request.target.size));
        println!("  Bus:         {}", request.target.connection_bus);
        println!("  Removable:   {}", request.target.removable);
        if request.target.mount_points.is_empty() {
            println!("  Mounts:      none");
        } else {
            println!("  Mounts:      {}", request.target.mount_points.join(", "));
        }
        println!("  Risk:        {:?}", request.assessment.risk_level);
        println!("  Writable:    {}", request.assessment.writable);
        println!("  Reasons:     {:?}", request.assessment.reasons);
        println!("  Block path:  {}", request.block_path);
        println!("  DiskSeq:     {:?}", request.diskseq);
        println!("Image:");
        println!("  Path:        {}", self.image_path);
        println!("  Size:        {}", format_size(request.image_size));
        println!("Verification mode: {:?}", request.verify_mode);
        println!();
        println!("WARNING: Writing will overwrite the target device.");
        println!("ALL EXISTING DATA ON THIS DEVICE MAY BE DESTROYED. This cannot be undone.");
        println!();

        self.expected_device = request.expected_text.to_string();
        println!(
            "Type the target device name exactly to continue: {}",
            self.expected_device
        );
        print!("> ");
        let _ = std::io::stdout().flush();

        // stdin is read on a separate thread so a Ctrl+C here is noticed
        // within `PROMPT_CANCEL_POLL_INTERVAL` instead of being swallowed by
        // the `SA_RESTART`-restarted `read_line()` (see
        // `install_cancel_handler`).
        let cancel = &self.cancel;
        let prompt_input = match spawn_prompt_reader() {
            Ok(receiver) => wait_for_prompt_input(
                &receiver,
                || cancel.is_requested(),
                PROMPT_CANCEL_POLL_INTERVAL,
            ),
            Err(error) => PromptInput::Error(error),
        };

        match prompt_input {
            PromptInput::Line(input) => ConfirmationDecision::Submitted(input),
            // EOF (e.g. stdin closed or redirected from an empty source):
            // "no answer given", never an implicit yes.
            PromptInput::Eof => ConfirmationDecision::InputClosed,
            PromptInput::Error(error) => ConfirmationDecision::InputFailed(error),
            PromptInput::Cancelled => ConfirmationDecision::Cancelled,
        }
    }

    // TEST-ONLY (`--test-pause-before-verify`): see
    // `pause_before_verify_for_test`. The write FD is already closed here,
    // and Verify's fresh snapshot, re-check, OpenDevice and FD binding all
    // still run afterward.
    fn pause_before_verify(&mut self) -> bool {
        !self.test_pause_before_verify || pause_before_verify_for_test()
    }
}

// How a `write-test` run ended, printed exactly as it always has been.
fn print_write_test_outcome(
    outcome: &orchestration::outcome::OperationOutcome,
    observer: &CliObserver<'_>,
    block_path: &str,
) {
    use orchestration::operation::{ConfirmError, PrepareImageError, TargetNotReady};
    use orchestration::outcome::{CancelledAt, OperationError, OperationOutcome, VerifyNotStarted};

    let identity_preserved = |image_size: u64| {
        println!("write-test: SelectedImage identity preserved (image_size={image_size})")
    };
    let identity_preserved_after_verification = |image_size: u64| {
        println!(
            "write-test: SelectedImage identity preserved after verification (image_size={image_size})"
        )
    };
    let confirmation_failed = || {
        println!("write-test: confirmation failed; no device was opened and nothing was written")
    };
    let print_lines = |lines: Vec<String>| {
        for line in lines {
            println!("write-test: {line}");
        }
    };

    match outcome {
        OperationOutcome::Completed { verify, image_size } => {
            println!("write-test: {}", format_verify_succeeded(verify));
            identity_preserved_after_verification(*image_size);
        }

        OperationOutcome::Cancelled(at) => match at {
            CancelledAt::Preflight | CancelledAt::BeforeConfirmation => {
                print_lines(format_cancelled_before_confirmation())
            }
            CancelledAt::Confirmation => {
                println!();
                print_lines(format_cancelled_before_confirmation());
            }
            CancelledAt::Write {
                cancelled,
                image_size,
            } => {
                print_lines(format_write_cancelled(cancelled));
                println!(
                    "write-test: not syncing -- retry_requires_fresh_gate={}",
                    cancelled.retry_requires_fresh_gate
                );
                identity_preserved(*image_size);
            }
            CancelledAt::AfterSync => print_lines(format_cancelled_after_sync()),
            CancelledAt::BeforeVerify => print_lines(format_verify_cancelled_before_start()),
            CancelledAt::Verify {
                cancelled,
                image_size,
            } => {
                print_lines(format_verify_cancelled(
                    cancelled,
                    observer.last_verify_total_bytes,
                ));
                identity_preserved_after_verification(*image_size);
            }
        },

        OperationOutcome::Failed(error) => match error {
            OperationError::Target(not_ready) => {
                match not_ready {
                    TargetNotReady::Select(error) => {
                        print_select_error(error, block_path);
                        print_selection_state(&core::SelectionState::NoSelection);
                    }
                    TargetNotReady::NotReady(state) => print_selection_state(state),
                }
                println!("\nwrite-test: Selection invalid -- stopping before the Write Gate.");
            }
            OperationError::Image(PrepareImageError::CompressedImage(rejection)) => {
                print_lines(format_compressed_image_rejected(rejection))
            }
            OperationError::Image(PrepareImageError::TargetChanged(state)) => {
                println!("write-test: the target changed while the image was being validated:");
                print_selection_state(state);
                println!("write-test: stopping before confirmation; nothing was written.");
            }
            OperationError::Image(PrepareImageError::Image(
                error @ (image_source::ImageSourceError::UnsupportedFormat(_)
                | image_source::ImageSourceError::ExtensionMismatch { .. }),
            )) => print_lines(format_image_format_rejected(error)),
            OperationError::Image(PrepareImageError::Image(error)) => println!(
                "write-test: failed to open image {}: {error:?}",
                observer.image_path
            ),
            OperationError::Confirmation(ConfirmError::Mismatch)
            | OperationError::ConfirmationInputClosed => confirmation_failed(),
            OperationError::Confirmation(ConfirmError::Intent(error)) => {
                println!(
                    "write-test: confirmation accepted for {}",
                    observer.expected_device
                );
                println!("write-test: WriteIntent construction rejected: {error:?}");
            }
            OperationError::ConfirmationInputFailed(error) => {
                println!("write-test: failed to read the confirmation input: {error}");
                confirmation_failed();
            }
            OperationError::WriteGate(error) => {
                println!("write-test: Write Gate rejected before OpenDevice: {error:?}")
            }
            OperationError::WriteDeviceRejected { error, .. } => {
                println!("write-test: Write Gate rejected after OpenDevice: {error:?}");
                println!(
                    "write-test: FD (if any was opened) was already closed via RAII inside the Write Gate"
                );
            }
            OperationError::ImageBinding(error) => {
                println!("write-test: AuthorizedExecution::bind() rejected: {error:?}");
                println!("write-test: AuthorizedWrite dropped -- FD closed via RAII");
            }
            OperationError::ReaderOpen(error) => {
                println!("write-test: begin_write() failed to open the image reader: {error}");
                println!(
                    "write-test: AuthorizedExecution dropped -- FD closed via RAII, 0 bytes written"
                );
            }
            OperationError::Write { failed, image_size } => {
                println!("write-test: write FAILED: {failed:?}");
                println!(
                    "write-test: not syncing -- retry_requires_fresh_gate={}",
                    failed.retry_requires_fresh_gate
                );
                identity_preserved(*image_size);
            }
            // Cancelled, but the pending data could not be written back: a
            // failure, not a clean cancellation.
            OperationError::CancelDrain { failed, image_size } => {
                println!("write-test: cancelled, but writing back pending data FAILED: {failed:?}");
                println!(
                    "write-test: the target may be partially overwritten -- retry_requires_fresh_gate={}",
                    failed.retry_requires_fresh_gate
                );
                identity_preserved(*image_size);
            }
            OperationError::CancelDrainWorkerPanicked => println!(
                "write-test: cancelled, but the drain worker panicked -- writing back pending data is not confirmed; the target may be partially overwritten"
            ),
            OperationError::SyncWorkerPanicked { cancel_requested } => {
                print_lines(format_sync_worker_panicked());
                if *cancel_requested {
                    println!(
                        "write-test: cancellation was also requested; the sync failure above is the result"
                    );
                }
            }
            // A real sync failure is never masked by a cancellation
            // requested at the same time: the result stays the failure;
            // this only notes it.
            OperationError::Sync {
                failed,
                cancel_requested,
                image_size,
            } => {
                println!("write-test: sync FAILED: {failed:?}");
                println!(
                    "write-test: retry_requires_fresh_gate={} -- not retrying automatically",
                    failed.retry_requires_fresh_gate
                );
                if *cancel_requested {
                    println!(
                        "write-test: cancellation was also requested; the sync failure above is the result"
                    );
                }
                identity_preserved(*image_size);
            }
            OperationError::VerifyNotStarted(VerifyNotStarted::TestPauseEnded) => {
                println!(
                    "write-test: no input received on the test pause (EOF or I/O error) -- stopping before verification."
                );
                println!(
                    "write-test: write + sync already completed successfully; verification was not attempted."
                );
            }
            // Verify Pre-flight Diagnostics: the re-check's rejection carries
            // the diagnostics that produced it whenever one could be
            // computed -- only `SnapshotRefreshFailed` has none. Write and
            // sync already succeeded; the wording keeps that explicit.
            OperationError::VerifyNotStarted(VerifyNotStarted::TargetCheck {
                error,
                diagnostics,
                image_size,
            }) => {
                println!("write-test: write + sync completed successfully.");
                println!("write-test: verification could not start: {error:?}");
                match diagnostics {
                    Some(diagnostics) => {
                        println!("write-test: verify target re-check: FAILED");
                        for line in format_verify_diagnostics_summary(diagnostics) {
                            println!("write-test:   {line}");
                        }
                    }
                    None => {
                        println!("write-test: verify target re-check: unavailable");
                        for line in format_verify_diagnostics_unavailable() {
                            println!("write-test:   {line}");
                        }
                    }
                }
                identity_preserved(*image_size);
            }
            OperationError::VerifyNotStarted(VerifyNotStarted::Start {
                error, image_size, ..
            }) => {
                println!("write-test: write + sync completed successfully.");
                println!("write-test: verification could not start: {error:?}");
                identity_preserved(*image_size);
            }
            OperationError::Verify { failed, image_size } => {
                println!("write-test: write + sync completed successfully.");
                println!(
                    "write-test: verification failed: {}",
                    format_verify_failure_reason(&failed.reason)
                );
                println!(
                    "write-test: verified_bytes={} before failure (mode={:?})",
                    failed.verified_bytes, failed.mode
                );
                identity_preserved_after_verification(*image_size);
            }
        },
    }
}

// Parses a CLI verify-mode argument (`none` | `quick` | `full`, lowercase
// only -- this PoC does not attempt case-insensitive matching). `None` means
// the argument itself did not match any known mode; the caller is
// responsible for rejecting that with a usage message and a non-zero exit,
// never for silently falling back to a default (a default is only ever
// applied when the argument is *absent* -- see the `write-test` CLI dispatch
// in `main()`).
fn parse_verify_mode(value: &str) -> Option<core::VerifyMode> {
    match value {
        "none" => Some(core::VerifyMode::None),
        "quick" => Some(core::VerifyMode::Quick),
        "full" => Some(core::VerifyMode::Full),
        _ => None,
    }
}

// Why `parse_write_test_trailing_args` (below) rejected the `write-test`
// CLI's third/fourth arguments. Kept separate from the message text itself
// (formatted at the call site in `main()`, which still has the original raw
// argument strings to include in the message) so this function stays pure
// data in, data out -- exactly like `parse_verify_mode` above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteTestArgsError {
    InvalidVerifyMode,
    UnrecognizedFourthArgument,
    TestPauseRequiresVerification,
}

// Parses `write-test`'s two optional trailing arguments (verify-mode and
// `--test-pause-before-verify`) together, since the second one's validity
// depends on the first: pure data in, data out, no I/O, no `std::process::exit`
// -- the caller (`main()`) owns all user-facing messages and the actual
// process exit, exactly the same split `parse_verify_mode` already
// established. `--test-pause-before-verify` is a TEST-ONLY diagnostic option
// (see `pause_before_verify_for_test`'s own doc comment) for manually
// exercising Verify pre-flight's mount_points-allowance branch on real
// hardware -- it is deliberately rejected outright (not silently accepted
// as a no-op) when paired with `verify-mode none`, since `VerifyMode::None`
// never produces `VerifyStart::Pending` and the flag would then have no
// observable effect at all.
fn parse_write_test_trailing_args(
    verify_mode_arg: Option<&str>,
    fourth_arg: Option<&str>,
) -> Result<(core::VerifyMode, bool), WriteTestArgsError> {
    // Absent -> `VerifyMode::None` (see `parse_verify_mode`'s own doc
    // comment for why that default was chosen). Present but unrecognized ->
    // rejected, never a silent fallback to `None` or to any other mode.
    let verify_mode = match verify_mode_arg {
        None => core::VerifyMode::None,
        Some(mode_str) => {
            parse_verify_mode(mode_str).ok_or(WriteTestArgsError::InvalidVerifyMode)?
        }
    };

    // Absent -> disabled (existing behavior, unchanged). Present and exactly
    // `--test-pause-before-verify` -> enabled. Anything else -> rejected,
    // never silently ignored.
    let test_pause_before_verify = match fourth_arg {
        None => false,
        Some("--test-pause-before-verify") => true,
        Some(_) => return Err(WriteTestArgsError::UnrecognizedFourthArgument),
    };

    if test_pause_before_verify && verify_mode == core::VerifyMode::None {
        return Err(WriteTestArgsError::TestPauseRequiresVerification);
    }

    Ok((verify_mode, test_pause_before_verify))
}

// Formats a successful Verify outcome for the CLI. Shared by both
// `VerifyStart::Skipped` (VerifyMode::None) and a completed
// `Verifying::run()` (Quick/Full) -- both ultimately produce a
// `write_job::VerifySucceeded`, so this one function is the single place
// that decides how each mode's success is worded, rather than duplicating
// the match between the two call sites.
fn format_verify_succeeded(succeeded: &write_job::VerifySucceeded) -> String {
    match succeeded.mode {
        core::VerifyMode::None => "Verification: skipped".to_string(),
        core::VerifyMode::Quick => format!(
            "Quick verification succeeded ({} bytes sampled)",
            succeeded.verified_bytes
        ),
        core::VerifyMode::Full => format!(
            "Full verification succeeded ({} bytes verified)",
            succeeded.verified_bytes
        ),
    }
}

// Formats a `VerifyFailureReason` for the CLI, distinguishing source vs.
// target for both the I/O-error and unexpected-EOF cases -- which side
// failed is genuinely different diagnostic information (see
// `write_job.rs`'s own doc comment on `VerifyFailureReason`).
fn format_verify_failure_reason(reason: &write_job::VerifyFailureReason) -> String {
    match reason {
        write_job::VerifyFailureReason::Mismatch {
            offset,
            expected,
            actual,
        } => format!(
            "mismatch at offset {offset}\n  expected: 0x{expected:02x}\n  actual:   0x{actual:02x}"
        ),
        write_job::VerifyFailureReason::SourceUnexpectedEof => {
            "the image source ended before all expected bytes could be read".to_string()
        }
        write_job::VerifyFailureReason::TargetUnexpectedEof => {
            "the target device ended before all expected bytes could be read".to_string()
        }
        write_job::VerifyFailureReason::SourceReadError(error) => {
            format!("failed to read the image source: {error}")
        }
        write_job::VerifyFailureReason::TargetReadError(error) => {
            format!("failed to read the target device: {error}")
        }
        write_job::VerifyFailureReason::UnsupportedAccess => {
            "quick verification requires an image source with random-access support, which this image does not provide".to_string()
        }
        write_job::VerifyFailureReason::SourceChanged(changed) => {
            format!("{changed}; the verification result was not accepted")
        }
    }
}

// ---------------------------------------------------------------------
// Verify Pre-flight Diagnostics CLI display (implementation step 5+6).
//
// Every helper below is a pure formatter: it takes already-computed
// `core::VerifyTargetDiagnostics` data (or one of its fields) and returns a
// `String`/`Vec<String>`, never printing anything itself. `println!` calls
// live only at the two `write-test` call sites (`CliObserver`'s success
// path and `print_write_test_outcome`'s failure path), which loop over the
// returned lines -- this keeps the
// comparison/wording logic testable without capturing stdout, and keeps
// `core.rs`/`write_job.rs` themselves free of any UI/logging dependency
// (the diagnostics data they produce is plain data; only `main.rs` decides
// how it looks on screen).
//
// "Simple by default": normal display is a five-line, diff-centric summary
// (identity/instance/mount points/read-only/hazards), never a full
// `DeviceSnapshot` field dump -- see reports/latest.md's Verify Pre-flight
// Diagnostics Design for the fuller rationale. `diskseq`/`size`/
// `connection_bus`/`removable` diffs were considered but deliberately left
// out of this step's summary to keep it exactly matching this step's
// review scope; a future "verbose" mode remains the natural place for them.
// ---------------------------------------------------------------------

// Human-readable text for an `IdentityComparison`, matching the enum's own
// vocabulary for the successful case (`Same`) but adding a short, fixed
// explanation for the two rejection cases -- deterministic wording, not a
// paraphrase that could drift between calls.
fn format_identity_comparison(identity: IdentityComparison) -> &'static str {
    match identity {
        IdentityComparison::Same => "Same",
        IdentityComparison::Changed => "Changed (different device)",
        IdentityComparison::InsufficientIdentity => "Insufficient (no usable serial to compare)",
    }
}

// Human-readable text for an `InstanceComparison`, mirroring
// `format_identity_comparison`'s approach.
fn format_instance_comparison(instance: InstanceComparison) -> &'static str {
    match instance {
        InstanceComparison::SameInstance => "SameInstance",
        InstanceComparison::Recreated => "Recreated (disconnected and reconnected)",
        InstanceComparison::InsufficientInformation => "Insufficient (no diskseq to compare)",
    }
}

// Renders a mount-point list for display: `none` for an empty list, or a
// comma-joined list of the paths themselves. Mount paths are shown as-is
// (no masking) -- this step's design deliberately does not add a masking
// mechanism (see reports/latest.md), and the mount path is exactly the
// piece of information this diagnostic summary exists to surface.
fn format_mount_points_list(mount_points: &[String]) -> String {
    if mount_points.is_empty() {
        "none".to_string()
    } else {
        mount_points.join(", ")
    }
}

// Diffs two mount-point lists for display: `unchanged (none)` /
// `unchanged (<paths>)` when the baseline and the fresh snapshot agree,
// `<before> -> <after>` otherwise.
fn format_mount_points_change(baseline: &[String], current: &[String]) -> String {
    if baseline == current {
        format!("unchanged ({})", format_mount_points_list(baseline))
    } else {
        format!(
            "{} -> {}",
            format_mount_points_list(baseline),
            format_mount_points_list(current)
        )
    }
}

// Diffs a `read_only` flag for display: `unchanged (<value>)` or
// `<before> -> <after>`, mirroring `format_mount_points_change`.
fn format_read_only_change(baseline: bool, current: bool) -> String {
    if baseline == current {
        format!("unchanged ({current})")
    } else {
        format!("{baseline} -> {current}")
    }
}

// Human-readable text for a single `core::HardHazardReason`. Wording kept
// short and lowercase, matching this CLI's existing tone (see
// `format_verify_failure_reason` above).
fn format_hard_hazard_reason(reason: core::HardHazardReason) -> &'static str {
    match reason {
        core::HardHazardReason::SystemDevice => "system device",
        core::HardHazardReason::ActiveSwap => "active swap",
        core::HardHazardReason::ComplexStorage => "complex storage",
        core::HardHazardReason::HintIgnore => "ignored by system policy",
        core::HardHazardReason::MediaUnavailable => "media unavailable",
    }
}

// Renders every hazard `core::verify_target_hard_hazards` found, in the
// same deterministic order it already returns them in: `none` when empty,
// otherwise a comma-joined list -- never just the first one, since more
// than one hazard can apply at once (see `core.rs`'s own
// `diagnostics_report_multiple_hazards_in_deterministic_order` test).
fn format_hard_hazards(hazards: &[core::HardHazardReason]) -> String {
    if hazards.is_empty() {
        "none".to_string()
    } else {
        hazards
            .iter()
            .map(|reason| format_hard_hazard_reason(*reason))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

// The single, shared diagnostic summary for both the success path (Verify
// pre-flight passed) and the failure path (Identity/Instance/hazard
// rejection) of `PendingVerify::check_target()` -- both display exactly the
// same five lines, from the same `core::VerifyTargetDiagnostics` value, so
// there is no risk of the two call sites silently drifting into showing
// different information for what is structurally the same diagnostic data.
// Returns the *content* of each line (no `write-test:` prefix, no leading
// indentation) -- the caller decides how to prefix/indent them, keeping
// this function pure formatting with no CLI-framing baked in.
fn format_verify_diagnostics_summary(diagnostics: &core::VerifyTargetDiagnostics) -> Vec<String> {
    vec![
        format!(
            "identity: {}",
            format_identity_comparison(diagnostics.identity())
        ),
        format!(
            "instance: {}",
            format_instance_comparison(diagnostics.instance())
        ),
        format!(
            "mount points: {}",
            format_mount_points_change(
                &diagnostics.baseline().mount_points,
                &diagnostics.current().mount_points
            )
        ),
        format!(
            "read-only: {}",
            format_read_only_change(
                diagnostics.baseline().read_only,
                diagnostics.current().read_only
            )
        ),
        format!("hazards: {}", format_hard_hazards(diagnostics.hazards())),
    ]
}

// The fallback shown in place of `format_verify_diagnostics_summary`'s
// output when `check_target()` returned `VerifyStartError::SnapshotRefreshFailed`
// -- there is no fresh `DeviceSnapshot` in that case, so no diagnostics
// value exists to summarize (see `PendingVerify::check_target()`'s own doc
// comment on why this is the one rejection with no diagnostics). Returns
// the line content only, matching `format_verify_diagnostics_summary`'s own
// contract, so the caller prefixes/indents it the same way.
fn format_verify_diagnostics_unavailable() -> Vec<String> {
    vec!["no fresh device snapshot was available for diagnostics".to_string()]
}

// ---------------------------------------------------------------------
// Cancel (Ctrl+C) CLI messaging (Cancel機能 Ctrl+C実配線 implementation).
// Pure formatters, matching the existing `format_verify_diagnostics_*`
// pattern: no `println!` here, only line content for the caller to prefix.
// ---------------------------------------------------------------------

// A cancelled write is never a success: it is always reported as a
// cancellation, and `verification was not attempted` is stated
// unconditionally. What it says about the target follows
// `target_may_be_modified` -- the Job layer's safety-biased field for
// exactly this question -- OR'd with `bytes_written > 0` as a defensive
// fallback, so any disagreement between the two is resolved toward
// "partial image". Only when both say nothing was written (a cancellation
// `writer::write()` observed before its first chunk) does this say so. The
// target was still opened read-write by then, so the wording is "no data was
// written", never "not opened"/"untouched".
fn format_write_cancelled(cancelled: &write_job::Cancelled) -> Vec<String> {
    let target_line = if cancelled.target_may_be_modified || cancelled.bytes_written > 0 {
        "target may contain a partial image"
    } else {
        "no data was written to the target device"
    };

    vec![
        format!(
            "write cancelled after {} of {} bytes",
            cancelled.bytes_written, cancelled.image_size
        ),
        target_line.to_string(),
        "verification was not attempted".to_string(),
    ]
}

// Shown when a cancellation is observed before the Human Confirmation step
// completed -- after the image was opened, or while waiting at the prompt.
// At that point the target has not been opened by this program at all
// (OpenDevice only happens after confirmation), which is what makes the
// second line a statement of fact rather than a hope.
fn format_cancelled_before_confirmation() -> Vec<String> {
    vec![
        "cancelled before confirmation.".to_string(),
        "the target device has not been opened or modified.".to_string(),
    ]
}

// Shown when a cancellation requested while sync was running is observed
// right after sync succeeded. Sync is never interrupted, so by then the full
// image has been written and synced; only verification is skipped. Worded so
// it cannot be read as a partial write.
fn format_cancelled_after_sync() -> Vec<String> {
    vec![
        "cancellation was requested during sync; sync was allowed to finish.".to_string(),
        "write + sync completed successfully; the full image is on the target.".to_string(),
        "verification was not started.".to_string(),
    ]
}

fn format_preflight_progress(progress: &image_source::compressed::PreflightProgress) -> String {
    format!(
        "validating compressed image: {}/{} compressed bytes read, {} bytes decoded",
        progress.compressed_consumed, progress.compressed_total, progress.logical_produced
    )
}

// Shown when a compressed image was not accepted (see
// `CompressedImageRejection`). A cancellation uses
// `format_cancelled_before_confirmation` instead.
fn format_compressed_image_rejected(rejection: &CompressedImageRejection) -> Vec<String> {
    use image_source::compressed::PreflightError;

    let reason = match rejection {
        CompressedImageRejection::QuickVerifyUnsupported(format) => format!(
            "quick verification is not available for {} images (they cannot be read at random offsets); use verify-mode full or none",
            format.name()
        ),
        CompressedImageRejection::Preflight(error) => match error {
            PreflightError::Cancelled => "validation was cancelled".to_string(),
            PreflightError::Corrupt(error) => {
                format!("the compressed image is corrupt: {error}")
            }
            PreflightError::Incomplete(error) => {
                format!("the compressed image is incomplete (truncated): {error}")
            }
            PreflightError::IntegrityCheckMissing => {
                "the compressed image has no integrity check, so its content cannot be verified"
                    .to_string()
            }
            PreflightError::UnsupportedIntegrityCheck => {
                "the compressed image uses an integrity check type that cannot be verified"
                    .to_string()
            }
            PreflightError::DecoderMemoryLimitExceeded { limit } => format!(
                "decompressing the image would need more memory than the decoder's safety limit ({limit} bytes)"
            ),
            PreflightError::DecoderFailure(error) => {
                format!("the decompressor failed (not caused by the image data): {error}")
            }
            PreflightError::LogicalSizeOverflow => {
                "the decompressed size is too large to represent".to_string()
            }
            PreflightError::LogicalSizeLimitExceeded { limit } => format!(
                "the decompressed image is larger than the target device ({limit} bytes)"
            ),
            PreflightError::InputConsumptionMismatch {
                consumed,
                compressed_size,
            } => format!(
                "the compressed stream ended after {consumed} of {compressed_size} bytes"
            ),
            PreflightError::CompressedInputBudgetExceeded { .. } => {
                "the compressed image needs too much input per byte of output (refused as a resource limit)".to_string()
            }
            PreflightError::Io(error) => format!("failed to read the compressed image: {error}"),
        },
        CompressedImageRejection::SourceChanged(changed) => changed.to_string(),
    };

    vec![
        format!("compressed image rejected: {reason}"),
        "the image was not written.".to_string(),
        "the target device has not been opened or modified.".to_string(),
    ]
}

// Shown when `open_image` refused the image because of its format:
// a recognized but unsupported compressed/archive format, or a `.gz`/`.xz`
// name whose content is not that format. Either way the file was not treated
// as a raw image. Any other `ImageSourceError` keeps the pre-existing
// "failed to open image" message at the call site.
fn format_image_format_rejected(error: &image_source::ImageSourceError) -> Vec<String> {
    let reason = match error {
        image_source::ImageSourceError::UnsupportedFormat(kind) => format!(
            "the image is {} data, which is not supported; it was not treated as a raw image.",
            kind.name()
        ),
        image_source::ImageSourceError::ExtensionMismatch { expected } => format!(
            "the file name says {} but the content is not {} data; refusing to write it as a raw image.",
            expected.name(),
            expected.name()
        ),
        other => format!("the image could not be used: {other:?}"),
    };

    vec![
        reason,
        "the target device has not been opened or modified.".to_string(),
    ]
}

// Shown when the sync worker thread panicked instead of returning a
// `SyncAttemptOutcome`. Treated like a sync failure: the write had
// completed, but sync never reported success, so durability is not
// confirmed. The write-mode FD was released when the worker unwound.
fn format_sync_worker_panicked() -> Vec<String> {
    vec![
        "sync FAILED: the sync worker thread panicked before reporting a result".to_string(),
        "the image was written, but sync did not report success; durability is not confirmed."
            .to_string(),
        "verification was not started.".to_string(),
    ]
}

// Unlike a write cancellation, a Verify cancellation never implies the
// target itself is suspect: Verify is read-only, and by the time it can run
// at all, write+sync have already succeeded -- see `format_verify_cancelled`'s
// caller for why this reads "written image remains on the target" rather
// than any wording implying the target is now in doubt. `total_bytes` is the
// last value observed from the Verify progress callback (the same
// `total_bytes` the ordinary progress lines already display), not
// `image.logical_size()` -- for `VerifyMode::Quick` those two differ, and
// this must report the same "sampled total" Quick was actually checking
// against, never the full image size.
fn format_verify_cancelled(
    cancelled: &write_job::VerifyCancelled,
    total_bytes: u64,
) -> Vec<String> {
    let mode_label = match cancelled.mode {
        core::VerifyMode::Quick => "Quick",
        core::VerifyMode::Full => "Full",
        core::VerifyMode::None => {
            unreachable!(
                "VerifyMode::None never reaches Verifying -- see SyncSucceeded::begin_verify()"
            )
        }
    };

    vec![
        format!(
            "{mode_label} verification cancelled after {} of {} bytes",
            cancelled.verified_bytes, total_bytes
        ),
        "written image remains on the target".to_string(),
    ]
}

// Shown when `cancel.is_requested()` is already `true` by the time
// `VerifyStart::Pending` is reached -- before `collect_device_snapshot()`,
// `check_target()`, or `OpenDevice(mode="r", O_DIRECT)` are ever called (see the early
// cancel check before Verify in `orchestration::operation::run_on`). Deliberately does not claim any FD/D-Bus
// state was opened-then-closed: none of it was ever opened at all.
fn format_verify_cancelled_before_start() -> Vec<String> {
    vec![
        "verification was cancelled before it could start.".to_string(),
        "write + sync already completed successfully; the target was not re-opened for verification."
            .to_string(),
    ]
}

// PoC mode: Writer self-test (`cargo run -- writer-test`). Exercises the
// Writer Core (src/writer.rs) end to end against a throwaway *regular file*
// only — never a block device. The target path is always chosen by this
// function itself (under the OS temp directory, named with this process's
// PID), never taken from a CLI argument, specifically so this mode cannot be
// pointed at /dev/* by accident or by a caller's mistake. The temporary file
// is removed before returning, whether the test passes or fails.
fn run_writer_test() -> zbus::Result<()> {
    let temp_path = std::env::temp_dir().join(format!(
        "linux-usb-writer-selftest-{}.bin",
        std::process::id()
    ));

    let result = run_writer_test_inner(&temp_path);
    let cleanup_result = std::fs::remove_file(&temp_path);

    match &result {
        Ok(()) => println!("\nwriter-test: PASSED"),
        Err(message) => println!("\nwriter-test: FAILED -- {message}"),
    }

    match cleanup_result {
        Ok(()) => println!("Temporary file removed: {}", temp_path.display()),
        Err(error) => println!(
            "Temporary file cleanup failed for {}: {error}",
            temp_path.display()
        ),
    }

    Ok(())
}

fn run_writer_test_inner(temp_path: &std::path::Path) -> Result<(), String> {
    const PATTERN_SIZE: usize = 4 * 1024 * 1024; // 4 MiB known pattern, regular file only.

    let data: Vec<u8> = (0..PATTERN_SIZE).map(|i| (i % 256) as u8).collect();

    let plan = writer::WritePlan::new(
        data.len() as u64,
        data.len() as u64,
        writer::DEFAULT_CHUNK_SIZE,
    )
    .map_err(|error| format!("plan rejected: {error:?}"))?;

    println!(
        "writer-test: temporary target file: {}",
        temp_path.display()
    );
    println!(
        "writer-test: image size = {} bytes, chunk size = {} bytes",
        data.len(),
        plan.chunk_size
    );

    let target_file = std::fs::File::create(temp_path)
        .map_err(|error| format!("failed to create temp file: {error}"))?;

    let source = std::io::Cursor::new(data.clone());

    let written = writer::write(
        &plan,
        source,
        target_file,
        |progress| {
            println!(
                "writer-test: progress {}/{} bytes",
                progress.bytes_written, progress.total_bytes
            );
        },
        || false,
    )
    .map_err(|error| format!("write failed: {error:?}"))?;

    println!("writer-test: wrote and flushed {written} bytes");

    let written_file = std::fs::File::open(temp_path)
        .map_err(|error| format!("failed to reopen temp file for read-back: {error}"))?;

    let matches = writer::verify_equal(std::io::Cursor::new(data), written_file)
        .map_err(|error| format!("read-back comparison failed: {error}"))?;

    if !matches {
        return Err("read-back data did not match the original pattern".to_string());
    }

    println!("writer-test: read-back verified byte-for-byte identical to the original pattern");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        PromptInput, WriteTestArgsError, WriteTestExit, format_cancelled_after_sync,
        format_cancelled_before_confirmation, format_hard_hazards, format_identity_comparison,
        format_image_format_rejected, format_instance_comparison, format_sync_worker_panicked,
        format_verify_cancelled, format_verify_cancelled_before_start,
        format_verify_diagnostics_summary, format_verify_diagnostics_unavailable,
        format_verify_failure_reason, format_verify_succeeded, format_write_cancelled,
        parse_verify_mode, parse_write_test_trailing_args, wait_for_prompt_input, write_test_exit,
        write_test_exit_code,
    };
    use crate::device::DeviceSnapshot;
    use crate::execution::core::{self, HardHazardReason, VerifyMode};
    use crate::execution::write_job::{
        CancelHandle, CancelReason, Cancelled, VerifyCancelled, VerifyFailureReason,
        VerifySucceeded,
    };
    use crate::identity::{IdentityComparison, InstanceComparison};
    use crate::image_source::source_identity::SourceChanged;
    use std::cell::Cell;
    use std::sync::mpsc;
    use std::time::Duration;

    // ---------------------------------------------------------------------
    // Which `OpenAccess` each OpenDevice call site uses. The call sites are
    // D-Bus calls that cannot run in a test, so their source text is checked
    // instead: every read-write FD (open-test, prepare-test, the write in
    // write-test) is `WriteExclusive` (O_EXCL), and the Verify FD is
    // `ReadOnlyDirect` (O_DIRECT, never exclusive).
    // ---------------------------------------------------------------------

    // This file's code above its test module.
    fn production_source() -> &'static str {
        let source = include_str!("main.rs");
        let end = source
            .find("\n#[cfg(test)]\nmod tests {")
            .expect("test module marker");
        &source[..end]
    }

    // The source of the top-level `fn name(...)`, up to its closing brace.
    fn production_fn_source(name: &str) -> &'static str {
        let production = production_source();
        let start = production
            .find(&format!("\nfn {name}("))
            .unwrap_or_else(|| panic!("fn {name} not found"))
            + 1;
        let length = production[start..]
            .find("\n}\n")
            .unwrap_or_else(|| panic!("end of fn {name} not found"));
        &production[start..start + length]
    }

    #[test]
    fn every_open_device_call_uses_the_intended_access() {
        let production = production_source();
        assert_eq!(
            production.matches("linux_access::open_device(").count(),
            2,
            "a new OpenDevice call site must be added to this test"
        );

        for name in ["run_open_test", "run_prepare_test"] {
            let source = production_fn_source(name);
            assert_eq!(
                source.matches("OpenAccess::WriteExclusive").count(),
                1,
                "{name}"
            );
            assert_eq!(
                source.matches("OpenAccess::ReadOnlyDirect").count(),
                0,
                "{name}"
            );
        }

        // write-test opens nothing itself: its two OpenDevice calls (the
        // exclusive write FD, then -- only for Quick / Full, after the write
        // FD is closed -- the read-only O_DIRECT Verify FD) are made by
        // `orchestration::operation` (checked by its
        // `the_production_path_runs_in_order`), through the platform, which
        // passes the requested access on unchanged.
        assert!(!production_fn_source("run_write_test").contains("OpenAccess"));
        let platform = include_str!("orchestration/platform.rs");
        assert_eq!(
            platform
                .matches("linux_access::open_device(block_path, access)")
                .count(),
            1
        );
        assert!(!platform.contains("OpenAccess::"));
    }

    // `write-test` only wires Ctrl+C, runs the operation and reports how it
    // ended; the safe sequence itself -- select, image, confirmation, Write
    // Gate, OpenDevice, FD binding, write, sync, Verify -- is
    // `orchestration::operation::run_write_operation`'s alone (its order is
    // checked there). Nothing in the CLI's write-test code calls a step of
    // it.
    #[test]
    fn write_test_only_drives_the_operation() {
        let write_test = production_fn_source("run_write_test");
        let mut previous = 0;
        for step in [
            "install_cancel_handler(cancel.clone())",
            "orchestration::operation::run_write_operation(request, &cancel, &mut observer)",
            "print_write_test_outcome(&outcome, &observer, &block_path)",
            "write_test_exit(&outcome)",
        ] {
            assert_eq!(write_test.matches(step).count(), 1, "{step}");
            let at = write_test.find(step).unwrap();
            assert!(at > previous, "{step} is out of order");
            previous = at;
        }

        // The CLI's write-test code, without string literals (its messages
        // name some of these steps) and comments.
        let production = production_source();
        let cli = &production[production.find("\nfn run_write_test(").unwrap()
            ..production
                .find("\n// Parses a CLI verify-mode argument")
                .unwrap()];
        let mut code = String::new();
        let mut in_string = false;
        let mut escaped = false;
        for c in cli.chars() {
            if in_string {
                match (escaped, c) {
                    (false, '\\') => escaped = true,
                    (false, '"') => in_string = false,
                    _ => escaped = false,
                }
            } else if c == '"' {
                in_string = true;
            } else {
                code.push(c);
            }
        }
        let code: String = code
            .lines()
            .map(|line| line.split("//").next().unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(code.contains("run_write_operation"));
        for step in [
            "linux_access::",
            "collect_device_snapshot",
            "select_target",
            "core::select",
            "core::revalidate",
            "open_image",
            "prepare_compressed_image",
            "confirmation_matches",
            "WriteIntent",
            "ConfirmationToken",
            "prepare_for_open",
            "finalize_prepared_write",
            "AuthorizedExecution::bind",
            "begin_write",
            "begin_sync",
            "begin_verify",
            "check_target",
        ] {
            assert!(!code.contains(step), "write-test calls {step} itself");
        }
    }

    // Exit status: 130 for a cancellation at any point (including right
    // after a successful sync), and `Completed` (exit 0) for everything
    // else -- a refusal or failure included, unchanged in Phase 3A.
    #[test]
    fn write_test_exit_is_130_only_for_a_cancellation() {
        use crate::orchestration::outcome::{CancelledAt, OperationError, OperationOutcome};

        for at in [
            CancelledAt::Preflight,
            CancelledAt::BeforeConfirmation,
            CancelledAt::Confirmation,
            CancelledAt::AfterSync,
            CancelledAt::BeforeVerify,
        ] {
            let exit = write_test_exit(&OperationOutcome::Cancelled(at));
            assert_eq!(exit, WriteTestExit::Cancelled);
            assert_eq!(write_test_exit_code(exit), Some(130));
        }

        for outcome in [
            OperationOutcome::Completed {
                verify: VerifySucceeded {
                    mode: VerifyMode::None,
                    verified_bytes: 0,
                    skipped: true,
                },
                image_size: 1,
            },
            OperationOutcome::Failed(OperationError::ConfirmationInputClosed),
            OperationOutcome::Failed(OperationError::WriteGate(
                core::WriteGateError::SnapshotRefreshFailed,
            )),
        ] {
            let exit = write_test_exit(&outcome);
            assert_eq!(exit, WriteTestExit::Completed, "{outcome:?}");
            assert_eq!(write_test_exit_code(exit), None);
        }
    }

    // ---------------------------------------------------------------------
    // parse_verify_mode / format_verify_succeeded / format_verify_failure_reason
    // (Built-in Verify implementation step 5: write-test CLI wiring)
    // ---------------------------------------------------------------------

    // F. Each of the three documented CLI names parses to the matching
    // VerifyMode.
    #[test]
    fn parse_verify_mode_accepts_the_three_known_names() {
        assert_eq!(parse_verify_mode("none"), Some(VerifyMode::None));
        assert_eq!(parse_verify_mode("quick"), Some(VerifyMode::Quick));
        assert_eq!(parse_verify_mode("full"), Some(VerifyMode::Full));
    }

    // G. Unknown values are rejected with None, never a silent fallback to
    // any particular mode.
    #[test]
    fn parse_verify_mode_rejects_unknown_values() {
        assert_eq!(parse_verify_mode("foo"), None);
        assert_eq!(parse_verify_mode("fast"), None);
        assert_eq!(parse_verify_mode("sha256"), None);
        assert_eq!(parse_verify_mode(""), None);
    }

    // H. Case sensitivity policy: lowercase only, fixed deliberately (see
    // `parse_verify_mode`'s own doc comment) -- any other casing is rejected
    // exactly like any other unknown value, not accepted as a convenience.
    #[test]
    fn parse_verify_mode_rejects_non_lowercase_casing() {
        assert_eq!(parse_verify_mode("None"), None);
        assert_eq!(parse_verify_mode("QUICK"), None);
        assert_eq!(parse_verify_mode("Full"), None);
    }

    // I. VerifyMode::None success formats as the documented "skipped"
    // message.
    #[test]
    fn format_verify_succeeded_none_reports_skipped() {
        let succeeded = VerifySucceeded {
            mode: VerifyMode::None,
            verified_bytes: 0,
            skipped: true,
        };
        assert_eq!(format_verify_succeeded(&succeeded), "Verification: skipped");
    }

    // J. Quick success reports the sampled byte count, worded as "sampled"
    // -- never implying the whole image was checked.
    #[test]
    fn format_verify_succeeded_quick_reports_sampled_bytes() {
        let succeeded = VerifySucceeded {
            mode: VerifyMode::Quick,
            verified_bytes: 12_582_912,
            skipped: false,
        };
        assert_eq!(
            format_verify_succeeded(&succeeded),
            "Quick verification succeeded (12582912 bytes sampled)"
        );
    }

    // K. Full success reports the verified byte count, worded as
    // "verified".
    #[test]
    fn format_verify_succeeded_full_reports_verified_bytes() {
        let succeeded = VerifySucceeded {
            mode: VerifyMode::Full,
            verified_bytes: 1_828_716_544,
            skipped: false,
        };
        assert_eq!(
            format_verify_succeeded(&succeeded),
            "Full verification succeeded (1828716544 bytes verified)"
        );
    }

    // L. A Mismatch failure's formatted text includes the offset and both
    // the expected and actual byte values.
    #[test]
    fn format_verify_failure_reason_mismatch_reports_offset_and_bytes() {
        let reason = VerifyFailureReason::Mismatch {
            offset: 123456,
            expected: 0x12,
            actual: 0x34,
        };
        let formatted = format_verify_failure_reason(&reason);
        assert!(formatted.contains("123456"));
        assert!(formatted.contains("0x12"));
        assert!(formatted.contains("0x34"));
    }

    // M. Source vs. target EOF/read-error messages are distinguishable from
    // one another -- a user (or a future automated triage) should be able to
    // tell which side failed from the text alone.
    #[test]
    fn format_verify_failure_reason_distinguishes_source_and_target_eof() {
        let source = format_verify_failure_reason(&VerifyFailureReason::SourceUnexpectedEof);
        let target = format_verify_failure_reason(&VerifyFailureReason::TargetUnexpectedEof);
        assert!(source.contains("image source"));
        assert!(target.contains("target device"));
        assert_ne!(source, target);
    }

    // N. UnsupportedAccess's message makes clear that Quick specifically
    // needs random-access support, so a user understands why Quick (and not
    // necessarily Full) failed for this image.
    #[test]
    fn format_verify_failure_reason_unsupported_access_mentions_random_access() {
        let formatted = format_verify_failure_reason(&VerifyFailureReason::UnsupportedAccess);
        assert!(formatted.to_lowercase().contains("random"));
    }

    // A source change at a Verify checkpoint says the image changed and that
    // the verification result was not accepted, without naming a cause.
    #[test]
    fn format_verify_failure_reason_source_changed_rejects_the_result() {
        let changed = SourceChanged::Unverifiable(std::io::Error::other("simulated"));
        let formatted = format_verify_failure_reason(&VerifyFailureReason::SourceChanged(changed));
        assert!(formatted.contains("image file"));
        assert!(formatted.contains("not accepted"));
    }

    // ---------------------------------------------------------------------
    // Verify Pre-flight Diagnostics CLI display (implementation step 5+6)
    // ---------------------------------------------------------------------

    fn base_snapshot() -> DeviceSnapshot {
        DeviceSnapshot {
            device: "/dev/sdx".to_string(),
            block_path: "/org/freedesktop/UDisks2/block_devices/sdx".to_string(),
            drive_path: "/org/freedesktop/UDisks2/drives/Test_Model_TEST-SERIAL-0001".to_string(),
            major: 8,
            minor: 0,
            diskseq: Some(12),
            size: 8_000_000_000,
            read_only: false,
            media_available: true,
            model: "Test Model".to_string(),
            vendor: "Test Vendor".to_string(),
            serial: "TEST-SERIAL-0001".to_string(),
            connection_bus: "usb".to_string(),
            removable: true,
            hint_system: false,
            hint_ignore: false,
            hint_partitionable: true,
            mount_points: Vec::new(),
            active_swap: false,
            swap_devices: Vec::new(),
            complex_storage: false,
            complex_storage_details: Vec::new(),
        }
    }

    // O. An unchanged snapshot (Identity Same, Instance SameInstance, no
    // mount/read-only change, no hazards) produces the exact five-line
    // "everything is fine" summary.
    #[test]
    fn format_verify_diagnostics_summary_for_unchanged_snapshot() {
        let baseline = base_snapshot();
        let current = base_snapshot();
        let diagnostics = core::diagnose_identity_instance_for_verify(&baseline, &current);

        assert_eq!(
            format_verify_diagnostics_summary(&diagnostics),
            vec![
                "identity: Same".to_string(),
                "instance: SameInstance".to_string(),
                "mount points: unchanged (none)".to_string(),
                "read-only: unchanged (false)".to_string(),
                "hazards: none".to_string(),
            ]
        );
    }

    // P. mount_points growing from empty to a single path -- the real
    // post-write auto-mount scenario -- is shown as a `before -> after`
    // diff, and does not appear as a hazard.
    #[test]
    fn format_verify_diagnostics_summary_reports_a_newly_mounted_filesystem() {
        let baseline = base_snapshot();
        let mut current = base_snapshot();
        current.mount_points = vec!["/media/test/MYPOCKETOS".to_string()];
        let diagnostics = core::diagnose_identity_instance_for_verify(&baseline, &current);

        let lines = format_verify_diagnostics_summary(&diagnostics);
        assert!(lines.contains(&"mount points: none -> /media/test/MYPOCKETOS".to_string()));
        assert!(lines.contains(&"hazards: none".to_string()));
    }

    // Q. Multiple mount points are listed in a deterministic,
    // comma-separated order (the same order `DeviceSnapshot.mount_points`
    // itself carries them in -- this formatter never sorts or reorders).
    #[test]
    fn format_verify_diagnostics_summary_lists_multiple_mount_points_in_order() {
        let baseline = base_snapshot();
        let mut current = base_snapshot();
        current.mount_points = vec![
            "/media/test/MYPOCKETOS".to_string(),
            "/media/test/MYPOCKETOS-EFI".to_string(),
        ];
        let diagnostics = core::diagnose_identity_instance_for_verify(&baseline, &current);

        let lines = format_verify_diagnostics_summary(&diagnostics);
        assert!(lines.contains(
            &"mount points: none -> /media/test/MYPOCKETOS, /media/test/MYPOCKETOS-EFI".to_string()
        ));
    }

    // R. `read_only` going from `false` to `true` is shown as a
    // `before -> after` diff, and (per Step 3's deliberate design) is never
    // itself a hazard.
    #[test]
    fn format_verify_diagnostics_summary_reports_read_only_becoming_true() {
        let baseline = base_snapshot();
        let mut current = base_snapshot();
        current.read_only = true;
        let diagnostics = core::diagnose_identity_instance_for_verify(&baseline, &current);

        let lines = format_verify_diagnostics_summary(&diagnostics);
        assert!(lines.contains(&"read-only: false -> true".to_string()));
        assert!(lines.contains(&"hazards: none".to_string()));
    }

    // S. A single hazard is rendered as its human-readable text, not the
    // raw Rust enum name.
    #[test]
    fn format_verify_diagnostics_summary_reports_a_single_hazard() {
        let baseline = base_snapshot();
        let mut current = base_snapshot();
        current.hint_system = true;
        let diagnostics = core::diagnose_identity_instance_for_verify(&baseline, &current);

        let lines = format_verify_diagnostics_summary(&diagnostics);
        assert!(lines.contains(&"hazards: system device".to_string()));
    }

    // T. Multiple simultaneous hazards are listed in the same deterministic
    // order `core::verify_target_hard_hazards` produces them in.
    #[test]
    fn format_verify_diagnostics_summary_lists_multiple_hazards_in_deterministic_order() {
        let baseline = base_snapshot();
        let mut current = base_snapshot();
        current.complex_storage = true;
        current.active_swap = true;
        let diagnostics = core::diagnose_identity_instance_for_verify(&baseline, &current);

        let lines = format_verify_diagnostics_summary(&diagnostics);
        assert!(lines.contains(&"hazards: active swap, complex storage".to_string()));
    }

    // U. Every `IdentityComparison` variant has a distinct, deterministic
    // rendering -- including both rejection cases, not just `Same`.
    #[test]
    fn format_identity_comparison_covers_every_variant() {
        assert_eq!(format_identity_comparison(IdentityComparison::Same), "Same");
        assert_eq!(
            format_identity_comparison(IdentityComparison::Changed),
            "Changed (different device)"
        );
        assert_eq!(
            format_identity_comparison(IdentityComparison::InsufficientIdentity),
            "Insufficient (no usable serial to compare)"
        );
    }

    // V. Every `InstanceComparison` variant has a distinct, deterministic
    // rendering -- including both rejection cases, not just `SameInstance`.
    #[test]
    fn format_instance_comparison_covers_every_variant() {
        assert_eq!(
            format_instance_comparison(InstanceComparison::SameInstance),
            "SameInstance"
        );
        assert_eq!(
            format_instance_comparison(InstanceComparison::Recreated),
            "Recreated (disconnected and reconnected)"
        );
        assert_eq!(
            format_instance_comparison(InstanceComparison::InsufficientInformation),
            "Insufficient (no diskseq to compare)"
        );
    }

    // W. `format_hard_hazards` itself: empty, single, and multiple-reason
    // cases, independent of the full summary wiring above.
    #[test]
    fn format_hard_hazards_lists_every_reason_in_order() {
        assert_eq!(format_hard_hazards(&[]), "none");
        assert_eq!(
            format_hard_hazards(&[HardHazardReason::SystemDevice]),
            "system device"
        );
        assert_eq!(
            format_hard_hazards(&[
                HardHazardReason::ActiveSwap,
                HardHazardReason::ComplexStorage
            ]),
            "active swap, complex storage"
        );
    }

    // X. When `check_target()` rejected with `SnapshotRefreshFailed`, no
    // `VerifyTargetDiagnostics` exists -- the fallback text says so plainly,
    // rather than guessing at values that were never fetched.
    #[test]
    fn format_verify_diagnostics_unavailable_reports_no_snapshot() {
        assert_eq!(
            format_verify_diagnostics_unavailable(),
            vec!["no fresh device snapshot was available for diagnostics".to_string()]
        );
    }

    // ---------------------------------------------------------------------
    // parse_write_test_trailing_args (--test-pause-before-verify)
    // ---------------------------------------------------------------------

    // Y1. `full` + the flag is accepted.
    #[test]
    fn parse_write_test_trailing_args_accepts_full_with_test_pause() {
        assert_eq!(
            parse_write_test_trailing_args(Some("full"), Some("--test-pause-before-verify")),
            Ok((VerifyMode::Full, true))
        );
    }

    // Y2. `quick` + the flag is accepted.
    #[test]
    fn parse_write_test_trailing_args_accepts_quick_with_test_pause() {
        assert_eq!(
            parse_write_test_trailing_args(Some("quick"), Some("--test-pause-before-verify")),
            Ok((VerifyMode::Quick, true))
        );
    }

    // Y3. `none` + the flag is rejected outright -- `VerifyMode::None` never
    // produces `VerifyStart::Pending`, so the flag would have no effect.
    #[test]
    fn parse_write_test_trailing_args_rejects_none_with_test_pause() {
        assert_eq!(
            parse_write_test_trailing_args(Some("none"), Some("--test-pause-before-verify")),
            Err(WriteTestArgsError::TestPauseRequiresVerification)
        );
    }

    // Y4. An unrecognized fourth token is rejected, never silently ignored.
    #[test]
    fn parse_write_test_trailing_args_rejects_unknown_fourth_token() {
        assert_eq!(
            parse_write_test_trailing_args(Some("full"), Some("--bogus-flag")),
            Err(WriteTestArgsError::UnrecognizedFourthArgument)
        );
    }

    // Y5. No fourth argument at all -> existing behavior (flag disabled),
    // for every verify-mode including the absent (default) case.
    #[test]
    fn parse_write_test_trailing_args_without_fourth_argument_matches_existing_behavior() {
        assert_eq!(
            parse_write_test_trailing_args(None, None),
            Ok((VerifyMode::None, false))
        );
        assert_eq!(
            parse_write_test_trailing_args(Some("quick"), None),
            Ok((VerifyMode::Quick, false))
        );
        assert_eq!(
            parse_write_test_trailing_args(Some("full"), None),
            Ok((VerifyMode::Full, false))
        );
    }

    // Y6. Existing verify-mode parsing (absent -> None, unknown -> rejected)
    // is preserved unchanged through the new combined parser.
    #[test]
    fn parse_write_test_trailing_args_verify_mode_parsing_has_no_regression() {
        assert_eq!(
            parse_write_test_trailing_args(Some("none"), None),
            Ok((VerifyMode::None, false))
        );
        assert_eq!(
            parse_write_test_trailing_args(Some("bogus"), None),
            Err(WriteTestArgsError::InvalidVerifyMode)
        );
        // An invalid verify-mode is reported even when a fourth argument is
        // also present -- verify-mode is validated first.
        assert_eq!(
            parse_write_test_trailing_args(Some("bogus"), Some("--test-pause-before-verify")),
            Err(WriteTestArgsError::InvalidVerifyMode)
        );
    }

    // ---------------------------------------------------------------------
    // Cancel (Ctrl+C) CLI messaging and exit code (Cancel機能 Ctrl+C実配線
    // implementation). Shared-`CancelHandle` behavior itself (clone sees the
    // same `request_cancel()`) is already covered extensively by
    // `execution::write_job`'s own tests (e.g.
    // `cancel_partway_through_multiple_chunks_reports_partial_progress`,
    // which clones a handle into the write loop and calls `request_cancel()`
    // from the original) -- not duplicated here.
    // ---------------------------------------------------------------------

    // Z1. A write cancelled after a partial write reports the exact
    // bytes_written/image_size pair, and unconditionally states that the
    // target may be partial and that verification was not attempted.
    #[test]
    fn format_write_cancelled_reports_partial_bytes_and_no_verification() {
        let cancelled = Cancelled {
            image_size: 1_000_000,
            bytes_written: 400_000,
            reason: CancelReason::UserRequested,
            target_may_be_modified: true,
            retry_requires_fresh_gate: true,
        };

        assert_eq!(
            format_write_cancelled(&cancelled),
            vec![
                "write cancelled after 400000 of 1000000 bytes".to_string(),
                "target may contain a partial image".to_string(),
                "verification was not attempted".to_string(),
            ]
        );
    }

    // Z2. A write cancelled before any chunk completed (bytes_written == 0,
    // target_may_be_modified == false) is still reported as a cancellation,
    // but says no data was written -- never "partial image", and never "not
    // opened" (the target was opened read-write by then).
    #[test]
    fn format_write_cancelled_reports_zero_bytes_before_any_chunk() {
        let cancelled = Cancelled {
            image_size: 1_000_000,
            bytes_written: 0,
            reason: CancelReason::UserRequested,
            target_may_be_modified: false,
            retry_requires_fresh_gate: true,
        };

        let lines = format_write_cancelled(&cancelled);
        assert_eq!(
            lines,
            vec![
                "write cancelled after 0 of 1000000 bytes".to_string(),
                "no data was written to the target device".to_string(),
                "verification was not attempted".to_string(),
            ]
        );
        assert!(lines.iter().all(|line| !line.contains("partial image")));
        assert!(lines.iter().all(|line| !line.contains("not opened")));
    }

    // Z2b. Defensive: if `target_may_be_modified` is true even though
    // `bytes_written == 0` (not produced by today's `Writing::write()`, but
    // the field is the safety-biased authority), the safe-side "partial
    // image" wording wins.
    #[test]
    fn format_write_cancelled_zero_bytes_but_possibly_modified_reports_partial() {
        let cancelled = Cancelled {
            image_size: 1_000_000,
            bytes_written: 0,
            reason: CancelReason::UserRequested,
            target_may_be_modified: true,
            retry_requires_fresh_gate: true,
        };

        let lines = format_write_cancelled(&cancelled);
        assert_eq!(lines[1], "target may contain a partial image");
        assert!(
            lines
                .iter()
                .all(|line| !line.contains("no data was written"))
        );
    }

    // Z2c. Defensive in the other direction: bytes were written but the flag
    // says unmodified (also not produced today) -- still "partial image".
    #[test]
    fn format_write_cancelled_bytes_written_without_flag_reports_partial() {
        let cancelled = Cancelled {
            image_size: 1_000_000,
            bytes_written: 4096,
            reason: CancelReason::UserRequested,
            target_may_be_modified: false,
            retry_requires_fresh_gate: true,
        };

        assert_eq!(
            format_write_cancelled(&cancelled)[1],
            "target may contain a partial image"
        );
    }

    // Z2d. Cancelled before confirmation: states the target was neither
    // opened nor modified.
    #[test]
    fn format_cancelled_before_confirmation_states_target_untouched() {
        assert_eq!(
            format_cancelled_before_confirmation(),
            vec![
                "cancelled before confirmation.".to_string(),
                "the target device has not been opened or modified.".to_string(),
            ]
        );
    }

    // Z2e. Cancelled during sync: states sync finished, the full image is on
    // the target, and verification was not started -- never "partial".
    #[test]
    fn format_cancelled_after_sync_states_full_image_and_no_verification() {
        let lines = format_cancelled_after_sync();

        assert_eq!(
            lines,
            vec![
                "cancellation was requested during sync; sync was allowed to finish.".to_string(),
                "write + sync completed successfully; the full image is on the target.".to_string(),
                "verification was not started.".to_string(),
            ]
        );
        assert!(lines.iter().all(|line| !line.contains("partial")));
    }

    // ---------------------------------------------------------------------
    // Image format refusals (XZ/GZIP Phase 1): pure formatters only.
    // ---------------------------------------------------------------------

    // F2. An unsupported format names the format and says it was not treated
    // as raw; the target was not touched.
    #[test]
    fn format_image_format_rejected_for_unsupported_format() {
        let error = crate::image_source::ImageSourceError::UnsupportedFormat(
            crate::image_source::UnsupportedCompression::Zstd,
        );

        assert_eq!(
            format_image_format_rejected(&error),
            vec![
                "the image is zstd data, which is not supported; it was not treated as a raw image."
                    .to_string(),
                "the target device has not been opened or modified.".to_string(),
            ]
        );
    }

    // F3. An extension mismatch names the promised format and refuses raw.
    #[test]
    fn format_image_format_rejected_for_extension_mismatch() {
        let error = crate::image_source::ImageSourceError::ExtensionMismatch {
            expected: crate::image_source::CompressionFormat::Xz,
        };

        let lines = format_image_format_rejected(&error);
        assert_eq!(
            lines[0],
            "the file name says xz but the content is not xz data; refusing to write it as a raw image."
        );
        assert_eq!(
            lines[1],
            "the target device has not been opened or modified."
        );
    }

    // S1f. A panicked sync worker is reported as a sync failure: it never
    // claims success or a full, synced image.
    #[test]
    fn format_sync_worker_panicked_reports_failure_without_claiming_durability() {
        let lines = format_sync_worker_panicked();

        assert!(lines[0].starts_with("sync FAILED"));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("durability is not confirmed"))
        );
        assert!(lines.iter().all(|line| !line.contains("succeeded")));
    }

    // ---------------------------------------------------------------------
    // Human Confirmation prompt wait (D3): `wait_for_prompt_input` is driven
    // here with a plain channel and a plain closure -- no stdin, no terminal,
    // no signal. A 1 ms poll interval keeps the waiting tests fast.
    // ---------------------------------------------------------------------

    const TEST_POLL: Duration = Duration::from_millis(1);

    // P1. A line that arrives with no cancellation is returned unchanged.
    #[test]
    fn prompt_wait_returns_line() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(PromptInput::Line("/dev/sdb\n".to_string()))
            .unwrap();

        match wait_for_prompt_input(&receiver, || false, TEST_POLL) {
            PromptInput::Line(line) => assert_eq!(line, "/dev/sdb\n"),
            other => panic!("expected Line, got {other:?}"),
        }
    }

    // P2. EOF is passed through as Eof (never treated as a confirmation).
    #[test]
    fn prompt_wait_returns_eof() {
        let (sender, receiver) = mpsc::channel();
        sender.send(PromptInput::Eof).unwrap();

        assert!(matches!(
            wait_for_prompt_input(&receiver, || false, TEST_POLL),
            PromptInput::Eof
        ));
    }

    // P3. A read error is passed through as Error.
    #[test]
    fn prompt_wait_returns_input_error() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(PromptInput::Error(std::io::Error::other("boom")))
            .unwrap();

        match wait_for_prompt_input(&receiver, || false, TEST_POLL) {
            PromptInput::Error(error) => assert_eq!(error.to_string(), "boom"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    // P4. Cancellation requested while waiting (no input ever arrives; the
    // sender stays alive, like a reader thread blocked in read_line) ends the
    // wait with Cancelled after a few polls.
    #[test]
    fn prompt_wait_returns_cancelled_when_cancel_arrives_while_waiting() {
        let (_sender, receiver) = mpsc::channel::<PromptInput>();
        let checks = Cell::new(0u32);

        let result = wait_for_prompt_input(
            &receiver,
            || {
                checks.set(checks.get() + 1);
                checks.get() > 3
            },
            TEST_POLL,
        );

        assert!(matches!(result, PromptInput::Cancelled));
        assert!(checks.get() > 3, "must have polled before cancelling");
    }

    // P4b. Same, driven by a real `CancelHandle` from another thread -- the
    // exact `|| cancel.is_requested()` shape `run_write_test` uses.
    #[test]
    fn prompt_wait_observes_cancel_handle_from_another_thread() {
        let (_sender, receiver) = mpsc::channel::<PromptInput>();
        let cancel = CancelHandle::new();
        let remote = cancel.clone();

        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            remote.request_cancel(CancelReason::UserRequested);
        });

        let result = wait_for_prompt_input(&receiver, || cancel.is_requested(), TEST_POLL);
        canceller.join().unwrap();

        assert!(matches!(result, PromptInput::Cancelled));
    }

    // P5. Already cancelled before waiting starts: Cancelled, even though a
    // (correct) line is already queued.
    #[test]
    fn prompt_wait_returns_cancelled_when_already_cancelled() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(PromptInput::Line("/dev/sdb\n".to_string()))
            .unwrap();

        assert!(matches!(
            wait_for_prompt_input(&receiver, || true, TEST_POLL),
            PromptInput::Cancelled
        ));
    }

    // P6. Input and cancellation at (nearly) the same moment: the first
    // check sees no cancellation, the line is received, and the re-check
    // right after receipt sees it -- cancellation wins and the line is
    // discarded.
    #[test]
    fn prompt_wait_prefers_cancel_over_simultaneous_input() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(PromptInput::Line("/dev/sdb\n".to_string()))
            .unwrap();
        let checks = Cell::new(0u32);

        let result = wait_for_prompt_input(
            &receiver,
            || {
                checks.set(checks.get() + 1);
                checks.get() > 1
            },
            TEST_POLL,
        );

        assert!(matches!(result, PromptInput::Cancelled));
        assert_eq!(checks.get(), 2);
    }

    // P7. The reader ended without sending anything: reported as Error, never
    // as a confirmation -- unless a cancellation was requested, which wins.
    #[test]
    fn prompt_wait_disconnected_reader_is_error_or_cancelled() {
        let (sender, receiver) = mpsc::channel::<PromptInput>();
        drop(sender);
        assert!(matches!(
            wait_for_prompt_input(&receiver, || false, TEST_POLL),
            PromptInput::Error(_)
        ));

        let (sender, receiver) = mpsc::channel::<PromptInput>();
        drop(sender);
        let checks = Cell::new(0u32);
        let result = wait_for_prompt_input(
            &receiver,
            || {
                checks.set(checks.get() + 1);
                checks.get() > 1
            },
            TEST_POLL,
        );
        assert!(matches!(result, PromptInput::Cancelled));
    }

    // Z3. A cancelled Full Verify reports "Full", the bytes actually
    // verified, and the caller-supplied total (not `image.logical_size()`
    // directly, since Quick's total legitimately differs -- see Z4) -- and
    // states that the already-written image remains on the target.
    #[test]
    fn format_verify_cancelled_reports_full_mode_and_total() {
        let cancelled = VerifyCancelled {
            mode: VerifyMode::Full,
            verified_bytes: 500_000,
        };

        assert_eq!(
            format_verify_cancelled(&cancelled, 1_000_000),
            vec![
                "Full verification cancelled after 500000 of 1000000 bytes".to_string(),
                "written image remains on the target".to_string(),
            ]
        );
    }

    // Z4. A cancelled Quick Verify reports "Quick" and the sampled total
    // (e.g. 12 MiB for a 16 MiB image), never the full image size.
    #[test]
    fn format_verify_cancelled_reports_quick_mode_and_sampled_total() {
        let cancelled = VerifyCancelled {
            mode: VerifyMode::Quick,
            verified_bytes: 4_194_304,
        };

        let lines = format_verify_cancelled(&cancelled, 12_582_912);
        assert_eq!(
            lines[0],
            "Quick verification cancelled after 4194304 of 12582912 bytes"
        );
    }

    // Z5. The pre-verify early-cancel message never implies any FD or D-Bus
    // state was opened and then closed -- nothing was ever opened.
    #[test]
    fn format_verify_cancelled_before_start_does_not_mention_fd_or_opendevice() {
        let lines = format_verify_cancelled_before_start();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("cancelled before it could start"));
        assert!(lines[1].contains("write + sync already completed successfully"));
        for line in &lines {
            assert!(!line.to_lowercase().contains("opendevice"));
            assert!(!line.to_lowercase().contains("fd"));
        }
    }

    // Z6. `WriteTestExit::Cancelled` maps to exit code 130 (128 + SIGINT),
    // the conventional Unix code for a signal-interrupted process.
    #[test]
    fn write_test_exit_code_maps_cancelled_to_130() {
        assert_eq!(write_test_exit_code(WriteTestExit::Cancelled), Some(130));
    }

    // Z7. `WriteTestExit::Completed` maps to `None`, telling the caller to
    // let `main` exit the ordinary way (code 0) rather than calling
    // `std::process::exit` at all.
    #[test]
    fn write_test_exit_code_maps_completed_to_none() {
        assert_eq!(write_test_exit_code(WriteTestExit::Completed), None);
    }

    // ---------------------------------------------------------------------
    // Compressed image messages (rejection / Preflight progress) and the
    // post-Preflight target re-check. The preparation itself, and its tests,
    // are in `orchestration::image`.
    // ---------------------------------------------------------------------

    use super::CompressedImageRejection;
    use super::{format_compressed_image_rejected, format_preflight_progress};
    use crate::image_source::compressed::{PreflightError, PreflightProgress};

    // The xz-specific Preflight rejections are displayed like every other
    // one: nothing was written, the target was not touched.
    #[test]
    fn format_compressed_image_rejected_covers_xz_reasons() {
        let rejections = [
            CompressedImageRejection::QuickVerifyUnsupported(
                crate::image_source::CompressionFormat::Xz,
            ),
            CompressedImageRejection::Preflight(PreflightError::IntegrityCheckMissing),
            CompressedImageRejection::Preflight(PreflightError::UnsupportedIntegrityCheck),
            CompressedImageRejection::Preflight(PreflightError::DecoderMemoryLimitExceeded {
                limit: 512 * 1024 * 1024,
            }),
            CompressedImageRejection::Preflight(PreflightError::DecoderFailure(
                std::io::Error::other("simulated"),
            )),
        ];
        for rejection in &rejections {
            let lines = format_compressed_image_rejected(rejection);
            assert!(
                lines[0].starts_with("compressed image rejected: "),
                "{lines:?}"
            );
            assert_eq!(
                lines.last().unwrap(),
                "the target device has not been opened or modified."
            );
        }
        let quick = format_compressed_image_rejected(&rejections[0]);
        assert!(quick[0].contains("xz") && quick[0].contains("full") && quick[0].contains("none"));
    }

    // Every rejection message says nothing was written and the target was
    // not touched; the Quick message points to the modes that do work.
    #[test]
    fn format_compressed_image_rejected_states_target_untouched() {
        let rejections = [
            CompressedImageRejection::QuickVerifyUnsupported(
                crate::image_source::CompressionFormat::Gzip,
            ),
            CompressedImageRejection::Preflight(PreflightError::Corrupt(std::io::Error::other(
                "bad crc",
            ))),
            CompressedImageRejection::Preflight(PreflightError::LogicalSizeLimitExceeded {
                limit: 42,
            }),
            CompressedImageRejection::SourceChanged(
                crate::image_source::source_identity::SourceChanged::Unverifiable(
                    std::io::Error::other("simulated"),
                ),
            ),
        ];
        for rejection in &rejections {
            let lines = format_compressed_image_rejected(rejection);
            assert!(lines[0].starts_with("compressed image rejected: "));
            assert_eq!(
                lines.last().unwrap(),
                "the target device has not been opened or modified."
            );
        }
        let quick = format_compressed_image_rejected(&rejections[0]);
        assert!(quick[0].contains("full") && quick[0].contains("none"));
    }

    #[test]
    fn format_preflight_progress_shows_both_sides() {
        let line = format_preflight_progress(&PreflightProgress {
            compressed_consumed: 10,
            compressed_total: 20,
            logical_produced: 30,
        });
        assert!(line.contains("10/20") && line.contains("30 bytes decoded"));
    }

    // The post-Preflight target re-check (`core::revalidate` on a fresh
    // snapshot, then `is_ready_to_open`): a target replaced, recreated or
    // removed while Preflight ran is no longer ready to open.
    #[test]
    fn target_changed_during_preflight_is_not_ready_to_open() {
        let selected = || core::select(base_snapshot()).unwrap();

        let unchanged = core::revalidate(
            selected(),
            crate::device::SnapshotFetchOutcome::Found(base_snapshot()),
        );
        assert!(core::is_ready_to_open(&unchanged));

        let mut replaced = base_snapshot();
        replaced.serial = "OTHER-SERIAL".to_string();
        let mut recreated = base_snapshot();
        recreated.diskseq = Some(13);
        let mut resized = base_snapshot();
        resized.size = 4_000_000_000;
        let mut mounted = base_snapshot();
        mounted.mount_points = vec!["/".to_string()];

        for (name, outcome) in [
            (
                "replaced",
                crate::device::SnapshotFetchOutcome::Found(replaced),
            ),
            (
                "recreated",
                crate::device::SnapshotFetchOutcome::Found(recreated),
            ),
            (
                "resized",
                crate::device::SnapshotFetchOutcome::Found(resized),
            ),
            (
                "mounted",
                crate::device::SnapshotFetchOutcome::Found(mounted),
            ),
            ("removed", crate::device::SnapshotFetchOutcome::NotFound),
        ] {
            let state = core::revalidate(selected(), outcome);
            assert!(!core::is_ready_to_open(&state), "{name}");
        }
    }
}

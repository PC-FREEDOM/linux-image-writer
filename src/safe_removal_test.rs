// CLI diagnostic mode (`safe-removal-test`): DESTRUCTIVE, for real-USB
// testing of Safe Removal only.
//
//   1. Writes an image to the device through the Production write worker
//      (`spawn_write_worker`: selection, image preparation, the final
//      confirmation -- the device name typed here -- the Write Gate,
//      OpenDevice, FD binding, write, sync, Verify), exactly as a GUI does.
//   2. After `Finished`, takes the operation's own `RemovalTarget`
//      (`WriteWorker::removal_target`, `None` when the outcome offers no
//      removal) and joins the worker.
//   3. With `--pause-for-mount-setup` (test-only), pauses so the tester
//      can mount a filesystem of the written drive by hand, then shows the
//      drive's current filesystems and mount points (read-only; nothing is
//      mounted, unmounted or changed here).
//   4. Only if the user then types `remove`, calls `request_safe_removal`,
//      which unmounts the drive's mounted filesystems and asks UDisks2 to
//      power it off (never forced, never interactive), and prints the
//      outcome.
//
// The removal target comes only from the write operation; no path given on
// the command line becomes one. The serial number is never printed.

use std::io::{self, Write as _};

use crate::device::RemovalFactsOutcome;
use crate::execution::core::VerifyMode;
use crate::linux_backend::removal_facts::collect_removal_facts;
use crate::orchestration::candidates::TargetRef;
use crate::orchestration::events::{ConfirmationDecision, OpenPurpose};
use crate::orchestration::operation::WriteOperationRequest;
use crate::orchestration::outcome::{CancelledAt, OperationError, OperationOutcome};
use crate::orchestration::removal::{SafeRemovalOutcome, request_safe_removal};
use crate::orchestration::worker::{WorkerEvent, WorkerMessage, spawn_write_worker};

const USAGE: &str = "usage: cargo run --locked --bin linux-usb-writer -- safe-removal-test <image-path> <udisks2-block-object-path> [none|quick|full] [--pause-for-mount-setup]\n\
     DESTRUCTIVE: writes the image to the device (after the typed confirmation),\n\
     then, only if you type 'remove', unmounts the drive's filesystems and powers it off.\n\
     --pause-for-mount-setup: before the 'remove' prompt, wait for Enter so a filesystem\n\
     of the written drive can be mounted by hand (test setup only).";

const PAUSE_FLAG: &str = "--pause-for-mount-setup";

pub fn run(mut args: Vec<String>) -> zbus::Result<()> {
    let pause_for_mount_setup = match args.iter().position(|arg| arg == PAUSE_FLAG) {
        Some(index) => {
            args.remove(index);
            true
        }
        None => false,
    };
    let (image_path, block_path, verify_mode) = match args.as_slice() {
        [image, block] => (image.clone(), block.clone(), VerifyMode::None),
        [image, block, mode] => match parse_verify_mode(mode) {
            Some(mode) => (image.clone(), block.clone(), mode),
            None => {
                eprintln!("safe-removal-test: invalid verify-mode '{mode}'");
                eprintln!("{USAGE}");
                std::process::exit(1);
            }
        },
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(1);
        }
    };

    println!("safe-removal-test: DESTRUCTIVE real-device test.");
    println!("Step 1 writes {image_path} to the device you confirm below.");
    println!("Step 2 (only if you type 'remove') unmounts and powers off that drive.");

    let request = WriteOperationRequest {
        target: TargetRef::from_block_path(block_path),
        image_path,
        verify_mode,
    };
    let mut worker = match spawn_write_worker(request) {
        Ok(worker) => worker,
        Err(error) => {
            eprintln!("safe-removal-test: could not start the write worker: {error}");
            return Ok(());
        }
    };

    let mut progress = Progress::default();
    let mut outcome = None;
    while let Some(message) = worker.recv() {
        match message {
            WorkerMessage::Event(event) => progress.show(&event),
            WorkerMessage::ConfirmationRequested(request) => {
                println!();
                println!("Final confirmation:");
                println!(
                    "  target:  {} {} ({}, {} bytes, {})",
                    request.target.vendor,
                    request.target.model,
                    request.target.device,
                    request.target.size,
                    request.target.connection_bus
                );
                println!("  object:  {}", request.block_path);
                println!("  diskseq: {:?}", request.diskseq);
                println!("  image:   {} bytes", request.image_size);
                println!("  verify:  {:?}", request.verify_mode);
                println!("  ALL DATA ON THIS DEVICE WILL BE ERASED.");
                let decision = match prompt(&format!(
                    "Type the device name ({}) to write, anything else to cancel: ",
                    request.expected_text
                )) {
                    Some(typed) => ConfirmationDecision::Submitted(typed),
                    None => ConfirmationDecision::InputClosed,
                };
                if worker.submit_confirmation(decision).is_err() {
                    eprintln!("safe-removal-test: the confirmation was not delivered");
                }
            }
            WorkerMessage::Finished(finished) => outcome = Some(*finished),
        }
    }

    // After `Finished`, before `join` (which consumes the worker).
    let removal = worker.removal_target();
    if worker.join().is_err() {
        eprintln!("safe-removal-test: the write worker panicked");
        return Ok(());
    }

    println!();
    match &outcome {
        Some(outcome) => println!("Write operation: {}", summarize(outcome)),
        None => println!("Write operation: ended without an outcome"),
    }
    let Some(removal) = removal else {
        println!(
            "Safe Removal is not offered after this outcome. Nothing was unmounted or powered off."
        );
        return Ok(());
    };

    println!();
    println!("Safe Removal target: {removal:?}");
    if pause_for_mount_setup && !pause_for_mount_setup_step(removal.block_path()) {
        println!("Not requested. Nothing was unmounted or powered off.");
        return Ok(());
    }
    println!("Safe Removal will: re-check that this is the device and instance just written,");
    println!("unmount its mounted filesystems (never forced), re-check it again, then ask UDisks2");
    println!("to power the drive off. No authentication prompt is allowed; if one would be");
    println!("needed, it stops instead.");
    if prompt("Type 'remove' to request safe removal, anything else to stop: ").as_deref()
        != Some("remove")
    {
        println!("Not requested. Nothing was unmounted or powered off.");
        return Ok(());
    }

    match request_safe_removal(&removal) {
        SafeRemovalOutcome::Removed { device, unmounted } => {
            println!(
                "Removed: UDisks2 powered off {} {} ({}).",
                device.vendor, device.model, device.device
            );
            println!("Unmounted by this removal: {unmounted:?}");
            println!("The drive can now be removed.");
        }
        other => {
            println!("Safe Removal did not complete: {other:?}");
            println!("Do not treat the drive as safe to remove.");
        }
    }

    Ok(())
}

// Test-only setup step: waits while the tester mounts a filesystem of the
// written drive by hand (for example, with the desktop file manager), then
// shows what is mounted now. Read-only: this step itself mounts, unmounts
// and changes nothing, and its reading is for the tester only --
// `request_safe_removal` reads and checks everything again itself.
// `false` when the input ends instead (then nothing further is done).
fn pause_for_mount_setup_step(block_path: &str) -> bool {
    println!();
    println!("Mounted-filesystem test setup (test-only pause).");
    println!("Now mount a filesystem of the written drive by hand if the test needs one.");
    println!("This program does not mount anything, and nothing is unmounted or powered off yet.");
    if prompt("Press Enter when the setup is done: ").is_none() {
        return false;
    }

    println!("Current state of the written drive (read-only, for your information only):");
    match collect_removal_facts(block_path) {
        RemovalFactsOutcome::Found(facts) => {
            if facts.filesystems.is_empty() {
                println!("  filesystems: none");
            }
            for filesystem in &facts.filesystems {
                let mounts = if filesystem.mount_points.is_empty() {
                    "not mounted".to_string()
                } else {
                    format!("mounted at {}", filesystem.mount_points.join(", "))
                };
                println!("  {}  {mounts}", filesystem.object_path);
            }
        }
        RemovalFactsOutcome::NotFound => println!("  the device is not found"),
        RemovalFactsOutcome::Error(error) => println!("  could not be read: {error}"),
    }
    true
}

fn parse_verify_mode(value: &str) -> Option<VerifyMode> {
    match value {
        "none" => Some(VerifyMode::None),
        "quick" => Some(VerifyMode::Quick),
        "full" => Some(VerifyMode::Full),
        _ => None,
    }
}

// A line from stdin, without its line ending; `None` on end of input or a
// read error.
fn prompt(text: &str) -> Option<String> {
    print!("{text}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim_end_matches(['\n', '\r']).to_string()),
    }
}

// The operation's steps, briefly (no device snapshots, no serial numbers);
// progress every 10%.
#[derive(Default)]
struct Progress {
    written: Option<u64>,
    verified: Option<u64>,
}

impl Progress {
    fn show(&mut self, event: &WorkerEvent) {
        let decile = |done: u64, total: u64| {
            if total == 0 {
                0
            } else {
                (u128::from(done) * 10 / u128::from(total)) as u64
            }
        };
        match event {
            WorkerEvent::TargetSelected {
                target, diskseq, ..
            } => println!("target selected: {} (diskseq {diskseq:?})", target.device),
            WorkerEvent::CompressedImageDetected { format } => {
                println!("compressed image: {format:?}")
            }
            WorkerEvent::ImageSelected { image_size } => println!("image: {image_size} bytes"),
            WorkerEvent::WriteGatePassed { .. } => println!("write gate passed"),
            WorkerEvent::OpeningDevice { purpose, .. } => {
                println!("opening the device ({})", name(*purpose))
            }
            WorkerEvent::DeviceOpened { purpose, metadata } => match metadata {
                Some(metadata) => println!(
                    "device opened ({}): {}:{} diskseq {:?}",
                    name(*purpose),
                    metadata.major,
                    metadata.minor,
                    metadata.diskseq
                ),
                None => println!("device opened ({})", name(*purpose)),
            },
            WorkerEvent::DeviceOpenFailed { purpose } => {
                println!("the device could not be opened ({})", name(*purpose))
            }
            WorkerEvent::WriteStarted => println!("write started"),
            WorkerEvent::WriteProgress(progress) => {
                let step = decile(progress.bytes_written, progress.total_bytes);
                if self.written != Some(step) {
                    self.written = Some(step);
                    println!(
                        "written {} / {} bytes",
                        progress.bytes_written, progress.total_bytes
                    );
                }
            }
            WorkerEvent::SyncStarted => println!("sync started"),
            WorkerEvent::SyncSucceeded { .. } => println!("sync done"),
            WorkerEvent::VerifyStarted => println!("verify started"),
            WorkerEvent::VerifyProgress(progress) => {
                let step = decile(progress.verified_bytes, progress.total_bytes);
                if self.verified != Some(step) {
                    self.verified = Some(step);
                    println!(
                        "verified {} / {} bytes",
                        progress.verified_bytes, progress.total_bytes
                    );
                }
            }
            _ => {}
        }
    }
}

fn name(purpose: OpenPurpose) -> &'static str {
    match purpose {
        OpenPurpose::Write => "write",
        OpenPurpose::Verify => "verify",
    }
}

// How the operation ended, by name only (some outcomes carry device
// snapshots, which include the serial number).
fn summarize(outcome: &OperationOutcome) -> String {
    match outcome {
        OperationOutcome::Completed { verify, image_size } => format!(
            "completed ({:?}, {} of {image_size} bytes verified)",
            verify.mode, verify.verified_bytes
        ),
        OperationOutcome::Cancelled(at) => format!(
            "cancelled ({})",
            match at {
                CancelledAt::Preflight => "Preflight",
                CancelledAt::BeforeConfirmation => "BeforeConfirmation",
                CancelledAt::Confirmation => "Confirmation",
                CancelledAt::Write { .. } => "Write",
                CancelledAt::AfterSync => "AfterSync",
                CancelledAt::BeforeVerify => "BeforeVerify",
                CancelledAt::Verify { .. } => "Verify",
            }
        ),
        OperationOutcome::Failed(error) => format!(
            "failed ({})",
            match error {
                OperationError::Target(_) => "Target",
                OperationError::Image(_) => "Image",
                OperationError::Confirmation(_) => "Confirmation",
                OperationError::ConfirmationInputClosed => "ConfirmationInputClosed",
                OperationError::ConfirmationInputFailed(_) => "ConfirmationInputFailed",
                OperationError::WriteGate(_) => "WriteGate",
                OperationError::WriteDeviceRejected { .. } => "WriteDeviceRejected",
                OperationError::ImageBinding(_) => "ImageBinding",
                OperationError::ReaderOpen(_) => "ReaderOpen",
                OperationError::Write { .. } => "Write",
                OperationError::SyncWorkerPanicked { .. } => "SyncWorkerPanicked",
                OperationError::Sync { .. } => "Sync",
                OperationError::VerifyNotStarted(_) => "VerifyNotStarted",
                OperationError::Verify { .. } => "Verify",
            }
        ),
    }
}

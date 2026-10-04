// What the GUI says, in words: the library's typed answers turned into
// short text for the normal view (written in English and translated with
// gettext, see `i18n`), and their exact names for the technical details,
// which stay untranslated. Presentation only -- every decision is the
// library's.

use std::io;

use linux_image_writer::report::{CompressionFormat, ImageSourceError};
use linux_image_writer::{
    DeviceDisplay, ImageAccess, OpenPurpose, RiskLevel, RiskReason, VerifyMode,
    VerifyUnavailableReason, WorkerConfirmationRequest,
};

use crate::i18n::{fill, ntr, tr};
use crate::model::{ClearReason, NoAvailableTarget, VerifyNotice, WriteBlocker};
use crate::operation::{Activity, Mark, Reason, Step, Tracker};
use crate::result::{RemovalNotice, RemovalStatus, ResultAction, ResultCase, ResultKind, StepLine};

// ---- Sizes ----

// A count of bytes, in words ("512 bytes").
fn bytes_text(count: u64, digits: &str) -> String {
    fill(
        ntr("{bytes} byte", "{bytes} bytes", count),
        &[("bytes", digits)],
    )
}

// A size as people read it (SI units, one decimal place).
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return bytes_text(bytes, &bytes.to_string());
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

// Progress as "done / total", both in the total's unit with two decimals
// (so the done part moves visibly).
pub fn transfer(done: u64, total: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if total < 1000 {
        return fill(
            ntr("{done} / {total} byte", "{done} / {total} bytes", total),
            &[("done", &done.to_string()), ("total", &total.to_string())],
        );
    }
    let mut divisor = 1000.0;
    let mut unit = 0;
    while total as f64 / divisor >= 1000.0 && unit < UNITS.len() - 1 {
        divisor *= 1000.0;
        unit += 1;
    }
    format!(
        "{:.2} {unit_name} / {:.2} {unit_name}",
        done as f64 / divisor,
        total as f64 / divisor,
        unit_name = UNITS[unit]
    )
}

// The exact byte count, for the technical details.
pub fn exact_bytes(bytes: u64) -> String {
    let digits = bytes.to_string();
    let mut grouped = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    bytes_text(bytes, &grouped)
}

// ---- Image ----

pub fn image_kind(compression: Option<CompressionFormat>) -> String {
    match compression {
        None => tr("Uncompressed image"),
        Some(CompressionFormat::Gzip) => tr("GZIP-compressed image"),
        Some(CompressionFormat::Xz) => tr("XZ-compressed image"),
    }
}

pub fn compression_name(compression: Option<CompressionFormat>) -> &'static str {
    match compression {
        None => "None",
        Some(CompressionFormat::Gzip) => "gzip",
        Some(CompressionFormat::Xz) => "xz",
    }
}

pub fn access_name(access: ImageAccess) -> &'static str {
    match access {
        ImageAccess::RandomAccess => "Random Access",
        ImageAccess::SequentialReplay => "Sequential Replay",
    }
}

// Why an image cannot be written, in the user's terms.
pub fn image_error(error: &ImageSourceError) -> String {
    match error {
        ImageSourceError::Io(error) => match error.kind() {
            io::ErrorKind::NotFound => tr("File not found"),
            io::ErrorKind::PermissionDenied => tr("No permission to read the file"),
            _ => tr("The file could not be read"),
        },
        ImageSourceError::NotRegularFile => {
            tr("Not a regular file (folders and devices cannot be chosen)")
        }
        ImageSourceError::UnsupportedFormat(kind) => fill(
            tr("{format}-compressed files are not supported"),
            &[("format", &kind.name().to_uppercase())],
        ),
        ImageSourceError::ExtensionMismatch { expected } => fill(
            tr("The file name indicates {format} compression, but the contents do not match"),
            &[("format", &expected.name().to_uppercase())],
        ),
    }
}

// ---- Targets ----

pub fn bus(connection_bus: &str) -> String {
    match connection_bus {
        "" => tr("Unknown connection"),
        "usb" => "USB".to_string(),
        other => other.to_uppercase(),
    }
}

// A device's name: vendor and model, or a fallback.
pub fn device_name(vendor: &str, model: &str) -> String {
    let name = [vendor.trim(), model.trim()]
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    if name.is_empty() {
        tr("Unnamed device")
    } else {
        name
    }
}

// One reason, as a sentence.
fn risk_reason(reason: &RiskReason) -> String {
    match reason {
        RiskReason::SystemDevice => tr("It is a system disk."),
        RiskReason::CriticalMount => tr("It is in use by the system (/, /boot or similar)."),
        RiskReason::ActiveSwap => tr("It is in use as swap."),
        RiskReason::ComplexStorage => tr("It is part of an LVM, RAID or encrypted setup."),
        RiskReason::MountedFilesystem => tr("It is mounted."),
        RiskReason::ReadOnly => tr("It is read-only."),
        RiskReason::IgnoredBySystem => tr("The system has set it to be hidden."),
        RiskReason::NotPartitionable => tr("It is a device that cannot be partitioned."),
        RiskReason::UsbRemovable => tr("It is a removable USB device."),
        RiskReason::UnknownOrNonRemovable => {
            tr("It could not be confirmed to be a removable USB device.")
        }
        RiskReason::MediaUnavailable => tr("No media is inserted."),
    }
}

// Why a protected device cannot be chosen: the Safety Engine's reasons as
// sentences, except the informational one a selectable device also carries.
pub fn protection(reasons: &[RiskReason]) -> String {
    reasons
        .iter()
        .filter(|reason| !matches!(reason, RiskReason::UsbRemovable))
        .map(risk_reason)
        .reduce(|sentences, next| {
            fill(
                tr("{sentences} {next_sentence}"),
                &[("sentences", &sentences), ("next_sentence", &next)],
            )
        })
        .unwrap_or_else(|| tr("It cannot be chosen, for safety."))
}

pub fn risk_level_name(level: &RiskLevel) -> &'static str {
    match level {
        RiskLevel::Normal => "Normal",
        RiskLevel::Caution => "Caution",
        RiskLevel::Blocked => "Blocked",
    }
}

// What the target section says when no drive can be chosen (title, line
// under it). Never a promise that a drive will become selectable: the
// Safety Engine decides that again on the next refresh.
pub fn no_available_target(state: NoAvailableTarget) -> (String, String) {
    match state {
        NoAvailableTarget::NoUsb => (
            tr("No USB drive found"),
            tr("Connect the USB drive to write to"),
        ),
        NoAvailableTarget::UsbInUse => (
            tr("The connected USB drive is in use and cannot be chosen"),
            tr("Unmount it in your file manager, then try again"),
        ),
        NoAvailableTarget::UsbProtected => (
            tr("The connected USB drive cannot be chosen as the target"),
            tr("See “Protected devices” for the reason"),
        ),
    }
}

// Why an image opened from outside (e.g. "Open with") was not taken:
// `processing` while the operation or Safe Removal runs, otherwise while its
// result is shown. The image stays as it was; nothing is queued.
pub fn open_refused(processing: bool) -> String {
    if processing {
        tr("Another image cannot be opened while processing. Try again once it has finished")
    } else {
        tr("Close the result view, then try again")
    }
}

// The detail (`CandidateListError`) goes to the technical details.
pub fn candidate_list_error_message() -> String {
    tr("The device list could not be read")
}

// Why the selection was cleared, when it needs saying. Choosing another
// drive was the user's own request: the empty selection says it all.
pub fn clear_reason(reason: ClearReason) -> Option<String> {
    match reason {
        ClearReason::Disappeared => Some(tr(
            "The selected USB drive can no longer be found. Choose the target again.",
        )),
        ClearReason::NoLongerSelectable => Some(tr(
            "The selected USB drive cannot be chosen at the moment. Choose the target again.",
        )),
        ClearReason::TooSmall => Some(tr(
            "The selected USB drive is too small for this image. Choose the target again.",
        )),
        ClearReason::ChooseAnother => None,
    }
}

// ---- Verify ----

pub fn verify_title(mode: VerifyMode) -> String {
    match mode {
        VerifyMode::Quick => tr("Quick verification"),
        VerifyMode::Full => tr("Full verification"),
        VerifyMode::None => tr("No verification"),
    }
}

pub fn verify_description(mode: VerifyMode) -> String {
    match mode {
        VerifyMode::Quick => tr("After writing, part of the data is read back and checked."),
        VerifyMode::Full => tr(
            "All of the written data is read back and checked.\nThis takes longer than quick verification.",
        ),
        VerifyMode::None => tr("The written data is not read back and checked."),
    }
}

// The one explanation shown under the Verify choices: the selected mode's,
// once an image is known.
pub fn verify_help(image_ready: bool, selected: VerifyMode) -> Option<String> {
    image_ready.then(|| verify_description(selected))
}

// Next to a mode the image does not allow.
pub fn verify_unavailable_short(reason: VerifyUnavailableReason) -> String {
    match reason {
        VerifyUnavailableReason::NeedsRandomAccess => tr("Not available for this image"),
    }
}

// Why (tooltip and accessible description of that mode).
pub fn verify_unavailable(reason: VerifyUnavailableReason) -> String {
    match reason {
        VerifyUnavailableReason::NeedsRandomAccess => tr(
            "Images in this format cannot be read from an arbitrary position, so quick verification, which reads back parts of the image, is not available",
        ),
    }
}

// What "available" means (technical details): a format check, not a
// promise that Verify will succeed on the device.
pub fn verify_availability_note() -> String {
    tr(
        "Based on the image format. Verification itself can still fail when it runs, depending on the state of the device",
    )
}

pub fn verify_unavailable_name(reason: VerifyUnavailableReason) -> &'static str {
    match reason {
        VerifyUnavailableReason::NeedsRandomAccess => "Needs Random Access",
    }
}

pub fn verify_notice(notice: VerifyNotice) -> String {
    fill(
        tr("{wanted} is not available for this image, so it was changed to {used}"),
        &[
            ("wanted", &verify_title(notice.wanted)),
            ("used", &verify_title(notice.used)),
        ],
    )
}

// ---- Write ----

// How much more capacity a drive would need.
pub fn shortfall(missing: u64) -> String {
    fill(
        tr("Not enough capacity: {size} more is needed"),
        &[("size", &size(missing))],
    )
}

pub fn write_blocker(blocker: WriteBlocker) -> String {
    match blocker {
        WriteBlocker::NoImage => tr("Choose an image to write"),
        WriteBlocker::ImageInspecting => tr("Checking the image…"),
        WriteBlocker::ImageInvalid => tr("This image cannot be written"),
        WriteBlocker::DeviceListUnavailable => {
            tr("The device list could not be read, so the target cannot be checked")
        }
        WriteBlocker::NoTarget => tr("Choose the USB drive to write to"),
        WriteBlocker::VerifyUnavailable => tr("This verification mode is not available"),
        WriteBlocker::TooSmall { shortfall: missing } => shortfall(missing),
    }
}

// ---- The operation ----

pub fn step_name(step: Step) -> String {
    match step {
        Step::Prepare => tr("Prepare"),
        Step::Write => tr("Write"),
        Step::Verify => tr("Verify"),
    }
}

pub fn mark_label(mark: Mark) -> String {
    match mark {
        Mark::Waiting => tr("Waiting"),
        Mark::Active => tr("Running"),
        Mark::Done => tr("Done"),
        // Under the step's own name ("Verify"), as on the result view.
        Mark::Skipped => tr("None"),
        Mark::Cancelled => tr("Cancelled"),
        Mark::Failed => tr("Failed"),
    }
}

pub fn mark_icon(mark: Mark) -> &'static str {
    match mark {
        Mark::Waiting => "content-loading-symbolic",
        Mark::Active => "media-playback-start-symbolic",
        Mark::Done => "emblem-ok-symbolic",
        // "Skip": neutral in Adwaita and Breeze alike. Breeze draws
        // `list-remove-symbolic` as a red cross, which reads as a failure.
        Mark::Skipped => "media-skip-forward-symbolic",
        Mark::Cancelled => "process-stop-symbolic",
        Mark::Failed => "dialog-error-symbolic",
    }
}

// The operation view's heading while it runs: the current phase.
pub fn headline(tracker: &Tracker) -> String {
    if tracker.cancel_requested {
        return tr("Cancelling");
    }
    match (tracker.step(), tracker.activity) {
        (Step::Prepare, _) => tr("Preparing to Write"),
        (Step::Write, Activity::Syncing) => tr("Finishing the Write"),
        (Step::Write, _) => tr("Writing to USB"),
        (Step::Verify, _) => match tracker.verify_mode {
            VerifyMode::Full => tr("Checking All of the Written Data"),
            _ => tr("Checking the Written Data"),
        },
    }
}

// What the operation is doing, and a line under it.
pub fn activity(tracker: &Tracker) -> (String, String) {
    let not_written = || tr("Nothing has been written to the USB drive yet.");
    let keep_connected = || tr("Do not remove the USB drive.");
    if tracker.cancel_requested {
        // The heading already says "Cancelling".
        let note = if tracker.step() == Step::Write {
            keep_connected()
        } else {
            String::new()
        };
        return (tr("Please wait until it stops."), note);
    }
    let authentication =
        || tr("If the system asks you to authenticate, complete the authentication.");
    match tracker.activity {
        Activity::Starting => (tr("Checking the target…"), not_written()),
        Activity::PreparingImage => (tr("Checking the image…"), not_written()),
        Activity::Preflight => (tr("Checking the compressed image…"), not_written()),
        Activity::AwaitingConfirmation => {
            (tr("Waiting for the final confirmation…"), not_written())
        }
        Activity::StartingWrite => (tr("Starting to write…"), keep_connected()),
        Activity::OpeningDevice(OpenPurpose::Write) => {
            (tr("Opening the USB drive…"), authentication())
        }
        Activity::OpeningDevice(OpenPurpose::Verify) => (
            tr("Opening the USB drive for verification…"),
            authentication(),
        ),
        Activity::Writing if tracker.compression.is_some() => {
            (tr("Decompressing and writing"), keep_connected())
        }
        Activity::Writing => (tr("Writing"), keep_connected()),
        Activity::Syncing => (
            tr("Waiting for the USB drive to finish writing…"),
            keep_connected(),
        ),
        Activity::PreparingVerify => (tr("Preparing verification…"), keep_connected()),
        Activity::Verifying => match tracker.verify_mode {
            VerifyMode::Full => (
                tr("Full verification in progress"),
                tr("Checking all of the written data."),
            ),
            _ => (
                tr("Quick verification in progress"),
                tr("Checking the written data."),
            ),
        },
        Activity::Finished => (String::new(), String::new()),
    }
}

// The line under the progress bar: the current phase's own progress, in
// bytes. Written-back bytes, not accepted ones, are "written to the USB
// drive"; while none is confirmed yet, the bytes read from the image are
// said as such. `None` when the phase has nothing measurable to say.
pub fn phase_detail(tracker: &Tracker) -> Option<String> {
    let pending = || tracker.pending_writeback().filter(|pending| *pending > 0);
    if tracker.cancel_requested {
        if tracker.step() != Step::Write {
            return None;
        }
        return Some(match pending() {
            Some(pending) => fill(
                tr("Safely stopping the write: about {size} left to write"),
                &[("size", &size(pending))],
            ),
            None => tr("Safely stopping the write"),
        });
    }
    match tracker.activity {
        Activity::Preflight => tracker.preflight.map(|t| transfer(t.done, t.total)),
        Activity::Writing => match (tracker.writeback, tracker.written) {
            (Some(writeback), _) => Some(fill(
                tr("Written to the USB drive: {amount}"),
                &[("amount", &transfer(writeback.done, writeback.total))],
            )),
            (None, Some(accepted)) => Some(fill(
                tr(
                    "Waiting for the USB drive to confirm the first data ({size} read from the image)",
                ),
                &[("size", &size(accepted.done))],
            )),
            (None, None) => None,
        },
        Activity::Syncing if tracker.synced => None,
        Activity::Syncing => Some(match pending() {
            Some(pending) => fill(
                tr("Finishing the last {size} or so"),
                &[("size", &size(pending))],
            ),
            None => tr("Confirming the write to the USB drive"),
        }),
        Activity::Verifying => tracker.verified.map(|verified| {
            fill(
                tr("Checking: {amount}"),
                &[("amount", &transfer(verified.done, verified.total))],
            )
        }),
        _ => None,
    }
}

// Why the window cannot be closed now: a heading and a line. Only asked
// while the operation runs.
pub fn cannot_close(tracker: &Tracker) -> (String, String) {
    if tracker.cancel_requested && tracker.step() == Step::Write {
        return (
            tr("Cancelling"),
            tr(
                "The write to the USB drive is being stopped safely. The window cannot be closed until it has finished.",
            ),
        );
    }
    if tracker.activity == Activity::Syncing && !tracker.cancel_requested {
        return (
            tr("Finishing the Write"),
            tr(
                "The rest of the data is being written to the USB drive. The window cannot be closed until it has finished.",
            ),
        );
    }
    (
        tr("Writing in Progress"),
        tr("The window cannot be closed until it has finished. To stop, press “Cancel”."),
    )
}

// ---- The result ----

pub fn result_title(case: ResultCase) -> String {
    match case {
        ResultCase::Verified(_) => tr("Complete"),
        ResultCase::WrittenWithoutVerify => tr("Writing Complete"),
        ResultCase::VerifyCancelled => tr("Writing Is Complete"),
        ResultCase::VerifyMismatch => tr("Verification Found a Problem"),
        ResultCase::VerifyNotCompleted(_) => tr("Verification Could Not Be Completed"),
        ResultCase::WriteFailed { .. } => tr("Writing Could Not Be Completed"),
        ResultCase::WriteCancelled { .. } | ResultCase::CancelledBeforeWrite => {
            tr("Writing Cancelled")
        }
        ResultCase::NotStarted(_) => tr("Writing Could Not Start"),
        ResultCase::Lost => tr("The Write Operation Could Not Be Completed"),
    }
}

// The same icon as a failed step on the result view.
pub fn result_icon(kind: ResultKind) -> &'static str {
    match kind {
        ResultKind::Success => "emblem-ok-symbolic",
        ResultKind::WrittenNotVerified => "dialog-information-symbolic",
        ResultKind::Cancelled => "process-stop-symbolic",
        ResultKind::Failed => "dialog-error-symbolic",
    }
}

// What one step says on the result view, under the step's name in the same
// row of steps as while the operation ran: a few words for its final state
// (what it means for the drive is the result message's). The operation has
// ended, so no step is ever "waiting" or "running" here: a step it never
// reached was not run ("None" for a Verify that was not requested).
pub fn result_step(case: ResultCase, line: StepLine) -> String {
    match line.mark {
        Mark::Done => match (line.step, case) {
            (Step::Verify, ResultCase::Verified(VerifyMode::Full)) => tr("Full verification done"),
            (Step::Verify, ResultCase::Verified(_)) => tr("Quick verification done"),
            _ => tr("Done"),
        },
        // Never reached (`Active` does not outlast an operation).
        Mark::Waiting | Mark::Active => tr("Not run"),
        Mark::Skipped => tr("None"),
        Mark::Cancelled => tr("Cancelled"),
        Mark::Failed => match (line.step, case) {
            (Step::Verify, ResultCase::VerifyMismatch) => tr("Mismatch"),
            (Step::Verify, ResultCase::VerifyNotCompleted(reason)) => {
                if verify_ran(reason) {
                    tr("Not completed")
                } else {
                    tr("Could not start")
                }
            }
            (Step::Write, ResultCase::WriteFailed { .. }) => tr("Not completed"),
            (Step::Write, ResultCase::NotStarted(_)) => tr("Could not start"),
            _ => tr("Failed"),
        },
    }
}

// The icon next to a step's final state on the result view: a step that
// was not run looks like one that was not requested, never like one still
// to come.
pub fn result_mark_icon(mark: Mark) -> &'static str {
    match mark {
        Mark::Waiting | Mark::Active => mark_icon(Mark::Skipped),
        other => mark_icon(other),
    }
}

// Whether Verify had started reading when it failed (otherwise it could
// not start).
fn verify_ran(reason: Reason) -> bool {
    matches!(
        reason,
        Reason::VerifyReadError
            | Reason::VerifyLengthMismatch
            | Reason::QuickVerifyUnsupported
            | Reason::ImageChanged
    )
}

// What the result means for the USB drive now, and what to do. Built only
// from the case's typed reasons, never from an error's own text.
pub fn result_message(case: ResultCase) -> String {
    let incomplete =
        || tr("The USB drive may contain an incomplete image. Do not use it as a boot drive.");
    let nothing_written = || tr("Nothing was written to the USB drive.");
    match case {
        ResultCase::Verified(_) => tr("The USB drive is ready to use."),
        ResultCase::WrittenWithoutVerify => tr("The written data was not verified."),
        ResultCase::VerifyCancelled => tr("Verification of the written data was not completed."),
        ResultCase::VerifyMismatch => tr(
            "Writing finished, but the data read back did not match the image. It may not have been written correctly, so using this USB drive as a boot drive is not recommended.",
        ),
        ResultCase::VerifyNotCompleted(reason) => format!(
            "{}\n{}",
            tr("Writing is complete, but verification could not be completed."),
            reason_text(reason)
        ),
        ResultCase::WriteFailed {
            reason,
            target_modified,
        } => format!(
            "{}\n{}",
            reason_text(reason),
            if target_modified {
                incomplete()
            } else {
                nothing_written()
            }
        ),
        ResultCase::WriteCancelled { target_modified } => {
            if target_modified {
                incomplete()
            } else {
                nothing_written()
            }
        }
        ResultCase::CancelledBeforeWrite => tr("Writing to the USB drive was not started."),
        ResultCase::NotStarted(reason) => format!("{}\n{}", reason_text(reason), nothing_written()),
        ResultCase::Lost => format!(
            "{}\n{}",
            tr("The operation ended because of an internal error."),
            incomplete()
        ),
    }
}

pub fn result_action(action: ResultAction) -> String {
    match action {
        ResultAction::SafeRemoval => tr("Safely Remove"),
        ResultAction::RetryRemoval => tr("Try Again"),
        ResultAction::WriteAnother => tr("Write to Another USB"),
        ResultAction::WriteAgain => tr("Write Again"),
        ResultAction::Retry => tr("Start Over"),
        ResultAction::BackToMain => tr("Back to Main View"),
        ResultAction::Done => tr("Done"),
    }
}

// ---- Safe Removal ----

// What the result view says about Safe Removal: a title, a message and, when
// the outcome says so, one extra line. Worded from the status only -- never
// from an error's own text, which may name object paths or serial numbers.
// Only `Removed` says the drive can be removed; a failure never claims that
// nothing was changed (filesystems may have been unmounted before it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalText {
    pub title: String,
    pub message: String,
    pub extra: Option<String>,
}

// `target_name`: the target as the operation's own events named it.
pub fn removal(notice: RemovalNotice, target_name: Option<&str>) -> RemovalText {
    let text = |title: String, message: String| RemovalText {
        title,
        message,
        extra: None,
    };
    let status = match notice {
        RemovalNotice::Removing => {
            return text(
                tr("Safely removing the USB drive…"),
                tr("Do not unplug the USB drive yet."),
            );
        }
        RemovalNotice::Finished(status) => status,
    };
    match status {
        RemovalStatus::Removed {
            unmounted_filesystems,
        } => RemovalText {
            title: tr("The USB drive can be safely removed"),
            message: match target_name {
                Some(name) => fill(tr("Unplug {name} from the computer."), &[("name", name)]),
                None => tr("Unplug the USB drive from the computer."),
            },
            extra: unmounted_filesystems.then(|| tr("Its filesystems were unmounted")),
        },
        RemovalStatus::DeviceGone => text(
            tr("USB drive not found"),
            tr("Make sure the USB drive is connected."),
        ),
        RemovalStatus::DeviceChanged => text(
            tr("The USB drive has changed"),
            tr(
                "It could not be confirmed to be the same device as when writing, so safe removal was stopped.",
            ),
        ),
        RemovalStatus::Unsupported => text(
            tr("This USB drive cannot be safely removed from this app"),
            tr("Eject it from a file manager or similar instead."),
        ),
        RemovalStatus::Busy => text(
            tr("The USB drive could not be removed"),
            tr(
                "The USB drive is being used by another app. Close any files or apps that use it, then try again.",
            ),
        ),
        RemovalStatus::NotAuthorized => text(
            tr("Safe removal could not be performed"),
            tr(
                "You are not allowed to do this. Eject the USB drive from a file manager or similar instead.",
            ),
        ),
        RemovalStatus::NotCompleted => text(
            tr("Safe removal could not be completed"),
            tr("Do not unplug the USB drive yet. Eject it from a file manager or similar instead."),
        ),
    }
}

// The icon next to the Safe Removal status (always with its title). Not the
// result's own icons for a failed write or Verify.
pub fn removal_icon(notice: RemovalNotice) -> Option<&'static str> {
    match notice {
        // A spinner is shown instead.
        RemovalNotice::Removing => None,
        RemovalNotice::Finished(status) => Some(match status {
            RemovalStatus::Removed { .. } => "emblem-ok-symbolic",
            RemovalStatus::DeviceGone | RemovalStatus::Unsupported => "dialog-information-symbolic",
            RemovalStatus::DeviceChanged
            | RemovalStatus::Busy
            | RemovalStatus::NotAuthorized
            | RemovalStatus::NotCompleted => "dialog-warning-symbolic",
        }),
    }
}

// The target as the operation and result views name it: its name and
// device node (never its serial number or object path).
pub fn target_summary(target: &DeviceDisplay) -> String {
    format!(
        "{} · {}",
        device_name(&target.vendor, &target.model),
        target.device
    )
}

pub fn reason_text(reason: Reason) -> String {
    match reason {
        Reason::TargetNotFound => tr("The target USB drive was not found."),
        Reason::TargetUnreadable => tr("Information about the target USB drive could not be read."),
        Reason::TargetChanged => {
            tr("The target USB drive could not be confirmed to be the same one that was selected.")
        }
        Reason::TargetNotSelectable => {
            tr("The target USB drive cannot be chosen as the target at the moment.")
        }
        Reason::ImageUnreadable => tr("The image file could not be read."),
        Reason::ImageRefused => tr("This image cannot be written."),
        Reason::QuickVerifyUnsupported => tr("Quick verification is not available for this image."),
        Reason::CompressedImageDamaged => tr("The compressed image is damaged or incomplete."),
        Reason::CompressedImageTooLarge => {
            tr("The decompressed size is larger than the capacity of the target USB drive.")
        }
        Reason::CompressedImageTooDemanding => tr(
            "This compressed image needs too many resources to decompress, so it cannot be written.",
        ),
        Reason::ImageChanged => tr("The image file changed while it was being checked."),
        Reason::ConfirmationFailed => {
            tr("Writing was not started because it did not match the final confirmation.")
        }
        Reason::TargetRecheckFailed => {
            tr("The check right before writing found a problem with the target USB drive.")
        }
        Reason::AccessDenied => tr("Access to the USB drive was not allowed."),
        Reason::AuthenticationCancelled => {
            tr("Authentication was cancelled, so the USB drive could not be opened.")
        }
        Reason::DeviceBusyOrRefused => tr("The USB drive could not be opened. It may be in use."),
        Reason::SystemServiceUnavailable => {
            tr("Could not communicate with the system's disk management service (UDisks2).")
        }
        Reason::OpenedDeviceMismatch => tr(
            "Writing was not started because the opened device could not be confirmed to be the target.",
        ),
        Reason::WriteError => tr("An error occurred while writing to the USB drive."),
        Reason::ImageChangedDuringWrite => tr("The image file changed during writing."),
        Reason::SyncError => {
            tr("The write could not be finished (the data could not be committed).")
        }
        Reason::VerifyTargetChanged => tr(
            "The check before verification could not confirm that the USB drive is the same as the target.",
        ),
        Reason::VerifyOpenFailed => tr("The USB drive could not be opened for verification."),
        Reason::VerifyDirectReadUnavailable => {
            tr("Direct reading, which verification needs, is not available in this environment.")
        }
        Reason::VerifyNotStarted => tr("Verification did not start."),
        Reason::VerifyMismatch => tr(
            "The written data did not match the image. This USB drive may not have been written correctly.",
        ),
        Reason::VerifyReadError => tr("An error occurred while reading back."),
        Reason::VerifyLengthMismatch => tr("The length of the data read back did not match."),
    }
}

// The final confirmation, built only from the worker's request (what the
// operation itself selected and opened) and the name of the file the
// request named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmationText {
    pub image_name: String,
    // (label, value) lines under the image name.
    pub image_lines: Vec<(String, String)>,
    pub target_name: String,
    pub target_line: String,
    pub verify: String,
    pub warning: String,
}

pub fn confirmation(
    request: &WorkerConfirmationRequest,
    image_name: &str,
    compression: Option<CompressionFormat>,
    compressed_size: Option<u64>,
) -> ConfirmationText {
    let target = &request.target;
    let target_name = device_name(&target.vendor, &target.model);
    let image_lines = match compression {
        None => vec![(tr("Size"), size(request.image_size))],
        Some(format) => {
            let mut lines = vec![(tr("Format"), image_kind(Some(format)))];
            if let Some(compressed) = compressed_size {
                lines.push((tr("Compressed file"), size(compressed)));
            }
            lines.push((tr("Write size"), size(request.image_size)));
            lines
        }
    };
    ConfirmationText {
        image_name: image_name.to_string(),
        image_lines,
        target_line: format!(
            "{} · {} · {}",
            target.device,
            size(target.size),
            bus(&target.connection_bus)
        ),
        warning: fill(
            tr("All data on {name} ({device}) will be erased."),
            &[("name", &target_name), ("device", &target.device)],
        ),
        target_name,
        verify: verify_title(request.verify_mode),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_image_writer::report::UnsupportedCompression;
    use linux_image_writer::{DeviceDisplay, RiskLevel, SafetyAssessment};

    #[test]
    fn sizes_read_naturally() {
        crate::i18n::ja(|| {
            assert_eq!(size(512), "512 バイト");
            assert_eq!(size(8_054_112_256), "8.1 GB");
            assert_eq!(size(620_000_000), "620.0 MB");
            assert_eq!(exact_bytes(8_054_112_256), "8,054,112,256 バイト");
            assert_eq!(exact_bytes(999), "999 バイト");
        });
    }

    #[test]
    fn image_refusals_are_explained_without_type_names() {
        let cases = [
            ImageSourceError::Io(io::Error::from(io::ErrorKind::NotFound)),
            ImageSourceError::Io(io::Error::from(io::ErrorKind::PermissionDenied)),
            ImageSourceError::Io(io::Error::other("x")),
            ImageSourceError::NotRegularFile,
            ImageSourceError::UnsupportedFormat(UnsupportedCompression::Zstd),
            ImageSourceError::ExtensionMismatch {
                expected: CompressionFormat::Gzip,
            },
        ];
        for error in &cases {
            let text = image_error(error);
            assert!(!text.is_empty());
            for type_name in [
                "Io",
                "NotRegularFile",
                "UnsupportedFormat",
                "ExtensionMismatch",
            ] {
                assert!(!text.contains(type_name), "{text}");
            }
        }
        assert!(image_error(&cases[4]).contains("ZSTD"));
    }

    #[test]
    fn only_the_selected_verify_mode_is_explained() {
        crate::i18n::ja(|| {
            assert_eq!(verify_help(false, VerifyMode::Quick), None);
            assert_eq!(
                verify_help(true, VerifyMode::Quick).as_deref(),
                Some("書き込み後、一部を読み戻して確認します。")
            );
            assert_eq!(
                verify_help(true, VerifyMode::Full).as_deref(),
                Some(
                    "書き込んだデータをすべて読み戻して確認します。\nクイック検証より時間がかかります。"
                )
            );
            assert_eq!(
                verify_help(true, VerifyMode::None).as_deref(),
                Some("書き込み後の読み戻し確認は行われません。")
            );
            // Each mode has its own explanation.
            let modes = [VerifyMode::Quick, VerifyMode::Full, VerifyMode::None];
            for a in modes {
                for b in modes {
                    assert_eq!(a == b, verify_help(true, a) == verify_help(true, b));
                }
            }
        });
    }

    #[test]
    fn a_protected_device_says_why() {
        crate::i18n::ja(|| {
            assert_eq!(
                protection(&[RiskReason::CriticalMount, RiskReason::SystemDevice]),
                "システム領域（/ や /boot など）が使用しています。システムのディスクです。"
            );
            // The informational reason alone explains nothing.
            assert_eq!(
                protection(&[RiskReason::UsbRemovable]),
                "安全のため選択できません。"
            );
        });
    }

    #[test]
    fn transfers_use_the_totals_unit() {
        crate::i18n::ja(|| {
            assert_eq!(transfer(1_150_000_000, 1_800_000_000), "1.15 GB / 1.80 GB");
            assert_eq!(transfer(0, 620_000_000), "0.00 MB / 620.00 MB");
            assert_eq!(transfer(12, 512), "12 / 512 バイト");
        });
    }

    // The request as the worker sends it: its values are the operation's
    // own, fresh ones.
    fn request(verify_mode: VerifyMode) -> WorkerConfirmationRequest {
        WorkerConfirmationRequest {
            target: DeviceDisplay {
                device: "/dev/sdq".to_string(),
                vendor: "Fresh".to_string(),
                model: "Drive".to_string(),
                serial: "S".to_string(),
                size: 16_000_000_000,
                connection_bus: "usb".to_string(),
                removable: true,
                read_only: false,
                media_available: true,
                mount_points: Vec::new(),
            },
            block_path: "/org/freedesktop/UDisks2/block_devices/sdq".to_string(),
            diskseq: Some(7),
            assessment: SafetyAssessment {
                risk_level: RiskLevel::Normal,
                writable: true,
                reasons: Vec::new(),
            },
            image_size: 3_800_000_000,
            verify_mode,
            expected_text: "/dev/sdq".to_string(),
        }
    }

    #[test]
    fn the_confirmation_shows_the_requests_values() {
        crate::i18n::ja(|| {
            let text = confirmation(&request(VerifyMode::Quick), "os.img", None, None);
            assert_eq!(text.image_name, "os.img");
            assert_eq!(
                text.image_lines,
                vec![("サイズ".to_string(), "3.8 GB".to_string())]
            );
            assert_eq!(text.target_name, "Fresh Drive");
            assert_eq!(text.target_line, "/dev/sdq · 16.0 GB · USB");
            assert_eq!(text.verify, "クイック検証");
            assert_eq!(
                text.warning,
                "Fresh Drive（/dev/sdq）のデータはすべて消去されます。"
            );
            // The serial number is not shown.
            assert!(!format!("{text:?}").contains("\"S\""));
        });
    }

    #[test]
    fn a_compressed_confirmation_shows_both_sizes() {
        crate::i18n::ja(|| {
            let text = confirmation(
                &request(VerifyMode::Full),
                "os.img.xz",
                Some(CompressionFormat::Xz),
                Some(620_000_000),
            );
            assert_eq!(
                text.image_lines,
                vec![
                    ("形式".to_string(), "XZ 圧縮イメージ".to_string()),
                    ("圧縮ファイル".to_string(), "620.0 MB".to_string()),
                    ("書き込みサイズ".to_string(), "3.8 GB".to_string()),
                ]
            );
            assert_eq!(text.verify, "完全検証");
        });
    }

    #[test]
    fn compressed_writes_say_they_decompress() {
        crate::i18n::ja(|| {
            let mut tracker = Tracker::new(VerifyMode::Full);
            tracker.activity = Activity::Writing;
            assert_eq!(activity(&tracker).0, "書き込み中");
            tracker.compression = Some(CompressionFormat::Gzip);
            assert_eq!(activity(&tracker).0, "展開しながら書き込み中");
            tracker.activity = Activity::Syncing;
            assert_eq!(
                activity(&tracker).0,
                "USB ドライブの書き込み完了を待っています…"
            );
            tracker.cancel_requested = true;
            assert_eq!(activity(&tracker).0, "処理が止まるまでお待ちください。");
        });
    }

    #[test]
    fn verify_is_worded_as_checking_the_written_data() {
        crate::i18n::ja(|| {
            let mut tracker = Tracker::new(VerifyMode::Quick);
            tracker.activity = Activity::Verifying;
            assert_eq!(
                activity(&tracker),
                (
                    "クイック検証中".to_string(),
                    "書き込んだデータを確認しています。".to_string()
                )
            );
            tracker.verify_mode = VerifyMode::Full;
            assert_eq!(
                activity(&tracker),
                (
                    "完全検証中".to_string(),
                    "書き込んだデータをすべて確認しています。".to_string()
                )
            );
            for text in [
                activity(&tracker).0,
                activity(&tracker).1,
                headline(&tracker),
            ] {
                assert!(!text.contains("安全"), "{text}");
            }
        });
    }

    // ---- current phase and phase detail ----

    fn tracker_at(mode: VerifyMode, activity: Activity) -> Tracker {
        let mut tracker = Tracker::new(mode);
        tracker.activity = activity;
        tracker
    }

    fn transfer_of(done: u64, total: u64) -> Option<crate::operation::Transfer> {
        Some(crate::operation::Transfer { done, total })
    }

    // The heading names the phase: preparing, writing, finishing, checking
    // (Quick or Full), cancelling.
    #[test]
    fn the_heading_names_the_current_phase() {
        crate::i18n::ja(|| {
            let heading = |mode, activity| headline(&tracker_at(mode, activity));
            assert_eq!(
                heading(VerifyMode::Quick, Activity::Starting),
                "書き込みを準備しています"
            );
            assert_eq!(
                heading(VerifyMode::Quick, Activity::Writing),
                "USB に書き込んでいます"
            );
            assert_eq!(
                heading(VerifyMode::Quick, Activity::Syncing),
                "書き込みを仕上げています"
            );
            assert_eq!(
                heading(VerifyMode::Quick, Activity::Verifying),
                "書き込んだデータを確認しています"
            );
            assert_eq!(
                heading(VerifyMode::Full, Activity::Verifying),
                "書き込んだデータをすべて確認しています"
            );
            let mut cancelling = tracker_at(VerifyMode::Quick, Activity::Writing);
            cancelling.cancel_requested = true;
            assert_eq!(headline(&cancelling), "中止しています");
        });
    }

    // Writing: written-back bytes are "written to the USB drive"; before
    // the first confirmation, the bytes read are said as read -- never as
    // written.
    #[test]
    fn writing_detail_says_written_back_bytes_only() {
        crate::i18n::ja(|| {
            let mut tracker = tracker_at(VerifyMode::Quick, Activity::Writing);
            assert_eq!(phase_detail(&tracker), None);
            tracker.written = transfer_of(30_000_000, 3_990_000_000);
            assert_eq!(
                phase_detail(&tracker).unwrap(),
                "USB への書き込みの確認を待っています（イメージから 30.0 MB 読み込み済み）"
            );
            tracker.written = transfer_of(2_900_000_000, 3_990_000_000);
            tracker.writeback = transfer_of(2_840_000_000, 3_990_000_000);
            assert_eq!(
                phase_detail(&tracker).unwrap(),
                "USB に書き込み済み: 2.84 GB / 3.99 GB"
            );
        });
    }

    // Finishing: what is left, when known; otherwise that the write is
    // being confirmed. The sync's own end says nothing more.
    #[test]
    fn finishing_detail_says_what_is_left() {
        crate::i18n::ja(|| {
            let mut tracker = tracker_at(VerifyMode::None, Activity::Syncing);
            tracker.written = transfer_of(3_990_000_000, 3_990_000_000);
            assert_eq!(
                phase_detail(&tracker).unwrap(),
                "USB への書き込みを確定しています"
            );
            tracker.writeback = transfer_of(3_949_000_000, 3_990_000_000);
            assert_eq!(
                phase_detail(&tracker).unwrap(),
                "残り約 41.0 MB を仕上げています"
            );
            tracker.writeback = transfer_of(3_990_000_000, 3_990_000_000);
            assert_eq!(
                phase_detail(&tracker).unwrap(),
                "USB への書き込みを確定しています"
            );
            tracker.synced = true;
            assert_eq!(phase_detail(&tracker), None);
        });
    }

    // Cancelling during the write: what is left to write before it stops,
    // when known -- never "cancelled" before the outcome says so.
    #[test]
    fn cancelling_detail_says_what_is_left_to_stop_safely() {
        crate::i18n::ja(|| {
            let mut tracker = tracker_at(VerifyMode::Quick, Activity::Writing);
            tracker.cancel_requested = true;
            assert_eq!(
                phase_detail(&tracker).unwrap(),
                "書き込みを安全に終了しています"
            );
            tracker.written = transfer_of(700_000_000, 3_990_000_000);
            tracker.writeback = transfer_of(659_000_000, 3_990_000_000);
            let detail = phase_detail(&tracker).unwrap();
            assert_eq!(detail, "残り約 41.0 MB の書き込みを安全に終了しています");
            assert!(!detail.contains("中止しました"));
            assert_eq!(headline(&tracker), "中止しています");
            // Outside the write, a cancellation has nothing to finish.
            let mut verifying = tracker_at(VerifyMode::Full, Activity::Verifying);
            verifying.cancel_requested = true;
            assert_eq!(phase_detail(&verifying), None);
        });
    }

    // Verify: the bytes checked, as reported.
    #[test]
    fn verify_detail_shows_the_bytes_checked() {
        crate::i18n::ja(|| {
            let mut tracker = tracker_at(VerifyMode::Quick, Activity::Verifying);
            tracker.verified = transfer_of(8_000_000, 12_000_000);
            assert_eq!(
                phase_detail(&tracker).unwrap(),
                "確認中: 8.00 MB / 12.00 MB"
            );
            tracker.verify_mode = VerifyMode::Full;
            tracker.verified = transfer_of(1_420_000_000, 3_990_000_000);
            assert_eq!(phase_detail(&tracker).unwrap(), "確認中: 1.42 GB / 3.99 GB");
        });
    }

    // Closing is refused with the reason that applies now.
    #[test]
    fn closing_is_refused_with_the_current_reason() {
        crate::i18n::ja(|| {
            let mut tracker = tracker_at(VerifyMode::Quick, Activity::Writing);
            assert_eq!(cannot_close(&tracker).0, "書き込み処理の実行中です");
            tracker.activity = Activity::Syncing;
            assert_eq!(
                cannot_close(&tracker),
                (
                    "書き込みを仕上げています".to_string(),
                    "残りのデータを USB ドライブに書き込んでいます。完了するまで、ウィンドウは閉じられません。".to_string()
                )
            );
            tracker.cancel_requested = true;
            assert_eq!(
                cannot_close(&tracker),
                (
                    "中止しています".to_string(),
                    "USB への書き込みを安全に終了しています。完了するまで、ウィンドウは閉じられません。".to_string()
                )
            );
        });
    }

    #[test]
    fn preparing_says_nothing_was_written_yet() {
        crate::i18n::ja(|| {
            let tracker = Tracker::new(VerifyMode::Quick);
            assert_eq!(
                activity(&tracker).1,
                "まだ USB ドライブには書き込んでいません。"
            );
        });
    }

    fn every_case() -> Vec<ResultCase> {
        vec![
            ResultCase::Verified(VerifyMode::Quick),
            ResultCase::Verified(VerifyMode::Full),
            ResultCase::WrittenWithoutVerify,
            ResultCase::VerifyCancelled,
            ResultCase::VerifyMismatch,
            ResultCase::VerifyNotCompleted(Reason::VerifyReadError),
            ResultCase::VerifyNotCompleted(Reason::VerifyTargetChanged),
            ResultCase::WriteFailed {
                reason: Reason::WriteError,
                target_modified: true,
            },
            ResultCase::WriteFailed {
                reason: Reason::SyncError,
                target_modified: false,
            },
            ResultCase::WriteCancelled {
                target_modified: true,
            },
            ResultCase::WriteCancelled {
                target_modified: false,
            },
            ResultCase::CancelledBeforeWrite,
            ResultCase::NotStarted(Reason::AccessDenied),
            ResultCase::Lost,
        ]
    }

    // Everything the result view says for a case, in one string.
    fn everything_said(case: ResultCase) -> String {
        let mut said = format!("{}\n{}", result_title(case), result_message(case));
        for (step, mark) in [Step::Prepare, Step::Write, Step::Verify]
            .into_iter()
            .flat_map(|step| {
                [
                    Mark::Waiting,
                    Mark::Done,
                    Mark::Skipped,
                    Mark::Cancelled,
                    Mark::Failed,
                ]
                .map(|mark| (step, mark))
            })
        {
            said.push('\n');
            said.push_str(&result_step(case, StepLine { step, mark }));
        }
        said
    }

    fn line(step: Step, mark: Mark) -> StepLine {
        StepLine { step, mark }
    }

    #[test]
    fn success_says_what_was_verified() {
        crate::i18n::ja(|| {
            let quick = ResultCase::Verified(VerifyMode::Quick);
            assert_eq!(result_title(quick), "完了しました");
            assert_eq!(result_step(quick, line(Step::Write, Mark::Done)), "完了");
            assert_eq!(
                result_step(quick, line(Step::Verify, Mark::Done)),
                "クイック検証完了"
            );
            assert_eq!(result_message(quick), "USB ドライブを使用できます。");
            let full = ResultCase::Verified(VerifyMode::Full);
            assert_eq!(
                result_step(full, line(Step::Verify, Mark::Done)),
                "完全検証完了"
            );
        });
    }

    #[test]
    fn verify_none_never_claims_a_verified_or_usable_drive() {
        crate::i18n::ja(|| {
            let case = ResultCase::WrittenWithoutVerify;
            assert_eq!(result_title(case), "書き込みが完了しました");
            assert_eq!(result_step(case, line(Step::Verify, Mark::Skipped)), "なし");
            assert_eq!(result_message(case), "書き込み後の検証は行っていません。");
            let said = everything_said(case);
            for forbidden in ["検証済み", "使用できます", "正常", "安全"] {
                assert!(!said.contains(forbidden), "{forbidden}: {said}");
            }
        });
    }

    #[test]
    fn a_cancelled_verify_says_the_write_is_complete() {
        crate::i18n::ja(|| {
            let case = ResultCase::VerifyCancelled;
            assert_eq!(result_title(case), "書き込みは完了しています");
            assert_eq!(
                result_step(case, line(Step::Verify, Mark::Cancelled)),
                "中止"
            );
            assert_eq!(result_message(case), "書き込み後の検証は完了していません。");
            // Not worded as an error: the lines it actually shows.
            let said = [
                result_title(case).to_string(),
                result_message(case),
                result_step(case, line(Step::Write, Mark::Done)).to_string(),
                result_step(case, line(Step::Verify, Mark::Cancelled)).to_string(),
            ]
            .join("\n");
            for error_like in ["失敗", "エラー", "問題"] {
                assert!(!said.contains(error_like), "{said}");
            }
        });
    }

    #[test]
    fn a_mismatch_is_not_called_a_failed_write() {
        crate::i18n::ja(|| {
            let case = ResultCase::VerifyMismatch;
            assert_eq!(result_title(case), "検証で問題が見つかりました");
            assert_eq!(
                result_step(case, line(Step::Verify, Mark::Failed)),
                "不一致"
            );
            assert_eq!(result_step(case, line(Step::Write, Mark::Done)), "完了");
            let message = result_message(case);
            assert!(message.contains("おすすめしません"), "{message}");
            let said = everything_said(case);
            assert!(!said.contains("書き込み失敗"), "{said}");
            assert!(!said.contains("書き込みを完了できませんでした"), "{said}");
        });
    }

    #[test]
    fn a_verify_error_is_not_a_mismatch_and_the_write_is_complete() {
        crate::i18n::ja(|| {
            let during = ResultCase::VerifyNotCompleted(Reason::VerifyReadError);
            assert_eq!(result_title(during), "検証を完了できませんでした");
            assert_eq!(
                result_step(during, line(Step::Verify, Mark::Failed)),
                "完了できず"
            );
            assert!(result_message(during).starts_with("書き込みは完了しています"));
            assert!(!everything_said(during).contains("一致しませんでした"));
            // One that never started reading is not said to have run.
            let before = ResultCase::VerifyNotCompleted(Reason::VerifyOpenFailed);
            assert_eq!(
                result_step(before, line(Step::Verify, Mark::Failed)),
                "開始できず"
            );
        });
    }

    #[test]
    fn a_failed_or_cancelled_write_warns_about_an_incomplete_image() {
        crate::i18n::ja(|| {
            let failed = ResultCase::WriteFailed {
                reason: Reason::WriteError,
                target_modified: true,
            };
            assert_eq!(result_title(failed), "書き込みを完了できませんでした");
            assert!(result_message(failed).contains("不完全なイメージ"));
            assert!(
                result_message(failed).contains("起動用の USB ドライブとして使用しないでください")
            );

            let cancelled = ResultCase::WriteCancelled {
                target_modified: true,
            };
            assert_eq!(result_title(cancelled), "書き込みを中止しました");
            assert!(result_message(cancelled).contains("不完全なイメージ"));
            assert!(!everything_said(cancelled).contains("エラー"));
        });
    }

    #[test]
    fn a_cancellation_before_the_write_says_it_never_started() {
        crate::i18n::ja(|| {
            let case = ResultCase::CancelledBeforeWrite;
            assert_eq!(result_title(case), "書き込みを中止しました");
            assert_eq!(
                result_message(case),
                "USB ドライブへの書き込みは開始されていません。"
            );
        });
    }

    // No result says the drive can be removed (only a completed Safe
    // Removal may), nor that nothing was changed on it by removal.
    #[test]
    fn no_result_speaks_for_safe_removal() {
        crate::i18n::ja(|| {
            for case in every_case() {
                let said = everything_said(case);
                for forbidden in ["安全", "取り外", "電源", "何も変更"] {
                    assert!(!said.contains(forbidden), "{case:?}: {said}");
                }
            }
            assert_eq!(result_action(ResultAction::SafeRemoval), "安全に取り外す");
        });
    }

    // Result texts are built from typed reasons only: no type names, no
    // object paths, no serial numbers.
    #[test]
    fn results_show_no_raw_errors_or_identifiers() {
        for case in every_case() {
            let said = everything_said(case);
            for raw in ["Error", "/org/freedesktop", "UDisks2.", "Serial", "{", "::"] {
                assert!(!said.contains(raw), "{case:?}: {said}");
            }
        }
        let target = DeviceDisplay {
            device: "/dev/sdq".to_string(),
            vendor: "General".to_string(),
            model: "UDisk".to_string(),
            serial: "SERIAL-123".to_string(),
            size: 8_000_000_000,
            connection_bus: "usb".to_string(),
            removable: true,
            read_only: false,
            media_available: true,
            mount_points: Vec::new(),
        };
        assert_eq!(target_summary(&target), "General UDisk · /dev/sdq");
    }

    #[test]
    fn every_result_kind_has_its_own_icon() {
        let kinds = [
            ResultKind::Success,
            ResultKind::WrittenNotVerified,
            ResultKind::Cancelled,
            ResultKind::Failed,
        ];
        for a in kinds {
            for b in kinds {
                assert_eq!(a == b, result_icon(a) == result_icon(b));
            }
        }
    }

    // ---- Safe Removal ----

    fn every_status() -> Vec<RemovalStatus> {
        vec![
            RemovalStatus::Removed {
                unmounted_filesystems: false,
            },
            RemovalStatus::Removed {
                unmounted_filesystems: true,
            },
            RemovalStatus::DeviceGone,
            RemovalStatus::DeviceChanged,
            RemovalStatus::Unsupported,
            RemovalStatus::Busy,
            RemovalStatus::NotAuthorized,
            RemovalStatus::NotCompleted,
        ]
    }

    fn said(text: &RemovalText) -> String {
        format!(
            "{}\n{}\n{}",
            text.title,
            text.message,
            text.extra.as_deref().unwrap_or("")
        )
    }

    #[test]
    fn removed_names_the_drive_and_says_it_can_be_removed() {
        crate::i18n::ja(|| {
            let removed = |unmounted_filesystems, name| {
                removal(
                    RemovalNotice::Finished(RemovalStatus::Removed {
                        unmounted_filesystems,
                    }),
                    name,
                )
            };
            let text = removed(false, Some("General UDisk"));
            assert_eq!(text.title, "USB を安全に取り外せます");
            assert_eq!(
                text.message,
                "General UDisk をパソコンから取り外してください。"
            );
            assert_eq!(text.extra, None);
            assert_eq!(
                removed(false, None).message,
                "USB ドライブをパソコンから取り外してください。"
            );
            // Only when the outcome lists filesystems it unmounted.
            assert_eq!(
                removed(true, Some("General UDisk")).extra.as_deref(),
                Some("ファイルシステムを終了しました")
            );
        });
    }

    #[test]
    fn removing_says_not_to_remove_yet() {
        crate::i18n::ja(|| {
            let text = removal(RemovalNotice::Removing, Some("General UDisk"));
            assert_eq!(text.title, "USB を安全に取り外しています…");
            assert_eq!(text.message, "USB ドライブはまだ取り外さないでください。");
            assert_eq!(removal_icon(RemovalNotice::Removing), None);
        });
    }

    #[test]
    fn each_failure_reads_as_specified() {
        crate::i18n::ja(|| {
            let finished = |status| removal(RemovalNotice::Finished(status), Some("General UDisk"));
            assert_eq!(
                finished(RemovalStatus::DeviceGone).title,
                "USB が見つかりません"
            );
            assert_eq!(
                finished(RemovalStatus::DeviceGone).message,
                "USB ドライブが接続されていることを確認してください。"
            );
            assert_eq!(
                finished(RemovalStatus::DeviceChanged).title,
                "USB の状態が変わりました"
            );
            assert!(
                finished(RemovalStatus::DeviceChanged)
                    .message
                    .contains("同じデバイス")
            );
            assert_eq!(
                finished(RemovalStatus::Unsupported).title,
                "この USB はアプリから安全に取り外せません"
            );
            assert_eq!(
                finished(RemovalStatus::Unsupported).message,
                "ファイルマネージャーなどから取り外してください。"
            );
            assert_eq!(
                finished(RemovalStatus::Busy).title,
                "USB を取り外せませんでした"
            );
            assert!(
                finished(RemovalStatus::Busy)
                    .message
                    .contains("もう一度お試しください")
            );
            assert_eq!(
                finished(RemovalStatus::NotAuthorized).title,
                "安全な取り外しを実行できませんでした"
            );
            assert!(
                finished(RemovalStatus::NotAuthorized)
                    .message
                    .contains("権限がありません")
            );
            let not_completed = finished(RemovalStatus::NotCompleted);
            assert_eq!(not_completed.title, "安全な取り外しを完了できませんでした");
            assert!(not_completed.message.contains("まだ取り外さないでください"));
        });
    }

    // Only Removed says the drive can be removed; nothing overstates what
    // powering off means, and no failure claims nothing was changed (or
    // calls the drive faulty).
    #[test]
    fn only_removed_says_the_drive_can_be_removed() {
        crate::i18n::ja(|| {
            for status in every_status() {
                let text = said(&removal(
                    RemovalNotice::Finished(status),
                    Some("General UDisk"),
                ));
                let removed = matches!(status, RemovalStatus::Removed { .. });
                assert_eq!(text.contains("安全に取り外せます"), removed, "{status:?}");
                for forbidden in [
                    "電源を切りました",
                    "完全に安全",
                    "絶対",
                    "何も変更",
                    "何もしていません",
                    "Unmount",
                    "故障",
                    "異常",
                ] {
                    assert!(!text.contains(forbidden), "{status:?}: {text}");
                }
            }
            assert!(!said(&removal(RemovalNotice::Removing, None)).contains("取り外せます"));
        });
    }

    // Worded from the status alone: no error text, object path, stage or
    // serial number can reach it.
    #[test]
    fn removal_texts_show_no_raw_errors_or_identifiers() {
        for status in every_status() {
            let text = said(&removal(
                RemovalNotice::Finished(status),
                Some("General UDisk"),
            ));
            for raw in [
                "Error", "/org/", "UDisks2", "PowerOff", "diskseq", "SERIAL", "::", "{",
            ] {
                assert!(!text.contains(raw), "{status:?}: {text}");
            }
        }
    }

    // Removed has its own icon, apart from every failure.
    #[test]
    fn removal_icons_tell_success_from_failure() {
        let removed = removal_icon(RemovalNotice::Finished(RemovalStatus::Removed {
            unmounted_filesystems: false,
        }));
        for status in every_status() {
            if !matches!(status, RemovalStatus::Removed { .. }) {
                assert_ne!(removal_icon(RemovalNotice::Finished(status)), removed);
            }
        }
    }

    #[test]
    fn no_available_target_says_what_to_do_without_promises() {
        crate::i18n::ja(|| {
            assert_eq!(
                no_available_target(NoAvailableTarget::NoUsb),
                (
                    "USB ドライブが見つかりません".to_string(),
                    "書き込み先の USB ドライブを接続してください".to_string()
                )
            );
            for state in [NoAvailableTarget::UsbInUse, NoAvailableTarget::UsbProtected] {
                let (title, line) = no_available_target(state);
                let said = format!("{title}\n{line}");
                for forbidden in [
                    "見つかりません",
                    "接続してください",
                    "取り外",
                    "選べるようになります",
                    "選べます",
                ] {
                    assert!(!said.contains(forbidden), "{state:?}: {said}");
                }
            }
            let (title, line) = no_available_target(NoAvailableTarget::UsbProtected);
            assert!(!format!("{title}{line}").contains("マウント"));
            let (_, line) = no_available_target(NoAvailableTarget::UsbInUse);
            assert!(line.contains("マウントを解除"));
        });
    }

    #[test]
    fn an_image_opened_meanwhile_is_refused_with_the_reason() {
        crate::i18n::ja(|| {
            let processing = open_refused(true);
            let result = open_refused(false);
            assert_ne!(processing, result);
            assert!(processing.contains("処理中"), "{processing}");
            assert!(!result.contains("処理中"), "{result}");
            assert!(result.contains("結果画面"), "{result}");
            // Nothing was switched, opened or queued.
            for text in [processing, result] {
                for misleading in ["切り替え", "開きました", "変更しました", "後で", "自動"]
                {
                    assert!(!text.contains(misleading), "{text}");
                }
            }
        });
    }

    #[test]
    fn marks_are_words() {
        crate::i18n::ja(|| {
            // Marks are words, never only an icon.
            for mark in [
                Mark::Waiting,
                Mark::Active,
                Mark::Done,
                Mark::Skipped,
                Mark::Cancelled,
                Mark::Failed,
            ] {
                assert!(!mark_label(mark).is_empty());
            }
            assert_eq!(mark_label(Mark::Skipped), "なし");
        });
    }

    #[test]
    fn a_skipped_verify_looks_neither_failed_nor_cancelled() {
        let marks = [
            Mark::Waiting,
            Mark::Active,
            Mark::Done,
            Mark::Skipped,
            Mark::Cancelled,
            Mark::Failed,
        ];
        // Every mark has its own icon.
        for a in marks {
            for b in marks {
                assert_eq!(a == b, mark_icon(a) == mark_icon(b), "{a:?} {b:?}");
            }
        }
        // Not a cross, a stop or an error sign (Breeze draws
        // `list-remove-symbolic` as a red cross).
        for failure_like in [
            "list-remove-symbolic",
            "process-stop-symbolic",
            "dialog-error-symbolic",
            "window-close-symbolic",
        ] {
            assert_ne!(mark_icon(Mark::Skipped), failure_like);
        }
    }

    // ---- The steps on the result view ----

    use crate::operation::Ending;
    use crate::result::{RemovalPresentation, view};

    const ALL_MODES: [VerifyMode; 3] = [VerifyMode::Quick, VerifyMode::Full, VerifyMode::None];

    fn every_ending() -> Vec<Ending> {
        let mut endings = vec![
            Ending::Verified(VerifyMode::Quick),
            Ending::Verified(VerifyMode::Full),
            Ending::WrittenWithoutVerify,
            Ending::CancelledBeforeWrite,
            Ending::CancelledDuringWrite {
                target_modified: true,
            },
            Ending::CancelledDuringWrite {
                target_modified: false,
            },
            Ending::CancelledDuringVerify,
            Ending::WriteFailed {
                reason: Reason::WriteError,
                target_modified: true,
            },
            Ending::VerifyFailed {
                reason: Reason::VerifyMismatch,
            },
            Ending::VerifyFailed {
                reason: Reason::VerifyReadError,
            },
            Ending::VerifyFailed {
                reason: Reason::VerifyOpenFailed,
            },
            Ending::Lost,
        ];
        for at in [Step::Prepare, Step::Write] {
            endings.push(Ending::NotStarted {
                at,
                reason: Reason::AccessDenied,
            });
        }
        endings
    }

    // The three steps as the result view shows them: (words, icon).
    fn result_steps(ending: Ending, mode: VerifyMode) -> [(String, &'static str); 3] {
        let shown =
            view(ending, mode, &RemovalPresentation::<()>::Unavailable).expect("a result view");
        shown
            .steps
            .map(|line| (result_step(shown.case, line), result_mark_icon(line.mark)))
    }

    #[test]
    fn a_result_never_shows_a_step_as_still_to_come_or_running() {
        for ending in every_ending() {
            for mode in ALL_MODES {
                for (words, icon) in result_steps(ending, mode) {
                    for in_progress in [mark_label(Mark::Waiting), mark_label(Mark::Active)] {
                        assert_ne!(words, in_progress, "{ending:?} {mode:?}");
                    }
                    for in_progress in [mark_icon(Mark::Waiting), mark_icon(Mark::Active)] {
                        assert_ne!(icon, in_progress, "{ending:?} {mode:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_step_never_reached_was_not_run() {
        crate::i18n::ja(|| {
            // A write stopped by the user (the real Cancellation Case).
            let steps = result_steps(
                Ending::CancelledDuringWrite {
                    target_modified: true,
                },
                VerifyMode::Full,
            );
            assert_eq!(
                steps.clone().map(|(words, _)| words),
                ["完了", "中止", "未実施"]
            );
            assert_eq!(
                result_steps(Ending::CancelledBeforeWrite, VerifyMode::Quick)
                    .map(|(words, _)| words),
                ["中止", "未実施", "未実施"]
            );
            assert_eq!(
                result_steps(
                    Ending::NotStarted {
                        at: Step::Prepare,
                        reason: Reason::TargetNotFound,
                    },
                    VerifyMode::Full,
                )
                .map(|(words, _)| words),
                ["失敗", "未実施", "未実施"]
            );
            // Not run reads like not requested: neither still to come nor failed.
            assert_eq!(steps[2].1, mark_icon(Mark::Skipped));
        });
    }

    #[test]
    fn with_verify_none_an_unreached_verify_is_none_not_unrun() {
        crate::i18n::ja(|| {
            for ending in [
                Ending::WrittenWithoutVerify,
                Ending::CancelledBeforeWrite,
                Ending::CancelledDuringWrite {
                    target_modified: true,
                },
                Ending::WriteFailed {
                    reason: Reason::SyncError,
                    target_modified: true,
                },
            ] {
                assert_eq!(
                    result_steps(ending, VerifyMode::None)[2].0,
                    "なし",
                    "{ending:?}"
                );
            }
        });
    }

    #[test]
    fn each_final_step_state_has_its_own_short_words() {
        crate::i18n::ja(|| {
            let verify = |ending| result_steps(ending, VerifyMode::Full)[2].0.clone();
            assert_eq!(verify(Ending::Verified(VerifyMode::Full)), "完全検証完了");
            assert_eq!(
                result_steps(Ending::Verified(VerifyMode::Quick), VerifyMode::Quick)[2]
                    .0
                    .clone(),
                "クイック検証完了"
            );
            assert_eq!(verify(Ending::CancelledDuringVerify), "中止");
            assert_eq!(
                verify(Ending::VerifyFailed {
                    reason: Reason::VerifyMismatch
                }),
                "不一致"
            );
            assert_eq!(
                verify(Ending::VerifyFailed {
                    reason: Reason::VerifyReadError
                }),
                "完了できず"
            );
            assert_eq!(
                verify(Ending::VerifyFailed {
                    reason: Reason::VerifyOpenFailed
                }),
                "開始できず"
            );
            let write = |ending| result_steps(ending, VerifyMode::Full)[1].0.clone();
            assert_eq!(
                write(Ending::WriteFailed {
                    reason: Reason::WriteError,
                    target_modified: true,
                }),
                "完了できず"
            );
            assert_eq!(
                write(Ending::NotStarted {
                    at: Step::Write,
                    reason: Reason::AccessDenied,
                }),
                "開始できず"
            );
            assert_eq!(
                result_steps(Ending::Lost, VerifyMode::Full).map(|(words, _)| words),
                ["失敗"; 3]
            );
        });
    }

    // Under each step's name, side by side: a few words, never a sentence
    // (the result message says the rest).
    #[test]
    fn result_step_words_are_short() {
        crate::i18n::ja(|| {
            for case in every_case() {
                for step in [Step::Prepare, Step::Write, Step::Verify] {
                    for mark in [
                        Mark::Waiting,
                        Mark::Active,
                        Mark::Done,
                        Mark::Skipped,
                        Mark::Cancelled,
                        Mark::Failed,
                    ] {
                        let words = result_step(case, StepLine { step, mark });
                        assert!(words.chars().count() <= 8, "{case:?} {step:?}: {words}");
                        for sentence_like in ["。", "（", "ました", "ません"] {
                            assert!(!words.contains(sentence_like), "{words}");
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn a_failed_result_has_the_same_icon_as_its_failed_step() {
        assert_eq!(result_icon(ResultKind::Failed), mark_icon(Mark::Failed));
        assert_eq!(
            result_icon(ResultKind::Cancelled),
            mark_icon(Mark::Cancelled)
        );
    }

    // ---- English, the source language ----

    fn has_japanese(text: &str) -> bool {
        text.chars().any(|c| matches!(c, '\u{3040}'..='\u{30ff}' | '\u{4e00}'..='\u{9fff}' | '\u{ff00}'..='\u{ffef}'))
    }

    // Without a catalog (LANG=C, or a language with no translation) the
    // program speaks English, and says the same things as in Japanese.
    #[test]
    fn english_is_complete_and_keeps_the_safety_wording() {
        for case in every_case() {
            let said = everything_said(case);
            assert!(!has_japanese(&said), "{case:?}: {said}");
            // No result speaks for Safe Removal.
            for forbidden in ["safe", "Safe", "unplug", "Unplug", "eject", "power"] {
                assert!(!said.contains(forbidden), "{case:?}: {said}");
            }
        }
        // Verify None never claims a verified or usable drive.
        let none = everything_said(ResultCase::WrittenWithoutVerify);
        for forbidden in ["ready to use", "verified successfully", "verification done"] {
            assert!(!none.contains(forbidden), "{none}");
        }
        assert_eq!(
            result_message(ResultCase::WrittenWithoutVerify),
            "The written data was not verified."
        );
        // A cancelled Verify is not an error; the write is complete.
        let cancelled_verify = format!(
            "{}\n{}",
            result_title(ResultCase::VerifyCancelled),
            result_message(ResultCase::VerifyCancelled)
        );
        for error_like in ["error", "Error", "failed", "Failed", "problem", "Problem"] {
            assert!(!cancelled_verify.contains(error_like), "{cancelled_verify}");
        }
        // A failed or cancelled write warns about an incomplete image.
        for case in [
            ResultCase::WriteFailed {
                reason: Reason::WriteError,
                target_modified: true,
            },
            ResultCase::WriteCancelled {
                target_modified: true,
            },
        ] {
            let message = result_message(case);
            assert!(message.contains("incomplete image"), "{message}");
            assert!(
                message.contains("Do not use it as a boot drive"),
                "{message}"
            );
        }
        assert_eq!(
            result_message(ResultCase::WriteCancelled {
                target_modified: false
            }),
            "Nothing was written to the USB drive."
        );
        // The final confirmation names the drive whose data will be erased.
        let text = confirmation(&request(VerifyMode::Quick), "os.img", None, None);
        assert_eq!(
            text.warning,
            "All data on Fresh Drive (/dev/sdq) will be erased."
        );
        assert_eq!(text.verify, "Quick verification");
        assert_eq!(
            text.image_lines,
            vec![("Size".to_string(), "3.8 GB".to_string())]
        );
        // Counts and sentences read naturally.
        assert_eq!(size(1), "1 byte");
        assert_eq!(size(512), "512 bytes");
        assert_eq!(exact_bytes(8_054_112_256), "8,054,112,256 bytes");
        assert_eq!(transfer(12, 512), "12 / 512 bytes");
        assert_eq!(
            protection(&[RiskReason::CriticalMount, RiskReason::SystemDevice]),
            "It is in use by the system (/, /boot or similar). It is a system disk."
        );
        assert_eq!(
            no_available_target(NoAvailableTarget::UsbInUse).1,
            "Unmount it in your file manager, then try again"
        );
    }

    #[test]
    fn in_english_only_removed_says_the_drive_can_be_removed() {
        for status in every_status() {
            let text = said(&removal(
                RemovalNotice::Finished(status),
                Some("General UDisk"),
            ));
            assert!(!has_japanese(&text), "{text}");
            let removed = matches!(status, RemovalStatus::Removed { .. });
            assert_eq!(
                text.contains("can be safely removed"),
                removed,
                "{status:?}"
            );
            for forbidden in [
                "powered off",
                "completely safe",
                "nothing was changed",
                "Nothing was changed",
                "Unmount",
                "failure",
                "malfunction",
            ] {
                assert!(!text.contains(forbidden), "{status:?}: {text}");
            }
        }
        let removing = said(&removal(RemovalNotice::Removing, None));
        assert!(!removing.contains("can be"), "{removing}");
        assert!(removing.contains("Do not unplug"), "{removing}");
        assert_eq!(
            removal(
                RemovalNotice::Finished(RemovalStatus::Removed {
                    unmounted_filesystems: false
                }),
                Some("General UDisk")
            )
            .message,
            "Unplug General UDisk from the computer."
        );
    }
}

// Development-only state preview: shows the window in a fixed state --
// the main view, or the operation view while writing, finishing,
// cancelling, verifying, or on a result -- without a USB drive, to check
// how each state looks. Compiled only into debug builds (`main.rs`:
// `#[cfg(debug_assertions)] mod preview;`), so a release build has neither
// the option nor this code.
//
//   linux-image-writer --preview <state> [--preview-theme light|dark]
//                      [--preview-screenshot FILE.png]
//
// Nothing here runs an operation: no worker is started, no device is
// listed, opened, written, synced, verified or removed. A state is a
// `Tracker` fed the same worker events a real operation would send (plus,
// for a result, its `Ending`), drawn by the window's normal code; the
// window refuses every action that would start real work while it shows a
// preview (`window::present_preview`).

use linux_image_writer::report::{VerifyProgress, WriteProgress, WritebackProgress};
use linux_image_writer::{DeviceDisplay, RiskLevel, SafetyAssessment, VerifyMode, WorkerEvent};

use crate::operation::{Ending, Reason, Tracker};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewState {
    Initial,
    ReadyToWrite,
    Preparing,
    WritingUnconfirmed,
    Writing,
    Finalizing,
    Cancelling,
    VerifyQuick,
    VerifyFull,
    Completed,
    Cancelled,
    Failed,
}

// Every state, by the name given on the command line.
pub const STATES: [(&str, PreviewState); 12] = [
    ("initial", PreviewState::Initial),
    ("ready", PreviewState::ReadyToWrite),
    ("preparing", PreviewState::Preparing),
    ("writing-unconfirmed", PreviewState::WritingUnconfirmed),
    ("writing", PreviewState::Writing),
    ("finalizing", PreviewState::Finalizing),
    ("cancelling", PreviewState::Cancelling),
    ("verify-quick", PreviewState::VerifyQuick),
    ("verify-full", PreviewState::VerifyFull),
    ("completed", PreviewState::Completed),
    ("cancelled", PreviewState::Cancelled),
    ("failed", PreviewState::Failed),
];

pub fn parse(name: &str) -> Option<PreviewState> {
    STATES
        .iter()
        .find(|(state_name, _)| *state_name == name)
        .map(|(_, state)| *state)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Light,
    Dark,
}

// What the command line asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub state: PreviewState,
    pub theme: Option<Theme>,
    // Save the window as a PNG here once drawn, then quit.
    pub screenshot: Option<std::path::PathBuf>,
}

// `Ok(None)` when the command line does not ask for a preview (the program
// then runs as usual); an error for a malformed preview request.
pub fn request_from_args(args: &[String]) -> Result<Option<Request>, String> {
    let Some(at) = args.iter().position(|arg| arg == "--preview") else {
        return Ok(None);
    };
    let names = STATES.map(|(name, _)| name).join(", ");
    let state = args
        .get(at + 1)
        .and_then(|name| parse(name))
        .ok_or_else(|| format!("--preview needs one of: {names}"))?;
    let value = |option: &str| {
        args.iter()
            .position(|arg| arg == option)
            .map(|at| args.get(at + 1).cloned())
    };
    let theme = match value("--preview-theme") {
        None => None,
        Some(Some(theme)) if theme == "light" => Some(Theme::Light),
        Some(Some(theme)) if theme == "dark" => Some(Theme::Dark),
        Some(_) => return Err("--preview-theme needs light or dark".to_string()),
    };
    let screenshot = match value("--preview-screenshot") {
        None => None,
        Some(Some(path)) => Some(path.into()),
        Some(None) => return Err("--preview-screenshot needs a file name".to_string()),
    };
    Ok(Some(Request {
        state,
        theme,
        screenshot,
    }))
}

// ---- What each state shows ----

// The image of every operation state: 1.8 GB, Quick or Full as the state
// says.
const IMAGE_SIZE: u64 = 1_800_000_000;
const MIB: u64 = 1024 * 1024;
const QUICK_VERIFY_BYTES: u64 = 12 * MIB;
pub const IMAGE_NAME: &str = "debian-13-amd64-netinst.iso";

pub enum Scene {
    // The main view. `ready`: an image and a target are chosen.
    Main { ready: bool },
    // Boxed: a tracker is far larger than the main view's flag.
    Operation(Box<OperationScene>),
}

pub struct OperationScene {
    pub tracker: Tracker,
    pub image_name: String,
    // A result: how it ended, its technical detail line, and whether Safe
    // Removal is offered (shown only; it does nothing in a preview).
    pub ended: Option<(Ending, String)>,
    pub removal_offered: bool,
}

pub fn scene(state: PreviewState) -> Scene {
    let running = |tracker| {
        Scene::Operation(Box::new(OperationScene {
            tracker,
            image_name: IMAGE_NAME.to_string(),
            ended: None,
            removal_offered: false,
        }))
    };
    let ended = |mut tracker: Tracker, ending: Ending, removal_offered: bool| {
        tracker.finished();
        Scene::Operation(Box::new(OperationScene {
            tracker,
            image_name: IMAGE_NAME.to_string(),
            ended: Some((ending, "Preview".to_string())),
            removal_offered,
        }))
    };
    match state {
        PreviewState::Initial => Scene::Main { ready: false },
        PreviewState::ReadyToWrite => Scene::Main { ready: true },
        PreviewState::Preparing => {
            let mut tracker = Tracker::new(VerifyMode::Quick);
            tracker.apply(&target_selected());
            running(tracker)
        }
        PreviewState::WritingUnconfirmed => {
            let mut tracker = writing(VerifyMode::Quick);
            tracker.apply(&accepted(40_000_000));
            running(tracker)
        }
        PreviewState::Writing => {
            let mut tracker = writing(VerifyMode::Quick);
            tracker.apply(&accepted(IMAGE_SIZE / 2 + 60 * MIB));
            tracker.apply(&written_back(IMAGE_SIZE / 2));
            running(tracker)
        }
        PreviewState::Finalizing => running(finalizing(VerifyMode::Quick)),
        PreviewState::Cancelling => {
            let mut tracker = writing(VerifyMode::Quick);
            let at = IMAGE_SIZE / 2;
            tracker.apply(&accepted(at));
            tracker.apply(&written_back(at - 41 * MIB));
            tracker.cancel_requested = true;
            tracker.apply(&WorkerEvent::CancelDrainStarted { bytes_written: at });
            running(tracker)
        }
        PreviewState::VerifyQuick => {
            let mut tracker = verifying(VerifyMode::Quick);
            tracker.apply(&verified(
                VerifyMode::Quick,
                QUICK_VERIFY_BYTES / 2,
                QUICK_VERIFY_BYTES,
            ));
            running(tracker)
        }
        PreviewState::VerifyFull => {
            let mut tracker = verifying(VerifyMode::Full);
            tracker.apply(&verified(VerifyMode::Full, IMAGE_SIZE / 2, IMAGE_SIZE));
            running(tracker)
        }
        PreviewState::Completed => {
            let mut tracker = verifying(VerifyMode::Quick);
            tracker.apply(&verified(
                VerifyMode::Quick,
                QUICK_VERIFY_BYTES,
                QUICK_VERIFY_BYTES,
            ));
            ended(tracker, Ending::Verified(VerifyMode::Quick), true)
        }
        PreviewState::Cancelled => {
            let mut tracker = writing(VerifyMode::Quick);
            tracker.apply(&accepted(IMAGE_SIZE / 2));
            tracker.apply(&written_back(IMAGE_SIZE / 2));
            tracker.cancel_requested = true;
            ended(
                tracker,
                Ending::CancelledDuringWrite {
                    target_modified: true,
                },
                true,
            )
        }
        PreviewState::Failed => {
            let mut tracker = writing(VerifyMode::Quick);
            tracker.apply(&accepted(IMAGE_SIZE / 3));
            ended(
                tracker,
                Ending::WriteFailed {
                    reason: Reason::WriteError,
                    target_modified: true,
                },
                false,
            )
        }
    }
}

// The target every operation state names: an example USB drive. Display
// data only, never a device reference.
pub fn example_target() -> DeviceDisplay {
    DeviceDisplay {
        device: "/dev/sdz".to_string(),
        vendor: "Example".to_string(),
        model: "USB Flash Drive".to_string(),
        serial: String::new(),
        size: 32_000_000_000,
        connection_bus: "usb".to_string(),
        removable: true,
        read_only: false,
        media_available: true,
        mount_points: Vec::new(),
    }
}

fn target_selected() -> WorkerEvent {
    WorkerEvent::TargetSelected {
        target: example_target(),
        block_path: "/org/freedesktop/UDisks2/block_devices/sdz".to_string(),
        diskseq: Some(42),
        assessment: SafetyAssessment {
            risk_level: RiskLevel::Normal,
            writable: true,
            reasons: Vec::new(),
        },
    }
}

fn accepted(done: u64) -> WorkerEvent {
    WorkerEvent::WriteProgress(WriteProgress {
        bytes_written: done,
        total_bytes: IMAGE_SIZE,
    })
}

fn written_back(done: u64) -> WorkerEvent {
    WorkerEvent::WritebackProgress(WritebackProgress {
        completed_bytes: done,
        total_bytes: IMAGE_SIZE,
    })
}

fn verified(mode: VerifyMode, done: u64, total: u64) -> WorkerEvent {
    WorkerEvent::VerifyProgress(VerifyProgress {
        mode,
        verified_bytes: done,
        total_bytes: total,
    })
}

// Approved and writing, nothing written back yet.
fn writing(mode: VerifyMode) -> Tracker {
    let mut tracker = Tracker::new(mode);
    tracker.apply(&target_selected());
    tracker.apply(&WorkerEvent::ImageSelected {
        image_size: IMAGE_SIZE,
    });
    tracker.confirmation_requested();
    tracker.answered(true);
    tracker.apply(&WorkerEvent::WriteStarted);
    tracker
}

// Every byte accepted, the last window (64 MiB) not yet written back, the
// final sync running.
fn finalizing(mode: VerifyMode) -> Tracker {
    let mut tracker = writing(mode);
    tracker.apply(&accepted(IMAGE_SIZE));
    tracker.apply(&written_back(IMAGE_SIZE - 64 * MIB));
    tracker.apply(&WorkerEvent::WriteSucceeded {
        bytes_written: IMAGE_SIZE,
        image_size: IMAGE_SIZE,
    });
    tracker.apply(&WorkerEvent::SyncStarted);
    tracker
}

// Synced, Verify running.
fn verifying(mode: VerifyMode) -> Tracker {
    let mut tracker = finalizing(mode);
    tracker.apply(&written_back(IMAGE_SIZE));
    tracker.apply(&WorkerEvent::SyncSucceeded {
        bytes_written: IMAGE_SIZE,
    });
    tracker.apply(&WorkerEvent::VerifyPending { mode });
    tracker.apply(&WorkerEvent::VerifyStarted);
    tracker
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::ja;
    use crate::operation::{Activity, CancelAction};
    use crate::{progress, text};

    fn operation(state: PreviewState) -> OperationScene {
        match scene(state) {
            Scene::Operation(scene) => *scene,
            Scene::Main { .. } => panic!("{state:?} is not an operation state"),
        }
    }

    fn percent(scene: &OperationScene) -> Option<u8> {
        progress::shown(
            &scene.tracker,
            scene.ended.as_ref().map(|(ending, _)| *ending),
        )
        .map(|overall| overall.percent())
    }

    // 1. A preview cannot run anything: this module names none of the
    // library's operations or device calls, and a scene holds no worker
    // (only a tracker and an ending).
    #[test]
    fn a_preview_cannot_start_real_work() {
        let source = include_str!("preview.rs");
        let code = source.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "spawn_write_worker",
            "WriteOperationRequest",
            "request_safe_removal",
            "RemovalTarget",
            "list_candidates",
            "inspect_image",
            "OpenDevice",
            "open_device",
        ] {
            assert!(!code.contains(forbidden), "{forbidden}");
        }
        // The window refuses the real actions while it shows a preview.
        let window = include_str!("window.rs");
        for guard in [
            "fn start_operation(ui: &Rc<Ui>) {\n    if ui.preview {",
            "fn result_action(ui: &Rc<Ui>, action: ResultAction) {\n    if ui.preview {",
            "fn refresh_targets(ui: &Rc<Ui>) {\n    if ui.preview {",
            "fn choose_image(ui: &Rc<Ui>) {\n    if ui.preview {",
        ] {
            assert!(window.contains(guard), "missing preview guard: {guard}");
        }
    }

    // 2. Every state has its name, and only those names parse.
    #[test]
    fn every_state_has_a_name() {
        for (name, state) in STATES {
            assert_eq!(parse(name), Some(state));
        }
        assert_eq!(parse("write"), None);
        assert_eq!(
            request_from_args(&["prog".into(), "image.iso".into()]),
            Ok(None)
        );
        assert_eq!(
            request_from_args(&[
                "prog".into(),
                "--preview".into(),
                "finalizing".into(),
                "--preview-theme".into(),
                "light".into(),
            ]),
            Ok(Some(Request {
                state: PreviewState::Finalizing,
                theme: Some(Theme::Light),
                screenshot: None,
            }))
        );
        assert!(request_from_args(&["prog".into(), "--preview".into()]).is_err());
        assert!(
            request_from_args(&["prog".into(), "--preview".into(), "nonsense".into()]).is_err()
        );
    }

    #[test]
    fn main_states_are_the_main_view() {
        assert!(matches!(
            scene(PreviewState::Initial),
            Scene::Main { ready: false }
        ));
        assert!(matches!(
            scene(PreviewState::ReadyToWrite),
            Scene::Main { ready: true }
        ));
    }

    // 3-10. Each operation state shows what the real one would: heading,
    // detail, overall progress (100% only for completed), Cancel.
    #[test]
    fn operation_states_show_what_the_real_ones_would() {
        ja(|| {
            let preparing = operation(PreviewState::Preparing);
            assert_eq!(
                text::headline(&preparing.tracker),
                "書き込みを準備しています"
            );

            let unconfirmed = operation(PreviewState::WritingUnconfirmed);
            assert_eq!(percent(&unconfirmed), Some(3));
            assert!(unconfirmed.tracker.overall().busy);
            assert!(
                text::phase_detail(&unconfirmed.tracker)
                    .unwrap()
                    .starts_with("USB への書き込みの確認を待っています")
            );

            let writing = operation(PreviewState::Writing);
            assert_eq!(text::headline(&writing.tracker), "USB に書き込んでいます");
            assert_eq!(
                text::phase_detail(&writing.tracker).unwrap(),
                "USB に書き込み済み: 0.90 GB / 1.80 GB"
            );
            assert_eq!(percent(&writing), Some(47));
            assert_eq!(writing.tracker.cancel_action(), CancelAction::AskFirst);

            let finalizing = operation(PreviewState::Finalizing);
            assert_eq!(
                text::headline(&finalizing.tracker),
                "書き込みを仕上げています"
            );
            assert_eq!(
                text::phase_detail(&finalizing.tracker).unwrap(),
                "USB への書き込みを確定しています"
            );
            assert_eq!(percent(&finalizing), Some(92));
            assert!(finalizing.tracker.overall().busy);

            let cancelling = operation(PreviewState::Cancelling);
            assert_eq!(text::headline(&cancelling.tracker), "中止しています");
            assert_eq!(
                text::phase_detail(&cancelling.tracker).unwrap(),
                "書き込みを安全に終了しています"
            );
            // 11. Cancel is disabled while cancelling.
            assert_eq!(
                cancelling.tracker.cancel_action(),
                CancelAction::Unavailable
            );
            assert!(cancelling.tracker.overall().busy);
            assert!(percent(&cancelling).unwrap() < 100);

            let quick = operation(PreviewState::VerifyQuick);
            assert_eq!(
                text::headline(&quick.tracker),
                "書き込んだデータを確認しています"
            );
            assert_eq!(quick.tracker.activity, Activity::Verifying);
            assert_eq!(percent(&quick), Some(99));
            let full = operation(PreviewState::VerifyFull);
            assert_eq!(
                text::headline(&full.tracker),
                "書き込んだデータをすべて確認しています"
            );
            assert_eq!(percent(&full), Some(90));

            // 12. Completed alone is 100%; cancelled and failed show none.
            let completed = operation(PreviewState::Completed);
            assert_eq!(percent(&completed), Some(100));
            assert!(completed.removal_offered);
            let cancelled = operation(PreviewState::Cancelled);
            assert_eq!(percent(&cancelled), None);
            assert!(!cancelled.ended.as_ref().unwrap().0.is_completed());
            let failed = operation(PreviewState::Failed);
            assert_eq!(percent(&failed), None);
            assert!(!failed.ended.as_ref().unwrap().0.is_completed());
        });
    }

    // The preview's example target is display data with no serial number.
    #[test]
    fn the_example_target_has_no_serial() {
        assert!(example_target().serial.is_empty());
    }
}

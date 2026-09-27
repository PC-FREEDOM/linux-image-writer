// The first half of a write (orchestration, Core layer): from the target
// reference to a confirmed intent, in four steps whose order the types fix.
//
//   select_write_target()        select + an immediate re-verification
//     -> SelectedWriteTarget
//   .prepare_image()             open the image once; a compressed image is
//     -> ImagePrepared            validated in full (Preflight) and the target
//                                 re-verified after it
//   .request_confirmation()      stop if cancellation was requested
//     -> PendingConfirmation      (what the user must be shown and must type)
//   .confirm(typed)              the typed text must match; then WriteIntent
//     -> PreparedOperation        and ConfirmationToken
//
// Each step consumes the previous value, so no step can be skipped or
// repeated, and the caller does its own user interaction (messages, the
// prompt, the Ctrl+C handler) between them. Nothing here prints or reads
// input. The existing primitives do all the deciding (`core::select`,
// `core::revalidate`, `prepare_compressed_image`, `core::WriteIntent`,
// `core::ConfirmationToken`); this module only runs them in order.
//
// `PreparedOperation` keeps the confirmation token, the `SelectedImage` and
// the selection to itself. The second half (fresh Write Gate, OpenDevice,
// FD binding, write, sync, Verify) is still driven by `main.rs`
// (`run_write_test`) through `fresh_gate()` and `bind()`, which use them
// without handing them out.

use super::candidates::{self, DeviceDisplay, SelectTargetError, TargetRef};
use super::confirmation_matches;
use super::image::{CompressedImageRejection, prepare_compressed_image};
use super::platform::Platform;
use crate::device::{DeviceSnapshot, SnapshotFetchOutcome};
use crate::execution::core::{
    self, AuthorizedWrite, ConfirmationToken, IntentBuildError, ReadyToOpen, SelectionState,
    VerifyMode, WriteGateError, WriteIntent,
};
use crate::execution::write_job::{AuthorizedExecution, CancelHandle, ImageBindingError};
use crate::image_source::compressed::PreflightProgress;
use crate::image_source::{
    self, CompressionFormat, ImageSource, ImageSourceError, OpenedImage, SelectedImage,
};
use crate::safety::SafetyAssessment;

// The selected target's baseline snapshot and assessment. Every value in
// this module that holds a `SelectionState` was built from one that passed
// `core::is_ready_to_open`, i.e. `Selected`.
fn selected(state: &SelectionState) -> (&DeviceSnapshot, &SafetyAssessment) {
    match state {
        SelectionState::Selected {
            baseline,
            baseline_assessment,
            ..
        } => (baseline, baseline_assessment),
        _ => unreachable!("only a state that passed is_ready_to_open is kept"),
    }
}

// ---- Step 1: the target ----

// A target that `core::select` accepted and that still passed an immediate
// re-verification (a second fresh snapshot, `core::revalidate`).
pub(crate) struct SelectedWriteTarget {
    state: SelectionState,
}

// Why there is no usable target.
#[derive(Debug)]
pub(crate) enum TargetNotReady {
    // Selecting it failed (see `candidates::select_target`); there is no
    // selection.
    Select(SelectTargetError),
    // It was selected, but the re-verification right after invalidated it.
    NotReady(SelectionState),
}

pub(crate) fn select_write_target(
    platform: &impl Platform,
    target: &TargetRef,
) -> Result<SelectedWriteTarget, TargetNotReady> {
    let mut state = candidates::select_target(platform, target).map_err(TargetNotReady::Select)?;

    if let SelectionState::Selected { baseline, .. } = &state {
        let outcome = platform.fetch_snapshot(&baseline.block_path);
        state = core::revalidate(state, outcome);
    }

    if !core::is_ready_to_open(&state) {
        return Err(TargetNotReady::NotReady(state));
    }

    Ok(SelectedWriteTarget { state })
}

impl SelectedWriteTarget {
    pub(crate) fn state(&self) -> &SelectionState {
        &self.state
    }

    // Opens `image_path` -- the one and only open of the image for this
    // operation: `open_image` opens it once, classifies it by content and
    // builds the source from that same open file. A compressed image is then
    // refused for Quick Verify, validated in full with its decoded size
    // bounded by the target's capacity (`prepare_compressed_image`;
    // `cancel` is checked during Preflight), and -- since that can take a
    // long time -- the target is re-verified (fresh snapshot; Identity /
    // Instance / Safety). A raw image needs neither. `on_compressed` is
    // called once a compressed image is recognised, before Preflight starts.
    //
    // Every refusal here happens before the target is opened.
    pub(crate) fn prepare_image(
        self,
        platform: &impl Platform,
        image_path: &str,
        verify_mode: VerifyMode,
        cancel: &CancelHandle,
        on_compressed: impl FnOnce(CompressionFormat),
        on_preflight_progress: impl FnMut(PreflightProgress),
    ) -> Result<ImagePrepared, PrepareImageError> {
        let SelectedWriteTarget { mut state } = self;
        let (baseline, _) = selected(&state);
        let target_capacity = baseline.size;
        let target_block_path = baseline.block_path.clone();

        let source: Box<dyn ImageSource> =
            match image_source::open_image(image_path).map_err(PrepareImageError::Image)? {
                OpenedImage::Raw(source) => Box::new(source),
                OpenedImage::Compressed(compressed) => {
                    on_compressed(compressed.format());
                    let source = prepare_compressed_image(
                        compressed,
                        verify_mode,
                        target_capacity,
                        || cancel.is_requested(),
                        on_preflight_progress,
                    )
                    .map_err(PrepareImageError::CompressedImage)?;

                    state = core::revalidate(state, platform.fetch_snapshot(&target_block_path));
                    if !core::is_ready_to_open(&state) {
                        return Err(PrepareImageError::TargetChanged(state));
                    }

                    Box::new(source)
                }
            };

        Ok(ImagePrepared {
            state,
            image: SelectedImage::new(source),
            verify_mode,
        })
    }
}

// Why `prepare_image` stopped. Nothing was opened on the target.
#[derive(Debug)]
pub(crate) enum PrepareImageError {
    // `open_image` refused or failed (including an unsupported format or a
    // name that does not match the content).
    Image(ImageSourceError),
    // A compressed image was refused: Quick Verify, Preflight (including a
    // cancellation during it), or the file changed during Preflight.
    CompressedImage(CompressedImageRejection),
    // The target no longer passes re-verification after Preflight; the
    // invalidated state says why.
    TargetChanged(SelectionState),
}

// ---- Step 2: the image is ready ----

pub(crate) struct ImagePrepared {
    state: SelectionState,
    image: SelectedImage,
    verify_mode: VerifyMode,
}

// A cancellation was requested before the confirmation.
#[derive(Debug)]
pub(crate) struct CancelledBeforeConfirmation;

impl ImagePrepared {
    pub(crate) fn logical_size(&self) -> u64 {
        self.image.logical_size()
    }

    // A cancellation may have arrived while the image was being opened.
    // Nothing on the target side has been opened yet, so stopping here is
    // unconditionally safe.
    pub(crate) fn request_confirmation(
        self,
        cancel: &CancelHandle,
    ) -> Result<PendingConfirmation, CancelledBeforeConfirmation> {
        if cancel.is_requested() {
            return Err(CancelledBeforeConfirmation);
        }

        Ok(PendingConfirmation {
            state: self.state,
            image: self.image,
            verify_mode: self.verify_mode,
        })
    }
}

// ---- Step 3: the human confirmation ----

// Waiting for the user to confirm. `request()` is what they must be shown
// and what they must type; `confirm()` is the only way on.
pub(crate) struct PendingConfirmation {
    state: SelectionState,
    image: SelectedImage,
    verify_mode: VerifyMode,
}

// What the user confirms, taken from the same re-verified selection and
// image the confirmation will be bound to.
pub(crate) struct ConfirmationRequest<'a> {
    pub(crate) target: DeviceDisplay,
    pub(crate) block_path: &'a str,
    pub(crate) diskseq: Option<u64>,
    pub(crate) assessment: &'a SafetyAssessment,
    pub(crate) image_size: u64,
    pub(crate) verify_mode: VerifyMode,
    // The text the user must type: the target's `/dev` node.
    pub(crate) expected_text: &'a str,
}

// Why `confirm()` did not produce a `PreparedOperation`. Either way
// nothing was opened on the target.
#[derive(Debug)]
pub(crate) enum ConfirmError {
    // The typed text is not the expected text (compared as
    // `confirmation_matches` does).
    Mismatch,
    // The confirmed intent could not be built.
    Intent(IntentBuildError),
}

impl PendingConfirmation {
    pub(crate) fn request(&self) -> ConfirmationRequest<'_> {
        let (baseline, assessment) = selected(&self.state);
        ConfirmationRequest {
            target: DeviceDisplay::from_snapshot(baseline),
            block_path: &baseline.block_path,
            diskseq: baseline.diskseq,
            assessment,
            image_size: self.image.logical_size(),
            verify_mode: self.verify_mode,
            expected_text: &baseline.device,
        }
    }

    // Accepts the confirmation only if `typed` matches the target's `/dev`
    // node (`confirmation_matches`: trimmed, then exact). Then freezes what
    // was confirmed -- target, selection, image, Verify mode -- into a
    // `WriteIntent` and a `ConfirmationToken`, which never leave the
    // returned `PreparedOperation`.
    pub(crate) fn confirm(self, typed: &str) -> Result<PreparedOperation, ConfirmError> {
        let (baseline, _) = selected(&self.state);
        if !confirmation_matches(typed, &baseline.device) {
            return Err(ConfirmError::Mismatch);
        }

        let intent =
            WriteIntent::from_selection(&self.state, self.image.selection(), self.verify_mode)
                .map_err(ConfirmError::Intent)?;
        let confirmation = ConfirmationToken::confirm(intent);

        Ok(PreparedOperation {
            state: self.state,
            image: self.image,
            verify_mode: self.verify_mode,
            confirmation,
        })
    }
}

// ---- Step 4: confirmed ----

// Everything the second half needs, held together: the selection, the
// image (the same `SelectedImage`, from the same open file), the Verify
// mode and the confirmation token bound to them. There is no way to take
// them out; the second half uses them through `fresh_gate()` and `bind()`.
pub(crate) struct PreparedOperation {
    state: SelectionState,
    image: SelectedImage,
    verify_mode: VerifyMode,
    confirmation: ConfirmationToken,
}

// What was confirmed, for display.
pub(crate) struct ConfirmedSummary<'a> {
    pub(crate) target_block_path: &'a str,
    pub(crate) image_size: u64,
    pub(crate) verify_mode: VerifyMode,
}

impl PreparedOperation {
    pub(crate) fn confirmed(&self) -> ConfirmedSummary<'_> {
        let intent = self.confirmation.intent();
        ConfirmedSummary {
            target_block_path: intent.target_block_path(),
            image_size: intent.image_size(),
            verify_mode: intent.verify_mode(),
        }
    }

    // The block path the fresh Write Gate snapshot must be read for.
    pub(crate) fn target_block_path(&self) -> &str {
        &selected(&self.state).0.block_path
    }

    // The fresh Write Gate (`core::prepare_for_open`) for this operation's
    // own selection, image, Verify mode and confirmation token. The caller
    // reads the fresh snapshot (for `target_block_path()`) immediately
    // before calling this.
    pub(crate) fn fresh_gate(
        &self,
        refreshed: SnapshotFetchOutcome,
    ) -> Result<ReadyToOpen, WriteGateError> {
        core::prepare_for_open(
            &self.state,
            refreshed,
            self.image.selection(),
            self.verify_mode,
            Some(&self.confirmation),
        )
    }

    // Binds the Gate-authorized write to this operation's image
    // (`AuthorizedExecution::bind`: image generation, size, Quick access).
    pub(crate) fn bind(
        self,
        authorized: AuthorizedWrite,
    ) -> Result<AuthorizedExecution, ImageBindingError> {
        AuthorizedExecution::bind(authorized, self.image)
    }

    #[cfg(test)]
    fn image(&self) -> &SelectedImage {
        &self.image
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_source::compressed::PreflightError;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io::Read as _;

    // Answers `fetch_snapshot` from a script, in order, and records every
    // block path asked for. Running out of answers is a test failure.
    struct ScriptedPlatform {
        answers: RefCell<VecDeque<SnapshotFetchOutcome>>,
        asked: RefCell<Vec<String>>,
    }

    impl ScriptedPlatform {
        fn new(answers: Vec<SnapshotFetchOutcome>) -> Self {
            ScriptedPlatform {
                answers: RefCell::new(answers.into()),
                asked: RefCell::new(Vec::new()),
            }
        }

        fn fetches(&self) -> usize {
            self.asked.borrow().len()
        }
    }

    impl Platform for ScriptedPlatform {
        fn fetch_snapshot(&self, block_path: &str) -> SnapshotFetchOutcome {
            self.asked.borrow_mut().push(block_path.to_string());
            self.answers
                .borrow_mut()
                .pop_front()
                .expect("an unexpected snapshot fetch")
        }
    }

    fn usb_stick() -> DeviceSnapshot {
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

    fn found(snapshot: DeviceSnapshot) -> SnapshotFetchOutcome {
        SnapshotFetchOutcome::Found(snapshot)
    }

    fn recreated() -> DeviceSnapshot {
        let mut snapshot = usb_stick();
        snapshot.diskseq = Some(13);
        snapshot
    }

    // A temporary image file, removed when dropped (if still there).
    struct TempImage(std::path::PathBuf);

    impl Drop for TempImage {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    impl TempImage {
        fn path(&self) -> &str {
            self.0.to_str().unwrap()
        }
    }

    fn temp_image(tag: &str, extension: &str, contents: &[u8]) -> TempImage {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "linux-usb-writer-operation-test-{tag}-{}-{id}.{extension}",
            std::process::id()
        ));
        std::fs::write(&path, contents).unwrap();
        TempImage(path)
    }

    fn payload() -> Vec<u8> {
        (0..300_000u32).map(|i| (i % 251) as u8).collect()
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn xz(data: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let stream =
            liblzma::stream::Stream::new_easy_encoder(0, liblzma::stream::Check::Crc64).unwrap();
        let mut encoder = liblzma::write::XzEncoder::new_stream(Vec::new(), stream);
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn cli_target() -> TargetRef {
        TargetRef::from_block_path(usb_stick().block_path)
    }

    fn selected_target(platform: &ScriptedPlatform) -> SelectedWriteTarget {
        select_write_target(platform, &cli_target()).expect("a selectable target")
    }

    fn prepare(
        platform: &ScriptedPlatform,
        image: &TempImage,
        mode: VerifyMode,
        cancel: &CancelHandle,
    ) -> Result<ImagePrepared, PrepareImageError> {
        selected_target(platform).prepare_image(
            platform,
            image.path(),
            mode,
            cancel,
            |_| {},
            |_| {},
        )
    }

    fn read_all(image: &SelectedImage) -> Vec<u8> {
        let mut bytes = Vec::new();
        image
            .open_reader()
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        bytes
    }

    // 21.1: a raw image goes all the way to a PreparedOperation, with the
    // same two snapshot reads `write-test` always made before the image
    // (select, then the immediate re-verification) and none after it.
    #[test]
    fn a_raw_image_is_prepared_through_every_step() {
        let data = payload();
        let image = temp_image("raw", "img", &data);
        let platform = ScriptedPlatform::new(vec![found(usb_stick()), found(usb_stick())]);
        let cancel = CancelHandle::new();

        let prepared = prepare(&platform, &image, VerifyMode::Full, &cancel).unwrap();
        assert_eq!(prepared.logical_size(), data.len() as u64);
        let pending = prepared.request_confirmation(&cancel).unwrap();

        let request = pending.request();
        assert_eq!(request.expected_text, "/dev/sdx");
        assert_eq!(request.block_path, usb_stick().block_path);
        assert_eq!(request.diskseq, Some(12));
        assert_eq!(request.target, DeviceDisplay::from_snapshot(&usb_stick()));
        assert_eq!(request.image_size, data.len() as u64);
        assert_eq!(request.verify_mode, VerifyMode::Full);
        assert!(request.assessment.writable);

        let operation = pending.confirm("/dev/sdx\n").unwrap();
        let confirmed = operation.confirmed();
        assert_eq!(confirmed.target_block_path, usb_stick().block_path);
        assert_eq!(confirmed.image_size, data.len() as u64);
        assert_eq!(confirmed.verify_mode, VerifyMode::Full);
        assert_eq!(operation.target_block_path(), usb_stick().block_path);
        assert_eq!(platform.fetches(), 2);
        assert_eq!(read_all(operation.image()), data);
    }

    // 21.1: a gzip and an xz image are validated in full, the target is
    // re-verified once after Preflight (a third snapshot read), and the
    // prepared image replays the decoded bytes.
    #[test]
    fn a_compressed_image_is_validated_and_the_target_rechecked() {
        let data = payload();
        for (extension, contents, format) in [
            ("img.gz", gzip(&data), CompressionFormat::Gzip),
            ("img.xz", xz(&data), CompressionFormat::Xz),
        ] {
            let image = temp_image("compressed", extension, &contents);
            let platform = ScriptedPlatform::new(vec![
                found(usb_stick()),
                found(usb_stick()),
                found(usb_stick()),
            ]);
            let cancel = CancelHandle::new();
            let mut recognised = None;
            let mut last_progress = None;

            let prepared = selected_target(&platform)
                .prepare_image(
                    &platform,
                    image.path(),
                    VerifyMode::Full,
                    &cancel,
                    |format| recognised = Some(format),
                    |progress| last_progress = Some(progress.logical_produced),
                )
                .unwrap();

            assert_eq!(recognised, Some(format));
            assert_eq!(last_progress, Some(data.len() as u64));
            assert_eq!(platform.fetches(), 3);
            let operation = prepared
                .request_confirmation(&cancel)
                .unwrap()
                .confirm("/dev/sdx")
                .unwrap();
            assert_eq!(operation.confirmed().image_size, data.len() as u64);
            assert_eq!(read_all(operation.image()), data);
        }
    }

    // 21.2: only the target's `/dev` node, trimmed, confirms -- exactly
    // `confirmation_matches`.
    #[test]
    fn only_the_exact_device_name_confirms() {
        let image = temp_image("confirm", "img", &payload());
        for (typed, accepted) in [
            ("/dev/sdx", true),
            ("/dev/sdx\n", true),
            ("/dev/sdx\r\n", true),
            ("  /dev/sdx  ", true),
            ("/dev/sdy", false),
            ("/dev/sdx1", false),
            ("/DEV/SDX", false),
            ("sdx", false),
            ("yes", false),
            ("y", false),
            ("", false),
            ("\n", false),
        ] {
            assert_eq!(
                confirmation_matches(typed, "/dev/sdx"),
                accepted,
                "{typed:?}"
            );
            let platform = ScriptedPlatform::new(vec![found(usb_stick()), found(usb_stick())]);
            let cancel = CancelHandle::new();
            let pending = prepare(&platform, &image, VerifyMode::None, &cancel)
                .unwrap()
                .request_confirmation(&cancel)
                .unwrap();
            match pending.confirm(typed) {
                Ok(_) => assert!(accepted, "{typed:?} must not confirm"),
                Err(ConfirmError::Mismatch) => assert!(!accepted, "{typed:?} must confirm"),
                Err(other) => panic!("{typed:?}: {other:?}"),
            }
        }
    }

    // 21.3: a target that changed during Preflight is refused before any
    // confirmation is requested; the invalidated state says why.
    #[test]
    fn a_target_changed_during_preflight_is_refused_before_confirmation() {
        let image = temp_image("changed", "img.gz", &gzip(&payload()));
        let platform = ScriptedPlatform::new(vec![
            found(usb_stick()),
            found(usb_stick()),
            found(recreated()),
        ]);
        let cancel = CancelHandle::new();

        match prepare(&platform, &image, VerifyMode::Full, &cancel) {
            Err(PrepareImageError::TargetChanged(SelectionState::Invalidated {
                reason: core::InvalidationReason::InstanceRecreated,
                ..
            })) => {}
            Err(other) => panic!("expected TargetChanged, got {other:?}"),
            Ok(_) => panic!("expected TargetChanged"),
        }

        let platform = ScriptedPlatform::new(vec![
            found(usb_stick()),
            found(usb_stick()),
            SnapshotFetchOutcome::NotFound,
        ]);
        assert!(matches!(
            prepare(&platform, &image, VerifyMode::Full, &cancel),
            Err(PrepareImageError::TargetChanged(
                SelectionState::Invalidated { .. }
            ))
        ));
    }

    // 21.4: a target picked from the candidate list must still be the same
    // device and instance when preparation starts. A CLI block-path
    // reference has nothing to compare, and a target invalidated by the
    // immediate re-verification is not ready either.
    #[test]
    fn the_target_must_still_be_the_one_selected() {
        let listed = candidates::tests_support::listed(usb_stick());

        let platform = ScriptedPlatform::new(vec![found(recreated())]);
        assert!(matches!(
            select_write_target(&platform, &listed),
            Err(TargetNotReady::Select(SelectTargetError::CandidateChanged(
                _
            )))
        ));
        assert_eq!(platform.fetches(), 1);

        let platform = ScriptedPlatform::new(vec![found(usb_stick()), found(usb_stick())]);
        assert!(select_write_target(&platform, &listed).is_ok());

        let platform = ScriptedPlatform::new(vec![found(recreated()), found(recreated())]);
        assert!(select_write_target(&platform, &cli_target()).is_ok());

        let platform = ScriptedPlatform::new(vec![found(usb_stick()), found(recreated())]);
        assert!(matches!(
            select_write_target(&platform, &cli_target()),
            Err(TargetNotReady::NotReady(SelectionState::Invalidated { .. }))
        ));

        let mut mounted_root = usb_stick();
        mounted_root.mount_points = vec!["/".to_string()];
        let platform = ScriptedPlatform::new(vec![found(mounted_root)]);
        assert!(matches!(
            select_write_target(&platform, &cli_target()),
            Err(TargetNotReady::Select(SelectTargetError::NotSelectable(_)))
        ));
    }

    // 21.5: cancellation stops the first half at exactly the two points it
    // always did -- during a compressed image's Preflight, and right after
    // the image is prepared -- and nowhere else: opening a raw image and
    // confirming do not check it.
    #[test]
    fn cancellation_is_checked_only_where_it_was_before() {
        let cancelled = || {
            let cancel = CancelHandle::new();
            cancel.request_cancel(crate::execution::write_job::CancelReason::UserRequested);
            cancel
        };

        let gz = temp_image("cancel", "img.gz", &gzip(&payload()));
        let platform = ScriptedPlatform::new(vec![found(usb_stick()), found(usb_stick())]);
        assert!(matches!(
            prepare(&platform, &gz, VerifyMode::Full, &cancelled()),
            Err(PrepareImageError::CompressedImage(
                CompressedImageRejection::Preflight(PreflightError::Cancelled)
            ))
        ));
        assert_eq!(
            platform.fetches(),
            2,
            "no re-check after a cancelled Preflight"
        );

        let raw = temp_image("cancel", "img", &payload());
        let platform = ScriptedPlatform::new(vec![found(usb_stick()), found(usb_stick())]);
        let cancel = cancelled();
        let prepared = prepare(&platform, &raw, VerifyMode::Full, &cancel)
            .expect("opening a raw image does not check cancellation");
        assert!(prepared.request_confirmation(&cancel).is_err());

        let platform = ScriptedPlatform::new(vec![found(usb_stick()), found(usb_stick())]);
        let cancel = CancelHandle::new();
        let pending = prepare(&platform, &raw, VerifyMode::Full, &cancel)
            .unwrap()
            .request_confirmation(&cancel)
            .unwrap();
        cancel.request_cancel(crate::execution::write_job::CancelReason::UserRequested);
        assert!(
            pending.confirm("/dev/sdx").is_ok(),
            "confirm() does not check cancellation (the CLI prompt does)"
        );
    }

    // 21.6: gzip and xz with Quick Verify are refused before Preflight
    // (nothing decoded, no progress, no target re-check); there is no
    // fallback to Full.
    #[test]
    fn a_compressed_image_with_quick_verify_is_refused_before_preflight() {
        for (extension, contents, format) in [
            ("img.gz", gzip(&payload()), CompressionFormat::Gzip),
            ("img.xz", xz(&payload()), CompressionFormat::Xz),
        ] {
            let image = temp_image("quick", extension, &contents);
            let platform = ScriptedPlatform::new(vec![found(usb_stick()), found(usb_stick())]);
            let cancel = CancelHandle::new();
            let mut progress_calls = 0;
            let result = selected_target(&platform).prepare_image(
                &platform,
                image.path(),
                VerifyMode::Quick,
                &cancel,
                |_| {},
                |_| progress_calls += 1,
            );
            match result {
                Err(PrepareImageError::CompressedImage(
                    CompressedImageRejection::QuickVerifyUnsupported(refused),
                )) => assert_eq!(refused, format),
                Err(other) => panic!("expected QuickVerifyUnsupported, got {other:?}"),
                Ok(_) => panic!("expected QuickVerifyUnsupported"),
            }
            assert_eq!(progress_calls, 0);
            assert_eq!(platform.fetches(), 2);
        }
    }

    // 21.7: the image is opened once; after that the path is never used
    // again. Removing the file from its path after preparation changes
    // nothing: the PreparedOperation still reads the original bytes (raw)
    // or replays the original stream (gzip) from the file opened first.
    #[test]
    fn the_prepared_image_is_the_file_opened_first() {
        let data = payload();
        for (extension, contents) in [("img", data.clone()), ("img.gz", gzip(&data))] {
            let image = temp_image("lifetime", extension, &contents);
            let platform = ScriptedPlatform::new(vec![
                found(usb_stick()),
                found(usb_stick()),
                found(usb_stick()),
            ]);
            let cancel = CancelHandle::new();
            let prepared = prepare(&platform, &image, VerifyMode::Full, &cancel).unwrap();

            std::fs::remove_file(&image.0).unwrap();
            std::fs::write(&image.0, b"a different file at the same path").unwrap();

            let operation = prepared
                .request_confirmation(&cancel)
                .unwrap()
                .confirm("/dev/sdx")
                .unwrap();
            assert_eq!(read_all(operation.image()), data);
            assert_eq!(operation.confirmed().image_size, data.len() as u64);
        }
    }

    // The unsupported / mismatched format refusals of `open_image` come
    // through unchanged, before any Preflight or re-check.
    #[test]
    fn image_open_refusals_come_through_unchanged() {
        let platform = ScriptedPlatform::new(vec![found(usb_stick()), found(usb_stick())]);
        let not_gzip = temp_image("mismatch", "img.gz", &payload());
        assert!(matches!(
            prepare(&platform, &not_gzip, VerifyMode::Full, &CancelHandle::new()),
            Err(PrepareImageError::Image(
                ImageSourceError::ExtensionMismatch { .. }
            ))
        ));
        assert_eq!(platform.fetches(), 2);
    }

    // The second half uses the confirmation through `fresh_gate()`: the
    // gate accepts the target it was confirmed for and refuses one that
    // changed since, and `bind()` pairs the authorization with this image.
    #[test]
    fn the_fresh_gate_uses_this_operations_confirmation() {
        let image = temp_image("gate", "img", &payload());
        let platform = ScriptedPlatform::new(vec![found(usb_stick()), found(usb_stick())]);
        let cancel = CancelHandle::new();
        let operation = prepare(&platform, &image, VerifyMode::Full, &cancel)
            .unwrap()
            .request_confirmation(&cancel)
            .unwrap()
            .confirm("/dev/sdx")
            .unwrap();

        assert!(operation.fresh_gate(found(usb_stick())).is_ok());
        assert!(matches!(
            operation.fresh_gate(found(recreated())),
            Err(WriteGateError::InstanceRecreated)
        ));
        assert!(matches!(
            operation.fresh_gate(SnapshotFetchOutcome::NotFound),
            Err(WriteGateError::SnapshotRefreshFailed)
        ));
    }

    // The order of the first half inside this module, checked on the
    // source: image open, then Preflight, then the target re-check, then
    // the image is bound into a SelectedImage; the typed text is compared
    // before the WriteIntent, and the token is minted from that intent.
    #[test]
    fn the_first_half_runs_in_order() {
        let source = include_str!("operation.rs");
        let source = &source[..source.find("\n#[cfg(test)]\nmod tests {").unwrap()];
        let steps = [
            "candidates::select_target(platform, target)",
            "core::revalidate(state, outcome)",
            "image_source::open_image(image_path)",
            "on_compressed(compressed.format())",
            "prepare_compressed_image(",
            "core::revalidate(state, platform.fetch_snapshot(&target_block_path))",
            "image: SelectedImage::new(source)",
            "if cancel.is_requested() {",
            "confirmation_matches(typed, &baseline.device)",
            "WriteIntent::from_selection(&self.state, self.image.selection(), self.verify_mode)",
            "ConfirmationToken::confirm(intent)",
            "core::prepare_for_open(",
            "AuthorizedExecution::bind(authorized, self.image)",
        ];
        let mut previous = 0;
        for step in steps {
            assert_eq!(source.matches(step).count(), 1, "{step}");
            let at = source.find(step).unwrap();
            assert!(at > previous, "{step} is out of order");
            previous = at;
        }
        // The image path is opened once and never again.
        assert_eq!(source.matches("open_image(").count(), 1);
        assert_eq!(source.matches("File::open").count(), 0);
    }
}

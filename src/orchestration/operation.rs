// A write operation (orchestration, Core layer): `run_write_operation` runs
// the whole safe sequence -- target, image, confirmation, Write Gate,
// OpenDevice, FD binding, write, sync, Verify -- in one fixed order, and a
// caller only observes it (`OperationObserver`) and reads the outcome.
//
// The first half is four steps whose order the types fix:
//
//   select_write_target()        select + an immediate re-verification
//     -> SelectedWriteTarget
//   .prepare_image()             open the image once; a compressed image is
//     -> ImagePrepared            validated in full (Preflight) and the target
//                                 re-verified after it
//   .request_confirmation()      stop if cancellation was requested
//     -> PendingConfirmation      (what the user must be shown and must type)
//   .confirm(typed)              the typed text must match -- or the user
//   / .approve()                  explicitly approved the request as shown;
//     -> PreparedOperation        then WriteIntent and ConfirmationToken
//
// Each step consumes the previous value, so no step can be skipped or
// repeated. `PreparedOperation` keeps the confirmation token, the
// `SelectedImage` and the selection to itself; the second half uses them
// through its private `fresh_gate()` and `bind()`, and continues with the
// existing `core` / `write_job` state machine (see `run_on`).
//
// Nothing here prints or reads input. The existing primitives do all the
// deciding (`core::select`, `core::revalidate`, `prepare_compressed_image`,
// `core::WriteIntent`, `core::ConfirmationToken`, `core::prepare_for_open`,
// `core::finalize_prepared_write`, `write_job`); this module only runs them
// in order and reports what happened.

use super::candidates::{self, DeviceDisplay, SelectTargetError, TargetRef};
use super::events::{ConfirmationDecision, OpenPurpose, OperationEvent, OperationObserver};
use super::image::{CompressedImageRejection, prepare_compressed_image};
use super::outcome::{CancelledAt, OperationError, OperationOutcome, VerifyNotStarted};
use super::platform::{LinuxPlatform, Platform};
use super::sync_worker::{OffMainThread, run_off_main_thread};
use super::{AfterSync, after_successful_sync, confirmation_matches};
use crate::device::{DeviceSnapshot, SnapshotFetchOutcome};
use crate::execution::core::{
    self, AuthorizedWrite, ConfirmationToken, IntentBuildError, ReadyToOpen, SelectionState,
    VerifyMode, WriteGateError, WriteIntent,
};
use crate::execution::linux_access::OpenAccess;
use crate::execution::write_job::{
    AuthorizedExecution, CancelDrainOutcome, CancelHandle, ImageBindingError, SyncAttemptOutcome,
    VerifyOutcome, VerifyStart, WriteAttemptOutcome,
};
use crate::image_source::compressed::PreflightError;
use crate::image_source::{self, ImageSource, ImageSourceError, OpenedImage, SelectedImage};
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
pub enum TargetNotReady {
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
    // Instance / Safety). A raw image needs neither. `on_event` receives
    // `CompressedImageDetected` once a compressed image is recognised
    // (before Preflight starts), then `PreflightProgress`.
    //
    // Every refusal here happens before the target is opened.
    pub(crate) fn prepare_image(
        self,
        platform: &impl Platform,
        image_path: &str,
        verify_mode: VerifyMode,
        cancel: &CancelHandle,
        mut on_event: impl FnMut(OperationEvent<'_>),
    ) -> Result<ImagePrepared, PrepareImageError> {
        let SelectedWriteTarget { mut state } = self;
        let (baseline, _) = selected(&state);
        let target_capacity = baseline.size;
        let target_block_path = baseline.block_path.clone();

        let source: Box<dyn ImageSource> =
            match image_source::open_image(image_path).map_err(PrepareImageError::Image)? {
                OpenedImage::Raw(source) => Box::new(source),
                OpenedImage::Compressed(compressed) => {
                    on_event(OperationEvent::CompressedImageDetected {
                        format: compressed.format(),
                    });
                    let source = prepare_compressed_image(
                        compressed,
                        verify_mode,
                        target_capacity,
                        || cancel.is_requested(),
                        |progress| on_event(OperationEvent::PreflightProgress(progress)),
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
pub enum PrepareImageError {
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
// and what they may type; `confirm()` (typed) and `approve()` (explicit
// approval) are the only ways on, and both end in the same place.
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
pub enum ConfirmError {
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
    // node (`confirmation_matches`: trimmed, then exact).
    pub(crate) fn confirm(self, typed: &str) -> Result<PreparedOperation, ConfirmError> {
        let (baseline, _) = selected(&self.state);
        if !confirmation_matches(typed, &baseline.device) {
            return Err(ConfirmError::Mismatch);
        }

        self.into_prepared()
    }

    // Accepts the user's explicit approval of `request()` as shown (a GUI's
    // button). It can only be given here, to a pending confirmation, and it
    // continues exactly as a matching typed confirmation does.
    pub(crate) fn approve(self) -> Result<PreparedOperation, ConfirmError> {
        self.into_prepared()
    }

    // The human confirmation was given: freezes what was confirmed --
    // target, selection, image, Verify mode -- into a `WriteIntent` and a
    // `ConfirmationToken`, which never leave the returned
    // `PreparedOperation`.
    fn into_prepared(self) -> Result<PreparedOperation, ConfirmError> {
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
// them out; `run_on` uses them through `fresh_gate()` and `bind()`.
pub(crate) struct PreparedOperation {
    state: SelectionState,
    image: SelectedImage,
    verify_mode: VerifyMode,
    confirmation: ConfirmationToken,
}

// What was confirmed, for display.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ConfirmedSummary<'a> {
    pub(crate) target_block_path: &'a str,
    pub(crate) image_size: u64,
    pub(crate) verify_mode: VerifyMode,
}

impl PreparedOperation {
    fn confirmed(&self) -> ConfirmedSummary<'_> {
        let intent = self.confirmation.intent();
        ConfirmedSummary {
            target_block_path: intent.target_block_path(),
            image_size: intent.image_size(),
            verify_mode: intent.verify_mode(),
        }
    }

    // The block path the fresh Write Gate snapshot must be read for.
    fn target_block_path(&self) -> &str {
        &selected(&self.state).0.block_path
    }

    // The fresh Write Gate (`core::prepare_for_open`) for this operation's
    // own selection, image, Verify mode and confirmation token. `run_on`
    // reads the fresh snapshot (for `target_block_path()`) immediately
    // before calling this.
    fn fresh_gate(&self, refreshed: SnapshotFetchOutcome) -> Result<ReadyToOpen, WriteGateError> {
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
    fn bind(self, authorized: AuthorizedWrite) -> Result<AuthorizedExecution, ImageBindingError> {
        AuthorizedExecution::bind(authorized, self.image)
    }

    #[cfg(test)]
    fn image(&self) -> &SelectedImage {
        &self.image
    }
}

// ---- The whole operation ----

// What a caller asks for. Only choices, never a safety value: the target is
// an opaque reference (re-read and re-verified before use), the image a
// path (opened once, here), the Verify mode a policy. Everything the
// sequence depends on -- snapshots, selection, confirmation token, FDs,
// authorizations -- is produced inside the operation.
/// What to write where: a listed target, an image path and a [`VerifyMode`].
/// Only choices -- the operation derives everything else itself.
pub struct WriteOperationRequest {
    pub(crate) target: TargetRef,
    pub(crate) image_path: String,
    pub(crate) verify_mode: VerifyMode,
}

impl WriteOperationRequest {
    /// A request to write the image at `image_path` to `target`, then
    /// verify it as `verify_mode` says. Nothing is opened or checked here;
    /// the operation does all of that, in order, once it runs.
    pub fn new(target: TargetRef, image_path: impl Into<String>, verify_mode: VerifyMode) -> Self {
        WriteOperationRequest {
            target,
            image_path: image_path.into(),
            verify_mode,
        }
    }
}

// Runs one write operation from start to finish on the calling thread and
// reports how it ended. `cancel` is checked only at the existing cancel
// points; the observer sees the steps as they happen and is asked for the
// typed confirmation.
//
// The CLI binary's entry point. The library's entry point is the worker
// (`worker::spawn_write_worker`), so this is unused in the library build.
#[allow(dead_code)]
pub(crate) fn run_write_operation(
    request: WriteOperationRequest,
    cancel: &CancelHandle,
    observer: &mut impl OperationObserver,
) -> OperationOutcome {
    run_on(&LinuxPlatform, request, cancel, observer)
}

// The sequence itself, with the platform as a parameter so tests can run it
// against temporary files. The order is fixed here and checked by
// `tests::the_production_path_runs_in_order`.
pub(super) fn run_on(
    platform: &impl Platform,
    request: WriteOperationRequest,
    cancel: &CancelHandle,
    observer: &mut impl OperationObserver,
) -> OperationOutcome {
    use OperationOutcome::{Cancelled, Completed, Failed};

    let WriteOperationRequest {
        target,
        image_path,
        verify_mode,
    } = request;

    // ---- target, image, confirmation (nothing is opened on the target) ----
    let selected_target = match select_write_target(platform, &target) {
        Ok(selected_target) => selected_target,
        Err(not_ready) => return Failed(OperationError::Target(not_ready)),
    };
    observer.on_event(OperationEvent::TargetSelected {
        state: selected_target.state(),
    });

    let image =
        match selected_target.prepare_image(platform, &image_path, verify_mode, cancel, |event| {
            observer.on_event(event)
        }) {
            Ok(image) => image,
            Err(PrepareImageError::CompressedImage(CompressedImageRejection::Preflight(
                PreflightError::Cancelled,
            ))) => return Cancelled(CancelledAt::Preflight),
            Err(error) => return Failed(OperationError::Image(error)),
        };
    observer.on_event(OperationEvent::ImageSelected {
        image_size: image.logical_size(),
    });

    let pending = match image.request_confirmation(cancel) {
        Ok(pending) => pending,
        Err(CancelledBeforeConfirmation) => return Cancelled(CancelledAt::BeforeConfirmation),
    };

    let confirmed = match observer.request_confirmation(&pending.request()) {
        ConfirmationDecision::Submitted(typed) => pending.confirm(&typed),
        ConfirmationDecision::Approved => pending.approve(),
        ConfirmationDecision::InputClosed => {
            return Failed(OperationError::ConfirmationInputClosed);
        }
        ConfirmationDecision::InputFailed(error) => {
            return Failed(OperationError::ConfirmationInputFailed(error));
        }
        ConfirmationDecision::Cancelled => return Cancelled(CancelledAt::Confirmation),
    };
    let operation = match confirmed {
        Ok(operation) => operation,
        Err(error) => return Failed(OperationError::Confirmation(error)),
    };
    observer.on_event(OperationEvent::Confirmed(operation.confirmed()));

    // ---- fresh Write Gate, OpenDevice(WriteExclusive), FD binding ----
    let refreshed = platform.fetch_snapshot(operation.target_block_path());
    let ready = match operation.fresh_gate(refreshed) {
        Ok(ready) => ready,
        Err(error) => return Failed(OperationError::WriteGate(error)),
    };
    observer.on_event(OperationEvent::WriteGatePassed { plan: ready.plan() });

    observer.on_event(OperationEvent::OpeningDevice {
        purpose: OpenPurpose::Write,
        block_path: &ready.current().block_path,
    });
    let (handle, metadata, open_error) =
        match platform.open_device(&ready.current().block_path, OpenAccess::WriteExclusive) {
            Ok(handle) => {
                let metadata = platform.fd_metadata(&handle);
                observer.on_event(OperationEvent::DeviceOpened {
                    purpose: OpenPurpose::Write,
                    metadata: metadata.as_ref(),
                });
                (Some(handle), metadata, None)
            }
            Err(error) => {
                observer.on_event(OperationEvent::DeviceOpenFailed {
                    purpose: OpenPurpose::Write,
                    error: &error,
                });
                (None, None, Some(error))
            }
        };

    // The snapshot the FD binding checks, kept for Safe Removal: it is what
    // `finalize_prepared_write` keeps as the write's baseline, and it is
    // handed on only if the binding passes and the write starts.
    let bound = ready.current().clone();
    let prepared = match core::finalize_prepared_write(ready, handle, metadata.as_ref()) {
        Ok(prepared) => prepared,
        Err(error) => {
            return Failed(OperationError::WriteDeviceRejected {
                error,
                open_device: open_error,
            });
        }
    };
    observer.on_event(OperationEvent::FdBound {
        purpose: OpenPurpose::Write,
    });
    observer.on_event(OperationEvent::WriteAuthorized {
        target_block_path: &prepared.target_block_path,
        target_size: prepared.target_size,
        image_size: prepared.image_size,
        verify_mode,
    });

    // ---- authorization -> image binding -> write ----
    let execution = match operation.bind(prepared.begin()) {
        Ok(execution) => execution,
        Err(error) => return Failed(OperationError::ImageBinding(error)),
    };
    observer.on_event(OperationEvent::ImageBound);

    let writing = match execution.begin_write(cancel.clone()) {
        Ok(writing) => writing,
        Err(error) => return Failed(OperationError::ReaderOpen(error)),
    };
    observer.on_event(OperationEvent::WriteStarted);
    observer.write_started_on(bound);

    let (image, write_outcome) =
        writing.write(|progress| observer.on_event(OperationEvent::WriteProgress(progress)));
    let succeeded = match write_outcome {
        WriteAttemptOutcome::Succeeded(succeeded) => succeeded,
        WriteAttemptOutcome::Failed(failed) => {
            return Failed(OperationError::Write {
                failed,
                image_size: image.logical_size(),
            });
        }
        // ---- cancelled: drain on the still-open FD, then close it ----
        // Like sync: on a worker thread, awaited to completion, never
        // skipped. `Cancelled` is returned only after the FD is closed.
        WriteAttemptOutcome::CancelRequested(drain) => {
            observer.on_event(OperationEvent::CancelDrainStarted {
                bytes_written: drain.bytes_written,
            });
            let drained = match run_off_main_thread(drain, |drain| drain.drain()) {
                OffMainThread::Finished(outcome) => outcome,
                OffMainThread::NotStarted(drain, error) => {
                    observer.on_event(OperationEvent::CancelDrainOnCallingThread { error: &error });
                    drain.drain()
                }
                OffMainThread::Panicked => {
                    return Failed(OperationError::CancelDrainWorkerPanicked);
                }
            };
            return match drained {
                CancelDrainOutcome::Drained(cancelled) => Cancelled(CancelledAt::Write {
                    cancelled,
                    image_size: image.logical_size(),
                }),
                CancelDrainOutcome::Failed(failed) => Failed(OperationError::CancelDrain {
                    failed,
                    image_size: image.logical_size(),
                }),
            };
        }
    };
    observer.on_event(OperationEvent::WriteSucceeded {
        bytes_written: succeeded.bytes_written,
        image_size: succeeded.image_size,
    });

    // ---- sync: on a worker thread, awaited to completion, never skipped ----
    observer.on_event(OperationEvent::SyncStarted);
    let sync_outcome = match run_off_main_thread(succeeded.begin_sync(), |syncing| syncing.sync()) {
        OffMainThread::Finished(outcome) => outcome,
        OffMainThread::NotStarted(syncing, error) => {
            observer.on_event(OperationEvent::SyncOnCallingThread { error: &error });
            syncing.sync()
        }
        OffMainThread::Panicked => {
            return Failed(OperationError::SyncWorkerPanicked {
                cancel_requested: cancel.is_requested(),
            });
        }
    };
    let synced = match sync_outcome {
        SyncAttemptOutcome::Succeeded(synced) => synced,
        SyncAttemptOutcome::Failed(failed) => {
            return Failed(OperationError::Sync {
                failed,
                cancel_requested: cancel.is_requested(),
                image_size: image.logical_size(),
            });
        }
    };
    observer.on_event(OperationEvent::SyncSucceeded {
        bytes_written: synced.bytes_written,
    });

    if after_successful_sync(cancel.is_requested()) == AfterSync::Cancelled {
        return Cancelled(CancelledAt::AfterSync);
    }

    // ---- Verify: `begin_verify` closes the write FD first ----
    let pending = match synced.begin_verify(image, cancel.clone()) {
        VerifyStart::Skipped(image, verify) => {
            return Completed {
                verify,
                image_size: image.logical_size(),
            };
        }
        VerifyStart::Pending(pending) => pending,
    };
    observer.on_event(OperationEvent::VerifyPending { mode: verify_mode });

    if !observer.pause_before_verify() {
        return Failed(OperationError::VerifyNotStarted(
            VerifyNotStarted::TestPauseEnded,
        ));
    }
    if cancel.is_requested() {
        return Cancelled(CancelledAt::BeforeVerify);
    }

    observer.on_event(OperationEvent::VerifySnapshotRequested {
        block_path: pending.block_path(),
    });
    let refreshed = platform.fetch_snapshot(pending.block_path());
    let ready = match pending.check_target(refreshed) {
        Ok(ready) => ready,
        Err((image, error, diagnostics)) => {
            return Failed(OperationError::VerifyNotStarted(
                VerifyNotStarted::TargetCheck {
                    error,
                    diagnostics,
                    image_size: image.logical_size(),
                },
            ));
        }
    };
    observer.on_event(OperationEvent::VerifyTargetChecked {
        diagnostics: ready.diagnostics(),
    });

    observer.on_event(OperationEvent::OpeningDevice {
        purpose: OpenPurpose::Verify,
        block_path: ready.block_path(),
    });
    let (handle, metadata, open_error) =
        match platform.open_device(ready.block_path(), OpenAccess::ReadOnlyDirect) {
            Ok(handle) => {
                let metadata = platform.fd_metadata(&handle);
                observer.on_event(OperationEvent::DeviceOpened {
                    purpose: OpenPurpose::Verify,
                    metadata: metadata.as_ref(),
                });
                (Some(handle), metadata, None)
            }
            Err(error) => {
                observer.on_event(OperationEvent::DeviceOpenFailed {
                    purpose: OpenPurpose::Verify,
                    error: &error,
                });
                (None, None, Some(error))
            }
        };

    let verifying = match ready.finalize(handle, metadata.as_ref()) {
        Ok(verifying) => verifying,
        Err((image, error)) => {
            return Failed(OperationError::VerifyNotStarted(VerifyNotStarted::Start {
                error,
                open_device: open_error,
                image_size: image.logical_size(),
            }));
        }
    };
    observer.on_event(OperationEvent::FdBound {
        purpose: OpenPurpose::Verify,
    });
    observer.on_event(OperationEvent::VerifyStarted);

    let (image, verify_outcome) =
        verifying.run(|progress| observer.on_event(OperationEvent::VerifyProgress(progress)));
    let image_size = image.logical_size();
    match verify_outcome {
        VerifyOutcome::Succeeded(verify) => Completed { verify, image_size },
        VerifyOutcome::Failed(failed) => Failed(OperationError::Verify { failed, image_size }),
        VerifyOutcome::Cancelled(cancelled) => Cancelled(CancelledAt::Verify {
            cancelled,
            image_size,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::execution::linux_access::OpenDeviceError;
    use crate::execution::write_job::{CancelReason, VerifyFailureReason, WriteStage};
    use crate::image_source::CompressionFormat;
    use std::io::Read as _;

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
        selected_target(platform).prepare_image(platform, image.path(), mode, cancel, |_| {})
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
                    |event| match event {
                        OperationEvent::CompressedImageDetected { format } => {
                            recognised = Some(format)
                        }
                        OperationEvent::PreflightProgress(progress) => {
                            last_progress = Some(progress.logical_produced)
                        }
                        other => panic!("unexpected {other:?}"),
                    },
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

    // An explicit approval is the other way to confirm: it is given to a
    // pending confirmation (there is no other way to call it) and freezes
    // exactly what a matching typed answer freezes.
    #[test]
    fn an_approval_confirms_what_a_matching_typed_answer_confirms() {
        let image = temp_image("approve", "img", &payload());
        let pending = |verify_mode| {
            let platform = ScriptedPlatform::new(vec![found(usb_stick()), found(usb_stick())]);
            let cancel = CancelHandle::new();
            prepare(&platform, &image, verify_mode, &cancel)
                .unwrap()
                .request_confirmation(&cancel)
                .unwrap()
        };
        for verify_mode in [VerifyMode::None, VerifyMode::Quick, VerifyMode::Full] {
            let approved = pending(verify_mode).approve().unwrap();
            let typed = pending(verify_mode).confirm("/dev/sdx").unwrap();
            let (approved, typed) = (approved.confirmed(), typed.confirmed());
            assert_eq!(approved.target_block_path, typed.target_block_path);
            assert_eq!(approved.image_size, typed.image_size);
            assert_eq!(approved.verify_mode, typed.verify_mode);
            assert_eq!(approved.verify_mode, verify_mode);
        }
    }

    // An inspection is only a description: the operation opens the image
    // itself, so a file changed after it was inspected is prepared as it is
    // now (and its own Source Identity starts from that open).
    #[test]
    fn an_inspection_is_not_what_the_operation_uses() {
        use std::io::Write as _;
        let data = payload();
        let image = temp_image("inspect-then-change", "img", &data);
        let inspected = super::super::image::inspect_image(&image.0).unwrap();
        assert_eq!(inspected.logical_size(), Some(data.len() as u64));

        std::fs::OpenOptions::new()
            .append(true)
            .open(&image.0)
            .unwrap()
            .write_all(b"appended after inspection")
            .unwrap();

        let platform = ScriptedPlatform::new(vec![found(usb_stick()), found(usb_stick())]);
        let cancel = CancelHandle::new();
        let prepared = prepare(&platform, &image, VerifyMode::Full, &cancel).unwrap();
        assert_eq!(
            prepared.logical_size(),
            std::fs::metadata(&image.0).unwrap().len()
        );
        assert_ne!(Some(prepared.logical_size()), inspected.logical_size());
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
                |event| {
                    if let OperationEvent::PreflightProgress(_) = event {
                        progress_calls += 1
                    }
                },
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
            "OperationEvent::CompressedImageDetected {",
            "prepare_compressed_image(",
            "core::revalidate(state, platform.fetch_snapshot(&target_block_path))",
            "image: SelectedImage::new(source)",
            "return Err(CancelledBeforeConfirmation);",
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

    // ---- the whole operation (`run_on` with a scripted platform) ----

    // Records every event by name, answers the confirmation with `answer`,
    // and can request cancellation when a given event arrives (or in the
    // test-only pause hook).
    struct TestObserver {
        answer: Option<ConfirmationDecision>,
        events: Vec<String>,
        cancel_on: Option<(&'static str, CancelHandle)>,
        pause: Option<bool>,
        // Every snapshot `write_started_on` handed on (kept apart from the
        // events, whose exact order other tests check).
        bound: Vec<DeviceSnapshot>,
    }

    impl TestObserver {
        fn answering(typed: &str) -> Self {
            TestObserver::deciding(ConfirmationDecision::Submitted(typed.to_string()))
        }

        fn deciding(answer: ConfirmationDecision) -> Self {
            TestObserver {
                answer: Some(answer),
                events: Vec::new(),
                cancel_on: None,
                pause: None,
                bound: Vec::new(),
            }
        }

        fn saw(&self, name: &str) -> bool {
            self.events.iter().any(|event| event == name)
        }
    }

    fn event_name(event: &OperationEvent<'_>) -> String {
        let debug = format!("{event:?}");
        debug
            .split(|c: char| !c.is_alphanumeric())
            .next()
            .unwrap()
            .to_string()
    }

    impl OperationObserver for TestObserver {
        fn on_event(&mut self, event: OperationEvent<'_>) {
            let name = event_name(&event);
            if let Some((trigger, cancel)) = &self.cancel_on {
                if name == *trigger {
                    cancel.request_cancel(CancelReason::UserRequested);
                }
            }
            self.events.push(name);
        }

        fn request_confirmation(
            &mut self,
            request: &ConfirmationRequest<'_>,
        ) -> ConfirmationDecision {
            assert_eq!(request.expected_text, "/dev/sdx");
            self.events.push("ConfirmationRequested".to_string());
            self.answer.take().expect("asked for confirmation twice")
        }

        fn pause_before_verify(&mut self) -> bool {
            self.events.push("PauseBeforeVerify".to_string());
            match self.pause {
                Some(answer) => answer,
                None => true,
            }
        }

        fn write_started_on(&mut self, bound: DeviceSnapshot) {
            self.bound.push(bound);
        }
    }

    fn target_file(tag: &str) -> TempImage {
        temp_image(tag, "device", b"")
    }

    fn device(target: &TempImage) -> Device {
        Device {
            path: target.0.clone(),
            ..Device::default()
        }
    }

    // The snapshots a complete run reads: select, the immediate re-check,
    // the post-Preflight re-check (compressed only), the fresh Write Gate,
    // and Verify's re-check (Quick / Full only).
    fn snapshots(compressed: bool, verify: bool) -> Vec<SnapshotFetchOutcome> {
        let count = 3 + usize::from(compressed) + usize::from(verify);
        (0..count).map(|_| found(usb_stick())).collect()
    }

    fn run(
        platform: &ScriptedPlatform,
        image: &TempImage,
        verify_mode: VerifyMode,
        cancel: &CancelHandle,
        observer: &mut TestObserver,
    ) -> OperationOutcome {
        run_on(
            platform,
            WriteOperationRequest {
                target: cli_target(),
                image_path: image.path().to_string(),
                verify_mode,
            },
            cancel,
            observer,
        )
    }

    fn contents(file: &TempImage) -> Vec<u8> {
        std::fs::read(&file.0).unwrap()
    }

    // 26.1 - 26.7 and 26.17 / 26.18: raw (None / Quick / Full), gzip and xz
    // (None / Full) complete; the target holds the image; the Verify FD is
    // opened (read-only, O_DIRECT access) only for Quick / Full, and only
    // after the write FD is closed; the events arrive in the fixed order.
    #[test]
    fn every_supported_image_and_verify_mode_completes() {
        let data = payload();
        let cases = [
            ("img", data.clone(), false, VerifyMode::None),
            ("img", data.clone(), false, VerifyMode::Quick),
            ("img", data.clone(), false, VerifyMode::Full),
            ("img.gz", gzip(&data), true, VerifyMode::None),
            ("img.gz", gzip(&data), true, VerifyMode::Full),
            ("img.xz", xz(&data), true, VerifyMode::None),
            ("img.xz", xz(&data), true, VerifyMode::Full),
        ];
        for (extension, image_bytes, compressed, mode) in cases {
            let label = format!("{extension} {mode:?}");
            let image = temp_image("run", extension, &image_bytes);
            let target = target_file("run");
            let verify = mode != VerifyMode::None;
            let platform =
                ScriptedPlatform::with_device(snapshots(compressed, verify), device(&target));
            let mut observer = TestObserver::answering("/dev/sdx\n");

            let outcome = run(&platform, &image, mode, &CancelHandle::new(), &mut observer);

            match outcome {
                OperationOutcome::Completed {
                    verify: result,
                    image_size,
                } => {
                    assert_eq!(result.mode, mode, "{label}");
                    assert_eq!(result.skipped, !verify, "{label}");
                    assert_eq!(image_size, data.len() as u64, "{label}");
                    if mode == VerifyMode::Full {
                        assert_eq!(result.verified_bytes, data.len() as u64, "{label}");
                    }
                }
                other => panic!("{label}: {other:?}"),
            }
            assert_eq!(contents(&target), data, "{label}");
            assert_eq!(
                platform.fetches(),
                snapshots(compressed, verify).len(),
                "{label}"
            );
            if verify {
                assert_eq!(
                    platform.opens(),
                    [OpenAccess::WriteExclusive, OpenAccess::ReadOnlyDirect],
                    "{label}"
                );
                assert_eq!(*platform.fds_open_at_verify.borrow(), [0], "{label}");
            } else {
                assert_eq!(platform.opens(), [OpenAccess::WriteExclusive], "{label}");
                assert!(!observer.saw("VerifyPending"), "{label}");
            }

            let mut expected = vec!["TargetSelected"];
            if compressed {
                expected.push("CompressedImageDetected");
            }
            expected.extend(["ImageSelected", "ConfirmationRequested", "Confirmed"]);
            expected.extend([
                "WriteGatePassed",
                "OpeningDevice",
                "DeviceOpened",
                "FdBound",
            ]);
            expected.extend(["WriteAuthorized", "ImageBound", "WriteStarted"]);
            expected.extend(["WriteSucceeded", "SyncStarted", "SyncSucceeded"]);
            if verify {
                expected.extend([
                    "VerifyPending",
                    "PauseBeforeVerify",
                    "VerifySnapshotRequested",
                ]);
                expected.extend(["VerifyTargetChecked", "OpeningDevice", "DeviceOpened"]);
                expected.extend(["FdBound", "VerifyStarted"]);
            }
            let seen: Vec<&str> = observer
                .events
                .iter()
                .map(String::as_str)
                .filter(|name| {
                    !matches!(
                        *name,
                        "PreflightProgress" | "WriteProgress" | "VerifyProgress"
                    )
                })
                .collect();
            assert_eq!(seen, expected, "{label}");
            assert!(observer.saw("WriteProgress"), "{label}");
            assert_eq!(observer.saw("PreflightProgress"), compressed, "{label}");
            assert_eq!(observer.saw("VerifyProgress"), verify, "{label}");
        }
    }

    // 26.8: a wrong confirmation (and no answer at all) never reaches the
    // Write Gate or OpenDevice.
    #[test]
    fn without_a_matching_confirmation_nothing_is_opened() {
        let image = temp_image("confirm-run", "img", &payload());
        for (decision, cancelled) in [
            (
                ConfirmationDecision::Submitted("/dev/sdy".to_string()),
                false,
            ),
            (ConfirmationDecision::Submitted("yes".to_string()), false),
            (ConfirmationDecision::InputClosed, false),
            (
                ConfirmationDecision::InputFailed(std::io::Error::other("simulated")),
                false,
            ),
            (ConfirmationDecision::Cancelled, true),
        ] {
            let target = target_file("confirm-run");
            let platform = ScriptedPlatform::with_device(snapshots(false, false), device(&target));
            let mut observer = TestObserver::deciding(decision);
            let outcome = run(
                &platform,
                &image,
                VerifyMode::Full,
                &CancelHandle::new(),
                &mut observer,
            );

            match (&outcome, cancelled) {
                (OperationOutcome::Cancelled(CancelledAt::Confirmation), true) => {}
                (
                    OperationOutcome::Failed(
                        OperationError::Confirmation(ConfirmError::Mismatch)
                        | OperationError::ConfirmationInputClosed
                        | OperationError::ConfirmationInputFailed(_),
                    ),
                    false,
                ) => {}
                other => panic!("{other:?}"),
            }
            assert_eq!(platform.fetches(), 2);
            assert!(platform.opens().is_empty());
            assert!(contents(&target).is_empty());
        }
    }

    // 26.9: the fresh Write Gate refuses a target that changed after the
    // confirmation; OpenDevice is never requested.
    #[test]
    fn a_fresh_gate_refusal_stops_before_opendevice() {
        let image = temp_image("gate-run", "img", &payload());
        let target = target_file("gate-run");
        let platform = ScriptedPlatform::with_device(
            vec![found(usb_stick()), found(usb_stick()), found(recreated())],
            device(&target),
        );
        let outcome = run(
            &platform,
            &image,
            VerifyMode::None,
            &CancelHandle::new(),
            &mut TestObserver::answering("/dev/sdx"),
        );
        assert!(matches!(
            outcome,
            OperationOutcome::Failed(OperationError::WriteGate(WriteGateError::InstanceRecreated))
        ));
        assert!(platform.opens().is_empty());
    }

    // An approved operation takes the same second half as a typed one: the
    // fresh Write Gate still refuses a target that changed after the
    // confirmation (nothing is opened), and an unchanged one is written and
    // verified in full.
    #[test]
    fn an_approved_operation_still_passes_the_fresh_gate() {
        let image = temp_image("approve-gate", "img", &payload());
        let target = target_file("approve-gate");
        let platform = ScriptedPlatform::with_device(
            vec![found(usb_stick()), found(usb_stick()), found(recreated())],
            device(&target),
        );
        let outcome = run(
            &platform,
            &image,
            VerifyMode::None,
            &CancelHandle::new(),
            &mut TestObserver::deciding(ConfirmationDecision::Approved),
        );
        assert!(matches!(
            outcome,
            OperationOutcome::Failed(OperationError::WriteGate(WriteGateError::InstanceRecreated))
        ));
        assert!(platform.opens().is_empty());
        assert!(contents(&target).is_empty());

        let target = target_file("approve-run");
        let platform = ScriptedPlatform::with_device(snapshots(false, true), device(&target));
        let outcome = run(
            &platform,
            &image,
            VerifyMode::Full,
            &CancelHandle::new(),
            &mut TestObserver::deciding(ConfirmationDecision::Approved),
        );
        assert!(
            matches!(
                outcome,
                OperationOutcome::Completed {
                    verify: crate::execution::write_job::VerifySucceeded { skipped: false, .. },
                    ..
                }
            ),
            "{outcome:?}"
        );
        assert_eq!(contents(&target), payload());
        assert_eq!(
            platform.opens(),
            vec![OpenAccess::WriteExclusive, OpenAccess::ReadOnlyDirect]
        );
    }

    // 26.10: an OpenDevice failure keeps OpenDevice's own error, and the
    // Write Gate's verdict on it.
    #[test]
    fn a_write_opendevice_failure_is_kept_in_the_outcome() {
        let image = temp_image("open-run", "img", &payload());
        let target = target_file("open-run");
        let platform = ScriptedPlatform::with_device(
            snapshots(false, false),
            Device {
                fail: vec![OpenAccess::WriteExclusive],
                ..device(&target)
            },
        );
        let outcome = run(
            &platform,
            &image,
            VerifyMode::Full,
            &CancelHandle::new(),
            &mut TestObserver::answering("/dev/sdx"),
        );
        match outcome {
            OperationOutcome::Failed(OperationError::WriteDeviceRejected {
                error: WriteGateError::OpenDeviceFailed,
                open_device: Some(OpenDeviceError::Connection(message)),
            }) => assert_eq!(message, "simulated"),
            other => panic!("{other:?}"),
        }
        assert_eq!(platform.opens(), [OpenAccess::WriteExclusive]);
        assert!(contents(&target).is_empty());
    }

    // 26.11: an FD that is not bound to the re-verified device is refused
    // and closed; nothing is written.
    #[test]
    fn an_fd_binding_mismatch_writes_nothing() {
        let image = temp_image("binding-run", "img", &payload());
        let target = target_file("binding-run");
        let platform = ScriptedPlatform::with_device(
            snapshots(false, false),
            Device {
                wrong_metadata: true,
                ..device(&target)
            },
        );
        let mut observer = TestObserver::answering("/dev/sdx");
        let outcome = run(
            &platform,
            &image,
            VerifyMode::None,
            &CancelHandle::new(),
            &mut observer,
        );
        assert!(matches!(
            outcome,
            OperationOutcome::Failed(OperationError::WriteDeviceRejected {
                error: WriteGateError::FdBindingMismatch,
                open_device: None,
            })
        ));
        assert!(!observer.saw("WriteStarted"));
        assert!(contents(&target).is_empty());
        assert_eq!(fds_pointing_at(&target.0), 0);
    }

    // 26.12: a write failure is a failure (not a cancellation), with the
    // writer's own error; sync and Verify are not reached.
    #[test]
    fn a_write_failure_is_reported_with_its_cause() {
        let image = temp_image("write-fail", "img", &payload());
        let target = target_file("write-fail");
        let platform = ScriptedPlatform::with_device(
            snapshots(false, false),
            Device {
                read_only_write_fd: true,
                ..device(&target)
            },
        );
        let mut observer = TestObserver::answering("/dev/sdx");
        let outcome = run(
            &platform,
            &image,
            VerifyMode::Full,
            &CancelHandle::new(),
            &mut observer,
        );
        match &outcome {
            OperationOutcome::Failed(OperationError::Write { failed, image_size }) => {
                assert_eq!(failed.stage, WriteStage::Writing);
                assert!(failed.target_may_be_modified);
                assert_eq!(*image_size, payload().len() as u64);
            }
            other => panic!("{other:?}"),
        }
        assert!(!outcome.is_cancelled());
        assert!(!observer.saw("SyncStarted"));
        assert_eq!(platform.opens(), [OpenAccess::WriteExclusive]);
    }

    // Safe Removal's anchor: handed on once, right after the write started,
    // and it is the snapshot the write FD was bound to -- the fresh Write
    // Gate's (here with a device node the selection never saw), not the
    // selection's. A write that fails still started, so it is handed on.
    #[test]
    fn the_bound_target_is_handed_on_once_the_write_started() {
        let image = temp_image("bound-run", "img", &payload());
        let target = target_file("bound-run");
        let mut renamed = usb_stick();
        renamed.device = "/dev/sdy".to_string();
        let platform = ScriptedPlatform::with_device(
            vec![found(usb_stick()), found(usb_stick()), found(renamed)],
            device(&target),
        );
        let mut observer = TestObserver::answering("/dev/sdx");
        let outcome = run(
            &platform,
            &image,
            VerifyMode::None,
            &CancelHandle::new(),
            &mut observer,
        );
        assert!(
            matches!(outcome, OperationOutcome::Completed { .. }),
            "{outcome:?}"
        );
        assert_eq!(observer.bound.len(), 1);
        assert_eq!(observer.bound[0].device, "/dev/sdy");
        assert_eq!(observer.bound[0].block_path, usb_stick().block_path);
        assert_eq!(observer.bound[0].diskseq, usb_stick().diskseq);

        let target = target_file("bound-run-fail");
        let platform = ScriptedPlatform::with_device(
            snapshots(false, false),
            Device {
                read_only_write_fd: true,
                ..device(&target)
            },
        );
        let mut observer = TestObserver::answering("/dev/sdx");
        let outcome = run(
            &platform,
            &image,
            VerifyMode::None,
            &CancelHandle::new(),
            &mut observer,
        );
        assert!(matches!(
            outcome,
            OperationOutcome::Failed(OperationError::Write { .. })
        ));
        assert_eq!(observer.bound.len(), 1);
    }

    // No anchor unless the write started: not when the confirmation is
    // refused, the fresh Write Gate refuses, OpenDevice fails, or the FD
    // binding does not match (that snapshot is never handed on).
    #[test]
    fn no_bound_target_without_a_started_write() {
        let image = temp_image("unbound-run", "img", &payload());
        let cases: [(&str, Vec<SnapshotFetchOutcome>, Device, &str); 4] = [
            (
                "confirmation",
                snapshots(false, false),
                Device::default(),
                "/dev/sdz",
            ),
            (
                "gate",
                vec![found(usb_stick()), found(usb_stick()), found(recreated())],
                Device::default(),
                "/dev/sdx",
            ),
            (
                "open",
                snapshots(false, false),
                Device {
                    fail: vec![OpenAccess::WriteExclusive],
                    ..Device::default()
                },
                "/dev/sdx",
            ),
            (
                "binding",
                snapshots(false, false),
                Device {
                    wrong_metadata: true,
                    ..Device::default()
                },
                "/dev/sdx",
            ),
        ];
        for (name, answers, device_setup, typed) in cases {
            let target = target_file("unbound-run");
            let platform = ScriptedPlatform::with_device(
                answers,
                Device {
                    path: target.0.clone(),
                    ..device_setup
                },
            );
            let mut observer = TestObserver::answering(typed);
            let outcome = run(
                &platform,
                &image,
                VerifyMode::None,
                &CancelHandle::new(),
                &mut observer,
            );
            assert!(
                matches!(outcome, OperationOutcome::Failed(_)),
                "{name}: {outcome:?}"
            );
            assert!(!observer.saw("WriteStarted"), "{name}");
            assert!(observer.bound.is_empty(), "{name}");
            assert!(contents(&target).is_empty(), "{name}");
        }
    }

    // 26.13: a cancellation during the write stops it at the writer's own
    // per-chunk check; nothing is synced or verified.
    #[test]
    fn a_cancellation_during_the_write_stops_it() {
        let data: Vec<u8> = (0..3 * 1024 * 1024u32).map(|i| (i % 249) as u8).collect();
        let image = temp_image("cancel-write", "img", &data);
        let target = target_file("cancel-write");
        let platform = ScriptedPlatform::with_device(snapshots(false, false), device(&target));
        let cancel = CancelHandle::new();
        let mut observer = TestObserver::answering("/dev/sdx");
        observer.cancel_on = Some(("WriteProgress", cancel.clone()));

        let bytes_written = match run(&platform, &image, VerifyMode::Full, &cancel, &mut observer) {
            OperationOutcome::Cancelled(CancelledAt::Write {
                cancelled,
                image_size,
            }) => {
                assert!(cancelled.bytes_written < data.len() as u64);
                assert_eq!(image_size, data.len() as u64);
                cancelled.bytes_written
            }
            other => panic!("{other:?}"),
        };
        // The cancelled write is drained (and its FD closed) before the
        // outcome: the drain is the last thing announced, and what the
        // writer handed over is on the target.
        assert_eq!(
            observer.events.last().map(String::as_str),
            Some("CancelDrainStarted")
        );
        assert!(!observer.saw("WriteSucceeded"));
        assert!(!observer.saw("SyncStarted"));
        assert_eq!(contents(&target), data[..bytes_written as usize]);
        assert_eq!(platform.opens(), [OpenAccess::WriteExclusive]);
    }

    // 26.14: a cancellation while syncing is honoured after the sync
    // succeeded -- for every Verify mode -- and Verify does not start.
    #[test]
    fn a_cancellation_during_sync_stops_before_verify() {
        for mode in [VerifyMode::None, VerifyMode::Full] {
            let image = temp_image("cancel-sync", "img", &payload());
            let target = target_file("cancel-sync");
            let platform = ScriptedPlatform::with_device(snapshots(false, false), device(&target));
            let cancel = CancelHandle::new();
            let mut observer = TestObserver::answering("/dev/sdx");
            observer.cancel_on = Some(("SyncStarted", cancel.clone()));

            assert!(matches!(
                run(&platform, &image, mode, &cancel, &mut observer),
                OperationOutcome::Cancelled(CancelledAt::AfterSync)
            ));
            assert!(observer.saw("SyncSucceeded"));
            assert!(!observer.saw("VerifyPending"));
            assert_eq!(contents(&target), payload());
            assert_eq!(platform.opens(), [OpenAccess::WriteExclusive]);
        }
    }

    // The existing check before Verify (after the test-only pause): a
    // cancellation there stops before Verify reads anything; a pause without
    // input stops too. Neither opens the Verify FD.
    #[test]
    fn verify_does_not_start_after_a_late_cancellation_or_an_ended_pause() {
        for pause in [None, Some(false)] {
            let image = temp_image("before-verify", "img", &payload());
            let target = target_file("before-verify");
            let platform = ScriptedPlatform::with_device(snapshots(false, false), device(&target));
            let cancel = CancelHandle::new();
            let mut observer = TestObserver::answering("/dev/sdx");
            observer.pause = pause;
            if pause.is_none() {
                observer.cancel_on = Some(("VerifyPending", cancel.clone()));
            }

            let outcome = run(&platform, &image, VerifyMode::Full, &cancel, &mut observer);
            match (pause, &outcome) {
                (None, OperationOutcome::Cancelled(CancelledAt::BeforeVerify)) => {}
                (
                    Some(false),
                    OperationOutcome::Failed(OperationError::VerifyNotStarted(
                        VerifyNotStarted::TestPauseEnded,
                    )),
                ) => {}
                other => panic!("{other:?}"),
            }
            assert_eq!(platform.opens(), [OpenAccess::WriteExclusive]);
        }
    }

    // 26.15: after a successful write + sync, a Verify that cannot start is
    // a structured failure: a refused re-check (with or without diagnostics)
    // or a failed OpenDevice (keeping OpenDevice's error).
    #[test]
    fn verify_start_failures_are_structured() {
        let image = temp_image("verify-start", "img", &payload());

        let target = target_file("verify-start");
        let mut answers = snapshots(false, false);
        answers.push(found(recreated()));
        let platform = ScriptedPlatform::with_device(answers, device(&target));
        match run(
            &platform,
            &image,
            VerifyMode::Full,
            &CancelHandle::new(),
            &mut TestObserver::answering("/dev/sdx"),
        ) {
            OperationOutcome::Failed(OperationError::VerifyNotStarted(
                VerifyNotStarted::TargetCheck {
                    error: crate::execution::write_job::VerifyStartError::InstanceRecreated,
                    diagnostics: Some(_),
                    ..
                },
            )) => {}
            other => panic!("{other:?}"),
        }
        assert_eq!(platform.opens(), [OpenAccess::WriteExclusive]);

        let target = target_file("verify-start");
        let mut answers = snapshots(false, false);
        answers.push(SnapshotFetchOutcome::NotFound);
        let platform = ScriptedPlatform::with_device(answers, device(&target));
        assert!(matches!(
            run(
                &platform,
                &image,
                VerifyMode::Full,
                &CancelHandle::new(),
                &mut TestObserver::answering("/dev/sdx"),
            ),
            OperationOutcome::Failed(OperationError::VerifyNotStarted(
                VerifyNotStarted::TargetCheck {
                    diagnostics: None,
                    ..
                }
            ))
        ));

        let target = target_file("verify-start");
        let platform = ScriptedPlatform::with_device(
            snapshots(false, true),
            Device {
                fail: vec![OpenAccess::ReadOnlyDirect],
                ..device(&target)
            },
        );
        match run(
            &platform,
            &image,
            VerifyMode::Full,
            &CancelHandle::new(),
            &mut TestObserver::answering("/dev/sdx"),
        ) {
            OperationOutcome::Failed(OperationError::VerifyNotStarted(
                VerifyNotStarted::Start {
                    error: crate::execution::write_job::VerifyStartError::OpenDeviceFailed,
                    open_device: Some(OpenDeviceError::Connection(_)),
                    image_size,
                },
            )) => assert_eq!(image_size, payload().len() as u64),
            other => panic!("{other:?}"),
        }
        assert_eq!(contents(&target), payload());
    }

    // 26.16: a target whose content differs from the image fails Verify
    // with the first mismatching byte.
    #[test]
    fn a_verify_mismatch_is_a_structured_failure() {
        let data = payload();
        let image = temp_image("mismatch-run", "img", &data);
        let target = target_file("mismatch-run");
        let mut tampered = data.clone();
        tampered[1000] ^= 0xff;
        let other_device = temp_image("mismatch-run-read", "device", &tampered);
        let platform = ScriptedPlatform::with_device(
            snapshots(false, true),
            Device {
                verify_from: Some(other_device.0.clone()),
                ..device(&target)
            },
        );
        match run(
            &platform,
            &image,
            VerifyMode::Full,
            &CancelHandle::new(),
            &mut TestObserver::answering("/dev/sdx"),
        ) {
            OperationOutcome::Failed(OperationError::Verify { failed, image_size }) => {
                assert!(matches!(
                    failed.reason,
                    VerifyFailureReason::Mismatch { offset: 1000, .. }
                ));
                assert_eq!(image_size, data.len() as u64);
            }
            other => panic!("{other:?}"),
        }
    }

    // A compressed image with Quick Verify is refused at L1 by the
    // operation too: nothing is decoded, nothing is opened on the target.
    #[test]
    fn the_operation_refuses_quick_verify_for_a_compressed_image() {
        let image = temp_image("quick-run", "img.xz", &xz(&payload()));
        let target = target_file("quick-run");
        let platform = ScriptedPlatform::with_device(snapshots(false, false), device(&target));
        let mut observer = TestObserver::answering("/dev/sdx");
        assert!(matches!(
            run(
                &platform,
                &image,
                VerifyMode::Quick,
                &CancelHandle::new(),
                &mut observer
            ),
            OperationOutcome::Failed(OperationError::Image(PrepareImageError::CompressedImage(
                CompressedImageRejection::QuickVerifyUnsupported(CompressionFormat::Xz)
            )))
        ));
        assert!(!observer.saw("PreflightProgress"));
        assert!(!observer.saw("ConfirmationRequested"));
        assert!(platform.opens().is_empty());
    }

    // The whole production path, checked on the source of `run_on`: every
    // step in this order, the two OpenDevice calls with their access, and
    // the cancel points -- exactly the existing ones.
    #[test]
    fn the_production_path_runs_in_order() {
        let source = include_str!("operation.rs");
        let production = &source[..source.find("\n#[cfg(test)]\nmod tests {").unwrap()];
        let run_on = &production[production.find("\npub(super) fn run_on(").unwrap()..];
        let steps = [
            "select_write_target(platform, &target)",
            "selected_target.prepare_image(",
            "image.request_confirmation(cancel)",
            "observer.request_confirmation(&pending.request())",
            "pending.confirm(&typed)",
            "pending.approve()",
            "platform.fetch_snapshot(operation.target_block_path())",
            "operation.fresh_gate(refreshed)",
            "platform.open_device(&ready.current().block_path, OpenAccess::WriteExclusive)",
            "platform.fd_metadata(&handle)",
            "let bound = ready.current().clone()",
            "core::finalize_prepared_write(ready, handle, metadata.as_ref())",
            "operation.bind(prepared.begin())",
            "execution.begin_write(cancel.clone())",
            "observer.write_started_on(bound)",
            "writing.write(",
            "run_off_main_thread(succeeded.begin_sync()",
            "after_successful_sync(cancel.is_requested())",
            "synced.begin_verify(image, cancel.clone())",
            "VerifyStart::Skipped",
            "VerifyStart::Pending",
            "observer.pause_before_verify()",
            "return Cancelled(CancelledAt::BeforeVerify)",
            "platform.fetch_snapshot(pending.block_path())",
            "pending.check_target(refreshed)",
            "platform.open_device(ready.block_path(), OpenAccess::ReadOnlyDirect)",
            "platform.fd_metadata(&handle)",
            "ready.finalize(handle, metadata.as_ref())",
            "verifying.run(",
        ];
        let mut at = 0;
        for step in steps {
            let found = run_on[at..]
                .find(step)
                .unwrap_or_else(|| panic!("{step} is missing or out of order"));
            at += found + step.len();
        }

        assert_eq!(production.matches("OpenAccess::WriteExclusive").count(), 1);
        assert_eq!(production.matches("OpenAccess::ReadOnlyDirect").count(), 1);
        assert_eq!(production.matches("platform.open_device(").count(), 2);

        // The cancel points: one return per existing point, and no other
        // cancellation check in the sequence.
        for point in [
            "Cancelled(CancelledAt::Preflight)",
            "Cancelled(CancelledAt::BeforeConfirmation)",
            "Cancelled(CancelledAt::Confirmation)",
            "Cancelled(CancelledAt::Write {",
            "Cancelled(CancelledAt::AfterSync)",
            "Cancelled(CancelledAt::BeforeVerify)",
            "Cancelled(CancelledAt::Verify {",
        ] {
            assert_eq!(run_on.matches(point).count(), 1, "{point}");
        }
        assert_eq!(run_on.matches("if cancel.is_requested()").count(), 1);
    }
}

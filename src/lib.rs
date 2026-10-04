//! Linux Image Writer's Production API: write an image to a removable USB
//! device through the one safe sequence the CLI's `write-test` uses, from a
//! thread of its own.
//!
//! What a caller (a GUI) can do is exactly this:
//!
//! 1. [`list_candidates`] -- the devices, each with the Safety Engine's
//!    assessment and whether it can be selected; after a refresh,
//!    [`DeviceCandidate::is_same_device_as`] tells whether an entry is
//!    still the device (and instance) a held reference was listed as;
//! 2. take a candidate's opaque [`TargetRef`];
//! 3. [`inspect_image`] -- what the chosen image is and which
//!    [`VerifyMode`]s its format allows (for display only);
//! 4. build a [`WriteOperationRequest`] (target, image path, [`VerifyMode`]);
//! 5. [`spawn_write_worker`] -- the whole operation runs on its own thread;
//! 6. receive owned [`WorkerMessage`]s: events, one
//!    [`WorkerConfirmationRequest`], and finally the [`OperationOutcome`];
//! 7. answer the confirmation ([`ConfirmationDecision`]): the user's
//!    explicit approval of the request as shown (`Approved`), or what the
//!    user typed (`Submitted`, which the operation compares);
//! 8. cancel through the worker at any time ([`CancelHandle`]);
//! 9. once `Finished` has arrived, [`WriteWorker::removal_target`] -- the
//!    opaque [`RemovalTarget`] of the device the write started on, when the
//!    outcome allows safe removal (take it before `join`);
//! 10. when the user asks, [`request_safe_removal`] with it (blocking): only
//!     [`SafeRemovalOutcome::Removed`] means the drive can be removed.
//!
//! Everything the sequence is built from -- device snapshots used for
//! decisions, the Safety Engine, selection and confirmation tokens, the
//! image source, the Write Gate, OpenDevice and its file descriptors, FD
//! binding, write / sync / Verify -- lives in private modules. The types
//! below that describe what happened (the outcome's errors and diagnostics)
//! are read-only data: no function of this API takes them as input.
//!
//! The `linux-image-writer-dev` CLI binary compiles the same modules itself
//! (see `Cargo.toml`), so its diagnostic modes can use internals that this
//! library does not expose.
//!
//! ```no_run
//! use linux_image_writer::{
//!     ConfirmationDecision, VerifyAvailability, VerifyMode, WorkerMessage, WriteOperationRequest,
//!     inspect_image, list_candidates, spawn_write_worker,
//! };
//!
//! let candidates = list_candidates().expect("device list");
//! let candidate = candidates
//!     .iter()
//!     .find(|candidate| candidate.selectability().is_selectable())
//!     .expect("a selectable device");
//! let image = inspect_image("image.img").expect("a supported image");
//! let verify_mode = match image.verify_availability(VerifyMode::Quick) {
//!     VerifyAvailability::Available => VerifyMode::Quick,
//!     VerifyAvailability::Unavailable(_) => VerifyMode::Full,
//! };
//! let request = WriteOperationRequest::new(candidate.target().clone(), "image.img", verify_mode);
//! let mut worker = spawn_write_worker(request).expect("worker thread");
//! while let Some(message) = worker.recv() {
//!     match message {
//!         WorkerMessage::Event(_event) => { /* show progress */ }
//!         WorkerMessage::ConfirmationRequested(_request) => {
//!             // Show `_request` (what will be written where); when the user
//!             // clicks "Write", approve it.
//!             worker
//!                 .submit_confirmation(ConfirmationDecision::Approved)
//!                 .expect("a pending confirmation");
//!         }
//!         WorkerMessage::Finished(_outcome) => { /* show the result */ }
//!     }
//! }
//! worker.join().expect("worker thread");
//! ```
//!
//! The internals are not reachable from outside this crate. Each of these
//! fails to compile for the stated reason (the error code is checked):
//!
//! ```compile_fail,E0603
//! // No confirmation token, Write Gate or device open outside the operation.
//! use linux_image_writer::execution::core::ConfirmationToken;
//! ```
//!
//! ```compile_fail,E0603
//! use linux_image_writer::execution::linux_access::open_device;
//! ```
//!
//! ```compile_fail,E0603
//! // The image source is built and used inside the operation only.
//! use linux_image_writer::image_source::SelectedImage;
//! ```
//!
//! ```compile_fail,E0603
//! // The synchronous sequence, its observer and its platform seam are not
//! // public; the worker is the entry point.
//! use linux_image_writer::orchestration::operation::run_write_operation;
//! ```
//!
//! ```compile_fail,E0624
//! // A target reference comes only from the candidate list: the CLI's
//! // block-path reference is not part of this API.
//! let target = linux_image_writer::TargetRef::from_block_path("/org/freedesktop/UDisks2/block_devices/sda");
//! ```
//!
//! ```compile_fail,E0451
//! // A removal target cannot be assembled: its anchor is private.
//! fn forge() -> linux_image_writer::RemovalTarget {
//!     linux_image_writer::RemovalTarget { anchor: todo!() }
//! }
//! ```
//!
//! ```compile_fail,E0616
//! // Nor read: the device snapshot it holds is not reachable.
//! fn peek(target: &linux_image_writer::RemovalTarget) {
//!     let _ = &target.anchor;
//! }
//! ```
//!
//! ```compile_fail,E0624
//! // Only the write operation builds one, from the snapshot its write FD was
//! // bound to; that constructor is not part of this API.
//! fn forge(snapshot: linux_image_writer::report::DeviceSnapshot) -> linux_image_writer::RemovalTarget {
//!     linux_image_writer::RemovalTarget::from_bound_snapshot(snapshot)
//! }
//! ```
//!
//! ```compile_fail,E0277
//! // No device path turns into one.
//! let target: linux_image_writer::RemovalTarget = "/dev/sda".into();
//! ```
//!
//! ```compile_fail,E0277
//! // Nor does a target reference from the device list.
//! fn forge(target: linux_image_writer::TargetRef) -> linux_image_writer::RemovalTarget {
//!     target.into()
//! }
//! ```
//!
//! ```compile_fail,E0308
//! // Safe removal takes a removal target, never a device-list reference.
//! fn remove(target: &linux_image_writer::TargetRef) {
//!     let _ = linux_image_writer::request_safe_removal(target);
//! }
//! ```
//!
//! ```compile_fail,E0432
//! // What safe removal reads and plans stays inside the crate.
//! use linux_image_writer::report::RemovalFacts;
//! ```
//!
//! ```compile_fail,E0432
//! use linux_image_writer::RemovalPlan;
//! ```
//!
//! ```compile_fail,E0451
//! // A request cannot be assembled from its fields.
//! fn forge(target: linux_image_writer::TargetRef) -> linux_image_writer::WriteOperationRequest {
//!     linux_image_writer::WriteOperationRequest {
//!         target,
//!         image_path: String::new(),
//!         verify_mode: linux_image_writer::VerifyMode::Full,
//!     }
//! }
//! ```
//!
//! ```compile_fail,E0451
//! // An image description is only produced by `inspect_image`.
//! fn forge() -> linux_image_writer::ImageInfo {
//!     linux_image_writer::ImageInfo {
//!         file_size: 0,
//!         compression: None,
//!         access: todo!(),
//!         logical_size: None,
//!     }
//! }
//! ```
//!
//! ```compile_fail,E0277
//! // An image description is not an input to a write: a request names the
//! // image by path, and the operation opens and checks it itself.
//! fn request(
//!     target: linux_image_writer::TargetRef,
//!     info: linux_image_writer::ImageInfo,
//! ) -> linux_image_writer::WriteOperationRequest {
//!     linux_image_writer::WriteOperationRequest::new(target, info, linux_image_writer::VerifyMode::Full)
//! }
//! ```
//!
//! ```compile_fail,E0599
//! // Nor is it an image source: it cannot be read.
//! fn read(info: &linux_image_writer::ImageInfo) {
//!     let _ = info.open_reader();
//! }
//! ```
//!
//! ```compile_fail,E0423
//! // A selection generation (what a selection's authority rests on) cannot
//! // be minted outside the crate, so a `SelectionState::Selected` cannot be
//! // forged -- and nothing here would accept one anyway.
//! let generation = linux_image_writer::report::SelectionGeneration(1);
//! ```

// The implementation. Every module is private: only the re-exports below
// are the library's API. Some items are used only by the CLI binary, which
// compiles these same files as its own crate (its diagnostic modes); in
// this library they are unused, hence the per-module `dead_code`
// allowances where that happens.
mod device;
mod execution;
mod identity;
mod image_source;
mod linux_backend;
// UDisks2 signal monitoring: used only by the CLI binary's `monitor` and
// `select` modes.
#[allow(dead_code)]
mod linux_monitor;
mod orchestration;
mod safety;
mod writer;

// ---- Device discovery ----
pub use execution::core::{NotSelectableReason, Selectability};
pub use orchestration::candidates::{
    CandidateListError, DeviceCandidate, DeviceDisplay, TargetRef, list_candidates,
};
pub use safety::{RiskLevel, RiskReason, SafetyAssessment};

// ---- Image inspection (display only) ----
pub use orchestration::image::{
    ImageAccess, ImageInfo, VerifyAvailability, VerifyUnavailableReason, inspect_image,
};

// ---- Write request ----
pub use execution::core::VerifyMode;
pub use orchestration::operation::WriteOperationRequest;

// ---- Worker ----
pub use execution::write_job::{CancelHandle, CancelReason};
pub use orchestration::events::{ConfirmationDecision, OpenPurpose};
pub use orchestration::worker::{
    SubmitError, WorkerConfirmationRequest, WorkerEvent, WorkerMessage, WorkerState, WriteWorker,
    spawn_write_worker,
};

// ---- Safe Removal (the device a finished operation wrote to) ----
pub use orchestration::removal::{RemovalTarget, SafeRemovalOutcome, request_safe_removal};

// ---- Outcome (read-only data describing how an operation ended) ----
pub use orchestration::outcome::{CancelledAt, OperationError, OperationOutcome, VerifyNotStarted};

/// The data types that events and outcomes carry: descriptions of what the
/// operation saw, decided or failed at. They are for display and error
/// reporting; nothing in this API accepts them.
pub mod report {
    pub use crate::device::DeviceSnapshot;
    pub use crate::execution::core::{
        HardHazardReason, IntentBuildError, InvalidationReason, SelectionGeneration,
        SelectionState, VerifyTargetDiagnostics, WriteGateError,
    };
    pub use crate::execution::linux_access::{
        AuthorizationDenial, DirectReadSetupError, FdMetadata, OpenDeviceError,
    };
    pub use crate::execution::write_job::{
        CancelDrainFailed, Cancelled, Failed, ImageBindingError, VerifyCancelled, VerifyFailed,
        VerifyFailureReason, VerifyProgress, VerifyStartError, VerifySucceeded,
        WriteJobFailureCause, WriteStage,
    };
    pub use crate::identity::{IdentityComparison, InstanceComparison};
    pub use crate::image_source::compressed::{PreflightError, PreflightProgress, ReplayFailure};
    pub use crate::image_source::source_identity::SourceChanged;
    pub use crate::image_source::{CompressionFormat, ImageSourceError, UnsupportedCompression};
    pub use crate::orchestration::candidates::{SelectTargetError, TargetChange};
    pub use crate::orchestration::image::CompressedImageRejection;
    pub use crate::orchestration::operation::{ConfirmError, PrepareImageError, TargetNotReady};
    pub use crate::orchestration::removal::{
        RemovalActionError, RemovalStage, RemovalUnavailable, RemovalUnsupported,
    };
    pub use crate::writer::{WriteError, WritePlan, WriteProgress, WritebackProgress};
}

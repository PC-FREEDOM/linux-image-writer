//! Linux USB Writer's Production API: write an image to a removable USB
//! device through the one safe sequence the CLI's `write-test` uses, from a
//! thread of its own.
//!
//! What a caller (a GUI) can do is exactly this:
//!
//! 1. [`list_candidates`] -- the devices, each with the Safety Engine's
//!    assessment and whether it can be selected;
//! 2. take a candidate's opaque [`TargetRef`];
//! 3. build a [`WriteOperationRequest`] (target, image path, [`VerifyMode`]);
//! 4. [`spawn_write_worker`] -- the whole operation runs on its own thread;
//! 5. receive owned [`WorkerMessage`]s: events, one
//!    [`WorkerConfirmationRequest`], and finally the [`OperationOutcome`];
//! 6. answer the confirmation with what the user typed
//!    ([`ConfirmationDecision`]); the operation decides whether it matches;
//! 7. cancel through the worker at any time ([`CancelHandle`]).
//!
//! Everything the sequence is built from -- device snapshots used for
//! decisions, the Safety Engine, selection and confirmation tokens, the
//! image source, the Write Gate, OpenDevice and its file descriptors, FD
//! binding, write / sync / Verify -- lives in private modules. The types
//! below that describe what happened (the outcome's errors and diagnostics)
//! are read-only data: no function of this API takes them as input.
//!
//! The `linux-usb-writer` CLI binary compiles the same modules itself
//! (see `Cargo.toml`), so its diagnostic modes can use internals that this
//! library does not expose.
//!
//! ```no_run
//! use linux_usb_writer::{
//!     ConfirmationDecision, VerifyMode, WorkerMessage, WriteOperationRequest, list_candidates,
//!     spawn_write_worker,
//! };
//!
//! let candidates = list_candidates().expect("device list");
//! let candidate = candidates
//!     .iter()
//!     .find(|candidate| candidate.selectability().is_selectable())
//!     .expect("a selectable device");
//! let request =
//!     WriteOperationRequest::new(candidate.target().clone(), "image.img", VerifyMode::Full);
//! let mut worker = spawn_write_worker(request).expect("worker thread");
//! while let Some(message) = worker.recv() {
//!     match message {
//!         WorkerMessage::Event(_event) => { /* show progress */ }
//!         WorkerMessage::ConfirmationRequested(request) => {
//!             // Show `request` and ask the user to type `request.expected_text`.
//!             let typed = String::new();
//!             worker
//!                 .submit_confirmation(ConfirmationDecision::Submitted(typed))
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
//! use linux_usb_writer::execution::core::ConfirmationToken;
//! ```
//!
//! ```compile_fail,E0603
//! use linux_usb_writer::execution::linux_access::open_device;
//! ```
//!
//! ```compile_fail,E0603
//! // The image source is built and used inside the operation only.
//! use linux_usb_writer::image_source::SelectedImage;
//! ```
//!
//! ```compile_fail,E0603
//! // The synchronous sequence, its observer and its platform seam are not
//! // public; the worker is the entry point.
//! use linux_usb_writer::orchestration::operation::run_write_operation;
//! ```
//!
//! ```compile_fail,E0624
//! // A target reference comes only from the candidate list: the CLI's
//! // block-path reference is not part of this API.
//! let target = linux_usb_writer::TargetRef::from_block_path("/org/freedesktop/UDisks2/block_devices/sda");
//! ```
//!
//! ```compile_fail,E0451
//! // A request cannot be assembled from its fields.
//! fn forge(target: linux_usb_writer::TargetRef) -> linux_usb_writer::WriteOperationRequest {
//!     linux_usb_writer::WriteOperationRequest {
//!         target,
//!         image_path: String::new(),
//!         verify_mode: linux_usb_writer::VerifyMode::Full,
//!     }
//! }
//! ```
//!
//! ```compile_fail,E0423
//! // A selection generation (what a selection's authority rests on) cannot
//! // be minted outside the crate, so a `SelectionState::Selected` cannot be
//! // forged -- and nothing here would accept one anyway.
//! let generation = linux_usb_writer::report::SelectionGeneration(1);
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
        Cancelled, Failed, ImageBindingError, VerifyCancelled, VerifyFailed, VerifyFailureReason,
        VerifyProgress, VerifyStartError, VerifySucceeded, WriteJobFailureCause, WriteStage,
    };
    pub use crate::identity::{IdentityComparison, InstanceComparison};
    pub use crate::image_source::compressed::{PreflightError, PreflightProgress, ReplayFailure};
    pub use crate::image_source::source_identity::SourceChanged;
    pub use crate::image_source::{CompressionFormat, ImageSourceError, UnsupportedCompression};
    pub use crate::orchestration::candidates::{SelectTargetError, TargetChange};
    pub use crate::orchestration::image::CompressedImageRejection;
    pub use crate::orchestration::operation::{ConfirmError, PrepareImageError, TargetNotReady};
    pub use crate::writer::{WriteError, WritePlan, WriteProgress};
}

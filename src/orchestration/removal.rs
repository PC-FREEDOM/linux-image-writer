// Safe Removal (orchestration, Core layer): after a write operation has
// fully ended, the user may ask to make the USB drive safe to remove. The
// sequence -- re-read the device, confirm it is still the device and
// instance that was written, unmount its own filesystems, power it off --
// is decided here from plain facts; nothing in this module talks to D-Bus or
// sysfs, and nothing here unmounts or powers anything off (SR-1).
//
// What is reused, not re-implemented:
//   - identity and instance: `TargetRef::check_against` (block path, then
//     `compare_identity` == Same and `compare_instance` == SameInstance) --
//     the same rule the device list, selection and the write use;
//   - protected states: the Safety Engine's own reasons
//     (`safety::assess_device`), read as they are, never loosened.
// What is Safe Removal's own: which USB topologies v0.1 powers off, and
// which filesystems it may unmount.
//
// `request_safe_removal` runs the whole sequence (SR-4): the worker hands
// out the `RemovalTarget` (SR-2), `linux_backend::removal_facts` reads the
// facts (SR-3), and `linux_access` makes the two UDisks2 calls. The CLI
// binary compiles this module too but uses only part of it, hence the
// module-wide `dead_code` allowance.
#![allow(dead_code)]

use std::fmt;

use super::candidates::{DeviceDisplay, TargetChange, TargetCheck, TargetRef};
use super::outcome::{CancelledAt, OperationError, OperationOutcome, VerifyNotStarted};
use crate::device::{BlockRole, DeviceSnapshot, RemovalFacts, RemovalFactsOutcome, UsbTopology};
use crate::execution::linux_access::{self, AuthorizationDenial, RemovalCallError};
use crate::linux_backend::removal_facts::collect_removal_facts;
use crate::safety::{RiskReason, assess_device};

// ---- The reference ----

// The device a write operation actually opened for writing, for Safe
// Removal only. It carries no authority: removal re-reads the device and
// refuses unless it is still the same device and instance. The anchor is
// the snapshot the write FD was bound to (see
// `TargetRef::from_bound_snapshot`); it is private, and the only
// constructor is crate-private, so a caller can neither build one nor turn
// a device path into one.
/// The device a finished write operation wrote to, for safe removal. Only
/// the operation can produce one.
#[derive(Clone)]
pub struct RemovalTarget {
    anchor: TargetRef,
}

impl RemovalTarget {
    // `snapshot` must be the one the write FD was bound to.
    pub(crate) fn from_bound_snapshot(snapshot: DeviceSnapshot) -> Self {
        RemovalTarget {
            anchor: TargetRef::from_bound_snapshot(snapshot),
        }
    }

    // Where to re-read the device.
    pub(crate) fn block_path(&self) -> &str {
        self.anchor.block_path()
    }
}

// Only the block path: the anchor holds the device's serial number, which
// is never printed.
impl fmt::Debug for RemovalTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemovalTarget")
            .field("block_path", &self.block_path())
            .finish_non_exhaustive()
    }
}

// ---- Outcomes ----

/// How a safe removal ended. Only [`SafeRemovalOutcome::Removed`] means the
/// drive can be removed.
#[derive(Debug)]
pub enum SafeRemovalOutcome {
    // The drive was powered off: the only outcome that allows saying it can
    // be removed. `device` is boxed: far larger than every other variant.
    Removed {
        device: Box<DeviceDisplay>,
        unmounted: Vec<String>,
    },
    // The device is gone (unplugged, or already powered off).
    DeviceGone,
    // Not the device and instance that was written.
    DeviceChanged(TargetChange),
    Unsupported(RemovalUnsupported),
    // Something is using it; trying again later may work.
    Busy {
        stage: RemovalStage,
        unmounted: Vec<String>,
    },
    // Authorization would need interaction, which removal never starts.
    NotAuthorized {
        stage: RemovalStage,
        denial: AuthorizationDenial,
        unmounted: Vec<String>,
    },
    // The device's state could not be established; nothing was done.
    Unavailable(RemovalUnavailable),
    Failed {
        stage: RemovalStage,
        error: RemovalActionError,
        unmounted: Vec<String>,
    },
}

/// Which step of a safe removal an outcome is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalStage {
    Unmount,
    PowerOff,
}

/// Why this device is not removed by Linux USB Writer (v0.1 powers off only
/// a single, ordinary USB device).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalUnsupported {
    // Not on the USB bus.
    NotUsb,
    // UDisks2 does not offer to power it off.
    CannotPowerOff,
    // Other drives share the physical device, or the USB device has more
    // than one interface: powering it off would remove them too.
    SharedPhysicalDevice,
    // The USB topology could not be established.
    TopologyUnknown,
    // The Safety Engine protects it (a system device, or one the system
    // says to leave alone).
    ProtectedDevice,
    // A system mount point (`/`, `/boot`, `/boot/efi`) is on it.
    CriticalMount,
    ActiveSwap,
    // LVM, RAID or encryption is involved.
    ComplexStorage,
    // A filesystem on the drive that is neither the disk itself nor one of
    // its partitions.
    UnrecognizedLayout,
}

/// Why a device's state could not be established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemovalUnavailable {
    // The device's information could not be read (the collection's own
    // error, as text).
    DeviceInformation(String),
    // The mount information does not add up (an object outside the drive,
    // or a mount point no object accounts for).
    MountInformation,
    // The reference has nothing to compare with (not expected: every
    // `RemovalTarget` is built from a snapshot).
    NoAnchor,
}

/// A UDisks2 call that failed for a reason other than being busy or not
/// authorized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemovalActionError {
    // An error reply from UDisks2, by name; the message is for display only.
    Rejected {
        name: String,
        message: Option<String>,
    },
    // Not a reply from UDisks2 (the bus, the call itself).
    Transport(String),
    // The system bus could not be reached.
    Connection(String),
}

// ---- When removal is offered ----

// Whether a finished operation may offer Safe Removal. Every variant is
// decided explicitly (no wildcard), so a new variant cannot compile until
// it is decided here. A `RemovalTarget` exists only once the write started;
// this rule then leaves out what v0.1 does not offer.
pub(crate) fn removal_allowed(outcome: &OperationOutcome) -> bool {
    match outcome {
        OperationOutcome::Completed { .. } => true,
        OperationOutcome::Cancelled(at) => match at {
            CancelledAt::Preflight
            | CancelledAt::BeforeConfirmation
            | CancelledAt::Confirmation => false,
            CancelledAt::Write { .. }
            | CancelledAt::AfterSync
            | CancelledAt::BeforeVerify
            | CancelledAt::Verify { .. } => true,
        },
        OperationOutcome::Failed(error) => match error {
            OperationError::Target(_)
            | OperationError::Image(_)
            | OperationError::Confirmation(_)
            | OperationError::ConfirmationInputClosed
            | OperationError::ConfirmationInputFailed(_)
            | OperationError::WriteGate(_)
            | OperationError::WriteDeviceRejected { .. }
            | OperationError::ImageBinding(_)
            | OperationError::ReaderOpen(_) => false,
            OperationError::Write { .. } | OperationError::Sync { .. } => true,
            // A panic is not followed by further device operations.
            OperationError::SyncWorkerPanicked { .. } => false,
            OperationError::VerifyNotStarted(not_started) => match not_started {
                VerifyNotStarted::TestPauseEnded => false,
                // Verify's own re-check found the target changed.
                VerifyNotStarted::TargetCheck { .. } => false,
                VerifyNotStarted::Start { .. } => false,
            },
            OperationError::Verify { .. } => true,
        },
    }
}

// ---- Judging the device ----

// What removal would do, once the facts allow it.
#[derive(Debug)]
pub(crate) struct RemovalPlan {
    // The fresh drive object to power off (never a remembered path).
    pub(crate) drive_path: String,
    // The drive's mounted filesystems, each to unmount (Block object path
    // and its mount points), in object-path order.
    pub(crate) unmount: Vec<UnmountTarget>,
    pub(crate) device: DeviceDisplay,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnmountTarget {
    pub(crate) object_path: String,
    pub(crate) mount_points: Vec<String>,
}

// Decides from freshly collected facts whether `target` may be unmounted
// and powered off, and what to unmount. Fail-closed: anything unknown,
// inconsistent or not v0.1's single ordinary USB device is refused.
pub(crate) fn decide(
    target: &RemovalTarget,
    fetched: RemovalFactsOutcome,
) -> Result<RemovalPlan, SafeRemovalOutcome> {
    use SafeRemovalOutcome::{DeviceChanged, DeviceGone, Unavailable};

    let facts = match fetched {
        RemovalFactsOutcome::Found(facts) => *facts,
        RemovalFactsOutcome::NotFound => return Err(DeviceGone),
        RemovalFactsOutcome::Error(reason) => {
            return Err(Unavailable(RemovalUnavailable::DeviceInformation(reason)));
        }
    };

    match target.anchor.check_against(&facts.snapshot) {
        TargetCheck::Unchanged => {}
        TargetCheck::Changed(change) => return Err(DeviceChanged(change)),
        TargetCheck::Unverified => return Err(Unavailable(RemovalUnavailable::NoAnchor)),
    }

    assess(&facts)
}

// Everything `decide` judges after identity and instance: the protected
// states, v0.1's topology, and what to unmount. Crate-private; besides
// `decide`, only the CLI's read-only diagnostic uses it (which has no
// `RemovalTarget`, and says that it checked no identity).
pub(crate) fn assess(facts: &RemovalFacts) -> Result<RemovalPlan, SafeRemovalOutcome> {
    use SafeRemovalOutcome::Unsupported;

    if let Some(reason) = protection(&facts.snapshot) {
        return Err(Unsupported(reason));
    }
    if let Some(reason) = topology(facts) {
        return Err(Unsupported(reason));
    }
    let unmount = unmount_targets(facts)?;

    Ok(RemovalPlan {
        drive_path: facts.snapshot.drive_path.clone(),
        unmount,
        device: DeviceDisplay::from_snapshot(&facts.snapshot),
    })
}

// The Safety Engine's reasons that rule removal out. Unmounting cannot
// resolve any of them.
fn protection(snapshot: &DeviceSnapshot) -> Option<RemovalUnsupported> {
    let reasons = assess_device(snapshot).reasons;
    let has = |wanted: fn(&RiskReason) -> bool| reasons.iter().any(wanted);
    if has(|reason| {
        matches!(
            reason,
            RiskReason::SystemDevice | RiskReason::IgnoredBySystem
        )
    }) {
        Some(RemovalUnsupported::ProtectedDevice)
    } else if has(|reason| matches!(reason, RiskReason::CriticalMount)) {
        Some(RemovalUnsupported::CriticalMount)
    } else if has(|reason| matches!(reason, RiskReason::ActiveSwap)) {
        Some(RemovalUnsupported::ActiveSwap)
    } else if has(|reason| matches!(reason, RiskReason::ComplexStorage)) {
        Some(RemovalUnsupported::ComplexStorage)
    } else {
        None
    }
}

// v0.1 powers off only a single, ordinary USB device: on the USB bus,
// offered for power-off by UDisks2, sharing its physical device with no
// other drive, and a USB device with exactly one interface (established
// against this very instance in sysfs).
fn topology(facts: &RemovalFacts) -> Option<RemovalUnsupported> {
    if facts.snapshot.connection_bus != "usb" {
        return Some(RemovalUnsupported::NotUsb);
    }
    if !facts.can_power_off {
        return Some(RemovalUnsupported::CannotPowerOff);
    }
    if facts.sibling_id.is_empty() {
        return Some(RemovalUnsupported::TopologyUnknown);
    }
    if facts.other_siblings != 0 {
        return Some(RemovalUnsupported::SharedPhysicalDevice);
    }
    match facts.usb {
        UsbTopology::Bound { interfaces: 1 } => None,
        UsbTopology::Bound { .. } => Some(RemovalUnsupported::SharedPhysicalDevice),
        UsbTopology::Unknown => Some(RemovalUnsupported::TopologyUnknown),
    }
}

// The drive's mounted filesystems to unmount: only the whole disk itself
// (a filesystem written directly to it) and its partitions. Every object
// must belong to this drive and be what it says it is, and every mount
// point the device reports must be accounted for by one of them -- so a
// mounted whole-disk filesystem can never be missed.
fn unmount_targets(facts: &RemovalFacts) -> Result<Vec<UnmountTarget>, SafeRemovalOutcome> {
    use SafeRemovalOutcome::{Unavailable, Unsupported};
    let snapshot = &facts.snapshot;
    let inconsistent = || Unavailable(RemovalUnavailable::MountInformation);

    let mut targets = Vec::new();
    let mut accounted: Vec<String> = Vec::new();
    for filesystem in &facts.filesystems {
        if filesystem.drive_path != snapshot.drive_path {
            return Err(inconsistent());
        }
        let is_disk = filesystem.object_path == snapshot.block_path;
        match filesystem.role {
            BlockRole::WholeDisk if is_disk => {}
            BlockRole::Partition if !is_disk => {}
            BlockRole::WholeDisk | BlockRole::Partition => return Err(inconsistent()),
            BlockRole::Other => {
                return Err(Unsupported(RemovalUnsupported::UnrecognizedLayout));
            }
        }
        let mount_points: Vec<String> = filesystem
            .mount_points
            .iter()
            .filter(|point| !point.is_empty())
            .cloned()
            .collect();
        accounted.extend(mount_points.iter().cloned());
        if !mount_points.is_empty() {
            targets.push(UnmountTarget {
                object_path: filesystem.object_path.clone(),
                mount_points,
            });
        }
    }

    // The device's own merged list and the objects' lists must describe the
    // same mounts.
    let reported = &snapshot.mount_points;
    if reported.iter().any(|point| !accounted.contains(point))
        || accounted.iter().any(|point| !reported.contains(point))
    {
        return Err(inconsistent());
    }

    targets.sort_by(|a, b| a.object_path.cmp(&b.object_path));
    Ok(targets)
}

// ---- The removal ----

// The system calls Safe Removal makes, behind one crate-private seam so
// tests can run the whole sequence without a device. Production uses
// `LinuxRemovalPlatform`; every decision is made on what these return, by
// `decide`, whichever platform answers.
pub(crate) trait RemovalPlatform {
    // Fresh facts for the block path (`collect_removal_facts`).
    fn collect_facts(&self, block_path: &str) -> RemovalFactsOutcome;
    // UDisks2 Filesystem.Unmount of one Block object (never forced, never
    // interactive).
    fn unmount(&self, block_object_path: &str) -> Result<(), RemovalCallError>;
    // UDisks2 Drive.PowerOff (never interactive).
    fn power_off(&self, drive_path: &str) -> Result<(), RemovalCallError>;
}

pub(crate) struct LinuxRemovalPlatform;

impl RemovalPlatform for LinuxRemovalPlatform {
    fn collect_facts(&self, block_path: &str) -> RemovalFactsOutcome {
        collect_removal_facts(block_path)
    }

    fn unmount(&self, block_object_path: &str) -> Result<(), RemovalCallError> {
        linux_access::unmount_filesystem(block_object_path)
    }

    fn power_off(&self, drive_path: &str) -> Result<(), RemovalCallError> {
        linux_access::power_off_drive(drive_path)
    }
}

// Makes the drive a finished write operation wrote to safe to remove:
// re-reads the device and requires it to still be the same device and
// instance (and v0.1's single ordinary USB device), unmounts its own
// mounted filesystems, re-reads it again, and only then asks UDisks2 to
// power it off. Blocking (UDisks2 calls); a UI calls it off its main
// thread. Only `SafeRemovalOutcome::Removed` means the drive can be
// removed.
/// Makes the drive `target` names safe to remove: re-checks that it is
/// still the same device and instance, unmounts its filesystems, re-checks
/// it again, then asks UDisks2 to power it off. Blocks until done. Only
/// [`SafeRemovalOutcome::Removed`] means the drive can be removed.
pub fn request_safe_removal(target: &RemovalTarget) -> SafeRemovalOutcome {
    run_removal(&LinuxRemovalPlatform, target)
}

// The sequence, with the platform as a parameter:
//   1. fresh facts #1, `decide` (identity, instance, protected states,
//      topology, mount layout) -- a refusal ends here, nothing done;
//   2. unmount what #1's plan lists, in order; the first failure ends here
//      (nothing further is unmounted, nothing is re-mounted, no power-off);
//      "not mounted" (already unmounted meanwhile) counts as done;
//   3. fresh facts #2, `decide` again -- nothing from #1 is reused; any
//      refusal, or a filesystem mounted again, ends here (no second round
//      of unmounting, no power-off);
//   4. power off the drive #2 names; only its success is `Removed`.
pub(crate) fn run_removal(
    platform: &impl RemovalPlatform,
    target: &RemovalTarget,
) -> SafeRemovalOutcome {
    let first = match decide(target, platform.collect_facts(target.block_path())) {
        Ok(plan) => plan,
        Err(refused) => return refused,
    };

    let mut unmounted = Vec::new();
    for filesystem in &first.unmount {
        // UDisks2 unmounts one mount point per call.
        for mount_point in &filesystem.mount_points {
            match platform.unmount(&filesystem.object_path) {
                Ok(()) => unmounted.push(mount_point.clone()),
                Err(RemovalCallError::NotMounted) => {}
                Err(error) => return failed_call(RemovalStage::Unmount, error, unmounted),
            }
        }
    }

    let second = match decide(target, platform.collect_facts(target.block_path())) {
        Ok(plan) => plan,
        Err(refused) => return refused,
    };
    if !second.unmount.is_empty() {
        // Mounted again after unmounting: stop, never loop.
        return SafeRemovalOutcome::Busy {
            stage: RemovalStage::PowerOff,
            unmounted,
        };
    }

    match platform.power_off(&second.drive_path) {
        Ok(()) => SafeRemovalOutcome::Removed {
            device: Box::new(second.device),
            unmounted,
        },
        Err(error) => failed_call(RemovalStage::PowerOff, error, unmounted),
    }
}

// A failed UDisks2 call, as an outcome.
fn failed_call(
    stage: RemovalStage,
    error: RemovalCallError,
    unmounted: Vec<String>,
) -> SafeRemovalOutcome {
    let failed = |error| SafeRemovalOutcome::Failed {
        stage,
        error,
        unmounted: unmounted.clone(),
    };
    match error {
        RemovalCallError::Busy => SafeRemovalOutcome::Busy { stage, unmounted },
        RemovalCallError::NotAuthorized(denial) => SafeRemovalOutcome::NotAuthorized {
            stage,
            denial,
            unmounted,
        },
        // Only Unmount may say "not mounted"; anywhere else it is a failure.
        RemovalCallError::NotMounted => failed(RemovalActionError::Rejected {
            name: "org.freedesktop.UDisks2.Error.NotMounted".to_string(),
            message: None,
        }),
        RemovalCallError::Rejected { name, message } => {
            failed(RemovalActionError::Rejected { name, message })
        }
        RemovalCallError::Connection(detail) => failed(RemovalActionError::Connection(detail)),
        RemovalCallError::Transport(detail) => failed(RemovalActionError::Transport(detail)),
    }
}

// `pub(super)`: the worker's tests reuse `every_outcome`.
#[cfg(test)]
pub(super) mod tests {
    use super::super::operation::{ConfirmError, PrepareImageError, TargetNotReady};
    use super::super::test_support::{recreated, usb_stick};
    use super::*;
    use crate::device::BlockFilesystem;
    use crate::execution::core::{VerifyMode, WriteGateError};
    use crate::execution::write_job::{
        CancelReason, Cancelled, Failed, ImageBindingError, VerifyCancelled, VerifyFailed,
        VerifyFailureReason, VerifyStartError, VerifySucceeded, WriteJobFailureCause, WriteStage,
    };
    use crate::identity::{IdentityComparison, InstanceComparison};
    use crate::image_source::ImageSourceError;
    use crate::orchestration::candidates::SelectTargetError;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io;

    const DISK: &str = "/org/freedesktop/UDisks2/block_devices/sdx";
    const PART1: &str = "/org/freedesktop/UDisks2/block_devices/sdx1";
    const PART2: &str = "/org/freedesktop/UDisks2/block_devices/sdx2";

    fn target() -> RemovalTarget {
        RemovalTarget::from_bound_snapshot(usb_stick())
    }

    // An ordinary single USB stick, nothing mounted.
    fn facts() -> RemovalFacts {
        RemovalFacts {
            snapshot: usb_stick(),
            can_power_off: true,
            sibling_id: "/sys/devices/pci0000:00/usb2/2-1/2-1:1.0".to_string(),
            other_siblings: 0,
            usb: UsbTopology::Bound { interfaces: 1 },
            filesystems: Vec::new(),
        }
    }

    fn filesystem(object_path: &str, role: BlockRole, mount_points: &[&str]) -> BlockFilesystem {
        BlockFilesystem {
            object_path: object_path.to_string(),
            drive_path: usb_stick().drive_path,
            role,
            mount_points: mount_points.iter().map(|point| point.to_string()).collect(),
        }
    }

    // `facts` with these filesystems, and the device's merged mount points
    // set to match them.
    fn mounted(filesystems: Vec<BlockFilesystem>) -> RemovalFacts {
        let mut facts = facts();
        facts.snapshot.mount_points = filesystems
            .iter()
            .flat_map(|filesystem| filesystem.mount_points.clone())
            .collect();
        facts.filesystems = filesystems;
        facts
    }

    fn judge(facts: RemovalFacts) -> Result<RemovalPlan, SafeRemovalOutcome> {
        decide(&target(), RemovalFactsOutcome::Found(Box::new(facts)))
    }

    fn unsupported(facts: RemovalFacts) -> RemovalUnsupported {
        match judge(facts) {
            Err(SafeRemovalOutcome::Unsupported(reason)) => reason,
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    fn unavailable(facts: RemovalFacts) -> RemovalUnavailable {
        match judge(facts) {
            Err(SafeRemovalOutcome::Unavailable(reason)) => reason,
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    // ---- The removal sequence (a fake platform) ----

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Facts,
        Unmount(String),
        PowerOff(String),
    }

    // Answers each call from a script and records it. A call nothing was
    // scripted for panics -- so a power-off that must not happen fails the
    // test even before its assertions.
    struct Fake {
        facts: RefCell<VecDeque<RemovalFactsOutcome>>,
        unmounts: RefCell<VecDeque<Result<(), RemovalCallError>>>,
        power_off: RefCell<Option<Result<(), RemovalCallError>>>,
        calls: RefCell<Vec<Call>>,
    }

    impl Fake {
        fn new(facts: Vec<RemovalFactsOutcome>) -> Self {
            Fake {
                facts: RefCell::new(facts.into()),
                unmounts: RefCell::new(VecDeque::new()),
                power_off: RefCell::new(None),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn unmounting(self, results: Vec<Result<(), RemovalCallError>>) -> Self {
            *self.unmounts.borrow_mut() = results.into();
            self
        }

        fn powering_off(self, result: Result<(), RemovalCallError>) -> Self {
            *self.power_off.borrow_mut() = Some(result);
            self
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.borrow().clone()
        }

        fn power_offs(&self) -> usize {
            self.calls()
                .iter()
                .filter(|call| matches!(call, Call::PowerOff(_)))
                .count()
        }
    }

    impl RemovalPlatform for Fake {
        fn collect_facts(&self, block_path: &str) -> RemovalFactsOutcome {
            assert_eq!(block_path, DISK);
            self.calls.borrow_mut().push(Call::Facts);
            self.facts
                .borrow_mut()
                .pop_front()
                .expect("an unexpected facts fetch")
        }

        fn unmount(&self, block_object_path: &str) -> Result<(), RemovalCallError> {
            self.calls
                .borrow_mut()
                .push(Call::Unmount(block_object_path.to_string()));
            self.unmounts
                .borrow_mut()
                .pop_front()
                .expect("an unexpected unmount")
        }

        fn power_off(&self, drive_path: &str) -> Result<(), RemovalCallError> {
            self.calls
                .borrow_mut()
                .push(Call::PowerOff(drive_path.to_string()));
            self.power_off
                .borrow_mut()
                .take()
                .expect("an unexpected power-off")
        }
    }

    fn found(facts: RemovalFacts) -> RemovalFactsOutcome {
        RemovalFactsOutcome::Found(Box::new(facts))
    }

    // Whether an outcome is the one a case expects.
    type Expected = fn(&SafeRemovalOutcome) -> bool;

    fn remove(fake: &Fake) -> SafeRemovalOutcome {
        run_removal(fake, &target())
    }

    fn drive() -> String {
        usb_stick().drive_path
    }

    fn rejected() -> RemovalCallError {
        RemovalCallError::Rejected {
            name: "org.freedesktop.UDisks2.Error.Failed".to_string(),
            message: Some("failed".to_string()),
        }
    }

    // A: nothing mounted: two fresh checks, then the power-off, and only
    // its success is `Removed`.
    #[test]
    fn nothing_mounted_is_checked_twice_then_powered_off() {
        let fake = Fake::new(vec![found(facts()), found(facts())]).powering_off(Ok(()));
        match remove(&fake) {
            SafeRemovalOutcome::Removed { device, unmounted } => {
                assert_eq!(device.device, "/dev/sdx");
                assert!(unmounted.is_empty());
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            fake.calls(),
            [Call::Facts, Call::Facts, Call::PowerOff(drive())]
        );
    }

    // A: a mounted whole-disk filesystem is unmounted before the second
    // check.
    #[test]
    fn a_mounted_whole_disk_filesystem_is_unmounted_first() {
        let first = mounted(vec![filesystem(
            DISK,
            BlockRole::WholeDisk,
            &["/run/media/user/ISO"],
        )]);
        let fake = Fake::new(vec![found(first), found(facts())])
            .unmounting(vec![Ok(())])
            .powering_off(Ok(()));
        match remove(&fake) {
            SafeRemovalOutcome::Removed { unmounted, .. } => {
                assert_eq!(unmounted, ["/run/media/user/ISO"]);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            fake.calls(),
            [
                Call::Facts,
                Call::Unmount(DISK.to_string()),
                Call::Facts,
                Call::PowerOff(drive()),
            ]
        );
    }

    // A: every mounted partition is unmounted, once per mount point, in
    // object-path order; an unmounted one is not touched.
    #[test]
    fn mounted_partitions_are_unmounted_once_per_mount_point() {
        let first = mounted(vec![
            filesystem(PART2, BlockRole::Partition, &["/mnt/b", "/mnt/b2"]),
            filesystem(PART1, BlockRole::Partition, &["/mnt/a"]),
            filesystem(DISK, BlockRole::WholeDisk, &[]),
        ]);
        let fake = Fake::new(vec![found(first), found(facts())])
            .unmounting(vec![Ok(()), Ok(()), Ok(())])
            .powering_off(Ok(()));
        assert!(matches!(
            remove(&fake),
            SafeRemovalOutcome::Removed { ref unmounted, .. }
                if unmounted == &["/mnt/a", "/mnt/b", "/mnt/b2"]
        ));
        assert_eq!(
            fake.calls(),
            [
                Call::Facts,
                Call::Unmount(PART1.to_string()),
                Call::Unmount(PART2.to_string()),
                Call::Unmount(PART2.to_string()),
                Call::Facts,
                Call::PowerOff(drive()),
            ]
        );
    }

    // B: the first check refuses: nothing is unmounted, the facts are not
    // read again, nothing is powered off.
    #[test]
    fn a_first_check_refusal_does_nothing() {
        let mut changed = facts();
        changed.snapshot.serial = "OTHER-SERIAL".to_string();
        let mut protected = facts();
        protected.snapshot.hint_system = true;
        let mut swapping = facts();
        swapping.snapshot.active_swap = true;
        let mut recreated_facts = facts();
        recreated_facts.snapshot = recreated();

        let cases: Vec<(&str, RemovalFactsOutcome)> = vec![
            ("gone", RemovalFactsOutcome::NotFound),
            ("changed", found(changed)),
            ("recreated", found(recreated_facts)),
            ("protected", found(protected)),
            ("in use as swap", found(swapping)),
            (
                "unreadable",
                RemovalFactsOutcome::Error("D-Bus".to_string()),
            ),
        ];
        for (name, first) in cases {
            let fake = Fake::new(vec![first]);
            let outcome = remove(&fake);
            let expected = match name {
                "gone" => matches!(outcome, SafeRemovalOutcome::DeviceGone),
                "changed" | "recreated" => matches!(outcome, SafeRemovalOutcome::DeviceChanged(_)),
                "protected" => matches!(
                    outcome,
                    SafeRemovalOutcome::Unsupported(RemovalUnsupported::ProtectedDevice)
                ),
                "in use as swap" => matches!(
                    outcome,
                    SafeRemovalOutcome::Unsupported(RemovalUnsupported::ActiveSwap)
                ),
                _ => matches!(outcome, SafeRemovalOutcome::Unavailable(_)),
            };
            assert!(expected, "{name}: {outcome:?}");
            assert_eq!(fake.calls(), [Call::Facts], "{name}");
        }
    }

    // C: an unmount failure ends the removal there: nothing further is
    // unmounted, the facts are not read again, nothing is powered off, and
    // what was already unmounted is reported.
    #[test]
    fn an_unmount_failure_stops_everything_after_it() {
        let first = || {
            found(mounted(vec![
                filesystem(PART1, BlockRole::Partition, &["/mnt/a"]),
                filesystem(PART2, BlockRole::Partition, &["/mnt/b"]),
            ]))
        };
        let cases: Vec<(RemovalCallError, Expected)> = vec![
            (RemovalCallError::Busy, |outcome| {
                matches!(
                    outcome,
                    SafeRemovalOutcome::Busy {
                        stage: RemovalStage::Unmount,
                        ..
                    }
                )
            }),
            (
                RemovalCallError::NotAuthorized(AuthorizationDenial::CanObtain),
                |outcome| {
                    matches!(
                        outcome,
                        SafeRemovalOutcome::NotAuthorized {
                            stage: RemovalStage::Unmount,
                            denial: AuthorizationDenial::CanObtain,
                            ..
                        }
                    )
                },
            ),
            (rejected(), |outcome| {
                matches!(
                    outcome,
                    SafeRemovalOutcome::Failed {
                        stage: RemovalStage::Unmount,
                        error: RemovalActionError::Rejected { .. },
                        ..
                    }
                )
            }),
            (RemovalCallError::Transport("gone".to_string()), |outcome| {
                matches!(
                    outcome,
                    SafeRemovalOutcome::Failed {
                        error: RemovalActionError::Transport(_),
                        ..
                    }
                )
            }),
        ];
        for (error, expected) in cases {
            // Failing on the first filesystem.
            let fake = Fake::new(vec![first()]).unmounting(vec![Err(error.clone())]);
            let outcome = remove(&fake);
            assert!(expected(&outcome), "{error:?}: {outcome:?}");
            assert_eq!(
                fake.calls(),
                [Call::Facts, Call::Unmount(PART1.to_string())],
                "{error:?}"
            );

            // Failing on the second: the first stays unmounted and is
            // reported; nothing else happens.
            let fake = Fake::new(vec![first()]).unmounting(vec![Ok(()), Err(error.clone())]);
            let outcome = remove(&fake);
            assert!(expected(&outcome), "{error:?}: {outcome:?}");
            let unmounted = match &outcome {
                SafeRemovalOutcome::Busy { unmounted, .. }
                | SafeRemovalOutcome::NotAuthorized { unmounted, .. }
                | SafeRemovalOutcome::Failed { unmounted, .. } => unmounted.clone(),
                other => panic!("{other:?}"),
            };
            assert_eq!(unmounted, ["/mnt/a"]);
            assert_eq!(
                fake.calls(),
                [
                    Call::Facts,
                    Call::Unmount(PART1.to_string()),
                    Call::Unmount(PART2.to_string()),
                ]
            );
            assert_eq!(fake.power_offs(), 0);
        }
    }

    // C: "not mounted" (unmounted meanwhile) counts as done -- and is not
    // reported as unmounted by this removal.
    #[test]
    fn a_filesystem_already_unmounted_counts_as_done() {
        let first = mounted(vec![
            filesystem(PART1, BlockRole::Partition, &["/mnt/a"]),
            filesystem(PART2, BlockRole::Partition, &["/mnt/b"]),
        ]);
        let fake = Fake::new(vec![found(first), found(facts())])
            .unmounting(vec![Err(RemovalCallError::NotMounted), Ok(())])
            .powering_off(Ok(()));
        assert!(matches!(
            remove(&fake),
            SafeRemovalOutcome::Removed { ref unmounted, .. } if unmounted == &["/mnt/b"]
        ));
    }

    // D: the second check refuses: nothing is powered off.
    #[test]
    fn a_second_check_refusal_powers_nothing_off() {
        let mut changed = facts();
        changed.snapshot.serial = "OTHER-SERIAL".to_string();
        let mut recreated_facts = facts();
        recreated_facts.snapshot = recreated();
        let mut shared = facts();
        shared.usb = UsbTopology::Bound { interfaces: 2 };
        let mut protected = facts();
        protected.snapshot.hint_system = true;
        let mut unknown = facts();
        unknown.usb = UsbTopology::Unknown;
        let relaid = mounted(vec![filesystem(
            "/org/freedesktop/UDisks2/block_devices/dm_2d0",
            BlockRole::Other,
            &[],
        )]);

        let cases: Vec<(&str, RemovalFactsOutcome)> = vec![
            ("gone", RemovalFactsOutcome::NotFound),
            ("identity changed", found(changed)),
            ("diskseq changed", found(recreated_facts)),
            ("topology changed", found(shared)),
            ("topology unknown", found(unknown)),
            ("protected", found(protected)),
            (
                "unreadable",
                RemovalFactsOutcome::Error("D-Bus".to_string()),
            ),
            ("layout changed", found(relaid)),
        ];
        for (name, second) in cases {
            let fake = Fake::new(vec![found(facts()), second]);
            let outcome = remove(&fake);
            let expected = match name {
                "gone" => matches!(outcome, SafeRemovalOutcome::DeviceGone),
                "identity changed" | "diskseq changed" => {
                    matches!(outcome, SafeRemovalOutcome::DeviceChanged(_))
                }
                "unreadable" => matches!(outcome, SafeRemovalOutcome::Unavailable(_)),
                _ => matches!(outcome, SafeRemovalOutcome::Unsupported(_)),
            };
            assert!(expected, "{name}: {outcome:?}");
            assert_eq!(fake.calls(), [Call::Facts, Call::Facts], "{name}");
        }
    }

    // E: mounted again after unmounting: stop -- no second round of
    // unmounting, no power-off.
    #[test]
    fn a_filesystem_mounted_again_stops_without_retrying() {
        let first = || mounted(vec![filesystem(PART1, BlockRole::Partition, &["/mnt/a"])]);
        let fake = Fake::new(vec![found(first()), found(first())]).unmounting(vec![Ok(())]);
        assert!(matches!(
            remove(&fake),
            SafeRemovalOutcome::Busy {
                stage: RemovalStage::PowerOff,
                ref unmounted,
            } if unmounted == &["/mnt/a"]
        ));
        assert_eq!(
            fake.calls(),
            [Call::Facts, Call::Unmount(PART1.to_string()), Call::Facts]
        );
    }

    // F: a failed power-off is never `Removed`.
    #[test]
    fn only_a_successful_power_off_is_removed() {
        let cases: Vec<(RemovalCallError, Expected)> = vec![
            (RemovalCallError::Busy, |outcome| {
                matches!(
                    outcome,
                    SafeRemovalOutcome::Busy {
                        stage: RemovalStage::PowerOff,
                        ..
                    }
                )
            }),
            (
                RemovalCallError::NotAuthorized(AuthorizationDenial::CanObtain),
                |outcome| {
                    matches!(
                        outcome,
                        SafeRemovalOutcome::NotAuthorized {
                            stage: RemovalStage::PowerOff,
                            ..
                        }
                    )
                },
            ),
            (rejected(), |outcome| {
                matches!(
                    outcome,
                    SafeRemovalOutcome::Failed {
                        stage: RemovalStage::PowerOff,
                        ..
                    }
                )
            }),
            (
                RemovalCallError::Connection("no bus".to_string()),
                |outcome| {
                    matches!(
                        outcome,
                        SafeRemovalOutcome::Failed {
                            error: RemovalActionError::Connection(_),
                            ..
                        }
                    )
                },
            ),
            // "Not mounted" means nothing for a power-off: a failure.
            (RemovalCallError::NotMounted, |outcome| {
                matches!(
                    outcome,
                    SafeRemovalOutcome::Failed {
                        stage: RemovalStage::PowerOff,
                        ..
                    }
                )
            }),
        ];
        for (error, expected) in cases {
            let fake =
                Fake::new(vec![found(facts()), found(facts())]).powering_off(Err(error.clone()));
            let outcome = remove(&fake);
            assert!(expected(&outcome), "{error:?}: {outcome:?}");
            assert!(!matches!(outcome, SafeRemovalOutcome::Removed { .. }));
        }
    }

    // F: the drive powered off is the one the second fresh check names,
    // never one remembered from before.
    #[test]
    fn the_power_off_uses_the_second_checks_drive_path() {
        let mut second = facts();
        second.snapshot.drive_path = "/org/freedesktop/UDisks2/drives/Renamed".to_string();
        let fake = Fake::new(vec![found(facts()), found(second)]).powering_off(Ok(()));
        assert!(matches!(remove(&fake), SafeRemovalOutcome::Removed { .. }));
        assert_eq!(
            fake.calls().last(),
            Some(&Call::PowerOff(
                "/org/freedesktop/UDisks2/drives/Renamed".to_string()
            ))
        );
    }

    fn changed(facts: RemovalFacts) -> TargetChange {
        match judge(facts) {
            Err(SafeRemovalOutcome::DeviceChanged(change)) => change,
            other => panic!("expected DeviceChanged, got {other:?}"),
        }
    }

    // ---- The reference ----

    #[test]
    fn debug_output_never_shows_the_serial() {
        let shown = format!("{:?}", target());
        assert!(shown.contains(DISK));
        assert!(!shown.contains(&usb_stick().serial), "{shown}");
    }

    // ---- Identity and instance ----

    #[test]
    fn the_same_device_and_instance_is_accepted() {
        let plan = judge(facts()).expect("an ordinary USB stick");
        assert_eq!(plan.drive_path, usb_stick().drive_path);
        assert!(plan.unmount.is_empty());
        assert_eq!(plan.device.model, "Test Model");
    }

    #[test]
    fn a_different_device_is_refused() {
        let mut facts = facts();
        facts.snapshot.serial = "OTHER-SERIAL".to_string();
        assert_eq!(
            changed(facts),
            TargetChange::Device {
                identity: IdentityComparison::Changed,
                instance: InstanceComparison::SameInstance,
            }
        );
    }

    #[test]
    fn a_recreated_instance_is_refused() {
        let mut facts = facts();
        facts.snapshot = recreated();
        assert_eq!(
            changed(facts),
            TargetChange::Device {
                identity: IdentityComparison::Same,
                instance: InstanceComparison::Recreated,
            }
        );
    }

    #[test]
    fn insufficient_identity_or_instance_information_is_refused() {
        let mut facts = facts();
        facts.snapshot.serial = String::new();
        assert!(matches!(
            changed(facts),
            TargetChange::Device {
                identity: IdentityComparison::InsufficientIdentity,
                ..
            }
        ));

        let mut facts = self::facts();
        facts.snapshot.diskseq = None;
        assert!(matches!(
            changed(facts),
            TargetChange::Device {
                instance: InstanceComparison::InsufficientInformation,
                ..
            }
        ));
    }

    #[test]
    fn a_different_block_path_is_refused() {
        let mut facts = facts();
        facts.snapshot.block_path = "/org/freedesktop/UDisks2/block_devices/sdy".to_string();
        assert_eq!(changed(facts), TargetChange::DifferentTarget);
    }

    // major:minor are never identity evidence: matching ones do not make a
    // different device acceptable, and changed ones alone change nothing.
    #[test]
    fn major_minor_is_not_identity() {
        let mut facts = self::facts();
        facts.snapshot.serial = "OTHER-SERIAL".to_string();
        assert!(matches!(
            judge(facts),
            Err(SafeRemovalOutcome::DeviceChanged(_))
        ));

        let mut facts = self::facts();
        facts.snapshot.major = 259;
        facts.snapshot.minor = 7;
        assert!(judge(facts).is_ok());
    }

    #[test]
    fn a_missing_or_unreadable_device_does_nothing() {
        assert!(matches!(
            decide(&target(), RemovalFactsOutcome::NotFound),
            Err(SafeRemovalOutcome::DeviceGone)
        ));
        assert!(matches!(
            decide(&target(), RemovalFactsOutcome::Error("D-Bus".to_string())),
            Err(SafeRemovalOutcome::Unavailable(
                RemovalUnavailable::DeviceInformation(reason)
            )) if reason == "D-Bus"
        ));
    }

    // ---- Topology ----

    #[test]
    fn only_usb_is_supported() {
        let mut facts = facts();
        facts.snapshot.connection_bus = "sdio".to_string();
        // An SD card on a non-USB bus is not "usb" even when removable.
        assert_eq!(unsupported(facts), RemovalUnsupported::NotUsb);
    }

    #[test]
    fn a_drive_that_cannot_be_powered_off_is_refused() {
        let mut facts = facts();
        facts.can_power_off = false;
        assert_eq!(unsupported(facts), RemovalUnsupported::CannotPowerOff);
    }

    #[test]
    fn a_shared_physical_device_is_refused() {
        let mut facts = facts();
        facts.other_siblings = 1;
        assert_eq!(unsupported(facts), RemovalUnsupported::SharedPhysicalDevice);
    }

    #[test]
    fn a_usb_device_with_several_interfaces_is_refused() {
        for interfaces in [0, 2, 3] {
            let mut facts = facts();
            facts.usb = UsbTopology::Bound { interfaces };
            assert_eq!(
                unsupported(facts),
                RemovalUnsupported::SharedPhysicalDevice,
                "{interfaces}"
            );
        }
    }

    #[test]
    fn an_unknown_topology_is_refused() {
        let mut facts = facts();
        facts.usb = UsbTopology::Unknown;
        assert_eq!(unsupported(facts), RemovalUnsupported::TopologyUnknown);

        let mut facts = self::facts();
        facts.sibling_id = String::new();
        assert_eq!(unsupported(facts), RemovalUnsupported::TopologyUnknown);
    }

    // ---- Protected states ----

    #[test]
    fn a_protected_device_is_refused() {
        let mut facts = facts();
        facts.snapshot.hint_system = true;
        assert_eq!(unsupported(facts), RemovalUnsupported::ProtectedDevice);

        let mut facts = self::facts();
        facts.snapshot.hint_ignore = true;
        assert_eq!(unsupported(facts), RemovalUnsupported::ProtectedDevice);
    }

    #[test]
    fn active_swap_is_refused() {
        let mut facts = facts();
        facts.snapshot.active_swap = true;
        assert_eq!(unsupported(facts), RemovalUnsupported::ActiveSwap);
    }

    #[test]
    fn complex_storage_is_refused() {
        let mut facts = facts();
        facts.snapshot.complex_storage = true;
        assert_eq!(unsupported(facts), RemovalUnsupported::ComplexStorage);
    }

    #[test]
    fn a_critical_mount_is_refused_not_unmounted() {
        for critical in ["/", "/boot", "/boot/efi"] {
            let facts = mounted(vec![filesystem(PART1, BlockRole::Partition, &[critical])]);
            assert_eq!(
                unsupported(facts),
                RemovalUnsupported::CriticalMount,
                "{critical}"
            );
        }
    }

    // ---- Mounted filesystems ----

    #[test]
    fn a_whole_disk_filesystem_is_unmounted() {
        let plan = judge(mounted(vec![filesystem(
            DISK,
            BlockRole::WholeDisk,
            &["/run/media/user/ISO"],
        )]))
        .expect("a mounted whole-disk filesystem");
        assert_eq!(
            plan.unmount,
            vec![UnmountTarget {
                object_path: DISK.to_string(),
                mount_points: vec!["/run/media/user/ISO".to_string()],
            }]
        );
    }

    #[test]
    fn partition_filesystems_are_unmounted_and_unmounted_ones_skipped() {
        let plan = judge(mounted(vec![
            filesystem(
                PART2,
                BlockRole::Partition,
                &["/run/media/user/B", "/mnt/b"],
            ),
            filesystem(PART1, BlockRole::Partition, &["/run/media/user/A"]),
            filesystem(DISK, BlockRole::WholeDisk, &[]),
        ]))
        .expect("mounted partitions");
        let paths: Vec<&str> = plan
            .unmount
            .iter()
            .map(|target| target.object_path.as_str())
            .collect();
        assert_eq!(paths, [PART1, PART2]);
        assert_eq!(plan.unmount[1].mount_points.len(), 2);
    }

    #[test]
    fn a_mount_no_object_accounts_for_is_refused() {
        // The device reports a mount (e.g. of the whole disk) but no object
        // carries it: never assume it is unmounted.
        let mut facts = facts();
        facts.snapshot.mount_points = vec!["/run/media/user/ISO".to_string()];
        assert_eq!(unavailable(facts), RemovalUnavailable::MountInformation);

        // And the other way round.
        let mut facts = mounted(vec![filesystem(PART1, BlockRole::Partition, &["/mnt/a"])]);
        facts.snapshot.mount_points.clear();
        assert_eq!(unavailable(facts), RemovalUnavailable::MountInformation);
    }

    #[test]
    fn an_object_outside_the_drive_is_refused() {
        let mut outside = filesystem(PART1, BlockRole::Partition, &["/mnt/a"]);
        outside.drive_path = "/org/freedesktop/UDisks2/drives/Other".to_string();
        assert_eq!(
            unavailable(mounted(vec![outside])),
            RemovalUnavailable::MountInformation
        );
    }

    #[test]
    fn an_object_that_is_not_what_it_says_is_refused() {
        // A "whole disk" that is not the disk, a "partition" that is.
        for facts in [
            mounted(vec![filesystem(PART1, BlockRole::WholeDisk, &["/mnt/a"])]),
            mounted(vec![filesystem(DISK, BlockRole::Partition, &["/mnt/a"])]),
        ] {
            assert_eq!(unavailable(facts), RemovalUnavailable::MountInformation);
        }
    }

    #[test]
    fn an_unrecognized_object_on_the_drive_is_refused() {
        for mount_points in [&["/mnt/x"][..], &[]] {
            let facts = mounted(vec![filesystem(
                "/org/freedesktop/UDisks2/block_devices/dm_2d0",
                BlockRole::Other,
                mount_points,
            )]);
            assert_eq!(unsupported(facts), RemovalUnsupported::UnrecognizedLayout);
        }
    }

    // ---- When removal is offered ----

    fn cancelled() -> Cancelled {
        Cancelled {
            image_size: 10,
            bytes_written: 5,
            reason: CancelReason::UserRequested,
            target_may_be_modified: true,
            retry_requires_fresh_gate: true,
        }
    }

    fn failed(stage: WriteStage) -> Failed {
        Failed {
            image_size: 10,
            bytes_written: 5,
            stage,
            cause: WriteJobFailureCause::Sync(io::Error::other("EIO")),
            target_may_be_modified: true,
            retry_requires_fresh_gate: true,
        }
    }

    fn verify_failed(reason: VerifyFailureReason) -> OperationOutcome {
        OperationOutcome::Failed(OperationError::Verify {
            failed: VerifyFailed {
                mode: VerifyMode::Full,
                verified_bytes: 3,
                reason,
            },
            image_size: 10,
        })
    }

    // Every outcome variant, with whether it offers Safe Removal.
    pub(in crate::orchestration) fn every_outcome() -> Vec<(&'static str, OperationOutcome, bool)> {
        use OperationOutcome::{Cancelled as C, Completed, Failed as F};
        let completed = |mode, skipped| Completed {
            verify: VerifySucceeded {
                mode,
                verified_bytes: if skipped { 0 } else { 10 },
                skipped,
            },
            image_size: 10,
        };
        vec![
            ("Completed Quick", completed(VerifyMode::Quick, false), true),
            ("Completed Full", completed(VerifyMode::Full, false), true),
            ("Completed None", completed(VerifyMode::None, true), true),
            ("Cancelled Preflight", C(CancelledAt::Preflight), false),
            (
                "Cancelled BeforeConfirmation",
                C(CancelledAt::BeforeConfirmation),
                false,
            ),
            (
                "Cancelled Confirmation",
                C(CancelledAt::Confirmation),
                false,
            ),
            (
                "Cancelled Write",
                C(CancelledAt::Write {
                    cancelled: cancelled(),
                    image_size: 10,
                }),
                true,
            ),
            ("Cancelled AfterSync", C(CancelledAt::AfterSync), true),
            ("Cancelled BeforeVerify", C(CancelledAt::BeforeVerify), true),
            (
                "Cancelled Verify",
                C(CancelledAt::Verify {
                    cancelled: VerifyCancelled {
                        mode: VerifyMode::Full,
                        verified_bytes: 3,
                    },
                    image_size: 10,
                }),
                true,
            ),
            (
                "Failed Target",
                F(OperationError::Target(TargetNotReady::Select(
                    SelectTargetError::NotFound,
                ))),
                false,
            ),
            (
                "Failed Image",
                F(OperationError::Image(PrepareImageError::Image(
                    ImageSourceError::NotRegularFile,
                ))),
                false,
            ),
            (
                "Failed Confirmation",
                F(OperationError::Confirmation(ConfirmError::Mismatch)),
                false,
            ),
            (
                "Failed ConfirmationInputClosed",
                F(OperationError::ConfirmationInputClosed),
                false,
            ),
            (
                "Failed ConfirmationInputFailed",
                F(OperationError::ConfirmationInputFailed(io::Error::other(
                    "stdin",
                ))),
                false,
            ),
            (
                "Failed WriteGate",
                F(OperationError::WriteGate(WriteGateError::IdentityChanged)),
                false,
            ),
            (
                "Failed WriteDeviceRejected",
                F(OperationError::WriteDeviceRejected {
                    error: WriteGateError::FdBindingMismatch,
                    open_device: None,
                }),
                false,
            ),
            (
                "Failed ImageBinding",
                F(OperationError::ImageBinding(
                    ImageBindingError::GenerationMismatch,
                )),
                false,
            ),
            (
                "Failed ReaderOpen",
                F(OperationError::ReaderOpen(io::Error::other("open"))),
                false,
            ),
            (
                "Failed Write",
                F(OperationError::Write {
                    failed: failed(WriteStage::Writing),
                    image_size: 10,
                }),
                true,
            ),
            (
                "Failed SyncWorkerPanicked",
                F(OperationError::SyncWorkerPanicked {
                    cancel_requested: false,
                }),
                false,
            ),
            (
                "Failed Sync",
                F(OperationError::Sync {
                    failed: failed(WriteStage::Syncing),
                    cancel_requested: false,
                    image_size: 10,
                }),
                true,
            ),
            (
                "Failed VerifyNotStarted TestPauseEnded",
                F(OperationError::VerifyNotStarted(
                    VerifyNotStarted::TestPauseEnded,
                )),
                false,
            ),
            (
                "Failed VerifyNotStarted TargetCheck",
                F(OperationError::VerifyNotStarted(
                    VerifyNotStarted::TargetCheck {
                        error: VerifyStartError::IdentityChanged,
                        diagnostics: None,
                        image_size: 10,
                    },
                )),
                false,
            ),
            (
                "Failed VerifyNotStarted Start",
                F(OperationError::VerifyNotStarted(VerifyNotStarted::Start {
                    error: VerifyStartError::OpenDeviceFailed,
                    open_device: None,
                    image_size: 10,
                })),
                false,
            ),
            (
                "Failed Verify mismatch",
                verify_failed(VerifyFailureReason::Mismatch {
                    offset: 3,
                    expected: 1,
                    actual: 2,
                }),
                true,
            ),
            (
                "Failed Verify runtime error",
                verify_failed(VerifyFailureReason::TargetReadError(io::Error::other(
                    "EIO",
                ))),
                true,
            ),
        ]
    }

    #[test]
    fn removal_is_offered_exactly_as_decided() {
        for (name, outcome, offered) in every_outcome() {
            assert_eq!(removal_allowed(&outcome), offered, "{name}");
        }
    }

    // The three the v0.1 decision singled out.
    #[test]
    fn no_removal_after_a_sync_panic_a_changed_verify_target_or_a_test_pause() {
        let refused: Vec<&str> = every_outcome()
            .into_iter()
            .filter(|(_, outcome, _)| !removal_allowed(outcome))
            .map(|(name, _, _)| name)
            .collect();
        for name in [
            "Failed SyncWorkerPanicked",
            "Failed VerifyNotStarted TargetCheck",
            "Failed VerifyNotStarted TestPauseEnded",
        ] {
            assert!(refused.contains(&name), "{name}");
        }
    }

    // `every_outcome` covers every variant `removal_allowed` decides (its
    // match has no wildcard, so a new variant fails to compile there; this
    // keeps the table here complete as well).
    #[test]
    fn the_outcome_table_covers_every_variant() {
        assert_eq!(every_outcome().len(), 27);
    }
}

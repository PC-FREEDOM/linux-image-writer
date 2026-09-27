// Device candidates and target selection (orchestration, Core layer): what a
// UI shows as the list of devices, and how the device the user picked is
// turned into a `SelectionState`. A UI never builds a `DeviceSnapshot` or
// judges a device itself -- the list comes from the Linux Backend's normal,
// fail-closed collection, each entry carries the Safety Engine's own
// assessment, and whether it can be selected is `core::selectability()`,
// the same decision `core::select()` makes.

use super::platform::Platform;
use crate::device::{DeviceSnapshot, SnapshotFetchOutcome};
use crate::execution::core::{self, NotSelectableReason, Selectability, SelectionState};
use crate::identity::{IdentityComparison, InstanceComparison, compare_identity, compare_instance};
use crate::linux_backend::collect_device_snapshots;
use crate::safety::{SafetyAssessment, assess_device};

// An opaque reference to the device a caller wants to select, handed back
// to `select_target()`. It carries no authority: selecting always re-reads
// the device, and the fresh snapshot -- never anything stored here -- is
// what `core::select()` judges.
//
// A reference taken from the candidate list (`DeviceCandidate::target()`)
// also remembers the snapshot that list entry was built from, so selecting
// it requires the device to still be the same one (Identity) and the same
// block-device instance (diskseq) the user saw. A reference built from a
// bare block path (`from_block_path`, what the CLI's `<udisks2-block-object-
// path>` argument means) has nothing to compare with and is only the
// unverified path; the fields are private, so neither kind can be forged or
// turned into the other.
#[derive(Debug, Clone)]
pub(crate) struct TargetRef {
    block_path: String,
    origin: TargetOrigin,
}

#[derive(Debug, Clone)]
enum TargetOrigin {
    // Built from this snapshot by the candidate list (used from tests until
    // a UI exists).
    #[allow(dead_code)]
    Listed(DeviceSnapshot),
    // Only a block path was given.
    BlockPathOnly,
}

// How a freshly read snapshot compares with what a `TargetRef` remembers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetCheck {
    // The reference came from a bare block path: nothing to compare.
    Unverified,
    // Same device (Identity `Same`) and same instance (`SameInstance`).
    Unchanged,
    Changed(TargetChange),
}

// Why a listed target no longer counts as the one the user saw. Anything
// short of `Same` + `SameInstance` -- including insufficient information --
// is a change: the user has to pick again from a fresh list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetChange {
    // The fresh snapshot is for a different block path (not expected from
    // `select_target()`, which reads by the reference's own path).
    DifferentTarget,
    // Fields are read through Debug output only, until a UI reads them.
    #[allow(dead_code)]
    Device {
        identity: IdentityComparison,
        instance: InstanceComparison,
    },
}

impl TargetRef {
    pub(crate) fn from_block_path(block_path: impl Into<String>) -> Self {
        TargetRef {
            block_path: block_path.into(),
            origin: TargetOrigin::BlockPathOnly,
        }
    }

    pub(crate) fn block_path(&self) -> &str {
        &self.block_path
    }

    pub(crate) fn check_against(&self, current: &DeviceSnapshot) -> TargetCheck {
        let listed = match &self.origin {
            TargetOrigin::BlockPathOnly => return TargetCheck::Unverified,
            TargetOrigin::Listed(listed) => listed,
        };

        if current.block_path != self.block_path {
            return TargetCheck::Changed(TargetChange::DifferentTarget);
        }

        let identity = compare_identity(listed, current);
        let instance = compare_instance(listed, current);
        if identity == IdentityComparison::Same && instance == InstanceComparison::SameInstance {
            TargetCheck::Unchanged
        } else {
            TargetCheck::Changed(TargetChange::Device { identity, instance })
        }
    }
}

// What a UI shows for a device: a read-only copy of the descriptive fields.
// Nothing here is read back for any decision; the Safety Engine's view is in
// `DeviceCandidate::assessment()`.
// The CLI's confirmation summary reads most fields; `read_only` and
// `media_available` wait for a UI.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceDisplay {
    pub(crate) device: String,
    pub(crate) vendor: String,
    pub(crate) model: String,
    pub(crate) serial: String,
    pub(crate) size: u64,
    pub(crate) connection_bus: String,
    pub(crate) removable: bool,
    pub(crate) read_only: bool,
    pub(crate) media_available: bool,
    pub(crate) mount_points: Vec<String>,
}

impl DeviceDisplay {
    pub(crate) fn from_snapshot(snapshot: &DeviceSnapshot) -> Self {
        DeviceDisplay {
            device: snapshot.device.clone(),
            vendor: snapshot.vendor.clone(),
            model: snapshot.model.clone(),
            serial: snapshot.serial.clone(),
            size: snapshot.size,
            connection_bus: snapshot.connection_bus.clone(),
            removable: snapshot.removable,
            read_only: snapshot.read_only,
            media_available: snapshot.media_available,
            mount_points: snapshot.mount_points.clone(),
        }
    }
}

// One entry of the device list. Built only by `list_candidates()`; the
// fields are private and have no setters, so the assessment and
// selectability shown are always the ones computed from the same snapshot
// the entry's `TargetRef` remembers.
// Read by a future UI; built and checked by tests today.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct DeviceCandidate {
    target: TargetRef,
    display: DeviceDisplay,
    assessment: SafetyAssessment,
    selectability: Selectability,
}

// Read by a future UI; built and checked by tests today.
#[allow(dead_code)]
impl DeviceCandidate {
    pub(crate) fn target(&self) -> &TargetRef {
        &self.target
    }

    pub(crate) fn display(&self) -> &DeviceDisplay {
        &self.display
    }

    pub(crate) fn assessment(&self) -> &SafetyAssessment {
        &self.assessment
    }

    pub(crate) fn selectability(&self) -> &Selectability {
        &self.selectability
    }
}

// Why the device list could not be produced. The backend reports any
// collection failure (D-Bus, an object or property it cannot read,
// /proc/swaps) as an error for the whole list -- never a partial list -- and
// that error is passed on as it is (its non-D-Bus causes are already text at
// the backend boundary; Phase 3A keeps that).
// Read by a future UI; built and checked by tests today.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum CandidateListError {
    Collection(zbus::Error),
}

// Why `select_target()` did not produce a `Selected` state.
#[derive(Debug)]
pub(crate) enum SelectTargetError {
    // The block path no longer exists.
    NotFound,
    // The device's information could not be read (backend fail-closed; the
    // backend already reduced the cause to text).
    SnapshotUnavailable(String),
    // A listed target is no longer the device the user saw.
    CandidateChanged(TargetChange),
    // `core::select()` refused it; every failed condition.
    // The reasons are read through Debug output only, until a UI reads them.
    #[allow(dead_code)]
    NotSelectable(Vec<NotSelectableReason>),
}

// Every whole disk the backend reports, each with its Safety Engine
// assessment and selectability.
// Called by a future UI; its pieces are tested today.
#[allow(dead_code)]
pub(crate) fn list_candidates() -> Result<Vec<DeviceCandidate>, CandidateListError> {
    candidates_from(collect_device_snapshots())
}

// See `list_candidates`.
#[allow(dead_code)]
fn candidates_from(
    collected: zbus::Result<Vec<DeviceSnapshot>>,
) -> Result<Vec<DeviceCandidate>, CandidateListError> {
    let snapshots = collected.map_err(CandidateListError::Collection)?;
    Ok(snapshots.into_iter().map(candidate_from_snapshot).collect())
}

// See `list_candidates`.
#[allow(dead_code)]
fn candidate_from_snapshot(snapshot: DeviceSnapshot) -> DeviceCandidate {
    let assessment = assess_device(&snapshot);
    let selectability = core::selectability(&snapshot, &assessment);
    let display = DeviceDisplay::from_snapshot(&snapshot);

    DeviceCandidate {
        target: TargetRef {
            block_path: snapshot.block_path.clone(),
            origin: TargetOrigin::Listed(snapshot),
        },
        display,
        assessment,
        selectability,
    }
}

// Selects the referenced device: reads a fresh snapshot of it, checks a
// listed reference still names the same device and instance, then asks
// `core::select()`.
pub(crate) fn select_target(
    platform: &impl Platform,
    target: &TargetRef,
) -> Result<SelectionState, SelectTargetError> {
    select_from_outcome(target, platform.fetch_snapshot(target.block_path()))
}

fn select_from_outcome(
    target: &TargetRef,
    fetched: SnapshotFetchOutcome,
) -> Result<SelectionState, SelectTargetError> {
    let snapshot = match fetched {
        SnapshotFetchOutcome::Found(snapshot) => snapshot,
        SnapshotFetchOutcome::NotFound => return Err(SelectTargetError::NotFound),
        SnapshotFetchOutcome::Error(reason) => {
            return Err(SelectTargetError::SnapshotUnavailable(reason));
        }
    };

    if let TargetCheck::Changed(change) = target.check_against(&snapshot) {
        return Err(SelectTargetError::CandidateChanged(change));
    }

    core::select(snapshot).map_err(|core::SelectionError::NotSelectable(reasons)| {
        SelectTargetError::NotSelectable(reasons)
    })
}

// Lets other modules' tests hold a reference exactly as the candidate list
// builds it, without a D-Bus call.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::{DeviceSnapshot, TargetRef, candidate_from_snapshot};

    pub(crate) fn listed(snapshot: DeviceSnapshot) -> TargetRef {
        candidate_from_snapshot(snapshot).target().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::RiskLevel;

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

    // Every combination of the fields the Safety Engine and `select()` read:
    // 10 booleans (size zero or not included) x 4 mount-point sets.
    fn all_variants() -> Vec<DeviceSnapshot> {
        let mounts: [&[&str]; 4] = [&[], &["/"], &["/boot/efi"], &["/media/user/STICK"]];
        let mut variants = Vec::new();
        for bits in 0u32..(1 << 10) {
            for mount_points in mounts {
                let bit = |n: u32| bits & (1 << n) != 0;
                let mut s = usb_stick();
                s.read_only = bit(0);
                s.media_available = !bit(1);
                s.size = if bit(2) { 0 } else { 8_000_000_000 };
                s.hint_system = bit(3);
                s.hint_ignore = bit(4);
                s.hint_partitionable = !bit(5);
                s.removable = !bit(6);
                s.connection_bus = if bit(7) { "ata" } else { "usb" }.to_string();
                s.active_swap = bit(8);
                s.complex_storage = bit(9);
                s.mount_points = mount_points.iter().map(|m| m.to_string()).collect();
                variants.push(s);
            }
        }
        variants
    }

    // The selection rule exactly as `select()` wrote it before the
    // conditions moved into `selectability()`, kept here as the oracle.
    fn selectable_before_3a2(snapshot: &DeviceSnapshot) -> bool {
        let assessment = assess_device(snapshot);
        assessment.writable
            && matches!(assessment.risk_level, RiskLevel::Normal)
            && snapshot.media_available
            && snapshot.size > 0
    }

    // 16.1: for every variant, `select()`, `selectability()`, the candidate
    // list and the CLI path (`select_target` on a bare block path) all agree
    // with the pre-3A-2 rule -- nothing that was selectable stops being so,
    // and nothing refused becomes selectable.
    #[test]
    fn selectability_matches_select_for_every_variant() {
        let variants = all_variants();
        let mut selectable = 0;
        for snapshot in variants {
            let expected = selectable_before_3a2(&snapshot);
            let label = format!("{snapshot:?}");

            let assessment = assess_device(&snapshot);
            assert_eq!(
                core::selectability(&snapshot, &assessment).is_selectable(),
                expected,
                "{label}"
            );
            assert_eq!(core::select(snapshot.clone()).is_ok(), expected, "{label}");

            let candidate = candidate_from_snapshot(snapshot.clone());
            assert_eq!(
                candidate.selectability().is_selectable(),
                expected,
                "{label}"
            );

            let cli = select_from_outcome(
                &TargetRef::from_block_path(snapshot.block_path.clone()),
                SnapshotFetchOutcome::Found(snapshot.clone()),
            );
            assert_eq!(cli.is_ok(), expected, "{label}");

            let listed =
                select_from_outcome(candidate.target(), SnapshotFetchOutcome::Found(snapshot));
            assert_eq!(listed.is_ok(), expected, "{label}");

            selectable += usize::from(expected);
        }
        // The variants include selectable devices, not only refusals.
        assert!(selectable > 0);
    }

    // 16.2: each reason is reported exactly when its condition fails, the
    // list is never empty for a refusal, and `select()` returns the same
    // reasons `selectability()` computed.
    #[test]
    fn reasons_follow_their_conditions() {
        for snapshot in all_variants() {
            let assessment = assess_device(&snapshot);
            let expected: Vec<NotSelectableReason> = [
                (!assessment.writable, NotSelectableReason::NotWritable),
                (
                    !matches!(assessment.risk_level, RiskLevel::Normal),
                    NotSelectableReason::RiskNotNormal,
                ),
                (
                    !snapshot.media_available,
                    NotSelectableReason::MediaUnavailable,
                ),
                (snapshot.size == 0, NotSelectableReason::ZeroSize),
            ]
            .into_iter()
            .filter_map(|(failed, reason)| failed.then_some(reason))
            .collect();

            let label = format!("{snapshot:?}");
            match core::selectability(&snapshot, &assessment) {
                Selectability::Selectable => assert!(expected.is_empty(), "{label}"),
                Selectability::NotSelectable(reasons) => {
                    assert!(!reasons.is_empty(), "{label}");
                    assert_eq!(reasons, expected, "{label}");
                }
            }
            match core::select(snapshot) {
                Ok(_) => assert!(expected.is_empty(), "{label}"),
                Err(core::SelectionError::NotSelectable(reasons)) => {
                    assert_eq!(reasons, expected, "{label}")
                }
            }
        }
    }

    // 16.2: concrete refusals, and how they relate to the Safety Engine's
    // own `RiskReason`s (kept separate: those say why the device was judged
    // so, these say which selection condition failed).
    #[test]
    fn typical_refusals_report_their_reasons() {
        use crate::safety::RiskReason;
        let reasons = |snapshot: &DeviceSnapshot| match core::selectability(
            snapshot,
            &assess_device(snapshot),
        ) {
            Selectability::Selectable => Vec::new(),
            Selectability::NotSelectable(reasons) => reasons,
        };
        let risk = |snapshot: &DeviceSnapshot| format!("{:?}", assess_device(snapshot).reasons);

        let mut system = usb_stick();
        system.mount_points = vec!["/".to_string()];
        assert_eq!(
            reasons(&system),
            [
                NotSelectableReason::NotWritable,
                NotSelectableReason::RiskNotNormal
            ]
        );
        assert_eq!(risk(&system), format!("{:?}", [RiskReason::CriticalMount]));

        let mut read_only = usb_stick();
        read_only.read_only = true;
        assert_eq!(
            reasons(&read_only),
            [
                NotSelectableReason::NotWritable,
                NotSelectableReason::RiskNotNormal
            ]
        );
        assert_eq!(risk(&read_only), format!("{:?}", [RiskReason::ReadOnly]));

        let mut no_media = usb_stick();
        no_media.media_available = false;
        assert_eq!(
            reasons(&no_media),
            [
                NotSelectableReason::NotWritable,
                NotSelectableReason::RiskNotNormal,
                NotSelectableReason::MediaUnavailable
            ]
        );

        let mut empty = usb_stick();
        empty.size = 0;
        assert_eq!(
            reasons(&empty),
            [
                NotSelectableReason::NotWritable,
                NotSelectableReason::RiskNotNormal,
                NotSelectableReason::ZeroSize
            ]
        );

        assert_eq!(reasons(&usb_stick()), []);
        assert_eq!(
            risk(&usb_stick()),
            format!("{:?}", [RiskReason::UsbRemovable])
        );
    }

    // A candidate shows exactly what its snapshot says, with the Safety
    // Engine's own assessment, and its reference remembers that snapshot.
    #[test]
    fn a_candidate_is_built_from_one_snapshot() {
        let mut snapshot = usb_stick();
        snapshot.mount_points = vec!["/media/user/STICK".to_string()];
        let candidate = candidate_from_snapshot(snapshot.clone());

        assert_eq!(
            candidate.display(),
            &DeviceDisplay {
                device: "/dev/sdx".to_string(),
                vendor: "Test Vendor".to_string(),
                model: "Test Model".to_string(),
                serial: "TEST-SERIAL-0001".to_string(),
                size: 8_000_000_000,
                connection_bus: "usb".to_string(),
                removable: true,
                read_only: false,
                media_available: true,
                mount_points: vec!["/media/user/STICK".to_string()],
            }
        );
        assert_eq!(
            format!("{:?}", candidate.assessment()),
            format!("{:?}", assess_device(&snapshot))
        );
        assert!(!candidate.selectability().is_selectable());
        assert_eq!(candidate.target().block_path(), snapshot.block_path);
        assert_eq!(
            candidate.target().check_against(&snapshot),
            TargetCheck::Unchanged
        );
    }

    // 16.3: a listed reference compares a fresh snapshot with the one it was
    // listed from; a bare block path cannot.
    #[test]
    fn a_listed_target_detects_a_changed_device_or_instance() {
        let listed = candidate_from_snapshot(usb_stick()).target().clone();
        let check = |edit: fn(&mut DeviceSnapshot)| {
            let mut current = usb_stick();
            edit(&mut current);
            listed.check_against(&current)
        };

        assert_eq!(check(|_| {}), TargetCheck::Unchanged);
        assert_eq!(
            check(|s| s.diskseq = Some(13)),
            TargetCheck::Changed(TargetChange::Device {
                identity: IdentityComparison::Same,
                instance: InstanceComparison::Recreated,
            })
        );
        assert_eq!(
            check(|s| s.diskseq = None),
            TargetCheck::Changed(TargetChange::Device {
                identity: IdentityComparison::Same,
                instance: InstanceComparison::InsufficientInformation,
            })
        );
        assert_eq!(
            check(|s| s.serial = "OTHER-SERIAL".to_string()),
            TargetCheck::Changed(TargetChange::Device {
                identity: IdentityComparison::Changed,
                instance: InstanceComparison::SameInstance,
            })
        );
        assert_eq!(
            check(|s| s.serial = String::new()),
            TargetCheck::Changed(TargetChange::Device {
                identity: IdentityComparison::InsufficientIdentity,
                instance: InstanceComparison::SameInstance,
            })
        );
        assert_eq!(
            check(|s| s.block_path = "/org/freedesktop/UDisks2/block_devices/sdy".to_string()),
            TargetCheck::Changed(TargetChange::DifferentTarget)
        );

        let mut replaced = usb_stick();
        replaced.serial = "OTHER-SERIAL".to_string();
        assert_eq!(
            TargetRef::from_block_path(usb_stick().block_path).check_against(&replaced),
            TargetCheck::Unverified
        );
    }

    // Selecting a listed target refuses a changed device before asking
    // `select()`, and a still-identical one is judged afresh (a listed
    // device that became unsafe is refused).
    #[test]
    fn selecting_a_listed_target_requires_the_same_device() {
        let listed = candidate_from_snapshot(usb_stick()).target().clone();

        let selected = select_from_outcome(&listed, SnapshotFetchOutcome::Found(usb_stick()));
        match selected {
            Ok(SelectionState::Selected { baseline, .. }) => {
                assert_eq!(baseline.block_path, usb_stick().block_path)
            }
            other => panic!("expected Selected, got {other:?}"),
        }

        let mut recreated = usb_stick();
        recreated.diskseq = Some(13);
        assert!(matches!(
            select_from_outcome(&listed, SnapshotFetchOutcome::Found(recreated)),
            Err(SelectTargetError::CandidateChanged(TargetChange::Device {
                instance: InstanceComparison::Recreated,
                ..
            }))
        ));

        let mut now_mounted = usb_stick();
        now_mounted.mount_points = vec!["/".to_string()];
        assert!(matches!(
            select_from_outcome(&listed, SnapshotFetchOutcome::Found(now_mounted)),
            Err(SelectTargetError::NotSelectable(_))
        ));
    }

    // 16.4: nothing is selected or listed from information that could not
    // be read -- the backend's failures come through as errors, never as an
    // empty or partial result.
    #[test]
    fn collection_failures_stay_failures() {
        let target = TargetRef::from_block_path(usb_stick().block_path);
        assert!(matches!(
            select_from_outcome(&target, SnapshotFetchOutcome::NotFound),
            Err(SelectTargetError::NotFound)
        ));
        match select_from_outcome(
            &target,
            SnapshotFetchOutcome::Error("missing property".to_string()),
        ) {
            Err(SelectTargetError::SnapshotUnavailable(reason)) => {
                assert_eq!(reason, "missing property")
            }
            other => panic!("expected SnapshotUnavailable, got {other:?}"),
        }

        let listed = candidate_from_snapshot(usb_stick()).target().clone();
        assert!(matches!(
            select_from_outcome(&listed, SnapshotFetchOutcome::NotFound),
            Err(SelectTargetError::NotFound)
        ));

        match candidates_from(Err(zbus::Error::Failure(
            "cannot read /proc/swaps".to_string(),
        ))) {
            Err(CandidateListError::Collection(zbus::Error::Failure(reason))) => {
                assert_eq!(reason, "cannot read /proc/swaps")
            }
            other => panic!("expected a collection error, got {other:?}"),
        }

        let listed = candidates_from(Ok(vec![usb_stick()])).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].selectability().is_selectable());
    }
}

// What Safe Removal reads about one device (Linux Backend: collection
// only). Everything UDisks2 says comes from one GetManagedObjects tree, so
// the snapshot, the drive's properties and the filesystems describe the
// same moment; the snapshot itself is the normal one
// (`snapshot_from_objects`, unchanged). sysfs adds the USB topology, and is
// trusted only when it is tied to this very device instance (its diskseq).
//
// Nothing here judges: whether a device may be removed is
// `orchestration::removal`'s decision (`decide` / `assess`). Nothing here
// writes, unmounts or powers anything off either.
//
// Used by the CLI's read-only `removal-check` now and by Safe Removal
// itself later (SR-4); unused in the library build until then.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::{fs, io};

use zbus::blocking::Connection;
use zbus::zvariant::OwnedObjectPath;

use super::{
    BLOCK, CollectionError, DRIVE, FILESYSTEM, ManagedObjects, Object, fetch_managed_objects,
    filesystem_mount_points, merge_mount_points, partition_objects, read_active_swaps,
    read_diskseq, snapshot_from_objects,
};
use crate::device::{
    BlockFilesystem, BlockRole, DeviceSnapshot, RemovalFacts, RemovalFactsOutcome, UsbTopology,
};

// ---- sysfs ----

// The two sysfs reads the topology needs, behind a seam so tests need no
// real sysfs. `None` for anything that cannot be read.
pub(crate) trait Sysfs {
    // The canonical path `path` resolves to.
    fn realpath(&self, path: &Path) -> Option<PathBuf>;
    // A file's contents.
    fn read(&self, path: &Path) -> Option<String>;
}

pub(crate) struct LinuxSysfs;

impl Sysfs for LinuxSysfs {
    fn realpath(&self, path: &Path) -> Option<PathBuf> {
        fs::canonicalize(path).ok()
    }

    fn read(&self, path: &Path) -> Option<String> {
        fs::read_to_string(path).ok()
    }
}

// How the drive's USB interface (UDisks2's SiblingId) was tied to this
// block device instance in sysfs, or the first step that failed. Only
// `Bound` becomes `UsbTopology::Bound`; every other value is `Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SysfsBinding {
    Bound { interfaces: u32 },
    // UDisks2 reports no SiblingId.
    NoSiblingId,
    // The SiblingId is not a USB interface's sysfs path
    // (`/sys/devices/…/<dev>/<dev>:<config>.<interface>`).
    UnrecognizedSiblingId,
    // The device node is not a plain `/dev/<name>`.
    UnrecognizedDeviceName,
    // `/sys/block/<name>` does not resolve.
    BlockNotResolved,
    // The block device is not under that USB interface.
    NotUnderInterface,
    // The snapshot has no diskseq to compare with.
    NoDiskseq,
    DiskseqUnreadable,
    // sysfs reports a different instance than the snapshot.
    DiskseqMismatch,
    InterfacesUnreadable,
    InterfacesInvalid,
}

impl SysfsBinding {
    fn topology(self) -> UsbTopology {
        match self {
            SysfsBinding::Bound { interfaces } => UsbTopology::Bound { interfaces },
            _ => UsbTopology::Unknown,
        }
    }
}

// Ties `sibling_id` (a USB interface's sysfs path, as UDisks2 reports it)
// to the block device `device_node` with `diskseq`: the block device must
// resolve to a path strictly under that interface (compared by path
// components, never as a string prefix), sysfs must report the same diskseq
// there, and the interface's USB device must report its bNumInterfaces.
// No step is guessed or skipped.
pub(crate) fn bind_usb_topology(
    sysfs: &dyn Sysfs,
    sibling_id: &str,
    device_node: &str,
    diskseq: Option<u64>,
) -> SysfsBinding {
    if sibling_id.is_empty() {
        return SysfsBinding::NoSiblingId;
    }
    let interface = Path::new(sibling_id);
    let Some(usb_device) = usb_device_of_interface(interface) else {
        return SysfsBinding::UnrecognizedSiblingId;
    };
    let Some(name) = device_node
        .strip_prefix("/dev/")
        .filter(|name| is_plain_name(name))
    else {
        return SysfsBinding::UnrecognizedDeviceName;
    };
    let Some(block) = sysfs.realpath(&Path::new("/sys/block").join(name)) else {
        return SysfsBinding::BlockNotResolved;
    };
    if block == interface || !block.starts_with(interface) {
        return SysfsBinding::NotUnderInterface;
    }
    let Some(expected) = diskseq else {
        return SysfsBinding::NoDiskseq;
    };
    // Read from the resolved directory itself, so it is the same device the
    // containment check just looked at.
    let Some(found) = sysfs
        .read(&block.join("diskseq"))
        .and_then(|text| text.trim().parse::<u64>().ok())
    else {
        return SysfsBinding::DiskseqUnreadable;
    };
    if found != expected {
        return SysfsBinding::DiskseqMismatch;
    }
    let Some(text) = sysfs.read(&usb_device.join("bNumInterfaces")) else {
        return SysfsBinding::InterfacesUnreadable;
    };
    match text.trim().parse::<u32>() {
        Ok(interfaces) => SysfsBinding::Bound { interfaces },
        Err(_) => SysfsBinding::InterfacesInvalid,
    }
}

// The USB device directory of a USB interface's sysfs path, or `None` if
// `interface` is not one: an absolute path under /sys/devices with no `.`
// or `..`, ending in `<dev>:<config>.<interface>` (e.g. `2-1:1.0`,
// `3-1.4:1.0`) directly under its device directory `<dev>` (`2-1`).
fn usb_device_of_interface(interface: &Path) -> Option<&Path> {
    // Checked on the text itself: `Path::components` silently drops an
    // inner `.` and repeated slashes.
    let plain = interface.to_str().is_some_and(|text| {
        text.strip_prefix('/')
            .is_some_and(|rest| rest.split('/').all(is_plain_name))
    });
    if !plain || !interface.starts_with("/sys/devices") {
        return None;
    }
    let name = interface.file_name()?.to_str()?;
    let (device, rest) = name.split_once(':')?;
    let (configuration, number) = rest.split_once('.')?;
    let device_ok = device
        .split_once('-')
        .is_some_and(|(bus, ports)| is_number(bus) && ports.split('.').all(is_number));
    if !device_ok || !is_number(configuration) || !is_number(number) {
        return None;
    }
    let usb_device = interface.parent()?;
    (usb_device.file_name()?.to_str()? == device).then_some(usb_device)
}

fn is_number(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

// A single path component that means nothing special.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/')
}

// ---- The facts ----

// The facts for `block_path` from one tree (plus sysfs), with how the USB
// topology was bound: `Ok(None)` when the path is not a whole disk with a
// drive, an error when anything UDisks2 must report cannot be read. A sysfs
// failure is not an error: the facts are kept, with an unknown topology.
fn facts_from_objects(
    objects: &ManagedObjects,
    block_path: &str,
    active_swaps: &[String],
    read_diskseq: &dyn Fn(&str) -> Option<u64>,
    sysfs: &dyn Sysfs,
) -> Result<Option<(RemovalFacts, SysfsBinding)>, CollectionError> {
    let Some(snapshot) = snapshot_from_objects(objects, block_path, active_swaps, read_diskseq)?
    else {
        return Ok(None);
    };

    let drive = Object::find(objects, &snapshot.drive_path)?;
    let can_power_off: bool = drive.get(DRIVE, "CanPowerOff")?;
    let sibling_id: String = drive.get(DRIVE, "SiblingId")?;
    let other_siblings = other_drives_sharing(objects, &snapshot.drive_path, &sibling_id)?;
    let filesystems = drive_filesystems(objects, &snapshot)?;

    let binding = bind_usb_topology(sysfs, &sibling_id, &snapshot.device, snapshot.diskseq);

    Ok(Some((
        RemovalFacts {
            snapshot,
            can_power_off,
            sibling_id,
            other_siblings,
            usb: binding.topology(),
            filesystems,
        },
        binding,
    )))
}

// How many other drives report the same, non-empty SiblingId. Every drive's
// SiblingId must be readable.
fn other_drives_sharing(
    objects: &ManagedObjects,
    drive_path: &str,
    sibling_id: &str,
) -> Result<usize, CollectionError> {
    let mut count = 0;
    for (path, interfaces) in objects {
        if path == drive_path || !interfaces.contains_key(DRIVE) {
            continue;
        }
        let other: String = Object::find(objects, path)?.get(DRIVE, "SiblingId")?;
        if !sibling_id.is_empty() && other == sibling_id {
            count += 1;
        }
    }
    Ok(count)
}

// Every Block object with a Filesystem whose Block.Drive is the snapshot's
// drive, mounted or not, in object-path order. Its role is read from the
// tree: the disk itself, one of the disk's own PartitionTable entries, or
// anything else (`Other`) -- sharing the drive never makes an object a
// partition.
fn drive_filesystems(
    objects: &ManagedObjects,
    snapshot: &DeviceSnapshot,
) -> Result<Vec<BlockFilesystem>, CollectionError> {
    let disk = Object::find(objects, &snapshot.block_path)?;
    let partitions: Vec<&str> = partition_objects(objects, disk)?
        .iter()
        .map(|partition| partition.path)
        .collect();

    let mut paths: Vec<&String> = objects.keys().collect();
    paths.sort();

    let mut filesystems = Vec::new();
    for path in paths {
        let object = Object::find(objects, path)?;
        if !object.has(BLOCK) || !object.has(FILESYSTEM) {
            continue;
        }
        let drive: OwnedObjectPath = object.get(BLOCK, "Drive")?;
        if drive.as_str() != snapshot.drive_path {
            continue;
        }
        let role = if path == &snapshot.block_path {
            BlockRole::WholeDisk
        } else if partitions.contains(&path.as_str()) {
            BlockRole::Partition
        } else {
            BlockRole::Other
        };
        let mount_points = merge_mount_points(std::iter::once(filesystem_mount_points(object)?));
        filesystems.push(BlockFilesystem {
            object_path: path.clone(),
            drive_path: drive.as_str().to_string(),
            role,
            mount_points,
        });
    }
    Ok(filesystems)
}

fn outcome(
    result: Result<Option<(RemovalFacts, SysfsBinding)>, CollectionError>,
) -> Result<Option<(RemovalFacts, SysfsBinding)>, String> {
    result.map_err(|error| error.to_string())
}

// One tree, one read of /proc/swaps: either failing is an error, never
// facts built from partial information.
fn collect(
    objects: Result<ManagedObjects, CollectionError>,
    active_swaps: io::Result<Vec<String>>,
    block_path: &str,
    read_diskseq: &dyn Fn(&str) -> Option<u64>,
    sysfs: &dyn Sysfs,
) -> Result<Option<(RemovalFacts, SysfsBinding)>, String> {
    outcome(objects.and_then(|objects| {
        let active_swaps = active_swaps.map_err(CollectionError::Swaps)?;
        facts_from_objects(&objects, block_path, &active_swaps, read_diskseq, sysfs)
    }))
}

fn collect_now(block_path: &str) -> Result<Option<(RemovalFacts, SysfsBinding)>, String> {
    let connection = Connection::system().map_err(|error| error.to_string())?;
    collect(
        fetch_managed_objects(&connection),
        read_active_swaps(),
        block_path,
        &read_diskseq,
        &LinuxSysfs,
    )
}

// The facts Safe Removal judges, freshly read for `block_path`.
pub(crate) fn collect_removal_facts(block_path: &str) -> RemovalFactsOutcome {
    match collect_now(block_path) {
        Ok(Some((facts, _))) => RemovalFactsOutcome::Found(Box::new(facts)),
        Ok(None) => RemovalFactsOutcome::NotFound,
        Err(error) => RemovalFactsOutcome::Error(error),
    }
}

// The same, with how the USB topology was bound: for the CLI's read-only
// diagnostic only.
pub(crate) fn collect_removal_diagnostics(
    block_path: &str,
) -> Result<Option<(RemovalFacts, SysfsBinding)>, String> {
    collect_now(block_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use zbus::zvariant::{ObjectPath, OwnedValue, Value};

    const DISK: &str = "/org/freedesktop/UDisks2/block_devices/sdx";
    const DRIVE_PATH: &str = "/org/freedesktop/UDisks2/drives/Test_Model_TEST_SERIAL";
    const OTHER_DRIVE: &str = "/org/freedesktop/UDisks2/drives/Other_Model_OTHER_SERIAL";
    const INTERFACE: &str = "/sys/devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0";
    const USB_DEVICE: &str = "/sys/devices/pci0000:00/0000:00:14.0/usb2/2-1";
    const PARTITION_TABLE: &str = "org.freedesktop.UDisks2.PartitionTable";
    const PARTITION: &str = "org.freedesktop.UDisks2.Partition";

    fn value<'a>(value: impl Into<Value<'a>>) -> OwnedValue {
        OwnedValue::try_from(value.into()).expect("test value without file descriptors")
    }

    fn object_path(path: &str) -> OwnedValue {
        value(ObjectPath::try_from(path).expect("valid object path"))
    }

    fn nul_terminated(text: &str) -> Vec<u8> {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        bytes
    }

    fn block(device: &str, drive: &str) -> HashMap<String, OwnedValue> {
        HashMap::from([
            ("Device".to_string(), value(nul_terminated(device))),
            ("Size".to_string(), value(8_000_000_000u64)),
            ("ReadOnly".to_string(), value(false)),
            ("Drive".to_string(), object_path(drive)),
            ("DeviceNumber".to_string(), value(8u64 << 8)),
            ("HintSystem".to_string(), value(false)),
            ("HintIgnore".to_string(), value(false)),
            ("HintPartitionable".to_string(), value(true)),
            ("IdUsage".to_string(), value("")),
            ("IdType".to_string(), value("")),
            ("MDRaid".to_string(), object_path("/")),
            ("MDRaidMember".to_string(), object_path("/")),
            ("CryptoBackingDevice".to_string(), object_path("/")),
        ])
    }

    fn drive(serial: &str, sibling_id: &str) -> HashMap<String, OwnedValue> {
        HashMap::from([
            ("Model".to_string(), value("Test Model")),
            ("Vendor".to_string(), value("Test Vendor")),
            ("Serial".to_string(), value(serial)),
            ("ConnectionBus".to_string(), value("usb")),
            ("Removable".to_string(), value(true)),
            ("MediaAvailable".to_string(), value(true)),
            ("CanPowerOff".to_string(), value(true)),
            ("SiblingId".to_string(), value(sibling_id)),
        ])
    }

    fn filesystem(mount_points: &[&str]) -> HashMap<String, OwnedValue> {
        let points: Vec<Vec<u8>> = mount_points
            .iter()
            .map(|point| nul_terminated(point))
            .collect();
        HashMap::from([("MountPoints".to_string(), value(points))])
    }

    // A single USB stick, its drive, and one unrelated drive.
    fn tree() -> ManagedObjects {
        HashMap::from([
            (
                DISK.to_string(),
                HashMap::from([(BLOCK.to_string(), block("/dev/sdx", DRIVE_PATH))]),
            ),
            (
                DRIVE_PATH.to_string(),
                HashMap::from([(DRIVE.to_string(), drive("TEST-SERIAL", INTERFACE))]),
            ),
            (
                OTHER_DRIVE.to_string(),
                HashMap::from([(
                    DRIVE.to_string(),
                    drive(
                        "OTHER",
                        "/sys/devices/pci0000:00/0000:00:14.0/usb2/2-2/2-2:1.0",
                    ),
                )]),
            ),
        ])
    }

    fn set(tree: &mut ManagedObjects, path: &str, interface: &str, property: &str, to: OwnedValue) {
        tree.get_mut(path)
            .unwrap()
            .entry(interface.to_string())
            .or_default()
            .insert(property.to_string(), to);
    }

    // Adds partitions (by number) to the disk's PartitionTable, each a Block
    // + Partition object on the disk's drive.
    fn add_partitions(tree: &mut ManagedObjects, numbers: &[u32]) {
        let paths: Vec<String> = numbers.iter().map(|n| format!("{DISK}{n}")).collect();
        for (number, path) in numbers.iter().zip(&paths) {
            let mut partition = block(&format!("/dev/sdx{number}"), DRIVE_PATH);
            partition.insert("HintPartitionable".to_string(), value(false));
            tree.insert(
                path.clone(),
                HashMap::from([
                    (BLOCK.to_string(), partition),
                    (
                        PARTITION.to_string(),
                        HashMap::from([("Number".to_string(), value(*number))]),
                    ),
                ]),
            );
        }
        let listed: Vec<ObjectPath<'_>> = paths
            .iter()
            .map(|path| ObjectPath::try_from(path.as_str()).unwrap())
            .collect();
        set(tree, DISK, PARTITION_TABLE, "Partitions", value(listed));
    }

    fn mount(tree: &mut ManagedObjects, path: &str, mount_points: &[&str]) {
        tree.get_mut(path)
            .unwrap()
            .insert(FILESYSTEM.to_string(), filesystem(mount_points));
    }

    // A sysfs in which /dev/sdx sits under INTERFACE with diskseq 12 and
    // the USB device has one interface.
    #[derive(Default)]
    struct FakeSysfs {
        links: HashMap<PathBuf, PathBuf>,
        files: HashMap<PathBuf, String>,
    }

    impl FakeSysfs {
        fn usb_stick() -> Self {
            let block = PathBuf::from(format!("{INTERFACE}/host6/target6:0:0/6:0:0:0/block/sdx"));
            FakeSysfs {
                links: HashMap::from([(PathBuf::from("/sys/block/sdx"), block.clone())]),
                files: HashMap::from([
                    (block.join("diskseq"), "12\n".to_string()),
                    (
                        PathBuf::from(USB_DEVICE).join("bNumInterfaces"),
                        " 1\n".to_string(),
                    ),
                ]),
            }
        }

        fn block_dir(&self) -> PathBuf {
            self.links[Path::new("/sys/block/sdx")].clone()
        }
    }

    impl Sysfs for FakeSysfs {
        fn realpath(&self, path: &Path) -> Option<PathBuf> {
            self.links.get(path).cloned()
        }

        fn read(&self, path: &Path) -> Option<String> {
            self.files.get(path).cloned()
        }
    }

    fn fixed_diskseq(_device: &str) -> Option<u64> {
        Some(12)
    }

    fn facts_with(tree: &ManagedObjects, sysfs: &FakeSysfs) -> (RemovalFacts, SysfsBinding) {
        facts_from_objects(tree, DISK, &[], &fixed_diskseq, sysfs)
            .expect("facts should be collected")
            .expect("the disk is a whole disk with a drive")
    }

    fn facts(tree: &ManagedObjects) -> RemovalFacts {
        facts_with(tree, &FakeSysfs::usb_stick()).0
    }

    fn roles(facts: &RemovalFacts) -> Vec<(String, BlockRole, Vec<String>)> {
        facts
            .filesystems
            .iter()
            .map(|fs| (fs.object_path.clone(), fs.role, fs.mount_points.clone()))
            .collect()
    }

    fn bind(sysfs: &FakeSysfs, sibling_id: &str, diskseq: Option<u64>) -> SysfsBinding {
        bind_usb_topology(sysfs, sibling_id, "/dev/sdx", diskseq)
    }

    // ---- ManagedObjects ----

    #[test]
    fn a_whole_disk_filesystem_is_listed() {
        let mut tree = tree();
        mount(&mut tree, DISK, &["/run/media/user/ISO"]);
        let facts = facts(&tree);
        assert_eq!(
            roles(&facts),
            [(
                DISK.to_string(),
                BlockRole::WholeDisk,
                vec!["/run/media/user/ISO".to_string()]
            )]
        );
        // The same mount the normal snapshot reports.
        assert_eq!(facts.snapshot.mount_points, ["/run/media/user/ISO"]);
    }

    #[test]
    fn partition_filesystems_are_listed_mounted_or_not_with_their_own_mounts() {
        let mut tree = tree();
        add_partitions(&mut tree, &[1, 2, 3]);
        mount(&mut tree, &format!("{DISK}1"), &["/mnt/a", "/mnt/a2"]);
        mount(&mut tree, &format!("{DISK}2"), &[]);
        // Partition 3 carries no filesystem: not listed.
        let facts = facts(&tree);
        assert_eq!(
            roles(&facts),
            [
                (
                    format!("{DISK}1"),
                    BlockRole::Partition,
                    vec!["/mnt/a".to_string(), "/mnt/a2".to_string()]
                ),
                (format!("{DISK}2"), BlockRole::Partition, Vec::new()),
            ]
        );
    }

    #[test]
    fn an_object_on_the_drive_that_is_not_a_listed_partition_is_other() {
        let mut tree = tree();
        add_partitions(&mut tree, &[1]);
        // Same Block.Drive, but not in the disk's PartitionTable.
        let stray = format!("{DISK}9");
        tree.insert(
            stray.clone(),
            HashMap::from([
                (BLOCK.to_string(), block("/dev/sdx9", DRIVE_PATH)),
                (FILESYSTEM.to_string(), filesystem(&["/mnt/x"])),
            ]),
        );
        mount(&mut tree, &format!("{DISK}1"), &[]);
        let facts = facts(&tree);
        assert_eq!(
            roles(&facts),
            [
                (format!("{DISK}1"), BlockRole::Partition, Vec::new()),
                (stray, BlockRole::Other, vec!["/mnt/x".to_string()]),
            ]
        );
    }

    #[test]
    fn filesystems_of_other_drives_are_not_mixed_in() {
        let mut tree = tree();
        tree.insert(
            "/org/freedesktop/UDisks2/block_devices/sdy".to_string(),
            HashMap::from([
                (BLOCK.to_string(), block("/dev/sdy", OTHER_DRIVE)),
                (FILESYSTEM.to_string(), filesystem(&["/mnt/other"])),
            ]),
        );
        assert!(facts(&tree).filesystems.is_empty());
    }

    #[test]
    fn the_drive_path_comes_from_the_same_tree() {
        let mut tree = tree();
        mount(&mut tree, DISK, &[]);
        let facts = facts(&tree);
        assert_eq!(facts.snapshot.drive_path, DRIVE_PATH);
        assert_eq!(facts.filesystems[0].drive_path, DRIVE_PATH);
    }

    // ---- Drive ----

    #[test]
    fn the_drive_properties_are_read() {
        let facts = facts(&tree());
        assert_eq!(facts.snapshot.connection_bus, "usb");
        assert!(facts.can_power_off);
        assert_eq!(facts.sibling_id, INTERFACE);
        assert_eq!(facts.other_siblings, 0);

        let mut tree = tree();
        set(&mut tree, DRIVE_PATH, DRIVE, "CanPowerOff", value(false));
        assert!(!self::facts(&tree).can_power_off);
    }

    #[test]
    fn another_drive_with_the_same_sibling_id_is_counted() {
        let mut tree = tree();
        set(&mut tree, OTHER_DRIVE, DRIVE, "SiblingId", value(INTERFACE));
        assert_eq!(facts(&tree).other_siblings, 1);
    }

    #[test]
    fn an_empty_sibling_id_leaves_the_topology_unknown() {
        let mut tree = tree();
        set(&mut tree, DRIVE_PATH, DRIVE, "SiblingId", value(""));
        // Another drive with an empty SiblingId is not a sibling.
        set(&mut tree, OTHER_DRIVE, DRIVE, "SiblingId", value(""));
        let (facts, binding) = facts_with(&tree, &FakeSysfs::usb_stick());
        assert_eq!(facts.other_siblings, 0);
        assert_eq!(binding, SysfsBinding::NoSiblingId);
        assert_eq!(facts.usb, UsbTopology::Unknown);
    }

    #[test]
    fn missing_or_mistyped_properties_are_errors() {
        let cases: [(&str, &str, Option<OwnedValue>); 5] = [
            (DRIVE_PATH, "CanPowerOff", None),
            (DRIVE_PATH, "CanPowerOff", Some(value("yes"))),
            (DRIVE_PATH, "SiblingId", None),
            (DRIVE_PATH, "SiblingId", Some(value(1u32))),
            (OTHER_DRIVE, "SiblingId", None),
        ];
        for (path, property, replacement) in cases {
            let mut tree = tree();
            match replacement {
                Some(to) => set(&mut tree, path, DRIVE, property, to),
                None => {
                    tree.get_mut(path)
                        .unwrap()
                        .get_mut(DRIVE)
                        .unwrap()
                        .remove(property);
                }
            }
            let result =
                facts_from_objects(&tree, DISK, &[], &fixed_diskseq, &FakeSysfs::usb_stick());
            assert!(
                matches!(
                    result,
                    Err(CollectionError::MissingProperty { .. }
                        | CollectionError::InvalidProperty { .. })
                ),
                "{path} {property}"
            );
        }

        // A Filesystem whose MountPoints cannot be read.
        let mut tree = tree();
        tree.get_mut(DISK)
            .unwrap()
            .insert(FILESYSTEM.to_string(), HashMap::new());
        assert!(
            facts_from_objects(&tree, DISK, &[], &fixed_diskseq, &FakeSysfs::usb_stick()).is_err()
        );
    }

    // ---- sysfs ----

    #[test]
    fn a_valid_binding_reads_the_interface_count() {
        let sysfs = FakeSysfs::usb_stick();
        assert_eq!(
            bind(&sysfs, INTERFACE, Some(12)),
            SysfsBinding::Bound { interfaces: 1 }
        );
        let (facts, _) = facts_with(&tree(), &sysfs);
        assert_eq!(facts.usb, UsbTopology::Bound { interfaces: 1 });
    }

    #[test]
    fn an_unresolvable_block_device_is_not_bound() {
        let mut sysfs = FakeSysfs::usb_stick();
        sysfs.links.clear();
        assert_eq!(
            bind(&sysfs, INTERFACE, Some(12)),
            SysfsBinding::BlockNotResolved
        );
    }

    #[test]
    fn a_block_device_outside_the_interface_is_not_bound() {
        let mut sysfs = FakeSysfs::usb_stick();
        sysfs.links.insert(
            PathBuf::from("/sys/block/sdx"),
            PathBuf::from("/sys/devices/pci0000:00/0000:00:17.0/ata1/host0/block/sdx"),
        );
        assert_eq!(
            bind(&sysfs, INTERFACE, Some(12)),
            SysfsBinding::NotUnderInterface
        );
        // The interface directory itself is not "under" it.
        sysfs
            .links
            .insert(PathBuf::from("/sys/block/sdx"), PathBuf::from(INTERFACE));
        assert_eq!(
            bind(&sysfs, INTERFACE, Some(12)),
            SysfsBinding::NotUnderInterface
        );
    }

    // 1-1:1.0 is not a prefix of 1-10:1.0: paths are compared by component.
    #[test]
    fn a_similar_looking_interface_is_not_a_parent() {
        let mut sysfs = FakeSysfs::usb_stick();
        let usb1 = "/sys/devices/pci0000:00/0000:00:14.0/usb1";
        sysfs.links.insert(
            PathBuf::from("/sys/block/sdx"),
            PathBuf::from(format!("{usb1}/1-10/1-10:1.0/host6/block/sdx")),
        );
        assert_eq!(
            bind(&sysfs, &format!("{usb1}/1-1/1-1:1.0"), Some(12)),
            SysfsBinding::NotUnderInterface
        );
        // A sibling id that is only a string prefix of the real one.
        assert_eq!(
            bind(&sysfs, &format!("{usb1}/1-1/1-1:1"), Some(12)),
            SysfsBinding::UnrecognizedSiblingId
        );
    }

    #[test]
    fn a_different_or_unreadable_diskseq_is_not_bound() {
        let sysfs = FakeSysfs::usb_stick();
        assert_eq!(
            bind(&sysfs, INTERFACE, Some(13)),
            SysfsBinding::DiskseqMismatch
        );
        assert_eq!(bind(&sysfs, INTERFACE, None), SysfsBinding::NoDiskseq);

        let mut unreadable = FakeSysfs::usb_stick();
        let diskseq = unreadable.block_dir().join("diskseq");
        unreadable.files.remove(&diskseq);
        assert_eq!(
            bind(&unreadable, INTERFACE, Some(12)),
            SysfsBinding::DiskseqUnreadable
        );

        let mut garbled = FakeSysfs::usb_stick();
        garbled.files.insert(diskseq, "twelve\n".to_string());
        assert_eq!(
            bind(&garbled, INTERFACE, Some(12)),
            SysfsBinding::DiskseqUnreadable
        );
    }

    #[test]
    fn an_unreadable_or_invalid_interface_count_is_not_bound() {
        let count = PathBuf::from(USB_DEVICE).join("bNumInterfaces");
        let mut sysfs = FakeSysfs::usb_stick();
        sysfs.files.remove(&count);
        assert_eq!(
            bind(&sysfs, INTERFACE, Some(12)),
            SysfsBinding::InterfacesUnreadable
        );

        for garbled in ["", "one", "-1", "1.5"] {
            let mut sysfs = FakeSysfs::usb_stick();
            sysfs.files.insert(count.clone(), garbled.to_string());
            assert_eq!(
                bind(&sysfs, INTERFACE, Some(12)),
                SysfsBinding::InterfacesInvalid,
                "{garbled:?}"
            );
        }
    }

    #[test]
    fn several_interfaces_are_reported_as_they_are() {
        let mut sysfs = FakeSysfs::usb_stick();
        sysfs.files.insert(
            PathBuf::from(USB_DEVICE).join("bNumInterfaces"),
            " 2\n".to_string(),
        );
        assert_eq!(
            bind(&sysfs, INTERFACE, Some(12)),
            SysfsBinding::Bound { interfaces: 2 }
        );
    }

    #[test]
    fn only_a_usb_interface_path_is_accepted_as_a_sibling_id() {
        let sysfs = FakeSysfs::usb_stick();
        for sibling_id in [
            "2-1:1.0",
            "/sys/devices/../usb2/2-1/2-1:1.0",
            "/sys/devices/pci0000:00/0000:00:14.0/usb2/./2-1/2-1:1.0",
            "/sys/devices/pci0000:00/0000:00:14.0/usb2//2-1/2-1:1.0",
            "/sys/devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/",
            "/sys/bus/usb/devices/2-1:1.0",
            "/sys/devices/pci0000:00/0000:00:14.0/usb2/2-1",
            "/sys/devices/pci0000:00/0000:00:14.0/usb2/2-1/2-2:1.0",
            "/sys/devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1:x.0",
        ] {
            assert_eq!(
                bind(&sysfs, sibling_id, Some(12)),
                SysfsBinding::UnrecognizedSiblingId,
                "{sibling_id}"
            );
        }
        // A deeper port chain is still a USB interface.
        assert_eq!(
            usb_device_of_interface(Path::new("/sys/devices/x/usb3/3-1/3-1.4/3-1.4:1.0")),
            Some(Path::new("/sys/devices/x/usb3/3-1/3-1.4"))
        );
    }

    #[test]
    fn an_unusual_device_node_is_not_bound() {
        let sysfs = FakeSysfs::usb_stick();
        for node in ["/dev/../sdx", "sdx", "/dev/", "/dev/mapper/x"] {
            assert_eq!(
                bind_usb_topology(&sysfs, INTERFACE, node, Some(12)),
                SysfsBinding::UnrecognizedDeviceName,
                "{node}"
            );
        }
    }

    // ---- The facts ----

    #[test]
    fn a_missing_device_is_not_found_and_failures_are_errors() {
        let sysfs = FakeSysfs::usb_stick();
        let result = collect(
            Ok(tree()),
            Ok(Vec::new()),
            "/org/freedesktop/UDisks2/block_devices/sdz",
            &fixed_diskseq,
            &sysfs,
        );
        assert!(matches!(result, Ok(None)));

        let result = collect(
            Err(CollectionError::DBus(zbus::Error::Failure(
                "no bus".to_string(),
            ))),
            Ok(Vec::new()),
            DISK,
            &fixed_diskseq,
            &sysfs,
        );
        assert!(matches!(result, Err(message) if message.contains("no bus")));

        let result = collect(
            Ok(tree()),
            Err(io::Error::other("no swaps")),
            DISK,
            &fixed_diskseq,
            &sysfs,
        );
        assert!(result.is_err());

        // The snapshot itself cannot be built (a Block property is missing).
        let mut broken = tree();
        broken
            .get_mut(DISK)
            .unwrap()
            .get_mut(BLOCK)
            .unwrap()
            .remove("Size");
        assert!(collect(Ok(broken), Ok(Vec::new()), DISK, &fixed_diskseq, &sysfs).is_err());
    }

    // A sysfs failure keeps the device's facts, with an unknown topology.
    #[test]
    fn a_sysfs_failure_keeps_the_facts_with_an_unknown_topology() {
        let (facts, binding) = facts_with(&tree(), &FakeSysfs::default());
        assert_eq!(binding, SysfsBinding::BlockNotResolved);
        assert_eq!(facts.usb, UsbTopology::Unknown);
        assert_eq!(facts.snapshot.block_path, DISK);
        assert!(facts.can_power_off);
    }
}

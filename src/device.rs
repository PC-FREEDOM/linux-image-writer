#[derive(Debug, Clone)]
pub struct DeviceSnapshot {
    pub device: String,
    pub block_path: String,
    pub drive_path: String,
    pub major: u32,
    pub minor: u32,
    pub diskseq: Option<u64>,
    pub size: u64,
    pub read_only: bool,
    pub media_available: bool,
    pub model: String,
    pub vendor: String,
    pub serial: String,
    pub connection_bus: String,
    pub removable: bool,
    pub hint_system: bool,
    pub hint_ignore: bool,
    pub hint_partitionable: bool,
    pub mount_points: Vec<String>,
    pub active_swap: bool,
    pub swap_devices: Vec<String>,
    pub complex_storage: bool,
    pub complex_storage_details: Vec<String>,
}

// Outcome of a targeted, single-device re-fetch (see
// `linux_backend::collect_device_snapshot`). Kept here as a plain data type so
// the Core layer can consume it without depending on how it was collected.
#[derive(Debug)]
pub enum SnapshotFetchOutcome {
    Found(DeviceSnapshot),
    NotFound,
    Error(String),
}

// ---- Safe Removal ----
//
// What Safe Removal reads about one device before it unmounts or powers
// anything off (`orchestration::removal` judges it). Plain data, collected
// in one pass so every part describes the same moment. Nothing collects it
// yet (Safe Removal SR-1 only judges given facts); hence the `dead_code`
// allowances.

// Everything Safe Removal reads about one device.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RemovalFacts {
    // The device as the normal collection reports it (identity, instance,
    // bus, hints, the merged mount points).
    pub snapshot: DeviceSnapshot,
    // UDisks2 Drive.CanPowerOff of `snapshot.drive_path`.
    pub can_power_off: bool,
    // UDisks2 Drive.SiblingId: groups drives of one physical device; empty
    // when UDisks2 reports none.
    pub sibling_id: String,
    // How many *other* drives report the same, non-empty `sibling_id`.
    pub other_siblings: usize,
    pub usb: UsbTopology,
    // Every Block object whose Block.Drive is `snapshot.drive_path` and that
    // carries a Filesystem interface, mounted or not.
    pub filesystems: Vec<BlockFilesystem>,
}

// The USB device the drive belongs to, as read from sysfs.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbTopology {
    // The block device was found under the drive's USB interface in sysfs,
    // and sysfs reported the same diskseq as `snapshot`; `interfaces` is the
    // USB device's bNumInterfaces.
    Bound { interfaces: u32 },
    // Not established (not found, not readable, a different instance).
    Unknown,
}

// One filesystem-bearing Block object of the drive.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct BlockFilesystem {
    pub object_path: String,
    // Its Block.Drive, as read.
    pub drive_path: String,
    pub role: BlockRole,
    // Filesystem.MountPoints, decoded; empty when not mounted.
    pub mount_points: Vec<String>,
}

// What a Block object is to the whole disk.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockRole {
    // The whole disk itself (a filesystem written directly to the disk, e.g.
    // a superfloppy or some ISO images).
    WholeDisk,
    // Listed in the whole disk's PartitionTable.
    Partition,
    // Anything else (not the disk, not one of its partitions).
    Other,
}

// Outcome of collecting `RemovalFacts` for one block path.
#[allow(dead_code)]
#[derive(Debug)]
pub enum RemovalFactsOutcome {
    // Boxed: far larger than the other variants.
    Found(Box<RemovalFacts>),
    NotFound,
    Error(String),
}

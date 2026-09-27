// Shared fixtures for orchestration tests (test builds only): a scripted
// platform that plays UDisks2 and the device with temporary files, a
// selectable USB stick snapshot, and temporary raw / gzip / xz images.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use super::platform::Platform;
use crate::device::{DeviceSnapshot, SnapshotFetchOutcome};
use crate::execution::linux_access::{FdMetadata, OpenAccess, OpenDeviceError, OpenedDeviceHandle};

// Answers `fetch_snapshot` from a script, in order, and records every
// block path asked for; running out of answers is a test failure. For a
// whole operation it also plays the device: `open_device` opens a
// temporary file standing in for it (never a block device, never
// D-Bus), and `fd_metadata` reports the metadata of the scripted
// snapshot, so the real FD binding check (`core::check_fd_binding`)
// runs on it.
pub(crate) struct ScriptedPlatform {
    pub(crate) answers: RefCell<VecDeque<SnapshotFetchOutcome>>,
    pub(crate) asked: RefCell<Vec<String>>,
    pub(crate) device: Option<Device>,
    pub(crate) opens: RefCell<Vec<OpenAccess>>,
    // For each `ReadOnlyDirect` open: how many FDs of this process
    // still pointed at the device file at that moment.
    pub(crate) fds_open_at_verify: RefCell<Vec<usize>>,
}

#[derive(Default)]
pub(crate) struct Device {
    pub(crate) path: PathBuf,
    // Verify reads this file instead (a device whose content differs).
    pub(crate) verify_from: Option<PathBuf>,
    pub(crate) fail: Vec<OpenAccess>,
    // Report a different minor number than the snapshot's.
    pub(crate) wrong_metadata: bool,
    // Open the "write" FD read-only, so every write fails.
    pub(crate) read_only_write_fd: bool,
}

impl ScriptedPlatform {
    pub(crate) fn new(answers: Vec<SnapshotFetchOutcome>) -> Self {
        ScriptedPlatform {
            answers: RefCell::new(answers.into()),
            asked: RefCell::new(Vec::new()),
            device: None,
            opens: RefCell::new(Vec::new()),
            fds_open_at_verify: RefCell::new(Vec::new()),
        }
    }

    pub(crate) fn with_device(answers: Vec<SnapshotFetchOutcome>, device: Device) -> Self {
        ScriptedPlatform {
            device: Some(device),
            ..ScriptedPlatform::new(answers)
        }
    }

    pub(crate) fn fetches(&self) -> usize {
        self.asked.borrow().len()
    }

    pub(crate) fn opens(&self) -> Vec<OpenAccess> {
        self.opens.borrow().clone()
    }
}

pub(crate) fn fds_pointing_at(path: &Path) -> usize {
    let path = std::fs::canonicalize(path).unwrap();
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
        .filter(|target| *target == path)
        .count()
}

impl Platform for ScriptedPlatform {
    fn fetch_snapshot(&self, block_path: &str) -> SnapshotFetchOutcome {
        self.asked.borrow_mut().push(block_path.to_string());
        self.answers
            .borrow_mut()
            .pop_front()
            .expect("an unexpected snapshot fetch")
    }

    fn open_device(
        &self,
        block_path: &str,
        access: OpenAccess,
    ) -> Result<OpenedDeviceHandle, OpenDeviceError> {
        let device = self.device.as_ref().expect("an unexpected OpenDevice");
        assert_eq!(block_path, usb_stick().block_path);
        self.opens.borrow_mut().push(access);
        if access == OpenAccess::ReadOnlyDirect {
            self.fds_open_at_verify
                .borrow_mut()
                .push(fds_pointing_at(&device.path));
        }
        if device.fail.contains(&access) {
            return Err(OpenDeviceError::Connection("simulated".to_string()));
        }
        let file = match access {
            OpenAccess::WriteExclusive => std::fs::OpenOptions::new()
                .read(true)
                .write(!device.read_only_write_fd)
                .open(&device.path),
            OpenAccess::ReadOnlyDirect => {
                std::fs::File::open(device.verify_from.as_ref().unwrap_or(&device.path))
            }
        }
        .unwrap();
        Ok(OpenedDeviceHandle::from_file_for_test(file))
    }

    fn fd_metadata(&self, _handle: &OpenedDeviceHandle) -> Option<FdMetadata> {
        let device = self.device.as_ref().unwrap();
        let snapshot = usb_stick();
        Some(FdMetadata {
            major: snapshot.major,
            minor: snapshot.minor + u32::from(device.wrong_metadata),
            size: Some(snapshot.size),
            diskseq: snapshot.diskseq,
            proc_fd_target: None,
        })
    }
}

pub(crate) fn usb_stick() -> DeviceSnapshot {
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

pub(crate) fn found(snapshot: DeviceSnapshot) -> SnapshotFetchOutcome {
    SnapshotFetchOutcome::Found(snapshot)
}

pub(crate) fn recreated() -> DeviceSnapshot {
    let mut snapshot = usb_stick();
    snapshot.diskseq = Some(13);
    snapshot
}

// A temporary image file, removed when dropped (if still there).
pub(crate) struct TempImage(pub(crate) std::path::PathBuf);

impl Drop for TempImage {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl TempImage {
    pub(crate) fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }
}

pub(crate) fn temp_image(tag: &str, extension: &str, contents: &[u8]) -> TempImage {
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

pub(crate) fn payload() -> Vec<u8> {
    (0..300_000u32).map(|i| (i % 251) as u8).collect()
}

pub(crate) fn gzip(data: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

pub(crate) fn xz(data: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let stream =
        liblzma::stream::Stream::new_easy_encoder(0, liblzma::stream::Check::Crc64).unwrap();
    let mut encoder = liblzma::write::XzEncoder::new_stream(Vec::new(), stream);
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

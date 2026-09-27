// The system calls orchestration makes, behind one crate-private seam so
// tests can supply their own answers (orchestration, Core layer). Production
// uses `LinuxPlatform`, which calls the existing backend / `linux_access`
// functions exactly as the code did before the seam existed. Only data
// *collection* and the OpenDevice request go through here: every decision
// made on what they return -- `core::select`, `core::revalidate`, the Safety
// Engine, the Write Gate, `core::check_fd_binding` -- still runs, so a
// substitute can change what the device looks like, never whether the
// safety checks apply. Which access is requested is chosen by the caller
// (`orchestration::operation`) and passed through unchanged.

use crate::device::SnapshotFetchOutcome;
use crate::execution::linux_access::{
    self, FdMetadata, OpenAccess, OpenDeviceError, OpenedDeviceHandle,
};
use crate::linux_backend::collect_device_snapshot;

pub(crate) trait Platform {
    // A fresh snapshot of one block device (`collect_device_snapshot`).
    fn fetch_snapshot(&self, block_path: &str) -> SnapshotFetchOutcome;

    // UDisks2 `OpenDevice` with exactly `access` (`linux_access::open_device`).
    fn open_device(
        &self,
        block_path: &str,
        access: OpenAccess,
    ) -> Result<OpenedDeviceHandle, OpenDeviceError>;

    // What the kernel reports about the opened FD
    // (`OpenedDeviceHandle::metadata`), for the FD binding check.
    fn fd_metadata(&self, handle: &OpenedDeviceHandle) -> Option<FdMetadata>;
}

pub(crate) struct LinuxPlatform;

impl Platform for LinuxPlatform {
    fn fetch_snapshot(&self, block_path: &str) -> SnapshotFetchOutcome {
        collect_device_snapshot(block_path)
    }

    fn open_device(
        &self,
        block_path: &str,
        access: OpenAccess,
    ) -> Result<OpenedDeviceHandle, OpenDeviceError> {
        linux_access::open_device(block_path, access)
    }

    fn fd_metadata(&self, handle: &OpenedDeviceHandle) -> Option<FdMetadata> {
        handle.metadata()
    }
}

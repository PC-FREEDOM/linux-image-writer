// The system calls orchestration makes, behind one crate-private seam so
// tests can supply their own answers (orchestration, Core layer). Production
// uses `LinuxPlatform`, which calls the existing backend functions exactly
// as the code did before the seam existed. Only data *collection* goes
// through here: every decision made on what it returns -- `core::select`,
// `core::revalidate`, the Safety Engine, the Write Gate -- still runs, so a
// substitute can change what the device looks like, never whether the
// safety checks apply.

use crate::device::SnapshotFetchOutcome;
use crate::linux_backend::collect_device_snapshot;

pub(crate) trait Platform {
    // A fresh snapshot of one block device (`collect_device_snapshot`).
    fn fetch_snapshot(&self, block_path: &str) -> SnapshotFetchOutcome;
}

pub(crate) struct LinuxPlatform;

impl Platform for LinuxPlatform {
    fn fetch_snapshot(&self, block_path: &str) -> SnapshotFetchOutcome {
        collect_device_snapshot(block_path)
    }
}

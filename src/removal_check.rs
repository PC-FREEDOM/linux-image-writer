// CLI diagnostic mode (`cargo run -- removal-check`): for every whole disk
// the normal device list reports, read the facts Safe Removal would judge
// (`linux_backend::removal_facts`) and show them with Safe Removal's own
// assessment of topology, protected states and filesystems
// (`orchestration::removal::assess`).
//
// Read-only: one GetManagedObjects per disk, /proc/swaps and a few sysfs
// files. Nothing is opened, written, unmounted or powered off, and no
// polkit authorization is requested. There is no `RemovalTarget` here (only
// a finished write operation produces one), so identity and instance
// against a written device are not checked -- and nothing here says a
// device can be removed.
//
// The serial number is never printed, nor the drive object path (which
// contains it).

use crate::device::{BlockRole, RemovalFacts, UsbTopology};
use crate::linux_backend::collect_device_snapshots;
use crate::linux_backend::removal_facts::{SysfsBinding, collect_removal_diagnostics};
use crate::orchestration::removal::{SafeRemovalOutcome, assess};

pub fn run() -> zbus::Result<()> {
    println!("Safe Removal facts (read-only diagnostic).");
    println!("Nothing is opened, written, unmounted or powered off.");
    println!("No write operation is involved, so no identity is checked against one.");

    for snapshot in collect_device_snapshots()? {
        println!();
        println!("Device:            {}", snapshot.device);
        println!("Block path:        {}", snapshot.block_path);

        match collect_removal_diagnostics(&snapshot.block_path) {
            Ok(Some((facts, binding))) => show(&facts, binding),
            Ok(None) => println!("Facts:             not found (the device went away)"),
            Err(error) => println!("Facts:             could not be collected: {error}"),
        }
    }

    Ok(())
}

fn show(facts: &RemovalFacts, binding: SysfsBinding) {
    let snapshot = &facts.snapshot;
    let yes_no = |value: bool| if value { "yes" } else { "no" };

    println!("Facts:             collected (one UDisks2 tree + sysfs)");
    println!(
        "Identity evidence: serial {} (not shown), size {} bytes",
        if snapshot.serial.is_empty() {
            "missing"
        } else {
            "present"
        },
        snapshot.size
    );
    println!(
        "Instance:          diskseq {}, major:minor {}:{}",
        snapshot
            .diskseq
            .map(|seq| seq.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        snapshot.major,
        snapshot.minor
    );
    println!(
        "Connection bus:    {}",
        display_or_none(&snapshot.connection_bus)
    );
    println!("CanPowerOff:       {}", yes_no(facts.can_power_off));
    println!("SiblingId:         {}", display_or_none(&facts.sibling_id));
    println!("Sharing drives:    {}", facts.other_siblings);
    match binding {
        SysfsBinding::Bound { interfaces } => {
            println!("sysfs binding:     bound (bNumInterfaces {interfaces})")
        }
        failure => println!("sysfs binding:     not bound ({failure:?})"),
    }
    println!(
        "USB topology:      {}",
        match facts.usb {
            UsbTopology::Bound { interfaces } => format!("bound, {interfaces} interface(s)"),
            UsbTopology::Unknown => "unknown".to_string(),
        }
    );

    if facts.filesystems.is_empty() {
        println!("Filesystems:       none");
    } else {
        println!("Filesystems:");
        for filesystem in &facts.filesystems {
            let role = match filesystem.role {
                BlockRole::WholeDisk => "whole disk",
                BlockRole::Partition => "partition ",
                BlockRole::Other => "OTHER     ",
            };
            let mounts = if filesystem.mount_points.is_empty() {
                "not mounted".to_string()
            } else {
                format!("mounted at {}", filesystem.mount_points.join(", "))
            };
            println!("  {role}  {}  {mounts}", filesystem.object_path);
        }
    }
    if snapshot.mount_points.is_empty() {
        println!("Device mounts:     none");
    } else {
        println!("Device mounts:     {}", snapshot.mount_points.join(", "));
    }

    println!(
        "v0.1 assessment:   {}",
        match assess(facts) {
            Ok(plan) => format!(
                "in scope (would unmount {} filesystem(s), then ask UDisks2 to power off the drive)",
                plan.unmount.len()
            ),
            Err(SafeRemovalOutcome::Unsupported(reason)) => format!("out of scope ({reason:?})"),
            Err(SafeRemovalOutcome::Unavailable(reason)) => {
                format!("information does not add up ({reason:?})")
            }
            Err(_) => "unexpected result".to_string(),
        }
    );
    println!("                   (a diagnostic only: this does not mean the drive can be removed)");
}

fn display_or_none(text: &str) -> &str {
    if text.is_empty() { "none" } else { text }
}

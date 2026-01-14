use anyhow::{bail, Result};
use console::style;
use std::path::Path;

use crate::config::Config;
use crate::utils::prompt::{confirm_or_yes, info, section, select, step, success, warn};
use crate::utils::shell::run as shell_run;

pub fn run(config: &Config, snapshot: Option<String>, yes: bool) -> Result<()> {
    println!("{}", style("Restore from Snapshot").bold().cyan());
    println!();

    let snapshot_dir = format!("{}/{}", config.mount.base, config.btrbk.snapshot_dir);

    // Get available snapshots
    let snapshots = shell_run("ls", &["-1", &snapshot_dir])?;
    if snapshots.is_empty() {
        bail!("No snapshots found in {}", snapshot_dir);
    }

    let snapshot_list: Vec<&str> = snapshots.lines().collect();

    // Select snapshot
    let selected = if let Some(ref name) = snapshot {
        if !snapshot_list.contains(&name.as_str()) {
            bail!("Snapshot '{}' not found", name);
        }
        name.clone()
    } else {
        // Interactive selection
        let options: Vec<&str> = snapshot_list.iter().rev().take(10).cloned().collect();
        let idx = select("Select snapshot to restore", &options, 0)?;
        options[idx].to_string()
    };

    println!();
    info(&format!("Selected: {}", selected));

    // Parse snapshot name to get subvolume
    // Format: subvol.YYYYMMDDTHHMMSS or subvol.YYYYMMDD (btrbk formats)
    let parts: Vec<&str> = selected.rsplitn(2, '.').collect();
    if parts.len() < 2 {
        bail!("Invalid snapshot name format: {}", selected);
    }
    // rsplitn reverses order, so parts[1] is the subvol name
    let subvol_base = parts[1];
    let subvol_name = format!("@{}", subvol_base);

    info(&format!("Target subvolume: {}", subvol_name));

    // Verify the subvolume exists in config
    let is_backup_subvol = config.subvolumes.backup.contains_key(&subvol_name);
    let is_etc_subvol = subvol_name == "@etc";
    if !is_backup_subvol && !is_etc_subvol {
        warn(&format!(
            "Subvolume {} is not in backup configuration",
            subvol_name
        ));
    }

    // Get mount point for the subvolume
    let mount_point = if is_etc_subvol {
        None // @etc is snapshot-only, not mounted
    } else {
        config
            .subvolumes
            .backup
            .get(&subvol_name)
            .map(|b| b.mount().to_string())
    };

    // Show restore plan
    section("Restore Plan");
    println!("  Source snapshot: {}/{}", snapshot_dir, selected);
    println!("  Target subvolume: {}", subvol_name);
    if let Some(ref mp) = mount_point {
        println!("  Mount point: {}", mp);
    }
    println!();

    // Warn about destructive operation
    warn("This will REPLACE the current subvolume with the snapshot!");
    warn("All changes since the snapshot will be LOST!");
    if mount_point.is_some() {
        warn("The mount point must be unmounted during restore.");
    }
    println!();

    if !confirm_or_yes("Proceed with restore?", false, yes)? {
        println!("Aborted.");
        return Ok(());
    }

    // Execute restore
    let total_steps = if mount_point.is_some() { 5 } else { 3 };
    let mut current_step = 0;

    // Step 1: Unmount if needed
    if let Some(ref mp) = mount_point {
        current_step += 1;
        step(current_step, total_steps, &format!("Unmount {}", mp));

        // Check if mounted
        let mounts = shell_run("mount", &[]).unwrap_or_default();
        if mounts.contains(&format!(" {} ", mp)) || mounts.contains(&format!(" {}\n", mp)) {
            // Try to unmount
            match shell_run("umount", &[mp]) {
                Ok(_) => success("Unmounted successfully"),
                Err(e) => {
                    warn(&format!("Failed to unmount: {}", e));
                    warn("The mount point may be in use. Please close all programs using it.");
                    if !confirm_or_yes("Retry unmount?", true, yes)? {
                        bail!("Cannot proceed without unmounting {}", mp);
                    }
                    shell_run("umount", &["-l", mp])?; // Lazy unmount as fallback
                    success("Lazy unmount completed");
                }
            }
        } else {
            info("Already unmounted");
        }
    }

    // Step 2: Rename current subvolume to .old
    current_step += 1;
    step(
        current_step,
        total_steps,
        &format!("Backup current {}", subvol_name),
    );

    let current_subvol = format!("{}/{}", config.mount.base, subvol_name);
    let backup_subvol = format!("{}/{}.restore-backup", config.mount.base, subvol_name);

    // Remove old backup if exists
    if Path::new(&backup_subvol).exists() {
        info("Removing old restore backup...");
        shell_run("btrfs", &["subvolume", "delete", &backup_subvol])?;
    }

    // Rename current to backup
    if Path::new(&current_subvol).exists() {
        shell_run("mv", &[&current_subvol, &backup_subvol])?;
        success(&format!("Backed up to {}.restore-backup", subvol_name));
    } else {
        info("Current subvolume not found, skipping backup");
    }

    // Step 3: Create snapshot from selected snapshot
    current_step += 1;
    step(
        current_step,
        total_steps,
        &format!("Restore {} from snapshot", subvol_name),
    );

    let source_snapshot = format!("{}/{}", snapshot_dir, selected);
    shell_run(
        "btrfs",
        &["subvolume", "snapshot", &source_snapshot, &current_subvol],
    )?;
    success("Snapshot restored");

    // Step 4: Remount if needed
    if let Some(ref mp) = mount_point {
        current_step += 1;
        step(current_step, total_steps, &format!("Remount {}", mp));

        // Get mount options from config
        let uuid = config.uuid.as_deref().unwrap_or("");
        let base_opts = config
            .subvolumes
            .backup
            .get(&subvol_name)
            .and_then(|b| b.options())
            .unwrap_or(&config.mount.options);
        let opts = format!("subvol={},{}", subvol_name, base_opts);

        shell_run(
            "mount",
            &["-t", "btrfs", "-o", &opts, &format!("UUID={}", uuid), mp],
        )?;
        success("Remounted successfully");
    }

    // Step 5: Cleanup (optional)
    current_step += 1;
    step(current_step, total_steps, "Cleanup");

    println!();
    info(&format!(
        "Old subvolume backed up as {}.restore-backup",
        subvol_name
    ));
    println!(
        "  To delete it (free space): btrfs subvolume delete {}",
        backup_subvol
    );
    println!("  To rollback: reverse the restore process");

    // Done
    println!();
    println!("{}", style("Restore complete!").green().bold());

    if mount_point.is_some() {
        println!();
        println!("Note: You may need to restart services or reboot for full effect.");
    }

    Ok(())
}

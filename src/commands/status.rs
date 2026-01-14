use anyhow::Result;
use console::style;

use crate::config::Config;
use crate::generators::systemd;
use crate::utils::prompt::{kv, section};
use crate::utils::shell::run as shell_run;

pub fn run(config: &Config) -> Result<()> {
    println!("{}", style("WSL Btrfs Status").bold().cyan());

    // Configuration
    section("Configuration");
    kv("Config UUID", config.uuid.as_deref().unwrap_or("not set"));
    kv("VHDX", &config.vhdx.path);
    kv("Mount base", &config.mount.base);
    kv("User", &config.get_user());

    // Btrfs mounts
    section("Btrfs Mounts");
    let mounts = shell_run("mount", &["-t", "btrfs"]).unwrap_or_default();
    if mounts.is_empty() {
        println!("  No Btrfs mounts found");
    } else {
        for line in mounts.lines() {
            println!("  {}", line);
        }
    }

    // Subvolumes (if mounted)
    section("Subvolumes");
    let subvols = shell_run("btrfs", &["subvolume", "list", &config.mount.base]);
    match subvols {
        Ok(output) if !output.is_empty() => {
            for line in output.lines() {
                // Extract just the path from btrfs output
                if let Some(path) = line.split_whitespace().last() {
                    println!("  {}", path);
                }
            }
        }
        Ok(_) => println!("  No subvolumes found"),
        Err(_) => println!("  {} not mounted", config.mount.base),
    }

    // Snapshots
    section("Snapshots");
    let snapshot_dir = format!("{}/{}", config.mount.base, config.btrbk.snapshot_dir);
    let snapshots = shell_run("ls", &["-1", &snapshot_dir]);
    match snapshots {
        Ok(output) if !output.is_empty() => {
            let lines: Vec<&str> = output.lines().collect();
            let count = lines.len();
            println!("  Total: {} snapshots", count);
            println!();
            // Show last 5
            for snap in lines.iter().rev().take(5).rev() {
                println!("  {}", snap);
            }
            if count > 5 {
                println!("  ... and {} more", count - 5);
            }
        }
        Ok(_) => println!("  No snapshots found"),
        Err(_) => println!("  Snapshot directory not accessible"),
    }

    // Systemd services
    section("Systemd Services");
    check_service("btrbk.timer");

    // Next timer
    let next = shell_run("systemctl", &["list-timers", "--no-pager", "btrbk.timer"]);
    if let Ok(output) = next {
        if output.contains("btrbk") {
            println!();
            for line in output.lines().skip(1).take(1) {
                println!("  Next run: {}", line.trim());
            }
        }
    }

    // Mount units status
    section("Mount Units");

    // Base mount
    let base_unit = systemd::mount_unit_filename(&config.mount.base);
    check_service(&base_unit);

    // Backup mounts
    for backup in config.subvolumes.backup.values() {
        let unit = systemd::mount_unit_filename(backup.mount());
        check_service(&unit);
    }

    // Transfer mounts
    for transfer in config.subvolumes.transfer.values() {
        let unit = systemd::mount_unit_filename(&transfer.mount);
        check_service(&unit);
    }

    // Failed mounts hint
    let failed = shell_run("systemctl", &["--failed", "--type=mount", "--no-legend"]);
    if let Ok(output) = failed {
        if !output.trim().is_empty() {
            println!();
            println!(
                "  {} Failed mounts detected! Check with:",
                style("⚠").yellow()
            );
            println!("    systemctl --failed --type=mount");
            println!("    journalctl -u <unit-name>.mount");
        }
    }

    Ok(())
}

fn check_service(name: &str) {
    let status = shell_run("systemctl", &["is-enabled", name]).unwrap_or_default();
    let active = shell_run("systemctl", &["is-active", name]).unwrap_or_default();

    let enabled_icon = if status.trim() == "enabled" {
        style("✓").green()
    } else {
        style("✗").red()
    };

    let active_icon = if active.trim() == "active" {
        style("●").green()
    } else {
        style("○").dim()
    };

    println!(
        "  {} {} {} ({})",
        enabled_icon,
        active_icon,
        name,
        status.trim()
    );
}

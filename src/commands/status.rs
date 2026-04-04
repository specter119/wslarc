use anyhow::Result;
use console::style;

use crate::config::Config;
use crate::generators::systemd;
use crate::utils::cli::{find_mount, list_btrfs_mounts, list_directory_names, systemctl_property};
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
    let mounts = list_btrfs_mounts().unwrap_or_default();
    if mounts.is_empty() {
        println!("  No Btrfs mounts found");
    } else {
        for mount in mounts {
            println!("  {}", format_mount(&mount));
        }
    }

    // Subvolumes (if mounted)
    section("Subvolumes");
    if !is_mounted(&config.mount.base) {
        println!("  {} not mounted", config.mount.base);
    } else {
        let subvols = shell_run("btrfs", &["subvolume", "list", &config.mount.base]);
        match subvols {
            Ok(output) if !output.is_empty() => {
                for line in output.lines() {
                    if let Some(path) = line.split_whitespace().last() {
                        println!("  {}", path);
                    }
                }
            }
            Ok(_) => println!("  No subvolumes found"),
            Err(err) => {
                for line in subvolume_unavailable_lines(config, &err) {
                    println!("{}", line);
                }
            }
        }
    }

    // Snapshots
    section("Snapshots");
    let snapshot_dir = format!("{}/{}", config.mount.base, config.btrbk.snapshot_dir);
    match list_directory_names(&snapshot_dir) {
        Ok(entries) if !entries.is_empty() => {
            let count = entries.len();
            println!("  Total: {} snapshots", count);
            println!();
            for snap in entries.iter().rev().take(5).rev() {
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
    if let Some(next) = read_unit_property("btrbk.timer", "NextElapseUSecRealtime") {
        println!();
        println!("  Next run: {}", next);
        if let Some(last) = read_unit_property("btrbk.timer", "LastTriggerUSec") {
            println!("  Last run: {}", last);
        }
    }

    // Mount units status
    section("Mount Units");

    let mount_units = mount_unit_names(config);
    for unit in &mount_units {
        check_service(unit);
    }

    // Failed mounts hint
    let failed_units: Vec<String> = mount_units
        .into_iter()
        .filter(|unit| is_failed_mount_status(&read_unit_status(unit)))
        .collect();
    if !failed_units.is_empty() {
        println!();
        println!("  {} Failed wslarc mounts detected:", style("⚠").yellow());
        for unit in &failed_units {
            println!("    {}", unit);
        }
        println!("    systemctl status <unit-name>.mount");
        println!("    journalctl -u <unit-name>.mount");
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UnitStatus {
    unit_file_state: String,
    active_state: String,
    result: String,
}

fn check_service(name: &str) {
    let status = read_unit_status(name);

    let enabled_icon = if status.unit_file_state.trim() == "enabled" {
        style("✓").green()
    } else {
        style("✗").red()
    };

    let active_icon = if status.active_state.trim() == "active" {
        style("●").green()
    } else {
        style("○").dim()
    };

    println!(
        "  {} {} {} ({})",
        enabled_icon,
        active_icon,
        name,
        status.unit_file_state.trim()
    );
}

fn is_mounted(path: &str) -> bool {
    find_mount(path)
        .map(|mount| mount.is_some())
        .unwrap_or(false)
}

fn configured_subvolume_lines(config: &Config) -> Vec<String> {
    let mut lines = Vec::new();

    for (name, backup) in &config.subvolumes.backup {
        lines.push(format!("{} -> {} [backup]", name, backup.mount()));
    }

    for (name, transfer) in &config.subvolumes.transfer {
        let mut tags = vec!["transfer"];
        if transfer.nodatacow {
            tags.push("nodatacow");
        }
        lines.push(format!(
            "{} -> {} [{}]",
            name,
            transfer.mount,
            tags.join(", ")
        ));
    }

    lines.push("@etc [snapshot-only]".to_string());
    lines.sort();
    lines
}

fn mount_unit_names(config: &Config) -> Vec<String> {
    let mut units = vec![systemd::mount_unit_filename(&config.mount.base)];

    for backup in config.subvolumes.backup.values() {
        units.push(systemd::mount_unit_filename(backup.mount()));
    }

    for transfer in config.subvolumes.transfer.values() {
        units.push(systemd::mount_unit_filename(&transfer.mount));
    }

    units
}

fn format_mount(mount: &crate::utils::cli::MountInfo) -> String {
    format!(
        "{} on {} type {} ({})",
        mount.source, mount.target, mount.fstype, mount.options
    )
}

fn subvolume_unavailable_lines(config: &Config, err: &anyhow::Error) -> Vec<String> {
    let mut lines = vec![
        format!("  {} mounted", config.mount.base),
        format!(
            "  Live subvolume list unavailable: {}",
            summarize_error(err)
        ),
        "  Configured subvolumes:".to_string(),
    ];

    lines.extend(
        configured_subvolume_lines(config)
            .into_iter()
            .map(|line| format!("    {}", line)),
    );

    lines
}

fn read_unit_status(name: &str) -> UnitStatus {
    UnitStatus {
        unit_file_state: read_unit_property(name, "UnitFileState")
            .unwrap_or_else(|| "unknown".to_string()),
        active_state: read_unit_property(name, "ActiveState")
            .unwrap_or_else(|| "unknown".to_string()),
        result: read_unit_property(name, "Result").unwrap_or_default(),
    }
}

fn is_failed_mount_status(status: &UnitStatus) -> bool {
    status.active_state == "failed"
        || (!status.result.is_empty() && status.result != "success" && status.result != "done")
}

fn read_unit_property(name: &str, property: &str) -> Option<String> {
    systemctl_property(name, property)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && value != "n/a")
}

fn summarize_error(err: &anyhow::Error) -> String {
    for cause in err.chain() {
        if let Some(line) = cause
            .to_string()
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty() && !line.starts_with("Command failed:"))
        {
            return line.trim().to_string();
        }
    }

    err.to_string()
        .lines()
        .next()
        .unwrap_or("unknown error")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_error_prefers_specific_failure_line() {
        let err = anyhow::anyhow!(
            "Command failed: btrfs subvolume list /mnt/btrfs\nERROR: can't perform the search: Operation not permitted"
        );

        assert_eq!(
            summarize_error(&err),
            "ERROR: can't perform the search: Operation not permitted"
        );
    }

    #[test]
    fn configured_subvolume_lines_include_snapshot_only_and_tags() {
        let config = Config::default();
        let lines = configured_subvolume_lines(&config);

        assert!(lines.iter().any(|line| line == "@etc [snapshot-only]"));
        assert!(lines.iter().any(|line| line == "@usr -> /usr [backup]"));
        assert!(lines
            .iter()
            .any(|line| { line == "@containers -> /var/lib/containers [transfer, nodatacow]" }));
    }

    #[test]
    fn format_mount_matches_mount_style() {
        let mount = crate::utils::cli::MountInfo {
            target: "/mnt/btrfs".to_string(),
            source: "/dev/sdd".to_string(),
            fstype: "btrfs".to_string(),
            options: "rw".to_string(),
            uuid: None,
        };

        assert_eq!(
            format_mount(&mount),
            "/dev/sdd on /mnt/btrfs type btrfs (rw)"
        );
    }

    #[test]
    fn subvolume_unavailable_lines_include_mounted_reason_and_configured() {
        let config = Config::default();
        let err = anyhow::anyhow!(
            "Command failed: btrfs subvolume list /mnt/btrfs\nERROR: can't perform the search: Operation not permitted"
        );

        let lines = subvolume_unavailable_lines(&config, &err);

        assert_eq!(lines[0], "  /mnt/btrfs mounted");
        assert_eq!(
            lines[1],
            "  Live subvolume list unavailable: ERROR: can't perform the search: Operation not permitted"
        );
        assert!(lines.iter().any(|line| line == "    @etc [snapshot-only]"));
    }

    #[test]
    fn failed_mount_status_detects_failed_active_or_result() {
        let active_failed = UnitStatus {
            unit_file_state: "enabled".to_string(),
            active_state: "failed".to_string(),
            result: "exit-code".to_string(),
        };
        let result_failed = UnitStatus {
            unit_file_state: "enabled".to_string(),
            active_state: "inactive".to_string(),
            result: "resources".to_string(),
        };
        let success = UnitStatus {
            unit_file_state: "enabled".to_string(),
            active_state: "active".to_string(),
            result: "success".to_string(),
        };

        assert!(is_failed_mount_status(&active_failed));
        assert!(is_failed_mount_status(&result_failed));
        assert!(!is_failed_mount_status(&success));
    }
}

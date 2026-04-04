use anyhow::Result;

use crate::config::Config;
use crate::generators::systemd::path_to_unit_name;
use crate::utils::cli::{find_mount_uuid, pacman_query_depends};

pub const SYSTEMD_PACKAGES: [&str; 3] = ["systemd", "systemd-libs", "systemd-sysvcompat"];

/// Get ext4 root UUID dynamically
pub fn get_ext4_root_uuid() -> Option<String> {
    find_mount_uuid("/")
}

/// Generate systemd mount unit for ext4 root
pub fn generate_ext4_mount(config: &Config, uuid: &str) -> String {
    let mount_point = &config.ext4_sync.mount_point;

    format!(
        r#"[Unit]
Description=Mount ext4 root for sync

[Mount]
What=UUID={uuid}
Where={mount_point}
Type=ext4
Options=defaults

[Install]
WantedBy=multi-user.target
"#
    )
}

pub fn generate_pacman_hook(targets: &[String]) -> String {
    let mut lines = Vec::new();
    lines.push("[Trigger]".to_string());
    lines.push("Operation = Upgrade".to_string());
    lines.push("Type = Package".to_string());
    for target in targets {
        lines.push(format!("Target = {}", target));
    }
    lines.push(String::new());
    lines.push("[Action]".to_string());
    lines.push("Description = Syncing systemd to ext4...".to_string());
    lines.push("When = PostTransaction".to_string());
    lines.push("NeedsTargets".to_string());
    lines.push("Exec = /usr/local/bin/wslarc hook-sync-systemd".to_string());
    lines.push(String::new());
    lines.join("\n")
}

pub fn collect_hook_targets() -> Result<Vec<String>> {
    let mut targets = std::collections::HashSet::new();

    for pkg in SYSTEMD_PACKAGES {
        targets.insert(pkg.to_string());
        targets.extend(pacman_query_depends(pkg)?);
    }

    let mut list: Vec<String> = targets.into_iter().collect();
    list.sort();
    Ok(list)
}

pub fn ext4_mount_unit_filename(config: &Config) -> String {
    let mount_point = &config.ext4_sync.mount_point;
    format!("{}.mount", path_to_unit_name(mount_point))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_pacman_hook_includes_needs_targets() {
        let hook = generate_pacman_hook(&["systemd".to_string(), "glibc".to_string()]);

        assert!(hook.contains("Target = systemd"));
        assert!(hook.contains("Target = glibc"));
        assert!(hook.contains("NeedsTargets"));
    }
}

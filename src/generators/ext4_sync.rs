use std::process::Command;

use crate::config::Config;

/// Get ext4 root UUID dynamically
pub fn get_ext4_root_uuid() -> Option<String> {
    Command::new("findmnt")
        .args(["/", "-o", "UUID", "-n"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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

pub fn generate_pacman_hook() -> String {
    r#"[Trigger]
Operation = Upgrade
Type = Package
Target = systemd
Target = systemd-libs
Target = systemd-sysvcompat

[Action]
Description = Syncing systemd to ext4...
When = PostTransaction
Exec = /usr/local/bin/wslarc hook-sync-systemd
"#
    .to_string()
}

pub fn ext4_mount_unit_filename(config: &Config) -> String {
    let mount_point = &config.ext4_sync.mount_point;
    let escaped = mount_point.trim_start_matches('/').replace('/', "-");
    format!("{}.mount", escaped)
}

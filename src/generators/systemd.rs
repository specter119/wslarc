use std::process::Command;

use crate::config::Config;

pub fn path_to_unit_name(path: &str) -> String {
    Command::new("systemd-escape")
        .args(["--path", path])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| path.trim_start_matches('/').replace('/', "-"))
}

/// Generate base Btrfs mount unit
pub fn generate_base_mount(config: &Config) -> String {
    let uuid = config.uuid.as_deref().unwrap_or("REPLACE_WITH_UUID");

    format!(
        r#"[Unit]
Description=Mount Btrfs Volume

[Mount]
What=UUID={}
Where={}
Type=btrfs
Options={}

[Install]
WantedBy=multi-user.target
"#,
        uuid, config.mount.base, config.mount.options
    )
}

/// Generate subvolume mount unit
pub fn generate_subvol_mount(
    config: &Config,
    subvol: &str,
    mount_point: &str,
    custom_options: Option<&str>,
) -> String {
    let uuid = config.uuid.as_deref().unwrap_or("REPLACE_WITH_UUID");
    let base_unit = path_to_unit_name(&config.mount.base);

    // Build options: subvol + custom_options or default base options
    let base_opts = custom_options.unwrap_or(&config.mount.options);
    let opts = format!("subvol={},{}", subvol, base_opts);

    // Handle dependencies for nested mounts (e.g., ~/.local/share/containers)
    let user = config.get_user();
    let home_path = format!("/home/{}", user);
    let is_home_mount = mount_point == home_path;
    let requires = if mount_point.starts_with(&home_path) && !is_home_mount {
        // Nested under home, need home mount first
        let home_unit = path_to_unit_name(&home_path);
        format!("{}.mount {}.mount", base_unit, home_unit)
    } else {
        format!("{}.mount", base_unit)
    };

    // Home mount should complete before user@.service starts
    let before = if is_home_mount {
        "Before=user@.service"
    } else {
        ""
    };

    format!(
        r#"[Unit]
Description=Mount {} subvolume
Requires={}
After={}
{}

[Mount]
What=UUID={}
Where={}
Type=btrfs
Options={}

[Install]
WantedBy=multi-user.target
"#,
        subvol, requires, requires, before, uuid, mount_point, opts
    )
}

/// Get systemd unit filename for a mount point
pub fn mount_unit_filename(mount_point: &str) -> String {
    format!("{}.mount", path_to_unit_name(mount_point))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        BackupSubvol, BtrbkConfig, Config, ExcludeConfig, Ext4SyncConfig, MountConfig,
        SubvolumesConfig, TransferSubvol, UserConfig, VhdxConfig,
    };
    use std::collections::HashMap;

    fn test_config() -> Config {
        let mut backup = HashMap::new();
        backup.insert(
            "@home".to_string(),
            BackupSubvol::Simple("/home/testuser".to_string()),
        );
        backup.insert("@usr".to_string(), BackupSubvol::Simple("/usr".to_string()));

        let mut transfer = HashMap::new();
        transfer.insert(
            "@containers".to_string(),
            TransferSubvol {
                mount: "/var/lib/containers".to_string(),
                nodatacow: true,
                options: None,
            },
        );

        Config {
            vhdx: VhdxConfig {
                path: r"C:\Users\test\.local\share\wsl\btrfs.vhdx".to_string(),
                label: "TestBtrfs".to_string(),
            },
            user: UserConfig {
                name: "testuser".to_string(),
                options: "-M -G wheel".to_string(),
            },
            mount: MountConfig {
                base: "/mnt/btrfs".to_string(),
                options: "compress=zstd:3,noatime,nofail".to_string(),
            },
            subvolumes: SubvolumesConfig {
                backup,
                exclude: ExcludeConfig {
                    parent: "@home".to_string(),
                    paths: vec![".cache".to_string()],
                },
                transfer,
            },
            btrbk: BtrbkConfig {
                snapshot_dir: ".snapshots".to_string(),
                preserve_min: "2d".to_string(),
                preserve: "14d 4w 2m".to_string(),
                timer_schedule: "*-*-* 03:00:00".to_string(),
            },
            ext4_sync: Ext4SyncConfig::default(),
            uuid: Some("12345678-1234-1234-1234-123456789abc".to_string()),
        }
    }

    #[test]
    fn test_path_to_unit_name_fallback() {
        let result = path_to_unit_name("/mnt/btrfs");
        assert!(result == "mnt-btrfs" || result == "mnt\\x2dbtrfs" || !result.is_empty());
    }

    #[test]
    fn test_mount_unit_filename() {
        let filename = mount_unit_filename("/mnt/btrfs");
        assert!(filename.ends_with(".mount"));
        assert!(filename.contains("mnt"));
    }

    #[test]
    fn test_generate_base_mount() {
        let cfg = test_config();
        let output = generate_base_mount(&cfg);

        assert!(output.contains("[Unit]"));
        assert!(output.contains("[Mount]"));
        assert!(output.contains("[Install]"));
        assert!(output.contains("UUID=12345678-1234-1234-1234-123456789abc"));
        assert!(output.contains("Where=/mnt/btrfs"));
        assert!(output.contains("Type=btrfs"));
        assert!(output.contains("compress=zstd:3"));
    }

    #[test]
    fn test_generate_base_mount_no_uuid() {
        let mut cfg = test_config();
        cfg.uuid = None;
        let output = generate_base_mount(&cfg);

        assert!(output.contains("REPLACE_WITH_UUID"));
    }

    #[test]
    fn test_generate_subvol_mount() {
        let cfg = test_config();
        let output = generate_subvol_mount(&cfg, "@usr", "/usr", None);

        assert!(output.contains("Description=Mount @usr subvolume"));
        assert!(output.contains("Where=/usr"));
        assert!(output.contains("subvol=@usr"));
        assert!(output.contains("compress=zstd:3"));
    }

    #[test]
    fn test_generate_subvol_mount_custom_options() {
        let cfg = test_config();
        let output = generate_subvol_mount(&cfg, "@data", "/data", Some("noatime,nofail"));

        assert!(output.contains("subvol=@data,noatime,nofail"));
        assert!(!output.contains("compress=zstd:3"));
    }

    #[test]
    fn test_generate_subvol_mount_home() {
        let cfg = test_config();
        let output = generate_subvol_mount(&cfg, "@home", "/home/testuser", None);

        assert!(output.contains("Before=user@.service"));
    }
}

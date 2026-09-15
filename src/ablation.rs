use std::collections::HashMap;

use tempfile::NamedTempFile;

use crate::config::{BackupSubvol, Config, Distribution};
use crate::generators::{btrbk, ext4_sync, systemd};

fn sorted_keys<T>(values: &HashMap<String, T>) -> Vec<String> {
    let mut keys: Vec<_> = values.keys().cloned().collect();
    keys.sort();
    keys
}

#[test]
fn distribution_templates_share_common_rules() {
    let arch = Config::for_distribution(Distribution::Arch);
    let debian = Config::for_distribution(Distribution::Debian);

    assert_eq!(
        sorted_keys(&arch.subvolumes.transfer),
        sorted_keys(&debian.subvolumes.transfer)
    );
    assert_eq!(
        sorted_keys(&arch.subvolumes.snapshot_only),
        sorted_keys(&debian.subvolumes.snapshot_only)
    );
    for name in ["@home", "@usr", "@opt"] {
        assert!(arch.subvolumes.backup.contains_key(name));
        assert!(debian.subvolumes.backup.contains_key(name));
    }

    assert!(arch.subvolumes.backup.contains_key("@var_lib_pacman"));
    assert!(!arch.subvolumes.backup.contains_key("@var_lib_dpkg"));
    assert!(debian.subvolumes.backup.contains_key("@var_lib_dpkg"));
    assert!(debian.subvolumes.backup.contains_key("@var_lib_apt"));
    assert!(!debian.subvolumes.backup.contains_key("@var_lib_pacman"));
}

#[test]
fn saved_template_is_flat_and_survives_reload() {
    let mut config = Config::for_distribution(Distribution::Arch);
    config.subvolumes.backup.remove("@var_lib_pacman");
    config.subvolumes.backup.insert(
        "@data".to_string(),
        BackupSubvol::Simple("/data".to_string()),
    );

    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_str().unwrap();
    config.save(path).unwrap();
    let saved = std::fs::read_to_string(path).unwrap();
    let loaded = Config::load_or_default(path, Distribution::Debian).unwrap();

    assert!(!saved.contains("distribution"));
    assert!(saved.contains("[subvolumes.backup]"));
    assert!(loaded.subvolumes.backup.contains_key("@data"));
    assert!(!loaded.subvolumes.backup.contains_key("@var_lib_pacman"));
}

#[test]
fn flat_template_drives_backup_mount_and_snapshot_artifacts() {
    let mut config = Config::for_distribution(Distribution::Arch);
    config.subvolumes.backup.insert(
        "@data".to_string(),
        BackupSubvol::Full {
            mount: "/data".to_string(),
            options: Some("noatime".to_string()),
        },
    );
    config.subvolumes.snapshot_only.insert(
        "@state".to_string(),
        crate::config::SnapshotSubvol {
            snapshot_name: "state".to_string(),
            source: "/var/lib/state".to_string(),
        },
    );

    let btrbk = btrbk::generate_config(&config);
    let mount = systemd::generate_subvol_mount(&config, "@data", "/data", Some("noatime"));

    assert!(btrbk.contains("subvolume @data"));
    assert!(btrbk.contains("subvolume @state"));
    assert!(btrbk.contains("snapshot_name state"));
    assert!(mount.contains("subvol=@data,noatime"));
}

#[test]
fn removing_template_entries_removes_downstream_artifacts() {
    let mut config = Config::for_distribution(Distribution::Arch);
    config.subvolumes.backup.remove("@usr");
    config.subvolumes.transfer.remove("@var_log");
    config.subvolumes.snapshot_only.remove("@etc");

    let btrbk = btrbk::generate_config(&config);

    assert!(!btrbk.contains("subvolume @usr"));
    assert!(!btrbk.contains("subvolume @etc"));
    assert!(!config.subvolumes.transfer.contains_key("@var_log"));
}

#[test]
fn package_hooks_are_runtime_adapters_only() {
    let arch = Config::for_distribution(Distribution::Arch);
    let debian = Config::for_distribution(Distribution::Debian);
    let arch_hook = ext4_sync::generate_package_hook(Distribution::Arch, &[]);
    let debian_hook = ext4_sync::generate_package_hook(Distribution::Debian, &[]);

    assert!(btrbk::generate_config(&arch).contains("subvolume @var_lib_pacman"));
    assert!(btrbk::generate_config(&debian).contains("subvolume @var_lib_dpkg"));
    assert_eq!(arch_hook.0, ext4_sync::PACMAN_HOOK_PATH);
    assert_eq!(debian_hook.0, ext4_sync::APT_HOOK_PATH);
    assert!(arch_hook.1.contains("NeedsTargets"));
    assert!(debian_hook.1.contains("DPkg::Post-Invoke"));
}

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub vhdx: VhdxConfig,
    pub user: UserConfig,
    pub mount: MountConfig,
    pub subvolumes: SubvolumesConfig,
    pub btrbk: BtrbkConfig,
    /// Ext4 root sync config (for systemd version sync)
    #[serde(default)]
    pub ext4_sync: Ext4SyncConfig,

    /// UUID of the Btrfs filesystem (set after formatting)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ext4SyncConfig {
    #[serde(default = "default_ext4_mount")]
    pub mount_point: String,
}

fn default_ext4_mount() -> String {
    "/mnt/ext4-root".to_string()
}

impl Default for Ext4SyncConfig {
    fn default() -> Self {
        Self {
            mount_point: default_ext4_mount(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VhdxConfig {
    /// Windows path to the VHDX file
    pub path: String,
    /// Btrfs label
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserConfig {
    /// Target username (required, will be created if not exists)
    pub name: String,
    /// useradd options (e.g., "-m -G wheel -s /bin/zsh")
    #[serde(default = "default_useradd_options")]
    pub options: String,
}

fn default_useradd_options() -> String {
    "-M -G wheel".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountConfig {
    /// Base mount point for Btrfs volume
    pub base: String,
    /// Mount options for base volume (default: compress=zstd:3,noatime,nofail)
    #[serde(default = "default_base_options")]
    pub options: String,
}

fn default_base_options() -> String {
    "compress=zstd:3,noatime,nofail".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubvolumesConfig {
    /// A-class: backup targets (subvol_name -> config)
    pub backup: HashMap<String, BackupSubvol>,
    /// B-class: excluded paths (nested subvolumes)
    pub exclude: ExcludeConfig,
    /// C-class: transfer subvolumes (high I/O, nodatacow)
    pub transfer: HashMap<String, TransferSubvol>,
}

/// A-class backup subvolume config
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BackupSubvol {
    /// Simple form: just the mount point string
    Simple(String),
    /// Full form: mount point with options
    Full {
        mount: String,
        #[serde(default = "default_subvol_options")]
        options: Option<String>,
    },
}

impl BackupSubvol {
    pub fn mount(&self) -> &str {
        match self {
            BackupSubvol::Simple(m) => m,
            BackupSubvol::Full { mount, .. } => mount,
        }
    }

    pub fn options(&self) -> Option<&str> {
        match self {
            BackupSubvol::Simple(_) => None,
            BackupSubvol::Full { options, .. } => options.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExcludeConfig {
    /// Parent subvolume for nested exclusions
    pub parent: String,
    /// Paths to exclude (relative to parent)
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferSubvol {
    /// Mount point
    pub mount: String,
    /// Whether to disable COW (chattr +C)
    #[serde(default)]
    pub nodatacow: bool,
    /// Custom mount options (default: compress=zstd:3,noatime,nofail)
    #[serde(default = "default_subvol_options")]
    pub options: Option<String>,
}

fn default_subvol_options() -> Option<String> {
    None
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BtrbkConfig {
    /// Snapshot directory name
    pub snapshot_dir: String,
    /// Minimum preserve time
    pub preserve_min: String,
    /// Preserve policy (e.g., "14d 4w 2m")
    pub preserve: String,
    /// Systemd timer schedule
    pub timer_schedule: String,
}

impl Config {
    /// Load config from file, or return default if file doesn't exist
    pub fn load_or_default(path: &str) -> Result<Self> {
        if Path::new(path).exists() {
            Self::load(path)
        } else {
            Ok(Self::default())
        }
    }

    /// Load config from file
    pub fn load(path: &str) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path))?;
        let mut config: Config = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path))?;
        config.expand_variables();
        Ok(config)
    }

    /// Save config to file
    pub fn save(&self, path: &str) -> Result<()> {
        let dir = Path::new(path).parent().unwrap_or(Path::new("/"));
        fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create config directory: {}", dir.display()))?;

        let content = toml::to_string_pretty(self).context("Failed to serialize config")?;
        fs::write(path, content)
            .with_context(|| format!("Failed to write config file: {}", path))?;
        Ok(())
    }

    /// Expand $USER and other variables in paths
    fn expand_variables(&mut self) {
        let user = self.get_user();

        // Expand in backup subvolumes
        for backup in self.subvolumes.backup.values_mut() {
            match backup {
                BackupSubvol::Simple(m) => *m = m.replace("$USER", &user),
                BackupSubvol::Full { mount, .. } => *mount = mount.replace("$USER", &user),
            }
        }

        // Expand in transfer subvolumes
        for subvol in self.subvolumes.transfer.values_mut() {
            subvol.mount = subvol.mount.replace("$USER", &user);
        }
    }

    /// Get the target user
    pub fn get_user(&self) -> String {
        self.user.name.clone()
    }

    /// Set user and expand variables in paths
    pub fn set_user(&mut self, user: &str) {
        self.user.name = user.to_string();
        self.expand_variables();
    }
}

impl Default for Config {
    fn default() -> Self {
        let mut backup = HashMap::new();
        // Note: @etc is snapshot-only (not mounted to /etc) to avoid systemd unit duplication
        backup.insert("@usr".to_string(), BackupSubvol::Simple("/usr".to_string()));
        backup.insert("@opt".to_string(), BackupSubvol::Simple("/opt".to_string()));
        backup.insert(
            "@home".to_string(),
            BackupSubvol::Simple("/home/$USER".to_string()),
        );
        // @var_lib_pacman must be snapshotted together with @usr for consistency
        backup.insert(
            "@var_lib_pacman".to_string(),
            BackupSubvol::Simple("/var/lib/pacman".to_string()),
        );

        let mut transfer = HashMap::new();
        transfer.insert(
            "@containers".to_string(),
            TransferSubvol {
                mount: "/var/lib/containers".to_string(),
                nodatacow: true,
                options: None,
            },
        );
        transfer.insert(
            "@var_cache".to_string(),
            TransferSubvol {
                mount: "/var/cache".to_string(),
                nodatacow: true,
                options: None,
            },
        );
        transfer.insert(
            "@var_log".to_string(),
            TransferSubvol {
                mount: "/var/log".to_string(),
                nodatacow: false,
                options: None,
            },
        );
        transfer.insert(
            "@var_tmp".to_string(),
            TransferSubvol {
                mount: "/var/tmp".to_string(),
                nodatacow: true,
                options: None,
            },
        );

        Self {
            vhdx: VhdxConfig {
                // Must be provided by user
                path: String::new(),
                label: "ArchBtrfs".to_string(),
            },
            user: UserConfig {
                name: String::new(),
                options: default_useradd_options(),
            },
            mount: MountConfig {
                base: "/mnt/btrfs".to_string(),
                options: default_base_options(),
            },
            subvolumes: SubvolumesConfig {
                backup,
                exclude: ExcludeConfig {
                    parent: "@home".to_string(),
                    paths: vec![
                        ".cache".to_string(),
                        ".local".to_string(),
                        ".npm".to_string(),
                        ".bun".to_string(),
                        ".vscode-server-insiders".to_string(),
                    ],
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
            uuid: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_default_config() {
        let cfg = Config::default();

        assert!(cfg.vhdx.path.is_empty());
        assert_eq!(cfg.vhdx.label, "ArchBtrfs");
        assert_eq!(cfg.mount.base, "/mnt/btrfs");
        assert!(cfg.mount.options.contains("compress=zstd:3"));
        assert!(cfg.uuid.is_none());
    }

    #[test]
    fn test_backup_subvol_simple() {
        let subvol = BackupSubvol::Simple("/usr".to_string());
        assert_eq!(subvol.mount(), "/usr");
        assert!(subvol.options().is_none());
    }

    #[test]
    fn test_backup_subvol_full() {
        let subvol = BackupSubvol::Full {
            mount: "/data".to_string(),
            options: Some("noatime".to_string()),
        };
        assert_eq!(subvol.mount(), "/data");
        assert_eq!(subvol.options(), Some("noatime"));
    }

    #[test]
    fn test_set_user_expands_variables() {
        let mut cfg = Config::default();
        cfg.set_user("alice");

        assert_eq!(cfg.get_user(), "alice");

        if let Some(backup) = cfg.subvolumes.backup.get("@home") {
            assert!(backup.mount().contains("alice"));
            assert!(!backup.mount().contains("$USER"));
        }
    }

    #[test]
    fn test_load_config_from_toml() {
        let toml_content = r#"
[vhdx]
path = "C:\\Users\\test\\btrfs.vhdx"
label = "TestLabel"

[user]
name = "testuser"

[mount]
base = "/mnt/test"

[subvolumes.backup]
"@home" = "/home/testuser"

[subvolumes.exclude]
parent = "@home"
paths = [".cache"]

[subvolumes.transfer]

[btrbk]
snapshot_dir = ".snapshots"
preserve_min = "1d"
preserve = "7d"
timer_schedule = "*-*-* 02:00:00"
"#;

        let mut file = NamedTempFile::new().unwrap();
        file.write_all(toml_content.as_bytes()).unwrap();

        let cfg = Config::load(file.path().to_str().unwrap()).unwrap();

        assert_eq!(cfg.vhdx.label, "TestLabel");
        assert_eq!(cfg.mount.base, "/mnt/test");
        assert_eq!(cfg.btrbk.preserve_min, "1d");
    }

    #[test]
    fn test_load_or_default_missing_file() {
        let cfg = Config::load_or_default("/nonexistent/path/config.toml").unwrap();
        assert!(cfg.vhdx.path.is_empty());
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let mut cfg = Config::default();
        cfg.vhdx.path = "C:\\test.vhdx".to_string();
        cfg.user.name = "roundtrip_user".to_string();
        cfg.uuid = Some("test-uuid-1234".to_string());

        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_str().unwrap();

        cfg.save(path).unwrap();
        let loaded = Config::load(path).unwrap();

        assert_eq!(loaded.vhdx.path, cfg.vhdx.path);
        assert_eq!(loaded.uuid, cfg.uuid);
    }

    #[test]
    fn test_ext4_sync_default() {
        let sync = Ext4SyncConfig::default();
        assert_eq!(sync.mount_point, "/mnt/ext4-root");
    }
}

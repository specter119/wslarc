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
    #[serde(default)]
    pub subvolumes: SubvolumesConfig,
    pub btrbk: BtrbkConfig,
    /// Ext4 root sync config (for systemd version sync)
    #[serde(default)]
    pub ext4_sync: Ext4SyncConfig,

    /// UUID of the Btrfs filesystem (set after formatting)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distribution {
    Arch,
    Debian,
}

impl Distribution {
    pub fn detect() -> Self {
        let os_release = fs::read_to_string("/etc/os-release").unwrap_or_default();
        let id = os_release
            .lines()
            .find_map(|line| line.strip_prefix("ID="))
            .unwrap_or_default()
            .trim_matches('"')
            .to_ascii_lowercase();

        match id.as_str() {
            "debian" | "ubuntu" | "linuxmint" | "kali" => Self::Debian,
            _ => Self::Arch,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Distribution::Arch => "Arch",
            Distribution::Debian => "Debian",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubvolumesConfig {
    /// A-class backup targets (subvol_name -> config)
    #[serde(default = "default_backup")]
    pub backup: HashMap<String, BackupSubvol>,
    /// B-class excluded paths (nested subvolumes)
    #[serde(default = "default_user_exclude")]
    pub exclude: ExcludeConfig,
    /// C-class transfer subvolumes (high I/O, nodatacow)
    #[serde(default = "default_transfer")]
    pub transfer: HashMap<String, TransferSubvol>,
    /// Snapshot-only subvolumes, not mounted directly
    #[serde(default = "default_snapshot_only")]
    pub snapshot_only: HashMap<String, SnapshotSubvol>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotSubvol {
    pub snapshot_name: String,
    #[serde(default = "default_etc_source")]
    pub source: String,
}

fn default_user_backup() -> HashMap<String, BackupSubvol> {
    let mut backup = HashMap::new();
    backup.insert(
        "@home".to_string(),
        BackupSubvol::Simple("/home/$USER".to_string()),
    );
    backup
}

fn default_user_exclude() -> ExcludeConfig {
    ExcludeConfig {
        parent: "@home".to_string(),
        paths: vec![
            ".cache".to_string(),
            ".local".to_string(),
            ".npm".to_string(),
            ".bun".to_string(),
            ".vscode-server-insiders".to_string(),
        ],
    }
}

fn default_common_backup() -> HashMap<String, BackupSubvol> {
    let mut backup = HashMap::new();
    backup.insert("@usr".to_string(), BackupSubvol::Simple("/usr".to_string()));
    backup.insert("@opt".to_string(), BackupSubvol::Simple("/opt".to_string()));
    backup
}

fn default_distribution_backup(distribution: Distribution) -> HashMap<String, BackupSubvol> {
    let mut backup = default_common_backup();
    match distribution {
        Distribution::Arch => {
            backup.insert(
                "@var_lib_pacman".to_string(),
                BackupSubvol::Simple("/var/lib/pacman".to_string()),
            );
        }
        Distribution::Debian => {
            backup.insert(
                "@var_lib_dpkg".to_string(),
                BackupSubvol::Simple("/var/lib/dpkg".to_string()),
            );
            backup.insert(
                "@var_lib_apt".to_string(),
                BackupSubvol::Simple("/var/lib/apt".to_string()),
            );
        }
    }

    backup
}

fn default_backup_for(distribution: Distribution) -> HashMap<String, BackupSubvol> {
    let mut backup = default_user_backup();
    backup.extend(default_distribution_backup(distribution));
    backup
}

fn default_backup() -> HashMap<String, BackupSubvol> {
    default_backup_for(Distribution::detect())
}

fn default_transfer() -> HashMap<String, TransferSubvol> {
    let mut transfer = HashMap::new();
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
    transfer.insert(
        "@containers".to_string(),
        TransferSubvol {
            mount: "/var/lib/containers".to_string(),
            nodatacow: true,
            options: None,
        },
    );
    transfer
}

fn default_snapshot_only() -> HashMap<String, SnapshotSubvol> {
    let mut snapshot_only = HashMap::new();
    snapshot_only.insert(
        "@etc".to_string(),
        SnapshotSubvol {
            snapshot_name: "etc".to_string(),
            source: default_etc_source(),
        },
    );
    snapshot_only
}

fn default_etc_source() -> String {
    "/etc".to_string()
}

impl SubvolumesConfig {
    /// Build the initial editable subvolume template for a distribution.
    pub fn for_distribution(distribution: Distribution) -> Self {
        Self {
            backup: default_backup_for(distribution),
            exclude: default_user_exclude(),
            transfer: default_transfer(),
            snapshot_only: default_snapshot_only(),
        }
    }
}

impl Default for SubvolumesConfig {
    fn default() -> Self {
        Self::for_distribution(Distribution::detect())
    }
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
    /// Preserve policy (e.g., "2d 1w 2m")
    pub preserve: String,
    /// Systemd timer schedule
    pub timer_schedule: String,
}

impl Default for BtrbkConfig {
    fn default() -> Self {
        Self {
            snapshot_dir: ".snapshots".to_string(),
            preserve_min: "latest".to_string(),
            preserve: "2d 1w 2m".to_string(),
            timer_schedule: "*-*-* 03:00:00".to_string(),
        }
    }
}

impl Config {
    /// Load an existing config, or build an initial template if it is missing.
    #[cfg(test)]
    pub fn load_or_default(path: &str, distribution: Distribution) -> Result<Self> {
        if Path::new(path).exists() {
            Self::load(path)
        } else {
            Ok(Self::for_distribution(distribution))
        }
    }

    /// Load an existing config without resolving path templates.
    ///
    /// Initialization needs to collect the target user before resolving
    /// `$USER`. Other commands should continue using [`Self::load`]
    /// so they receive paths ready for runtime use.
    pub fn load_or_default_unexpanded(path: &str, distribution: Distribution) -> Result<Self> {
        if Path::new(path).exists() {
            Self::load_unexpanded(path)
        } else {
            Ok(Self::for_distribution(distribution))
        }
    }

    /// Load config from file
    pub fn load(path: &str) -> Result<Self> {
        let mut config = Self::load_unexpanded(path)?;
        config.expand_variables();
        Ok(config)
    }

    /// Load config from file without resolving path templates.
    pub fn load_unexpanded(path: &str) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path))?;
        toml::from_str(&content).with_context(|| format!("Failed to parse config file: {}", path))
    }

    /// Return a runtime copy with path templates resolved for this config's user.
    ///
    /// The original config is not changed, so initialization can save the
    /// original `$USER` template instead of persisting a user-specific rewrite.
    pub fn resolve_variables(&self) -> Self {
        let mut config = self.clone();
        config.expand_variables();
        config
    }

    /// Set the target user without rewriting configured path templates.
    ///
    /// This is intended for initialization's interactive flow. Literal paths
    /// remain literal and `$USER` is resolved later on a runtime copy.
    pub fn set_user_unexpanded(&mut self, user: &str) {
        self.user.name = user.to_string();
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

        for backup in self.subvolumes.backup.values_mut() {
            match backup {
                BackupSubvol::Simple(m) => *m = m.replace("$USER", &user),
                BackupSubvol::Full { mount, .. } => *mount = mount.replace("$USER", &user),
            }
        }

        for subvol in self.subvolumes.transfer.values_mut() {
            subvol.mount = subvol.mount.replace("$USER", &user);
        }

        for snapshot in self.subvolumes.snapshot_only.values_mut() {
            snapshot.source = snapshot.source.replace("$USER", &user);
        }
    }

    /// Get the target user
    pub fn get_user(&self) -> String {
        self.user.name.clone()
    }

    /// Set user and expand variables in paths
    #[cfg(test)]
    pub fn set_user(&mut self, user: &str) {
        self.user.name = user.to_string();
        self.expand_variables();
    }

    /// Build a new configuration with the selected distribution's template.
    pub fn for_distribution(distribution: Distribution) -> Self {
        Self {
            vhdx: VhdxConfig {
                // Must be provided by user
                path: String::new(),
                label: format!("{}Btrfs", distribution.display_name()),
            },
            user: UserConfig {
                name: String::new(),
                options: default_useradd_options(),
            },
            mount: MountConfig {
                base: "/mnt/btrfs".to_string(),
                options: default_base_options(),
            },
            subvolumes: SubvolumesConfig::for_distribution(distribution),
            btrbk: BtrbkConfig::default(),
            ext4_sync: Ext4SyncConfig::default(),
            uuid: None,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::for_distribution(Distribution::detect())
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
        assert_eq!(
            cfg.vhdx.label,
            format!("{}Btrfs", Distribution::detect().display_name())
        );
        assert_eq!(cfg.mount.base, "/mnt/btrfs");
        assert!(cfg.mount.options.contains("compress=zstd:3"));
        assert_eq!(cfg.btrbk.preserve_min, "latest");
        assert_eq!(cfg.btrbk.preserve, "2d 1w 2m");
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
    fn test_unexpanded_load_preserves_user_template() {
        let mut cfg = Config::for_distribution(Distribution::Arch);
        cfg.user.name = "old-user".to_string();
        cfg.subvolumes.backup.insert(
            "@home".to_string(),
            BackupSubvol::Simple("/home/$USER".to_string()),
        );

        let file = NamedTempFile::new().unwrap();
        cfg.save(file.path().to_str().unwrap()).unwrap();

        let raw = Config::load_unexpanded(file.path().to_str().unwrap()).unwrap();
        assert_eq!(raw.user.name, "old-user");
        assert_eq!(
            raw.subvolumes.backup.get("@home").unwrap().mount(),
            "/home/$USER"
        );

        let runtime = raw.resolve_variables();
        assert_eq!(
            runtime.subvolumes.backup.get("@home").unwrap().mount(),
            "/home/old-user"
        );
    }

    #[test]
    fn test_set_user_unexpanded_keeps_literal_paths() {
        let mut cfg = Config::for_distribution(Distribution::Arch);
        cfg.subvolumes.backup.insert(
            "@literal".to_string(),
            BackupSubvol::Simple("/home/old-user/data".to_string()),
        );
        cfg.subvolumes.backup.insert(
            "@template".to_string(),
            BackupSubvol::Simple("/home/$USER/data".to_string()),
        );

        cfg.set_user_unexpanded("new-user");
        let runtime = cfg.resolve_variables();

        assert_eq!(
            runtime.subvolumes.backup.get("@literal").unwrap().mount(),
            "/home/old-user/data"
        );
        assert_eq!(
            runtime.subvolumes.backup.get("@template").unwrap().mount(),
            "/home/new-user/data"
        );
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
        assert!(cfg.subvolumes.backup.contains_key("@home"));
    }

    #[test]
    fn test_load_or_default_missing_file() {
        let cfg =
            Config::load_or_default("/nonexistent/path/config.toml", Distribution::Arch).unwrap();
        assert!(cfg.vhdx.path.is_empty());
        assert!(cfg.subvolumes.backup.contains_key("@var_lib_pacman"));
    }

    #[test]
    fn test_load_or_default_unexpanded_keeps_default_user_template() {
        let cfg =
            Config::load_or_default_unexpanded("/nonexistent/path/config.toml", Distribution::Arch)
                .unwrap();
        assert_eq!(
            cfg.subvolumes.backup.get("@home").unwrap().mount(),
            "/home/$USER"
        );
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
        let saved = fs::read_to_string(path).unwrap();

        assert_eq!(loaded.vhdx.path, cfg.vhdx.path);
        assert_eq!(loaded.uuid, cfg.uuid);
        assert!(!saved.contains("distribution"));
    }

    #[test]
    fn test_ext4_sync_default() {
        let sync = Ext4SyncConfig::default();
        assert_eq!(sync.mount_point, "/mnt/ext4-root");
    }
}

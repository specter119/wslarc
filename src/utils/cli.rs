use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::utils::shell::run as shell_run;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dependency {
    pub package: &'static str,
    pub commands: &'static [&'static str],
}

impl Dependency {
    pub const fn new(package: &'static str, commands: &'static [&'static str]) -> Self {
        Self { package, commands }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockDevice {
    pub name: String,
    pub label: Option<String>,
    pub fstype: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
    pub target: String,
    pub source: String,
    pub fstype: String,
    pub options: String,
    pub uuid: Option<String>,
}

pub fn ensure_dependencies(dependencies: &[Dependency]) -> Result<()> {
    let mut missing = Vec::new();

    for dependency in dependencies {
        let missing_commands: Vec<&str> = dependency
            .commands
            .iter()
            .copied()
            .filter(|command| !command_exists(command))
            .collect();

        if !missing_commands.is_empty() {
            missing.push((dependency.package, missing_commands));
        }
    }

    if missing.is_empty() {
        return Ok(());
    }

    let mut packages = Vec::new();
    let mut details = Vec::new();
    for (package, commands) in missing {
        packages.push(package);
        details.push(format!(
            "  - {} (commands: {})",
            package,
            commands.join(", ")
        ));
    }

    bail!(
        "Missing required dependencies:\n{}\nInstall with: sudo pacman -S {}",
        details.join("\n"),
        packages.join(" ")
    )
}

pub fn command_exists(command: &str) -> bool {
    let path = Path::new(command);
    if path.is_absolute() {
        return path.is_file();
    }

    env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).any(|dir| dir.join(command).is_file()))
        .unwrap_or(false)
}

pub fn find_btrfs_device_by_label(label: &str) -> Result<Option<String>> {
    Ok(list_block_devices()?
        .into_iter()
        .find(|device| {
            device.fstype.as_deref() == Some("btrfs") && device.label.as_deref() == Some(label)
        })
        .map(|device| format!("/dev/{}", device.name)))
}

pub fn list_block_device_names() -> Result<Vec<String>> {
    Ok(list_block_devices()?
        .into_iter()
        .map(|device| device.name)
        .collect())
}

pub fn read_block_device(device: &str) -> Result<Option<BlockDevice>> {
    let output = shell_run("lsblk", &["-J", "-d", "-o", "NAME,LABEL,FSTYPE", device])?;
    Ok(parse_lsblk_devices(&output)?.into_iter().next())
}

pub fn list_btrfs_mounts() -> Result<Vec<MountInfo>> {
    let output = shell_run(
        "findmnt",
        &[
            "-J",
            "-t",
            "btrfs",
            "-o",
            "TARGET,SOURCE,FSTYPE,OPTIONS,UUID",
        ],
    )?;
    parse_findmnt_mounts(&output)
}

pub fn find_mount(path: &str) -> Result<Option<MountInfo>> {
    let output = Command::new("findmnt")
        .args(["-J", path, "-o", "TARGET,SOURCE,FSTYPE,OPTIONS,UUID"])
        .output()
        .with_context(|| format!("Failed to execute: findmnt -J {}", path))?;

    if !output.status.success() {
        if output.status.code() == Some(1) {
            return Ok(None);
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Command failed: findmnt -J {}\n{}", path, stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_findmnt_mounts(&stdout)?.into_iter().next())
}

pub fn is_mountpoint(path: &str) -> bool {
    find_mount(path)
        .map(|mount| mount.is_some())
        .unwrap_or(false)
}

pub fn find_mount_uuid(path: &str) -> Option<String> {
    find_mount(path)
        .ok()
        .flatten()
        .and_then(|mount| mount.uuid)
        .filter(|uuid| !uuid.is_empty())
}

pub fn systemctl_property(unit: &str, property: &str) -> Result<String> {
    let property_arg = format!("--property={}", property);
    shell_run("systemctl", &["show", unit, &property_arg, "--value"])
}

pub fn pacman_query_version(package: &str) -> Result<Option<String>> {
    let output = Command::new("pacman")
        .args(["-Q", package])
        .output()
        .with_context(|| format!("Failed to execute: pacman -Q {}", package))?;

    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_pacman_query_version(&stdout))
}

pub fn pacman_query_depends(package: &str) -> Result<Vec<String>> {
    let output = Command::new("pacman")
        .args(["-Qi", package])
        .output()
        .with_context(|| format!("Failed to execute: pacman -Qi {}", package))?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_pacman_depends(&stdout))
}

pub fn list_directory_names(path: &str) -> Result<Vec<String>> {
    let mut entries = fs::read_dir(path)?
        .map(|entry| entry.map(|item| item.file_name().to_string_lossy().to_string()))
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort();
    Ok(entries)
}

fn list_block_devices() -> Result<Vec<BlockDevice>> {
    let output = shell_run("lsblk", &["-J", "-d", "-o", "NAME,LABEL,FSTYPE"])?;
    parse_lsblk_devices(&output)
}

fn parse_lsblk_devices(output: &str) -> Result<Vec<BlockDevice>> {
    let parsed: LsblkOutput = serde_json::from_str(output).context("Failed to parse lsblk JSON")?;
    Ok(parsed
        .blockdevices
        .into_iter()
        .map(|device| BlockDevice {
            name: device.name,
            label: device.label,
            fstype: device.fstype,
        })
        .collect())
}

fn parse_findmnt_mounts(output: &str) -> Result<Vec<MountInfo>> {
    let parsed: FindmntOutput =
        serde_json::from_str(output).context("Failed to parse findmnt JSON")?;
    let mut mounts = Vec::new();
    for filesystem in parsed.filesystems {
        flatten_filesystem(&filesystem, &mut mounts);
    }
    Ok(mounts)
}

fn flatten_filesystem(filesystem: &FindmntFilesystem, mounts: &mut Vec<MountInfo>) {
    mounts.push(MountInfo {
        target: filesystem.target.clone(),
        source: filesystem.source.clone().unwrap_or_default(),
        fstype: filesystem.fstype.clone().unwrap_or_default(),
        options: filesystem.options.clone().unwrap_or_default(),
        uuid: filesystem.uuid.clone(),
    });

    for child in &filesystem.children {
        flatten_filesystem(child, mounts);
    }
}

fn parse_pacman_query_version(output: &str) -> Option<String> {
    let line = output.lines().next()?.trim();
    let (_, version) = line.split_once(char::is_whitespace)?;
    let version = version.trim();
    (!version.is_empty()).then(|| version.to_string())
}

fn parse_pacman_depends(output: &str) -> Vec<String> {
    let mut deps = Vec::new();
    let mut in_depends = false;

    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("Depends On") {
            in_depends = true;
            if let Some((_, after_colon)) = rest.split_once(':') {
                push_pacman_dep_tokens(after_colon, &mut deps);
            }
            continue;
        }

        if in_depends {
            if line.starts_with(' ') || line.starts_with('\t') {
                push_pacman_dep_tokens(line, &mut deps);
            } else {
                break;
            }
        }
    }

    deps
}

fn push_pacman_dep_tokens(line: &str, deps: &mut Vec<String>) {
    for token in line.split_whitespace() {
        if token == "None" {
            continue;
        }

        let name = token.split(['<', '>', '=']).next().unwrap_or("").trim();

        if !name.is_empty() {
            deps.push(name.to_string());
        }
    }
}

#[derive(Debug, Deserialize)]
struct LsblkOutput {
    #[serde(default)]
    blockdevices: Vec<LsblkDevice>,
}

#[derive(Debug, Deserialize)]
struct LsblkDevice {
    name: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    fstype: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FindmntOutput {
    #[serde(default)]
    filesystems: Vec<FindmntFilesystem>,
}

#[derive(Debug, Deserialize)]
struct FindmntFilesystem {
    #[serde(default)]
    target: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    fstype: Option<String>,
    #[serde(default)]
    options: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    children: Vec<FindmntFilesystem>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn parse_lsblk_devices_reads_json() {
        let output = r#"{
            "blockdevices": [
                {"name":"sda","label":"ArchBtrfs","fstype":"btrfs"},
                {"name":"sdb","label":null,"fstype":"ext4"}
            ]
        }"#;

        let devices = parse_lsblk_devices(output).unwrap();

        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].name, "sda");
        assert_eq!(devices[0].label.as_deref(), Some("ArchBtrfs"));
        assert_eq!(devices[0].fstype.as_deref(), Some("btrfs"));
    }

    #[test]
    fn parse_findmnt_mounts_flattens_children() {
        let output = r#"{
            "filesystems": [
                {
                    "target": "/mnt/btrfs",
                    "source": "/dev/sdd",
                    "fstype": "btrfs",
                    "options": "rw",
                    "children": [
                        {
                            "target": "/usr",
                            "source": "/dev/sdd",
                            "fstype": "btrfs",
                            "options": "rw,subvol=@usr"
                        }
                    ]
                }
            ]
        }"#;

        let mounts = parse_findmnt_mounts(output).unwrap();

        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].target, "/mnt/btrfs");
        assert_eq!(mounts[1].target, "/usr");
        assert_eq!(mounts[0].uuid, None);
    }

    #[test]
    fn parse_pacman_query_version_extracts_version() {
        let version = parse_pacman_query_version("systemd 260.1-1\n");

        assert_eq!(version.as_deref(), Some("260.1-1"));
    }

    #[test]
    fn parse_pacman_depends_strips_constraints() {
        let output = "\
Depends On      : glibc  libcap>=2.0  sh\n\
Optional Deps   : None\n";

        let deps = parse_pacman_depends(output);

        assert_eq!(deps, vec!["glibc", "libcap", "sh"]);
    }

    #[test]
    fn ensure_dependencies_reports_packages() {
        let dependency = Dependency::new("fakepkg", &["missingcmd"]);
        let error = ensure_dependencies(&[dependency]).unwrap_err().to_string();

        assert!(error.contains("fakepkg"));
        assert!(error.contains("sudo pacman -S fakepkg"));
    }

    #[test]
    fn list_directory_names_returns_sorted_names() {
        let tempdir = tempdir().unwrap();
        fs::write(tempdir.path().join("b"), "").unwrap();
        fs::write(tempdir.path().join("a"), "").unwrap();

        let entries = list_directory_names(tempdir.path().to_string_lossy().as_ref()).unwrap();

        assert_eq!(entries, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn command_exists_accepts_absolute_paths() {
        let tempdir = tempdir().unwrap();
        let fake = tempdir.path().join("fakecmd");
        fs::write(&fake, "echo ok").unwrap();

        assert!(command_exists(fake.to_string_lossy().as_ref()));
        assert!(!command_exists(
            PathBuf::from(tempdir.path())
                .join("missing")
                .to_string_lossy()
                .as_ref()
        ));
    }
}

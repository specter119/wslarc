use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::config::Distribution;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacmanPackage {
    pub name: String,
    pub version: String,
    pub architecture: String,
}

pub const PACMAN_SYNC_GUARD_ENV: &str = "WSLARC_SYSTEMD_SYNC_IN_PROGRESS";

pub fn ensure_dependencies_for(
    dependencies: &[Dependency],
    distribution: Distribution,
) -> Result<()> {
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

    let install_command = match distribution {
        Distribution::Arch => format!("sudo pacman -S {}", packages.join(" ")),
        Distribution::Debian => format!("sudo apt-get install {}", packages.join(" ")),
    };

    bail!(
        "Missing required dependencies for {}:\n{}\nInstall with: {}",
        distribution.display_name(),
        details.join("\n"),
        install_command
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

pub fn pacman_query_package(package: &str) -> Result<Option<PacmanPackage>> {
    query_pacman_package(&["-Qi", package], &format!("pacman -Qi {}", package))
}

pub fn pacman_query_package_in_root(root: &str, package: &str) -> Result<Option<PacmanPackage>> {
    query_pacman_package(
        &["--sysroot", root, "-Qi", package],
        &format!("pacman --sysroot {} -Qi {}", root, package),
    )
}

pub fn pacman_query_archive_package(path: &Path) -> Result<Option<PacmanPackage>> {
    let path = path.to_string_lossy();
    query_pacman_package(&["-Qip", path.as_ref()], &format!("pacman -Qip {}", path))
}

pub fn pacman_install_archives(root: &str, archives: &[String]) -> Result<()> {
    if archives.is_empty() {
        return Ok(());
    }

    let version = shell_run("pacman", &["--version"])?;
    let args = pacman_install_args(root, archives, pacman_sysroot_chroots(&version)?)?;
    let mut command = Command::new("pacman");
    command.env(PACMAN_SYNC_GUARD_ENV, "1").args(&args);

    let output = command
        .output()
        .with_context(|| format!("Failed to execute: pacman --sysroot {} -U", root))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Command failed: pacman --sysroot {} -U\n{}",
            root,
            stderr.trim()
        );
    }

    Ok(())
}

fn pacman_sysroot_chroots(version: &str) -> Result<bool> {
    let version = version
        .split("Pacman v")
        .nth(1)
        .context("Cannot identify pacman version")?;
    let mut parts = version.split('.');
    let major = parts
        .next()
        .context("Missing pacman major version")?
        .parse::<u32>()?;
    let minor = parts
        .next()
        .context("Missing pacman minor version")?
        .parse::<u32>()?;
    // pacman 7.1 prepends configuration paths instead of entering a chroot.
    Ok((major, minor) < (7, 1))
}

fn pacman_install_args(root: &str, archives: &[String], chroots: bool) -> Result<Vec<String>> {
    let mut args = vec![
        "--sysroot".to_string(),
        root.to_string(),
        "-U".to_string(),
        "--noconfirm".to_string(),
    ];
    for archive in archives {
        let relative = Path::new(archive)
            .strip_prefix(root)
            .with_context(|| format!("Archive is outside the target sysroot: {archive}"))?;
        if relative
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            bail!("Invalid archive path inside sysroot: {archive}");
        }
        args.push(if chroots {
            format!("/{}", relative.display())
        } else {
            archive.clone()
        });
    }
    Ok(args)
}

pub fn pacman_query_depends(package: &str) -> Result<Vec<String>> {
    let output = Command::new("pacman")
        .env("LC_ALL", "C")
        .args(["-Qi", package])
        .output()
        .with_context(|| format!("Failed to execute: pacman -Qi {}", package))?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_pacman_depends(&stdout))
}

pub fn debian_query_version(package: &str) -> Result<Option<String>> {
    let output = Command::new("dpkg-query")
        .args(["-W", "-f", "${Status}\t${Version}", package])
        .output()
        .with_context(|| format!("Failed to execute: dpkg-query -W {}", package))?;

    if !output.status.success() {
        return Ok(None);
    }

    Ok(parse_debian_status_version(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

pub fn debian_query_depends(package: &str) -> Result<Vec<String>> {
    let output = Command::new("dpkg-query")
        .args(["-W", "-f", "${Depends}\n${Pre-Depends}", package])
        .output()
        .with_context(|| format!("Failed to execute: dpkg-query -W {}", package))?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    let alternatives = parse_debian_depends(&String::from_utf8_lossy(&output.stdout));
    let mut dependencies = Vec::new();

    for group in alternatives {
        if let Some(provider) = select_debian_dependency(group, |dependency| {
            debian_query_version(dependency).map(|version| version.is_some())
        })? {
            dependencies.push(provider);
        }
    }

    Ok(dependencies)
}

pub fn debian_query_files(package: &str) -> Result<Vec<String>> {
    let output = Command::new("dpkg-query")
        .args(["-L", package])
        .output()
        .with_context(|| format!("Failed to execute: dpkg-query -L {}", package))?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|path| path.starts_with('/') && *path != "/")
        .map(str::to_string)
        .collect())
}

pub fn debian_query_deb_package_name(path: &str) -> Result<Option<String>> {
    let output = Command::new("dpkg-deb")
        .args(["-f", path, "Package"])
        .output()
        .with_context(|| format!("Failed to execute: dpkg-deb -f {} Package", path))?;

    if !output.status.success() {
        return Ok(None);
    }

    let package = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((!package.is_empty()).then_some(package))
}

pub fn debian_file_owned_by_installed_package(path: &str) -> Result<bool> {
    let output = Command::new("dpkg-query")
        .args(["-S", path])
        .output()
        .with_context(|| format!("Failed to execute: dpkg-query -S {}", path))?;

    if !output.status.success() {
        return Ok(false);
    }

    for owner in String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            line.split_once(": ")
                .or_else(|| line.split_once(':'))
                .map(|(package, _)| package.trim())
        })
    {
        if debian_query_version(owner)?.is_some() {
            return Ok(true);
        }
    }

    Ok(false)
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

fn query_pacman_package(args: &[&str], command_description: &str) -> Result<Option<PacmanPackage>> {
    let output = Command::new("pacman")
        .env("LC_ALL", "C")
        .args(args)
        .output()
        .with_context(|| format!("Failed to execute: {}", command_description))?;

    if !output.status.success() {
        return Ok(None);
    }

    Ok(parse_pacman_package_info(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn parse_pacman_package_info(output: &str) -> Option<PacmanPackage> {
    let field = |name: &str| {
        output.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == name).then(|| value.trim().to_string())
        })
    };

    let name = field("Name")?;
    let version = field("Version")?;
    let architecture = field("Architecture")?;

    if name.is_empty() || version.is_empty() || architecture.is_empty() {
        return None;
    }

    Some(PacmanPackage {
        name,
        version,
        architecture,
    })
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

fn parse_debian_status_version(output: &str) -> Option<String> {
    let (status, version) = output.trim().split_once('\t')?;
    if status.split_whitespace().last() != Some("installed") {
        return None;
    }

    (!version.is_empty()).then_some(version.to_string())
}

fn parse_debian_depends(output: &str) -> Vec<Vec<String>> {
    let mut dependency_groups = Vec::new();

    for alternative_group in output.split([',', '\n']) {
        let mut alternatives = Vec::new();
        for package in alternative_group
            .split('|')
            .map(str::trim)
            .map(|dependency| dependency.split_whitespace().next().unwrap_or_default())
            .map(|dependency| dependency.trim_end_matches([')', '(']))
        {
            if !package.is_empty()
                && !package.starts_with("${")
                && !alternatives.iter().any(|existing| existing == package)
            {
                alternatives.push(package.to_string());
            }
        }

        if !alternatives.is_empty() {
            dependency_groups.push(alternatives);
        }
    }

    dependency_groups
}

fn select_debian_dependency<F>(
    alternatives: Vec<String>,
    mut is_installed: F,
) -> Result<Option<String>>
where
    F: FnMut(&str) -> Result<bool>,
{
    for dependency in alternatives {
        if is_installed(&dependency)? {
            return Ok(Some(dependency));
        }
    }

    Ok(None)
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
    fn parse_pacman_package_info_keeps_any_architecture() {
        let package = parse_pacman_package_info(
            "Name            : systemd\n\
             Version         : 256.5-1\n\
             Architecture    : any\n",
        )
        .unwrap();

        assert_eq!(package.name, "systemd");
        assert_eq!(package.version, "256.5-1");
        assert_eq!(package.architecture, "any");
    }

    #[test]
    fn parse_pacman_archive_info_uses_supported_query_output() {
        assert_eq!(
            parse_pacman_package_info("Name : systemd\nVersion : 256.5-1\nArchitecture : any\n"),
            Some(PacmanPackage {
                name: "systemd".to_string(),
                version: "256.5-1".to_string(),
                architecture: "any".to_string(),
            })
        );
    }

    #[test]
    fn pacman_install_args_use_paths_inside_legacy_sysroot() {
        let args = pacman_install_args(
            "/mnt/ext4",
            &["/mnt/ext4/var/cache/pacman/pkg/systemd.pkg.tar.zst".to_string()],
            true,
        )
        .unwrap();

        assert_eq!(
            args,
            vec![
                "--sysroot",
                "/mnt/ext4",
                "-U",
                "--noconfirm",
                "/var/cache/pacman/pkg/systemd.pkg.tar.zst"
            ]
        );
    }

    #[test]
    fn pacman_install_rejects_archives_outside_sysroot() {
        assert!(
            pacman_install_args("/mnt/ext4", &["/var/cache/pkg.tar.zst".into()], false).is_err()
        );
        assert!(
            pacman_install_args("/mnt/ext4", &["/mnt/ext4/../pkg.tar.zst".into()], false).is_err()
        );
    }

    #[test]
    fn pacman_71_keeps_host_archive_paths() {
        assert!(pacman_sysroot_chroots("Pacman v7.0.0 - libalpm").unwrap());
        assert!(!pacman_sysroot_chroots("Pacman v7.1.0 - libalpm").unwrap());
        assert!(pacman_sysroot_chroots("unknown").is_err());
        let archive = "/mnt/ext4/var/cache/pkg.tar.zst".to_string();
        let args = pacman_install_args("/mnt/ext4", std::slice::from_ref(&archive), false).unwrap();
        assert_eq!(args.last(), Some(&archive));
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
    fn parse_debian_depends_strips_constraints_and_alternatives() {
        let output =
            "libc6 (>= 2.34), libcap2 (>= 1:2.10), default-logind | logind, ${shlibs:Depends}";

        assert_eq!(
            parse_debian_depends(output),
            vec![
                vec!["libc6".to_string()],
                vec!["libcap2".to_string()],
                vec!["default-logind".to_string(), "logind".to_string()]
            ]
        );
    }

    #[test]
    fn parse_debian_status_version_only_accepts_installed_packages() {
        assert_eq!(
            parse_debian_status_version("install ok installed\t1.2.3\n"),
            Some("1.2.3".to_string())
        );
        assert_eq!(
            parse_debian_status_version("hold ok installed\t1.2.3\n"),
            Some("1.2.3".to_string())
        );
        assert_eq!(
            parse_debian_status_version("deinstall ok config-files\t1.2.3\n"),
            None
        );
    }

    #[test]
    fn parse_debian_depends_preserves_multiarch_identifiers() {
        assert_eq!(
            parse_debian_depends("libc6:amd64 (>= 2.34), libfoo:any"),
            vec![
                vec!["libc6:amd64".to_string()],
                vec!["libfoo:any".to_string()]
            ]
        );
    }

    #[test]
    fn parse_debian_depends_includes_pre_depends_on_a_new_line() {
        assert_eq!(
            parse_debian_depends("libc6 (>= 2.34)\ninit-system-helpers (>= 1.18)"),
            vec![
                vec!["libc6".to_string()],
                vec!["init-system-helpers".to_string()]
            ]
        );
    }

    #[test]
    fn select_debian_dependency_uses_installed_alternative() {
        let selected = select_debian_dependency(
            vec!["default-logind".to_string(), "logind".to_string()],
            |dependency| Ok(dependency == "logind"),
        )
        .unwrap();

        assert_eq!(selected, Some("logind".to_string()));
    }

    #[test]
    fn ensure_dependencies_reports_packages() {
        let dependency = Dependency::new("fakepkg", &["missingcmd"]);
        let error = ensure_dependencies_for(&[dependency], Distribution::Arch)
            .unwrap_err()
            .to_string();

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

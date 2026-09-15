use anyhow::{bail, Context, Result};
use std::collections::{BTreeSet, HashSet};
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::config::{Config, Distribution};
use crate::generators::ext4_sync;
use crate::utils::cli::{
    debian_file_owned_by_installed_package, debian_query_deb_package_name, debian_query_files,
    debian_query_version, is_mountpoint, pacman_install_archives, pacman_query_archive_package,
    pacman_query_package, pacman_query_package_in_root, pacman_query_version, PacmanPackage,
    PACMAN_SYNC_GUARD_ENV,
};
use crate::utils::prompt::{info, success, warn};
use crate::utils::shell::run_or_dry;

const SYNC_MANIFEST: &str = ".wslarc-systemd-sync-manifest";

pub fn run(config: &Config, distribution: Distribution, dry_run: bool) -> Result<()> {
    if env::var_os(PACMAN_SYNC_GUARD_ENV).is_some() {
        info("Skipping nested pacman hook during ext4 package installation");
        return Ok(());
    }

    let triggered = read_triggered_packages();
    if distribution == Distribution::Arch {
        run_arch_with_retry(config, dry_run, &triggered)
    } else {
        run_with_triggered(config, distribution, dry_run, &triggered)
    }
}

pub fn run_apt_pre() -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;

    record_apt_pending(
        Path::new(ext4_sync::APT_PENDING_PACKAGES),
        &input,
        debian_query_deb_package_name,
    )
}

pub fn run_apt_post(config: &Config, distribution: Distribution, dry_run: bool) -> Result<()> {
    let pending_path = Path::new(ext4_sync::APT_PENDING_PACKAGES);
    let triggered = read_package_file(pending_path)?;

    let targets = ext4_sync::collect_hook_targets(distribution)?;
    let current_state = installed_package_state(distribution, &targets)?;
    let previous_state = read_state(Path::new(ext4_sync::APT_SYNC_STATE))?;
    let state_changed = previous_state.as_deref() != Some(current_state.as_str());

    if triggered.is_empty() {
        if !state_changed {
            info("No APT packages require ext4 synchronization");
            return Ok(());
        }
        info("The installed systemd package set changed; refreshing ext4 sync");
    }

    finish_apt_post(
        pending_path,
        Path::new(ext4_sync::APT_SYNC_STATE),
        &triggered,
        &current_state,
        previous_state.as_deref(),
        dry_run,
        |sync_triggers| run_with_triggered(config, distribution, dry_run, sync_triggers),
    )
}

fn run_arch_with_retry(config: &Config, dry_run: bool, triggered: &[String]) -> Result<()> {
    let pending_path = Path::new(ext4_sync::ARCH_PENDING_PACKAGES);
    let pending = read_package_file(pending_path)?;
    let combined = merge_package_lines(&pending, triggered);
    let retry_state = if combined.is_empty() {
        ext4_sync::collect_hook_targets(Distribution::Arch)?
    } else {
        combined.clone()
    };

    if !dry_run && !retry_state.is_empty() {
        write_package_file(pending_path, &retry_state)?;
    }

    run_with_triggered(config, Distribution::Arch, dry_run, &combined)?;

    if !dry_run {
        clear_pending(pending_path)?;
    }

    Ok(())
}

fn run_with_triggered(
    config: &Config,
    distribution: Distribution,
    dry_run: bool,
    triggered: &[String],
) -> Result<()> {
    if !triggered.is_empty() {
        info(&format!("Triggered by: {}", triggered.join(", ")));
    }

    let packages = select_sync_packages(distribution, triggered)?;
    if packages.is_empty() && !(distribution == Distribution::Debian && triggered.is_empty()) {
        info("No relevant systemd packages require ext4 synchronization");
        return Ok(());
    }

    let mount_point = &config.ext4_sync.mount_point;
    ensure_mounted(mount_point, dry_run)?;

    match distribution {
        Distribution::Arch => {
            sync_arch_packages(mount_point, &packages, dry_run)?;
        }
        Distribution::Debian => {
            sync_debian_files(mount_point, &packages, dry_run)?;
        }
    }

    success(&format!(
        "{} ext4 systemd sync complete",
        distribution.display_name()
    ));
    Ok(())
}

fn ensure_mounted(mount_point: &str, dry_run: bool) -> Result<()> {
    if is_mountpoint(mount_point) {
        info(&format!("{} already mounted", mount_point));
        return Ok(());
    }

    let ext4_uuid = ext4_sync::get_ext4_root_uuid()
        .ok_or_else(|| anyhow::anyhow!("Could not get ext4 root UUID"))?;

    run_or_dry(
        "mount",
        &[&format!("UUID={}", ext4_uuid), mount_point],
        dry_run,
    )?;
    info(&format!("Mounted ext4 root to {}", mount_point));
    Ok(())
}

fn read_triggered_packages() -> Vec<String> {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return Vec::new();
    }

    parse_package_lines(&input)
}

fn read_package_file(path: &Path) -> Result<Vec<String>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    Ok(parse_package_lines(&fs::read_to_string(path)?))
}

fn record_apt_pending<F>(pending_path: &Path, input: &str, mut package_name: F) -> Result<()>
where
    F: FnMut(&str) -> Result<Option<String>>,
{
    let mut packages: BTreeSet<String> = read_package_file(pending_path)?.into_iter().collect();
    for deb_path in input
        .lines()
        .map(str::trim)
        .filter(|path| path.ends_with(".deb"))
    {
        if let Some(package) = package_name(deb_path)? {
            packages.insert(package);
        }
    }

    if packages.is_empty() {
        return Ok(());
    }

    write_package_file(pending_path, &packages.into_iter().collect::<Vec<_>>())
}

fn write_package_file(path: &Path, packages: &[String]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Invalid package state path"))?;
    fs::create_dir_all(parent)?;

    let temp_path = path.with_extension("tmp");
    let content = if packages.is_empty() {
        String::new()
    } else {
        format!("{}\n", packages.join("\n"))
    };
    fs::write(&temp_path, content)?;
    fs::rename(temp_path, path)?;
    Ok(())
}

fn clear_pending(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn finish_apt_post<F>(
    pending_path: &Path,
    state_path: &Path,
    triggered: &[String],
    current_state: &str,
    previous_state: Option<&str>,
    dry_run: bool,
    sync: F,
) -> Result<()>
where
    F: FnOnce(&[String]) -> Result<()>,
{
    let state_changed = previous_state != Some(current_state);
    let sync_triggers: &[String] = if state_changed { &[] } else { triggered };
    sync(sync_triggers)?;

    if !dry_run {
        write_state(state_path, current_state)?;
        clear_pending(pending_path)?;
    }

    Ok(())
}

fn merge_package_lines(left: &[String], right: &[String]) -> Vec<String> {
    let mut packages = BTreeSet::new();
    packages.extend(left.iter().cloned());
    packages.extend(right.iter().cloned());
    packages.into_iter().collect()
}

fn parse_package_lines(input: &str) -> Vec<String> {
    let mut packages = HashSet::new();
    for line in input.lines() {
        let name = line.trim();
        if !name.is_empty() {
            packages.insert(name.to_string());
        }
    }

    let mut list: Vec<String> = packages.into_iter().collect();
    list.sort();
    list
}

fn installed_package_state(distribution: Distribution, packages: &[String]) -> Result<String> {
    installed_package_state_with(packages, |package| match distribution {
        Distribution::Arch => pacman_query_version(package),
        Distribution::Debian => debian_query_version(package),
    })
}

fn installed_package_state_with<F>(packages: &[String], mut version_for: F) -> Result<String>
where
    F: FnMut(&str) -> Result<Option<String>>,
{
    let mut states = Vec::new();
    for package in packages {
        if let Some(version) = version_for(package)? {
            states.push(format!("{}={}", package, version));
        }
    }
    states.sort();
    states.dedup();
    Ok(states.join("\n"))
}

fn read_state(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }

    Ok(Some(fs::read_to_string(path)?))
}

fn write_state(path: &Path, state: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Invalid APT sync state path"))?;
    fs::create_dir_all(parent)?;

    let temp_path = path.with_extension("tmp");
    fs::write(&temp_path, state)?;
    fs::rename(temp_path, path)?;
    Ok(())
}

fn select_sync_packages(distribution: Distribution, triggered: &[String]) -> Result<Vec<String>> {
    let hook_targets = ext4_sync::collect_hook_targets(distribution)?;
    Ok(filter_sync_packages(triggered, &hook_targets))
}

fn filter_sync_packages(triggered: &[String], hook_targets: &[String]) -> Vec<String> {
    if triggered.is_empty() {
        return hook_targets.to_vec();
    }

    let allowed: HashSet<&str> = hook_targets.iter().map(String::as_str).collect();
    if triggered.iter().any(|pkg| allowed.contains(pkg.as_str())) {
        hook_targets.to_vec()
    } else {
        Vec::new()
    }
}

fn get_arch_package_metadata(packages: &[String]) -> Result<Vec<PacmanPackage>> {
    let mut metadata = Vec::new();
    for pkg in packages {
        if let Some(package) = pacman_query_package(pkg)? {
            metadata.push(package);
        } else {
            bail!(
                "Required host Arch package {} is no longer installed; retaining sync retry",
                pkg
            );
        }
    }
    Ok(metadata)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArchArchive {
    package: PacmanPackage,
    source: PathBuf,
    destination: PathBuf,
}

fn sync_arch_packages(mount_point: &str, packages: &[String], dry_run: bool) -> Result<()> {
    let host_packages = get_arch_package_metadata(packages)?;
    let packages_to_install = select_arch_packages_to_install(&host_packages, |package| {
        pacman_query_package_in_root(mount_point, package)
    })?;

    if packages_to_install.is_empty() {
        info("All target Arch packages are already current");
        return Ok(());
    }

    let archives = resolve_arch_archives(
        Path::new("/var/cache/pacman/pkg"),
        &Path::new(mount_point).join("var/cache/pacman/pkg"),
        &packages_to_install,
        pacman_query_archive_package,
    )?;

    sync_arch_cache(&archives, dry_run)?;
    install_arch_packages(mount_point, &archives, dry_run)?;
    Ok(())
}

fn select_arch_packages_to_install<F>(
    packages: &[PacmanPackage],
    mut target_package: F,
) -> Result<Vec<PacmanPackage>>
where
    F: FnMut(&str) -> Result<Option<PacmanPackage>>,
{
    let mut packages_to_install = Vec::new();
    for package in packages {
        let target = target_package(&package.name)?;
        let is_current = target.as_ref().is_some_and(|installed| {
            installed.name == package.name
                && installed.version == package.version
                && installed.architecture == package.architecture
        });
        if !is_current {
            packages_to_install.push(package.clone());
        }
    }
    Ok(packages_to_install)
}

fn resolve_arch_archives<F>(
    source_cache: &Path,
    destination_cache: &Path,
    packages: &[PacmanPackage],
    mut archive_package: F,
) -> Result<Vec<ArchArchive>>
where
    F: FnMut(&Path) -> Result<Option<PacmanPackage>>,
{
    let mut entries = fs::read_dir(source_cache)
        .with_context(|| {
            format!(
                "Failed to read Arch package cache {}",
                source_cache.display()
            )
        })?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_type()
                .map(|file_type| file_type.is_file())
                .unwrap_or(false)
        })
        .filter(|entry| entry.file_name().to_string_lossy().contains(".pkg.tar."))
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());

    let mut archives = Vec::new();
    for package in packages {
        let mut archive = None;
        let prefix = format!("{}-", package.name);
        for entry in &entries {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with(&prefix) || name.ends_with(".sig") {
                continue;
            }
            let path = entry.path();
            let Some(metadata) = archive_package(&path)? else {
                continue;
            };
            if metadata == *package {
                archive = Some((path, metadata));
                break;
            }
        }

        let Some((source, metadata)) = archive else {
            bail!(
                "Required cached Arch archive is missing for {} {} {} in {}",
                package.name,
                package.version,
                package.architecture,
                source_cache.display()
            );
        };

        let destination = destination_cache.join(
            source
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("Invalid Arch package cache entry"))?,
        );
        archives.push(ArchArchive {
            package: metadata,
            source,
            destination,
        });
    }

    Ok(archives)
}

fn sync_arch_cache(archives: &[ArchArchive], dry_run: bool) -> Result<()> {
    let Some(destination_cache) = archives
        .first()
        .and_then(|archive| archive.destination.parent())
    else {
        return Ok(());
    };

    if !dry_run {
        fs::create_dir_all(destination_cache)?;
    }

    for archive in archives {
        if dry_run {
            info(&format!(
                "[dry-run] Would copy {} to {}",
                archive.source.display(),
                archive.destination.display()
            ));
        } else {
            fs::copy(&archive.source, &archive.destination)
                .map_err(anyhow::Error::from)
                .with_context(|| {
                    format!("Failed to copy cached package {}", archive.source.display())
                })?;
            info(&format!(
                "Copied {}",
                archive
                    .source
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            ));
        }
    }
    Ok(())
}

fn install_arch_packages(mount_point: &str, archives: &[ArchArchive], dry_run: bool) -> Result<()> {
    if archives.is_empty() {
        info("No packages to sync");
        return Ok(());
    }

    let package_paths: Vec<String> = archives
        .iter()
        .map(|archive| archive.destination.to_string_lossy().into_owned())
        .collect();

    if dry_run {
        info(&format!(
            "[dry-run] Would install {} Arch package archive(s) into {}",
            package_paths.len(),
            mount_point
        ));
    } else {
        // The CLI adapter handles legacy chroot and current sysroot path semantics.
        pacman_install_archives(mount_point, &package_paths)?;
    }
    Ok(())
}

fn sync_debian_files(mount_point: &str, packages: &[String], dry_run: bool) -> Result<()> {
    let mut files = Vec::new();
    for package in packages {
        if debian_query_version(package)?.is_none() {
            warn(&format!("Package {} not installed, skipping", package));
            continue;
        }
        files.extend(
            debian_query_files(package)?
                .into_iter()
                .filter(|file| is_syncable_file(file)),
        );
    }
    files.sort();
    files.dedup();

    let manifest_path = Path::new(mount_point).join(SYNC_MANIFEST);
    let previous_files = read_manifest(&manifest_path)?;
    let sync_files: Vec<String> = files
        .into_iter()
        .filter(|file| dry_run || Path::new(file).exists())
        .collect();

    for file in &sync_files {
        run_or_dry("rsync", &["-aAX", "--relative", file, mount_point], dry_run)?;
    }

    if !dry_run {
        remove_stale_files(mount_point, &previous_files, &sync_files)?;
        write_manifest(&manifest_path, &sync_files)?;
    }

    // The ext4 root has its own linker cache. Rebuild it after the package
    // files have been copied from the Btrfs-backed system.
    run_or_dry("ldconfig", &["-r", mount_point], dry_run)?;
    Ok(())
}

fn is_syncable_file(path: &str) -> bool {
    match fs::symlink_metadata(path) {
        Ok(metadata) => metadata.file_type().is_file() || metadata.file_type().is_symlink(),
        Err(_) => false,
    }
}

fn read_manifest(path: &Path) -> Result<Vec<String>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    Ok(parse_package_lines(&fs::read_to_string(path)?))
}

fn remove_stale_files(mount_point: &str, previous: &[String], current: &[String]) -> Result<()> {
    remove_stale_files_with(mount_point, previous, current, |file| {
        debian_file_owned_by_installed_package(file)
    })
}

fn remove_stale_files_with<F>(
    mount_point: &str,
    previous: &[String],
    current: &[String],
    mut is_owned: F,
) -> Result<()>
where
    F: FnMut(&str) -> Result<bool>,
{
    let current: HashSet<&str> = current.iter().map(String::as_str).collect();

    for file in previous {
        if current.contains(file.as_str()) {
            continue;
        }

        let relative = file
            .strip_prefix('/')
            .ok_or_else(|| anyhow::anyhow!("Manifest path is not absolute: {}", file))?;
        let target = Path::new(mount_point).join(relative);
        if is_owned(file)? {
            continue;
        }

        match fs::remove_file(&target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Could not remove stale synced file {}", target.display())
                });
            }
        }
    }

    Ok(())
}

fn write_manifest(path: &Path, files: &[String]) -> Result<()> {
    let temp_path = PathBuf::from(format!("{}.tmp", path.display()));
    let content = if files.is_empty() {
        String::new()
    } else {
        format!("{}\n", files.join("\n"))
    };
    fs::write(&temp_path, content)?;
    fs::rename(temp_path, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn sample_hook_targets() -> Vec<String> {
        vec![
            "systemd".to_string(),
            "libgcrypt".to_string(),
            "glibc".to_string(),
        ]
    }

    #[test]
    fn filter_sync_packages_syncs_full_closure_when_target_triggered() {
        let packages = filter_sync_packages(
            &["libgcrypt".to_string(), "not-a-target".to_string()],
            &sample_hook_targets(),
        );

        assert_eq!(packages, sample_hook_targets());
    }

    #[test]
    fn filter_sync_packages_falls_back_to_all_targets_without_stdin() {
        let packages = filter_sync_packages(&[], &sample_hook_targets());

        assert!(packages.iter().any(|pkg| pkg == "systemd"));
        assert!(packages.iter().any(|pkg| pkg == "libgcrypt"));
    }

    #[test]
    fn filter_sync_packages_discards_unknown_triggers() {
        let packages = filter_sync_packages(&["unknown".to_string()], &sample_hook_targets());

        assert!(packages.is_empty());
    }

    #[test]
    fn parse_package_lines_sorts_and_deduplicates() {
        assert_eq!(
            parse_package_lines("libb\nliba\nlibb\n"),
            vec!["liba".to_string(), "libb".to_string()]
        );
    }

    #[test]
    fn directory_entries_are_not_syncable_files() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let file = directory.path().join("file");
        std::fs::write(&file, "content").unwrap();

        assert!(!is_syncable_file(nested.to_str().unwrap()));
        assert!(is_syncable_file(file.to_str().unwrap()));
    }

    #[test]
    fn manifest_removes_files_no_longer_in_current_package_list() {
        let directory = tempfile::tempdir().unwrap();
        let stale = directory.path().join("etc/old.conf");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, "old").unwrap();

        remove_stale_files_with(
            directory.path().to_str().unwrap(),
            &["/etc/old.conf".to_string()],
            &[],
            |_| Ok(false),
        )
        .unwrap();

        assert!(!stale.exists());
    }

    #[test]
    fn manifest_keeps_files_still_owned_by_installed_package() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("etc/owned.conf");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "owned").unwrap();

        remove_stale_files_with(
            directory.path().to_str().unwrap(),
            &["/etc/owned.conf".to_string()],
            &[],
            |_| Ok(true),
        )
        .unwrap();

        assert!(file.exists());
    }

    #[test]
    fn manifest_roundtrip_is_sorted_and_deduplicated() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("manifest");
        write_manifest(&path, &["/usr/lib/b".to_string(), "/usr/lib/a".to_string()]).unwrap();

        assert_eq!(
            read_manifest(&path).unwrap(),
            vec!["/usr/lib/a".to_string(), "/usr/lib/b".to_string()]
        );
    }

    fn package(name: &str, version: &str, architecture: &str) -> PacmanPackage {
        PacmanPackage {
            name: name.to_string(),
            version: version.to_string(),
            architecture: architecture.to_string(),
        }
    }

    #[test]
    fn archive_mapping_uses_real_name_version_and_architecture() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("cache");
        let destination = directory.path().join("target-cache");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("systemd-256.5-1-any.pkg.tar.zst"), "").unwrap();
        fs::write(source.join("glibc-2.40-1-x86_64.pkg.tar.zst"), "").unwrap();

        let archives = resolve_arch_archives(
            &source,
            &destination,
            &[
                package("systemd", "256.5-1", "any"),
                package("glibc", "2.40-1", "x86_64"),
            ],
            |path| {
                Ok(match path.file_name().unwrap().to_str().unwrap() {
                    "systemd-256.5-1-any.pkg.tar.zst" => Some(package("systemd", "256.5-1", "any")),
                    "glibc-2.40-1-x86_64.pkg.tar.zst" => Some(package("glibc", "2.40-1", "x86_64")),
                    _ => None,
                })
            },
        )
        .unwrap();

        assert_eq!(archives[0].package.architecture, "any");
        assert_eq!(
            archives[0].destination,
            destination.join("systemd-256.5-1-any.pkg.tar.zst")
        );
        assert_eq!(archives[1].package.name, "glibc");
    }

    #[test]
    fn missing_archive_reports_exact_required_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("cache");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("systemd-256.4-1-any.pkg.tar.zst"), "").unwrap();

        let error = resolve_arch_archives(
            &source,
            &directory.path().join("target-cache"),
            &[package("systemd", "256.5-1", "any")],
            |_| Ok(None),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("systemd 256.5-1 any"));
        assert!(error.contains("Required cached Arch archive is missing"));
    }

    #[test]
    fn current_target_package_is_skipped_by_exact_metadata() {
        let current = package("systemd", "256.5-1", "any");
        let packages = select_arch_packages_to_install(std::slice::from_ref(&current), |name| {
            assert_eq!(name, "systemd");
            Ok(Some(current.clone()))
        })
        .unwrap();

        assert!(packages.is_empty());
    }

    #[test]
    fn apt_upgrade_failure_retains_pending_and_previous_state() {
        let directory = tempfile::tempdir().unwrap();
        let pending = directory.path().join("apt-pending");
        let state = directory.path().join("apt-state");
        write_package_file(&pending, &["systemd".to_string()]).unwrap();
        fs::write(&state, "systemd=256.4-1").unwrap();
        let called = Cell::new(false);

        let result = finish_apt_post(
            &pending,
            &state,
            &["systemd".to_string()],
            "systemd=256.5-1",
            Some("systemd=256.4-1"),
            false,
            |triggers| {
                called.set(true);
                assert!(triggers.is_empty());
                bail!("simulated sync failure")
            },
        );

        assert!(result.is_err());
        assert!(called.get());
        assert_eq!(
            read_package_file(&pending).unwrap(),
            vec!["systemd".to_string()]
        );
        assert_eq!(fs::read_to_string(&state).unwrap(), "systemd=256.4-1");
    }

    #[test]
    fn apt_dry_run_does_not_clear_pending_or_write_state() {
        let directory = tempfile::tempdir().unwrap();
        let pending = directory.path().join("apt-pending");
        let state = directory.path().join("apt-state");
        write_package_file(&pending, &["systemd".to_string()]).unwrap();
        fs::write(&state, "systemd=256.4-1").unwrap();

        finish_apt_post(
            &pending,
            &state,
            &["systemd".to_string()],
            "systemd=256.4-1",
            Some("systemd=256.4-1"),
            true,
            |triggers| {
                assert_eq!(triggers, &["systemd".to_string()]);
                Ok(())
            },
        )
        .unwrap();

        assert!(pending.exists());
        assert_eq!(fs::read_to_string(&state).unwrap(), "systemd=256.4-1");
    }

    #[test]
    fn removed_root_is_absent_from_installed_package_state() {
        let state = installed_package_state_with(
            &["systemd".to_string(), "shared".to_string()],
            |package| Ok((package == "shared").then(|| "1.0-1".to_string())),
        )
        .unwrap();

        assert_eq!(state, "shared=1.0-1");
    }

    #[test]
    fn cleanup_failure_returns_error_without_replacing_manifest() {
        let directory = tempfile::tempdir().unwrap();
        let manifest = directory.path().join("manifest");
        write_manifest(&manifest, &["/etc/old.conf".to_string()]).unwrap();
        let stale_directory = directory.path().join("etc/old.conf");
        fs::create_dir_all(&stale_directory).unwrap();
        let result = remove_stale_files_with(
            directory.path().to_str().unwrap(),
            &["/etc/old.conf".to_string()],
            &[],
            |_| Ok(false),
        );

        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&manifest).unwrap(), "/etc/old.conf\n");
    }
}

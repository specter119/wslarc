use anyhow::{Context, Result};
use std::collections::HashSet;
use std::io::Read;

use crate::config::Config;
use crate::generators::ext4_sync;
use crate::utils::cli::{is_mountpoint, pacman_query_version};
use crate::utils::prompt::{info, success, warn};
use crate::utils::shell::run_or_dry;

pub fn run(config: &Config, dry_run: bool) -> Result<()> {
    let mount_point = &config.ext4_sync.mount_point;

    ensure_mounted(mount_point, dry_run)?;

    let triggered = read_triggered_packages();
    if !triggered.is_empty() {
        info(&format!("Triggered by: {}", triggered.join(", ")));
    }

    let packages = select_sync_packages(&triggered)?;
    let versions = get_package_versions(&packages)?;

    sync_cache(mount_point, &versions, dry_run)?;

    install_packages(mount_point, &versions, dry_run)?;

    success("ext4 systemd sync complete");
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

fn select_sync_packages(triggered: &[String]) -> Result<Vec<String>> {
    let hook_targets = ext4_sync::collect_hook_targets()?;
    Ok(filter_sync_packages(triggered, &hook_targets))
}

fn filter_sync_packages(triggered: &[String], hook_targets: &[String]) -> Vec<String> {
    if triggered.is_empty() {
        return hook_targets.to_vec();
    }

    let allowed: HashSet<&str> = hook_targets.iter().map(String::as_str).collect();
    let mut packages: Vec<String> = triggered
        .iter()
        .filter(|pkg| allowed.contains(pkg.as_str()))
        .cloned()
        .collect();
    packages.sort();
    packages.dedup();
    packages
}

fn get_package_versions(packages: &[String]) -> Result<Vec<(String, String)>> {
    let mut versions = Vec::new();
    for pkg in packages {
        if let Some(version) = pacman_query_version(pkg)? {
            versions.push((pkg.to_string(), version));
        } else {
            warn(&format!("Package {} not installed, skipping", pkg));
        }
    }
    Ok(versions)
}

fn sync_cache(mount_point: &str, versions: &[(String, String)], dry_run: bool) -> Result<()> {
    let dest_cache = format!("{}/var/cache/pacman/pkg", mount_point);

    if !dry_run {
        std::fs::create_dir_all(&dest_cache)?;
    }

    let arch = std::env::consts::ARCH;

    for (pkg, ver) in versions {
        let pkg_file = format!("{}-{}-{}.pkg.tar.zst", pkg, ver, arch);
        let src = format!("/var/cache/pacman/pkg/{}", pkg_file);
        let dst = format!("{}/{}", dest_cache, pkg_file);

        if dry_run {
            info(&format!("[dry-run] Would copy {} to {}", src, dst));
        } else {
            std::fs::copy(&src, &dst)
                .map_err(anyhow::Error::from)
                .with_context(|| format!("Failed to copy cached package {}", src))?;
            info(&format!("Copied {}", pkg_file));
        }
    }
    Ok(())
}

fn install_packages(mount_point: &str, versions: &[(String, String)], dry_run: bool) -> Result<()> {
    let arch = std::env::consts::ARCH;

    if versions.is_empty() {
        info("No packages to sync");
        return Ok(());
    }

    let pkg_paths: Vec<String> = versions
        .iter()
        .map(|(pkg, ver)| {
            format!(
                "{}/var/cache/pacman/pkg/{}-{}-{}.pkg.tar.zst",
                mount_point, pkg, ver, arch
            )
        })
        .collect();

    let mut args = vec!["--sysroot", mount_point, "-U", "--noconfirm"];
    for path in &pkg_paths {
        args.push(path);
    }

    run_or_dry("pacman", &args, dry_run)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_hook_targets() -> Vec<String> {
        vec![
            "systemd".to_string(),
            "libgcrypt".to_string(),
            "glibc".to_string(),
        ]
    }

    #[test]
    fn filter_sync_packages_uses_triggered_targets_only() {
        let packages = filter_sync_packages(
            &["libgcrypt".to_string(), "not-a-target".to_string()],
            &sample_hook_targets(),
        );

        assert_eq!(packages, vec!["libgcrypt".to_string()]);
    }

    #[test]
    fn filter_sync_packages_falls_back_to_all_targets_without_stdin() {
        let packages = filter_sync_packages(&[], &sample_hook_targets());

        assert!(packages.iter().any(|pkg| pkg == "systemd"));
        assert!(packages.iter().any(|pkg| pkg == "libgcrypt"));
    }

    #[test]
    fn filter_sync_packages_sorts_dedups_and_discards_unknowns() {
        let packages = filter_sync_packages(
            &[
                "libgcrypt".to_string(),
                "systemd".to_string(),
                "libgcrypt".to_string(),
                "unknown".to_string(),
                "glibc".to_string(),
            ],
            &sample_hook_targets(),
        );

        assert_eq!(
            packages,
            vec![
                "glibc".to_string(),
                "libgcrypt".to_string(),
                "systemd".to_string(),
            ]
        );
    }
}

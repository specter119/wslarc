use anyhow::{bail, Result};
use std::process::Command;

use crate::config::Config;
use crate::generators::ext4_sync;
use crate::utils::prompt::{info, success};
use crate::utils::shell::run_or_dry;

const PACKAGES: &[&str] = &["systemd", "systemd-libs", "systemd-sysvcompat"];

pub fn run(config: &Config, dry_run: bool) -> Result<()> {
    let mount_point = &config.ext4_sync.mount_point;

    ensure_mounted(mount_point, dry_run)?;

    let versions = get_package_versions()?;

    sync_cache(mount_point, &versions, dry_run)?;

    install_packages(mount_point, &versions, dry_run)?;

    success("ext4 systemd sync complete");
    Ok(())
}

fn ensure_mounted(mount_point: &str, dry_run: bool) -> Result<()> {
    let is_mounted = Command::new("mountpoint")
        .arg("-q")
        .arg(mount_point)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if is_mounted {
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

fn get_package_versions() -> Result<Vec<(String, String)>> {
    let mut versions = Vec::new();
    for pkg in PACKAGES {
        let output = Command::new("pacman").args(["-Q", pkg]).output()?;

        if !output.status.success() {
            bail!("Package {} not installed", pkg);
        }

        let line = String::from_utf8_lossy(&output.stdout);
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            versions.push((pkg.to_string(), parts[1].to_string()));
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
            std::fs::copy(&src, &dst)?;
            info(&format!("Copied {}", pkg_file));
        }
    }
    Ok(())
}

fn install_packages(mount_point: &str, versions: &[(String, String)], dry_run: bool) -> Result<()> {
    let arch = std::env::consts::ARCH;

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

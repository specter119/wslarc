//! Attach Btrfs VHDX if not already mounted
//!
//! This command is called by wsl.conf at boot time to ensure the Btrfs VHDX
//! is attached before systemd mount units try to mount it.

use anyhow::Result;
use std::process::Command;

use crate::config::Config;

/// Check if a Btrfs filesystem with the given label is available
fn is_btrfs_available(label: &str) -> bool {
    Command::new("lsblk")
        .args(["-f", "-n", "-o", "FSTYPE,LABEL"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|output| {
            output
                .lines()
                .any(|line| line.contains("btrfs") && line.contains(label))
        })
        .unwrap_or(false)
}

/// Ensure binfmt_misc is configured so wsl.exe can be executed
fn setup_binfmt() -> Result<()> {
    Command::new("/usr/lib/systemd/systemd-binfmt")
        .status()
        .map_err(|e| anyhow::anyhow!("Failed to run systemd-binfmt: {}", e))?;
    Ok(())
}

/// Attach the VHDX using wsl.exe
fn attach_vhdx(vhdx_path: &str) -> Result<()> {
    // Convert path: forward slashes to backslashes for Windows
    let windows_path = vhdx_path.replace('/', "\\");

    let status = Command::new("/mnt/c/Windows/System32/wsl.exe")
        .args(["--mount", "--vhd", &windows_path, "--bare"])
        .status()
        .map_err(|e| anyhow::anyhow!("Failed to run wsl.exe: {}", e))?;

    if !status.success() {
        anyhow::bail!("wsl.exe --mount failed with exit code: {:?}", status.code());
    }

    Ok(())
}

pub fn run(config: &Config) -> Result<()> {
    // Ensure binfmt_misc is configured so wsl.exe can be executed
    setup_binfmt()?;

    let label = &config.vhdx.label;
    let vhdx_path = &config.vhdx.path;

    // Check if Btrfs with this label is already available
    if is_btrfs_available(label) {
        // Already mounted, nothing to do
        return Ok(());
    }

    // Attach the VHDX
    attach_vhdx(vhdx_path)?;

    Ok(())
}

//! Attach Btrfs VHDX if not already mounted
//!
//! This command is called by wsl.conf at boot time to ensure the Btrfs VHDX
//! is attached before systemd mount units try to mount it.

use anyhow::{bail, Context, Result};
use std::process::Command;

use crate::config::Config;
use crate::utils::cli::find_btrfs_device_by_label;

/// Check if a Btrfs filesystem with the given label is available
fn is_btrfs_available(label: &str) -> bool {
    find_btrfs_device_by_label(label).unwrap_or(None).is_some()
}

/// Ensure binfmt_misc is configured so wsl.exe can be executed
fn setup_binfmt() -> Result<()> {
    run_command("/usr/lib/systemd/systemd-binfmt", &[])
}

/// Attach the VHDX using wsl.exe
fn attach_vhdx(vhdx_path: &str) -> Result<()> {
    // Convert path: forward slashes to backslashes for Windows
    let windows_path = vhdx_path.replace('/', "\\");

    run_command(
        "/mnt/c/Windows/System32/wsl.exe",
        &["--mount", "--vhd", &windows_path, "--bare"],
    )
    .context("wsl.exe --mount failed")
}

fn run_command(command: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(command)
        .args(args)
        .status()
        .with_context(|| format!("Failed to run {} {}", command, args.join(" ")))?;

    if !status.success() {
        bail!(
            "{} {} failed with exit code: {:?}",
            command,
            args.join(" "),
            status.code()
        );
    }

    Ok(())
}

fn repair_binfmt() -> Result<()> {
    repair_binfmt_with(run_command)
}

fn repair_binfmt_with<F>(mut run: F) -> Result<()>
where
    F: FnMut(&str, &[&str]) -> Result<()>,
{
    run(
        "sudo",
        &[
            "sh",
            "-c",
            "echo :WSLInterop:M::MZ::/init:PF > /usr/lib/binfmt.d/WSLInterop.conf",
        ],
    )?;
    run("sudo", &["systemctl", "unmask", "systemd-binfmt.service"])?;
    run("sudo", &["systemctl", "restart", "systemd-binfmt"])?;
    run("sudo", &["systemctl", "mask", "systemd-binfmt.service"])?;
    Ok(())
}

fn run_once(config: &Config) -> Result<()> {
    setup_binfmt()?;

    let label = &config.vhdx.label;
    let vhdx_path = &config.vhdx.path;

    // Check if Btrfs with this label is already available
    if is_btrfs_available(label) {
        // Already mounted, nothing to do
        return Ok(());
    }

    attach_vhdx(vhdx_path)
}

pub fn run(config: &Config) -> Result<()> {
    match run_once(config) {
        Ok(()) => Ok(()),
        Err(first_error) => {
            eprintln!(
                "warning: attach failed: {first_error:#}; repairing WSLInterop binfmt and retrying once"
            );
            repair_binfmt().with_context(|| {
                format!(
                    "Initial attach failed and binfmt repair could not complete: {first_error:#}"
                )
            })?;
            run_once(config).with_context(|| {
                format!("Attach retry failed after initial error: {first_error:#}")
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binfmt_repair_runs_commands_in_order() {
        let mut calls = Vec::new();
        repair_binfmt_with(|command, args| {
            calls.push((
                command.to_string(),
                args.iter()
                    .map(|arg| (*arg).to_string())
                    .collect::<Vec<_>>(),
            ));
            Ok(())
        })
        .unwrap();

        assert_eq!(
            calls,
            vec![
                (
                    "sudo".to_string(),
                    vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        "echo :WSLInterop:M::MZ::/init:PF > /usr/lib/binfmt.d/WSLInterop.conf"
                            .to_string(),
                    ],
                ),
                (
                    "sudo".to_string(),
                    vec![
                        "systemctl".to_string(),
                        "unmask".to_string(),
                        "systemd-binfmt.service".to_string(),
                    ],
                ),
                (
                    "sudo".to_string(),
                    vec![
                        "systemctl".to_string(),
                        "restart".to_string(),
                        "systemd-binfmt".to_string(),
                    ],
                ),
                (
                    "sudo".to_string(),
                    vec![
                        "systemctl".to_string(),
                        "mask".to_string(),
                        "systemd-binfmt.service".to_string(),
                    ],
                ),
            ]
        );
    }

    #[test]
    fn binfmt_repair_stops_after_first_failed_command() {
        let mut calls = Vec::new();
        let result = repair_binfmt_with(|command, args| {
            calls.push((command.to_string(), args.join(" ")));
            if calls.len() == 2 {
                bail!("injected failure");
            }
            Ok(())
        });

        assert!(result.is_err());
        assert_eq!(calls.len(), 2);
        assert!(calls[1].1.contains("unmask"));
    }
}

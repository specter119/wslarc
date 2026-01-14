use anyhow::Result;
use console::style;

use crate::config::Config;
use crate::generators::systemd;
use crate::utils::prompt::{confirm_or_yes, info, step, success};
use crate::utils::shell::run_or_dry;

pub fn run(config: &Config, yes: bool, dry_run: bool) -> Result<()> {
    println!("{}", style("WSL Btrfs Unmount").bold().cyan());

    println!();
    println!(
        "{}",
        style("This will disable all wslarc systemd mount units.").yellow()
    );
    println!("After restart, the Btrfs subvolumes will not be mounted.");
    println!();

    if !confirm_or_yes("Disable all mount units?", false, yes)? {
        println!("Aborted.");
        return Ok(());
    }

    let total_steps = 2;

    // Step 1: Disable mount units
    step(1, total_steps, "Disable systemd mount units");
    disable_mount_units(config, dry_run)?;

    // Step 2: Disable btrbk timer
    step(2, total_steps, "Disable btrbk timer");
    run_or_dry("systemctl", &["disable", "btrbk.timer"], dry_run)?;
    success("btrbk.timer disabled");

    // Done
    println!();
    println!("{}", style("Unmount setup complete!").green().bold());
    println!();
    println!("Restart WSL to apply: {}", style("wsl --shutdown").cyan());
    println!();
    println!("Note: The [boot] command in /etc/wsl.conf is still active.");
    println!(
        "To fully disable, edit {} and remove the command line.",
        style("/etc/wsl.conf").cyan()
    );

    Ok(())
}

fn disable_mount_units(config: &Config, dry_run: bool) -> Result<()> {
    // Disable base mount
    let base_unit = systemd::mount_unit_filename(&config.mount.base);
    run_or_dry("systemctl", &["disable", &base_unit], dry_run)?;
    info(&format!("{} disabled", base_unit));

    // Disable backup mounts
    for backup in config.subvolumes.backup.values() {
        let unit = systemd::mount_unit_filename(backup.mount());
        run_or_dry("systemctl", &["disable", &unit], dry_run)?;
        info(&format!("{} disabled", unit));
    }

    // Disable transfer mounts
    for transfer in config.subvolumes.transfer.values() {
        let unit = systemd::mount_unit_filename(&transfer.mount);
        run_or_dry("systemctl", &["disable", &unit], dry_run)?;
        info(&format!("{} disabled", unit));
    }

    success("All mount units disabled");
    Ok(())
}

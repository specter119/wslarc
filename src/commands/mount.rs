use anyhow::{bail, Result};
use console::style;
use ini::Ini;
use std::fs;
use std::path::Path;

use crate::config::Config;
use crate::generators::{btrbk, ext4_sync, systemd};
use crate::utils::prompt::{confirm_or_yes, info, step, success, warn};
use crate::utils::shell::run_or_dry;

const SYSTEMD_DIR: &str = "/etc/systemd/system";
const BTRBK_CONF: &str = "/etc/btrbk/btrbk.conf";
const WSLARC_BIN: &str = "/usr/local/bin/wslarc";
const WSL_CONF: &str = "/etc/wsl.conf";
const PACMAN_HOOK_PATH: &str = "/etc/pacman.d/hooks/sync-systemd-ext4.hook";

fn has_usr_subvol(config: &Config) -> bool {
    config.subvolumes.backup.contains_key("@usr")
}

pub fn run(config: &Config, yes: bool, dry_run: bool) -> Result<()> {
    println!("{}", style("WSL Btrfs Mount Setup").bold().cyan());

    if config.uuid.is_none() {
        bail!("UUID not set. Run 'wslarc init' first.");
    }

    let needs_ext4_sync = has_usr_subvol(config);

    show_summary(config, needs_ext4_sync);

    if !confirm_or_yes("Generate and install systemd units?", true, yes)? {
        println!("Aborted.");
        return Ok(());
    }

    let total_steps = if needs_ext4_sync { 6 } else { 5 };

    step(1, total_steps, "Install wslarc binary");
    install_binary(config, dry_run)?;

    step(2, total_steps, "Setup wsl.conf boot command");
    update_wsl_conf(dry_run)?;

    step(3, total_steps, "Generate systemd mount units");
    generate_systemd_units(config, dry_run)?;

    step(4, total_steps, "Generate btrbk configuration");
    generate_btrbk_config(config, dry_run)?;

    step(5, total_steps, "Enable systemd services");
    enable_services(config, dry_run)?;

    if needs_ext4_sync {
        step(6, total_steps, "Setup ext4 systemd sync");
        setup_ext4_sync(config, dry_run)?;
    }

    println!();
    println!("{}", style("Mount setup complete!").green().bold());
    println!();
    println!("Restart WSL to apply: {}", style("wsl --shutdown").cyan());

    Ok(())
}

fn show_summary(config: &Config, needs_ext4_sync: bool) {
    println!();
    println!("{}", style("Files to generate:").bold());

    println!("  {}", WSLARC_BIN);
    println!("  {} (update [boot] command)", WSL_CONF);

    let base_unit = systemd::mount_unit_filename(&config.mount.base);
    println!("  {}/{}", SYSTEMD_DIR, base_unit);

    for backup in config.subvolumes.backup.values() {
        let unit = systemd::mount_unit_filename(backup.mount());
        println!("  {}/{}", SYSTEMD_DIR, unit);
    }

    for transfer in config.subvolumes.transfer.values() {
        let unit = systemd::mount_unit_filename(&transfer.mount);
        println!("  {}/{}", SYSTEMD_DIR, unit);
    }

    println!("  {}", BTRBK_CONF);
    println!("  {}/btrbk.service", SYSTEMD_DIR);
    println!("  {}/btrbk.timer", SYSTEMD_DIR);

    if needs_ext4_sync {
        let ext4_unit = ext4_sync::ext4_mount_unit_filename(config);
        println!("  {}/{}", SYSTEMD_DIR, ext4_unit);
        println!("  {}", PACMAN_HOOK_PATH);
    }

    println!();
}

/// Install wslarc binary to /usr/local/bin (ext4 and @usr subvolume)
fn install_binary(config: &Config, dry_run: bool) -> Result<()> {
    let current_exe = std::env::current_exe()?;
    let current_path = current_exe.to_string_lossy();

    // Skip if already running from target location
    if current_path == WSLARC_BIN {
        success("wslarc already installed");
        return Ok(());
    }

    if dry_run {
        info(&format!(
            "[dry-run] Would copy {} to {}",
            current_exe.display(),
            WSLARC_BIN
        ));
        return Ok(());
    }

    // Create directory if needed
    fs::create_dir_all("/usr/local/bin")?;

    // Remove old binary first (can't overwrite running executable)
    let _ = fs::remove_file(WSLARC_BIN);

    // Copy binary to ext4
    fs::copy(&current_exe, WSLARC_BIN)?;
    run_or_dry("chmod", &["+x", WSLARC_BIN], false)?;

    // Also copy to @usr subvolume if mounted
    let btrfs_bin = format!("{}/@usr/local/bin/wslarc", config.mount.base);
    let btrfs_bin_dir = format!("{}/@usr/local/bin", config.mount.base);
    if Path::new(&format!("{}/@usr", config.mount.base)).exists() {
        fs::create_dir_all(&btrfs_bin_dir)?;
        let _ = fs::remove_file(&btrfs_bin);
        fs::copy(&current_exe, &btrfs_bin)?;
        run_or_dry("chmod", &["+x", &btrfs_bin], false)?;
    }

    success(&format!("wslarc installed to {}", WSLARC_BIN));
    Ok(())
}

const WSLARC_ATTACH_CMD: &str = "/usr/local/bin/wslarc attach";

fn update_wsl_conf(dry_run: bool) -> Result<()> {
    if dry_run {
        info(&format!(
            "[dry-run] Would update {} with [boot] command",
            WSL_CONF
        ));
        return Ok(());
    }

    let mut conf = Ini::load_from_file(WSL_CONF).unwrap_or_else(|_| Ini::new());

    if let Some(boot) = conf.section(Some("boot")) {
        if let Some(cmd) = boot.get("command") {
            if cmd == WSLARC_ATTACH_CMD {
                success("wsl.conf already configured");
                return Ok(());
            }
            warn(&format!("Overwriting existing [boot] command: {}", cmd));
        }
    }

    conf.with_section(Some("boot"))
        .set("command", WSLARC_ATTACH_CMD);

    conf.write_to_file(WSL_CONF)?;
    success("wsl.conf updated with boot command");
    Ok(())
}

fn generate_systemd_units(config: &Config, dry_run: bool) -> Result<()> {
    let mut units_to_verify = Vec::new();

    // Base mount
    let base_content = systemd::generate_base_mount(config);
    let base_unit = systemd::mount_unit_filename(&config.mount.base);
    write_systemd_unit(&base_unit, &base_content, dry_run)?;
    units_to_verify.push(format!("{}/{}", SYSTEMD_DIR, base_unit));
    success(&format!("{} created", base_unit));

    // Backup subvolumes (A-class)
    info("Creating A-class (backup) mount units...");
    for (subvol, backup) in &config.subvolumes.backup {
        let content =
            systemd::generate_subvol_mount(config, subvol, backup.mount(), backup.options());
        let unit = systemd::mount_unit_filename(backup.mount());
        write_systemd_unit(&unit, &content, dry_run)?;
        units_to_verify.push(format!("{}/{}", SYSTEMD_DIR, unit));
    }

    // Transfer subvolumes (C-class)
    info("Creating C-class (transfer) mount units...");
    for (subvol, transfer) in &config.subvolumes.transfer {
        let content = systemd::generate_subvol_mount(
            config,
            subvol,
            &transfer.mount,
            transfer.options.as_deref(),
        );
        let unit = systemd::mount_unit_filename(&transfer.mount);
        write_systemd_unit(&unit, &content, dry_run)?;
        units_to_verify.push(format!("{}/{}", SYSTEMD_DIR, unit));
    }

    // Verify all units with systemd-analyze
    if !dry_run {
        info("Validating systemd units...");
        let mut args = vec!["verify"];
        let unit_refs: Vec<&str> = units_to_verify.iter().map(|s| s.as_str()).collect();
        args.extend(unit_refs);
        run_or_dry("systemd-analyze", &args, false)?;
    }

    success("All mount units created and validated");
    Ok(())
}

fn generate_btrbk_config(config: &Config, dry_run: bool) -> Result<()> {
    // Create /etc/btrbk directory
    if !dry_run {
        fs::create_dir_all("/etc/btrbk")?;
    }

    // Generate btrbk.conf
    let conf_content = btrbk::generate_config(config);
    write_file(BTRBK_CONF, &conf_content, dry_run)?;

    // Validate btrbk config syntax
    if !dry_run {
        info("Validating btrbk.conf syntax...");
        run_or_dry("btrbk", &["-c", BTRBK_CONF, "dryrun"], false)?;
    }
    success("btrbk.conf created and validated");

    // Generate btrbk.service
    let service_content = btrbk::generate_service(config);
    write_systemd_unit("btrbk.service", &service_content, dry_run)?;
    success("btrbk.service created");

    // Generate btrbk.timer
    let timer_content = btrbk::generate_timer(&config.btrbk.timer_schedule);
    write_systemd_unit("btrbk.timer", &timer_content, dry_run)?;
    success("btrbk.timer created");

    Ok(())
}

fn enable_services(config: &Config, dry_run: bool) -> Result<()> {
    // Reload systemd
    run_or_dry("systemctl", &["daemon-reload"], dry_run)?;
    success("systemd daemon reloaded");

    // Enable base mount
    let base_unit = systemd::mount_unit_filename(&config.mount.base);
    run_or_dry("systemctl", &["enable", &base_unit], dry_run)?;

    // Enable backup mounts
    for backup in config.subvolumes.backup.values() {
        let unit = systemd::mount_unit_filename(backup.mount());
        run_or_dry("systemctl", &["enable", &unit], dry_run)?;
    }

    // Enable transfer mounts
    for transfer in config.subvolumes.transfer.values() {
        let unit = systemd::mount_unit_filename(&transfer.mount);
        run_or_dry("systemctl", &["enable", &unit], dry_run)?;
    }

    // Enable btrbk timer
    run_or_dry("systemctl", &["enable", "btrbk.timer"], dry_run)?;

    success("All services enabled");
    Ok(())
}

fn write_file(path: &str, content: &str, dry_run: bool) -> Result<()> {
    if dry_run {
        info(&format!("[dry-run] Would write {}", path));
        return Ok(());
    }

    // Create parent directory if needed
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(path, content)?;
    Ok(())
}

/// Write systemd unit file to ext4 /etc
fn write_systemd_unit(filename: &str, content: &str, dry_run: bool) -> Result<()> {
    let path = format!("{}/{}", SYSTEMD_DIR, filename);
    write_file(&path, content, dry_run)
}

fn setup_ext4_sync(config: &Config, dry_run: bool) -> Result<()> {
    let ext4_uuid = ext4_sync::get_ext4_root_uuid()
        .ok_or_else(|| anyhow::anyhow!("Could not get ext4 root UUID"))?;
    info(&format!("ext4 root UUID: {}", ext4_uuid));

    let mount_point = &config.ext4_sync.mount_point;
    if !dry_run {
        fs::create_dir_all(mount_point)?;
    }

    let mount_unit = ext4_sync::generate_ext4_mount(config, &ext4_uuid);
    let mount_unit_name = ext4_sync::ext4_mount_unit_filename(config);
    write_systemd_unit(&mount_unit_name, &mount_unit, dry_run)?;
    success(&format!("{} created", mount_unit_name));

    let hook = ext4_sync::generate_pacman_hook();
    write_file(PACMAN_HOOK_PATH, &hook, dry_run)?;
    success("pacman hook created");

    Ok(())
}

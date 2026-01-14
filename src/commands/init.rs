use anyhow::{bail, Context, Result};
use console::style;
use std::fs;
use std::path::Path;

use crate::config::Config;
use crate::utils::prompt::{self, confirm_or_yes, info, input, step, success, warn};
use crate::utils::shell::{run as shell_run, run_or_dry};

const CONFIG_PATH: &str = "/etc/wslarc/config.toml";

pub fn run(config: &Config, yes: bool, dry_run: bool) -> Result<()> {
    println!("{}", style("WSL Btrfs Initialization").bold().cyan());

    // Check if already initialized
    if Path::new(CONFIG_PATH).exists() && config.uuid.is_some() {
        warn("Configuration already exists with UUID. Re-running will overwrite.");
        if !confirm_or_yes("Continue anyway?", false, yes)? {
            return Ok(());
        }
    }

    // Collect configuration (interactive or from file)
    let mut cfg = if yes {
        config.clone()
    } else {
        collect_config(config)?
    };

    // Validate required fields
    if cfg.vhdx.path.is_empty() {
        bail!("VHDX path is required. Set it in config file or run without --yes for interactive mode.");
    }
    if cfg.user.name.is_empty() {
        bail!("User is required. Set it in config file or run without --yes for interactive mode.");
    }

    // Show summary
    show_summary(&cfg);

    // Confirm before proceeding
    if !confirm_or_yes("Proceed with initialization?", true, yes)? {
        println!("Aborted.");
        return Ok(());
    }

    let total_steps = 7;

    step(1, total_steps, "Ensure user exists");
    ensure_user(&cfg, dry_run)?;

    step(2, total_steps, "Mount VHDX to WSL");
    let device = mount_vhdx(&cfg, dry_run)?;
    info(&format!("Device: {}", device));

    step(3, total_steps, "Format as Btrfs");
    format_btrfs(&cfg, &device, dry_run, yes)?;

    step(4, total_steps, "Get filesystem UUID");
    let uuid = get_uuid(&device, dry_run)?;
    cfg.uuid = Some(uuid.clone());
    success(&format!("UUID: {}", uuid));

    step(5, total_steps, "Create subvolumes");
    create_subvolumes(&cfg, &device, dry_run)?;

    step(6, total_steps, "Save configuration");
    if !dry_run {
        cfg.save(CONFIG_PATH)?;
        success(&format!("Saved to {}", CONFIG_PATH));
    } else {
        info(&format!("[dry-run] Would save to {}", CONFIG_PATH));
    }

    step(7, total_steps, "Mount base volume");
    mount_base(&cfg, &device, dry_run)?;

    // Done
    println!();
    println!("{}", style("Initialization complete!").green().bold());
    println!();
    println!(
        "Next step: {} to set up systemd mounts",
        style("wslarc mount").cyan()
    );

    Ok(())
}

/// Interactive configuration collection
fn collect_config(base: &Config) -> Result<Config> {
    let mut cfg = base.clone();

    prompt::section("User Configuration");
    let username = input("Target Linux username", &cfg.user.name)?;

    // Set user and update paths
    cfg.set_user(&username);

    prompt::section("VHDX Configuration");
    cfg.vhdx.path = input("VHDX path (Windows, full path)", &cfg.vhdx.path)?;
    cfg.vhdx.label = input("Btrfs label", &cfg.vhdx.label)?;

    prompt::section("Mount Configuration");
    cfg.mount.base = input("Mount base", &cfg.mount.base)?;

    prompt::section("Subvolumes");
    println!("  Using default subvolume configuration:");
    println!("  A-class (backup): @usr, @opt, @home, @var_lib_pacman");
    println!("  Snapshot-only: @etc (not mounted, for btrbk backup)");
    println!("  B-class (exclude): .cache, .local, .npm, .bun, .vscode-server-insiders");
    println!("  C-class (transfer): @containers, @containers_user, @var_cache, @var_log, @var_tmp");

    Ok(cfg)
}

/// Show configuration summary
fn show_summary(cfg: &Config) {
    prompt::section("Configuration Summary");
    prompt::kv("VHDX", &cfg.vhdx.path);
    prompt::kv("Label", &cfg.vhdx.label);
    prompt::kv("Mount base", &cfg.mount.base);
    prompt::kv("User", &cfg.get_user());

    let backup_count = cfg.subvolumes.backup.len();
    let exclude_count = cfg.subvolumes.exclude.paths.len();
    let transfer_count = cfg.subvolumes.transfer.len();
    prompt::kv(
        "Subvolumes",
        &format!(
            "{} backup + {} exclude + {} transfer",
            backup_count, exclude_count, transfer_count
        ),
    );
    if !cfg.user.options.is_empty() {
        prompt::kv("User options", &cfg.user.options);
    }
}

/// Ensure target user exists, create if not
fn ensure_user(cfg: &Config, dry_run: bool) -> Result<()> {
    let user = cfg.get_user();

    // Check if user already exists
    let user_exists = shell_run("id", &[&user]).is_ok();

    if user_exists {
        success(&format!("User '{}' already exists", user));
        return Ok(());
    }

    // Create user with configured options
    info(&format!("Creating user '{}'...", user));

    // Parse options string into args
    let mut args: Vec<&str> = cfg.user.options.split_whitespace().collect();
    args.push(&user);

    run_or_dry("useradd", &args, dry_run)?;

    success(&format!("User '{}' created", user));
    Ok(())
}

/// Mount VHDX to WSL and return device path
fn mount_vhdx(cfg: &Config, dry_run: bool) -> Result<String> {
    if dry_run {
        info("[dry-run] Would mount VHDX");
        return Ok("<device>".to_string());
    }

    // Check if VHDX is already mounted (by label)
    let existing = shell_run("lsblk", &["-n", "-o", "NAME,LABEL"]).unwrap_or_default();
    for line in existing.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && parts[1] == cfg.vhdx.label {
            let device = format!("/dev/{}", parts[0]);
            success(&format!(
                "Already mounted as {} (label: {})",
                device, cfg.vhdx.label
            ));
            return Ok(device);
        }
    }

    // Get current block devices
    let before = shell_run("lsblk", &["-d", "-n", "-o", "NAME"])?;
    let before_devs: Vec<&str> = before.lines().collect();

    // Mount VHDX
    // Normalize path: wsl.exe accepts both / and \, but we standardize to \
    let vhdx_path = cfg.vhdx.path.replace('/', "\\");
    shell_run(
        "/mnt/c/Windows/System32/wsl.exe",
        &["--mount", "--vhd", &vhdx_path, "--bare"],
    )
    .context("Failed to mount VHDX. Make sure the VHDX exists and WSL interop is enabled.")?;

    // Find the new device
    std::thread::sleep(std::time::Duration::from_millis(500));
    let after = shell_run("lsblk", &["-d", "-n", "-o", "NAME"])?;

    let new_dev = after
        .lines()
        .find(|d| !before_devs.contains(d))
        .ok_or_else(|| anyhow::anyhow!("Could not find new device after mounting VHDX"))?;

    let device = format!("/dev/{}", new_dev);
    success(&format!("Mounted as {}", device));
    Ok(device)
}

/// Format device as Btrfs
fn format_btrfs(cfg: &Config, device: &str, dry_run: bool, yes: bool) -> Result<()> {
    if dry_run {
        info("[dry-run] Would format as Btrfs");
        return Ok(());
    }

    // Check if already formatted
    let fstype = shell_run("lsblk", &["-n", "-o", "FSTYPE", device]).unwrap_or_default();

    if fstype.trim() == "btrfs" {
        // Check label
        let current_label = shell_run("lsblk", &["-n", "-o", "LABEL", device]).unwrap_or_default();
        let current_label = current_label.trim();

        if current_label == cfg.vhdx.label {
            success(&format!(
                "Device already formatted as Btrfs with label '{}'",
                current_label
            ));
            return Ok(());
        }

        // Label mismatch - this is potentially dangerous
        if current_label.is_empty() {
            warn(&format!(
                "Device is Btrfs but has no label (expected '{}')",
                cfg.vhdx.label
            ));
        } else {
            warn(&format!(
                "Device is Btrfs with label '{}' (expected '{}')",
                current_label, cfg.vhdx.label
            ));
        }
        warn("This may be a different volume! Continuing could corrupt data.");

        if !confirm_or_yes("Continue with this device anyway?", false, yes)? {
            bail!("Aborted due to label mismatch");
        }
        return Ok(());
    }

    run_or_dry("mkfs.btrfs", &["-L", &cfg.vhdx.label, device], dry_run)?;
    success("Formatted as Btrfs");
    Ok(())
}

/// Get filesystem UUID
fn get_uuid(device: &str, dry_run: bool) -> Result<String> {
    if dry_run {
        return Ok("<uuid>".to_string());
    }

    let output = shell_run("blkid", &["-s", "UUID", "-o", "value", device])?;
    let uuid = output.trim().to_string();

    if uuid.is_empty() {
        bail!("Could not get UUID for {}", device);
    }

    Ok(uuid)
}

/// Create all subvolumes
fn create_subvolumes(cfg: &Config, device: &str, dry_run: bool) -> Result<()> {
    let mount_point = "/mnt/btrfs-setup";

    // Mount device
    if !dry_run {
        fs::create_dir_all(mount_point)?;
        shell_run("mount", &[device, mount_point])?;
    } else {
        info(&format!(
            "[dry-run] Would mount {} to {}",
            device, mount_point
        ));
    }

    // Create subvolumes
    let result = create_all_subvolumes(cfg, mount_point, dry_run);

    // Save config to @etc subvolume (before unmount!)
    if !dry_run && result.is_ok() {
        let subvol_config_dir = format!("{}/@etc/wslarc", mount_point);
        if Path::new(&format!("{}/@etc", mount_point)).exists() {
            fs::create_dir_all(&subvol_config_dir)?;
            let subvol_config = format!("{}/config.toml", subvol_config_dir);
            cfg.save(&subvol_config)?;
            info("  config.toml saved to @etc subvolume");
        }
    }

    // Unmount
    if !dry_run {
        shell_run("umount", &[mount_point])?;
        fs::remove_dir(mount_point)?;
    }

    result
}

fn create_all_subvolumes(cfg: &Config, mount_point: &str, dry_run: bool) -> Result<()> {
    // A-class: Backup targets
    info("Creating A-class (backup) subvolumes...");
    for subvol in cfg.subvolumes.backup.keys() {
        create_subvolume(mount_point, subvol, dry_run)?;
    }

    // @etc: snapshot-only (not in backup HashMap, but still created for btrbk)
    info("Creating @etc subvolume (snapshot-only)...");
    create_subvolume(mount_point, "@etc", dry_run)?;

    // Copy essential system directories if subvolumes are empty
    copy_if_empty(mount_point, "@etc", "/etc", dry_run)?;
    copy_if_empty(mount_point, "@usr", "/usr", dry_run)?;
    copy_if_empty(mount_point, "@opt", "/opt", dry_run)?;
    copy_if_empty(mount_point, "@var_lib_pacman", "/var/lib/pacman", dry_run)?;

    // B-class: Excluded paths (nested under parent)
    info("Creating B-class (exclude) nested subvolumes...");
    let parent = &cfg.subvolumes.exclude.parent;
    let user = cfg.get_user();
    for path in &cfg.subvolumes.exclude.paths {
        let nested = format!("{}/{}", parent, path);
        create_subvolume(mount_point, &nested, dry_run)?;
        // chown to target user (these are in user's home)
        let nested_path = format!("{}/{}", mount_point, nested);
        run_or_dry(
            "chown",
            &[&format!("{}:{}", user, user), &nested_path],
            dry_run,
        )?;
    }

    // Also chown @home itself to target user
    let home_path = format!("{}/{}", mount_point, parent);
    run_or_dry(
        "chown",
        &[&format!("{}:{}", user, user), &home_path],
        dry_run,
    )?;

    // C-class: Transfer subvolumes
    info("Creating C-class (transfer) subvolumes...");
    let mut nodatacow_dirs = Vec::new();
    for (subvol, transfer) in &cfg.subvolumes.transfer {
        create_subvolume(mount_point, subvol, dry_run)?;
        if transfer.nodatacow {
            nodatacow_dirs.push(format!("{}/{}", mount_point, subvol));
        }
        // chown subvolumes under user's home to target user
        if transfer.mount.contains(&format!("/home/{}", user)) {
            let subvol_path = format!("{}/{}", mount_point, subvol);
            run_or_dry(
                "chown",
                &["-R", &format!("{}:{}", user, user), &subvol_path],
                dry_run,
            )?;
        }
    }

    // Set nodatacow on transfer subvolumes
    if !nodatacow_dirs.is_empty() {
        info("Setting nodatacow attribute...");
        for dir in nodatacow_dirs {
            run_or_dry("chattr", &["+C", &dir], dry_run)?;
        }
    }

    // Create .snapshots directory
    info("Creating snapshot directory...");
    create_subvolume(mount_point, &cfg.btrbk.snapshot_dir, dry_run)?;

    success("All subvolumes created");
    Ok(())
}

fn create_subvolume(mount_point: &str, name: &str, dry_run: bool) -> Result<()> {
    let path = format!("{}/{}", mount_point, name);

    // Check if subvolume already exists
    if !dry_run && Path::new(&path).exists() {
        info(&format!("  {} (exists, skipped)", name));
        return Ok(());
    }

    run_or_dry("btrfs", &["subvolume", "create", &path], dry_run)?;
    info(&format!("  {} (created)", name));
    Ok(())
}

/// Copy source directory content to subvolume if the subvolume is empty
/// This is essential for @etc and @usr to prevent empty mount overlay
fn copy_if_empty(mount_point: &str, subvol: &str, source: &str, dry_run: bool) -> Result<()> {
    let target = format!("{}/{}", mount_point, subvol);

    if dry_run {
        info(&format!(
            "[dry-run] Would copy {} to {} if empty",
            source, target
        ));
        return Ok(());
    }

    // Check if target subvolume exists
    if !Path::new(&target).exists() {
        return Ok(()); // Subvolume doesn't exist, skip
    }

    // Check if target is empty (only has . and ..)
    let is_empty = fs::read_dir(&target)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false);

    if !is_empty {
        info(&format!("  {} already has content, skipping copy", subvol));
        return Ok(());
    }

    // Check if source exists and has content
    if !Path::new(source).exists() {
        warn(&format!("  {} does not exist, skipping copy", source));
        return Ok(());
    }

    info(&format!("Copying {} to {}...", source, subvol));
    warn("This may take a while for large directories like /usr");

    // Use rsync to preserve permissions, ACLs, and xattrs
    run_or_dry(
        "rsync",
        &[
            "-aAX",
            "--info=progress2",
            &format!("{}/", source),
            &format!("{}/", target),
        ],
        dry_run,
    )?;

    success(&format!("  {} copied to {}", source, subvol));
    Ok(())
}

/// Mount base Btrfs volume to config.mount.base
fn mount_base(cfg: &Config, device: &str, dry_run: bool) -> Result<()> {
    let mount_point = &cfg.mount.base;

    // Check if already mounted
    let mounts = shell_run("mount", &[]).unwrap_or_default();
    if mounts.contains(&format!(" {} ", mount_point))
        || mounts.contains(&format!(" {}\n", mount_point))
    {
        success(&format!("{} already mounted", mount_point));
        return Ok(());
    }

    // Create mount point
    if !dry_run {
        fs::create_dir_all(mount_point)?;
    }

    // Mount with configured options
    run_or_dry(
        "mount",
        &["-o", &cfg.mount.options, device, mount_point],
        dry_run,
    )?;

    success(&format!("Mounted {} to {}", device, mount_point));
    Ok(())
}

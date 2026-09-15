use anyhow::{bail, Context, Error, Result};
use console::style;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::config::{Config, Distribution};
use crate::utils::cli::{
    ensure_dependencies_for, find_btrfs_device_by_label, is_mountpoint, list_block_device_names,
    read_block_device, Dependency,
};
use crate::utils::prompt::{self, confirm_or_yes, info, input, step, success, warn};
use crate::utils::shell::{run as shell_run, run_or_dry};
use crate::utils::storage::atomic_write;

pub fn run(
    config: &Config,
    distribution: Distribution,
    config_path: &str,
    yes: bool,
    dry_run: bool,
) -> Result<()> {
    println!("{}", style("WSL Btrfs Initialization").bold().cyan());

    // Check if already initialized
    if Path::new(config_path).exists() && config.uuid.is_some() {
        warn("Configuration already exists with UUID. Re-running will overwrite.");
        if !confirm_or_yes("Continue anyway?", false, yes)? {
            return Ok(());
        }
    }

    // Collect configuration (interactive or from file)
    let mut cfg = if yes {
        config.clone()
    } else {
        collect_config(config, distribution)?
    };

    // Validate required fields
    if cfg.vhdx.path.is_empty() {
        bail!("VHDX path is required. Set it in config file or run without --yes for interactive mode.");
    }
    if cfg.user.name.is_empty() {
        bail!("User is required. Set it in config file or run without --yes for interactive mode.");
    }

    check_runtime_dependencies(&cfg, distribution)?;

    // Show summary
    show_summary(&cfg, distribution);

    // Confirm before proceeding
    if !confirm_or_yes("Proceed with initialization?", true, yes)? {
        println!("Aborted.");
        return Ok(());
    }

    // Keep the configured paths intact for persistence, while using a
    // user-specific copy for filesystem operations.
    let mut runtime_cfg = cfg.resolve_variables();

    let total_steps = 7;

    step(1, total_steps, "Ensure user exists");
    ensure_user(&cfg, dry_run)?;

    step(2, total_steps, "Mount VHDX to WSL");
    let device = mount_vhdx(&cfg, dry_run)?;
    info(&format!("Device: {}", device));

    step(3, total_steps, "Format as Btrfs");
    format_btrfs(&mut runtime_cfg, &device, dry_run, yes)?;

    step(4, total_steps, "Get filesystem UUID");
    let uuid = get_uuid(&device, dry_run)?;
    runtime_cfg.uuid = Some(uuid.clone());
    cfg.uuid = Some(uuid.clone());
    cfg.vhdx.label = runtime_cfg.vhdx.label.clone();
    success(&format!("UUID: {}", uuid));

    step(5, total_steps, "Create subvolumes");
    create_subvolumes(&cfg, &runtime_cfg, &device, dry_run)?;

    step(6, total_steps, "Save configuration");
    if !dry_run {
        cfg.save(config_path)?;
        success(&format!("Saved to {}", config_path));
    } else {
        info(&format!("[dry-run] Would save to {}", config_path));
    }

    step(7, total_steps, "Mount base volume");
    mount_base(&runtime_cfg, &device, dry_run)?;

    // Done
    println!();
    println!("{}", style("Initialization complete!").green().bold());
    println!();
    println!(
        "Next step: {} to set up systemd mounts",
        style(format!(
            "wslarc --config {} mount",
            crate::generators::invocation::shell_argument(config_path)
        ))
        .cyan()
    );

    Ok(())
}

fn check_runtime_dependencies(config: &Config, distribution: Distribution) -> Result<()> {
    let mut dependencies = vec![
        Dependency::new("btrfs-progs", &["mkfs.btrfs", "btrfs"]),
        Dependency::new("rsync", &["rsync"]),
    ];

    if config
        .subvolumes
        .transfer
        .values()
        .any(|transfer| transfer.nodatacow)
    {
        dependencies.push(Dependency::new("e2fsprogs", &["chattr"]));
    }

    ensure_dependencies_for(&dependencies, distribution)
}

/// Interactive configuration collection
fn collect_config(base: &Config, distribution: Distribution) -> Result<Config> {
    let mut cfg = base.clone();

    prompt::section("Distribution");
    println!("  Detected distribution: {}", distribution.display_name());

    prompt::section("User Configuration");
    let username = input("Target Linux username", &cfg.user.name)?;

    // Set user and update paths
    cfg.set_user_unexpanded(&username);

    prompt::section("VHDX Configuration");
    cfg.vhdx.path = input("VHDX path (Windows, full path)", &cfg.vhdx.path)?;
    cfg.vhdx.label = input("Btrfs label", &cfg.vhdx.label)?;

    prompt::section("Mount Configuration");
    cfg.mount.base = input("Mount base", &cfg.mount.base)?;

    prompt::section("Subvolumes");
    println!("  Using default subvolume configuration:");
    println!(
        "  {} template includes package database backup entries",
        distribution.display_name()
    );
    println!("  Backup, exclude, transfer, and snapshot-only entries are editable");
    println!("  B-class (exclude): .cache, .local, .npm, .bun, .vscode-server-insiders");
    println!("  C-class (transfer): @containers, @var_cache, @var_log, @var_tmp");

    Ok(cfg)
}

/// Show configuration summary
fn show_summary(cfg: &Config, distribution: Distribution) {
    let runtime_cfg = cfg.resolve_variables();
    prompt::section("Configuration Summary");
    prompt::kv("VHDX", &runtime_cfg.vhdx.path);
    prompt::kv("Label", &runtime_cfg.vhdx.label);
    prompt::kv("Mount base", &runtime_cfg.mount.base);
    prompt::kv("User", &runtime_cfg.get_user());
    prompt::kv("Distribution", distribution.display_name());
    prompt::kv(
        "Subvolumes",
        &format!(
            "{} backup + {} exclude + {} transfer + {} snapshot-only",
            cfg.subvolumes.backup.len(),
            cfg.subvolumes.exclude.paths.len(),
            cfg.subvolumes.transfer.len(),
            cfg.subvolumes.snapshot_only.len()
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
    if let Some(device) = find_btrfs_device_by_label(&cfg.vhdx.label)? {
        success(&format!(
            "Already mounted as {} (label: {})",
            device, cfg.vhdx.label
        ));
        return Ok(device);
    }

    // Get current block devices
    let before_devs = list_block_device_names()?;

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
    let after_devs = list_block_device_names()?;

    let new_dev = after_devs
        .iter()
        .find(|device| !before_devs.contains(device))
        .ok_or_else(|| anyhow::anyhow!("Could not find new device after mounting VHDX"))?;

    let device = format!("/dev/{}", new_dev);
    success(&format!("Mounted as {}", device));
    Ok(device)
}

/// Format device as Btrfs
fn format_btrfs(cfg: &mut Config, device: &str, dry_run: bool, yes: bool) -> Result<()> {
    if dry_run {
        info("[dry-run] Would format as Btrfs");
        return Ok(());
    }

    // Check if already formatted
    let block_device = read_block_device(device)?.unwrap_or(crate::utils::cli::BlockDevice {
        name: device.trim_start_matches("/dev/").to_string(),
        label: None,
        fstype: None,
    });

    if block_device.fstype.as_deref() == Some("btrfs") {
        // Check label
        let current_label = block_device.label.as_deref().unwrap_or("");

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
        if !current_label.is_empty() && current_label != cfg.vhdx.label {
            warn(&format!(
                "Using existing label '{}' and updating config",
                current_label
            ));
            cfg.vhdx.label = current_label.to_string();
        } else if current_label.is_empty() {
            warn("Device label is empty; attach by label may fail until you set a label.");
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
trait InitCommandRunner {
    fn run(&mut self, command: &str, args: &[&str]) -> Result<String>;
}

struct SystemInitCommandRunner;

impl InitCommandRunner for SystemInitCommandRunner {
    fn run(&mut self, command: &str, args: &[&str]) -> Result<String> {
        shell_run(command, args)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct InitProgress {
    #[serde(default)]
    seeds: HashMap<String, SeedProgress>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SeedProgress {
    source: String,
    target: String,
    status: SeedStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum SeedStatus {
    Incomplete,
    Complete,
}

const INIT_PROGRESS_FILE: &str = ".wslarc-init-progress.toml";

impl InitProgress {
    fn load(mount_point: &str) -> Result<Self> {
        let path = Path::new(mount_point).join(INIT_PROGRESS_FILE);
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(&path).with_context(|| {
            format!("Failed to read initialization progress: {}", path.display())
        })?;
        toml::from_str(&content).with_context(|| {
            format!(
                "Failed to parse initialization progress: {}",
                path.display()
            )
        })
    }

    fn save(&self, mount_point: &str) -> Result<()> {
        let path = Path::new(mount_point).join(INIT_PROGRESS_FILE);
        let content =
            toml::to_string_pretty(self).context("Failed to serialize initialization progress")?;
        atomic_write(&path, content.as_bytes()).with_context(|| {
            format!(
                "Failed to write initialization progress: {}",
                path.display()
            )
        })
    }
}

fn run_init_command(
    runner: &mut dyn InitCommandRunner,
    command: &str,
    args: &[&str],
    dry_run: bool,
) -> Result<String> {
    if dry_run {
        run_or_dry(command, args, dry_run)
    } else {
        runner.run(command, args)
    }
}

fn prepare_seed(
    mount_point: &str,
    subvol: &str,
    source: &str,
    progress: &mut InitProgress,
) -> Result<()> {
    let target = format!("{}/{}", mount_point, subvol);
    let target_is_empty = Path::new(&target).exists()
        && fs::read_dir(&target)
            .with_context(|| format!("Failed to inspect seed target {}", target))?
            .next()
            .is_none();
    let matching_state = progress
        .seeds
        .get(subvol)
        .filter(|seed| seed.source == source && seed.target == target)
        .map(|seed| seed.status);

    if target_is_empty && matching_state != Some(SeedStatus::Incomplete) {
        progress.seeds.insert(
            subvol.to_string(),
            SeedProgress {
                source: source.to_string(),
                target,
                status: SeedStatus::Incomplete,
            },
        );
        progress.save(mount_point)?;
    }

    Ok(())
}

fn cleanup_setup_mount<F, G>(
    mount_point: &str,
    result: Result<()>,
    unmount: F,
    remove_dir: G,
) -> Result<()>
where
    F: FnOnce(&str) -> Result<String>,
    G: FnOnce(&str) -> std::io::Result<()>,
{
    let mut cleanup_errors = Vec::new();
    match unmount(mount_point) {
        Ok(_) => {
            if let Err(error) = remove_dir(mount_point) {
                cleanup_errors.push(
                    Error::from(error)
                        .context(format!("Failed to remove mount point {}", mount_point)),
                );
            }
        }
        Err(error) => cleanup_errors.push(error.context(format!(
            "Failed to unmount initialization mount point {}",
            mount_point
        ))),
    }

    if cleanup_errors.is_empty() {
        return result;
    }

    let details = cleanup_errors
        .iter()
        .map(|error| format!("{error:#}"))
        .collect::<Vec<_>>()
        .join("; ");
    match result {
        Ok(()) => Err(anyhow::anyhow!("Initialization cleanup failed: {details}")),
        Err(error) => Err(error.context(format!("Initialization cleanup also failed: {details}"))),
    }
}

fn create_subvolumes(
    config: &Config,
    runtime_config: &Config,
    device: &str,
    dry_run: bool,
) -> Result<()> {
    let mount_point = "/mnt/btrfs-setup";

    // Mount device
    if !dry_run {
        fs::create_dir_all(mount_point)?;
        shell_run("mount", &["-o", "subvolid=5", device, mount_point])?;
    } else {
        info(&format!(
            "[dry-run] Would mount {} to {} (subvolid=5)",
            device, mount_point
        ));
    }

    let mut runner = SystemInitCommandRunner;
    let mut result =
        create_all_subvolumes_with_runner(runtime_config, mount_point, dry_run, &mut runner);

    // Save config alongside the snapshot-only /etc source before umounting.
    if !dry_run && result.is_ok() {
        if let Some(subvol) = runtime_config
            .subvolumes
            .snapshot_only
            .iter()
            .find_map(|(subvol, snapshot)| (snapshot.source == "/etc").then_some(subvol))
        {
            let subvol_path = format!("{}/{}", mount_point, subvol);
            if Path::new(&subvol_path).exists() {
                let subvol_config_dir = format!("{}/wslarc", subvol_path);
                if let Err(error) = fs::create_dir_all(&subvol_config_dir) {
                    result = Err(error.into());
                }
                let subvol_config = format!("{}/config.toml", subvol_config_dir);
                if result.is_ok() {
                    if let Err(error) = config.save(&subvol_config) {
                        result = Err(error);
                    } else {
                        info(&format!("  config.toml saved to {} subvolume", subvol));
                    }
                }
            }
        }
    }

    // Umount even when creation or config persistence failed. Never remove
    // the mount directory unless umount succeeded.
    if !dry_run {
        result = cleanup_setup_mount(
            mount_point,
            result,
            |path| shell_run("umount", &[path]),
            |path: &str| fs::remove_dir(path),
        );
    }

    result
}

fn create_all_subvolumes_with_runner(
    cfg: &Config,
    mount_point: &str,
    dry_run: bool,
    runner: &mut dyn InitCommandRunner,
) -> Result<()> {
    let mut progress = if dry_run {
        InitProgress::default()
    } else {
        InitProgress::load(mount_point)?
    };

    info("Creating configured A-class backup subvolumes...");
    for subvol in cfg.subvolumes.backup.keys() {
        create_subvolume_with_runner(mount_point, subvol, dry_run, runner)?;
    }

    info("Creating configured snapshot-only subvolumes...");
    for subvol in cfg.subvolumes.snapshot_only.keys() {
        create_subvolume_with_runner(mount_point, subvol, dry_run, runner)?;
    }

    // Establish nested B-class subvolumes before seeding @home. Rsync then
    // traverses the already-created nested targets and preserves their source
    // contents instead of copying a populated home over their mount points.
    info("Creating configured B-class nested subvolumes...");
    let parent = &cfg.subvolumes.exclude.parent;
    let user = cfg.get_user();
    let nested_sources: Vec<(String, String)> = cfg
        .subvolumes
        .backup
        .get(parent)
        .map(|backup| {
            cfg.subvolumes
                .exclude
                .paths
                .iter()
                .map(|path| {
                    let nested = format!("{}/{}", parent, path);
                    let source = format!(
                        "{}/{}",
                        backup.mount().trim_end_matches('/'),
                        path.trim_start_matches('/')
                    );
                    (nested, source)
                })
                .collect()
        })
        .unwrap_or_default();
    create_subvolume_with_runner(mount_point, parent, dry_run, runner)?;
    if let Some(backup) = cfg.subvolumes.backup.get(parent).filter(|_| !dry_run) {
        prepare_seed(mount_point, parent, backup.mount(), &mut progress)?;
    }
    for path in &cfg.subvolumes.exclude.paths {
        let nested = format!("{}/{}", parent, path);
        create_subvolume_with_runner(mount_point, &nested, dry_run, runner)?;
        // chown to target user (these are in user's home)
        let nested_path = format!("{}/{}", mount_point, nested);
        run_init_command(
            runner,
            "chown",
            &[&format!("{}:{}", user, user), &nested_path],
            dry_run,
        )?;
    }

    // Also chown @home itself to target user
    let home_path = format!("{}/{}", mount_point, parent);
    run_init_command(
        runner,
        "chown",
        &[&format!("{}:{}", user, user), &home_path],
        dry_run,
    )?;

    // C-class: Configured transfer subvolumes
    info("Creating configured C-class transfer subvolumes...");
    let mut nodatacow_dirs = Vec::new();
    for (subvol, transfer) in &cfg.subvolumes.transfer {
        create_subvolume_with_runner(mount_point, subvol, dry_run, runner)?;
        if transfer.nodatacow {
            nodatacow_dirs.push(format!("{}/{}", mount_point, subvol));
        }
        // chown subvolumes under user's home to target user
        if transfer.mount.starts_with(&format!("/home/{}/", user))
            || transfer.mount == format!("/home/{}", user)
        {
            let subvol_path = format!("{}/{}", mount_point, subvol);
            run_init_command(
                runner,
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
            run_init_command(runner, "chattr", &["+C", &dir], dry_run)?;
        }
    }

    // Seed after all nested and transfer subvolumes exist, and after C-class
    // nodatacow is set. Sources are never removed or cleaned. Pre-register
    // fresh targets before nested content can make their parents non-empty;
    // an untracked populated target is never treated as resumable.
    let mut seed_sources = nested_sources.clone();
    seed_sources.extend(
        cfg.subvolumes
            .backup
            .iter()
            .map(|(subvol, backup)| (subvol.clone(), backup.mount().to_string())),
    );
    seed_sources.extend(
        cfg.subvolumes
            .snapshot_only
            .iter()
            .map(|(subvol, snapshot)| (subvol.clone(), snapshot.source.clone())),
    );
    seed_sources.extend(
        cfg.subvolumes
            .transfer
            .iter()
            .map(|(subvol, transfer)| (subvol.clone(), transfer.mount.clone())),
    );
    if !dry_run {
        for (subvol, source) in &seed_sources {
            prepare_seed(mount_point, subvol, source, &mut progress)?;
        }
    }

    for (subvol, source) in &seed_sources {
        let excluded_paths: &[String] = if subvol == parent {
            &cfg.subvolumes.exclude.paths
        } else {
            &[]
        };
        seed_subvolume_with_excludes(
            mount_point,
            subvol,
            source,
            excluded_paths,
            dry_run,
            &mut progress,
            runner,
        )?;
    }

    // Create the btrbk snapshot directory
    info("Creating snapshot directory...");
    create_subvolume_with_runner(mount_point, &cfg.btrbk.snapshot_dir, dry_run, runner)?;

    success("All subvolumes created");
    Ok(())
}

fn create_subvolume_with_runner(
    mount_point: &str,
    name: &str,
    dry_run: bool,
    runner: &mut dyn InitCommandRunner,
) -> Result<()> {
    let path = format!("{}/{}", mount_point, name);

    if !dry_run && fs::symlink_metadata(&path).is_ok() {
        if runner.run("btrfs", &["subvolume", "show", &path]).is_ok() {
            info(&format!("  {} (pre-existing subvolume, skipped)", name));
            return Ok(());
        }

        bail!(
            "Refusing to use existing path '{}' for subvolume '{}': it is not a confirmed Btrfs subvolume",
            path,
            name
        );
    }

    run_init_command(runner, "btrfs", &["subvolume", "create", &path], dry_run)?;
    info(&format!("  {} (created)", name));
    Ok(())
}

/// Seed a subvolume without deleting or cleaning an existing source.
///
/// A populated target is copied into only when this initialization previously
/// recorded an incomplete seed. Otherwise it is treated as pre-existing state
/// and skipped safely. This prevents a retry from mistaking arbitrary content
/// for a completed copy while also preventing destructive overwrite behavior.
#[cfg(test)]
fn seed_subvolume(
    mount_point: &str,
    subvol: &str,
    source: &str,
    dry_run: bool,
    progress: &mut InitProgress,
    runner: &mut dyn InitCommandRunner,
) -> Result<()> {
    seed_subvolume_with_excludes(mount_point, subvol, source, &[], dry_run, progress, runner)
}

fn seed_subvolume_with_excludes(
    mount_point: &str,
    subvol: &str,
    source: &str,
    excluded_paths: &[String],
    dry_run: bool,
    progress: &mut InitProgress,
    runner: &mut dyn InitCommandRunner,
) -> Result<()> {
    let target = format!("{}/{}", mount_point, subvol);

    if dry_run {
        info(&format!(
            "[dry-run] Would copy {} to {} with resumable progress",
            source, target
        ));
        return Ok(());
    }

    // Check if target subvolume exists
    if !Path::new(&target).exists() {
        return Ok(()); // Subvolume doesn't exist, skip
    }

    // Check if source exists and has content
    if !Path::new(source).exists() {
        warn(&format!("  {} does not exist, skipping copy", source));
        return Ok(());
    }

    let target_is_empty = fs::read_dir(&target)
        .with_context(|| format!("Failed to inspect seed target {}", target))?
        .next()
        .is_none();
    let tracked = progress
        .seeds
        .get(subvol)
        .filter(|seed| seed.source == source && seed.target == target)
        .cloned();
    match (target_is_empty, tracked.as_ref().map(|seed| seed.status)) {
        (false, Some(SeedStatus::Incomplete)) => {
            info(&format!(
                "  {} has an incomplete tracked seed, retrying",
                subvol
            ));
        }
        (false, _) => {
            info(&format!(
                "  {} already has untracked or completed content, skipping copy",
                subvol
            ));
            return Ok(());
        }
        (true, Some(SeedStatus::Complete)) => {
            info(&format!("  {} was already seeded, skipping copy", subvol));
            return Ok(());
        }
        (true, Some(SeedStatus::Incomplete)) => {}
        (true, None) => {
            info(&format!(
                "  {} is empty but has no tracked initialization seed, skipping copy",
                subvol
            ));
            return Ok(());
        }
    }

    info(&format!("Copying {} to {}...", source, subvol));
    warn("This may take a while for large directories like /usr");

    // Use rsync to preserve permissions, ACLs, and xattrs
    let mut rsync_args = vec!["-aAX".to_string(), "--info=progress2".to_string()];
    for path in excluded_paths {
        rsync_args.push("--exclude".to_string());
        rsync_args.push(format!("/{}/", path.trim_matches('/')));
    }
    rsync_args.push(format!("{}/", source));
    rsync_args.push(format!("{}/", target));
    let rsync_args: Vec<&str> = rsync_args.iter().map(String::as_str).collect();
    run_init_command(runner, "rsync", &rsync_args, dry_run)?;

    if let Some(seed) = progress.seeds.get_mut(subvol) {
        seed.status = SeedStatus::Complete;
    }
    progress.save(mount_point)?;

    success(&format!("  {} copied to {}", source, subvol));
    Ok(())
}

/// Mount base Btrfs volume to config.mount.base
fn mount_base(cfg: &Config, device: &str, dry_run: bool) -> Result<()> {
    let mount_point = &cfg.mount.base;

    // Check if already mounted
    if is_mountpoint(mount_point) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BackupSubvol, TransferSubvol};
    use std::cell::Cell;
    use std::collections::HashSet;
    use std::fs::{self, File};
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    #[test]
    fn init_dry_run_does_not_create_progress_for_existing_empty_targets() {
        let temp = tempdir().unwrap();
        let mount = temp.path().join("mount");
        let source = temp.path().join("source");
        fs::create_dir_all(mount.join("@home")).unwrap();
        fs::create_dir_all(&source).unwrap();
        let config = test_config(&source, &temp.path().join("transfer-source"));
        let mut runner = FakeInitCommandRunner::default();
        create_all_subvolumes_with_runner(&config, mount.to_str().unwrap(), true, &mut runner)
            .unwrap();
        assert!(!mount.join(INIT_PROGRESS_FILE).exists());
        assert!(runner.calls.is_empty());
        assert_eq!(fs::read_dir(&mount).unwrap().count(), 1);
    }

    #[test]
    fn init_save_failure_still_unmounts_and_removes_empty_setup_directory() {
        let temp = tempdir().unwrap();
        let mount = temp.path().join("setup");
        fs::create_dir(&mount).unwrap();
        let unmounted = Cell::new(false);
        let result = cleanup_setup_mount(
            mount.to_str().unwrap(),
            Err(anyhow::anyhow!("configuration save failed")),
            |_| {
                unmounted.set(true);
                Ok(String::new())
            },
            |path| fs::remove_dir(path),
        );
        assert!(unmounted.get());
        assert!(!mount.exists());
        assert!(format!("{:#}", result.unwrap_err()).contains("configuration save failed"));
    }

    #[derive(Default)]
    struct FakeInitCommandRunner {
        subvolumes: HashSet<String>,
        calls: Vec<String>,
        fail_rsync_once: bool,
    }

    impl InitCommandRunner for FakeInitCommandRunner {
        fn run(&mut self, command: &str, args: &[&str]) -> Result<String> {
            self.calls.push(format!("{} {}", command, args.join(" ")));

            match (command, args) {
                ("btrfs", ["subvolume", "show", path]) => {
                    if self.subvolumes.contains(*path) {
                        Ok(String::new())
                    } else {
                        bail!("not a subvolume")
                    }
                }
                ("btrfs", ["subvolume", "create", path]) => {
                    fs::create_dir_all(path)?;
                    self.subvolumes.insert((*path).to_string());
                    Ok(String::new())
                }
                ("rsync", args) if args.len() >= 4 => {
                    if self.fail_rsync_once {
                        self.fail_rsync_once = false;
                        bail!("injected rsync failure")
                    }
                    let source = args[args.len() - 2];
                    let target = args[args.len() - 1];
                    copy_tree(Path::new(source), Path::new(target))?;
                    Ok(String::new())
                }
                ("chown", _) | ("chattr", _) => Ok(String::new()),
                _ => bail!("unexpected command: {command}"),
            }
        }
    }

    fn copy_tree(source: &Path, target: &Path) -> Result<()> {
        fs::create_dir_all(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let source_path = entry.path();
            let target_path = target.join(entry.file_name());
            if source_path.is_dir() {
                copy_tree(&source_path, &target_path)?;
            } else {
                fs::copy(source_path, target_path)?;
            }
        }
        Ok(())
    }

    fn touch(path: impl Into<PathBuf>) {
        File::create(path.into()).unwrap();
    }

    fn test_config(home_source: &Path, transfer_source: &Path) -> Config {
        let mut config = Config::for_distribution(crate::config::Distribution::Arch);
        config.user.name = "alice".to_string();
        config.subvolumes.backup.clear();
        config.subvolumes.backup.insert(
            "@home".to_string(),
            BackupSubvol::Simple(home_source.display().to_string()),
        );
        config.subvolumes.snapshot_only.clear();
        config.subvolumes.exclude.parent = "@home".to_string();
        config.subvolumes.exclude.paths = vec![".local".to_string()];
        config.subvolumes.transfer.clear();
        config.subvolumes.transfer.insert(
            "@transfer".to_string(),
            TransferSubvol {
                mount: transfer_source.display().to_string(),
                nodatacow: true,
                options: None,
            },
        );
        config
    }

    #[test]
    fn initialization_seeds_nested_home_and_transfer_sources() {
        let temp = tempdir().unwrap();
        let source_home = temp.path().join("source-home");
        let source_local = source_home.join(".local");
        let source_transfer = temp.path().join("source-transfer");
        fs::create_dir_all(&source_local).unwrap();
        fs::create_dir_all(&source_transfer).unwrap();
        touch(source_home.join("profile"));
        touch(source_local.join("state"));
        touch(source_transfer.join("container-layer"));

        let mount = temp.path().join("mount");
        fs::create_dir_all(&mount).unwrap();
        let config = test_config(&source_home, &source_transfer);
        let mut runner = FakeInitCommandRunner::default();

        create_all_subvolumes_with_runner(&config, mount.to_str().unwrap(), false, &mut runner)
            .unwrap();

        assert!(mount.join("@home/profile").exists());
        assert!(mount.join("@home/.local/state").exists());
        assert!(mount.join("@transfer/container-layer").exists());
        assert!(source_home.join(".local/state").exists());
        assert!(source_transfer.join("container-layer").exists());
        assert!(runner
            .subvolumes
            .contains(mount.join("@home/.local").to_str().unwrap()));

        let chattr_index = runner
            .calls
            .iter()
            .position(|call| call.starts_with("chattr "))
            .unwrap();
        let transfer_rsync_index = runner
            .calls
            .iter()
            .position(|call| call.contains("rsync") && call.contains("@transfer/"))
            .unwrap();
        assert!(chattr_index < transfer_rsync_index);
        assert!(runner
            .calls
            .iter()
            .any(|call| call.contains("rsync") && call.contains("--exclude /.local/")));
    }

    #[test]
    fn preexisting_nested_home_subvolume_is_seeded_from_its_source() {
        let temp = tempdir().unwrap();
        let source_home = temp.path().join("source-home");
        let source_local = source_home.join(".local");
        let mount = temp.path().join("mount");
        let target_home = mount.join("@home");
        let target_local = target_home.join(".local");
        fs::create_dir_all(&source_local).unwrap();
        fs::create_dir_all(&target_local).unwrap();
        touch(source_local.join("state"));

        let config = test_config(&source_home, &temp.path().join("unused-transfer"));
        let mut runner = FakeInitCommandRunner::default();
        runner
            .subvolumes
            .insert(target_home.to_string_lossy().into_owned());
        runner
            .subvolumes
            .insert(target_local.to_string_lossy().into_owned());

        create_all_subvolumes_with_runner(&config, mount.to_str().unwrap(), false, &mut runner)
            .unwrap();

        assert!(target_local.join("state").exists());
        assert!(source_local.join("state").exists());
    }

    #[test]
    fn existing_populated_target_without_progress_is_skipped_safely() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("mount/@data");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();
        touch(source.join("new-file"));
        touch(target.join("existing-file"));

        let mount = temp.path().join("mount");
        let mut progress = InitProgress::default();
        let mut runner = FakeInitCommandRunner::default();

        seed_subvolume(
            mount.to_str().unwrap(),
            "@data",
            source.to_str().unwrap(),
            false,
            &mut progress,
            &mut runner,
        )
        .unwrap();

        assert!(target.join("existing-file").exists());
        assert!(!target.join("new-file").exists());
        assert!(runner.calls.iter().all(|call| !call.starts_with("rsync ")));
    }

    #[test]
    fn ordinary_existing_path_is_not_silently_accepted_as_subvolume() {
        let temp = tempdir().unwrap();
        let mount = temp.path().join("mount");
        let path = mount.join("@data");
        fs::create_dir_all(&path).unwrap();
        let mut runner = FakeInitCommandRunner::default();

        let error =
            create_subvolume_with_runner(mount.to_str().unwrap(), "@data", false, &mut runner)
                .unwrap_err();

        assert!(error
            .to_string()
            .contains("not a confirmed Btrfs subvolume"));
        assert!(runner
            .calls
            .iter()
            .any(|call| call.starts_with("btrfs subvolume show")));
        assert!(runner
            .calls
            .iter()
            .all(|call| !call.starts_with("btrfs subvolume create")));
    }

    #[test]
    fn failed_seed_is_recorded_incomplete_and_retries() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let mount = temp.path().join("mount");
        let target = mount.join("@data");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();
        touch(source.join("data"));

        let mut progress = InitProgress::default();
        prepare_seed(
            mount.to_str().unwrap(),
            "@data",
            source.to_str().unwrap(),
            &mut progress,
        )
        .unwrap();
        let mut failing_runner = FakeInitCommandRunner {
            fail_rsync_once: true,
            ..Default::default()
        };

        assert!(seed_subvolume(
            mount.to_str().unwrap(),
            "@data",
            source.to_str().unwrap(),
            false,
            &mut progress,
            &mut failing_runner,
        )
        .is_err());
        assert_eq!(
            InitProgress::load(mount.to_str().unwrap())
                .unwrap()
                .seeds
                .get("@data")
                .unwrap()
                .status,
            SeedStatus::Incomplete
        );

        let mut retry_runner = FakeInitCommandRunner::default();
        seed_subvolume(
            mount.to_str().unwrap(),
            "@data",
            source.to_str().unwrap(),
            false,
            &mut progress,
            &mut retry_runner,
        )
        .unwrap();
        assert!(target.join("data").exists());
        assert_eq!(
            progress.seeds.get("@data").unwrap().status,
            SeedStatus::Complete
        );
    }

    #[test]
    fn ablation_old_empty_guard_misses_nested_home_data() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let mount = temp.path().join("mount");
        let target = mount.join("@home");
        let nested_target = target.join(".local");
        fs::create_dir_all(source.join(".local")).unwrap();
        fs::create_dir_all(&nested_target).unwrap();
        touch(source.join(".local/state"));

        // The old implementation treated the parent as non-empty after
        // creating nested paths and skipped the seed entirely.
        let old_would_copy = fs::read_dir(&target).unwrap().next().is_none();
        assert!(!old_would_copy);
        assert!(!target.join(".local/state").exists());

        let mut progress = InitProgress::default();
        prepare_seed(
            mount.to_str().unwrap(),
            "@home/.local",
            source.join(".local").to_str().unwrap(),
            &mut progress,
        )
        .unwrap();
        let mut runner = FakeInitCommandRunner::default();
        seed_subvolume(
            mount.to_str().unwrap(),
            "@home/.local",
            source.join(".local").to_str().unwrap(),
            false,
            &mut progress,
            &mut runner,
        )
        .unwrap();
        assert!(nested_target.join("state").exists());
    }

    #[test]
    fn cleanup_preserves_original_error_and_does_not_remove_after_unmount_failure() {
        let temp = tempdir().unwrap();
        let mount = temp.path().join("mount");
        fs::create_dir_all(&mount).unwrap();
        let remove_called = Cell::new(false);

        let error = cleanup_setup_mount(
            mount.to_str().unwrap(),
            Err(anyhow::anyhow!("subvolume creation failed")),
            |_path| Err(anyhow::anyhow!("unmount failed")),
            |_path: &str| {
                remove_called.set(true);
                Ok(())
            },
        )
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("subvolume creation failed"));
        assert!(message.contains("unmount failed"));
        assert!(!remove_called.get());
        assert!(mount.exists());
    }
}

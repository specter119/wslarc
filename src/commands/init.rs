use anyhow::{bail, Context, Error, Result};
use console::style;
use std::fs;
use std::path::Path;

use crate::config::{Config, Distribution};
use crate::utils::cli::{
    ensure_dependencies_for, find_btrfs_device_by_label, is_mountpoint, list_block_device_names,
    read_block_device, Dependency,
};
use crate::utils::prompt::{self, confirm_or_yes, info, input, step, success, warn};
use crate::utils::shell::{run as shell_run, run_or_dry, run_with_output_interruptible};

pub fn run(
    config: &Config,
    distribution: Distribution,
    config_path: &str,
    yes: bool,
    dry_run: bool,
) -> Result<()> {
    println!("{}", style("WSL Btrfs Initialization").bold().cyan());

    let first_init = !Path::new(config_path).exists();
    let confirm_yes = allows_noninteractive_confirmation(config_path, yes);

    // Check if already initialized
    if !first_init && config.uuid.is_some() {
        warn("Configuration already exists with UUID. Re-running will overwrite.");
        if !confirm_or_yes("Continue anyway?", false, confirm_yes)? {
            return Ok(());
        }
    }

    // Collect configuration (interactive or from file)
    let mut cfg = if confirm_yes {
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
    if !confirm_or_yes("Proceed with initialization?", true, confirm_yes)? {
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
    format_btrfs(&mut runtime_cfg, &device, dry_run, confirm_yes)?;

    step(4, total_steps, "Get filesystem UUID");
    let uuid = get_uuid(&device, dry_run)?;
    runtime_cfg.uuid = Some(uuid.clone());
    cfg.uuid = Some(uuid.clone());
    cfg.vhdx.label = runtime_cfg.vhdx.label.clone();
    success(&format!("UUID: {}", uuid));

    step(5, total_steps, "Create subvolumes");
    create_subvolumes(
        &cfg,
        &runtime_cfg,
        &device,
        first_init,
        confirm_yes,
        dry_run,
    )?;

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

fn allows_noninteractive_confirmation(config_path: &str, yes: bool) -> bool {
    yes && Path::new(config_path).exists()
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
    println!(
        "  First initialization skips /home and /nix; existing configs use target-state checks"
    );

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
        if command == "rsync" {
            run_with_output_interruptible(command, args)?;
            Ok(String::new())
        } else {
            shell_run(command, args)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncKind {
    Backup,
    SnapshotOnly,
    Transfer,
}

impl SyncKind {
    fn label(self) -> &'static str {
        match self {
            Self::Backup => "backup",
            Self::SnapshotOnly => "snapshot-only",
            Self::Transfer => "transfer",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetState {
    Empty,
    NonEmpty,
    Unknown,
}

impl TargetState {
    fn label(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::NonEmpty => "non-empty",
            Self::Unknown => "not inspected (dry-run)",
        }
    }
}

#[derive(Debug, Clone)]
struct SyncPlanEntry {
    subvol: String,
    source: String,
    target: String,
    kind: SyncKind,
    excluded_paths: Vec<String>,
    state: TargetState,
    copy: bool,
    reason: Option<String>,
}

struct PlanRequest<'a> {
    mount_point: &'a str,
    subvol: &'a str,
    source: &'a str,
    kind: SyncKind,
    excluded_paths: Vec<String>,
    requested: bool,
    requested_reason: Option<String>,
    ignored_children: &'a [String],
    allow_nonempty: bool,
    dry_run: bool,
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
    first_init: bool,
    confirm_yes: bool,
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
    let mut result = create_all_subvolumes_with_runner(
        runtime_config,
        device,
        mount_point,
        first_init,
        confirm_yes,
        dry_run,
        &mut runner,
    );

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
    device: &str,
    mount_point: &str,
    first_init: bool,
    confirm_yes: bool,
    dry_run: bool,
    runner: &mut dyn InitCommandRunner,
) -> Result<()> {
    info("Creating configured A-class backup subvolumes...");
    for subvol in cfg.subvolumes.backup.keys() {
        create_subvolume_with_runner(mount_point, subvol, dry_run, runner)?;
    }

    info("Creating configured snapshot-only subvolumes...");
    for subvol in cfg.subvolumes.snapshot_only.keys() {
        create_subvolume_with_runner(mount_point, subvol, dry_run, runner)?;
    }

    // Establish nested B-class subvolumes before building the home sync plan.
    // Rsync then
    // traverses the already-created nested targets and preserves their source
    // contents instead of copying a populated home over their mount points.
    info("Creating configured B-class nested subvolumes...");
    let parent = &cfg.subvolumes.exclude.parent;
    let user = cfg.get_user();
    create_subvolume_with_runner(mount_point, parent, dry_run, runner)?;
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

    // Create the btrbk snapshot directory
    info("Creating snapshot directory...");
    create_subvolume_with_runner(mount_point, &cfg.btrbk.snapshot_dir, dry_run, runner)?;

    let plan = build_sync_plan(cfg, mount_point, first_init, dry_run)?;
    show_sync_plan(&plan);

    let pending: Vec<SyncPlanEntry> = plan.iter().filter(|entry| entry.copy).cloned().collect();
    if pending.is_empty() {
        info("No rsync work is needed");
        success("All subvolumes created");
        return Ok(());
    }

    if !confirm_or_yes("Execute the rsync plan?", true, confirm_yes)? {
        println!("Synchronization aborted; no rsync was started.");
        success("All subvolumes created");
        return Ok(());
    }

    execute_sync_plan(&pending, device, first_init, dry_run, runner)?;

    success("All subvolumes created");
    Ok(())
}

fn build_sync_plan(
    cfg: &Config,
    mount_point: &str,
    first_init: bool,
    dry_run: bool,
) -> Result<Vec<SyncPlanEntry>> {
    let parent = &cfg.subvolumes.exclude.parent;
    let home_mount = format!("/home/{}", cfg.get_user());
    let mut plan = Vec::new();

    if first_init {
        for (subvol, backup) in &cfg.subvolumes.backup {
            let excluded_paths = if subvol == parent {
                cfg.subvolumes.exclude.paths.clone()
            } else {
                Vec::new()
            };
            let enabled =
                subvol != parent && backup.mount() != home_mount && backup.mount() != "/nix";
            let reason = (!enabled).then(|| {
                if backup.mount() == "/nix" {
                    "disabled by first-init policy for /nix".to_string()
                } else {
                    "disabled by first-init policy for the home directory".to_string()
                }
            });
            plan.push(make_plan_entry(PlanRequest {
                mount_point,
                subvol,
                source: backup.mount(),
                kind: SyncKind::Backup,
                excluded_paths,
                requested: enabled,
                requested_reason: reason,
                ignored_children: &[],
                allow_nonempty: first_init,
                dry_run,
            })?);
        }
    } else {
        if let Some(backup) = cfg.subvolumes.backup.get(parent) {
            for path in &cfg.subvolumes.exclude.paths {
                let nested = format!("{}/{}", parent, path);
                let source = format!(
                    "{}/{}",
                    backup.mount().trim_end_matches('/'),
                    path.trim_start_matches('/')
                );
                plan.push(make_plan_entry(PlanRequest {
                    mount_point,
                    subvol: &nested,
                    source: &source,
                    kind: SyncKind::Backup,
                    excluded_paths: Vec::new(),
                    requested: true,
                    requested_reason: None,
                    ignored_children: &[],
                    allow_nonempty: false,
                    dry_run,
                })?);
            }
        }

        for (subvol, backup) in &cfg.subvolumes.backup {
            let excluded_paths = if subvol == parent {
                cfg.subvolumes.exclude.paths.clone()
            } else {
                Vec::new()
            };
            let ignored_children = if subvol == parent {
                cfg.subvolumes.exclude.paths.as_slice()
            } else {
                &[]
            };
            plan.push(make_plan_entry(PlanRequest {
                mount_point,
                subvol,
                source: backup.mount(),
                kind: SyncKind::Backup,
                excluded_paths,
                requested: true,
                requested_reason: None,
                ignored_children,
                allow_nonempty: false,
                dry_run,
            })?);
        }
    }

    for (subvol, snapshot) in &cfg.subvolumes.snapshot_only {
        plan.push(make_plan_entry(PlanRequest {
            mount_point,
            subvol,
            source: &snapshot.source,
            kind: SyncKind::SnapshotOnly,
            excluded_paths: Vec::new(),
            requested: true,
            requested_reason: None,
            ignored_children: &[],
            allow_nonempty: true,
            dry_run,
        })?);
    }

    for (subvol, transfer) in &cfg.subvolumes.transfer {
        plan.push(make_plan_entry(PlanRequest {
            mount_point,
            subvol,
            source: &transfer.mount,
            kind: SyncKind::Transfer,
            excluded_paths: Vec::new(),
            requested: true,
            requested_reason: None,
            ignored_children: &[],
            allow_nonempty: first_init,
            dry_run,
        })?);
    }

    Ok(plan)
}

fn make_plan_entry(request: PlanRequest<'_>) -> Result<SyncPlanEntry> {
    let PlanRequest {
        mount_point,
        subvol,
        source,
        kind,
        excluded_paths,
        requested,
        requested_reason,
        ignored_children,
        allow_nonempty,
        dry_run,
    } = request;
    let target = format!("{}/{}", mount_point, subvol);
    let state = inspect_target(&target, ignored_children, dry_run)?;
    let source_exists = dry_run || Path::new(source).exists();

    let (copy, reason) = if !requested {
        (false, requested_reason)
    } else if !source_exists {
        (false, Some(format!("source does not exist: {}", source)))
    } else if allow_nonempty
        || matches!(kind, SyncKind::SnapshotOnly)
        || matches!(state, TargetState::Empty | TargetState::Unknown)
    {
        (true, None)
    } else {
        (
            false,
            Some("target is non-empty; skipped for an existing configuration".to_string()),
        )
    };

    Ok(SyncPlanEntry {
        subvol: subvol.to_string(),
        source: source.to_string(),
        target,
        kind,
        excluded_paths,
        state,
        copy,
        reason,
    })
}

fn inspect_target(target: &str, ignored_children: &[String], dry_run: bool) -> Result<TargetState> {
    if dry_run {
        return Ok(TargetState::Unknown);
    }
    if !Path::new(target).exists() {
        return Ok(TargetState::Empty);
    }

    let ignored: Vec<&str> = ignored_children
        .iter()
        .map(|path| path.trim_matches('/').split('/').next().unwrap_or(path))
        .collect();
    for entry in
        fs::read_dir(target).with_context(|| format!("Failed to inspect sync target {}", target))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !ignored.iter().any(|path| *path == name) {
            return Ok(TargetState::NonEmpty);
        }
    }
    Ok(TargetState::Empty)
}

fn show_sync_plan(plan: &[SyncPlanEntry]) {
    println!();
    println!("{}", style("Rsync plan").bold());
    if plan.is_empty() {
        println!("  No configured sources");
        return;
    }

    for entry in plan {
        if entry.copy {
            println!(
                "  [sync] {} {} -> {} (target: {})",
                entry.kind.label(),
                entry.source,
                entry.target,
                entry.state.label()
            );
        } else if let Some(reason) = &entry.reason {
            println!(
                "  [skip] {} {} -> {} ({})",
                entry.kind.label(),
                entry.source,
                entry.target,
                reason
            );
        }
    }
    println!();
}

fn execute_sync_plan(
    plan: &[SyncPlanEntry],
    device: &str,
    first_init: bool,
    dry_run: bool,
    runner: &mut dyn InitCommandRunner,
) -> Result<()> {
    let mut remaining = plan.to_vec();
    while let Some(entry) = remaining.first().cloned() {
        if first_init && entry.state == TargetState::NonEmpty {
            warn(&format!(
                "Target {} already contains data; it will be updated from {}.",
                entry.target, entry.source
            ));
            if !prompt::confirm(
                &format!(
                    "Copy {} into non-empty target {}?",
                    entry.source, entry.target
                ),
                false,
            )? {
                info(&format!("Skipped {}", entry.subvol));
                remaining.remove(0);
                continue;
            }
        }

        if let Err(error) = run_sync_entry(&entry, dry_run, runner) {
            print_resume_command(device, &remaining);
            return Err(error);
        }
        remaining.remove(0);
    }

    Ok(())
}

fn run_sync_entry(
    entry: &SyncPlanEntry,
    dry_run: bool,
    runner: &mut dyn InitCommandRunner,
) -> Result<()> {
    let mut args = vec![
        "-aAX".to_string(),
        "--partial".to_string(),
        "--info=progress2".to_string(),
    ];
    if entry.kind == SyncKind::SnapshotOnly {
        args.push("--delete".to_string());
    }
    for path in &entry.excluded_paths {
        args.push("--exclude".to_string());
        args.push(format!("/{}/", path.trim_matches('/')));
    }
    args.push(format!("{}/", entry.source.trim_end_matches('/')));
    args.push(format!("{}/", entry.target.trim_end_matches('/')));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();

    info(&format!("Syncing {} -> {}", entry.source, entry.target));
    run_init_command(runner, "rsync", &args, dry_run)?;
    success(&format!("{} synced", entry.subvol));
    Ok(())
}

fn print_resume_command(device: &str, pending: &[SyncPlanEntry]) {
    let Some(command) = render_resume_command(device, pending) else {
        return;
    };

    warn("Rsync stopped. Run the following command to resume the remaining sync:");
    println!();
    print!("{command}");
}

fn render_resume_command(device: &str, pending: &[SyncPlanEntry]) -> Option<String> {
    let pending: Vec<&SyncPlanEntry> = pending.iter().filter(|entry| entry.copy).collect();
    if pending.is_empty() {
        return None;
    }

    let mut command = String::new();
    command.push_str("set -e\n");
    command.push_str("resume_root=$(mktemp -d /tmp/wslarc-resume.XXXXXX)\n");
    command.push_str("cleanup() { sudo umount \"$resume_root\" >/dev/null 2>&1 || true; rmdir \"$resume_root\" 2>/dev/null || true; }\n");
    command.push_str("trap cleanup EXIT\n");
    command.push_str(&format!(
        "sudo mount -o subvolid=5 {} \"$resume_root\"\n",
        crate::generators::invocation::shell_argument(device)
    ));

    for entry in pending {
        let mut args = vec![
            "sudo".to_string(),
            "rsync".to_string(),
            "-aAX".to_string(),
            "--partial".to_string(),
            "--info=progress2".to_string(),
        ];
        if entry.kind == SyncKind::SnapshotOnly {
            args.push("--delete".to_string());
        }
        for path in &entry.excluded_paths {
            args.push("--exclude".to_string());
            args.push(format!("/{}/", path.trim_matches('/')));
        }
        args.push(format!("{}/", entry.source.trim_end_matches('/')));
        let target_suffix = format!("{}/", entry.subvol.trim_end_matches('/'));
        args.push(target_suffix);
        let rendered = args
            .iter()
            .enumerate()
            .map(|(index, arg)| {
                if index == args.len() - 1 {
                    format!(
                        "\"$resume_root\"/{}",
                        crate::generators::invocation::shell_argument(arg)
                    )
                } else {
                    crate::generators::invocation::shell_argument(arg)
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        command.push_str(&rendered);
        command.push('\n');
    }
    command.push_str(
        "# The temporary subvolume-5 mount is cleaned up automatically when this shell exits.\n",
    );
    Some(command)
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
    fn init_dry_run_does_not_execute_sync_commands() {
        let temp = tempdir().unwrap();
        let mount = temp.path().join("mount");
        let source = temp.path().join("source");
        fs::create_dir_all(mount.join("@home")).unwrap();
        fs::create_dir_all(&source).unwrap();
        let config = test_config(&source, &temp.path().join("transfer-source"));
        let mut runner = FakeInitCommandRunner::default();
        create_all_subvolumes_with_runner(
            &config,
            "/dev/test",
            mount.to_str().unwrap(),
            true,
            true,
            true,
            &mut runner,
        )
        .unwrap();
        assert!(runner.calls.is_empty());
        assert!(mount.join("@home").exists());
    }

    #[test]
    fn yes_requires_an_existing_config_file() {
        let temp = tempdir().unwrap();
        let missing = temp.path().join("missing.toml");
        let existing = temp.path().join("existing.toml");
        File::create(&existing).unwrap();

        assert!(!allows_noninteractive_confirmation(
            missing.to_str().unwrap(),
            true
        ));
        assert!(allows_noninteractive_confirmation(
            existing.to_str().unwrap(),
            true
        ));
        assert!(!allows_noninteractive_confirmation(
            existing.to_str().unwrap(),
            false
        ));
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
    fn first_init_skips_home_and_syncs_transfer_sources() {
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

        create_all_subvolumes_with_runner(
            &config,
            "/dev/test",
            mount.to_str().unwrap(),
            true,
            true,
            false,
            &mut runner,
        )
        .unwrap();

        assert!(!mount.join("@home/profile").exists());
        assert!(!mount.join("@home/.local/state").exists());
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
            .all(|call| !call.starts_with("rsync ") || !call.contains("@home")));
    }

    #[test]
    fn existing_config_syncs_empty_backup_and_nonempty_snapshot_only() {
        let temp = tempdir().unwrap();
        let mount = temp.path().join("mount");
        let backup_source = temp.path().join("backup-source");
        let empty_source = temp.path().join("empty-source");
        let snapshot_source = temp.path().join("snapshot-source");
        fs::create_dir_all(&mount).unwrap();
        fs::create_dir_all(&backup_source).unwrap();
        fs::create_dir_all(&empty_source).unwrap();
        fs::create_dir_all(&snapshot_source).unwrap();
        touch(backup_source.join("new-file"));
        touch(empty_source.join("empty-target-file"));
        touch(snapshot_source.join("current-file"));
        fs::create_dir_all(mount.join("@data")).unwrap();
        fs::create_dir_all(mount.join("@empty")).unwrap();
        fs::create_dir_all(mount.join("@etc")).unwrap();
        touch(mount.join("@data/existing-file"));
        touch(mount.join("@etc/old-file"));

        let mut config = Config::for_distribution(crate::config::Distribution::Arch);
        config.subvolumes.backup.clear();
        config.subvolumes.backup.insert(
            "@data".to_string(),
            BackupSubvol::Simple(backup_source.display().to_string()),
        );
        config.subvolumes.backup.insert(
            "@empty".to_string(),
            BackupSubvol::Simple(empty_source.display().to_string()),
        );
        config.subvolumes.exclude.paths.clear();
        config.subvolumes.snapshot_only.clear();
        config.subvolumes.snapshot_only.insert(
            "@etc".to_string(),
            crate::config::SnapshotSubvol {
                snapshot_name: "etc".to_string(),
                source: snapshot_source.display().to_string(),
            },
        );
        config.subvolumes.transfer.clear();

        let plan = build_sync_plan(&config, mount.to_str().unwrap(), false, false).unwrap();
        let data = plan.iter().find(|entry| entry.subvol == "@data").unwrap();
        let empty = plan.iter().find(|entry| entry.subvol == "@empty").unwrap();
        let snapshot = plan.iter().find(|entry| entry.subvol == "@etc").unwrap();

        assert!(!data.copy);
        assert_eq!(data.state, TargetState::NonEmpty);
        assert!(empty.copy);
        assert_eq!(empty.state, TargetState::Empty);
        assert!(snapshot.copy);
        assert_eq!(snapshot.state, TargetState::NonEmpty);
    }

    #[test]
    fn existing_config_ignores_nested_subvolume_when_checking_parent() {
        let temp = tempdir().unwrap();
        let mount = temp.path().join("mount");
        let source_home = temp.path().join("source-home");
        fs::create_dir_all(source_home.join(".local")).unwrap();
        fs::create_dir_all(mount.join("@home/.local")).unwrap();
        touch(source_home.join("profile"));
        touch(source_home.join(".local/state"));

        let config = test_config(&source_home, &temp.path().join("unused-transfer"));
        let plan = build_sync_plan(&config, mount.to_str().unwrap(), false, false).unwrap();
        let parent = plan.iter().find(|entry| entry.subvol == "@home").unwrap();
        let nested = plan
            .iter()
            .find(|entry| entry.subvol == "@home/.local")
            .unwrap();

        assert!(parent.copy);
        assert_eq!(parent.state, TargetState::Empty);
        assert!(nested.copy);
        assert_eq!(nested.state, TargetState::Empty);
    }

    #[test]
    fn resume_command_mounts_top_level_and_keeps_rsync_options() {
        let entry = SyncPlanEntry {
            subvol: "@data".to_string(),
            source: "/source/with space".to_string(),
            target: "/mnt/btrfs-setup/@data".to_string(),
            kind: SyncKind::Backup,
            excluded_paths: vec![".cache".to_string()],
            state: TargetState::Empty,
            copy: true,
            reason: None,
        };

        let command = render_resume_command("/dev/path with space", &[entry]).unwrap();

        assert!(command.contains("sudo mount -o subvolid=5 '/dev/path with space'"));
        assert!(command.contains("'sudo' 'rsync' '-aAX' '--partial' '--info=progress2'"));
        assert!(command.contains("'--exclude' '/.cache/'"));
        assert!(command.contains("'/source/with space/' \"$resume_root\"/'@data/'"));
        assert!(command.contains("trap cleanup EXIT"));
    }

    #[test]
    fn preexisting_nested_home_subvolume_is_synced_from_its_source() {
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

        create_all_subvolumes_with_runner(
            &config,
            "/dev/test",
            mount.to_str().unwrap(),
            false,
            true,
            false,
            &mut runner,
        )
        .unwrap();

        assert!(target_local.join("state").exists());
        assert!(source_local.join("state").exists());
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

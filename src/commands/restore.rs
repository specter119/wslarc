use anyhow::{anyhow, bail, Context, Result};
use console::style;
use std::fs;
use std::path::{Component, Path};

use crate::config::{Config, Distribution};
use crate::utils::cli::{command_exists, find_mount, list_directory_names, MountInfo};
use crate::utils::prompt::{confirm_or_yes, info, section, select, step, success, warn};
use crate::utils::shell::{run as shell_run, run_with_output};

pub fn run(
    config: &Config,
    distribution: Distribution,
    snapshot: Option<String>,
    yes: bool,
) -> Result<()> {
    println!("{}", style("Restore from Snapshot").bold().cyan());
    println!();

    let snapshot_dir = format!("{}/{}", config.mount.base, config.btrbk.snapshot_dir);

    // Get available snapshots.
    let snapshot_list = list_directory_names(&snapshot_dir)?;
    if snapshot_list.is_empty() {
        bail!("No snapshots found in {}", snapshot_dir);
    }

    // Select snapshot.
    let selected = if let Some(ref name) = snapshot {
        if !snapshot_list.contains(name) {
            bail!("Snapshot '{}' not found", name);
        }
        name.clone()
    } else {
        let options: Vec<&str> = snapshot_list
            .iter()
            .rev()
            .take(10)
            .map(|s| s.as_str())
            .collect();
        let idx = select("Select snapshot to restore", &options, 0)?;
        options[idx].to_string()
    };

    println!();
    info(&format!("Selected: {}", selected));

    // Parse snapshot name to get the configured target subvolume.
    // Format: subvol.YYYYMMDDTHHMMSS or subvol.YYYYMMDD (btrbk formats).
    let parts: Vec<&str> = selected.rsplitn(2, '.').collect();
    if parts.len() < 2 {
        bail!("Invalid snapshot name format: {}", selected);
    }
    let subvol_base = parts[1];
    let (subvol_name, live_source) = resolve_snapshot_target(config, subvol_base, &selected)?;

    info(&format!("Target subvolume: {}", subvol_name));

    let is_snapshot_only_subvol = config.subvolumes.snapshot_only.contains_key(&subvol_name);
    let mount_point = if is_snapshot_only_subvol {
        None
    } else {
        config
            .subvolumes
            .backup
            .get(&subvol_name)
            .map(|backup| backup.mount().to_string())
    };

    let request = RestoreRequest {
        base: config.mount.base.clone(),
        target: subvol_name,
        source_snapshot: format!("{}/{}", snapshot_dir, selected),
        source_name: selected.clone(),
        mount_point,
        live_source,
    };

    // The complete read-only preflight runs before asking for confirmation.
    // In particular, missing btrfs-progs must never be discovered after an
    // unmount or a subvolume rename.
    let mut backend = SystemRestoreBackend;
    let plan = preflight_restore(&mut backend, config, distribution, &request)?;

    section("Restore Plan");
    println!("  Source snapshot: {}", request.source_snapshot);
    println!("  Target subvolume: {}", plan.target);
    if let Some(ref mount_point) = plan.mount_point {
        println!("  Mount point: {}", mount_point);
    }
    if let Some(ref source) = plan.live_source {
        println!("  Restore source: {}", source);
    }
    println!();

    warn("This will REPLACE the current subvolume with the snapshot!");
    warn("All changes since the snapshot will be LOST!");
    if plan.mount_point.is_some() {
        warn("The mount point will be detached only after staging succeeds.");
    }
    if plan.live_source.is_some() {
        warn("The configured live source will be replaced with the restored snapshot.");
    }
    println!();

    if !confirm_or_yes("Proceed with restore?", false, yes)? {
        println!("Aborted.");
        return Ok(());
    }

    let outcome = execute_restore(&mut backend, config, &plan)?;

    println!();
    info(&format!(
        "Old subvolume retained at {}",
        outcome.backup_subvol
    ));
    println!(
        "  To delete it (free space): btrfs subvolume delete {}",
        outcome.backup_subvol
    );
    println!("  To rollback: reverse the restore process");

    println!();
    println!("{}", style("Restore complete!").green().bold());

    if outcome.mount_point.is_some() {
        println!();
        println!("Note: You may need to restart services or reboot for full effect.");
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct RestoreRequest {
    base: String,
    target: String,
    source_snapshot: String,
    source_name: String,
    mount_point: Option<String>,
    live_source: Option<String>,
}

#[derive(Debug, Clone)]
struct RestorePlan {
    target: String,
    current_subvol: String,
    source_snapshot: String,
    staged_subvol: String,
    backup_subvol: String,
    nested_children: Vec<String>,
    mount_point: Option<String>,
    original_mount: Option<MountInfo>,
    live_source: Option<String>,
    filesystem_uuid: String,
    mount_options: String,
}

#[derive(Debug, Default)]
struct CutoverState {
    original_unmounted: bool,
    children_in_staging: Vec<String>,
    parent_in_backup: bool,
    stage_in_current: bool,
}

#[derive(Debug)]
struct RestoreOutcome {
    backup_subvol: String,
    mount_point: Option<String>,
}

/// The restore transaction depends on a small, injectable host boundary.
///
/// Keeping this boundary local to restore makes the destructive sequence
/// testable without teaching the production CLI helpers about restore-specific
/// rollback state.
trait RestoreBackend {
    fn command_exists(&self, command: &str) -> bool;
    fn path_exists(&self, path: &str) -> bool;
    fn path_is_directory(&self, path: &str) -> bool;
    fn directory_is_empty(&self, path: &str) -> Result<bool>;
    fn remove_empty_directory(&mut self, path: &str) -> Result<()>;
    fn create_directory(&mut self, path: &str) -> Result<()>;
    fn find_mount(&mut self, path: &str) -> Result<Option<MountInfo>>;
    fn run(&mut self, command: &str, args: &[&str]) -> Result<String>;
    fn run_stream(&mut self, command: &str, args: &[&str]) -> Result<()>;
}

struct SystemRestoreBackend;

impl RestoreBackend for SystemRestoreBackend {
    fn command_exists(&self, command: &str) -> bool {
        command_exists(command)
    }

    fn path_exists(&self, path: &str) -> bool {
        fs::symlink_metadata(path).is_ok()
    }

    fn path_is_directory(&self, path: &str) -> bool {
        Path::new(path).is_dir()
    }

    fn directory_is_empty(&self, path: &str) -> Result<bool> {
        if !self.path_is_directory(path) {
            return Ok(false);
        }

        let mut entries =
            fs::read_dir(path).with_context(|| format!("Failed to read directory {}", path))?;
        Ok(entries.next().transpose()?.is_none())
    }

    fn remove_empty_directory(&mut self, path: &str) -> Result<()> {
        fs::remove_dir(path).with_context(|| format!("Failed to remove empty directory {}", path))
    }

    fn create_directory(&mut self, path: &str) -> Result<()> {
        fs::create_dir_all(path).with_context(|| format!("Failed to create directory {}", path))
    }

    fn find_mount(&mut self, path: &str) -> Result<Option<MountInfo>> {
        find_mount(path)
    }

    fn run(&mut self, command: &str, args: &[&str]) -> Result<String> {
        shell_run(command, args)
    }

    fn run_stream(&mut self, command: &str, args: &[&str]) -> Result<()> {
        run_with_output(command, args)
    }
}

fn preflight_restore(
    backend: &mut dyn RestoreBackend,
    config: &Config,
    distribution: Distribution,
    request: &RestoreRequest,
) -> Result<RestorePlan> {
    ensure_restore_dependencies(backend, distribution, request.live_source.is_some())?;

    let target = validate_relative_path(&request.target, "target subvolume")?;
    validate_snapshot_name(&request.source_name)?;
    let current_subvol = join_path(&request.base, &target);
    if let Some(source) = request.live_source.as_deref() {
        let source = Path::new(source);
        if !source.is_absolute()
            || source
                .components()
                .any(|part| matches!(part, Component::ParentDir))
            || Path::new(&request.base).starts_with(source)
            || source.starts_with(&current_subvol)
        {
            bail!("Snapshot-only source overlaps restore storage or is not an absolute safe path");
        }
    }

    let configured_uuid = config
        .uuid
        .as_deref()
        .filter(|uuid| !uuid.trim().is_empty())
        .ok_or_else(|| anyhow!("Configured Btrfs filesystem UUID is missing"))?;
    let base_mount = backend
        .find_mount(&request.base)?
        .ok_or_else(|| anyhow!("Btrfs base mount {} is not mounted", request.base))?;
    let mounted_uuid = base_mount
        .uuid
        .as_deref()
        .filter(|uuid| !uuid.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "Btrfs base mount {} does not expose a filesystem UUID",
                request.base
            )
        })?
        .to_string();
    if !configured_uuid.eq_ignore_ascii_case(&mounted_uuid) {
        bail!(
            "Configured Btrfs UUID '{}' does not match mounted UUID '{}'",
            configured_uuid,
            mounted_uuid
        );
    }
    let filesystem_uuid = configured_uuid.to_string();
    validate_mount_identity(&base_mount, &filesystem_uuid, "Btrfs base mount", None)?;

    if !backend.path_exists(&current_subvol) {
        bail!(
            "Current target subvolume {} does not exist; refusing restore",
            current_subvol
        );
    }

    let current_name = read_subvolume_name(backend, &current_subvol)?;
    if !subvolume_names_match(&current_name, &target) {
        bail!(
            "Current path {} identifies as subvolume '{}', expected '{}'",
            current_subvol,
            current_name,
            target
        );
    }

    let source_name = read_subvolume_name(backend, &request.source_snapshot)?;
    if !subvolume_names_match(&source_name, &request.source_name) {
        bail!(
            "Snapshot {} identifies as subvolume '{}', expected '{}'",
            request.source_snapshot,
            source_name,
            request.source_name
        );
    }

    let original_mount = if let Some(mount_point) = request.mount_point.as_deref() {
        let mount = backend.find_mount(mount_point)?;
        if let Some(ref mount_info) = mount {
            validate_mount_identity(
                mount_info,
                &filesystem_uuid,
                &format!("Target mount {}", mount_point),
                Some(&target),
            )?;
        }
        mount
    } else {
        None
    };

    let (nested_paths, nested_children) =
        discover_nested_children(backend, &current_subvol, &target)?;
    for child in &nested_paths {
        let child_path = join_path(&current_subvol, child);
        if !backend.path_exists(&child_path) {
            bail!(
                "Btrfs reports nested subvolume {} but its path is unavailable; refusing restore",
                child_path
            );
        }

        // A separately mounted child must not be moved underneath an active
        // mount.  Refuse it before any stage or cutover operation.
        if backend.find_mount(&child_path)?.is_some() {
            bail!(
                "Nested subvolume {} is separately mounted; refusing restore while it is live",
                child_path
            );
        }
        if let Some(mount_point) = request.mount_point.as_deref() {
            let nested_mount_point = join_path(mount_point, child);
            if backend.find_mount(&nested_mount_point)?.is_some() {
                bail!(
                    "Nested subvolume mount {} is live; refusing restore before cutover",
                    nested_mount_point
                );
            }
        }
    }

    let target_parent = parent_path(&current_subvol).to_string();
    let target_leaf = Path::new(&current_subvol)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            anyhow!(
                "Target subvolume path {} has no usable name",
                current_subvol
            )
        })?;
    let staged_subvol = unique_sibling(
        &target_parent,
        &format!(".wslarc-restore-staging-{}", target_leaf),
        backend,
    )?;
    let backup_subvol = unique_sibling(
        &target_parent,
        &format!("{}.restore-backup", target_leaf),
        backend,
    )?;

    let mount_options = config
        .subvolumes
        .backup
        .get(&target)
        .and_then(|backup| backup.options())
        .unwrap_or(&config.mount.options)
        .to_string();

    Ok(RestorePlan {
        target,
        current_subvol,
        source_snapshot: request.source_snapshot.clone(),
        staged_subvol,
        backup_subvol,
        nested_children,
        mount_point: request.mount_point.clone(),
        original_mount,
        live_source: request.live_source.clone(),
        filesystem_uuid,
        mount_options,
    })
}

fn ensure_restore_dependencies(
    backend: &dyn RestoreBackend,
    distribution: Distribution,
    needs_rsync: bool,
) -> Result<()> {
    let mut missing = Vec::new();
    if !backend.command_exists("btrfs") {
        missing.push(("btrfs-progs", "btrfs"));
    }
    if needs_rsync && !backend.command_exists("rsync") {
        missing.push(("rsync", "rsync"));
    }

    if missing.is_empty() {
        return Ok(());
    }

    let packages = missing
        .iter()
        .map(|(package, _)| *package)
        .collect::<Vec<_>>();
    let details = missing
        .iter()
        .map(|(package, command)| format!("  - {} (command: {})", package, command))
        .collect::<Vec<_>>();
    let install_command = match distribution {
        Distribution::Arch => format!("sudo pacman -S {}", packages.join(" ")),
        Distribution::Debian => format!("sudo apt-get install {}", packages.join(" ")),
    };

    bail!(
        "Missing required dependencies for {}:\n{}\nInstall with: {}",
        distribution.display_name(),
        details.join("\n"),
        install_command
    )
}

fn execute_restore(
    backend: &mut dyn RestoreBackend,
    _config: &Config,
    plan: &RestorePlan,
) -> Result<RestoreOutcome> {
    let total_steps = (1
        + usize::from(plan.original_mount.is_some())
        + 1
        + usize::from(plan.live_source.is_some())
        + usize::from(plan.original_mount.is_some())
        + 1) as u32;
    let mut current_step = 0;

    current_step += 1;
    step(
        current_step,
        total_steps,
        &format!("Stage snapshot for {}", plan.target),
    );
    if let Err(error) = snapshot_to_stage(backend, plan) {
        return Err(anyhow!(
            "Failed to stage snapshot at {}; no unmount or cutover was attempted: {:#}",
            plan.staged_subvol,
            error
        ));
    }

    // The writable snapshot must contain safe placeholders for every nested
    // child before the live mount is detached.  A non-empty placeholder means
    // that this layout is not safely movable with this narrow transaction.
    if let Err(error) = prepare_staged_children(backend, plan) {
        return Err(anyhow!(
            "Staged snapshot {} cannot preserve nested subvolumes; no unmount or cutover was attempted: {:#}",
            plan.staged_subvol,
            error
        ));
    }

    let mut state = CutoverState::default();

    if let Some(ref mount_point) = plan.mount_point {
        if plan.original_mount.is_some() {
            current_step += 1;
            step(
                current_step,
                total_steps,
                &format!("Umount {}", mount_point),
            );
            if let Err(error) = unmount_without_lazy_fallback(backend, mount_point) {
                // If the command returned an error but the mount is now
                // absent, repair the original mount before returning.  If it
                // is still present, do not issue a second mount operation.
                if matches!(backend.find_mount(mount_point), Ok(None)) {
                    state.original_unmounted = true;
                    return transaction_failure(backend, plan, state, error);
                }
                return Err(anyhow!(
                    "Cannot continue restore while {} remains mounted; staged snapshot retained at {}: {:#}",
                    mount_point,
                    plan.staged_subvol,
                    error
                ));
            }
            state.original_unmounted = true;
        }
    }

    current_step += 1;
    step(
        current_step,
        total_steps,
        &format!("Switch {} transactionally", plan.target),
    );

    for child in &plan.nested_children {
        if let Err(error) =
            move_nested_child(backend, &plan.current_subvol, &plan.staged_subvol, child)
        {
            return transaction_failure(
                backend,
                plan,
                state,
                anyhow!(
                    "Failed to move nested subvolume {} into staged snapshot: {:#}",
                    child,
                    error
                ),
            );
        }
        state.children_in_staging.push(child.clone());
    }

    if let Err(error) = move_path(backend, &plan.current_subvol, &plan.backup_subvol) {
        return transaction_failure(
            backend,
            plan,
            state,
            anyhow!(
                "Failed to retain current subvolume at {}: {:#}",
                plan.backup_subvol,
                error
            ),
        );
    }
    state.parent_in_backup = true;

    if let Err(error) = move_path(backend, &plan.staged_subvol, &plan.current_subvol) {
        return transaction_failure(
            backend,
            plan,
            state,
            anyhow!(
                "Failed to switch staged snapshot into {}: {:#}",
                plan.current_subvol,
                error
            ),
        );
    }
    state.stage_in_current = true;

    if let Some(ref mount_point) = plan.mount_point {
        if plan.original_mount.is_some() {
            current_step += 1;
            step(
                current_step,
                total_steps,
                &format!("Remount {}", mount_point),
            );
            if let Err(error) = mount_target_and_verify(backend, plan, &plan.target) {
                return transaction_failure(
                    backend,
                    plan,
                    state,
                    anyhow!("Failed to remount {}: {:#}", mount_point, error),
                );
            }
        }
    }

    if let Some(ref live_source) = plan.live_source {
        current_step += 1;
        step(
            current_step,
            total_steps,
            &format!("Restore live source {}", live_source),
        );
        let source_with_slash = format!("{}/", live_source);
        let restored_subvol_with_slash = format!("{}/", plan.current_subvol);
        let args = [
            "-aAX",
            "--delete",
            &restored_subvol_with_slash,
            &source_with_slash,
        ];
        if let Err(error) = backend.run_stream("rsync", &args) {
            warn(&format!(
                "Rsync to {} failed; the live source may be partially updated. \
                 The recovery subvolume remains at {}.",
                live_source, plan.backup_subvol
            ));
            return Err(anyhow!(
                "Snapshot subvolume was switched, but rsync to {} failed: {:#}; \
                 do not treat the live source as rolled back; recover from {}",
                live_source,
                error,
                plan.backup_subvol
            ));
        }
        success(&format!("Restored snapshot content to {}", live_source));
    }

    current_step += 1;
    step(current_step, total_steps, "Cleanup");
    success("Restore transaction committed");

    Ok(RestoreOutcome {
        backup_subvol: plan.backup_subvol.clone(),
        mount_point: plan.mount_point.clone(),
    })
}

fn snapshot_to_stage(backend: &mut dyn RestoreBackend, plan: &RestorePlan) -> Result<()> {
    if backend.path_exists(&plan.staged_subvol) {
        bail!("Staging destination already exists: {}", plan.staged_subvol);
    }
    let args = [
        "subvolume",
        "snapshot",
        plan.source_snapshot.as_str(),
        plan.staged_subvol.as_str(),
    ];
    backend.run("btrfs", &args)?;
    success("Snapshot staged as a writable subvolume");
    Ok(())
}

fn prepare_staged_children(backend: &mut dyn RestoreBackend, plan: &RestorePlan) -> Result<()> {
    for child in &plan.nested_children {
        let destination = join_path(&plan.staged_subvol, child);
        let destination_parent = Path::new(&destination)
            .parent()
            .ok_or_else(|| anyhow!("Nested destination {} has no parent", destination))?;
        let destination_parent = destination_parent.to_string_lossy().to_string();
        backend.create_directory(&destination_parent)?;

        if backend.path_exists(&destination) {
            if !backend.path_is_directory(&destination)
                || !backend.directory_is_empty(&destination)?
            {
                bail!(
                    "staged nested path {} is not an empty directory; refusing unsafe merge",
                    destination
                );
            }
            backend.remove_empty_directory(&destination)?;
        }
    }
    Ok(())
}

fn unmount_without_lazy_fallback(
    backend: &mut dyn RestoreBackend,
    mount_point: &str,
) -> Result<()> {
    backend.run("umount", &[mount_point]).with_context(|| {
        format!(
            "Cannot umount {} safely; refusing lazy unmount because live use is unknown",
            mount_point
        )
    })?;

    match backend.find_mount(mount_point)? {
        None => {
            success("Unmounted successfully");
            Ok(())
        }
        Some(_) => bail!(
            "umount {} reported success but the mount is still present; refusing cutover",
            mount_point
        ),
    }
}

fn move_nested_child(
    backend: &mut dyn RestoreBackend,
    source_parent: &str,
    destination_parent: &str,
    child: &str,
) -> Result<()> {
    let source = join_path(source_parent, child);
    let destination = join_path(destination_parent, child);
    if !backend.path_exists(&source) {
        bail!("nested subvolume source {} does not exist", source);
    }

    let destination_parent_path = Path::new(&destination)
        .parent()
        .ok_or_else(|| anyhow!("Nested destination {} has no parent", destination))?;
    backend.create_directory(&destination_parent_path.to_string_lossy())?;

    if backend.path_exists(&destination) {
        if !backend.path_is_directory(&destination) || !backend.directory_is_empty(&destination)? {
            bail!(
                "nested destination {} is not an empty directory; refusing unsafe merge",
                destination
            );
        }
        backend.remove_empty_directory(&destination)?;
    }

    move_path(backend, &source, &destination)
}

fn move_path(backend: &mut dyn RestoreBackend, source: &str, destination: &str) -> Result<()> {
    if backend.path_exists(destination) {
        bail!(
            "Refusing to replace existing restore destination {}",
            destination
        );
    }
    backend
        .run("mv", &["-T", "--no-clobber", "--", source, destination])
        .with_context(|| format!("Failed to move {} to {}", source, destination))?;
    if backend.path_exists(source) || !backend.path_exists(destination) {
        bail!(
            "Move was not completed from {} to {}; recovery paths retained",
            source,
            destination
        );
    }
    Ok(())
}

fn mount_target_and_verify(
    backend: &mut dyn RestoreBackend,
    plan: &RestorePlan,
    target: &str,
) -> Result<()> {
    let mount_point = plan
        .mount_point
        .as_deref()
        .ok_or_else(|| anyhow!("Cannot mount a target without a mount point"))?;
    let options = if plan.mount_options.trim().is_empty() {
        format!("subvol={}", target)
    } else {
        format!("subvol={},{}", target, plan.mount_options)
    };
    let device = format!("UUID={}", plan.filesystem_uuid);
    let args = ["-t", "btrfs", "-o", &options, &device, mount_point];
    backend.run("mount", &args)?;

    let mount = backend
        .find_mount(mount_point)?
        .ok_or_else(|| anyhow!("mount {} completed but no mount is visible", mount_point))?;
    validate_mount_identity(
        &mount,
        &plan.filesystem_uuid,
        &format!("Target mount {}", mount_point),
        Some(target),
    )
}

fn transaction_failure(
    backend: &mut dyn RestoreBackend,
    plan: &RestorePlan,
    state: CutoverState,
    operation_error: anyhow::Error,
) -> Result<RestoreOutcome> {
    let cleanup_errors = rollback_cutover(backend, plan, &state);
    Err(combine_transaction_errors(operation_error, cleanup_errors))
}

fn rollback_cutover(
    backend: &mut dyn RestoreBackend,
    plan: &RestorePlan,
    state: &CutoverState,
) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();

    // If the new subvolume was mounted, detach it before moving anything
    // below the mount point.  Never use umount -l here.
    if state.stage_in_current && plan.original_mount.is_some() {
        let mount_point = plan.mount_point.as_deref().unwrap_or_default();
        match backend.find_mount(mount_point) {
            Ok(None) => {}
            Ok(Some(_)) => {
                if let Err(error) = backend.run("umount", &[mount_point]) {
                    errors.push(anyhow!(
                        "rollback could not umount new target {} safely: {:#}",
                        mount_point,
                        error
                    ));
                    return errors;
                }
                match backend.find_mount(mount_point) {
                    Ok(None) => {}
                    Ok(Some(_)) => {
                        errors.push(anyhow!(
                            "rollback umount {} reported success but the mount remains",
                            mount_point
                        ));
                        return errors;
                    }
                    Err(error) => {
                        errors.push(anyhow!(
                            "rollback could not verify umount {}: {:#}",
                            mount_point,
                            error
                        ));
                        return errors;
                    }
                }
            }
            Err(error) => {
                errors.push(anyhow!(
                    "rollback could not inspect mount {} before moving data: {:#}",
                    mount_point,
                    error
                ));
                return errors;
            }
        }
    }

    if state.stage_in_current {
        // The new parent is at current_subvol and its preserved children are
        // below it.  Move those children back into the old parent first.
        if let Err(child_errors) = move_children(
            backend,
            &plan.current_subvol,
            &plan.backup_subvol,
            &state.children_in_staging,
        ) {
            errors.extend(child_errors);
            return errors;
        }

        let failed_subvol = match unique_sibling(
            parent_path(&plan.current_subvol),
            &format!(
                "{}.restore-failed",
                Path::new(&plan.current_subvol)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("subvolume")
            ),
            backend,
        ) {
            Ok(path) => path,
            Err(error) => {
                errors.push(anyhow!(
                    "rollback could not allocate a recovery path for the failed snapshot: {:#}",
                    error
                ));
                return errors;
            }
        };
        if let Err(error) = move_path(backend, &plan.current_subvol, &failed_subvol) {
            errors.push(anyhow!(
                "rollback could not retain failed snapshot at {}: {:#}",
                failed_subvol,
                error
            ));
            return errors;
        }
        if let Err(error) = move_path(backend, &plan.backup_subvol, &plan.current_subvol) {
            errors.push(anyhow!(
                "rollback could not restore original subvolume {} from {}: {:#}",
                plan.current_subvol,
                plan.backup_subvol,
                error
            ));
            return errors;
        }
    } else if state.parent_in_backup {
        // The staged parent never reached current_subvol.  Return its already
        // moved children to the old parent, then put the old parent back.
        if let Err(child_errors) = move_children(
            backend,
            &plan.staged_subvol,
            &plan.backup_subvol,
            &state.children_in_staging,
        ) {
            errors.extend(child_errors);
            return errors;
        }
        if let Err(error) = move_path(backend, &plan.backup_subvol, &plan.current_subvol) {
            errors.push(anyhow!(
                "rollback could not restore original subvolume {} from {}: {:#}",
                plan.current_subvol,
                plan.backup_subvol,
                error
            ));
            return errors;
        }
    } else {
        // The old parent is still live at current_subvol; only child moves
        // need to be reversed.
        if let Err(child_errors) = move_children(
            backend,
            &plan.staged_subvol,
            &plan.current_subvol,
            &state.children_in_staging,
        ) {
            errors.extend(child_errors);
            return errors;
        }
    }

    if state.original_unmounted {
        let mut original_plan = plan.clone();
        if let Some(mount) = &plan.original_mount {
            original_plan.mount_options = mount
                .options
                .split(',')
                .filter(|option| !option.starts_with("subvol=") && !option.starts_with("subvolid="))
                .collect::<Vec<_>>()
                .join(",");
        }
        if let Err(error) = mount_target_and_verify(backend, &original_plan, &plan.target) {
            errors.push(anyhow!(
                "rollback could not restore the original mount {}: {:#}",
                plan.mount_point.as_deref().unwrap_or_default(),
                error
            ));
        }
    }

    errors
}

fn move_children(
    backend: &mut dyn RestoreBackend,
    source_parent: &str,
    destination_parent: &str,
    children: &[String],
) -> std::result::Result<(), Vec<anyhow::Error>> {
    let mut errors = Vec::new();
    for child in children {
        if let Err(error) = move_nested_child(backend, source_parent, destination_parent, child) {
            errors.push(anyhow!(
                "rollback could not move nested subvolume {} from {} to {}: {:#}",
                child,
                source_parent,
                destination_parent,
                error
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn combine_transaction_errors(
    operation_error: anyhow::Error,
    cleanup_errors: Vec<anyhow::Error>,
) -> anyhow::Error {
    if cleanup_errors.is_empty() {
        return operation_error;
    }

    let cleanup = cleanup_errors
        .iter()
        .enumerate()
        .map(|(index, error)| format!("  {}. {:#}", index + 1, error))
        .collect::<Vec<_>>()
        .join("\n");
    anyhow!(
        "{:#}\nRollback/cleanup errors:\n{}",
        operation_error,
        cleanup
    )
}

fn read_subvolume_name(backend: &mut dyn RestoreBackend, path: &str) -> Result<String> {
    let output = backend.run("btrfs", &["subvolume", "show", path])?;
    parse_subvolume_show_name(&output)
        .ok_or_else(|| anyhow!("btrfs subvolume show {} did not report a name", path))
}

fn parse_subvolume_show_name(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        line.trim()
            .strip_prefix("Name:")
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
    })
}

fn discover_nested_children(
    backend: &mut dyn RestoreBackend,
    current_subvol: &str,
    target: &str,
) -> Result<(Vec<String>, Vec<String>)> {
    let output = backend.run("btrfs", &["subvolume", "list", "-o", current_subvol])?;
    let descendants = parse_nested_paths(&output, target)?;
    let direct_children = direct_nested_children(descendants.clone());
    Ok((descendants, direct_children))
}

#[cfg(test)]
fn parse_nested_children(output: &str, target: &str) -> Result<Vec<String>> {
    Ok(direct_nested_children(parse_nested_paths(output, target)?))
}

fn parse_nested_paths(output: &str, target: &str) -> Result<Vec<String>> {
    let target = validate_relative_path(target, "target subvolume")?;
    let mut descendants = Vec::new();

    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let path = line
            .split_once(" path ")
            .map(|(_, path)| path.trim().to_string())
            .or_else(|| {
                let mut pieces = line.split_whitespace();
                let path_index = pieces.position(|piece| piece == "path")?;
                Some(
                    line.split_whitespace()
                        .skip(path_index + 1)
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            })
            .ok_or_else(|| anyhow!("Unsupported btrfs subvolume list line: {}", line))?;
        let path = validate_relative_path(&path, "nested subvolume path")?;

        if path == target {
            continue;
        }
        let prefix = format!("{}/", target);
        let relative = if let Some(relative) = path.strip_prefix(&prefix) {
            relative
        } else {
            return Err(anyhow!(
                "Nested subvolume path {} is not below target {}; refusing restore",
                path,
                target
            ));
        };
        if relative.is_empty() {
            continue;
        }
        descendants.push(relative.to_string());
    }

    descendants.sort_by(|left, right| {
        Path::new(left)
            .components()
            .count()
            .cmp(&Path::new(right).components().count())
            .then_with(|| left.cmp(right))
    });
    descendants.dedup();
    Ok(descendants)
}

fn direct_nested_children(descendants: Vec<String>) -> Vec<String> {
    let mut direct_children = Vec::new();
    for descendant in descendants {
        let is_nested_in_known_child = direct_children.iter().any(|child: &String| {
            descendant
                .strip_prefix(child)
                .is_some_and(|rest| rest.starts_with('/'))
        });
        if !is_nested_in_known_child {
            direct_children.push(descendant);
        }
    }
    direct_children
}

fn validate_mount_identity(
    mount: &MountInfo,
    expected_uuid: &str,
    label: &str,
    expected_subvolume: Option<&str>,
) -> Result<()> {
    if !mount.fstype.eq_ignore_ascii_case("btrfs") {
        bail!(
            "{} has filesystem type '{}', expected btrfs",
            label,
            mount.fstype
        );
    }
    let actual_uuid = mount
        .uuid
        .as_deref()
        .filter(|uuid| !uuid.trim().is_empty())
        .ok_or_else(|| anyhow!("{} does not expose a filesystem UUID", label))?;
    if !actual_uuid.eq_ignore_ascii_case(expected_uuid) {
        bail!(
            "{} has UUID '{}', expected '{}'",
            label,
            actual_uuid,
            expected_uuid
        );
    }

    if let Some(expected_subvolume) = expected_subvolume {
        let actual_subvolume = parse_mount_subvolume(&mount.options).ok_or_else(|| {
            anyhow!(
                "{} does not expose a subvol= mount option; refusing identity-ambiguous restore",
                label
            )
        })?;
        if !subvolume_names_match(&actual_subvolume, expected_subvolume) {
            bail!(
                "{} uses subvolume '{}', expected '{}'",
                label,
                actual_subvolume,
                expected_subvolume
            );
        }
    }
    Ok(())
}

fn parse_mount_subvolume(options: &str) -> Option<String> {
    options.split(',').find_map(|option| {
        option
            .trim()
            .strip_prefix("subvol=")
            .map(|subvolume| subvolume.trim_start_matches('/').to_string())
            .filter(|subvolume| !subvolume.is_empty())
    })
}

fn subvolume_names_match(actual: &str, expected: &str) -> bool {
    actual.trim().trim_start_matches('/') == expected.trim().trim_start_matches('/')
}

fn validate_snapshot_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        bail!("Invalid snapshot name '{}'", name);
    }
    Ok(())
}

fn validate_relative_path(path: &str, label: &str) -> Result<String> {
    let path = Path::new(path);
    if path.is_absolute() {
        bail!("{} must be relative: {}", label, path.display());
    }

    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => components.push(part.to_string_lossy().to_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("{} contains an unsafe path: {}", label, path.display())
            }
        }
    }
    if components.is_empty() {
        bail!("{} is empty", label);
    }
    Ok(components.join("/"))
}

fn join_path(parent: &str, relative: &str) -> String {
    Path::new(parent)
        .join(relative)
        .to_string_lossy()
        .to_string()
}

fn parent_path(path: &str) -> &str {
    Path::new(path)
        .parent()
        .and_then(|parent| parent.to_str())
        .unwrap_or(path)
}

fn unique_sibling(parent: &str, stem: &str, backend: &dyn RestoreBackend) -> Result<String> {
    let stem_path = join_path(parent, stem);
    if !backend.path_exists(&stem_path) {
        return Ok(stem_path);
    }

    for suffix in 1..10_000 {
        let candidate = format!("{}.{}", stem_path, suffix);
        if !backend.path_exists(&candidate) {
            return Ok(candidate);
        }
    }
    bail!(
        "Could not allocate a unique recovery path beside {}",
        stem_path
    )
}

fn resolve_snapshot_target(
    config: &Config,
    subvol_base: &str,
    selected: &str,
) -> Result<(String, Option<String>)> {
    if let Some((subvol, snapshot)) = config
        .subvolumes
        .snapshot_only
        .iter()
        .find(|(_, snapshot)| snapshot.snapshot_name == subvol_base)
    {
        return Ok((subvol.clone(), Some(snapshot.source.clone())));
    }

    if let Some((subvol, _)) = config
        .subvolumes
        .backup
        .iter()
        .find(|(subvol, _)| subvol.trim_start_matches('@') == subvol_base)
    {
        return Ok((subvol.clone(), None));
    }

    bail!(
        "Snapshot '{}' does not match a configured backup or snapshot-only subvolume",
        selected
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BackupSubvol, Config, Distribution, SnapshotSubvol};
    use std::collections::{HashMap, HashSet};
    use tempfile::tempdir;

    #[test]
    fn restore_does_not_treat_distinct_at_prefixed_subvolumes_as_identical() {
        assert!(!subvolume_names_match("home", "@home"));
        assert!(subvolume_names_match("/@home", "@home"));
        assert!(
            parse_nested_children("ID 257 gen 1 top level 5 path home/.cache", "@home").is_err()
        );
    }

    #[test]
    fn restore_rejects_source_that_contains_recovery_storage() {
        let config = test_config();
        let mut backend = FakeBackend::new(&config);
        let request = test_request(Some("/fixture".into()));
        assert!(preflight_restore(&mut backend, &config, Distribution::Arch, &request).is_err());
        assert!(backend.commands.is_empty());
    }

    #[derive(Debug)]
    struct FakeBackend {
        available_commands: HashSet<String>,
        directories: HashSet<String>,
        files: HashSet<String>,
        subvolume_names: HashMap<String, String>,
        mounts: HashMap<String, MountInfo>,
        nested_output: String,
        commands: Vec<(String, Vec<String>)>,
        fail_snapshot: bool,
        fail_stage_switch_once: bool,
        fail_umount: bool,
        fail_mount_once: bool,
        fail_rsync: bool,
    }

    impl FakeBackend {
        fn new(config: &Config) -> Self {
            let mut available_commands = HashSet::new();
            available_commands.insert("btrfs".to_string());
            available_commands.insert("rsync".to_string());
            available_commands.insert("mv".to_string());
            available_commands.insert("mount".to_string());
            available_commands.insert("umount".to_string());

            let base = config.mount.base.clone();
            let current = join_path(&base, "@home");
            let snapshot = join_path(&base, ".snapshots/home.20260915");
            let home_mount = "/fixture/home".to_string();
            let cache = join_path(&current, ".cache");

            let mut directories = HashSet::from([
                base.clone(),
                current.clone(),
                snapshot.clone(),
                cache.clone(),
            ]);
            directories.insert(home_mount.clone());

            let mut subvolume_names = HashMap::new();
            subvolume_names.insert(current, "@home".to_string());
            subvolume_names.insert(snapshot, "home.20260915".to_string());

            let mounts = HashMap::from([
                (
                    base.clone(),
                    MountInfo {
                        target: base.clone(),
                        source: "/dev/fake".to_string(),
                        fstype: "btrfs".to_string(),
                        options: "rw,subvolid=5".to_string(),
                        uuid: Some("test-uuid".to_string()),
                    },
                ),
                (
                    home_mount.clone(),
                    MountInfo {
                        target: home_mount,
                        source: "/dev/fake".to_string(),
                        fstype: "btrfs".to_string(),
                        options: "rw,subvol=@home".to_string(),
                        uuid: Some("test-uuid".to_string()),
                    },
                ),
            ]);

            Self {
                available_commands,
                directories,
                files: HashSet::new(),
                subvolume_names,
                mounts,
                nested_output: "ID 257 gen 10 top level 5 path @home/.cache\n".to_string(),
                commands: Vec::new(),
                fail_snapshot: false,
                fail_stage_switch_once: false,
                fail_umount: false,
                fail_mount_once: false,
                fail_rsync: false,
            }
        }

        fn record(&mut self, command: &str, args: &[&str]) {
            self.commands.push((
                command.to_string(),
                args.iter().map(|arg| (*arg).to_string()).collect(),
            ));
        }

        fn rename_tree(&mut self, source: &str, destination: &str) {
            let source_prefix = format!("{}/", source);
            let remap = |path: &str| {
                if path == source {
                    Some(destination.to_string())
                } else {
                    path.strip_prefix(&source_prefix)
                        .map(|rest| format!("{}/{}", destination, rest))
                }
            };

            let old_directories = std::mem::take(&mut self.directories);
            self.directories = old_directories
                .into_iter()
                .map(|path| remap(&path).unwrap_or(path))
                .collect();

            let old_files = std::mem::take(&mut self.files);
            self.files = old_files
                .into_iter()
                .map(|path| remap(&path).unwrap_or(path))
                .collect();

            let old_names = std::mem::take(&mut self.subvolume_names);
            self.subvolume_names = old_names
                .into_iter()
                .map(|(path, name)| (remap(&path).unwrap_or(path), name))
                .collect();
        }
    }

    impl RestoreBackend for FakeBackend {
        fn command_exists(&self, command: &str) -> bool {
            self.available_commands.contains(command)
        }

        fn path_exists(&self, path: &str) -> bool {
            self.directories.contains(path) || self.files.contains(path)
        }

        fn path_is_directory(&self, path: &str) -> bool {
            self.directories.contains(path)
        }

        fn directory_is_empty(&self, path: &str) -> Result<bool> {
            if !self.path_is_directory(path) {
                return Ok(false);
            }
            let prefix = format!("{}/", path);
            Ok(!self
                .directories
                .iter()
                .any(|entry| entry.starts_with(&prefix))
                && !self.files.iter().any(|entry| entry.starts_with(&prefix)))
        }

        fn remove_empty_directory(&mut self, path: &str) -> Result<()> {
            if !self.directory_is_empty(path)? {
                bail!("fake directory {} is not empty", path);
            }
            self.directories.remove(path);
            Ok(())
        }

        fn create_directory(&mut self, path: &str) -> Result<()> {
            let mut current = Path::new(path).to_path_buf();
            let mut missing = Vec::new();
            while !self.path_exists(&current.to_string_lossy()) {
                missing.push(current.clone());
                if !current.pop() {
                    break;
                }
            }
            for directory in missing.into_iter().rev() {
                self.directories
                    .insert(directory.to_string_lossy().to_string());
            }
            Ok(())
        }

        fn find_mount(&mut self, path: &str) -> Result<Option<MountInfo>> {
            Ok(self.mounts.get(path).cloned())
        }

        fn run(&mut self, command: &str, args: &[&str]) -> Result<String> {
            self.record(command, args);
            match command {
                "btrfs" if args.starts_with(&["subvolume", "show"]) => {
                    let path = args
                        .get(2)
                        .ok_or_else(|| anyhow!("fake subvolume show missing path"))?;
                    let name = self
                        .subvolume_names
                        .get(*path)
                        .ok_or_else(|| anyhow!("fake subvolume {} not found", path))?;
                    Ok(format!("Name: {}\n", name))
                }
                "btrfs" if args.starts_with(&["subvolume", "list"]) => {
                    Ok(self.nested_output.clone())
                }
                "btrfs" if args.starts_with(&["subvolume", "snapshot"]) => {
                    if self.fail_snapshot {
                        bail!("fake snapshot failure");
                    }
                    let destination = args
                        .get(3)
                        .ok_or_else(|| anyhow!("fake snapshot missing destination"))?;
                    self.directories.insert((*destination).to_string());
                    self.subvolume_names
                        .insert((*destination).to_string(), "home.20260915".to_string());
                    Ok(String::new())
                }
                "mv" => {
                    let source = args
                        .get(args.len().saturating_sub(2))
                        .ok_or_else(|| anyhow!("fake mv missing source"))?;
                    let destination = args
                        .last()
                        .ok_or_else(|| anyhow!("fake mv missing destination"))?;
                    if !self.path_exists(source) {
                        bail!("fake mv source {} missing", source);
                    }
                    if self.fail_stage_switch_once
                        && source.contains(".wslarc-restore-staging-")
                        && destination.ends_with("/@home")
                    {
                        self.fail_stage_switch_once = false;
                        bail!("fake staged parent switch failure");
                    }
                    self.rename_tree(source, destination);
                    Ok(String::new())
                }
                "umount" => {
                    let mount_point = args
                        .first()
                        .ok_or_else(|| anyhow!("fake umount missing path"))?;
                    if self.fail_umount {
                        bail!("fake busy mount");
                    }
                    self.mounts.remove(*mount_point);
                    Ok(String::new())
                }
                "mount" => {
                    if self.fail_mount_once {
                        self.fail_mount_once = false;
                        bail!("fake mount failure");
                    }
                    let mount_point = args
                        .last()
                        .ok_or_else(|| anyhow!("fake mount missing path"))?;
                    let options = args
                        .get(3)
                        .ok_or_else(|| anyhow!("fake mount missing options"))?;
                    self.mounts.insert(
                        (*mount_point).to_string(),
                        MountInfo {
                            target: (*mount_point).to_string(),
                            source: "UUID=test-uuid".to_string(),
                            fstype: "btrfs".to_string(),
                            options: (*options).to_string(),
                            uuid: Some("test-uuid".to_string()),
                        },
                    );
                    Ok(String::new())
                }
                _ => Ok(String::new()),
            }
        }

        fn run_stream(&mut self, command: &str, args: &[&str]) -> Result<()> {
            self.record(command, args);
            if command == "rsync" && self.fail_rsync {
                bail!("fake rsync failure");
            }
            Ok(())
        }
    }

    fn test_config() -> Config {
        let mut config = Config::for_distribution(Distribution::Arch);
        config.mount.base = "/fixture/btrfs".to_string();
        config.uuid = Some("test-uuid".to_string());
        config.subvolumes.backup.clear();
        config.subvolumes.backup.insert(
            "@home".to_string(),
            BackupSubvol::Simple("/fixture/home".to_string()),
        );
        config.subvolumes.snapshot_only.clear();
        config.subvolumes.exclude.parent = "@home".to_string();
        config.subvolumes.exclude.paths = vec![".cache".to_string()];
        config
    }

    fn test_request(live_source: Option<String>) -> RestoreRequest {
        RestoreRequest {
            base: "/fixture/btrfs".to_string(),
            target: "@home".to_string(),
            source_snapshot: "/fixture/btrfs/.snapshots/home.20260915".to_string(),
            source_name: "home.20260915".to_string(),
            mount_point: Some("/fixture/home".to_string()),
            live_source,
        }
    }

    fn preflight_test_plan(backend: &mut FakeBackend, config: &Config) -> RestorePlan {
        preflight_restore(backend, config, Distribution::Arch, &test_request(None)).unwrap()
    }

    #[test]
    fn resolve_snapshot_target_uses_custom_backup_key() {
        let mut config = Config::for_distribution(Distribution::Arch);
        let usr = config.subvolumes.backup.remove("@usr").unwrap();
        config
            .subvolumes
            .backup
            .insert("system_usr".to_string(), usr);

        let target = resolve_snapshot_target(&config, "system_usr", "system_usr.20260915").unwrap();

        assert_eq!(target, ("system_usr".to_string(), None));
    }

    #[test]
    fn resolve_snapshot_target_returns_snapshot_only_source() {
        let mut config = Config::for_distribution(Distribution::Arch);
        config.subvolumes.snapshot_only.insert(
            "@state".to_string(),
            SnapshotSubvol {
                snapshot_name: "state".to_string(),
                source: "/var/lib/state".to_string(),
            },
        );

        let target = resolve_snapshot_target(&config, "state", "state.20260915").unwrap();

        assert_eq!(
            target,
            ("@state".to_string(), Some("/var/lib/state".to_string()))
        );
    }

    #[test]
    fn resolve_snapshot_target_rejects_unknown_snapshot() {
        let config = Config::for_distribution(Distribution::Arch);

        assert!(resolve_snapshot_target(&config, "unknown", "unknown.20260915").is_err());
    }

    #[test]
    fn resolve_snapshot_target_matches_backup_key_without_at_prefix() {
        let mut config = Config::for_distribution(Distribution::Arch);
        config.subvolumes.backup.insert(
            "@data".to_string(),
            BackupSubvol::Simple("/data".to_string()),
        );

        let target = resolve_snapshot_target(&config, "data", "data.20260915").unwrap();

        assert_eq!(target, ("@data".to_string(), None));
    }

    #[test]
    fn missing_dependency_is_rejected_before_host_inspection() {
        let config = test_config();
        let mut backend = FakeBackend::new(&config);
        backend.available_commands.remove("btrfs");

        let error = preflight_restore(
            &mut backend,
            &config,
            Distribution::Arch,
            &test_request(None),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("btrfs-progs"));
        assert!(backend.commands.is_empty());
    }

    #[test]
    fn live_nested_mount_is_rejected_before_staging() {
        let config = test_config();
        let mut backend = FakeBackend::new(&config);
        backend.mounts.insert(
            "/fixture/home/.cache".to_string(),
            MountInfo {
                target: "/fixture/home/.cache".to_string(),
                source: "/dev/fake".to_string(),
                fstype: "btrfs".to_string(),
                options: "rw,subvol=@cache".to_string(),
                uuid: Some("test-uuid".to_string()),
            },
        );

        let error = preflight_restore(
            &mut backend,
            &config,
            Distribution::Arch,
            &test_request(None),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("Nested subvolume mount"));
        assert!(!backend
            .commands
            .iter()
            .any(|(command, args)| command == "btrfs"
                && args.first().map(String::as_str) == Some("subvolume")
                && args.get(1).map(String::as_str) == Some("snapshot")));
    }

    #[test]
    fn stage_failure_happens_before_unmount_or_cutover() {
        let config = test_config();
        let mut backend = FakeBackend::new(&config);
        backend.fail_snapshot = true;
        let plan = preflight_test_plan(&mut backend, &config);

        let error = execute_restore(&mut backend, &config, &plan)
            .unwrap_err()
            .to_string();

        assert!(error.contains("no unmount or cutover was attempted"));
        assert!(!backend
            .commands
            .iter()
            .any(|(command, _)| command == "umount" || command == "mv"));
        assert!(backend.mounts.contains_key("/fixture/home"));
    }

    #[test]
    fn busy_unmount_fails_without_lazy_cutover() {
        let config = test_config();
        let mut backend = FakeBackend::new(&config);
        backend.fail_umount = true;
        let plan = preflight_test_plan(&mut backend, &config);

        let error = execute_restore(&mut backend, &config, &plan)
            .unwrap_err()
            .to_string();

        assert!(error.contains("refusing lazy unmount"));
        assert!(backend.mounts.contains_key("/fixture/home"));
        assert!(!backend.commands.iter().any(|(command, _)| command == "mv"));
    }

    #[test]
    fn remount_failure_rolls_back_parent_and_nested_child() {
        let config = test_config();
        let mut backend = FakeBackend::new(&config);
        backend.fail_mount_once = true;
        let plan = preflight_test_plan(&mut backend, &config);

        let error = execute_restore(&mut backend, &config, &plan)
            .unwrap_err()
            .to_string();

        assert!(error.contains("Failed to remount"));
        assert!(backend.path_exists("/fixture/btrfs/@home"));
        assert!(backend.path_exists("/fixture/btrfs/@home/.cache"));
        assert!(!backend.path_exists(&plan.backup_subvol));
        assert!(backend.mounts.contains_key("/fixture/home"));
        assert!(
            backend
                .commands
                .iter()
                .filter(|(command, _)| command == "mount")
                .count()
                >= 2
        );
    }

    #[test]
    fn rollback_restores_original_mount_options() {
        let config = test_config();
        let mut backend = FakeBackend::new(&config);
        backend.mounts.get_mut("/fixture/home").unwrap().options = "ro,subvol=@home".into();
        backend.fail_mount_once = true;
        let plan = preflight_test_plan(&mut backend, &config);
        assert!(execute_restore(&mut backend, &config, &plan).is_err());
        assert!(backend.mounts["/fixture/home"]
            .options
            .split(',')
            .any(|option| option == "ro"));
    }

    #[test]
    fn switch_failure_rolls_back_parent_and_nested_child() {
        let config = test_config();
        let mut backend = FakeBackend::new(&config);
        backend.fail_stage_switch_once = true;
        let plan = preflight_test_plan(&mut backend, &config);

        let error = execute_restore(&mut backend, &config, &plan)
            .unwrap_err()
            .to_string();

        assert!(error.contains("Failed to switch staged snapshot"));
        assert!(backend.path_exists("/fixture/btrfs/@home"));
        assert!(backend.path_exists("/fixture/btrfs/@home/.cache"));
        assert!(!backend.path_exists(&plan.backup_subvol));
        assert!(backend.mounts.contains_key("/fixture/home"));
    }

    #[test]
    fn nested_children_move_into_new_parent_and_prior_backup_survives() {
        let config = test_config();
        let mut backend = FakeBackend::new(&config);
        let prior_backup = "/fixture/btrfs/@home.restore-backup".to_string();
        backend.directories.insert(prior_backup.clone());
        backend.files.insert(format!("{}/sentinel", prior_backup));

        let plan = preflight_test_plan(&mut backend, &config);
        assert_ne!(plan.backup_subvol, prior_backup);

        execute_restore(&mut backend, &config, &plan).unwrap();

        assert!(backend.path_exists("/fixture/btrfs/@home/.cache"));
        assert!(backend.path_exists(&prior_backup));
        assert!(backend.path_exists(&format!("{}/sentinel", prior_backup)));
        assert!(backend.path_exists(&plan.backup_subvol));
    }

    #[test]
    fn snapshot_only_rsync_failure_retains_recovery_source_without_rollback_claim() {
        let mut config = test_config();
        config.subvolumes.backup.clear();
        config.subvolumes.snapshot_only.insert(
            "@home".to_string(),
            SnapshotSubvol {
                snapshot_name: "home".to_string(),
                source: "/fixture/live".to_string(),
            },
        );
        let mut backend = FakeBackend::new(&config);
        backend.mounts.remove("/fixture/home");
        backend.fail_rsync = true;
        let request = RestoreRequest {
            mount_point: None,
            live_source: Some("/fixture/live".to_string()),
            ..test_request(None)
        };
        let plan = preflight_restore(&mut backend, &config, Distribution::Arch, &request).unwrap();

        let error = execute_restore(&mut backend, &config, &plan)
            .unwrap_err()
            .to_string();

        assert!(error.contains("do not treat the live source as rolled back"));
        assert!(backend.path_exists(&plan.backup_subvol));
    }

    #[test]
    fn parse_nested_children_keeps_only_direct_descendants() {
        let output = "\
ID 257 gen 10 top level 5 path @home/.cache\n\
ID 258 gen 11 top level 257 path @home/.cache/tooling\n\
ID 259 gen 12 top level 5 path @home/.local\n";

        assert_eq!(
            parse_nested_children(output, "@home").unwrap(),
            vec![".cache".to_string(), ".local".to_string()]
        );
    }

    #[test]
    fn unique_recovery_path_uses_temp_fixture_without_overwriting_backup() {
        let fixture = tempdir().unwrap();
        let backup = fixture.path().join("@home.restore-backup");
        std::fs::create_dir(&backup).unwrap();
        std::fs::write(backup.join("sentinel"), "keep").unwrap();

        let backend = SystemRestoreBackend;
        let selected = unique_sibling(
            fixture.path().to_str().unwrap(),
            "@home.restore-backup",
            &backend,
        )
        .unwrap();

        assert_eq!(
            selected,
            fixture
                .path()
                .join("@home.restore-backup.1")
                .to_string_lossy()
        );
        assert_eq!(
            std::fs::read_to_string(backup.join("sentinel")).unwrap(),
            "keep"
        );
    }
}

"""Run single-factor regressions on disposable copies, never mutate the worktree."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[2]
CASES = [
    (
        "binary-source-deleted-before-copy",
        "src/utils/storage.rs",
        "    let mut source_file = File::open(source)?;",
        "    if targets.iter().any(|target| target == source) {\n"
        "        fs::remove_file(source)?;\n"
        "    }\n"
        "    let mut source_file = File::open(source)?;",
        "utils::storage::tests::binary_install_preserves_aliased_source_for_all_copies",
    ),
    (
        "binary-destination-deleted-before-staging",
        "src/utils/storage.rs",
        "        let mut staged = tempfile::NamedTempFile::new_in(parent)?;",
        "        let _ = fs::remove_file(target);\n"
        "        let mut staged = tempfile::NamedTempFile::new_in(parent)?;",
        "utils::storage::tests::binary_copy_failure_keeps_old_destination",
    ),
    (
        "preview-persists-configuration",
        "src/commands/snapshot.rs",
        "    if !dry_run {\n        atomic_write(installed_path,",
        "    if true {\n        atomic_write(installed_path,",
        "commands::snapshot::tests::prune_preview_uses_current_config_without_changing_installed_file",
    ),
    (
        "timer-discards-custom-configuration",
        "src/generators/btrbk.rs",
        "crate::generators::invocation::systemd_argument(config_path)",
        'crate::generators::invocation::systemd_argument("/etc/wslarc/config.toml")',
        "generators::btrbk::tests::test_generate_service",
    ),
    (
        "status-omits-managed-ext4-unit",
        "src/commands/status.rs",
        "        units.push(ext4_sync::ext4_mount_unit_filename(config));",
        "        // Ablation: omit the managed ext4 unit.",
        "commands::status::tests::status_includes_ext4_unit_only_when_managed",
    ),
    (
        "arch-archive-assumes-host-architecture",
        "src/commands/hook_sync_systemd.rs",
        "            if metadata == *package {",
        "            if metadata == *package && metadata.architecture == std::env::consts::ARCH {",
        "commands::hook_sync_systemd::tests::archive_mapping_uses_real_name_version_and_architecture",
    ),
    (
        "apt-clears-pending-before-sync",
        "src/commands/hook_sync_systemd.rs",
        "    sync(sync_triggers)?;",
        "    clear_pending(pending_path)?;\n    sync(sync_triggers)?;",
        "commands::hook_sync_systemd::tests::apt_upgrade_failure_retains_pending_and_previous_state",
    ),
    (
        "debian-rejects-held-installed-packages",
        "src/utils/cli.rs",
        '    if status.split_whitespace().last() != Some("installed") {',
        '    if status != "install ok installed" {',
        "utils::cli::tests::parse_debian_status_version_only_accepts_installed_packages",
    ),
    (
        "dependency-closure-retains-removed-roots",
        "src/generators/ext4_sync.rs",
        "        if !is_installed(&pkg)? {",
        "        if false {",
        "generators::ext4_sync::tests::test_collect_recursive_targets_returns_empty_when_all_roots_removed",
    ),
    (
        "init-forgets-parent-before-nested-creation",
        "src/commands/init.rs",
        "        prepare_seed(mount_point, parent, backup.mount(), &mut progress)?;",
        "        let _ = backup;",
        "commands::init::tests::initialization_seeds_nested_home_and_transfer_sources",
    ),
    (
        "init-omits-transfer-seeding",
        "src/commands/init.rs",
        "    if !dry_run {\n        for (subvol, source) in &seed_sources {",
        "    seed_sources.retain(|(subvol, _)| !cfg.subvolumes.transfer.contains_key(subvol));\n"
        "    if !dry_run {\n        for (subvol, source) in &seed_sources {",
        "commands::init::tests::initialization_seeds_nested_home_and_transfer_sources",
    ),
    (
        "init-accepts-an-ordinary-directory-as-subvolume",
        "src/commands/init.rs",
        '        if runner.run("btrfs", &["subvolume", "show", &path]).is_ok() {',
        "        if true {",
        "commands::init::tests::ordinary_existing_path_is_not_silently_accepted_as_subvolume",
    ),
    (
        "init-preview-writes-seed-progress",
        "src/commands/init.rs",
        "cfg.subvolumes.backup.get(parent).filter(|_| !dry_run)",
        "cfg.subvolumes.backup.get(parent).filter(|_| true)",
        "commands::init::tests::init_dry_run_does_not_create_progress_for_existing_empty_targets",
    ),
    (
        "restore-unmounts-before-staging",
        "src/commands/restore.rs",
        "fn snapshot_to_stage(backend: &mut dyn RestoreBackend, plan: &RestorePlan) -> Result<()> {\n",
        "fn snapshot_to_stage(backend: &mut dyn RestoreBackend, plan: &RestorePlan) -> Result<()> {\n"
        "    if let Some(mount) = plan.mount_point.as_deref() { backend.run(\"umount\", &[mount])?; }\n",
        "commands::restore::tests::stage_failure_happens_before_unmount_or_cutover",
    ),
    (
        "restore-omits-cutover-rollback",
        "src/commands/restore.rs",
        "    let cleanup_errors = rollback_cutover(backend, plan, &state);",
        "    let cleanup_errors = Vec::new();",
        "commands::restore::tests::remount_failure_rolls_back_parent_and_nested_child",
    ),
    (
        "restore-omits-nested-subvolume-moves",
        "src/commands/restore.rs",
        "    for child in &plan.nested_children {\n        if let Err(error) =",
        "    for child in plan.nested_children.iter().take(0) {\n        if let Err(error) =",
        "commands::restore::tests::nested_children_move_into_new_parent_and_prior_backup_survives",
    ),
]


def run_tests(directory, *args):
    env = dict(os.environ, CARGO_TARGET_DIR=str(directory / "target"))
    return subprocess.run(
        ["cargo", "test", "--offline", "--all-targets", *args],
        cwd=directory,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=180,
        check=False,
    )


def main():
    results = []
    with tempfile.TemporaryDirectory(prefix="wslarc-ablation-", dir=Path.home()) as temp:
        scratch = Path(temp)
        shutil.copytree(ROOT / "src", scratch / "src")
        for name in ("Cargo.toml", "Cargo.lock"):
            shutil.copy2(ROOT / name, scratch / name)
        digest = hashlib.sha256()
        for path in sorted((scratch / "src").rglob("*.rs")):
            digest.update(str(path.relative_to(scratch)).encode())
            digest.update(path.read_bytes())
        source_digest = digest.hexdigest()
        baseline = run_tests(scratch)
        if baseline.returncode:
            raise RuntimeError(f"Baseline failed:\n{baseline.stdout}")
        for name, filename, before, after, test in CASES:
            path = scratch / filename
            original = path.read_text()
            if original.count(before) != 1:
                raise RuntimeError(f"{name}: mutation anchor must match exactly once")
            try:
                path.write_text(original.replace(before, after, 1))
                mutant = run_tests(scratch, test, "--", "--exact")
            finally:
                path.write_text(original)
            killed = (
                mutant.returncode == 101
                and "running 1 test" in mutant.stdout
                and f"test {test} ... FAILED" in mutant.stdout
            )
            restored = run_tests(scratch, test, "--", "--exact")
            passed = restored.returncode == 0 and "1 passed" in restored.stdout
            results.append(
                dict(case=name, mutant_rejected=killed, restored_passed=passed, test=test)
            )
            if not killed or not passed:
                raise RuntimeError(
                    f"{name}: invalid experiment\nMUTANT:\n{mutant.stdout}"
                    f"\nRESTORED:\n{restored.stdout}"
                )
    print(json.dumps({
        "source_sha256": source_digest,
        "baseline_passed": True,
        "cases": results,
    }, indent=2))


if __name__ == "__main__":
    main()

"""Initialize the Btrfs VHDX and synchronize configured sources."""

from __future__ import annotations

import shlex
import time
from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path

from wslarc.config import Config, Distribution
from wslarc.generators.invocation import shell_argument
from wslarc.utils.block import (
    Dependency,
    ensure_dependencies_for,
    find_btrfs_device_by_label,
    list_block_device_names,
    read_block_device,
)
from wslarc.utils.prompt import (
    confirm,
    confirm_or_yes,
    info,
    input_default,
    kv,
    section,
    success,
    warn,
)
from wslarc.utils.shell import CommandRunner


class SyncKind(StrEnum):
    BACKUP = "backup"
    SNAPSHOT_ONLY = "snapshot-only"
    TRANSFER = "transfer"


class TargetState(StrEnum):
    EMPTY = "empty"
    NON_EMPTY = "non-empty"
    UNKNOWN = "not inspected (dry-run)"


@dataclass
class SyncPlanEntry:
    subvol: str
    source: str
    target: str
    kind: SyncKind
    excluded_paths: list[str]
    state: TargetState
    copy: bool
    reason: str | None = None


def allows_noninteractive_confirmation(config_path: str, yes: bool) -> bool:
    return yes and Path(config_path).exists()


def _check_runtime_dependencies(config: Config, distribution: Distribution) -> None:
    dependencies = [
        Dependency("btrfs-progs", ("mkfs.btrfs", "btrfs")),
        Dependency("rsync", ("rsync",)),
    ]
    if any(item.nodatacow for item in config.subvolumes.transfer.values()):
        dependencies.append(Dependency("e2fsprogs", ("chattr",)))
    ensure_dependencies_for(dependencies, distribution)


def _collect_config(config: Config, distribution: Distribution) -> Config:
    result = config
    section("Distribution")
    print(f"  Detected distribution: {distribution.display_name}")
    section("User Configuration")
    result.set_user_unexpanded(input_default("Target Linux username", result.user.name))
    section("VHDX Configuration")
    result.vhdx.path = input_default("VHDX path (Windows, full path)", result.vhdx.path)
    result.vhdx.label = input_default("Btrfs label", result.vhdx.label)
    section("Mount Configuration")
    result.mount.base = input_default("Mount base", result.mount.base)
    print("  First initialization skips /home and /nix; existing configs use target-state checks")
    return result


def _ensure_user(config: Config, runner: CommandRunner) -> None:
    result = runner.run("id", [config.get_user()], check=False)
    if result.strip():
        success(f"User '{config.get_user()}' already exists")
        return
    args = [*config.user.options.split(), config.get_user()]
    runner.run("useradd", args)
    success(f"User '{config.get_user()}' created")


def _mount_vhdx(config: Config, runner: CommandRunner) -> str:
    existing = find_btrfs_device_by_label(config.vhdx.label)
    if existing:
        return existing
    before = set(list_block_device_names())
    path = config.vhdx.path.replace("/", "\\")
    runner.run("/mnt/c/Windows/System32/wsl.exe", ["--mount", "--vhd", path, "--bare"])
    time.sleep(0.5)
    after = list_block_device_names()
    new_devices = [item for item in after if item not in before]
    if not new_devices:
        raise RuntimeError("could not find new device after mounting VHDX")
    return f"/dev/{new_devices[0]}"


def _format_btrfs(config: Config, device: str, yes: bool, runner: CommandRunner) -> None:
    if runner.dry_run:
        info("[dry-run] Would format as Btrfs if needed")
        return
    block = read_block_device(device)
    if block and block.fstype == "btrfs":
        if block.label == config.vhdx.label:
            return
        warn(f"Device label is {block.label or '<empty>'!r}; expected {config.vhdx.label!r}")
        if not confirm_or_yes("Continue with this device anyway?", False, yes):
            raise RuntimeError("aborted due to label mismatch")
        if block.label:
            config.vhdx.label = block.label
        return
    runner.run("mkfs.btrfs", ["-L", config.vhdx.label, device])


def _get_uuid(device: str, runner: CommandRunner) -> str:
    value = runner.run("blkid", ["-s", "UUID", "-o", "value", device]).strip()
    if not value:
        raise RuntimeError(f"could not get UUID for {device}")
    return value


def _create_subvolume(path: Path, runner: CommandRunner, dry_run: bool) -> None:
    if not dry_run and path.exists():
        try:
            runner.run("btrfs", ["subvolume", "show", str(path)])
            info(f"{path.name} (pre-existing subvolume, skipped)")
            return
        except Exception as exc:
            raise RuntimeError(f"refusing ordinary existing path for subvolume {path}") from exc
    runner.run("btrfs", ["subvolume", "create", str(path)])


def inspect_target(target: Path, ignored_children: list[str], dry_run: bool) -> TargetState:
    if dry_run:
        return TargetState.UNKNOWN
    if not target.exists():
        return TargetState.EMPTY
    ignored = {item.strip("/").split("/", 1)[0] for item in ignored_children}
    try:
        return (
            TargetState.EMPTY
            if all(item.name in ignored for item in target.iterdir())
            else TargetState.NON_EMPTY
        )
    except OSError as exc:
        raise RuntimeError(f"failed to inspect sync target {target}: {exc}") from exc


def _make_plan_entry(
    mount_point: Path,
    subvol: str,
    source: str,
    kind: SyncKind,
    excluded_paths: list[str],
    requested: bool,
    requested_reason: str | None,
    ignored_children: list[str],
    allow_nonempty: bool,
    dry_run: bool,
) -> SyncPlanEntry:
    target = mount_point / subvol
    state = inspect_target(target, ignored_children, dry_run)
    source_exists = dry_run or Path(source).exists()
    if not requested:
        copy, reason = False, requested_reason
    elif not source_exists:
        copy, reason = False, f"source does not exist: {source}"
    elif (
        allow_nonempty
        or kind is SyncKind.SNAPSHOT_ONLY
        or state
        in {
            TargetState.EMPTY,
            TargetState.UNKNOWN,
        }
    ):
        copy, reason = True, None
    else:
        copy, reason = False, "target is non-empty; skipped for an existing configuration"
    return SyncPlanEntry(
        subvol,
        source,
        str(target),
        kind,
        excluded_paths,
        state,
        copy,
        reason,
    )


def build_sync_plan(
    config: Config, mount_point: str, first_init: bool, dry_run: bool
) -> list[SyncPlanEntry]:
    base = Path(mount_point)
    parent = config.subvolumes.exclude.parent
    home_mount = f"/home/{config.get_user()}"
    plan: list[SyncPlanEntry] = []
    if first_init:
        for subvol, backup in config.subvolumes.backup.items():
            enabled = subvol != parent and backup.mount not in {home_mount, "/nix"}
            reason = (
                None
                if enabled
                else (
                    "disabled by first-init policy for /nix"
                    if backup.mount == "/nix"
                    else "disabled by first-init policy for the home directory"
                )
            )
            plan.append(
                _make_plan_entry(
                    base,
                    subvol,
                    backup.mount,
                    SyncKind.BACKUP,
                    config.subvolumes.exclude.paths if subvol == parent else [],
                    enabled,
                    reason,
                    [],
                    True,
                    dry_run,
                )
            )
    else:
        if parent in config.subvolumes.backup:
            parent_source = config.subvolumes.backup[parent].mount.rstrip("/")
            for path in config.subvolumes.exclude.paths:
                nested = f"{parent}/{path}"
                plan.append(
                    _make_plan_entry(
                        base,
                        nested,
                        f"{parent_source}/{path.lstrip('/')}",
                        SyncKind.BACKUP,
                        [],
                        True,
                        None,
                        [],
                        False,
                        dry_run,
                    )
                )
        for subvol, backup in config.subvolumes.backup.items():
            plan.append(
                _make_plan_entry(
                    base,
                    subvol,
                    backup.mount,
                    SyncKind.BACKUP,
                    config.subvolumes.exclude.paths if subvol == parent else [],
                    True,
                    None,
                    config.subvolumes.exclude.paths if subvol == parent else [],
                    False,
                    dry_run,
                )
            )
    for subvol, snapshot in config.subvolumes.snapshot_only.items():
        plan.append(
            _make_plan_entry(
                base,
                subvol,
                snapshot.source,
                SyncKind.SNAPSHOT_ONLY,
                [],
                True,
                None,
                [],
                True,
                dry_run,
            )
        )
    for subvol, transfer in config.subvolumes.transfer.items():
        plan.append(
            _make_plan_entry(
                base,
                subvol,
                transfer.mount,
                SyncKind.TRANSFER,
                [],
                True,
                None,
                [],
                first_init,
                dry_run,
            )
        )
    return plan


def show_sync_plan(plan: list[SyncPlanEntry]) -> None:
    print("\nRsync plan")
    if not plan:
        print("  No configured sources")
    for entry in plan:
        if entry.copy:
            print(
                f"  [sync] {entry.kind.value} {entry.source} -> {entry.target} "
                f"(target: {entry.state.value})"
            )
        elif entry.reason:
            print(f"  [skip] {entry.kind.value} {entry.source} -> {entry.target} ({entry.reason})")


def render_resume_command(device: str, pending: list[SyncPlanEntry]) -> str | None:
    pending = [entry for entry in pending if entry.copy]
    if not pending:
        return None
    lines = [
        "set -e",
        "resume_root=$(mktemp -d /tmp/wslarc-resume.XXXXXX)",
        'cleanup() { sudo umount "$resume_root" >/dev/null 2>&1 || true; '
        'rmdir "$resume_root" 2>/dev/null || true; }',
        "trap cleanup EXIT",
        f'sudo mount -o subvolid=5 {shell_argument(device)} "$resume_root"',
    ]
    for entry in pending:
        args = ["sudo", "rsync", "-aAX", "--partial", "--info=progress2"]
        if entry.kind is SyncKind.SNAPSHOT_ONLY:
            args.append("--delete")
        for path in entry.excluded_paths:
            args.extend(["--exclude", f"/{path.strip('/')}/"])
        args.extend([f"{entry.source.rstrip('/')}/", f'"$resume_root"/{entry.subvol.rstrip("/")}/'])
        lines.append(
            " ".join(
                shlex.quote(arg) if not arg.startswith('"$resume_root"') else arg for arg in args
            )
        )
    lines.append(
        "# The temporary subvolume-5 mount is cleaned up automatically when this shell exits."
    )
    return "\n".join(lines) + "\n"


def _run_sync_entry(entry: SyncPlanEntry, runner: CommandRunner) -> None:
    args = ["-aAX", "--partial", "--info=progress2"]
    if entry.kind is SyncKind.SNAPSHOT_ONLY:
        args.append("--delete")
    for path in entry.excluded_paths:
        args.extend(["--exclude", f"/{path.strip('/')}/"])
    args.extend([f"{entry.source.rstrip('/')}/", f"{entry.target.rstrip('/')}/"])
    info(f"Syncing {entry.source} -> {entry.target}")
    runner.run("rsync", args, stream=True)
    success(f"{entry.subvol} synced")


def execute_sync_plan(
    plan: list[SyncPlanEntry],
    device: str,
    first_init: bool,
    runner: CommandRunner,
) -> None:
    remaining = list(plan)
    while remaining:
        entry = remaining[0]
        if first_init and entry.state is TargetState.NON_EMPTY:
            warn(
                f"Target {entry.target} already contains data; it will be updated "
                f"from {entry.source}."
            )
            if not confirm(f"Copy {entry.source} into non-empty target {entry.target}?", False):
                remaining.pop(0)
                continue
        try:
            _run_sync_entry(entry, runner)
        except Exception:
            command = render_resume_command(device, remaining)
            if command:
                warn("Rsync stopped. Run the following command to resume the remaining sync:")
                print(command)
            raise
        remaining.pop(0)


def _create_all_subvolumes(
    config: Config,
    mount_point: str,
    device: str,
    first_init: bool,
    confirm_yes: bool,
    dry_run: bool,
    runner: CommandRunner,
) -> None:
    root = Path(mount_point)
    if not dry_run:
        root.mkdir(parents=True, exist_ok=True)
        runner.run("mount", ["-o", "subvolid=5", device, mount_point])
    try:
        for name in config.subvolumes.backup:
            _create_subvolume(root / name, runner, dry_run)
        for name in config.subvolumes.snapshot_only:
            _create_subvolume(root / name, runner, dry_run)
        parent = config.subvolumes.exclude.parent
        _create_subvolume(root / parent, runner, dry_run)
        for path in config.subvolumes.exclude.paths:
            nested = root / parent / path
            _create_subvolume(nested, runner, dry_run)
            runner.run("chown", [f"{config.get_user()}:{config.get_user()}", str(nested)])
        runner.run("chown", [f"{config.get_user()}:{config.get_user()}", str(root / parent)])
        for name, transfer in config.subvolumes.transfer.items():
            _create_subvolume(root / name, runner, dry_run)
            if transfer.nodatacow:
                runner.run("chattr", ["+C", str(root / name)])
        _create_subvolume(root / config.btrbk.snapshot_dir, runner, dry_run)
        plan = build_sync_plan(config, mount_point, first_init, dry_run)
        show_sync_plan(plan)
        pending = [item for item in plan if item.copy]
        if pending and confirm_or_yes("Execute the rsync plan?", True, confirm_yes):
            execute_sync_plan(pending, device, first_init, runner)
        elif pending:
            print("Synchronization aborted; no rsync was started.")
    finally:
        if not dry_run:
            runner.run("umount", [mount_point], check=False)
            try:
                root.rmdir()
            except OSError:
                pass


def _mount_base(config: Config, device: str, runner: CommandRunner) -> None:
    if Path(config.mount.base).is_mount():
        return
    Path(config.mount.base).mkdir(parents=True, exist_ok=True)
    runner.run("mount", ["-o", config.mount.options, device, config.mount.base])


def run(
    config: Config,
    distribution: Distribution,
    config_path: str,
    yes: bool = False,
    dry_run: bool = False,
) -> None:
    first_init = not Path(config_path).exists()
    confirm_yes = allows_noninteractive_confirmation(config_path, yes)
    if not first_init and config.uuid:
        warn("Configuration already exists with UUID. Re-running will overwrite.")
        if not confirm_or_yes("Continue anyway?", False, confirm_yes):
            return
    cfg = config if confirm_yes else _collect_config(config, distribution)
    if not cfg.vhdx.path or not cfg.user.name:
        raise ValueError("VHDX path and user are required")
    _check_runtime_dependencies(cfg, distribution)
    section("Configuration Summary")
    kv("VHDX", cfg.vhdx.path)
    kv("Label", cfg.vhdx.label)
    kv("Mount base", cfg.mount.base)
    kv("User", cfg.get_user())
    if not confirm_or_yes("Proceed with initialization?", True, confirm_yes):
        print("Aborted.")
        return
    runner = CommandRunner(dry_run=dry_run)
    _ensure_user(cfg, runner)
    device = "<device>" if dry_run else _mount_vhdx(cfg, runner)
    _format_btrfs(cfg, device, confirm_yes, runner)
    uuid = "<uuid>" if dry_run else _get_uuid(device, runner)
    cfg.uuid = uuid
    _create_all_subvolumes(
        cfg.resolve_variables(),
        "/mnt/btrfs-setup",
        device,
        first_init,
        confirm_yes,
        dry_run,
        runner,
    )
    if dry_run:
        info(f"[dry-run] Would save to {config_path}")
    else:
        cfg.save(config_path)
    _mount_base(cfg.resolve_variables(), device, runner)
    success("Initialization complete")

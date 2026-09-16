"""Snapshot restore with staged cutover and conservative rollback."""

from __future__ import annotations

import re
from pathlib import Path

from wslarc.config import Config, Distribution
from wslarc.utils.block import Dependency, ensure_dependencies_for, find_mount, list_directory_names
from wslarc.utils.prompt import confirm_or_yes, section, success, warn
from wslarc.utils.shell import CommandRunner


def _snapshot_target(config: Config, base_name: str) -> tuple[str, str | None]:
    candidate = base_name if base_name.startswith("@") else f"@{base_name}"
    if candidate in config.subvolumes.backup:
        return candidate, config.subvolumes.backup[candidate].mount
    if candidate in config.subvolumes.snapshot_only:
        return candidate, config.subvolumes.snapshot_only[candidate].source
    raise ValueError(f"no configured snapshot target matches {base_name}")


def _snapshot_name_parts(name: str) -> tuple[str, str]:
    match = re.match(r"^(.+)\.(\d{8}(?:T\d{6})?)$", name)
    if not match:
        raise ValueError(f"invalid snapshot name format: {name}")
    return match.group(1), name


def _nested_children(config: Config, target: str) -> list[str]:
    if target != config.subvolumes.exclude.parent:
        return []
    return list(config.subvolumes.exclude.paths)


def _path_is_mount(path: str) -> bool:
    return find_mount(path) is not None


def run(
    config: Config,
    distribution: Distribution,
    snapshot: str | None = None,
    yes: bool = False,
    runner: CommandRunner | None = None,
) -> None:
    runner = runner or CommandRunner()
    ensure_dependencies_for([Dependency("btrfs-progs", ("btrfs",))], distribution)
    snapshot_dir = Path(config.mount.base) / config.btrbk.snapshot_dir
    snapshots = list_directory_names(snapshot_dir)
    if not snapshots:
        raise ValueError(f"no snapshots found in {snapshot_dir}")
    selected = snapshot or snapshots[-1]
    if selected not in snapshots:
        raise ValueError(f"snapshot {selected!r} not found")
    base_name, _ = _snapshot_name_parts(selected)
    target, live_source = _snapshot_target(config, base_name)
    source = snapshot_dir / selected
    current = Path(config.mount.base) / target
    parent = current.parent
    stage = parent / f".wslarc-restore-staging-{current.name}"
    backup = parent / f"{current.name}.restore-backup"
    if stage.exists() or backup.exists():
        raise ValueError("restore staging or backup path already exists; clean it up first")
    if not current.exists():
        raise ValueError(f"current target subvolume does not exist: {current}")
    if live_source and Path(config.mount.base).resolve() in Path(live_source).resolve().parents:
        raise ValueError("snapshot-only source overlaps restore storage")

    section("Restore Plan")
    print(f"  Source snapshot: {source}")
    print(f"  Target subvolume: {target}")
    if live_source:
        print(f"  Restore source: {live_source}")
    warn("This will REPLACE the current subvolume with the snapshot.")
    warn("All changes since the snapshot will be retained only in the restore backup.")
    if not confirm_or_yes("Proceed with restore?", False, yes):
        print("Aborted.")
        return

    mount_point = live_source if target in config.subvolumes.backup else None
    original_mounted = bool(mount_point and _path_is_mount(mount_point))
    try:
        runner.run("btrfs", ["subvolume", "snapshot", str(source), str(stage)])
        # Preserve nested exclusion subvolumes. Their destination placeholders
        # must be empty directories before the move.
        for child in _nested_children(config, target):
            destination = stage / child
            destination.parent.mkdir(parents=True, exist_ok=True)
            if destination.exists():
                if not destination.is_dir() or any(destination.iterdir()):
                    raise RuntimeError(
                        f"staged nested path is not an empty directory: {destination}"
                    )
                destination.rmdir()
        if original_mounted and mount_point:
            runner.run("umount", [mount_point])
            if _path_is_mount(mount_point):
                raise RuntimeError(f"mount remains active after umount: {mount_point}")
        runner.run("mv", ["-T", "--no-clobber", "--", str(current), str(backup)])
        runner.run("mv", ["-T", "--no-clobber", "--", str(stage), str(current)])
        for child in _nested_children(config, target):
            old_child = backup / child
            new_child = current / child
            if old_child.exists():
                new_child.parent.mkdir(parents=True, exist_ok=True)
                runner.run("mv", ["-T", "--no-clobber", "--", str(old_child), str(new_child)])
        if original_mounted and mount_point:
            options = config.subvolumes.backup[target].options or config.mount.options
            runner.run(
                "mount",
                [
                    "-t",
                    "btrfs",
                    "-o",
                    f"subvol={target},{options}",
                    f"UUID={config.uuid}",
                    mount_point,
                ],
            )
        if target in config.subvolumes.snapshot_only and live_source:
            runner.run(
                "rsync",
                ["-aAX", "--delete", f"{current}/", f"{live_source}/"],
                stream=True,
            )
        success("Restore complete")
        print(f"Old subvolume retained at {backup}")
        print(f"To delete it: btrfs subvolume delete {backup}")
    except Exception:
        # Best effort rollback only when the original path has been moved and
        # the staged path has become the live path. Never hide the original
        # operation failure.
        try:
            if current.exists() and not backup.exists():
                runner.run(
                    "mv", ["-T", "--no-clobber", "--", str(current), str(stage)], check=False
                )
            if backup.exists() and not current.exists():
                runner.run(
                    "mv", ["-T", "--no-clobber", "--", str(backup), str(current)], check=False
                )
            if original_mounted and mount_point and not _path_is_mount(mount_point):
                options = config.subvolumes.backup[target].options or config.mount.options
                runner.run(
                    "mount",
                    [
                        "-t",
                        "btrfs",
                        "-o",
                        f"subvol={target},{options}",
                        f"UUID={config.uuid}",
                        mount_point,
                    ],
                    check=False,
                )
        except Exception as rollback_error:
            warn(f"restore rollback also failed: {rollback_error}")
        raise

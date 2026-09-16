"""Snapshot, list, and prune operations using btrbk."""

from __future__ import annotations

import tempfile
from pathlib import Path
from typing import Any

from wslarc.config import Config, Distribution
from wslarc.generators import btrbk
from wslarc.utils.block import Dependency, ensure_dependencies_for, list_directory_names
from wslarc.utils.prompt import confirm_or_yes, info, success, warn
from wslarc.utils.shell import CommandRunner

BTRBK_CONF = Path("/etc/btrbk/btrbk.conf")


def _temporary_config(config: Config) -> Any:
    file = tempfile.NamedTemporaryFile(
        mode="w", prefix="wslarc-btrbk-", suffix=".conf", delete=True
    )
    file.write(btrbk.generate_config(config))
    file.flush()
    return file


def _verify_volume(config: Config) -> None:
    if not config.uuid:
        raise ValueError("UUID not set; run wslarc init first")


def _sync_snapshot_only(config: Config, runner: CommandRunner) -> None:
    for subvol, snapshot in config.subvolumes.snapshot_only.items():
        info(f"Syncing {snapshot.source} to {subvol}...")
        runner.run(
            "rsync",
            ["-aAX", "--delete", f"{snapshot.source}/", f"{config.mount.base}/{subvol}/"],
            stream=True,
        )
        success(f"{snapshot.source} synced to {subvol}")


def run(
    config: Config,
    distribution: Distribution,
    runner: CommandRunner | None = None,
) -> None:
    runner = runner or CommandRunner()
    dependencies = [Dependency("btrbk", ("btrbk",)), Dependency("btrfs-progs", ("btrfs",))]
    if config.subvolumes.snapshot_only:
        dependencies.append(Dependency("rsync", ("rsync",)))
    ensure_dependencies_for(dependencies, distribution)
    _verify_volume(config)
    with _temporary_config(config) as temporary:
        _sync_snapshot_only(config, runner)
        runner.run("btrbk", ["-c", temporary.name, "-v", "run"], stream=True)
    success("Snapshot and retention run completed")


def list_snapshots(config: Config, distribution: Distribution) -> None:
    ensure_dependencies_for([Dependency("btrbk", ("btrbk",))], distribution)
    _verify_volume(config)
    with _temporary_config(config) as temporary:
        result = CommandRunner().run(
            "btrbk", ["-c", temporary.name, "list", "snapshots"], check=False
        )
    if result.strip():
        print(result)
        return
    snapshot_dir = Path(config.mount.base) / config.btrbk.snapshot_dir
    try:
        entries = list_directory_names(snapshot_dir)
    except OSError:
        print("Snapshot directory not accessible")
        return
    if entries:
        print("\n".join(entries))
    else:
        print("No snapshots found")


def prune(
    config: Config,
    distribution: Distribution,
    yes: bool = False,
    dry_run: bool = False,
    runner: CommandRunner | None = None,
) -> None:
    runner = runner or CommandRunner(dry_run=dry_run)
    ensure_dependencies_for([Dependency("btrbk", ("btrbk",))], distribution)
    _verify_volume(config)
    print(f"Retention minimum: {config.btrbk.preserve_min}")
    print(f"Retention policy: {config.btrbk.preserve}")
    if not dry_run:
        warn("This permanently deletes snapshots outside the configured retention policy.")
        if not confirm_or_yes("Proceed with snapshot cleanup?", False, yes):
            print("Aborted.")
            return
    with _temporary_config(config) as temporary:
        args = ["--dry-run", "-v", "prune"] if dry_run else ["-v", "prune"]
        runner.run("btrbk", ["-c", temporary.name, *args], stream=True)
    success(
        "Prune preview completed; no snapshots were removed"
        if dry_run
        else "Snapshot cleanup completed"
    )

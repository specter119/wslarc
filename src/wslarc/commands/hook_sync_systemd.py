"""Synchronize systemd package files to the hidden ext4 root."""

from __future__ import annotations

import os
import sys
from pathlib import Path

from wslarc.config import Config, Distribution
from wslarc.generators.hooks import (
    APT_PENDING_PACKAGES,
    APT_SYNC_STATE,
    DEBIAN_SYSTEMD_PACKAGES,
    collect_hook_targets,
)
from wslarc.utils.block import find_mount
from wslarc.utils.packages import (
    debian_query_version,
    pacman_query_version,
)
from wslarc.utils.shell import CommandRunner

GUARD = "WSLARC_SYSTEMD_SYNC_IN_PROGRESS"


def _state(packages: list[str], distribution: Distribution) -> str:
    values = []
    for package in packages:
        version = (
            pacman_query_version(package)
            if distribution is Distribution.ARCH
            else debian_query_version(package)
        )
        if version:
            values.append(f"{package}={version}")
    return "\n".join(sorted(values)) + "\n"


def run_apt_pre() -> None:
    Path(APT_PENDING_PACKAGES).parent.mkdir(parents=True, exist_ok=True)
    Path(APT_PENDING_PACKAGES).write_text(sys.stdin.read())


def _sync_tree(config: Config, runner: CommandRunner) -> None:
    ext4_mount = Path(config.ext4_sync.mount_point)
    mount = find_mount(str(ext4_mount))
    if mount is None:
        raise RuntimeError(f"ext4 sync root is not mounted: {ext4_mount}")
    if not Path("/usr").is_dir():
        return
    target = ext4_mount / "usr"
    target.mkdir(parents=True, exist_ok=True)
    runner.run("rsync", ["-aAX", "--delete", "/usr/", f"{target}/"], stream=True)


def run_apt_post(config: Config, distribution: Distribution, dry_run: bool = False) -> None:
    packages = list(DEBIAN_SYSTEMD_PACKAGES)
    current = _state(packages, distribution)
    previous = Path(APT_SYNC_STATE).read_text() if Path(APT_SYNC_STATE).exists() else None
    state_changed = previous is not None and previous != current
    pending = Path(APT_PENDING_PACKAGES).exists()
    if not state_changed and not pending:
        return
    if dry_run:
        print("[dry-run] Would synchronize systemd files to ext4")
        return
    runner = CommandRunner()
    os.environ[GUARD] = "1"
    try:
        _sync_tree(config, runner)
        Path(APT_SYNC_STATE).parent.mkdir(parents=True, exist_ok=True)
        Path(APT_SYNC_STATE).write_text(current)
        Path(APT_PENDING_PACKAGES).unlink(missing_ok=True)
    finally:
        os.environ.pop(GUARD, None)


def run(config: Config, distribution: Distribution, dry_run: bool = False) -> None:
    if os.environ.get(GUARD) == "1":
        return
    if distribution is Distribution.DEBIAN:
        run_apt_post(config, distribution, dry_run)
        return
    targets = collect_hook_targets(distribution)
    if dry_run:
        print(f"[dry-run] Would synchronize packages: {', '.join(targets)}")
        return
    _sync_tree(config, CommandRunner())

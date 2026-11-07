"""Synchronize systemd and btrfs-progs package files to the hidden ext4 root."""

from __future__ import annotations

import os
import shutil
import stat
import sys
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from pathlib import Path

from wslarc.config import Config, Distribution
from wslarc.generators.hooks import (
    APT_PENDING_PACKAGES,
    APT_SYNC_STATE,
    ARCH_PENDING_PACKAGES,
    collect_hook_targets,
)
from wslarc.utils.block import PacmanPackage, find_mount_uuid, is_mountpoint
from wslarc.utils.packages import (
    PACMAN_SYNC_GUARD_ENV,
    debian_file_owned_by_installed_package,
    debian_query_deb_package_name,
    debian_query_files,
    debian_query_version,
    pacman_install_archives,
    pacman_query_archive_package,
    pacman_query_package,
    pacman_query_package_in_root,
    pacman_query_version,
)
from wslarc.utils.prompt import info, success, warn
from wslarc.utils.shell import CommandRunner

GUARD = PACMAN_SYNC_GUARD_ENV
SYNC_MANIFEST = ".wslarc-systemd-sync-manifest"
VersionFor = Callable[[str], str | None]
PackageNameFor = Callable[[str], str | None]
IsOwned = Callable[[str], bool]
TargetPackage = Callable[[str], PacmanPackage | None]
ArchivePackage = Callable[[Path], PacmanPackage | None]


def run(config: Config, distribution: Distribution, dry_run: bool = False) -> None:
    if GUARD in os.environ:
        info("Skipping nested pacman hook during ext4 package installation")
        return
    triggered = read_triggered_packages()
    if distribution is Distribution.ARCH:
        run_arch_with_retry(config, dry_run, triggered)
    else:
        run_with_triggered(config, distribution, dry_run, triggered)


def run_apt_pre(
    package_name_for: PackageNameFor = debian_query_deb_package_name,
    pending_path: Path = Path(APT_PENDING_PACKAGES),
) -> None:
    record_apt_pending(pending_path, sys.stdin.read(), package_name_for)


def run_apt_post(config: Config, distribution: Distribution, dry_run: bool = False) -> None:
    pending_path = Path(APT_PENDING_PACKAGES)
    triggered = read_package_file(pending_path)

    targets = collect_hook_targets(distribution, config.ext4_sync.extra_packages)
    current_state = installed_package_state(distribution, targets)
    previous_state = read_state(Path(APT_SYNC_STATE))
    # A missing state file means this is the first observation, not that the
    # systemd package set changed. Only an existing, different state requires
    # a full refresh (for example after a package removal).
    state_changed = previous_state is not None and previous_state != current_state

    if not triggered:
        if not state_changed:
            info("No APT packages require ext4 synchronization")
            if not dry_run:
                write_state(Path(APT_SYNC_STATE), current_state)
                clear_pending(pending_path)
            return
        info("The installed systemd package set changed; refreshing ext4 sync")

    finish_apt_post(
        pending_path,
        Path(APT_SYNC_STATE),
        triggered,
        current_state,
        previous_state,
        dry_run,
        lambda sync_triggers: run_with_triggered(config, distribution, dry_run, sync_triggers),
    )


def run_arch_with_retry(config: Config, dry_run: bool, triggered: list[str]) -> None:
    pending_path = Path(ARCH_PENDING_PACKAGES)
    pending = read_package_file(pending_path)
    combined = merge_package_lines(pending, triggered)
    retry_state = (
        collect_hook_targets(Distribution.ARCH, config.ext4_sync.extra_packages)
        if not combined
        else combined
    )

    if not dry_run and retry_state:
        write_package_file(pending_path, retry_state)

    run_with_triggered(config, Distribution.ARCH, dry_run, combined)

    if not dry_run:
        clear_pending(pending_path)


def run_with_triggered(
    config: Config, distribution: Distribution, dry_run: bool, triggered: list[str]
) -> None:
    if triggered:
        info(f"Triggered by: {', '.join(triggered)}")

    packages = select_sync_packages(distribution, triggered, config.ext4_sync.extra_packages)
    if not packages and not (distribution is Distribution.DEBIAN and not triggered):
        info("No relevant systemd packages require ext4 synchronization")
        return

    mount_point = config.ext4_sync.mount_point
    ensure_mounted(mount_point, dry_run)

    if distribution is Distribution.ARCH:
        sync_arch_packages(mount_point, packages, dry_run)
    else:
        sync_debian_files(mount_point, packages, dry_run)

    success(f"{distribution.display_name} ext4 systemd sync complete")


def ensure_mounted(mount_point: str, dry_run: bool) -> None:
    if is_mountpoint(mount_point):
        info(f"{mount_point} already mounted")
        return

    ext4_uuid = find_mount_uuid("/")
    if ext4_uuid is None:
        raise RuntimeError("Could not get ext4 root UUID")

    CommandRunner(dry_run=dry_run).run("mount", [f"UUID={ext4_uuid}", mount_point])
    info(f"Mounted ext4 root to {mount_point}")


def read_triggered_packages() -> list[str]:
    try:
        content = sys.stdin.read()
    except OSError:
        return []
    return parse_package_lines(content)


def read_package_file(path: Path) -> list[str]:
    if not path.exists():
        return []
    return parse_package_lines(path.read_text())


def record_apt_pending(
    pending_path: Path, package_lines: str, package_name_for: PackageNameFor
) -> None:
    packages = set(read_package_file(pending_path))
    for deb_path in (line.strip() for line in package_lines.splitlines()):
        if not deb_path.endswith(".deb"):
            continue
        package = package_name_for(deb_path)
        if package is not None:
            packages.add(package)

    if not packages:
        return

    write_package_file(pending_path, sorted(packages))


def write_package_file(path: Path, packages: list[str]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temp_path = path.with_suffix(".tmp")
    content = "" if not packages else "\n".join(packages) + "\n"
    temp_path.write_text(content)
    temp_path.replace(path)


def clear_pending(path: Path) -> None:
    try:
        path.unlink()
    except FileNotFoundError:
        pass


def finish_apt_post(
    pending_path: Path,
    state_path: Path,
    triggered: list[str],
    current_state: str,
    previous_state: str | None,
    dry_run: bool,
    sync: Callable[[list[str]], None],
) -> None:
    state_changed = previous_state is not None and previous_state != current_state
    sync_triggers: list[str] = [] if state_changed else triggered
    sync(sync_triggers)

    if not dry_run:
        write_state(state_path, current_state)
        clear_pending(pending_path)


def merge_package_lines(left: list[str], right: list[str]) -> list[str]:
    return sorted(set(left) | set(right))


def parse_package_lines(content: str) -> list[str]:
    return sorted({line.strip() for line in content.splitlines() if line.strip()})


def installed_package_state(distribution: Distribution, packages: list[str]) -> str:
    version_for = (
        pacman_query_version if distribution is Distribution.ARCH else debian_query_version
    )
    return installed_package_state_with(packages, version_for)


def installed_package_state_with(packages: list[str], version_for: VersionFor) -> str:
    states: set[str] = set()
    for package in packages:
        version = version_for(package)
        if version is not None:
            states.add(f"{package}={version}")
    return "\n".join(sorted(states))


def read_state(path: Path) -> str | None:
    if not path.exists():
        return None
    return path.read_text()


def write_state(path: Path, state: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temp_path = path.with_suffix(".tmp")
    temp_path.write_text(state)
    temp_path.replace(path)


def select_sync_packages(
    distribution: Distribution, triggered: list[str], extra_packages: Sequence[str] = ()
) -> list[str]:
    hook_targets = collect_hook_targets(distribution, extra_packages)
    return filter_sync_packages(triggered, hook_targets)


def filter_sync_packages(triggered: list[str], hook_targets: list[str]) -> list[str]:
    if not triggered:
        return list(hook_targets)

    allowed = set(hook_targets)
    if any(package in allowed for package in triggered):
        return list(hook_targets)
    return []


@dataclass(frozen=True)
class ArchArchive:
    package: PacmanPackage
    source: Path
    destination: Path


def sync_arch_packages(mount_point: str, packages: list[str], dry_run: bool) -> None:
    host_packages = get_arch_package_metadata(packages)
    packages_to_install = select_arch_packages_to_install(
        host_packages,
        lambda name: pacman_query_package_in_root(mount_point, name),
    )

    if not packages_to_install:
        info("All target Arch packages are already current")
        return

    archives = resolve_arch_archives(
        Path("/var/cache/pacman/pkg"),
        Path(mount_point) / "var/cache/pacman/pkg",
        packages_to_install,
        pacman_query_archive_package,
    )

    sync_arch_cache(archives, dry_run)
    install_arch_packages(mount_point, archives, dry_run)


def get_arch_package_metadata(packages: list[str]) -> list[PacmanPackage]:
    metadata: list[PacmanPackage] = []
    for package in packages:
        host_package = pacman_query_package(package)
        if host_package is None:
            raise RuntimeError(
                f"Required host Arch package {package} is no longer installed; "
                "retaining sync retry"
            )
        metadata.append(host_package)
    return metadata


def select_arch_packages_to_install(
    packages: list[PacmanPackage], target_package: TargetPackage
) -> list[PacmanPackage]:
    packages_to_install: list[PacmanPackage] = []
    for package in packages:
        target = target_package(package.name)
        is_current = target is not None and target == package
        if not is_current:
            packages_to_install.append(package)
    return packages_to_install


def resolve_arch_archives(
    source_cache: Path,
    destination_cache: Path,
    packages: list[PacmanPackage],
    archive_package: ArchivePackage,
) -> list[ArchArchive]:
    try:
        entries = [
            entry
            for entry in source_cache.iterdir()
            if is_regular_file(entry) and ".pkg.tar." in entry.name
        ]
    except OSError as exc:
        raise RuntimeError(f"Failed to read Arch package cache {source_cache}") from exc
    entries.sort(key=lambda entry: entry.name)

    archives: list[ArchArchive] = []
    for package in packages:
        resolved: ArchArchive | None = None
        prefix = f"{package.name}-"
        for entry in entries:
            if not entry.name.startswith(prefix) or entry.name.endswith(".sig"):
                continue
            metadata = archive_package(entry)
            if metadata is None or metadata != package:
                continue
            resolved = ArchArchive(metadata, entry, destination_cache / entry.name)
            break

        if resolved is None:
            raise RuntimeError(
                f"Required cached Arch archive is missing for {package.name} "
                f"{package.version} {package.architecture} in {source_cache}"
            )
        archives.append(resolved)

    return archives


def sync_arch_cache(archives: list[ArchArchive], dry_run: bool) -> None:
    destination_cache = archives[0].destination.parent if archives else None
    if destination_cache is None:
        return

    if not dry_run:
        destination_cache.mkdir(parents=True, exist_ok=True)

    for archive in archives:
        if dry_run:
            info(f"[dry-run] Would copy {archive.source} to {archive.destination}")
            continue
        try:
            shutil.copy(archive.source, archive.destination)
        except OSError as exc:
            raise RuntimeError(f"Failed to copy cached package {archive.source}") from exc
        info(f"Copied {archive.source.name}")


def install_arch_packages(mount_point: str, archives: list[ArchArchive], dry_run: bool) -> None:
    if not archives:
        info("No packages to sync")
        return

    package_paths = [str(archive.destination) for archive in archives]

    if dry_run:
        info(
            f"[dry-run] Would install {len(package_paths)} Arch package archive(s) "
            f"into {mount_point}"
        )
        return
    # The packages adapter handles legacy chroot and current sysroot path semantics.
    pacman_install_archives(mount_point, package_paths)


def sync_debian_files(
    mount_point: str,
    packages: list[str],
    dry_run: bool,
    version_for: VersionFor = debian_query_version,
    files_for: Callable[[str], list[str]] = debian_query_files,
    is_owned: IsOwned = debian_file_owned_by_installed_package,
) -> None:
    runner = CommandRunner(dry_run=dry_run)
    files: set[str] = set()
    for package in packages:
        if version_for(package) is None:
            warn(f"Package {package} not installed, skipping")
            continue
        files.update(file for file in files_for(package) if is_syncable_file(file))

    manifest_path = Path(mount_point) / SYNC_MANIFEST
    previous_files = read_manifest(manifest_path)
    sync_files = [file for file in sorted(files) if dry_run or Path(file).exists()]

    for file in sync_files:
        runner.run("rsync", ["-aAX", "--relative", file, mount_point])

    if not dry_run:
        remove_stale_files_with(mount_point, previous_files, sync_files, is_owned)
        write_manifest(manifest_path, sync_files)

    # The ext4 root has its own linker cache. Rebuild it after the package
    # files have been copied from the Btrfs-backed system.
    runner.run("ldconfig", ["-r", mount_point])


def is_syncable_file(path: str) -> bool:
    try:
        metadata = os.lstat(path)
    except OSError:
        return False
    return stat.S_ISREG(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode)


def is_regular_file(path: Path) -> bool:
    try:
        return stat.S_ISREG(os.lstat(path).st_mode)
    except OSError:
        return False


def read_manifest(path: Path) -> list[str]:
    if not path.exists():
        return []
    return parse_package_lines(path.read_text())


def remove_stale_files_with(
    mount_point: str, previous: list[str], current: list[str], is_owned: IsOwned
) -> None:
    current_files = set(current)

    for file in previous:
        if file in current_files:
            continue

        relative = file.removeprefix("/")
        if relative == file:
            raise ValueError(f"Manifest path is not absolute: {file}")
        target = Path(mount_point) / relative
        if is_owned(file):
            continue

        try:
            target.unlink()
        except FileNotFoundError:
            pass
        except OSError as exc:
            raise RuntimeError(f"Could not remove stale synced file {target}") from exc


def write_manifest(path: Path, files: list[str]) -> None:
    temp_path = path.with_name(f"{path.name}.tmp")
    content = "" if not files else "\n".join(files) + "\n"
    temp_path.write_text(content)
    temp_path.replace(path)

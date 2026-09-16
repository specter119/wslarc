"""Block-device, mount-table, dependency, and package-query helpers."""

from __future__ import annotations

import json
import shutil
import subprocess
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from wslarc.config import Distribution
from wslarc.utils.shell import CommandError, run


@dataclass(frozen=True)
class Dependency:
    package: str
    commands: tuple[str, ...]


@dataclass(frozen=True)
class BlockDevice:
    name: str
    label: str | None
    fstype: str | None


@dataclass(frozen=True)
class MountInfo:
    target: str
    source: str
    fstype: str
    options: str
    uuid: str | None = None


@dataclass(frozen=True)
class PacmanPackage:
    name: str
    version: str
    architecture: str


def command_exists(command: str) -> bool:
    path = Path(command)
    return path.is_file() if path.is_absolute() else shutil.which(command) is not None


def ensure_dependencies_for(dependencies: list[Dependency], distribution: Distribution) -> None:
    missing: list[tuple[str, list[str]]] = []
    for dependency in dependencies:
        absent = [command for command in dependency.commands if not command_exists(command)]
        if absent:
            missing.append((dependency.package, absent))
    if not missing:
        return
    details = "\n".join(
        f"  - {package} (commands: {', '.join(commands)})" for package, commands in missing
    )
    installer = "sudo pacman -S" if distribution is Distribution.ARCH else "sudo apt-get install"
    packages = " ".join(package for package, _ in missing)
    raise CommandError(
        f"Missing required dependencies for {distribution.display_name}:\n"
        f"{details}\nInstall with: {installer} {packages}"
    )


def parse_lsblk_devices(output: str) -> list[BlockDevice]:
    raw = json.loads(output)
    return [
        BlockDevice(item["name"], item.get("label"), item.get("fstype"))
        for item in raw.get("blockdevices", [])
    ]


def list_block_devices() -> list[BlockDevice]:
    return parse_lsblk_devices(run("lsblk", ["-J", "-d", "-o", "NAME,LABEL,FSTYPE"]))


def list_block_device_names() -> list[str]:
    return [device.name for device in list_block_devices()]


def find_btrfs_device_by_label(label: str) -> str | None:
    for device in list_block_devices():
        if device.fstype == "btrfs" and device.label == label:
            return f"/dev/{device.name}"
    return None


def read_block_device(device: str) -> BlockDevice | None:
    devices = parse_lsblk_devices(run("lsblk", ["-J", "-d", "-o", "NAME,LABEL,FSTYPE", device]))
    return devices[0] if devices else None


def _flatten_mounts(item: dict[str, Any], result: list[MountInfo]) -> None:
    result.append(
        MountInfo(
            target=item.get("target", ""),
            source=item.get("source") or "",
            fstype=item.get("fstype") or "",
            options=item.get("options") or "",
            uuid=item.get("uuid"),
        )
    )
    for child in item.get("children", []):
        _flatten_mounts(child, result)


def parse_findmnt_mounts(output: str) -> list[MountInfo]:
    raw = json.loads(output)
    result: list[MountInfo] = []
    for filesystem in raw.get("filesystems", []):
        _flatten_mounts(filesystem, result)
    return result


def list_btrfs_mounts() -> list[MountInfo]:
    return parse_findmnt_mounts(
        run("findmnt", ["-J", "-t", "btrfs", "-o", "TARGET,SOURCE,FSTYPE,OPTIONS,UUID"])
    )


def find_mount(path: str) -> MountInfo | None:
    try:
        output = subprocess.run(
            ["findmnt", "-J", path, "-o", "TARGET,SOURCE,FSTYPE,OPTIONS,UUID"],
            check=False,
            capture_output=True,
            text=True,
        )
    except OSError as exc:
        raise CommandError(f"failed to execute findmnt: {exc}") from exc
    if output.returncode == 1:
        return None
    if output.returncode:
        raise CommandError(output.stderr.strip() or f"findmnt failed for {path}")
    mounts = parse_findmnt_mounts(output.stdout)
    return mounts[0] if mounts else None


def is_mountpoint(path: str) -> bool:
    return find_mount(path) is not None


def find_mount_uuid(path: str) -> str | None:
    mount = find_mount(path)
    return mount.uuid if mount else None


def systemctl_property(unit: str, property_name: str) -> str:
    return run("systemctl", ["show", unit, f"--property={property_name}", "--value"]).strip()


def list_directory_names(path: str | Path) -> list[str]:
    return sorted(item.name for item in Path(path).iterdir())


def parse_pacman_query_version(output: str) -> str | None:
    line = output.splitlines()[0].strip() if output.splitlines() else ""
    parts = line.split(None, 1)
    return parts[1].strip() if len(parts) == 2 and parts[1].strip() else None


def parse_pacman_package_info(output: str) -> PacmanPackage | None:
    fields: dict[str, str] = {}
    for line in output.splitlines():
        if ":" in line:
            key, value = line.split(":", 1)
            fields[key.strip()] = value.strip()
    name, version, architecture = (
        fields.get("Name"),
        fields.get("Version"),
        fields.get("Architecture"),
    )
    if not name or not version or not architecture:
        return None
    return PacmanPackage(name, version, architecture)


def parse_pacman_depends(output: str) -> list[str]:
    dependencies: list[str] = []
    in_depends = False
    for line in output.splitlines():
        if line.startswith("Depends On"):
            in_depends = True
            line = line.split(":", 1)[-1]
        elif in_depends and not line.startswith((" ", "\t")):
            break
        if in_depends:
            for token in line.split():
                if token != "None":
                    dependencies.append(token.split("<", 1)[0].split(">", 1)[0].split("=", 1)[0])
    return dependencies


def parse_debian_status_version(output: str) -> str | None:
    try:
        status, version = output.strip().split("\t", 1)
    except ValueError:
        return None
    state = status.split()[-1] if status.split() else ""
    present_states = {
        "installed",
        "half-installed",
        "unpacked",
        "half-configured",
        "triggers-awaited",
        "triggers-pending",
    }
    return version if state in present_states and version else None


def parse_debian_depends(output: str) -> list[list[str]]:
    groups: list[list[str]] = []
    for group in output.replace("\n", ",").split(","):
        alternatives: list[str] = []
        for item in group.split("|"):
            name = item.strip().split()[0] if item.strip() else ""
            name = name.rstrip("()")
            if name and not name.startswith("${") and name not in alternatives:
                alternatives.append(name)
        if alternatives:
            groups.append(alternatives)
    return groups


def select_debian_dependency(
    alternatives: list[str], is_installed: Callable[[str], bool]
) -> str | None:
    return next((item for item in alternatives if is_installed(item)), None)


def pacman_sysroot_chroots(version: str) -> bool:
    marker = version.split("Pacman v", 1)
    if len(marker) != 2:
        raise ValueError("cannot identify pacman version")
    parts = marker[1].split(".")
    major, minor = int(parts[0]), int(parts[1])
    return (major, minor) < (7, 1)


def pacman_install_args(root: str, archives: list[str], chroots: bool) -> list[str]:
    result = ["--sysroot", root, "-U", "--noconfirm"]
    root_path = Path(root).resolve()
    for archive in archives:
        archive_path = Path(archive).resolve()
        try:
            relative = archive_path.relative_to(root_path)
        except ValueError as exc:
            raise ValueError(f"archive is outside target sysroot: {archive}") from exc
        if any(part in {"..", "."} for part in relative.parts):
            raise ValueError(f"invalid archive path inside sysroot: {archive}")
        result.append(f"/{relative}" if chroots else archive)
    return result

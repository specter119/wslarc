"""Display configured and live WSLArc status."""

from __future__ import annotations

from dataclasses import dataclass

from wslarc.config import Config, Distribution
from wslarc.generators import hooks, systemd
from wslarc.utils.block import (
    list_btrfs_mounts,
    systemctl_property,
)


@dataclass(frozen=True)
class UnitStatus:
    unit_file_state: str
    active_state: str


def read_unit_status(name: str) -> UnitStatus:
    def get(property_name: str) -> str:
        try:
            return systemctl_property(name, property_name).strip()
        except Exception:
            return "unknown"

    return UnitStatus(get("UnitFileState"), get("ActiveState"))


def managed_unit_names(config: Config) -> list[str]:
    names = systemd.managed_mount_units(config)
    if any(item.mount == "/usr" for item in config.subvolumes.backup.values()):
        names.append(hooks.ext4_mount_unit_filename(config))
        names.extend([systemd.USR_PRE_UNIT, systemd.USR_POST_UNIT])
    return sorted(set(names))


def configured_subvolume_lines(config: Config) -> list[str]:
    lines = [f"{name} -> {item.mount} [backup]" for name, item in config.subvolumes.backup.items()]
    lines.extend(
        f"{name} -> {item.mount} [transfer{', nodatacow' if item.nodatacow else ''}]"
        for name, item in config.subvolumes.transfer.items()
    )
    lines.extend(f"{name} [snapshot-only]" for name in config.subvolumes.snapshot_only)
    return sorted(lines)


def run(config: Config, distribution: Distribution) -> None:
    print("WSL Btrfs Status")
    print("\nConfiguration")
    print(f"  Config UUID: {config.uuid or 'not set'}")
    print(f"  VHDX: {config.vhdx.path}")
    print(f"  Mount base: {config.mount.base}")
    print(f"  User: {config.get_user()}")
    print(f"  Distribution: {distribution.display_name}")
    print("\nBtrfs Mounts")
    mounts = list_btrfs_mounts()
    for mount in mounts or []:
        print(f"  {mount.source} on {mount.target} type {mount.fstype} ({mount.options})")
    if not mounts:
        print("  No Btrfs mounts found")
    print("\nConfigured Subvolumes")
    for line in configured_subvolume_lines(config):
        print(f"  {line}")
    print("\nSystemd Services")
    for unit in ["btrbk.timer", *managed_unit_names(config)]:
        status = read_unit_status(unit)
        print(f"  {unit}: {status.unit_file_state}, {status.active_state}")

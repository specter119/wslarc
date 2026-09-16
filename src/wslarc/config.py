"""Configuration models and TOML persistence."""

from __future__ import annotations

import copy
import os
import platform
import tomllib
from dataclasses import dataclass, field
from enum import StrEnum
from pathlib import Path
from typing import Any

import tomli_w


class ConfigError(ValueError):
    """Raised when a configuration file is invalid."""


class Distribution(StrEnum):
    ARCH = "arch"
    DEBIAN = "debian"

    @classmethod
    def detect(cls) -> Distribution:
        try:
            values = {}
            for line in Path("/etc/os-release").read_text().splitlines():
                if "=" in line:
                    key, value = line.split("=", 1)
                    values[key] = value.strip().strip('"').lower()
            distro_id = values.get("ID", "")
        except OSError:
            distro_id = ""
        if distro_id in {"debian", "ubuntu", "linuxmint", "kali"}:
            return cls.DEBIAN
        return cls.ARCH

    @property
    def display_name(self) -> str:
        return "Arch" if self is self.ARCH else "Debian"


@dataclass
class VhdxConfig:
    path: str
    label: str


@dataclass
class UserConfig:
    name: str
    options: str = "-M -G wheel"


@dataclass
class MountConfig:
    base: str
    options: str = "compress=zstd:3,noatime,nofail"


@dataclass
class BackupSubvol:
    mount: str
    options: str | None = None
    full: bool = False

    @classmethod
    def from_raw(cls, value: Any) -> BackupSubvol:
        if isinstance(value, str):
            return cls(value)
        if not isinstance(value, dict):
            raise ConfigError("backup subvolume must be a string or table")
        unknown = set(value) - {"mount", "options"}
        if unknown:
            names = ", ".join(sorted(unknown))
            raise ConfigError(f"unknown backup subvolume field(s): {names}")
        mount = value.get("mount")
        if not isinstance(mount, str):
            raise ConfigError("backup subvolume mount must be a string")
        options = value.get("options")
        if options is not None and not isinstance(options, str):
            raise ConfigError("backup subvolume options must be a string")
        return cls(mount, options, full=True)

    def to_raw(self) -> str | dict[str, str]:
        if not self.full and self.options is None:
            return self.mount
        result: dict[str, str] = {"mount": self.mount}
        if self.options is not None:
            result["options"] = self.options
        return result


@dataclass
class ExcludeConfig:
    parent: str
    paths: list[str]


@dataclass
class TransferSubvol:
    mount: str
    nodatacow: bool = False
    options: str | None = None


@dataclass
class SnapshotSubvol:
    snapshot_name: str
    source: str = "/etc"


@dataclass
class SubvolumesConfig:
    backup: dict[str, BackupSubvol] = field(default_factory=dict)
    exclude: ExcludeConfig = field(
        default_factory=lambda: ExcludeConfig(
            "@home",
            [".cache", ".local", ".npm", ".bun", ".vscode-server-insiders"],
        )
    )
    transfer: dict[str, TransferSubvol] = field(default_factory=dict)
    snapshot_only: dict[str, SnapshotSubvol] = field(default_factory=dict)

    @classmethod
    def for_distribution(cls, distribution: Distribution) -> SubvolumesConfig:
        backup = {
            "@home": BackupSubvol("/home/$USER"),
            "@usr": BackupSubvol("/usr"),
            "@opt": BackupSubvol("/opt"),
            "@nix": BackupSubvol("/nix"),
        }
        if distribution is Distribution.ARCH:
            backup["@var_lib_pacman"] = BackupSubvol("/var/lib/pacman")
        else:
            backup["@var_lib_dpkg"] = BackupSubvol("/var/lib/dpkg")
            backup["@var_lib_apt"] = BackupSubvol("/var/lib/apt")
        transfer = {
            "@var_cache": TransferSubvol("/var/cache", nodatacow=True),
            "@var_log": TransferSubvol("/var/log"),
            "@var_tmp": TransferSubvol("/var/tmp", nodatacow=True),
            "@containers": TransferSubvol("/var/lib/containers", nodatacow=True),
        }
        return cls(
            backup=backup,
            transfer=transfer,
            snapshot_only={"@etc": SnapshotSubvol("etc")},
        )


@dataclass
class BtrbkConfig:
    snapshot_dir: str = ".snapshots"
    preserve_min: str = "latest"
    preserve: str = "2d 1w 2m"
    timer_schedule: str = "*-*-* 03:00:00"


@dataclass
class Ext4SyncConfig:
    mount_point: str = "/mnt/ext4-root"


@dataclass
class Config:
    vhdx: VhdxConfig
    user: UserConfig
    mount: MountConfig
    subvolumes: SubvolumesConfig
    btrbk: BtrbkConfig
    ext4_sync: Ext4SyncConfig = field(default_factory=Ext4SyncConfig)
    uuid: str | None = None

    @classmethod
    def for_distribution(cls, distribution: Distribution) -> Config:
        return cls(
            vhdx=VhdxConfig("", f"{distribution.display_name}Btrfs"),
            user=UserConfig(""),
            mount=MountConfig("/mnt/btrfs"),
            subvolumes=SubvolumesConfig.for_distribution(distribution),
            btrbk=BtrbkConfig(),
        )

    @classmethod
    def load_or_default_unexpanded(cls, path: str | Path, distribution: Distribution) -> Config:
        return (
            cls.load_unexpanded(path) if Path(path).exists() else cls.for_distribution(distribution)
        )

    @classmethod
    def load_unexpanded(cls, path: str | Path) -> Config:
        try:
            with Path(path).open("rb") as stream:
                raw = tomllib.load(stream)
        except OSError as exc:
            raise ConfigError(f"failed to read config file {path}: {exc}") from exc
        except tomllib.TOMLDecodeError as exc:
            raise ConfigError(f"failed to parse config file {path}: {exc}") from exc
        return cls.from_dict(raw)

    @classmethod
    def load(cls, path: str | Path) -> Config:
        config = cls.load_unexpanded(path)
        config.expand_variables()
        return config

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> Config:
        try:
            vhdx_raw = raw["vhdx"]
            user_raw = raw["user"]
            mount_raw = raw["mount"]
        except KeyError as exc:
            raise ConfigError(f"missing required configuration section: {exc.args[0]}") from exc
        vhdx = VhdxConfig(str(vhdx_raw["path"]), str(vhdx_raw["label"]))
        user = UserConfig(str(user_raw["name"]), str(user_raw.get("options", "-M -G wheel")))
        mount = MountConfig(
            str(mount_raw["base"]),
            str(mount_raw.get("options", "compress=zstd:3,noatime,nofail")),
        )

        subvol_raw = raw.get("subvolumes", {})
        defaults = SubvolumesConfig.for_distribution(Distribution.detect())
        backup_raw = subvol_raw.get("backup")
        backup = (
            {key: BackupSubvol.from_raw(value) for key, value in backup_raw.items()}
            if isinstance(backup_raw, dict)
            else defaults.backup
        )
        exclude_raw = subvol_raw.get("exclude")
        exclude = (
            ExcludeConfig(str(exclude_raw["parent"]), [str(item) for item in exclude_raw["paths"]])
            if isinstance(exclude_raw, dict)
            else defaults.exclude
        )
        transfer_raw = subvol_raw.get("transfer")
        transfer = (
            {
                key: TransferSubvol(
                    str(value["mount"]),
                    bool(value.get("nodatacow", False)),
                    value.get("options"),
                )
                for key, value in transfer_raw.items()
            }
            if isinstance(transfer_raw, dict)
            else defaults.transfer
        )
        snapshot_raw = subvol_raw.get("snapshot_only")
        snapshot_only = (
            {
                key: SnapshotSubvol(str(value["snapshot_name"]), str(value.get("source", "/etc")))
                for key, value in snapshot_raw.items()
            }
            if isinstance(snapshot_raw, dict)
            else defaults.snapshot_only
        )
        btrbk_raw = raw.get("btrbk", {})
        btrbk = BtrbkConfig(
            str(btrbk_raw.get("snapshot_dir", ".snapshots")),
            str(btrbk_raw.get("preserve_min", "latest")),
            str(btrbk_raw.get("preserve", "2d 1w 2m")),
            str(btrbk_raw.get("timer_schedule", "*-*-* 03:00:00")),
        )
        ext4_raw = raw.get("ext4_sync", {})
        return cls(
            vhdx=vhdx,
            user=user,
            mount=mount,
            subvolumes=SubvolumesConfig(backup, exclude, transfer, snapshot_only),
            btrbk=btrbk,
            ext4_sync=Ext4SyncConfig(str(ext4_raw.get("mount_point", "/mnt/ext4-root"))),
            uuid=raw.get("uuid"),
        )

    def to_dict(self) -> dict[str, Any]:
        subvolumes = {
            "backup": {key: value.to_raw() for key, value in self.subvolumes.backup.items()},
            "exclude": {
                "parent": self.subvolumes.exclude.parent,
                "paths": self.subvolumes.exclude.paths,
            },
            "transfer": {
                key: {
                    "mount": value.mount,
                    "nodatacow": value.nodatacow,
                    **({"options": value.options} if value.options is not None else {}),
                }
                for key, value in self.subvolumes.transfer.items()
            },
            "snapshot_only": {
                key: {"snapshot_name": value.snapshot_name, "source": value.source}
                for key, value in self.subvolumes.snapshot_only.items()
            },
        }
        result: dict[str, Any] = {
            "vhdx": {"path": self.vhdx.path, "label": self.vhdx.label},
            "user": {"name": self.user.name, "options": self.user.options},
            "mount": {"base": self.mount.base, "options": self.mount.options},
            "subvolumes": subvolumes,
            "btrbk": {
                "snapshot_dir": self.btrbk.snapshot_dir,
                "preserve_min": self.btrbk.preserve_min,
                "preserve": self.btrbk.preserve,
                "timer_schedule": self.btrbk.timer_schedule,
            },
            "ext4_sync": {"mount_point": self.ext4_sync.mount_point},
        }
        if self.uuid is not None:
            result["uuid"] = self.uuid
        return result

    def save(self, path: str | Path) -> None:
        destination = Path(path)
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(tomli_w.dumps(self.to_dict()))

    def get_user(self) -> str:
        return self.user.name

    def set_user_unexpanded(self, user: str) -> None:
        self.user.name = user

    def resolve_variables(self) -> Config:
        result = copy.deepcopy(self)
        result.expand_variables()
        return result

    def expand_variables(self) -> None:
        user = self.user.name
        for backup in self.subvolumes.backup.values():
            backup.mount = backup.mount.replace("$USER", user)
        for transfer in self.subvolumes.transfer.values():
            transfer.mount = transfer.mount.replace("$USER", user)
        for snapshot in self.subvolumes.snapshot_only.values():
            snapshot.source = snapshot.source.replace("$USER", user)


def default_config(distribution: Distribution | None = None) -> Config:
    return Config.for_distribution(distribution or Distribution.detect())


def runtime_user() -> str:
    return os.environ.get("USER") or os.environ.get("LOGNAME") or platform.node()

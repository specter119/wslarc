"""systemd mount-unit generation."""

from __future__ import annotations

import subprocess

from wslarc.config import Config


def path_to_unit_name(path: str) -> str:
    try:
        result = subprocess.run(
            ["systemd-escape", "--path", path],
            check=False,
            capture_output=True,
            text=True,
        )
        if result.returncode == 0 and result.stdout.strip():
            return result.stdout.strip()
    except OSError:
        pass
    return path.strip("/").replace("/", "-")


def mount_unit_filename(mount_point: str) -> str:
    return f"{path_to_unit_name(mount_point)}.mount"


def generate_base_mount(config: Config) -> str:
    uuid = config.uuid or "REPLACE_WITH_UUID"
    return (
        "[Unit]\n"
        "Description=Mount Btrfs Volume\n\n"
        "[Mount]\n"
        f"What=UUID={uuid}\n"
        f"Where={config.mount.base}\n"
        "Type=btrfs\n"
        f"Options={config.mount.options}\n\n"
        "[Install]\n"
        "WantedBy=multi-user.target\n"
    )


def generate_subvol_mount(
    config: Config,
    subvol: str,
    mount_point: str,
    custom_options: str | None = None,
) -> str:
    uuid = config.uuid or "REPLACE_WITH_UUID"
    base_unit = path_to_unit_name(config.mount.base)
    base_options = custom_options or config.mount.options
    options = f"subvol={subvol},{base_options}"
    user = config.get_user()
    home_path = f"/home/{user}"
    is_home = mount_point == home_path
    requires = (
        f"{base_unit}.mount {path_to_unit_name(home_path)}.mount"
        if mount_point.startswith(home_path) and not is_home
        else f"{base_unit}.mount"
    )
    before = "Before=user@.service" if is_home else ""
    return (
        "[Unit]\n"
        f"Description=Mount {subvol} subvolume\n"
        f"Requires={requires}\n"
        f"After={requires}\n"
        f"{before}\n\n"
        "[Mount]\n"
        f"What=UUID={uuid}\n"
        f"Where={mount_point}\n"
        "Type=btrfs\n"
        f"Options={options}\n\n"
        "[Install]\n"
        "WantedBy=multi-user.target\n"
    )


def generate_ext4_mount(config: Config, uuid: str) -> str:
    return (
        "[Unit]\n"
        "Description=Mount ext4 root for sync\n\n"
        "[Mount]\n"
        f"What=UUID={uuid}\n"
        f"Where={config.ext4_sync.mount_point}\n"
        "Type=ext4\n"
        "Options=defaults\n\n"
        "[Install]\n"
        "WantedBy=multi-user.target\n"
    )


def ext4_mount_unit_filename(config: Config) -> str:
    return f"{path_to_unit_name(config.ext4_sync.mount_point)}.mount"

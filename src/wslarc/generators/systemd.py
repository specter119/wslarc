"""systemd mount-unit generation."""

from __future__ import annotations

import subprocess
from pathlib import Path

from wslarc.config import Config

UNIT_DIR = Path("/etc/systemd/system")
USR_PRE_UNIT = "wslarc-usr-pre.service"
USR_POST_UNIT = "wslarc-usr-post.service"
USER_DROPIN_PATH = UNIT_DIR / "user@.service.d/wslarc.conf"


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
    if mount_point == "/usr":
        # Starting /usr after a failed stash would hide the WSL mounts with no record of
        # them, so the pre unit is a hard dependency, not just an install-time Wants.
        requires = f"{requires} {USR_PRE_UNIT}"
    return (
        "[Unit]\n"
        f"Description=Mount {subvol} subvolume\n"
        f"Requires={requires}\n"
        f"After={requires}\n\n"
        "[Mount]\n"
        f"What=UUID={uuid}\n"
        f"Where={mount_point}\n"
        "Type=btrfs\n"
        f"Options={options}\n\n"
        "[Install]\n"
        "WantedBy=multi-user.target\n"
    )


def managed_mount_units(config: Config) -> list[str]:
    units = [mount_unit_filename(config.mount.base)]
    units.extend(mount_unit_filename(item.mount) for item in config.subvolumes.backup.values())
    units.extend(mount_unit_filename(item.mount) for item in config.subvolumes.transfer.values())
    return list(dict.fromkeys(units))


def generate_usr_pre(config: Config) -> str:
    base_condition = config.mount.base
    return (
        "[Unit]\n"
        "Description=Stash WSL mounts under /usr before the Btrfs /usr is mounted\n"
        "DefaultDependencies=no\n"
        f"After={mount_unit_filename(config.mount.base)} tmp.mount\n"
        f"Before={mount_unit_filename('/usr')}\n"
        f"ConditionPathIsMountPoint={base_condition}\n"
        f"PartOf={mount_unit_filename('/usr')}\n\n"
        "[Service]\n"
        "Type=oneshot\n"
        "RemainAfterExit=yes\n"
        "TimeoutStartSec=60s\n"
        "ExecStart=/usr/local/bin/wslarc boot-usr --pre\n\n"
        "[Install]\n"
        f"WantedBy={mount_unit_filename('/usr')}\n"
    )


def generate_usr_post(config: Config) -> str:
    usr_unit = mount_unit_filename("/usr")
    # ldconfig.service and systemd-tmpfiles-setup.service may run against the ext4
    # view of /usr; the post unit runs after them and repairs the shared state.
    after = [unit for unit in managed_mount_units(config) if unit != usr_unit]
    after.extend(["tmp.mount", "ldconfig.service", "systemd-tmpfiles-setup.service"])
    return (
        "[Unit]\n"
        "Description=Restore WSL mounts under /usr and reload systemd after the Btrfs mounts\n"
        "DefaultDependencies=no\n"
        f"Requires={usr_unit}\n"
        f"After={usr_unit} {' '.join(after)}\n"
        # Restarting usr.mount stops the restored binds; post must run again after it,
        # just like pre stashes again.
        f"PartOf={usr_unit}\n"
        "Before=shutdown.target\n"
        "Conflicts=shutdown.target\n\n"
        "[Service]\n"
        "Type=oneshot\n"
        "RemainAfterExit=yes\n"
        "TimeoutStartSec=60s\n"
        "ExecStart=/usr/local/bin/wslarc boot-usr --post\n\n"
        "[Install]\n"
        f"WantedBy={usr_unit}\n"
    )


def _covers(mount: str, path: str) -> bool:
    return path == mount or path.startswith(mount.rstrip("/") + "/")


def _home_mount(config: Config) -> str:
    """The managed mount that carries the user's home, or the home directory itself.

    Mounting @home at /home is common, and naming /home/alice.mount there would order
    user managers after a unit that does not exist.
    """
    home = f"/home/{config.get_user()}"
    mounted = [item.mount for item in config.subvolumes.backup.values()]
    mounted.extend(item.mount for item in config.subvolumes.transfer.values())
    covering = [mount for mount in mounted if _covers(mount, home)]
    return max(covering, key=len) if covering else home


def generate_user_dropin(config: Config, with_usr_post: bool) -> str:
    """Order user managers after the managed mounts.

    systemd ignores dependencies that name the ``user@.service`` template, so the
    ordering is expressed as a drop-in that applies to every instance.
    """
    after = [mount_unit_filename(_home_mount(config))]
    if with_usr_post:
        after.insert(0, USR_POST_UNIT)
    return "[Unit]\n" + f"After={' '.join(after)}\n"


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

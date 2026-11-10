"""Generate and install managed systemd mount units."""

from __future__ import annotations

import configparser
import sys
from pathlib import Path

from wslarc.config import Config, Distribution
from wslarc.generators import btrbk, hooks, invocation, systemd
from wslarc.utils.block import Dependency, ensure_dependencies_for, find_mount, is_mountpoint
from wslarc.utils.prompt import confirm_or_yes, info, success, warn
from wslarc.utils.shell import CommandRunner
from wslarc.utils.storage import install_copies, verify_mount

SYSTEMD_DIR = systemd.UNIT_DIR
BTRBK_CONF = Path("/etc/btrbk/btrbk.conf")
WSL_CONF = Path("/etc/wsl.conf")


def usr_subvol_name(config: Config) -> str | None:
    """The subvolume mounted at /usr, backup or transfer.

    Both kinds are written as a mount unit, so only asking the backup table would let
    `_remove_boot_units` delete the unit that `_generate_units` just wrote.
    """
    for table in (config.subvolumes.backup, config.subvolumes.transfer):
        for name, subvol in table.items():
            if subvol.mount == "/usr":
                return name
    return None


def validate_automation_config_path(config: Config, config_path: str) -> None:
    path = Path(config_path)
    if not path.is_absolute() or any(ord(char) < 32 for char in config_path):
        raise ValueError(
            "automation requires an absolute configuration path without control characters"
        )
    blocked = [
        config.mount.base,
        *(item.mount for item in config.subvolumes.backup.values()),
        *(item.mount for item in config.subvolumes.transfer.values()),
        "/tmp",
        "/run",
        "/var/tmp",
        config.ext4_sync.mount_point,
    ]
    if any(config_path.startswith(prefix) for prefix in blocked):
        raise ValueError(
            "configuration must be on persistent ext4 storage available before managed mounts"
        )


def _write(path: Path, content: str, runner: CommandRunner) -> None:
    if runner.dry_run:
        info(f"[dry-run] Would write {path}")
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content)


def _update_wsl_conf(config_path: str, runner: CommandRunner) -> None:
    command = f"{invocation.shell_command(config_path)} attach"
    if runner.dry_run:
        info(f"[dry-run] Would update {WSL_CONF} with [boot] command")
        return
    parser = configparser.ConfigParser(interpolation=None, delimiters=("="))
    if WSL_CONF.exists():
        parser.read(WSL_CONF)
    if not parser.has_section("boot"):
        parser.add_section("boot")
    parser.set("boot", "command", command)
    with WSL_CONF.open("w") as stream:
        parser.write(stream)
    success("wsl.conf updated with boot command")


def _generate_units(config: Config, runner: CommandRunner) -> None:
    _write(
        SYSTEMD_DIR / systemd.mount_unit_filename(config.mount.base),
        systemd.generate_base_mount(config),
        runner,
    )
    for name, backup in config.subvolumes.backup.items():
        _write(
            SYSTEMD_DIR / systemd.mount_unit_filename(backup.mount),
            systemd.generate_subvol_mount(config, name, backup.mount, backup.options),
            runner,
        )
    for name, transfer in config.subvolumes.transfer.items():
        _write(
            SYSTEMD_DIR / systemd.mount_unit_filename(transfer.mount),
            systemd.generate_subvol_mount(config, name, transfer.mount, transfer.options),
            runner,
        )


def _wslarc_usr_mount_unit(config: Config) -> Path | None:
    """The `usr.mount` unit wslarc generated, if it is still on disk.

    Ownership is the `[Mount]` table itself: `What=UUID=<configured uuid>` and
    `Where=/usr` must match exactly. Another disk mounted at /usr, or a unit that only
    mentions the UUID in a comment, is left alone, and without a known UUID nothing can be
    attributed to wslarc.
    """
    if config.uuid is None:
        return None
    unit = SYSTEMD_DIR / systemd.mount_unit_filename("/usr")
    if not unit.is_file():
        return None
    try:
        text = unit.read_text()
    except OSError:
        return None
    parser = configparser.ConfigParser(interpolation=None, delimiters=("=",))
    try:
        parser.read_string(text)
    except configparser.Error:
        return None
    if not parser.has_section("Mount"):
        return None
    if parser.get("Mount", "What", fallback="") != f"UUID={config.uuid}":
        return None
    if parser.get("Mount", "Where", fallback="") != "/usr":
        return None
    return unit


def _remove_boot_units(config: Config, runner: CommandRunner) -> None:
    """Disable and delete boot units left over from a configuration that had /usr."""
    usr_unit = _wslarc_usr_mount_unit(config)
    leftover = [
        unit
        for unit in (systemd.USR_PRE_UNIT, systemd.USR_POST_UNIT)
        if (SYSTEMD_DIR / unit).exists()
    ]
    if usr_unit is not None:
        # Leaving usr.mount enabled would keep /usr overmounted at the next boot with
        # nothing left to restore the WSL mounts below it.
        leftover.append(usr_unit.name)
    if not leftover:
        return
    if runner.dry_run:
        info(f"[dry-run] Would disable and remove {', '.join(leftover)}")
        return
    runner.run("systemctl", ["disable", *leftover])
    for unit in leftover:
        (SYSTEMD_DIR / unit).unlink(missing_ok=True)
    if usr_unit is not None and is_mountpoint("/usr"):
        warn("the running system keeps the backup /usr until the next boot")


def _generate_boot_units(config: Config, runner: CommandRunner) -> None:
    with_usr = usr_subvol_name(config) is not None
    if with_usr:
        _write(
            SYSTEMD_DIR / systemd.USR_PRE_UNIT,
            systemd.generate_usr_pre(config),
            runner,
        )
        _write(
            SYSTEMD_DIR / systemd.USR_POST_UNIT,
            systemd.generate_usr_post(config),
            runner,
        )
    else:
        _remove_boot_units(config, runner)
    _write(systemd.USER_DROPIN_PATH, systemd.generate_user_dropin(config, with_usr), runner)


def _setup_ext4_sync(
    config: Config, distribution: Distribution, config_path: str, runner: CommandRunner
) -> None:
    from wslarc.utils.block import find_mount_uuid

    uuid = find_mount_uuid("/") or "REPLACE_WITH_ROOT_UUID"
    _write(
        SYSTEMD_DIR / hooks.ext4_mount_unit_filename(config),
        hooks.generate_ext4_mount(config, uuid),
        runner,
    )
    targets = hooks.collect_hook_targets(distribution, config.ext4_sync.extra_packages)
    hook_path, content = hooks.generate_package_hook(distribution, targets)
    content = content.replace(
        "/usr/local/bin/wslarc",
        invocation.shell_command(config_path),
    )
    _write(Path(hook_path), content, runner)


def _install_targets(
    config: Config, runner: CommandRunner, *, require_ext4: bool = True
) -> list[Path]:
    """Where every copy of the binary goes, verified to be the managed filesystems.

    Below an unmounted base, writing the @usr copy would silently create a plain
    directory on the ext4 root: the install would report success while the real
    subvolume copy stays stale. The same is true for the copy on the ext4 root when
    that mount point is not active, which is why that half can be skipped while the
    ext4 mount unit is only about to be started.
    """
    targets = [Path("/usr/local/bin/wslarc")]
    usr_subvol = usr_subvol_name(config)
    if usr_subvol is None:
        return targets
    if config.uuid is None:
        raise ValueError(f"UUID of {config.mount.base} is unknown; cannot install into it")
    verify_mount(find_mount(config.mount.base), config.mount.base, "btrfs", config.uuid)
    subvol_dir = Path(config.mount.base) / usr_subvol
    if not subvol_dir.is_dir():
        raise ValueError(f"{subvol_dir} is not a directory; refusing to install into it")
    # An ordinary directory with the right name would swallow the copy just like an
    # unmounted base does, so ask btrfs whether this really is the subvolume.
    runner.run("btrfs", ["subvolume", "show", str(subvol_dir)])
    targets.append(subvol_dir / "local/bin/wslarc")
    if require_ext4:
        # Boot-time `attach` and `boot-usr` run from the ext4 root, so it needs its own copy.
        mount_point = config.ext4_sync.mount_point
        if not is_mountpoint(mount_point):
            raise ValueError(f"{mount_point} is not mounted; cannot update ext4")
        mount_info = find_mount(mount_point)
        if mount_info is None or mount_info.fstype != "ext4":
            raise ValueError(f"{mount_point} is not an ext4 mount point; cannot update ext4")
        root = find_mount("/")
        if root is not None and root.fstype == "ext4" and mount_info.uuid != root.uuid:
            # Another ext4 filesystem mounted here would take the copy that the boot
            # step is supposed to read from the root filesystem.
            raise ValueError(f"{mount_point} is not the ext4 root of this system")
        targets.append(Path(mount_point) / "usr/local/bin/wslarc")
    return targets


def _install_binary(config: Config, runner: CommandRunner) -> None:
    if runner.dry_run:
        info("[dry-run] Would install wslarc to /usr/local/bin/wslarc")
        return
    source = Path(sys.argv[0]).resolve()
    if not source.is_file():
        raise ValueError(f"current wslarc executable is not a file: {source}")
    targets = _install_targets(config, runner)
    install_copies(source, targets)
    success("wslarc binary installed")


def systemctl_commands(config: Config, needs_ext4: bool) -> list[list[str]]:
    commands = [["systemctl", "daemon-reload"]]
    # usr.mount is enabled last, from run(): enabling it before the protection units
    # exist would hide the WSL mounts below /usr at the next boot.
    usr_unit = systemd.mount_unit_filename("/usr")
    commands.extend(
        ["systemctl", "enable", unit]
        for unit in systemd.managed_mount_units(config)
        if unit != usr_unit
    )
    if needs_ext4:
        # The package hooks rsync into this mount, so it must be active now, not only after reboot.
        commands.append(["systemctl", "enable", "--now", hooks.ext4_mount_unit_filename(config)])
    commands.append(["systemctl", "enable", "btrbk.timer"])
    return commands


def run(
    config: Config,
    distribution: Distribution,
    config_path: str,
    yes: bool = False,
    dry_run: bool = False,
    runner: CommandRunner | None = None,
) -> None:
    runner = runner or CommandRunner(dry_run=dry_run)
    if config.uuid is None:
        raise ValueError("UUID not set. Run 'wslarc init' first.")
    validate_automation_config_path(config, config_path)
    needs_ext4 = usr_subvol_name(config) is not None
    dependencies = [Dependency("btrbk", ("btrbk",))]
    if needs_ext4:
        dependencies.extend(
            [
                Dependency("btrfs-progs", ("btrfs",)),
                Dependency("rsync", ("rsync",)),
                Dependency("pacman", ("pacman",))
                if distribution is Distribution.ARCH
                else Dependency("dpkg", ("dpkg-query", "dpkg-deb")),
            ]
        )
    ensure_dependencies_for(dependencies, distribution)
    print("WSL Btrfs Mount Setup")
    if not confirm_or_yes("Generate and install systemd units?", True, yes):
        print("Aborted.")
        return
    if not runner.dry_run:
        # Fail before writing or enabling anything when the base copy could not land on
        # the managed filesystem. The ext4 copy is checked later, once its mount unit is
        # started.
        _install_targets(config, runner, require_ext4=False)
        if needs_ext4:
            # A previous install may have left usr.mount enabled. Disable it for the
            # length of the re-install so a failure in the steps below cannot leave
            # /usr overmounted without the units that restore what is hidden.
            runner.run("systemctl", ["disable", systemd.mount_unit_filename("/usr")])
    _update_wsl_conf(config_path, runner)
    _generate_units(config, runner)
    _generate_boot_units(config, runner)
    _write(BTRBK_CONF, btrbk.generate_config(config), runner)
    _write(SYSTEMD_DIR / "btrbk.service", btrbk.generate_service(config, config_path), runner)
    _write(SYSTEMD_DIR / "btrbk.timer", btrbk.generate_timer(config.btrbk.timer_schedule), runner)
    if needs_ext4:
        _setup_ext4_sync(config, distribution, config_path, runner)
    for command in systemctl_commands(config, needs_ext4):
        runner.run(command[0], command[1:])
    # The ext4 mount unit is only active after the systemctl commands above.
    _install_binary(config, runner)
    if needs_ext4:
        # Enable the boot units only once every copy of the binary they call exists,
        # and enable usr.mount only after them: an enabled usr.mount without the units
        # that protect it would hide the WSL mounts below /usr at the next boot.
        runner.run("systemctl", ["enable", systemd.USR_PRE_UNIT, systemd.USR_POST_UNIT])
        runner.run("systemctl", ["enable", systemd.mount_unit_filename("/usr")])
    success("Mount setup complete")

"""Generate and install managed systemd mount units."""

from __future__ import annotations

import configparser
import sys
from pathlib import Path

from wslarc.config import Config, Distribution
from wslarc.generators import btrbk, hooks, invocation, systemd
from wslarc.utils.block import Dependency, ensure_dependencies_for
from wslarc.utils.prompt import confirm_or_yes, info, success
from wslarc.utils.shell import CommandRunner
from wslarc.utils.storage import install_copies

SYSTEMD_DIR = Path("/etc/systemd/system")
BTRBK_CONF = Path("/etc/btrbk/btrbk.conf")
WSL_CONF = Path("/etc/wsl.conf")


def usr_subvol_name(config: Config) -> str | None:
    return next(
        (name for name, subvol in config.subvolumes.backup.items() if subvol.mount == "/usr"),
        None,
    )


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
    parser.optionxform = str  # type: ignore[assignment, method-assign]
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
    targets = hooks.collect_hook_targets(distribution)
    hook_path, content = hooks.generate_package_hook(distribution, targets)
    content = content.replace(
        "/usr/local/bin/wslarc",
        invocation.shell_command(config_path),
    )
    _write(Path(hook_path), content, runner)


def _install_binary(config: Config, runner: CommandRunner) -> None:
    if runner.dry_run:
        info("[dry-run] Would install wslarc to /usr/local/bin/wslarc")
        return
    source = Path(sys.argv[0]).resolve()
    if not source.is_file():
        raise ValueError(f"current wslarc executable is not a file: {source}")
    targets = [Path("/usr/local/bin/wslarc")]
    usr_subvol = usr_subvol_name(config)
    if usr_subvol:
        targets.append(Path(config.mount.base) / usr_subvol / "local/bin/wslarc")
    install_copies(source, targets)
    success("wslarc binary installed")


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
    _install_binary(config, runner)
    _update_wsl_conf(config_path, runner)
    _generate_units(config, runner)
    _write(BTRBK_CONF, btrbk.generate_config(config), runner)
    _write(SYSTEMD_DIR / "btrbk.service", btrbk.generate_service(config, config_path), runner)
    _write(SYSTEMD_DIR / "btrbk.timer", btrbk.generate_timer(config.btrbk.timer_schedule), runner)
    if needs_ext4:
        _setup_ext4_sync(config, distribution, config_path, runner)
    commands = [
        ["systemctl", "daemon-reload"],
        ["systemctl", "enable", systemd.mount_unit_filename(config.mount.base)],
    ]
    commands.extend(
        ["systemctl", "enable", systemd.mount_unit_filename(item.mount)]
        for item in config.subvolumes.backup.values()
    )
    commands.extend(
        ["systemctl", "enable", systemd.mount_unit_filename(item.mount)]
        for item in config.subvolumes.transfer.values()
    )
    commands.extend(
        [
            ["systemctl", "enable", "btrbk.timer"],
        ]
    )
    for command in commands:
        runner.run(command[0], command[1:])
    success("Mount setup complete")

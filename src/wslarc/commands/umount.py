"""Disable managed systemd mount units."""

from __future__ import annotations

from wslarc.config import Config
from wslarc.generators import hooks, systemd
from wslarc.utils.prompt import confirm_or_yes, info, success
from wslarc.utils.shell import CommandRunner


def run(
    config: Config,
    yes: bool = False,
    dry_run: bool = False,
    runner: CommandRunner | None = None,
) -> None:
    runner = runner or CommandRunner(dry_run=dry_run)
    if not confirm_or_yes("Disable all mount units?", False, yes):
        print("Aborted.")
        return
    units = systemd.managed_mount_units(config)
    if any(item.mount == "/usr" for item in config.subvolumes.backup.values()):
        units.append(hooks.ext4_mount_unit_filename(config))
        # Installations that predate these units do not have them yet.
        units.extend(
            unit
            for unit in (systemd.USR_PRE_UNIT, systemd.USR_POST_UNIT)
            if (systemd.UNIT_DIR / unit).exists()
        )
    for unit in sorted(set(units)):
        runner.run("systemctl", ["disable", unit])
        info(f"{unit} disabled")
    if runner.dry_run:
        info(f"[dry-run] Would remove {systemd.USER_DROPIN_PATH}")
    else:
        systemd.USER_DROPIN_PATH.unlink(missing_ok=True)
    runner.run("systemctl", ["disable", "btrbk.timer"])
    success("All mount units disabled")

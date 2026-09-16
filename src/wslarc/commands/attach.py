"""Attach the configured VHDX through WSL interop."""

from __future__ import annotations

from wslarc.config import Config
from wslarc.utils.block import find_btrfs_device_by_label
from wslarc.utils.shell import CommandRunner


def repair_binfmt(runner: CommandRunner | None = None) -> None:
    runner = runner or CommandRunner()
    runner.run(
        "sudo",
        [
            "sh",
            "-c",
            "echo :WSLInterop:M::MZ::/init:PF > /usr/lib/binfmt.d/WSLInterop.conf",
        ],
    )
    runner.run("sudo", ["systemctl", "unmask", "systemd-binfmt.service"])
    runner.run("sudo", ["systemctl", "restart", "systemd-binfmt"])
    runner.run("sudo", ["systemctl", "mask", "systemd-binfmt.service"])


def _run_once(config: Config, runner: CommandRunner) -> None:
    runner.run("/usr/lib/systemd/systemd-binfmt", [])
    if find_btrfs_device_by_label(config.vhdx.label):
        return
    windows_path = config.vhdx.path.replace("/", "\\")
    runner.run("/mnt/c/Windows/System32/wsl.exe", ["--mount", "--vhd", windows_path, "--bare"])


def run(config: Config, runner: CommandRunner | None = None) -> None:
    runner = runner or CommandRunner()
    try:
        _run_once(config, runner)
    except Exception as first_error:
        print(f"warning: attach failed: {first_error}; repairing WSLInterop and retrying once")
        repair_binfmt(runner)
        try:
            _run_once(config, runner)
        except Exception as retry_error:
            raise RuntimeError(
                f"attach retry failed after initial error: {first_error}"
            ) from retry_error

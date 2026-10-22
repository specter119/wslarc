"""Windows host helpers reached through WSL interop."""

from __future__ import annotations

from wslarc.utils.shell import CommandError, CommandRunner

CMD_EXE = "/mnt/c/Windows/System32/cmd.exe"
DEFAULT_VHDX_RELATIVE_PATH = r".local\share\wslarc\btrfs.vhdx"


def windows_user_profile() -> str | None:
    try:
        output = CommandRunner().run(CMD_EXE, ["/c", "echo", "%USERPROFILE%"], cwd="/mnt/c")
    except CommandError:
        return None
    # cmd.exe leaves the variable unexpanded when it is undefined.
    profile = output.strip()
    return profile if profile and "%" not in profile else None


def default_vhdx_path() -> str:
    profile = windows_user_profile()
    return f"{profile}\\{DEFAULT_VHDX_RELATIVE_PATH}" if profile else ""

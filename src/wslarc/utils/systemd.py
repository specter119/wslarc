"""Small systemd installation helpers."""

from __future__ import annotations

from pathlib import Path

from wslarc.utils.shell import CommandRunner


def systemctl(*args: str, runner: CommandRunner | None = None) -> str:
    return (runner or CommandRunner()).run("systemctl", list(args))


def write_unit(path: str | Path, content: str, *, runner: CommandRunner | None = None) -> None:
    if runner and runner.dry_run:
        print(f"[dry-run] write {path}")
        return
    destination = Path(path)
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(content)

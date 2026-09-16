"""Safe subprocess helpers with dry-run and streaming support."""

from __future__ import annotations

import os
import signal
import subprocess
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path


class CommandError(RuntimeError):
    """A command failed or could not be started."""


@dataclass
class CommandRunner:
    dry_run: bool = False

    def run(
        self,
        command: str,
        args: Sequence[str] = (),
        *,
        check: bool = True,
        env: Mapping[str, str] | None = None,
        cwd: str | Path | None = None,
        capture: bool = True,
        stream: bool = False,
    ) -> str:
        argv = [command, *args]
        if self.dry_run:
            print("[dry-run] " + shell_join(argv))
            return ""
        try:
            if stream:
                process = subprocess.Popen(
                    argv,
                    cwd=cwd,
                    env=dict(env) if env is not None else None,
                    stdout=None,
                    stderr=None,
                    text=True,
                    start_new_session=False,
                )
                code = process.wait()
                output = ""
            else:
                completed = subprocess.run(
                    argv,
                    cwd=cwd,
                    env=dict(env) if env is not None else None,
                    check=False,
                    capture_output=capture,
                    text=True,
                )
                code = completed.returncode
                output = completed.stdout
                if code and capture and completed.stderr:
                    output = f"{output}{completed.stderr}"
        except OSError as exc:
            raise CommandError(f"failed to execute {' '.join(argv)}: {exc}") from exc
        if check and code:
            raise CommandError(f"command failed ({code}): {shell_join(argv)}\n{output.strip()}")
        return output


def shell_join(args: Iterable[str]) -> str:
    import shlex

    return shlex.join([str(item) for item in args])


def run(
    command: str,
    args: Sequence[str] = (),
    *,
    check: bool = True,
    env: Mapping[str, str] | None = None,
    cwd: str | Path | None = None,
) -> str:
    return CommandRunner().run(command, args, check=check, env=env, cwd=cwd)


def run_or_dry(command: str, args: Sequence[str] = (), dry_run: bool = False) -> str:
    return CommandRunner(dry_run=dry_run).run(command, args)


def run_with_output(command: str, args: Sequence[str] = (), *, stream: bool = True) -> str:
    return CommandRunner().run(command, args, stream=stream, capture=not stream)


def run_with_output_interruptible(
    command: str, args: Sequence[str] = (), *, dry_run: bool = False
) -> str:
    return CommandRunner(dry_run=dry_run).run(command, args, stream=True)


def atomic_write(path: str | Path, content: str) -> None:
    destination = Path(path)
    temporary = destination.with_name(f".{destination.name}.tmp")
    temporary.write_text(content)
    os.replace(temporary, destination)


def reset_sigint_default() -> None:
    signal.signal(signal.SIGINT, signal.SIG_DFL)

"""Filesystem safety and atomic-copy helpers."""

from __future__ import annotations

import os
import tempfile
from collections.abc import Sequence
from pathlib import Path

from wslarc.utils.block import MountInfo


def verify_mount(
    mount: MountInfo | None,
    target: str,
    fstype: str,
    uuid: str,
) -> None:
    if mount is None or mount.target != target or mount.fstype != fstype or mount.uuid != uuid:
        raise ValueError(
            f"refusing unexpected filesystem at {target}: expected {fstype} UUID={uuid}"
        )


def atomic_write(path: str | Path, content: str | bytes) -> None:
    destination = Path(path)
    destination.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary_name = tempfile.mkstemp(prefix=f".{destination.name}.", dir=destination.parent)
    temporary = Path(temporary_name)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(content.encode() if isinstance(content, str) else content)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, destination)
    finally:
        temporary.unlink(missing_ok=True)


def install_copies(source: str | Path, targets: Sequence[str | Path]) -> None:
    source_path = Path(source)
    data = source_path.read_bytes()
    mode = source_path.stat().st_mode
    for target in targets:
        destination = Path(target)
        destination.parent.mkdir(parents=True, exist_ok=True)
        fd, temporary_name = tempfile.mkstemp(
            prefix=f".{destination.name}.", dir=destination.parent
        )
        temporary = Path(temporary_name)
        try:
            with os.fdopen(fd, "wb") as stream:
                stream.write(data)
                stream.flush()
                os.fchmod(stream.fileno(), mode & 0o7777)
                os.fsync(stream.fileno())
            os.replace(temporary, destination)
        finally:
            temporary.unlink(missing_ok=True)

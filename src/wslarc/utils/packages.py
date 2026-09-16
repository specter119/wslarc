"""Package-manager adapters used by the systemd synchronization hook."""

from __future__ import annotations

import subprocess

from wslarc.utils.block import (
    PacmanPackage,
    parse_debian_depends,
    parse_debian_status_version,
    parse_pacman_depends,
    parse_pacman_package_info,
    parse_pacman_query_version,
)

PACMAN_SYNC_GUARD_ENV = "WSLARC_SYSTEMD_SYNC_IN_PROGRESS"


def pacman_query_version(package: str) -> str | None:
    result = subprocess.run(["pacman", "-Q", package], capture_output=True, text=True, check=False)
    return parse_pacman_query_version(result.stdout) if result.returncode == 0 else None


def pacman_query_package(package: str, root: str | None = None) -> PacmanPackage | None:
    args = ["-Qi", package] if root is None else ["--sysroot", root, "-Qi", package]
    result = subprocess.run(["pacman", *args], capture_output=True, text=True, check=False)
    return parse_pacman_package_info(result.stdout) if result.returncode == 0 else None


def pacman_query_depends(package: str) -> list[str]:
    result = subprocess.run(
        ["pacman", "-Qi", package], capture_output=True, text=True, check=False, env={"LC_ALL": "C"}
    )
    return parse_pacman_depends(result.stdout) if result.returncode == 0 else []


def debian_query_version(package: str) -> str | None:
    result = subprocess.run(
        ["dpkg-query", "-W", "-f", "${Status}\t${Version}", package],
        capture_output=True,
        text=True,
        check=False,
    )
    return parse_debian_status_version(result.stdout) if result.returncode == 0 else None


def debian_query_depends(package: str) -> list[str]:
    result = subprocess.run(
        ["dpkg-query", "-W", "-f", "${Depends}\n${Pre-Depends}", package],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode:
        return []
    selected = []
    for alternatives in parse_debian_depends(result.stdout):
        for candidate in alternatives:
            if debian_query_version(candidate):
                selected.append(candidate)
                break
    return selected


def debian_query_files(package: str) -> list[str]:
    result = subprocess.run(
        ["dpkg-query", "-L", package], capture_output=True, text=True, check=False
    )
    if result.returncode:
        return []
    return [
        line.strip() for line in result.stdout.splitlines() if line.startswith("/") and line != "/"
    ]


def debian_query_deb_package_name(path: str) -> str | None:
    result = subprocess.run(
        ["dpkg-deb", "-f", path, "Package"], capture_output=True, text=True, check=False
    )
    return result.stdout.strip() if result.returncode == 0 and result.stdout.strip() else None


def debian_file_owned_by_installed_package(path: str) -> bool:
    result = subprocess.run(["dpkg-query", "-S", path], capture_output=True, text=True, check=False)
    if result.returncode:
        return False
    for line in result.stdout.splitlines():
        package = line.split(":", 1)[0].strip()
        if package and debian_query_version(package):
            return True
    return False

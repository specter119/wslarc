"""Parsers for the systemd output the boot step reads."""

from __future__ import annotations


def parse_dependency_names(output: str) -> list[str]:
    """First column of ``systemctl list-dependencies --plain``, deduplicated and sorted."""
    return sorted({line.split()[0] for line in output.splitlines() if line.split()})


def parse_unit_properties(output: str) -> list[dict[str, str]]:
    """Parse blank-line-separated ``systemctl show`` blocks into key/value dicts."""
    return [
        dict(line.split("=", 1) for line in block.splitlines() if "=" in line)
        for block in output.split("\n\n")
        if block.strip()
    ]

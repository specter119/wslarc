"""Quoting helpers for shell and systemd command lines."""

from __future__ import annotations


def shell_argument(value: str) -> str:
    return "'" + value.replace("'", "'\\''") + "'"


def systemd_argument(value: str) -> str:
    value = value.replace("\\", "\\\\").replace('"', '\\"')
    return '"' + value.replace("%", "%%").replace("$", "$$") + '"'


def shell_command(config_path: str) -> str:
    return f"/usr/local/bin/wslarc --config {shell_argument(config_path)}"


def systemd_command(config_path: str) -> str:
    return f"/usr/local/bin/wslarc --config {systemd_argument(config_path)}"

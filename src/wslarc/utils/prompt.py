"""Small terminal prompt and output helpers."""

from __future__ import annotations


def confirm(message: str, default: bool = False) -> bool:
    suffix = " [Y/n] " if default else " [y/N] "
    answer = input(message + suffix).strip().lower()
    if not answer:
        return default
    return answer in {"y", "yes"}


def confirm_or_yes(message: str, default: bool, yes: bool) -> bool:
    return True if yes else confirm(message, default)


def input_default(message: str, default: str = "") -> str:
    answer = input(f"{message} [{default}] ").strip()
    return answer or default


def info(message: str) -> None:
    print(f"  → {message}")


def success(message: str) -> None:
    print(f"  ✓ {message}")


def warn(message: str) -> None:
    print(f"  ⚠ {message}")


def step(number: int, total: int, message: str) -> None:
    print(f"\n[{number}/{total}] {message}")


def section(title: str) -> None:
    print(f"\n{title}\n{'=' * len(title)}")


def kv(key: str, value: str) -> None:
    print(f"  {key}: {value}")

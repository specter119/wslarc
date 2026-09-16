"""Command-line entry point for wslarc."""

from __future__ import annotations

import argparse
import logging
import sys

from wslarc import __version__
from wslarc.config import Config, Distribution


def build_parser() -> argparse.ArgumentParser:
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("-c", "--config", default="/etc/wslarc/config.toml")
    common.add_argument("-y", "--yes", action="store_true", help="skip confirmation prompts")
    common.add_argument("-v", "--verbose", action="count", default=0)
    sub_common = argparse.ArgumentParser(add_help=False, argument_default=argparse.SUPPRESS)
    sub_common.add_argument("-c", "--config")
    sub_common.add_argument("-y", "--yes", action="store_true")
    sub_common.add_argument("-v", "--verbose", action="count")
    parser = argparse.ArgumentParser(
        description="WSL2 Btrfs backup and restore tool",
        parents=[common],
    )
    parser.add_argument("--version", action="version", version=f"wslarc {__version__}")
    commands = parser.add_subparsers(dest="command", required=True)

    init = commands.add_parser(
        "init", help="initialize Btrfs VHDX and create subvolumes", parents=[sub_common]
    )
    init.add_argument("--dry-run", action="store_true")
    mount = commands.add_parser(
        "mount", help="generate and install systemd mount units", parents=[sub_common]
    )
    mount.add_argument("--dry-run", action="store_true")
    umount = commands.add_parser("umount", help="disable systemd mount units", parents=[sub_common])
    umount.add_argument("--dry-run", action="store_true")
    commands.add_parser("status", help="show current status", parents=[sub_common])

    snapshot = commands.add_parser("snapshot", help="snapshot operations", parents=[sub_common])
    snapshot_commands = snapshot.add_subparsers(dest="snapshot_action", required=True)
    snapshot_commands.add_parser("run", help="create snapshots")
    snapshot_commands.add_parser("list", help="list snapshots")
    prune = snapshot_commands.add_parser("prune", help="preview or remove old snapshots")
    prune.add_argument("--dry-run", action="store_true")

    restore = commands.add_parser("restore", help="restore from a snapshot", parents=[sub_common])
    restore.add_argument("-s", "--snapshot")

    hook = commands.add_parser(
        "hook-sync-systemd", help="sync systemd packages to ext4", parents=[sub_common]
    )
    hook.add_argument("--dry-run", action="store_true")
    hook.add_argument("--apt-pre", action="store_true", help=argparse.SUPPRESS)
    hook.add_argument("--apt-post", action="store_true", help=argparse.SUPPRESS)
    commands.add_parser(
        "attach", help="attach Btrfs VHDX if not already mounted", parents=[sub_common]
    )
    return parser


def _load_config(path: str, command: str, distribution: Distribution) -> Config:
    if command == "init":
        return Config.load_or_default_unexpanded(path, distribution)
    return Config.load(path)


def run(args: argparse.Namespace) -> None:
    distribution = Distribution.detect()
    config = _load_config(args.config, args.command, distribution)
    if args.command == "init":
        from wslarc.commands.init import run as init_command

        init_command(config, distribution, args.config, args.yes, args.dry_run)
    elif args.command == "mount":
        from wslarc.commands.mount import run as mount_command

        mount_command(config, distribution, args.config, args.yes, args.dry_run)
    elif args.command == "umount":
        from wslarc.commands.umount import run as umount_command

        umount_command(config, args.yes, args.dry_run)
    elif args.command == "status":
        from wslarc.commands.status import run as command

        command(config, distribution)
    elif args.command == "snapshot":
        from wslarc.commands import snapshot

        if args.snapshot_action == "run":
            snapshot.run(config, distribution)
        elif args.snapshot_action == "list":
            snapshot.list_snapshots(config, distribution)
        else:
            snapshot.prune(config, distribution, args.yes, args.dry_run)
    elif args.command == "restore":
        from wslarc.commands.restore import run as restore_command

        restore_command(config, distribution, args.snapshot, args.yes)
    elif args.command == "hook-sync-systemd":
        from wslarc.commands import hook_sync_systemd

        if args.apt_pre:
            hook_sync_systemd.run_apt_pre()
        elif args.apt_post:
            hook_sync_systemd.run_apt_post(config, distribution, args.dry_run)
        else:
            hook_sync_systemd.run(config, distribution, args.dry_run)
    elif args.command == "attach":
        from wslarc.commands.attach import run as attach_command

        attach_command(config)


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    level = logging.WARNING
    if args.verbose == 1:
        level = logging.INFO
    elif args.verbose == 2:
        level = logging.DEBUG
    elif args.verbose >= 3:
        level = logging.NOTSET
    logging.basicConfig(level=level, format="%(levelname)s: %(message)s")
    try:
        run(args)
    except (OSError, RuntimeError, ValueError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

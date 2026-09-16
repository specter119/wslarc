from pathlib import Path

from wslarc.cli import build_parser
from wslarc.commands.attach import repair_binfmt
from wslarc.commands.init import (
    SyncKind,
    TargetState,
    build_sync_plan,
    render_resume_command,
)
from wslarc.config import Config, Distribution
from wslarc.utils.shell import CommandRunner


def test_first_init_skips_home_and_nix(tmp_path: Path) -> None:
    source = tmp_path / "usr"
    source.mkdir()
    config = Config.for_distribution(Distribution.ARCH)
    config.user.name = "alice"
    config.subvolumes.backup["@usr"].mount = str(source)
    plan = build_sync_plan(config, str(tmp_path / "mount"), True, False)
    skipped = {entry.subvol for entry in plan if entry.kind is SyncKind.BACKUP and not entry.copy}
    assert "@home" in skipped
    assert "@nix" in skipped
    assert any(entry.subvol == "@usr" and entry.copy for entry in plan)


def test_existing_config_skips_nonempty_backup_but_syncs_snapshot(tmp_path: Path) -> None:
    source = tmp_path / "source"
    source.mkdir()
    mount = tmp_path / "mount"
    (mount / "@usr").mkdir(parents=True)
    (mount / "@usr" / "existing").touch()
    (mount / "@etc").mkdir()
    config = Config.for_distribution(Distribution.ARCH)
    config.subvolumes.backup = {"@usr": config.subvolumes.backup["@usr"]}
    config.subvolumes.backup["@usr"].mount = str(source)
    config.subvolumes.snapshot_only["@etc"].source = str(source)
    plan = build_sync_plan(config, str(mount), False, False)
    by_name = {entry.subvol: entry for entry in plan}
    assert by_name["@usr"].state is TargetState.NON_EMPTY
    assert not by_name["@usr"].copy
    assert by_name["@etc"].copy


def test_resume_command_contains_top_level_mount_and_partial(tmp_path: Path) -> None:
    entry = next(
        item
        for item in build_sync_plan(
            Config.for_distribution(Distribution.ARCH),
            str(tmp_path),
            True,
            True,
        )
        if item.copy
    )
    command = render_resume_command("/dev/test", [entry])
    assert command is not None
    assert "subvolid=5" in command
    assert "--partial" in command
    assert "trap cleanup EXIT" in command


def test_cli_exposes_all_commands() -> None:
    parser = build_parser()
    args = parser.parse_args(["snapshot", "prune", "--dry-run"])
    assert args.snapshot_action == "prune"


def test_binfmt_repair_order() -> None:
    calls: list[tuple[str, list[str]]] = []

    class FakeRunner(CommandRunner):
        def run(self, command: str, args=(), **kwargs):  # type: ignore[no-untyped-def]
            calls.append((command, list(args)))
            return ""

    repair_binfmt(FakeRunner())
    assert [args[:2] for _, args in calls] == [
        ["sh", "-c"],
        ["systemctl", "unmask"],
        ["systemctl", "restart"],
        ["systemctl", "mask"],
    ]

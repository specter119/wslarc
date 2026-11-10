from collections.abc import Sequence
from pathlib import Path

import pytest

from wslarc.cli import build_parser
from wslarc.commands import init
from wslarc.commands.attach import repair_binfmt
from wslarc.commands.init import (
    SyncKind,
    SyncPlanEntry,
    TargetState,
    build_sync_plan,
    render_resume_command,
)
from wslarc.config import Config, Distribution
from wslarc.utils.block import MountInfo
from wslarc.utils.shell import CommandError, CommandRunner


def test_first_init_skips_home_and_nix(tmp_path: Path) -> None:
    source = tmp_path / "usr"
    source.mkdir()
    config = Config.for_distribution(Distribution.ARCH)
    config.user.name = "alice"
    config.subvolumes.backup["@usr"].mount = str(source)
    plan = build_sync_plan(config, str(tmp_path / "mount"), True, False, [])
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
    plan = build_sync_plan(config, str(mount), False, False, [])
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
            [],
        )
        if item.copy
    )
    command = render_resume_command("/dev/test", [entry])
    assert command is not None
    assert "subvolid=5" in command
    assert "--partial" in command
    assert "trap cleanup EXIT" in command


def test_resume_command_syncs_through_source_view() -> None:
    entry = SyncPlanEntry(
        "@usr", "/usr", "/target/@usr", SyncKind.BACKUP, ["bin/perf"], TargetState.EMPTY, True
    )
    command = render_resume_command("/dev/test", [entry])
    assert command is not None
    assert "source_view=$(mktemp -d /tmp/wslarc-source.XXXXXX)" in command
    assert "sudo mount --bind -o ro '/usr' \"$source_view\"" in command
    assert "--exclude /bin/perf" in command
    assert '"$source_view"/' in command
    assert 'sudo umount "$source_view"' in command


def test_init_plan_excludes_file_mounts(tmp_path: Path) -> None:
    source = tmp_path / "usr"
    (source / "lib/wsl/drivers").mkdir(parents=True)
    (source / "bin").mkdir(parents=True)
    (source / "bin/perf").touch()
    config = Config.for_distribution(Distribution.DEBIAN)
    config.user.name = "alice"
    config.subvolumes.backup["@usr"].mount = str(source)
    mounts = [
        MountInfo(target=f"{source}/lib/wsl/drivers", source="drivers", fstype="9p", options="ro"),
        MountInfo(target=f"{source}/bin/perf", source="/dev/sdb", fstype="ext4", options="ro"),
    ]
    plan = build_sync_plan(config, str(tmp_path / "mnt"), True, False, mounts)
    entry = next(item for item in plan if item.subvol == "@usr")
    assert entry.excluded_paths == ["bin/perf"]


class RecordingRunner(CommandRunner):
    def __init__(self, fail_on: str | None = None) -> None:
        super().__init__()
        self.calls: list[list[str]] = []
        self.fail_on = fail_on

    def run(self, command: str, args: Sequence[str] = (), **kwargs: object) -> str:  # type: ignore[override]
        self.calls.append([command, *args])
        if self.fail_on == command:
            raise CommandError("boom")
        return ""


def sync_entry(source: Path, target: Path) -> SyncPlanEntry:
    return SyncPlanEntry(
        "@usr", str(source), str(target), SyncKind.BACKUP, [], TargetState.EMPTY, True
    )


def test_sync_entry_uses_readonly_bind_view(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.setattr(init, "is_mountpoint", lambda path: True)
    runner = RecordingRunner()
    init._run_sync_entry(sync_entry(tmp_path / "usr", tmp_path / "mnt/@usr"), runner, tmp_path)
    mount_call, rsync_call, umount_call = runner.calls
    assert mount_call[:3] == ["mount", "--bind", "-o"]
    assert mount_call[3] == "ro"
    assert mount_call[4] == str(tmp_path / "usr")
    view = mount_call[5]
    assert rsync_call[0] == "rsync"
    assert f"{view}/" in rsync_call
    assert f"{tmp_path}/mnt/@usr/" in rsync_call
    assert umount_call == ["umount", view]
    assert not Path(view).exists()


def test_sync_entry_unmounts_view_on_rsync_failure(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.setattr(init, "is_mountpoint", lambda path: True)
    runner = RecordingRunner(fail_on="rsync")
    with pytest.raises(CommandError):
        init._run_sync_entry(sync_entry(tmp_path / "usr", tmp_path / "mnt/@usr"), runner, tmp_path)
    view = runner.calls[0][5]
    assert ["umount", view] in runner.calls
    assert not Path(view).exists()


def test_sync_entry_refuses_unmounted_view(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setattr(init, "is_mountpoint", lambda path: False)
    runner = RecordingRunner()
    with pytest.raises(RuntimeError, match="not a mount point"):
        init._run_sync_entry(sync_entry(tmp_path / "usr", tmp_path / "mnt/@usr"), runner, tmp_path)
    assert not any(call[0] == "rsync" for call in runner.calls)
    assert [call[0] for call in runner.calls] == ["mount", "umount"]


@pytest.mark.parametrize("phase", ["--pre", "--post"])
def test_cli_accepts_boot_usr_phases(phase: str) -> None:
    assert build_parser().parse_args(["boot-usr", phase]).command == "boot-usr"


def test_internal_commands_are_hidden_from_help() -> None:
    parser = build_parser()
    help_text = parser.format_help()
    assert "boot-usr" not in help_text
    assert "hook-sync-systemd" not in help_text
    assert parser.parse_args(["hook-sync-systemd"]).command == "hook-sync-systemd"


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

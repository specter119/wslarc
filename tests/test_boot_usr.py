from __future__ import annotations

import json
import threading
import time
from collections.abc import Sequence
from pathlib import Path

import pytest

from wslarc.commands import boot_usr
from wslarc.utils.block import MountInfo
from wslarc.utils.shell import CommandError, CommandRunner


@pytest.fixture(autouse=True)
def lock_in_tmp_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(boot_usr, "LOCK_PATH", tmp_path / "boot-usr.lock")


def mount(target: str, fstype: str = "9p") -> MountInfo:
    return MountInfo(target=target, source="source", fstype=fstype, options="rw")


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


def test_pre_stashes_directory_mounts_and_skips_file_mounts(tmp_path: Path) -> None:
    directory = tmp_path / "usr/lib/wsl"
    directory.mkdir(parents=True)
    file_target = tmp_path / "usr/bin/perf"
    file_target.parent.mkdir(parents=True)
    file_target.write_text("")
    mounts = [mount(str(directory)), mount(str(file_target), "ext4")]
    runner = RecordingRunner()
    records = boot_usr.run_pre(
        runner, mounts, str(tmp_path / "usr"), tmp_path / "stash", tmp_path / "records.json"
    )
    assert [record.target for record in records] == [str(directory)]
    assert (tmp_path / "stash/0").is_dir()
    assert runner.calls == [["mount", "--bind", str(directory), str(tmp_path / "stash/0")]]
    saved = json.loads((tmp_path / "records.json").read_text())
    assert saved == [{"target": str(directory), "stash": str(tmp_path / "stash/0")}]


def test_pre_is_noop_when_usr_already_mounted(tmp_path: Path) -> None:
    runner = RecordingRunner()
    mounts = [mount("/usr", "btrfs"), mount("/usr/x")]
    assert boot_usr.run_pre(runner, mounts, "/usr", tmp_path / "s", tmp_path / "r.json") == []
    assert runner.calls == []


def test_post_restores_mounts_then_repairs_shared_state(tmp_path: Path) -> None:
    usr = tmp_path / "usr"
    usr.mkdir()
    records_path = tmp_path / "records.json"
    target = usr / "lib/wsl/drivers"
    stash = tmp_path / "stash0"
    stash.mkdir()
    records_path.write_text(json.dumps([{"target": str(target), "stash": str(stash)}]))
    runner = RecordingRunner()
    boot_usr.run_post(runner, records_path, mounts=[mount(str(stash))])
    assert target.is_dir()
    assert runner.calls[0] == ["mount", "--bind", str(stash), str(target)]
    assert runner.calls[1] == ["umount", str(stash)]
    assert ["systemctl", "daemon-reload"] in runner.calls
    assert ["systemd-tmpfiles", "--create"] in runner.calls
    assert ["ldconfig", "-X"] in runner.calls
    assert [
        "systemctl",
        "--no-block",
        "restart",
        "systemd-sysctl.service",
        "systemd-modules-load.service",
    ] in runner.calls
    # The linker cache is shared with the ext4 root, so it is rebuilt only after
    # the restored mounts are in place and tmpfiles may have dropped ld.so.conf.d bits.
    assert runner.calls.index(["ldconfig", "-X"]) > runner.calls.index(
        ["systemd-tmpfiles", "--create"]
    )
    assert not any(call[0] == "udevadm" for call in runner.calls)
    assert not records_path.exists()


def test_post_restores_nested_child_after_parent(tmp_path: Path) -> None:
    parent, child = tmp_path / "usr/a", tmp_path / "usr/a/b"
    stashes = [tmp_path / "s0", tmp_path / "s1"]
    for stash in stashes:
        stash.mkdir()
    records_path = tmp_path / "records.json"
    # Listed child first on purpose: the restore order comes from the depth, not the file.
    records_path.write_text(
        json.dumps(
            [
                {"target": str(child), "stash": str(stashes[1])},
                {"target": str(parent), "stash": str(stashes[0])},
            ]
        )
    )
    runner = RecordingRunner()
    boot_usr.run_post(runner, records_path, mounts=[mount(str(stash)) for stash in stashes])
    binds = [call for call in runner.calls if call[:2] == ["mount", "--bind"]]
    assert [call[3] for call in binds] == [str(parent), str(child)]


def test_post_keeps_records_for_failed_restores(tmp_path: Path) -> None:
    stash = tmp_path / "s0"
    stash.mkdir()
    records_path = tmp_path / "records.json"
    entry = {"target": str(tmp_path / "usr/x"), "stash": str(stash)}
    records_path.write_text(json.dumps([entry]))
    boot_usr.run_post(RecordingRunner(fail_on="mount"), records_path, mounts=[mount(str(stash))])
    assert json.loads(records_path.read_text()) == [entry]


def test_post_continues_after_a_failed_step(tmp_path: Path) -> None:
    runner = RecordingRunner(fail_on="systemctl")
    boot_usr.run_post(runner, tmp_path / "missing.json")
    commands = [call[0] for call in runner.calls]
    assert "ldconfig" in commands
    assert "systemd-tmpfiles" in commands


class UnitRunner(RecordingRunner):
    LISTING = (
        "default.target\n"
        " avahi-daemon.service\n"
        " cron.service\n"
        " ldconfig.service\n"
        " missing.service\n"
        " -.mount\n"
        " nix-daemon.socket\n"
        " first-boot-complete.target\n"
        " conditional.service\n"
    )
    PROPERTIES = (
        "Id=avahi-daemon.service\nLoadState=loaded\nActiveState=inactive\n"
        "InactiveExitTimestampMonotonic=0\nConditionTimestampMonotonic=0\nConflicts=\n\n"
        "Id=cron.service\nLoadState=loaded\nActiveState=active\n"
        "InactiveExitTimestampMonotonic=5\nConditionTimestampMonotonic=5\n\n"
        "Id=ldconfig.service\nLoadState=loaded\nActiveState=inactive\n"
        "InactiveExitTimestampMonotonic=7\nConditionTimestampMonotonic=7\n\n"
        "Id=missing.service\nLoadState=not-found\nActiveState=inactive\n"
        "InactiveExitTimestampMonotonic=0\nConditionTimestampMonotonic=0\n\n"
        "Id=nix-daemon.socket\nLoadState=loaded\nActiveState=inactive\n"
        "InactiveExitTimestampMonotonic=0\nConditionTimestampMonotonic=0\n"
        "RefuseManualStart=no\nConflicts=\n\n"
        "Id=first-boot-complete.target\nLoadState=loaded\nActiveState=inactive\n"
        "InactiveExitTimestampMonotonic=0\nConditionTimestampMonotonic=0\n"
        "RefuseManualStart=yes\n\n"
        "Id=conditional.service\nLoadState=loaded\nActiveState=inactive\n"
        "InactiveExitTimestampMonotonic=0\nConditionTimestampMonotonic=12345\n"
    )
    DBUS = "ActiveState=active\nMainPID=0\n"
    CONFLICT_PROPS = ""

    def run(self, command: str, args: Sequence[str] = (), **kwargs: object) -> str:  # type: ignore[override]
        super().run(command, args, **kwargs)
        if args[:1] == ["list-dependencies"]:
            return self.LISTING
        if args[:1] == ["show"]:
            properties = args[1]
            if "MainPID" in properties:
                return self.DBUS
            if properties == "--property=Id,ActiveState":
                return self.CONFLICT_PROPS
            return self.PROPERTIES
        return ""


def start_calls(runner: RecordingRunner) -> list[list[str]]:
    return [call for call in runner.calls if call[1:3] == ["--no-block", "--job-mode=fail"]]


def test_post_starts_only_units_that_never_left_inactive(tmp_path: Path) -> None:
    runner = UnitRunner()
    boot_usr.run_post(runner, tmp_path / "missing.json")
    shown = next(call for call in runner.calls if call[1:2] == ["show"] and "Conflicts" in call[2])
    assert "-.mount" not in shown
    # One job per unit: on systemd >= 262 a multi-unit start is a single transaction,
    # where --job-mode=fail would drop every unit when a single one conflicts.
    assert start_calls(runner) == [
        ["systemctl", "--no-block", "--job-mode=fail", "start", "avahi-daemon.service"],
        ["systemctl", "--no-block", "--job-mode=fail", "start", "nix-daemon.socket"],
    ]
    assert ["systemctl", "--no-block", "start", "default.target"] not in runner.calls
    assert ["systemctl", "reload", "dbus.service"] in runner.calls
    # The daemon reload must happen before the start calls, or units shipped only
    # in @usr would not be loaded when systemctl show queries them.
    assert runner.calls.index(["systemctl", "daemon-reload"]) < runner.calls.index(
        start_calls(runner)[0]
    )


def test_post_tolerates_corrupt_records(tmp_path: Path, capsys: pytest.CaptureFixture) -> None:
    records_path = tmp_path / "records.json"
    records_path.write_text("{not json")
    runner = RecordingRunner()
    boot_usr.run_post(runner, records_path)
    assert "could not read" in capsys.readouterr().out
    # The repair steps still run, and the corrupt file is cleared.
    assert ["systemctl", "daemon-reload"] in runner.calls
    assert not records_path.exists()


def test_post_skips_malformed_records(tmp_path: Path, capsys: pytest.CaptureFixture) -> None:
    stash = tmp_path / "s0"
    stash.mkdir()
    records_path = tmp_path / "records.json"
    records_path.write_text(
        json.dumps(
            [
                {"stash": str(stash)},
                "junk",
                {"target": str(tmp_path / "usr/lib/wsl"), "stash": str(stash)},
            ]
        )
    )
    runner = RecordingRunner()
    boot_usr.run_post(runner, records_path, mounts=[mount(str(stash))])
    binds = [call for call in runner.calls if call[:2] == ["mount", "--bind"]]
    assert [call[3] for call in binds] == [str(tmp_path / "usr/lib/wsl")]
    assert "malformed stash record" in capsys.readouterr().out


def test_post_skips_units_conflicting_with_active(
    tmp_path: Path, capsys: pytest.CaptureFixture
) -> None:
    class ConflictRunner(UnitRunner):
        LISTING = "default.target\n cron.service\n"
        PROPERTIES = (
            "Id=cron.service\nLoadState=loaded\nActiveState=inactive\n"
            "InactiveExitTimestampMonotonic=0\nConditionTimestampMonotonic=0\n"
            "Conflicts=anacron.service\n"
        )
        CONFLICT_PROPS = "Id=anacron.service\nActiveState=active\n"

    runner = ConflictRunner()
    boot_usr.run_post(runner, tmp_path / "missing.json")
    assert start_calls(runner) == []
    assert "conflicts with active anacron.service" in capsys.readouterr().out


def test_post_skips_dbus_reload_in_foreign_namespace(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture
) -> None:
    class ForeignBusRunner(UnitRunner):
        DBUS = "ActiveState=active\nMainPID=4242\n"

    monkeypatch.setattr(boot_usr, "mount_namespace_id", lambda pid: f"mnt:[{pid}]")
    runner = ForeignBusRunner()
    boot_usr.run_post(runner, tmp_path / "missing.json")
    assert ["systemctl", "reload", "dbus.service"] not in runner.calls
    assert "ext4_sync.extra_packages" in capsys.readouterr().out


def test_post_reloads_dbus_in_shared_namespace(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    class PidBusRunner(UnitRunner):
        DBUS = "ActiveState=active\nMainPID=4242\n"

    monkeypatch.setattr(boot_usr, "mount_namespace_id", lambda pid: "mnt:[4026531841]")
    runner = PidBusRunner()
    boot_usr.run_post(runner, tmp_path / "missing.json")
    assert ["systemctl", "reload", "dbus.service"] in runner.calls


def test_post_skips_dbus_reload_when_inactive(tmp_path: Path) -> None:
    class InactiveBusRunner(UnitRunner):
        DBUS = "ActiveState=inactive\nMainPID=0\n"

    runner = InactiveBusRunner()
    boot_usr.run_post(runner, tmp_path / "missing.json")
    assert ["systemctl", "reload", "dbus.service"] not in runner.calls


def test_post_drops_the_record_before_unmounting_the_stash(tmp_path: Path) -> None:
    """An interruption after the bind must leave a mounted stash, never a bare record."""
    stash = tmp_path / "s0"
    stash.mkdir()
    records_path = tmp_path / "records.json"
    records_path.write_text(json.dumps([{"target": str(tmp_path / "usr/x"), "stash": str(stash)}]))
    seen: list[str] = []

    class InspectRunner(RecordingRunner):
        def run(self, command: str, args: Sequence[str] = (), **kwargs: object) -> str:  # type: ignore[override]
            if command == "umount":
                seen.append(records_path.read_text() if records_path.exists() else "")
            return super().run(command, args, **kwargs)

    boot_usr.run_post(InspectRunner(), records_path, mounts=[mount(str(stash))])
    assert seen == [""]


def test_post_does_not_bind_a_stash_that_is_no_longer_mounted(
    tmp_path: Path, capsys: pytest.CaptureFixture
) -> None:
    stash = tmp_path / "s0"
    stash.mkdir()
    records_path = tmp_path / "records.json"
    records_path.write_text(json.dumps([{"target": str(tmp_path / "usr/x"), "stash": str(stash)}]))
    runner = RecordingRunner()
    boot_usr.run_post(runner, records_path, mounts=[])
    assert not any(call[:2] == ["mount", "--bind"] for call in runner.calls)
    assert not any(call[0] == "umount" for call in runner.calls)
    assert not records_path.exists()
    assert "no longer a mount" in capsys.readouterr().out


def test_post_keeps_children_when_the_parent_restore_fails(
    tmp_path: Path, capsys: pytest.CaptureFixture
) -> None:
    parent, child = tmp_path / "usr/a", tmp_path / "usr/a/b"
    stashes = [tmp_path / "s0", tmp_path / "s1"]
    for stash in stashes:
        stash.mkdir()
    records_path = tmp_path / "records.json"
    records_path.write_text(
        json.dumps(
            [
                {"target": str(parent), "stash": str(stashes[0])},
                {"target": str(child), "stash": str(stashes[1])},
            ]
        )
    )

    class ParentFails(RecordingRunner):
        def run(self, command: str, args: Sequence[str] = (), **kwargs: object) -> str:  # type: ignore[override]
            if command == "mount" and args[-1] == str(parent):
                raise CommandError("boom")
            return super().run(command, args, **kwargs)

    runner = ParentFails()
    boot_usr.run_post(runner, records_path, mounts=[mount(str(stash)) for stash in stashes])
    # The child stash survives for a later run and the child is not restored yet.
    binds = [call for call in runner.calls if call[:2] == ["mount", "--bind"]]
    assert str(child) not in [call[3] for call in binds]
    assert not any(call[0] == "umount" for call in runner.calls)
    assert json.loads(records_path.read_text()) == [
        {"target": str(parent), "stash": str(stashes[0])},
        {"target": str(child), "stash": str(stashes[1])},
    ]
    assert "was not restored" in capsys.readouterr().out


def test_post_restores_siblings_of_a_failed_parent(
    tmp_path: Path, capsys: pytest.CaptureFixture
) -> None:
    parent = tmp_path / "usr/a/b"
    child = parent / "c"
    sibling = tmp_path / "usr/a/d"
    stashes = [tmp_path / "s0", tmp_path / "s1", tmp_path / "s2"]
    for stash in stashes:
        stash.mkdir()
    records_path = tmp_path / "records.json"
    records_path.write_text(
        json.dumps(
            [
                {"target": str(parent), "stash": str(stashes[0])},
                {"target": str(child), "stash": str(stashes[1])},
                {"target": str(sibling), "stash": str(stashes[2])},
            ]
        )
    )

    class ParentFails(RecordingRunner):
        def run(self, command: str, args: Sequence[str] = (), **kwargs: object) -> str:  # type: ignore[override]
            if command == "mount" and args[-1] == str(parent):
                raise CommandError("boom")
            return super().run(command, args, **kwargs)

    runner = ParentFails()
    boot_usr.run_post(runner, records_path, mounts=[mount(str(stash)) for stash in stashes])
    binds = [call[3] for call in runner.calls if call[:2] == ["mount", "--bind"]]
    assert binds == [str(sibling)]
    assert json.loads(records_path.read_text()) == [
        {"target": str(parent), "stash": str(stashes[0])},
        {"target": str(child), "stash": str(stashes[1])},
    ]
    assert "was not restored" in capsys.readouterr().out


def test_pre_does_not_restash_while_usr_is_mounted(tmp_path: Path) -> None:
    """A dropped record must not turn a same-boot re-run into a second stash of the same path."""
    records_path = tmp_path / "records.json"
    runner = RecordingRunner()
    assert (
        boot_usr.run_pre(runner, [mount("/usr", "btrfs")], "/usr", tmp_path / "s", records_path)
        == []
    )
    assert runner.calls == []
    assert not records_path.exists()


def test_pre_keeps_records_of_an_earlier_run(tmp_path: Path) -> None:
    """A second --pre must not re-bind the same stash or lose valid records when it fails."""
    usr = tmp_path / "usr"
    (usr / "a").mkdir(parents=True)
    (usr / "b").mkdir(parents=True)
    first = [mount(str(usr / "a")), mount(str(usr / "b"))]
    records_path = tmp_path / "records.json"
    runner = RecordingRunner()
    records = boot_usr.run_pre(runner, first, str(usr), tmp_path / "s", records_path)
    assert [record.target for record in records] == [str(usr / "a"), str(usr / "b")]

    class SecondBindFails(RecordingRunner):
        def run(self, command: str, args: Sequence[str] = (), **kwargs: object) -> str:  # type: ignore[override]
            if command == "mount" and args[1] == str(usr / "b"):
                raise CommandError("boom")
            return super().run(command, args, **kwargs)

    again = SecondBindFails()
    records = boot_usr.run_pre(again, first, str(usr), tmp_path / "s", records_path)
    assert [record.target for record in records] == [str(usr / "a"), str(usr / "b")]
    assert not any(call[:2] == ["mount", "--bind"] for call in again.calls)
    assert json.loads(records_path.read_text()) == [
        {"target": str(usr / "a"), "stash": str(tmp_path / "s/0")},
        {"target": str(usr / "b"), "stash": str(tmp_path / "s/1")},
    ]


def test_pre_stashes_a_new_mount_beside_a_recorded_one(tmp_path: Path) -> None:
    usr = tmp_path / "usr"
    (usr / "a").mkdir(parents=True)
    (usr / "c").mkdir(parents=True)
    records_path = tmp_path / "records.json"
    boot_usr.run_pre(
        RecordingRunner(), [mount(str(usr / "a"))], str(usr), tmp_path / "s", records_path
    )
    runner = RecordingRunner()
    records = boot_usr.run_pre(
        runner,
        [mount(str(usr / "a")), mount(str(usr / "c"))],
        str(usr),
        tmp_path / "s",
        records_path,
    )
    assert [record.target for record in records] == [str(usr / "a"), str(usr / "c")]
    binds = [call for call in runner.calls if call[:2] == ["mount", "--bind"]]
    assert binds == [["mount", "--bind", str(usr / "c"), str(tmp_path / "s/1")]]


def test_pre_fails_the_unit_when_a_stash_cannot_be_made(
    tmp_path: Path, capsys: pytest.CaptureFixture
) -> None:
    """usr.mount Requires the pre unit: a stash that did not happen must abort the overmount."""
    usr = tmp_path / "usr"
    (usr / "a").mkdir(parents=True)
    with pytest.raises(RuntimeError, match="could not stash 1 mount"):
        boot_usr.run_pre(
            RecordingRunner(fail_on="mount"),
            [mount(str(usr / "a"))],
            str(usr),
            tmp_path / "s",
            tmp_path / "records.json",
        )
    assert "could not stash" in capsys.readouterr().out


def test_pre_keeps_the_stashes_it_did_make_when_it_fails(tmp_path: Path) -> None:
    usr = tmp_path / "usr"
    (usr / "a").mkdir(parents=True)
    (usr / "b").mkdir(parents=True)
    records_path = tmp_path / "records.json"

    class SecondFails(RecordingRunner):
        def run(self, command: str, args: Sequence[str] = (), **kwargs: object) -> str:  # type: ignore[override]
            if command == "mount" and args[1] == str(usr / "b"):
                raise CommandError("boom")
            return super().run(command, args, **kwargs)

    with pytest.raises(RuntimeError):
        boot_usr.run_pre(
            SecondFails(),
            [mount(str(usr / "a")), mount(str(usr / "b"))],
            str(usr),
            tmp_path / "s",
            records_path,
        )
    assert json.loads(records_path.read_text()) == [
        {"target": str(usr / "a"), "stash": str(tmp_path / "s/0")}
    ]


def test_post_runs_its_repairs_even_when_restoring_raises(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture
) -> None:
    def boom(*args: object, **kwargs: object) -> None:
        raise CommandError("records unreadable")

    monkeypatch.setattr(boot_usr, "_restore_records", boom)
    runner = RecordingRunner()
    boot_usr.run_post(runner, tmp_path / "records.json")
    assert ["systemctl", "daemon-reload"] in runner.calls
    assert "restoring stashed mounts failed" in capsys.readouterr().out


def test_pre_and_post_serialize_on_one_lock(tmp_path: Path) -> None:
    """A manual `wslarc boot-usr` beside the unit must not act on a stale mount table."""
    order: list[str] = []
    holding = threading.Event()

    def hold() -> None:
        with boot_usr._transaction_lock(boot_usr.LOCK_PATH):
            order.append("first")
            holding.set()
            time.sleep(0.1)
            order.append("first-done")

    thread = threading.Thread(target=hold)
    thread.start()
    assert holding.wait(2)
    with boot_usr._transaction_lock(boot_usr.LOCK_PATH):
        order.append("second")
    thread.join()
    assert order == ["first", "first-done", "second"]
    assert boot_usr.LOCK_PATH.exists()

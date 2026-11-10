"""Keep WSL-injected mounts under /usr visible across the Btrfs /usr mount.

WSL mounts drivers, GPU libraries, kernel modules and ``perf`` below ``/usr`` of
the ext4 root before systemd starts. Mounting the ``@usr`` subvolume over ``/usr``
hides all of them. ``--pre`` runs before the overmount and parks each of those
mounts under ``/run``; ``--post`` runs after the managed mounts are in place,
bind-mounts them back, and makes systemd pick up units that only exist in ``@usr``.
"""

from __future__ import annotations

import fcntl
import json
import os
from collections.abc import Generator
from contextlib import contextmanager
from dataclasses import asdict, dataclass
from pathlib import Path

from wslarc.utils.block import MountInfo, list_mounts, mount_namespace_id, mounts_under
from wslarc.utils.prompt import info, warn
from wslarc.utils.shell import CommandError, CommandRunner
from wslarc.utils.storage import atomic_write
from wslarc.utils.systemd import parse_dependency_names, parse_unit_properties

USR = "/usr"
STASH_ROOT = Path("/run/wslarc/usr-submounts")
RECORDS_PATH = Path("/run/wslarc/usr-submounts.json")
LOCK_PATH = Path("/run/wslarc/boot-usr.lock")

POST_COMMANDS: tuple[tuple[str, tuple[str, ...]], ...] = (
    ("systemctl", ("daemon-reload",)),
    ("systemd-tmpfiles", ("--create",)),
    # /etc/ld.so.cache is shared with the ext4 root, and both WSL and ldconfig.service
    # rebuilt it from the ext4 view of /usr during boot. Rebuild it now that @usr and
    # the restored WSL mounts are in place. -X skips rebuilding the symlinks, which
    # belong to the package managers of the live view.
    ("ldconfig", ("-X",)),
    (
        "systemctl",
        ("--no-block", "restart", "systemd-sysctl.service", "systemd-modules-load.service"),
    ),
)

STARTABLE_SUFFIXES = (".service", ".socket", ".timer", ".path", ".target")


@contextmanager
def _transaction_lock(path: Path | None = None) -> Generator[None, None, None]:
    """Serialize pre and post across processes.

    Both steps read the same records file and mount table, so a second process that
    started with a stale view could bind an already emptied stash over a restored
    target. systemd serializes the units, but a manual `wslarc boot-usr` can run
    beside them.
    """
    lock = path or LOCK_PATH
    lock.parent.mkdir(parents=True, exist_ok=True)
    with lock.open("a+") as handle:
        fcntl.flock(handle, fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(handle, fcntl.LOCK_UN)


@dataclass(frozen=True)
class StashRecord:
    target: str
    stash: str


def run_pre(
    runner: CommandRunner | None = None,
    mounts: list[MountInfo] | None = None,
    usr: str = USR,
    stash_root: Path = STASH_ROOT,
    records_path: Path = RECORDS_PATH,
    lock_path: Path | None = None,
) -> list[StashRecord]:
    with _transaction_lock(lock_path):
        return _stash_mounts(runner, mounts, usr, stash_root, records_path)


def _stash_mounts(
    runner: CommandRunner | None,
    mounts: list[MountInfo] | None,
    usr: str,
    stash_root: Path,
    records_path: Path,
) -> list[StashRecord]:
    runner = runner or CommandRunner()
    mounts = list_mounts() if mounts is None else mounts
    if any(mount.target == usr for mount in mounts):
        info(f"{usr} is already a mount point; nothing to stash")
        return []
    # Reuse the records of an earlier run of this unit: a second run must not stack
    # another bind on the same stash, nor drop records that are still valid when it
    # fails partway through.
    records = load_records(records_path)
    known = {record.target for record in records}
    taken = {Path(record.stash).name for record in records}
    failed: list[str] = []
    for mount in mounts_under(mounts, usr):
        if not Path(mount.target).is_dir():
            # dpkg replaces a file by renaming over it, which fails on a mount point
            # (EXDEV), so a file mount such as /usr/bin/perf would block upgrades of
            # the package that owns it. Leave it hidden below @usr.
            info(f"leaving file mount {mount.target} hidden below {usr}")
            continue
        if mount.target in known:
            info(f"{mount.target} is already stashed")
            continue
        index = 0
        while str(index) in taken:
            index += 1
        stash = stash_root / str(index)
        try:
            stash.mkdir(parents=True, exist_ok=True)
            runner.run("mount", ["--bind", mount.target, str(stash)])
        except (CommandError, OSError) as exc:
            warn(f"could not stash {mount.target}: {exc}")
            failed.append(mount.target)
            continue
        records.append(StashRecord(mount.target, str(stash)))
        known.add(mount.target)
        taken.add(str(index))
        # Persist each stash as it is made: an interruption keeps what is already parked.
        atomic_write(records_path, json.dumps([asdict(record) for record in records]))
    if failed:
        # usr.mount Requires this unit: exiting non-zero keeps /usr on the ext4 view
        # instead of hiding mounts that were never recorded.
        raise RuntimeError(f"could not stash {len(failed)} mount(s) below {usr}")
    return records


def load_records(records_path: Path = RECORDS_PATH) -> list[StashRecord]:
    try:
        raw = json.loads(records_path.read_text())
    except FileNotFoundError:
        return []
    except (OSError, json.JSONDecodeError) as exc:
        # A corrupt records file must not stop the post unit before its repair
        # steps (daemon-reload, ldconfig) run; the restores are lost either way.
        warn(f"could not read {records_path}: {exc}")
        return []
    if not isinstance(raw, list):
        warn(f"could not read {records_path}: expected a list of records")
        return []
    records: list[StashRecord] = []
    for item in raw:
        if not isinstance(item, dict):
            warn(f"skipping malformed stash record: {item!r}")
            continue
        try:
            records.append(StashRecord(str(item["target"]), str(item["stash"])))
        except KeyError as exc:
            warn(f"skipping malformed stash record {item!r}: missing {exc}")
    return records


def _restore(record: StashRecord, runner: CommandRunner, mounts: list[MountInfo]) -> bool:
    """Bind the stash back onto its target; return True when the stash is still mounted.

    A stash that is no longer a mount means an earlier run already restored this target
    and died before dropping the record. Binding that leftover empty directory over the
    restored mount would hide the WSL content, so the record is simply dropped.
    """
    if not any(mount.target == record.stash for mount in mounts):
        warn(f"stash {record.stash} is no longer a mount; assuming {record.target} is restored")
        return False
    target = Path(record.target)
    # A bind mount shares the device and inode of its source, so this also holds for
    # children whose parent was just restored and which are still uncovered.
    try:
        if os.path.samefile(record.target, record.stash):
            info(f"{record.target} is already mounted")
            return True
    except OSError:
        pass
    target.mkdir(parents=True, exist_ok=True)
    runner.run("mount", ["--bind", record.stash, record.target])
    info(f"restored {record.target}")
    return True


def _write_records(records_path: Path, records: list[StashRecord]) -> None:
    if not records:
        records_path.unlink(missing_ok=True)
        return
    atomic_write(records_path, json.dumps([asdict(record) for record in records]))


def _is_under(path: str, ancestor: str) -> bool:
    return path == ancestor or path.startswith(ancestor.rstrip("/") + "/")


def _restore_records(
    runner: CommandRunner, records_path: Path, mounts: list[MountInfo] | None
) -> None:
    """Restore every pending stash, parents first.

    A record is persisted as restored *before* its stash is unmounted: an interruption
    can then only leave a mounted stash behind, never a record pointing at an empty
    directory. When an ancestor fails, its descendants are left untouched so the stash
    that feeds them survives for the next run of this unit in the same boot.
    """
    outstanding = load_records(records_path)
    if not outstanding:
        records_path.unlink(missing_ok=True)
        return
    table = list_mounts() if mounts is None else mounts
    outstanding.sort(key=lambda record: record.target.count("/"))
    blocked: list[str] = []
    for record in list(outstanding):
        blocker = next((item for item in blocked if _is_under(record.target, item)), None)
        if blocker is not None:
            info(f"leaving {record.target} pending: {blocker} was not restored")
            continue
        try:
            stash_mounted = _restore(record, runner, table)
        except (CommandError, OSError) as exc:
            warn(f"could not restore {record.target}: {exc}")
            blocked.append(record.target)
            continue
        outstanding = [item for item in outstanding if item != record]
        _write_records(records_path, outstanding)
        if stash_mounted:
            runner.run("umount", [record.stash], check=False)
    _write_records(records_path, outstanding)


def _unit_properties(
    runner: CommandRunner, names: list[str], properties: list[str]
) -> list[dict[str, str]]:
    output = runner.run(
        "systemctl",
        ["show", f"--property={','.join(properties)}", "--", *names],
    )
    return parse_unit_properties(output)


def _drop_conflicts_with_active(
    runner: CommandRunner, pending: list[str], shown: dict[str, dict[str, str]]
) -> list[str]:
    conflicts = sorted(
        {name for unit in pending for name in shown[unit].get("Conflicts", "").split()}
    )
    if not conflicts:
        return pending
    active = {
        fields["Id"]
        for fields in _unit_properties(runner, conflicts, ["Id", "ActiveState"])
        if fields.get("Id") and fields.get("ActiveState") not in {"inactive", "failed"}
    }
    startable: list[str] = []
    for unit in pending:
        blocked = sorted(set(shown[unit].get("Conflicts", "").split()) & active)
        if blocked:
            # --job-mode=fail only refuses to enqueue a conflicting job; it does not
            # stop a start job from stopping the active unit named in Conflicts=.
            info(f"leaving {unit} alone: it conflicts with active {', '.join(blocked)}")
        else:
            startable.append(unit)
    # Conflicts between two pending units are not checked: under --job-mode=fail the
    # first one enqueued wins, which is acceptable for these boot-completion units.
    return startable


def never_started_units(runner: CommandRunner) -> list[str]:
    """Return units wanted by default.target that systemd has not started yet.

    The boot transaction was built from the ext4 root, so units whose files live only
    in @usr are loaded after the daemon reload but never queued. Starting
    ``default.target`` again does not help: it is still activating while this runs, so
    the new job merges into the existing one without expanding its dependencies.
    Units that already ran or were skipped by an unmet condition are left alone
    (``InactiveExitTimestampMonotonic`` and ``ConditionTimestampMonotonic`` are zero
    only for units that never entered the start path), and so are units that refuse
    manual starts or conflict with a unit that is currently active.
    """
    listing = runner.run(
        "systemctl",
        ["list-dependencies", "--plain", "--no-legend", "--no-pager", "--all", "default.target"],
    )
    names = [name for name in parse_dependency_names(listing) if name.endswith(STARTABLE_SUFFIXES)]
    if not names:
        return []
    shown = {
        fields["Id"]: fields
        for fields in _unit_properties(
            runner,
            names,
            [
                "Id",
                "LoadState",
                "ActiveState",
                "InactiveExitTimestampMonotonic",
                "ConditionTimestampMonotonic",
                "RefuseManualStart",
                "Conflicts",
            ],
        )
        if fields.get("Id")
    }
    pending = [
        unit
        for unit, fields in shown.items()
        if fields.get("LoadState") == "loaded"
        and fields.get("ActiveState") == "inactive"
        and fields.get("InactiveExitTimestampMonotonic") == "0"
        and fields.get("ConditionTimestampMonotonic") == "0"
        and fields.get("RefuseManualStart") != "yes"
    ]
    return _drop_conflicts_with_active(runner, pending, shown)


def _reload_dbus(runner: CommandRunner) -> None:
    """Reload the D-Bus policy unless the daemon is stuck in the ext4 mount view.

    The bus daemon read its policy from the ext4 root before @usr covered /usr, so
    services shipped only in @usr are refused their bus names until it rereads it.
    A daemon that started before usr.mount inside a private mount namespace (Arch's
    dbus-broker) keeps seeing the ext4 /usr forever, so reloading it is useless; the
    fix there is adding the package to ext4_sync.extra_packages.
    """
    shown = _unit_properties(runner, ["dbus.service"], ["ActiveState", "MainPID"])
    fields = shown[0] if shown else {}
    if fields.get("ActiveState") != "active":
        info("dbus.service is not active; skipping policy reload")
        return
    pid = fields.get("MainPID", "0")
    bus_namespace = mount_namespace_id(int(pid)) if pid.isdigit() and pid != "0" else None
    own_namespace = mount_namespace_id(os.getpid())
    if bus_namespace is not None and own_namespace is not None and bus_namespace != own_namespace:
        warn(
            "dbus.service runs in a mount namespace that predates the /usr mount; "
            "D-Bus policy shipped only in @usr stays unloaded. "
            "Add its package to ext4_sync.extra_packages."
        )
        return
    runner.run("systemctl", ["reload", "dbus.service"])


def run_post(
    runner: CommandRunner | None = None,
    records_path: Path = RECORDS_PATH,
    mounts: list[MountInfo] | None = None,
    lock_path: Path | None = None,
) -> None:
    with _transaction_lock(lock_path):
        _run_post_locked(runner, records_path, mounts)


def _run_post_locked(
    runner: CommandRunner | None,
    records_path: Path,
    mounts: list[MountInfo] | None,
) -> None:
    runner = runner or CommandRunner()
    try:
        _restore_records(runner, records_path, mounts)
    except (CommandError, OSError) as exc:
        # The repair steps below (daemon-reload, ldconfig, starting units) matter even
        # when a restore fails; a broken record must not skip them.
        warn(f"restoring stashed mounts failed: {exc}")
    for command, args in POST_COMMANDS:
        try:
            runner.run(command, list(args))
        except CommandError as exc:
            warn(str(exc))
    try:
        _reload_dbus(runner)
    except CommandError as exc:
        warn(str(exc))
    try:
        pending = never_started_units(runner)
    except CommandError as exc:
        warn(str(exc))
        return
    if not pending:
        return
    info(f"starting {len(pending)} units wanted by default.target that did not start during boot")
    for unit in pending:
        try:
            # One job per unit: since systemd 262 a multi-unit start is submitted as a
            # single transaction, where --job-mode=fail would drop every unit when a
            # single one conflicts with a running job.
            runner.run("systemctl", ["--no-block", "--job-mode=fail", "start", unit])
        except CommandError as exc:
            warn(str(exc))

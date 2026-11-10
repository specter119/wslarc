"""Behavior tests for the systemd ext4 sync hook, ported from the rust test module."""

from __future__ import annotations

from collections.abc import Sequence
from pathlib import Path

import pytest

from wslarc.commands import hook_sync_systemd as hook
from wslarc.config import Config, Distribution
from wslarc.generators import hooks
from wslarc.utils.block import PacmanPackage
from wslarc.utils.shell import CommandError, CommandRunner


def sample_hook_targets() -> list[str]:
    return ["systemd", "libgcrypt", "glibc"]


def package(name: str, version: str, architecture: str) -> PacmanPackage:
    return PacmanPackage(name, version, architecture)


def test_filter_sync_packages_syncs_full_closure_when_target_triggered() -> None:
    packages = hook.filter_sync_packages(["libgcrypt", "not-a-target"], sample_hook_targets())

    assert packages == sample_hook_targets()


def test_filter_sync_packages_falls_back_to_all_targets_without_stdin() -> None:
    packages = hook.filter_sync_packages([], sample_hook_targets())

    assert "systemd" in packages
    assert "libgcrypt" in packages


def test_filter_sync_packages_discards_unknown_triggers() -> None:
    packages = hook.filter_sync_packages(["unknown"], sample_hook_targets())

    assert packages == []


def test_parse_package_lines_sorts_and_deduplicates() -> None:
    assert hook.parse_package_lines("libb\nliba\nlibb\n") == ["liba", "libb"]


def test_directory_entries_are_not_syncable_files(tmp_path: Path) -> None:
    nested = tmp_path / "nested"
    nested.mkdir()
    file = tmp_path / "file"
    file.write_text("content")
    link = tmp_path / "link"
    link.symlink_to(file)

    assert not hook.is_syncable_file(str(nested))
    assert hook.is_syncable_file(str(file))
    assert hook.is_syncable_file(str(link))


def test_manifest_removes_files_no_longer_in_current_package_list(tmp_path: Path) -> None:
    stale = tmp_path / "etc" / "old.conf"
    stale.parent.mkdir(parents=True)
    stale.write_text("old")

    hook.remove_stale_files_with(str(tmp_path), ["/etc/old.conf"], [], lambda file: False)

    assert not stale.exists()


def test_manifest_keeps_files_still_owned_by_installed_package(tmp_path: Path) -> None:
    file = tmp_path / "etc" / "owned.conf"
    file.parent.mkdir(parents=True)
    file.write_text("owned")

    hook.remove_stale_files_with(str(tmp_path), ["/etc/owned.conf"], [], lambda file: True)

    assert file.exists()


def test_manifest_roundtrip_is_sorted_and_deduplicated(tmp_path: Path) -> None:
    path = tmp_path / "manifest"
    hook.write_manifest(path, ["/usr/lib/b", "/usr/lib/a"])

    assert hook.read_manifest(path) == ["/usr/lib/a", "/usr/lib/b"]


def test_archive_mapping_uses_real_name_version_and_architecture(tmp_path: Path) -> None:
    source = tmp_path / "cache"
    destination = tmp_path / "target-cache"
    source.mkdir()
    (source / "systemd-256.5-1-any.pkg.tar.zst").write_text("")
    (source / "glibc-2.40-1-x86_64.pkg.tar.zst").write_text("")

    def archive_package(path: Path) -> PacmanPackage | None:
        if path.name == "systemd-256.5-1-any.pkg.tar.zst":
            return package("systemd", "256.5-1", "any")
        if path.name == "glibc-2.40-1-x86_64.pkg.tar.zst":
            return package("glibc", "2.40-1", "x86_64")
        return None

    archives = hook.resolve_arch_archives(
        source,
        destination,
        [package("systemd", "256.5-1", "any"), package("glibc", "2.40-1", "x86_64")],
        archive_package,
    )

    assert archives[0].package.architecture == "any"
    assert archives[0].destination == destination / "systemd-256.5-1-any.pkg.tar.zst"
    assert archives[1].package.name == "glibc"


def test_missing_archive_reports_exact_required_metadata(tmp_path: Path) -> None:
    source = tmp_path / "cache"
    source.mkdir()
    (source / "systemd-256.4-1-any.pkg.tar.zst").write_text("")

    with pytest.raises(RuntimeError) as error:
        hook.resolve_arch_archives(
            source,
            tmp_path / "target-cache",
            [package("systemd", "256.5-1", "any")],
            lambda path: None,
        )

    assert "systemd 256.5-1 any" in str(error.value)
    assert "Required cached Arch archive is missing" in str(error.value)


def test_current_target_package_is_skipped_by_exact_metadata() -> None:
    current = package("systemd", "256.5-1", "any")

    def target_package(name: str) -> PacmanPackage | None:
        assert name == "systemd"
        return current

    packages = hook.select_arch_packages_to_install([current], target_package)

    assert packages == []


def test_apt_upgrade_failure_retains_pending_and_previous_state(tmp_path: Path) -> None:
    pending = tmp_path / "apt-pending"
    state = tmp_path / "apt-state"
    hook.write_package_file(pending, ["systemd"])
    state.write_text("systemd=256.4-1")
    called = False

    def sync(triggers: list[str]) -> None:
        nonlocal called
        called = True
        assert triggers == []
        raise RuntimeError("simulated sync failure")

    with pytest.raises(RuntimeError):
        hook.finish_apt_post(
            pending, state, ["systemd"], "systemd=256.5-1", "systemd=256.4-1", False, sync
        )

    assert called
    assert hook.read_package_file(pending) == ["systemd"]
    assert state.read_text() == "systemd=256.4-1"


def test_apt_initial_observation_does_not_force_full_sync(tmp_path: Path) -> None:
    pending = tmp_path / "apt-pending"
    state = tmp_path / "apt-state"
    hook.write_package_file(pending, ["curl"])
    called = False

    def sync(triggers: list[str]) -> None:
        nonlocal called
        called = True
        assert triggers == ["curl"]

    hook.finish_apt_post(pending, state, ["curl"], "systemd=256.5-1", None, False, sync)

    assert called
    assert state.read_text() == "systemd=256.5-1"
    assert not pending.exists()


def test_apt_dry_run_does_not_clear_pending_or_write_state(tmp_path: Path) -> None:
    pending = tmp_path / "apt-pending"
    state = tmp_path / "apt-state"
    hook.write_package_file(pending, ["systemd"])
    state.write_text("systemd=256.4-1")

    def sync(triggers: list[str]) -> None:
        assert triggers == ["systemd"]

    hook.finish_apt_post(
        pending, state, ["systemd"], "systemd=256.4-1", "systemd=256.4-1", True, sync
    )

    assert pending.exists()
    assert state.read_text() == "systemd=256.4-1"


def test_removed_root_is_absent_from_installed_package_state() -> None:
    state = hook.installed_package_state_with(
        ["systemd", "shared"], lambda package: "1.0-1" if package == "shared" else None
    )

    assert state == "shared=1.0-1"


def test_cleanup_failure_returns_error_without_replacing_manifest(tmp_path: Path) -> None:
    manifest = tmp_path / "manifest"
    hook.write_manifest(manifest, ["/etc/old.conf"])
    stale_directory = tmp_path / "etc" / "old.conf"
    stale_directory.mkdir(parents=True)

    with pytest.raises(RuntimeError):
        hook.remove_stale_files_with(str(tmp_path), ["/etc/old.conf"], [], lambda file: False)

    assert manifest.read_text() == "/etc/old.conf\n"


def test_record_apt_pending_resolves_deb_paths_and_accumulates(tmp_path: Path) -> None:
    pending = tmp_path / "apt-pending"
    hook.write_package_file(pending, ["systemd"])

    hook.record_apt_pending(
        pending,
        "/var/cache/apt/curl.deb\n/tmp/notes.txt\n",
        lambda path: "curl" if path.endswith("curl.deb") else None,
    )

    assert hook.read_package_file(pending) == ["curl", "systemd"]


def test_record_apt_pending_skips_write_when_empty(tmp_path: Path) -> None:
    pending = tmp_path / "apt-pending"

    hook.record_apt_pending(pending, "/tmp/not-a-deb\n", lambda path: None)

    assert not pending.exists()


def test_sync_debian_files_dry_run_has_no_side_effects(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    host_file = tmp_path / "usr" / "lib" / "libsystemd.so"
    host_file.parent.mkdir(parents=True)
    host_file.write_text("lib")
    mount = tmp_path / "ext4"
    mount.mkdir()

    hook.sync_debian_files(
        str(mount),
        ["systemd"],
        True,
        version_for=lambda package: "256.5-1",
        files_for=lambda package: [str(host_file)],
        is_owned=lambda file: False,
    )

    assert not (mount / hook.SYNC_MANIFEST).exists()
    output = capsys.readouterr().out
    assert "rsync" in output
    assert "ldconfig" not in output


def test_pacman_hook_targets_include_btrfs_progs(monkeypatch: pytest.MonkeyPatch) -> None:
    installed = {"systemd", "systemd-libs", "systemd-sysvcompat", "btrfs-progs"}
    monkeypatch.setattr(
        hooks, "pacman_query_version", lambda package: "1-1" if package in installed else None
    )
    monkeypatch.setattr(hooks, "pacman_query_depends", lambda package: [])

    targets = hooks.collect_hook_targets(Distribution.ARCH)

    assert "btrfs-progs" in targets
    assert "Target = btrfs-progs" in hooks.generate_pacman_hook(targets)


def test_hook_targets_skip_uninstalled_btrfs_progs(monkeypatch: pytest.MonkeyPatch) -> None:
    installed = {"systemd"}
    monkeypatch.setattr(
        hooks, "pacman_query_version", lambda package: "1-1" if package in installed else None
    )
    monkeypatch.setattr(hooks, "pacman_query_depends", lambda package: [])

    targets = hooks.collect_hook_targets(Distribution.ARCH)

    assert "systemd" in targets
    assert "btrfs-progs" not in targets


def test_debian_hook_targets_include_btrfs_progs(monkeypatch: pytest.MonkeyPatch) -> None:
    installed = {"systemd", "btrfs-progs"}
    monkeypatch.setattr(
        hooks, "debian_query_version", lambda package: "1" if package in installed else None
    )
    monkeypatch.setattr(hooks, "debian_query_depends", lambda package: [])

    targets = hooks.collect_hook_targets(Distribution.DEBIAN)

    assert "btrfs-progs" in targets


def test_unrelated_package_never_reaches_the_ext4_sync(monkeypatch: pytest.MonkeyPatch) -> None:
    config = Config.for_distribution(Distribution.DEBIAN)
    synced: list[list[str]] = []
    monkeypatch.setattr(
        hook, "collect_hook_targets", lambda distribution, extra_packages: ["systemd", "glibc"]
    )
    monkeypatch.setattr(hook, "ensure_mounted", lambda mount_point, dry_run: None)
    monkeypatch.setattr(
        hook,
        "sync_debian_files",
        lambda mount_point, packages, dry_run: synced.append(list(packages)),
    )

    hook.run_with_triggered(config, Distribution.DEBIAN, False, ["curl"])

    assert synced == []


def test_triggered_closure_member_syncs_the_closure_only(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    config = Config.for_distribution(Distribution.DEBIAN)
    synced: list[list[str]] = []
    monkeypatch.setattr(
        hook, "collect_hook_targets", lambda distribution, extra_packages: ["systemd", "glibc"]
    )
    monkeypatch.setattr(hook, "ensure_mounted", lambda mount_point, dry_run: None)
    monkeypatch.setattr(
        hook,
        "sync_debian_files",
        lambda mount_point, packages, dry_run: synced.append(list(packages)),
    )

    hook.run_with_triggered(config, Distribution.DEBIAN, False, ["glibc", "curl"])

    assert synced == [["systemd", "glibc"]]


def test_hook_targets_include_configured_extra_packages(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    installed = {"systemd", "btrbk"}
    monkeypatch.setattr(
        hooks, "pacman_query_version", lambda package: "1-1" if package in installed else None
    )
    monkeypatch.setattr(hooks, "pacman_query_depends", lambda package: [])

    targets = hooks.collect_hook_targets(Distribution.ARCH, ["btrbk", "btrbk"])

    assert targets.count("btrbk") == 1
    assert "Target = btrbk" in hooks.generate_pacman_hook(targets)


def test_extra_packages_that_are_not_installed_are_skipped(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    installed = {"systemd"}
    monkeypatch.setattr(
        hooks, "pacman_query_version", lambda package: "1-1" if package in installed else None
    )
    monkeypatch.setattr(hooks, "pacman_query_depends", lambda package: [])

    targets = hooks.collect_hook_targets(Distribution.ARCH, ["btrbk"])

    assert targets == ["systemd"]


def test_configured_extra_package_triggers_the_closure_sync(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    config = Config.for_distribution(Distribution.ARCH)
    config.ext4_sync.extra_packages = ["btrbk"]
    synced: list[list[str]] = []
    monkeypatch.setattr(
        hook,
        "collect_hook_targets",
        lambda distribution, extra_packages: ["systemd", *extra_packages],
    )
    monkeypatch.setattr(hook, "ensure_mounted", lambda mount_point, dry_run: None)
    monkeypatch.setattr(
        hook,
        "sync_arch_packages",
        lambda mount_point, packages, dry_run: synced.append(list(packages)),
    )

    hook.run_with_triggered(config, Distribution.ARCH, False, ["btrbk"])

    assert synced == [["systemd", "btrbk"]]


def sample_archive() -> hook.ArchArchive:
    return hook.ArchArchive(
        package("systemd", "1-1", "x86_64"),
        Path("/var/cache/pacman/pkg/systemd-1-1-x86_64.pkg.tar.zst"),
        Path("/mnt/ext4-root/var/cache/pacman/pkg/systemd-1-1-x86_64.pkg.tar.zst"),
    )


def test_install_arch_packages_rebuilds_live_linker_cache(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[tuple[str, ...]] = []
    monkeypatch.setattr(
        hook,
        "pacman_install_archives",
        lambda root, paths: calls.append(("pacman", root)),
    )
    monkeypatch.setattr(
        hook, "shell_run", lambda command, args: calls.append((command, *args)) or ""
    )

    hook.install_arch_packages("/mnt/ext4-root", [sample_archive()], dry_run=False)

    # pacman's sysroot ldconfig rewrites the shared /etc/ld.so.cache from the
    # ext4-only view, so the cache is rebuilt from the live view afterwards.
    assert calls == [("pacman", "/mnt/ext4-root"), ("ldconfig", "-X")]


def test_install_arch_packages_rebuilds_cache_when_pacman_fails(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[tuple[str, ...]] = []

    def fail(root: str, paths: list[str]) -> None:
        raise CommandError("boom")

    monkeypatch.setattr(hook, "pacman_install_archives", fail)
    monkeypatch.setattr(
        hook, "shell_run", lambda command, args: calls.append((command, *args)) or ""
    )

    with pytest.raises(CommandError):
        hook.install_arch_packages("/mnt/ext4-root", [sample_archive()], dry_run=False)

    assert calls == [("ldconfig", "-X")]


def test_install_arch_packages_dry_run_skips_ldconfig(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def unexpected(command: str, args: Sequence[str]) -> str:
        raise AssertionError(f"dry run must not run {command}")

    monkeypatch.setattr(hook, "shell_run", unexpected)
    hook.install_arch_packages("/mnt/ext4-root", [sample_archive()], dry_run=True)


def test_sync_debian_files_keeps_symlinked_path_components(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """A merged-usr root has /lib as a symlink; rsync must not recreate it as a directory."""
    host_file = tmp_path / "usr" / "lib" / "libsystemd.so"
    host_file.parent.mkdir(parents=True)
    host_file.write_text("lib")
    mount_point = tmp_path / "ext4"
    mount_point.mkdir()
    calls: list[list[str]] = []

    class FakeRunner(CommandRunner):
        def run(self, command: str, args: Sequence[str] = (), **kwargs: object) -> str:  # type: ignore[override]
            calls.append([command, *args])
            return ""

    monkeypatch.setattr(hook, "CommandRunner", lambda **kwargs: FakeRunner())
    hook.sync_debian_files(
        str(mount_point),
        ["systemd"],
        False,
        version_for=lambda package: "256.5-1",
        files_for=lambda package: [str(host_file)],
        is_owned=lambda file: False,
    )
    assert calls == [
        [
            "rsync",
            "-aAX",
            "--relative",
            "--no-implied-dirs",
            str(host_file),
            str(mount_point),
        ]
    ]


def test_sync_arch_packages_repairs_the_cache_when_nothing_is_left_to_install(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A retried hook must still repair the cache its previous attempt failed to rebuild."""
    calls: list[tuple[str, ...]] = []
    monkeypatch.setattr(hook, "get_arch_package_metadata", lambda packages: [])
    monkeypatch.setattr(hook, "select_arch_packages_to_install", lambda packages, query: [])
    monkeypatch.setattr(
        hook, "shell_run", lambda command, args: calls.append((command, *args)) or ""
    )
    hook.sync_arch_packages("/mnt/ext4-root", ["systemd"], dry_run=False)
    assert calls == [("ldconfig", "-X")]


def test_sync_arch_packages_dry_run_does_not_touch_the_cache(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def unexpected(command: str, args: Sequence[str]) -> str:
        raise AssertionError(f"dry run must not run {command}")

    monkeypatch.setattr(hook, "get_arch_package_metadata", lambda packages: [])
    monkeypatch.setattr(hook, "select_arch_packages_to_install", lambda packages, query: [])
    monkeypatch.setattr(hook, "shell_run", unexpected)
    hook.sync_arch_packages("/mnt/ext4-root", ["systemd"], dry_run=True)


def test_rebuild_live_linker_cache_fails_the_hook(monkeypatch: pytest.MonkeyPatch) -> None:
    def boom(command: str, args: Sequence[str]) -> str:
        raise CommandError("ldconfig: read-only file system")

    monkeypatch.setattr(hook, "shell_run", boom)
    with pytest.raises(CommandError, match="could not rebuild the live linker cache"):
        hook.rebuild_live_linker_cache()

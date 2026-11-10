from collections.abc import Sequence
from pathlib import Path

import pytest

from wslarc.commands import mount, status
from wslarc.commands.mount import systemctl_commands
from wslarc.config import Config, Distribution, TransferSubvol
from wslarc.generators import btrbk, hooks, invocation, systemd
from wslarc.utils.block import MountInfo
from wslarc.utils.shell import CommandError, CommandRunner


class SilentRunner(CommandRunner):
    """Records what would run; never touches systemd."""

    def __init__(self) -> None:
        super().__init__()
        self.calls: list[list[str]] = []

    def run(self, command: str, args: Sequence[str] = (), **kwargs: object) -> str:  # type: ignore[override]
        self.calls.append([command, *args])
        return ""


def test_systemd_and_btrbk_generators() -> None:
    config = Config.for_distribution(Distribution.ARCH)
    config.user.name = "alice"
    config.uuid = "1234"
    assert "Where=/mnt/btrfs" in systemd.generate_base_mount(config)
    assert "subvol=@usr" in systemd.generate_subvol_mount(config, "@usr", "/usr")
    assert "snapshot_dir .snapshots" in btrbk.generate_config(config)
    assert "ExecStart=/usr/local/bin/wslarc" in btrbk.generate_service(
        config, "/etc/wslarc/config.toml"
    )


def test_mount_enables_ext4_sync_unit_only_when_needed() -> None:
    config = Config.for_distribution(Distribution.ARCH)
    unit = hooks.ext4_mount_unit_filename(config)
    assert ["systemctl", "enable", "--now", unit] in systemctl_commands(config, True)
    assert not any(unit in command for command in systemctl_commands(config, False))
    # The boot units are enabled only after the binary install succeeded.
    assert not any(systemd.USR_PRE_UNIT in command for command in systemctl_commands(config, True))


def test_generated_boot_units() -> None:
    config = Config.for_distribution(Distribution.DEBIAN)
    config.user.name = "alice"
    config.expand_variables()
    pre = systemd.generate_usr_pre(config)
    post = systemd.generate_usr_post(config)
    assert "Before=usr.mount" in pre
    assert "ConditionPathIsMountPoint=/mnt/btrfs" in pre
    assert "WantedBy=usr.mount" in pre
    assert "ExecStart=/usr/local/bin/wslarc boot-usr --pre\n" in pre
    assert "Requires=usr.mount" in post
    assert "PartOf=usr.mount" in post
    assert "home-alice.mount" in post
    assert "ldconfig.service systemd-tmpfiles-setup.service" in post
    assert "Conflicts=shutdown.target" in post
    assert "ExecStart=/usr/local/bin/wslarc boot-usr --post\n" in post
    assert "--config" not in pre
    assert "--config" not in post
    dropin = systemd.generate_user_dropin(config, True)
    assert dropin == "[Unit]\nAfter=wslarc-usr-post.service home-alice.mount\n"
    assert "Before=user@" not in systemd.generate_subvol_mount(config, "@home", "/home/alice")


def test_mount_enables_boot_units_after_install(monkeypatch: pytest.MonkeyPatch) -> None:
    calls: list[list[str]] = []

    class FakeRunner(CommandRunner):
        def run(self, command: str, args: Sequence[str] = (), **kwargs: object) -> str:  # type: ignore[override]
            calls.append([command, *args])
            return ""

    config = Config.for_distribution(Distribution.DEBIAN)
    config.user.name = "alice"
    config.expand_variables()
    config.uuid = "1234"
    monkeypatch.setattr(mount, "ensure_dependencies_for", lambda *args: None)
    monkeypatch.setattr(mount, "_update_wsl_conf", lambda *args: None)
    monkeypatch.setattr(mount, "_generate_units", lambda *args: None)
    monkeypatch.setattr(mount, "_generate_boot_units", lambda *args: None)
    monkeypatch.setattr(mount, "_setup_ext4_sync", lambda *args: None)
    monkeypatch.setattr(mount, "_write", lambda *args: None)
    monkeypatch.setattr(
        mount,
        "_install_targets",
        lambda config, runner, **kwargs: [Path("/usr/local/bin/wslarc")],
    )
    monkeypatch.setattr(mount, "_install_binary", lambda *args: calls.append(["INSTALL_BINARY"]))
    mount.run(config, Distribution.DEBIAN, "/etc/wslarc/config.toml", yes=True, runner=FakeRunner())
    enable_boot = ["systemctl", "enable", systemd.USR_PRE_UNIT, systemd.USR_POST_UNIT]
    enable_usr = ["systemctl", "enable", "usr.mount"]
    assert enable_boot in calls
    assert enable_usr in calls
    assert calls.index(["systemctl", "daemon-reload"]) < calls.index(["INSTALL_BINARY"])
    assert calls.index(["INSTALL_BINARY"]) < calls.index(enable_boot)
    # usr.mount comes last: enabling it before the protection units would hide /usr mounts.
    assert calls.index(enable_boot) < calls.index(enable_usr)
    # A previous install's enable is dropped first, so a failed re-install cannot
    # leave /usr overmounted either.
    disable_usr = ["systemctl", "disable", "usr.mount"]
    assert calls.index(disable_usr) < calls.index(["INSTALL_BINARY"])
    assert not any(
        call == ["systemctl", "enable", "usr.mount"] for call in calls[: calls.index(enable_boot)]
    )


def test_mount_removes_leftover_boot_units_without_usr(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    calls: list[list[str]] = []

    class FakeRunner(CommandRunner):
        def run(self, command: str, args: Sequence[str] = (), **kwargs: object) -> str:  # type: ignore[override]
            calls.append([command, *args])
            return ""

    config = Config.for_distribution(Distribution.DEBIAN)
    config.user.name = "alice"
    config.expand_variables()
    config.uuid = "1234"
    del config.subvolumes.backup["@usr"]
    for unit in (systemd.USR_PRE_UNIT, systemd.USR_POST_UNIT):
        (tmp_path / unit).write_text("leftover")
    monkeypatch.setattr(mount, "SYSTEMD_DIR", tmp_path)
    monkeypatch.setattr(mount, "ensure_dependencies_for", lambda *args: None)
    monkeypatch.setattr(mount, "_update_wsl_conf", lambda *args: None)
    monkeypatch.setattr(mount, "_generate_units", lambda *args: None)
    monkeypatch.setattr(mount, "_write", lambda *args: None)
    monkeypatch.setattr(
        mount,
        "_install_targets",
        lambda config, runner, **kwargs: [Path("/usr/local/bin/wslarc")],
    )
    monkeypatch.setattr(mount, "_install_binary", lambda *args: None)
    mount.run(config, Distribution.DEBIAN, "/etc/wslarc/config.toml", yes=True, runner=FakeRunner())
    disable = ["systemctl", "disable", systemd.USR_PRE_UNIT, systemd.USR_POST_UNIT]
    assert disable in calls
    assert not any(systemd.USR_PRE_UNIT in call for call in calls if call[1:2] == ["enable"])
    assert not (tmp_path / systemd.USR_PRE_UNIT).exists()
    assert not (tmp_path / systemd.USR_POST_UNIT).exists()


def test_status_lists_boot_units() -> None:
    config = Config.for_distribution(Distribution.DEBIAN)
    config.user.name = "alice"
    config.expand_variables()
    names = status.managed_unit_names(config)
    assert systemd.USR_PRE_UNIT in names
    assert systemd.USR_POST_UNIT in names
    assert "usr.mount" in names


def test_hook_and_argument_quoting() -> None:
    assert invocation.shell_argument("/etc/a'b") == "'/etc/a'\\''b'"
    assert invocation.systemd_argument("/etc/a $x%f") == '"/etc/a $$x%%f"'
    hook = hooks.generate_pacman_hook(["systemd", "glibc"])
    assert "NeedsTargets" in hook
    assert "Target = glibc" in hook
    assert "--apt-pre" in hooks.generate_apt_hook()


def test_mount_removes_the_leftover_usr_unit_without_usr(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    calls: list[list[str]] = []

    class FakeRunner(CommandRunner):
        def run(self, command: str, args: Sequence[str] = (), **kwargs: object) -> str:  # type: ignore[override]
            calls.append([command, *args])
            return ""

    config = Config.for_distribution(Distribution.DEBIAN)
    config.user.name = "alice"
    config.expand_variables()
    config.uuid = "1234"
    del config.subvolumes.backup["@usr"]
    usr_unit = tmp_path / systemd.mount_unit_filename("/usr")
    usr_unit.write_text("[Mount]\nWhat=UUID=1234\nWhere=/usr\n")
    monkeypatch.setattr(mount, "SYSTEMD_DIR", tmp_path)
    monkeypatch.setattr(mount, "is_mountpoint", lambda path: False)
    monkeypatch.setattr(mount, "ensure_dependencies_for", lambda *args: None)
    monkeypatch.setattr(mount, "_update_wsl_conf", lambda *args: None)
    monkeypatch.setattr(mount, "_generate_units", lambda *args: None)
    monkeypatch.setattr(mount, "_write", lambda *args: None)
    monkeypatch.setattr(mount, "_install_binary", lambda *args: None)
    mount.run(config, Distribution.DEBIAN, "/etc/wslarc/config.toml", yes=True, runner=FakeRunner())
    assert ["systemctl", "disable", usr_unit.name] in calls
    assert not usr_unit.exists()


def test_mount_keeps_a_usr_unit_for_another_disk(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture
) -> None:
    config = Config.for_distribution(Distribution.DEBIAN)
    config.user.name = "alice"
    config.expand_variables()
    config.uuid = "1234"
    del config.subvolumes.backup["@usr"]
    usr_unit = tmp_path / systemd.mount_unit_filename("/usr")
    usr_unit.write_text("[Mount]\nWhat=UUID=9999\nWhere=/usr\n")
    monkeypatch.setattr(mount, "SYSTEMD_DIR", tmp_path)
    mount._remove_boot_units(config, SilentRunner())
    assert usr_unit.exists()


def test_install_binary_refuses_a_base_that_is_not_the_managed_filesystem(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    config = Config.for_distribution(Distribution.DEBIAN)
    config.user.name = "alice"
    config.expand_variables()
    config.uuid = "1234"
    installed: list[object] = []
    monkeypatch.setattr(mount, "find_mount", lambda path: None)
    monkeypatch.setattr(mount, "install_copies", lambda source, targets: installed.append(targets))
    with pytest.raises(ValueError, match="refusing unexpected filesystem"):
        mount._install_binary(config, SilentRunner())
    assert installed == []


def test_install_binary_refuses_a_missing_subvolume_directory(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    config = Config.for_distribution(Distribution.DEBIAN)
    config.user.name = "alice"
    config.expand_variables()
    config.uuid = "1234"
    base = tmp_path / "btrfs"
    base.mkdir()
    config.mount.base = str(base)
    installed: list[object] = []
    monkeypatch.setattr(
        mount,
        "find_mount",
        lambda path: MountInfo(
            target=str(base), source="/dev/sdb", fstype="btrfs", options="rw", uuid="1234"
        ),
    )
    monkeypatch.setattr(mount, "is_mountpoint", lambda path: True)
    monkeypatch.setattr(mount, "install_copies", lambda source, targets: installed.append(targets))
    with pytest.raises(ValueError, match="is not a directory"):
        mount._install_binary(config, SilentRunner())
    assert installed == []


def test_mount_keeps_a_usr_unit_that_only_mentions_the_uuid(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    config = Config.for_distribution(Distribution.DEBIAN)
    config.user.name = "alice"
    config.expand_variables()
    config.uuid = "1234"
    del config.subvolumes.backup["@usr"]
    usr_unit = tmp_path / systemd.mount_unit_filename("/usr")
    usr_unit.write_text("[Unit]\n# What=UUID=1234 Where=/usr\n\n[Install]\nWantedBy=UUID=1234\n")
    monkeypatch.setattr(mount, "SYSTEMD_DIR", tmp_path)
    mount._remove_boot_units(config, SilentRunner())
    assert usr_unit.exists()


def test_mount_keeps_a_usr_unit_that_mounts_the_uuid_elsewhere(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    config = Config.for_distribution(Distribution.DEBIAN)
    config.user.name = "alice"
    config.expand_variables()
    config.uuid = "1234"
    del config.subvolumes.backup["@usr"]
    usr_unit = tmp_path / systemd.mount_unit_filename("/usr")
    usr_unit.write_text("[Mount]\nWhat=UUID=1234\nWhere=/mnt/other\n")
    monkeypatch.setattr(mount, "SYSTEMD_DIR", tmp_path)
    mount._remove_boot_units(config, SilentRunner())
    assert usr_unit.exists()


def test_mount_warns_that_the_backup_usr_stays_until_reboot(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture
) -> None:
    config = Config.for_distribution(Distribution.DEBIAN)
    config.user.name = "alice"
    config.expand_variables()
    config.uuid = "1234"
    del config.subvolumes.backup["@usr"]
    (tmp_path / systemd.mount_unit_filename("/usr")).write_text(
        "[Mount]\nWhat=UUID=1234\nWhere=/usr\n"
    )
    monkeypatch.setattr(mount, "SYSTEMD_DIR", tmp_path)
    monkeypatch.setattr(mount, "is_mountpoint", lambda path: True)
    mount._remove_boot_units(config, SilentRunner())
    assert "keeps the backup /usr until the next boot" in capsys.readouterr().out


def test_install_binary_writes_every_copy_for_a_managed_base(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    config = Config.for_distribution(Distribution.DEBIAN)
    config.user.name = "alice"
    config.expand_variables()
    config.uuid = "1234"
    base = tmp_path / "btrfs"
    (base / "@usr").mkdir(parents=True)
    config.mount.base = str(base)
    config.ext4_sync.mount_point = str(tmp_path / "ext4")
    installed: list[object] = []
    monkeypatch.setattr(
        mount,
        "find_mount",
        lambda path: MountInfo(
            target=path,
            source="/dev/sdb" if path == str(base) else "/dev/sda1",
            fstype="btrfs" if path == str(base) else "ext4",
            options="rw",
            uuid="1234" if path == str(base) else "9999",
        ),
    )
    monkeypatch.setattr(mount, "is_mountpoint", lambda path: True)
    monkeypatch.setattr(mount, "install_copies", lambda source, targets: installed.append(targets))
    mount._install_binary(config, SilentRunner())
    assert installed == [
        [
            Path("/usr/local/bin/wslarc"),
            base / "@usr" / "local/bin/wslarc",
            tmp_path / "ext4" / "usr/local/bin/wslarc",
        ]
    ]


def test_mount_fails_before_writing_units_when_the_base_is_unmounted(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture
) -> None:
    written: list[object] = []

    class FakeRunner(CommandRunner):
        def run(self, command: str, args: Sequence[str] = (), **kwargs: object) -> str:  # type: ignore[override]
            return ""

    config = Config.for_distribution(Distribution.DEBIAN)
    config.user.name = "alice"
    config.expand_variables()
    config.uuid = "1234"
    monkeypatch.setattr(mount, "ensure_dependencies_for", lambda *args: None)
    monkeypatch.setattr(mount, "_update_wsl_conf", lambda *args: None)
    monkeypatch.setattr(mount, "_write", lambda *args: written.append(args))
    monkeypatch.setattr(mount, "find_mount", lambda path: None)
    with pytest.raises(ValueError, match="refusing unexpected filesystem"):
        mount.run(
            config, Distribution.DEBIAN, "/etc/wslarc/config.toml", yes=True, runner=FakeRunner()
        )
    assert written == []
    assert "Mount setup complete" not in capsys.readouterr().out


def test_install_targets_skips_the_ext4_check_before_its_unit_is_started(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    config = Config.for_distribution(Distribution.DEBIAN)
    config.user.name = "alice"
    config.expand_variables()
    config.uuid = "1234"
    base = tmp_path / "btrfs"
    (base / "@usr").mkdir(parents=True)
    config.mount.base = str(base)
    monkeypatch.setattr(
        mount,
        "find_mount",
        lambda path: MountInfo(
            target=str(base), source="/dev/sdb", fstype="btrfs", options="rw", uuid="1234"
        ),
    )
    monkeypatch.setattr(mount, "is_mountpoint", lambda path: False)
    runner = SilentRunner()
    early = mount._install_targets(config, runner, require_ext4=False)
    assert early == [Path("/usr/local/bin/wslarc"), base / "@usr" / "local/bin/wslarc"]
    with pytest.raises(ValueError, match="is not mounted; cannot update ext4"):
        mount._install_targets(config, runner)


def test_install_targets_refuses_a_plain_directory_named_like_the_subvolume(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    config = Config.for_distribution(Distribution.DEBIAN)
    config.user.name = "alice"
    config.expand_variables()
    config.uuid = "1234"
    base = tmp_path / "btrfs"
    (base / "@usr").mkdir(parents=True)
    config.mount.base = str(base)
    monkeypatch.setattr(
        mount,
        "find_mount",
        lambda path: MountInfo(
            target=str(base), source="/dev/sdb", fstype="btrfs", options="rw", uuid="1234"
        ),
    )

    class NotASubvolume(SilentRunner):
        def run(self, command: str, args: Sequence[str] = (), **kwargs: object) -> str:  # type: ignore[override]
            if command == "btrfs":
                raise CommandError("not a btrfs subvolume")
            return super().run(command, args, **kwargs)

    with pytest.raises(CommandError, match="not a btrfs subvolume"):
        mount._install_targets(config, NotASubvolume(), require_ext4=False)


def test_usr_mount_requires_the_stash_unit(config: Config | None = None) -> None:
    cfg = config or Config.for_distribution(Distribution.DEBIAN)
    cfg.user.name = "alice"
    cfg.expand_variables()
    usr_unit = systemd.generate_subvol_mount(cfg, "@usr", "/usr", "compress=zstd")
    assert f"Requires=mnt-btrfs.mount {systemd.USR_PRE_UNIT}" in usr_unit
    assert f"After=mnt-btrfs.mount {systemd.USR_PRE_UNIT}" in usr_unit
    pre = systemd.generate_usr_pre(cfg)
    assert f"PartOf={systemd.mount_unit_filename('/usr')}" in pre


def test_user_dropin_uses_the_configured_home_mount() -> None:
    config = Config.for_distribution(Distribution.DEBIAN)
    config.user.name = "alice"
    config.expand_variables()
    config.subvolumes.backup["@home"].mount = "/home"
    dropin = systemd.generate_user_dropin(config, True)
    assert dropin == "[Unit]\nAfter=wslarc-usr-post.service home.mount\n"


def test_install_targets_refuses_another_ext4_filesystem(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    config = Config.for_distribution(Distribution.DEBIAN)
    config.user.name = "alice"
    config.expand_variables()
    config.uuid = "1234"
    base = tmp_path / "btrfs"
    (base / "@usr").mkdir(parents=True)
    config.mount.base = str(base)
    config.ext4_sync.mount_point = str(tmp_path / "ext4")

    def fake_find_mount(path: str) -> MountInfo:
        if path == str(base):
            return MountInfo(
                target=path, source="/dev/sdb", fstype="btrfs", options="rw", uuid="1234"
            )
        if path == "/":
            return MountInfo(
                target="/", source="/dev/sda1", fstype="ext4", options="rw", uuid="root"
            )
        return MountInfo(target=path, source="/dev/sdc1", fstype="ext4", options="rw", uuid="other")

    monkeypatch.setattr(mount, "find_mount", fake_find_mount)
    monkeypatch.setattr(mount, "is_mountpoint", lambda path: True)
    with pytest.raises(ValueError, match="is not the ext4 root of this system"):
        mount._install_targets(config, SilentRunner())


def test_usr_subvolume_is_recognised_in_the_transfer_table(tmp_path: Path) -> None:
    config = Config.for_distribution(Distribution.DEBIAN)
    config.user.name = "alice"
    config.expand_variables()
    del config.subvolumes.backup["@usr"]
    config.subvolumes.transfer["@usr"] = TransferSubvol(mount="/usr", options="compress=zstd")
    assert mount.usr_subvol_name(config) == "@usr"

from wslarc.config import Config, Distribution
from wslarc.generators import btrbk, hooks, invocation, systemd


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


def test_hook_and_argument_quoting() -> None:
    assert invocation.shell_argument("/etc/a'b") == "'/etc/a'\\''b'"
    assert invocation.systemd_argument("/etc/a $x%f") == '"/etc/a $$x%%f"'
    hook = hooks.generate_pacman_hook(["systemd", "glibc"])
    assert "NeedsTargets" in hook
    assert "Target = glibc" in hook
    assert "--apt-pre" in hooks.generate_apt_hook()

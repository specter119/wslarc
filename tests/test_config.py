from pathlib import Path

import pytest

from wslarc.config import Config, ConfigError, Distribution


def test_distribution_template_contains_nix_and_no_seed() -> None:
    config = Config.for_distribution(Distribution.ARCH)
    assert config.subvolumes.backup["@nix"].mount == "/nix"
    assert "seed" not in str(config.to_dict())


def test_seed_field_is_rejected() -> None:
    with pytest.raises(ConfigError, match="unknown backup"):
        Config.from_dict(
            {
                "vhdx": {"path": "C:\\test.vhdx", "label": "TestBtrfs"},
                "user": {"name": "alice"},
                "mount": {"base": "/mnt/btrfs"},
                "subvolumes": {"backup": {"@nix": {"mount": "/nix", "seed": False}}},
                "btrbk": {},
            }
        )


def test_user_template_round_trip(tmp_path: Path) -> None:
    config = Config.for_distribution(Distribution.ARCH)
    config.user.name = "alice"
    path = tmp_path / "config.toml"
    config.save(path)

    loaded = Config.load_unexpanded(path)
    assert loaded.subvolumes.backup["@home"].mount == "/home/$USER"
    assert loaded.resolve_variables().subvolumes.backup["@home"].mount == "/home/alice"
    assert '"@nix" = "/nix"' in path.read_text()


def _minimal_raw(ext4_sync: dict[str, object]) -> dict[str, object]:
    return {
        "vhdx": {"path": "C:\\test.vhdx", "label": "TestBtrfs"},
        "user": {"name": "alice"},
        "mount": {"base": "/mnt/btrfs"},
        "btrbk": {},
        "ext4_sync": ext4_sync,
    }


def test_ext4_sync_extra_packages_default_to_empty() -> None:
    config = Config.from_dict(_minimal_raw({"mount_point": "/mnt/ext4-root"}))

    assert config.ext4_sync.extra_packages == []
    assert config.to_dict()["ext4_sync"] == {
        "mount_point": "/mnt/ext4-root",
        "extra_packages": [],
    }


def test_ext4_sync_extra_packages_are_trimmed_and_deduplicated() -> None:
    config = Config.from_dict(_minimal_raw({"extra_packages": [" btrbk", "btrbk", "rsync"]}))

    assert config.ext4_sync.extra_packages == ["btrbk", "rsync"]


def test_ext4_sync_rejects_unknown_fields() -> None:
    with pytest.raises(ConfigError, match="unknown ext4_sync field"):
        Config.from_dict(_minimal_raw({"packages": ["btrbk"]}))


def test_ext4_sync_rejects_invalid_extra_packages() -> None:
    with pytest.raises(ConfigError, match="must be a list of package names"):
        Config.from_dict(_minimal_raw({"extra_packages": "btrbk"}))
    with pytest.raises(ConfigError, match="must be a list of package names"):
        Config.from_dict(_minimal_raw({"extra_packages": ["btrbk", 1]}))
    with pytest.raises(ConfigError, match="empty package names"):
        Config.from_dict(_minimal_raw({"extra_packages": [" "]}))


def test_ext4_sync_extra_packages_round_trip(tmp_path: Path) -> None:
    config = Config.for_distribution(Distribution.ARCH)
    config.user.name = "alice"
    config.ext4_sync.extra_packages = ["btrbk"]
    path = tmp_path / "config.toml"
    config.save(path)

    loaded = Config.load_unexpanded(path)
    assert loaded.ext4_sync.extra_packages == ["btrbk"]

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

from wslarc.utils.block import (
    parse_debian_depends,
    parse_debian_status_version,
    parse_findmnt_mounts,
    parse_pacman_depends,
    parse_pacman_package_info,
)


def test_findmnt_parser_flattens_children() -> None:
    mounts = parse_findmnt_mounts(
        '{"filesystems":[{"target":"/mnt/btrfs","source":"/dev/sda",'
        '"fstype":"btrfs","options":"rw","children":[{"target":"/usr",'
        '"source":"/dev/sda","fstype":"btrfs","options":"subvol=@usr"}]}]}'
    )
    assert [mount.target for mount in mounts] == ["/mnt/btrfs", "/usr"]


def test_package_parsers_match_rust_behavior() -> None:
    assert (
        parse_pacman_package_info("Name : systemd\nVersion : 256.5-1\nArchitecture : any\n").version
        == "256.5-1"
    )
    assert parse_pacman_depends("Depends On : glibc libcap>=2.0 sh\n") == [
        "glibc",
        "libcap",
        "sh",
    ]
    assert parse_debian_depends("libc6 (>= 2.34), default-logind | logind") == [
        ["libc6"],
        ["default-logind", "logind"],
    ]


def test_debian_transient_package_states_are_present() -> None:
    for state in (
        "installed",
        "half-installed",
        "unpacked",
        "half-configured",
        "triggers-awaited",
        "triggers-pending",
    ):
        assert parse_debian_status_version(f"install ok {state}\t1.2.3\n") == "1.2.3"
    assert parse_debian_status_version("deinstall ok config-files\t1.2.3\n") is None

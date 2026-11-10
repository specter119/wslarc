from wslarc.utils.block import (
    MountInfo,
    mounts_under,
    parse_debian_depends,
    parse_debian_status_version,
    parse_findmnt_mounts,
    parse_pacman_depends,
    parse_pacman_package_info,
)
from wslarc.utils.systemd import parse_dependency_names, parse_unit_properties


def test_findmnt_parser_flattens_children() -> None:
    mounts = parse_findmnt_mounts(
        '{"filesystems":[{"target":"/mnt/btrfs","source":"/dev/sda",'
        '"fstype":"btrfs","options":"rw","children":[{"target":"/usr",'
        '"source":"/dev/sda","fstype":"btrfs","options":"subvol=@usr"}]}]}'
    )
    assert [mount.target for mount in mounts] == ["/mnt/btrfs", "/usr"]


def test_findmnt_parser_reads_flat_list_with_spaces() -> None:
    # findmnt -J already resolves the octal escapes from mountinfo; JSON values
    # contain literal characters such as spaces.
    mounts = parse_findmnt_mounts(
        '{"filesystems":['
        '{"target":"/usr/bin/perf","source":"/dev/sdb[/perf/bin/perf]",'
        '"fstype":"ext4","options":"ro"},'
        '{"target":"/mnt/with space","source":"tmpfs","fstype":"tmpfs","options":"rw"}'
        "]}"
    )
    assert [mount.target for mount in mounts] == ["/usr/bin/perf", "/mnt/with space"]


def test_mounts_under_picks_topmost_of_a_stack_parents_first() -> None:
    mounts = [
        MountInfo("/usr/lib/wsl/lib", "old", "overlay", "rw"),
        MountInfo("/usr/bin/perf", "/dev/sdb", "ext4", "ro"),
        MountInfo("/usr", "/dev/sdd", "btrfs", "rw"),
        MountInfo("/usr/lib/wsl/lib", "new", "overlay", "rw"),
        MountInfo("/elsewhere/x", "tmpfs", "tmpfs", "rw"),
    ]
    found = mounts_under(mounts, "/usr")
    assert [mount.target for mount in found] == ["/usr/bin/perf", "/usr/lib/wsl/lib"]
    assert next(m for m in found if m.target == "/usr/lib/wsl/lib").source == "new"


def test_systemd_output_parsers() -> None:
    assert parse_dependency_names("b.service\n a.service\nb.service\n\n") == [
        "a.service",
        "b.service",
    ]
    blocks = parse_unit_properties("Id=a\nActiveState=active\n\nId=b\nActiveState=inactive\n")
    assert blocks == [
        {"Id": "a", "ActiveState": "active"},
        {"Id": "b", "ActiveState": "inactive"},
    ]
    assert parse_unit_properties("") == []


def test_package_parsers_match_rust_behavior() -> None:
    package = parse_pacman_package_info("Name : systemd\nVersion : 256.5-1\nArchitecture : any\n")
    assert package is not None
    assert package.version == "256.5-1"
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

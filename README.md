# WSLArc

WSL2 Btrfs backup and restore tool.

This branch is the Python implementation. The previous Rust implementation is
preserved in the separate `rust` branch for reference.

## Runtime requirements

`wslarc` is designed for Arch or Debian/Ubuntu WSL distributions. The Python
source requires Python 3.11 or newer. A packaged binary includes the Python
runtime and does not require Python to be installed on the target machine.

The system operations still require these external tools:

- `wslarc init`: `btrfs-progs`, `rsync`, and conditional `e2fsprogs`
- `wslarc mount`: `btrbk`
- `wslarc snapshot *`: `btrbk`
- `wslarc restore`: `btrfs-progs`, and conditional `rsync`
- `wslarc attach`: WSL Windows interop and `wsl.exe`
- `wslarc hook-sync-systemd`: `pacman` on Arch; `dpkg-query`, `dpkg-deb`,
  `rsync`, and `ldconfig` on Debian/Ubuntu

The command checks dependencies at the point where each operation needs them.
It does not perform a blanket preflight for unrelated commands.

## Installation

For development:

```bash
python3 -m venv .venv
. .venv/bin/activate
python -m pip install -e '.[dev]'
```

For a single-file Linux binary:

```bash
./scripts/build-binary.sh
```

The script runs the build through the `build` dependency group
(`uv run --group build`), so uv installs `nuitka[onefile]` (with zstandard
compression) and `patchelf` on demand. No system-level `patchelf` is
required; `ccache` is optional and only speeds up repeated compilations.
The resulting binary contains the Python runtime. It still invokes the
external Btrfs, rsync, btrbk, systemd, package-manager, and WSL tools listed
above.

## Quick start

Create a VHDX from an elevated Windows PowerShell. This uses `diskpart`, so it
does not require the Hyper-V PowerShell module:

```powershell
$vhdxPath = "$env:USERPROFILE\.local\share\wslarc\btrfs.vhdx"
New-Item -ItemType Directory -Force -Path (Split-Path $vhdxPath)
"create vdisk file=`"$vhdxPath`" maximum=524288 type=expandable" | diskpart
```

`maximum` is in MB (524288 MB = 512 GB) and `type=expandable` creates a
dynamically growing disk. Do not initialize or format the disk in Windows;
`wslarc init` attaches it to WSL and formats it as Btrfs.

Then initialize from WSL:

```bash
sudo wslarc init
```

During first initialization, the suggested VHDX path is
`%USERPROFILE%\.local\share\wslarc\btrfs.vhdx`, resolved through `cmd.exe`
interop. If the profile cannot be resolved, no default is suggested.

The default configuration path is `/etc/wslarc/config.toml`. Use
`--config PATH` to select another path.

## Commands

```text
wslarc init [--dry-run]
wslarc mount [--dry-run]
wslarc umount [--dry-run]
wslarc status
wslarc snapshot run
wslarc snapshot list
wslarc snapshot prune [--dry-run]
wslarc restore [--snapshot NAME]
wslarc attach
wslarc hook-sync-systemd [--dry-run] [--apt-pre|--apt-post]
```

`--yes` and `--verbose` are global options. `--yes` is accepted for saved
configurations, but it never bypasses the safety confirmations required by a
first initialization.

## Initialization behavior

`init` creates configured Btrfs subvolumes, builds an rsync plan, shows the
plan, and then asks before copying data. It rejects ordinary directories where
a configured Btrfs subvolume is required.

On first initialization, `/home` and `/nix` are created but are not copied
over existing WSL directories. `/usr`, `/opt`, distribution package
databases, snapshot-only sources such as `/etc`, and transfer subvolumes are
included in the first-run plan. Non-empty targets receive an additional
warning and confirmation.

When the configuration already exists, snapshot-only sources are synchronized
even when their targets are non-empty. Backup and transfer sources are
synchronized only when their targets are new or empty. Non-empty backup and
transfer targets are skipped without a warning.

Rsync output is streamed to the terminal and uses `--partial`. If a copy is
interrupted, `init` prints a complete recovery command that temporarily
mounts the Btrfs top-level subvolume (`subvolid=5`), resumes the remaining
copies, and unmounts it with a shell trap. No progress file is retained.

The first-init policy is implementation behavior, not runtime configuration.
There is no `seed` field. A configuration such as this is sufficient:

```toml
[subvolumes.backup]
"@usr" = "/usr"
"@opt" = "/opt"
"@home" = "/home/$USER"
"@nix" = "/nix"
```

Unknown backup fields, including the old `seed` field, are rejected.

## systemd ext4 sync hook

`wslarc mount` installs a package hook (`/etc/pacman.d/hooks/` on Arch,
`/etc/apt/apt.conf.d/` on Debian/Ubuntu) that runs
`wslarc hook-sync-systemd` after package transactions. The hook keeps the
hidden ext4 root bootable: it only synchronizes the recursive dependency
closure of the systemd package set plus `btrfs-progs`, so the ext4 root can
boot and mount its Btrfs volumes. Unrelated package installations never
trigger a copy. When `btrfs-progs` is not installed on the host, it is
skipped automatically.

Additional root packages can be added to the closure with the
`[ext4_sync]` table. The systemd and `btrfs-progs` roots are always kept, so
configuring extras cannot remove the boot requirement:

```toml
[ext4_sync]
mount_point = "/mnt/ext4-root"
extra_packages = ["btrbk", "rsync"]
```

Each entry names a host package whose files (and those of its installed
dependencies) are mirrored into the ext4 root. Entries that are not
installed are skipped. Changes affect the generated pacman hook targets, so
re-run `sudo wslarc mount` after editing this table.

On Debian/Ubuntu, the APT pre-invoke step records the package names of the
`.deb` files being installed. The post-invoke step then synchronizes the
files owned by the closure package by package with
`rsync -aAX --relative`, prunes synced files that are no longer owned by any
installed package according to the `.wslarc-systemd-sync-manifest` manifest
stored on the ext4 root, and rebuilds the ext4 linker cache with
`ldconfig -r <mount point>`.

On Arch, the hook queries the installed package metadata, skips packages
whose name, version, and architecture already match inside the ext4 root,
copies the matching archives from `/var/cache/pacman/pkg` into the ext4
root, and installs them with `pacman --sysroot <root> -U --noconfirm`. A
guard environment variable prevents the nested pacman hook from recursing
into another sync.

## Development checks

```bash
pytest
ruff check src tests
mypy
```

Tests use fake command runners and temporary directories. They do not attach
VHDX files, format filesystems, mount subvolumes, run real rsync, or change
systemd state.

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

The resulting binary contains the Python runtime. It still invokes the
external Btrfs, rsync, btrbk, systemd, package-manager, and WSL tools listed
above.

## Quick start

Create a VHDX from an elevated Windows PowerShell:

```powershell
$vhdxPath = "$env:USERPROFILE\.local\share\wsl\btrfs.vhdx"
New-Item -ItemType Directory -Force -Path (Split-Path $vhdxPath)
New-VHD -Path $vhdxPath -SizeBytes 150GB -Dynamic
```

Then initialize from WSL:

```bash
sudo wslarc init
```

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

## Development checks

```bash
pytest
ruff check src tests
mypy
```

Tests use fake command runners and temporary directories. They do not attach
VHDX files, format filesystems, mount subvolumes, run real rsync, or change
systemd state.

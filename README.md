# WSLArc

WSL2 Btrfs backup and restore tool.

## Features

- **Btrfs initialization**: Format VHDX and create subvolumes via interactive wizard
- **Subvolume management**: A/B/C class subvolumes with different backup strategies
- **Systemd integration**: Generate mount units and btrbk timer
- **Snapshot management**: Create, list, prune, and restore snapshots via btrbk

## Prerequisites

Before using wslarc, you must create a VHDX file in Windows (requires Administrator):

```powershell
# Run in PowerShell as Administrator
$vhdxPath = "$env:USERPROFILE\.local\share\wsl\btrfs.vhdx"
New-Item -ItemType Directory -Force -Path (Split-Path $vhdxPath)
New-VHD -Path $vhdxPath -SizeBytes 150GB -Dynamic
```

> **Note**: VHDX creation requires Administrator privileges in Windows. However, mounting the VHDX from WSL does not require Admin.

## Quick Start

One-liner to download and start initialization:

```bash
curl -fsSL https://github.com/specter119/wslarc/releases/latest/download/wslarc-linux-x86_64.tar.gz | sudo tar xz -C /usr/local/bin && sudo wslarc init
```

## Installation

Download the latest release from [GitHub Releases](https://github.com/specter119/wslarc/releases):

```bash
# Download and extract
curl -LO https://github.com/specter119/wslarc/releases/latest/download/wslarc-linux-x86_64.tar.gz
tar xzf wslarc-linux-x86_64.tar.gz

# Install
sudo mv wslarc /usr/local/bin/
```

## Runtime Dependencies

Beyond a basic Arch or Debian/Ubuntu WSL environment, `wslarc` checks the
dependencies actually required by each command:

- `wslarc init`
  - Required: `btrfs-progs`, `rsync`
  - Conditional: if any transfer subvolume sets `nodatacow = true`, `e2fsprogs` is required for `chattr`
- `wslarc mount`
  - Required: `btrbk`
  - Conditional: `btrfs-progs`, `rsync`, and `pacman` on Arch, or `dpkg-query` and `dpkg-deb` on Debian/Ubuntu, when `/usr` is a configured Btrfs subvolume
- `wslarc snapshot run`
  - Required: `btrbk`, `btrfs-progs`
  - Conditional: `rsync` when `subvolumes.snapshot_only` is non-empty
- `wslarc snapshot list`
  - Required: `btrbk`
- `wslarc snapshot prune`
  - Required: `btrbk`
- `wslarc restore`
  - Required: `btrfs-progs`
  - Conditional: `rsync` when restoring a snapshot-only subvolume

On Arch, install the common dependencies with:

```bash
sudo pacman -S btrfs-progs rsync btrbk e2fsprogs
```

On Debian/Ubuntu, use the equivalent packages from APT:

```bash
sudo apt-get install btrfs-progs rsync btrbk e2fsprogs
```

## Usage

### Initialize Btrfs VHDX

```bash
# Interactive mode
sudo wslarc init

# With custom config
sudo wslarc init --config /path/to/config.toml

# Silent mode (use defaults)
sudo wslarc init --yes
```

Initialization saves the selected configuration path, preserving `$USER`
templates for later loads. It creates nested exclusion subvolumes before
copying home data, and seeds transfer subvolumes after applying `nodatacow`.
Source directories are not deleted.

Stop applications that write to migration sources first, especially containers.
Initialization records seed progress in `.wslarc-init-progress.toml` at the
Btrfs root so interrupted copies can be retried. It does not overwrite an
untracked populated target, and refuses to mistake an ordinary directory for
a Btrfs subvolume. Existing installations with ordinary exclusion directories
need an explicit data-preserving migration before reinitialization.

### Generate systemd mount units

```bash
# Generate and enable mounts
sudo wslarc mount

# Preview only
sudo wslarc mount --dry-run
```

Generated automation keeps the configuration path supplied to `mount`. That
file must remain on persistent ext4 storage accessible before managed mounts,
such as `/etc/wslarc/custom.toml`, not under `/usr`, a managed home directory,
the Btrfs base, or a temporary directory. Rerun `mount` after changing the
configuration path.

When `/usr` is already mounted separately, mount the configured ext4 root
before updating wslarc. The installer verifies that it is the same ext4
filesystem as `/`, then stages and atomically replaces both binary copies.
It does not delete the running binary before copying it.

`wslarc mount` also installs a distribution package hook that syncs triggered
systemd-related package upgrades and their recursive dependency closure into
the ext4 root sysroot used by WSL.

On Arch, the hook uses pacman package targets and installs the matching cached
packages into the ext4 root. On Debian/Ubuntu, the APT hook records the `.deb`
packages before dpkg runs, then synchronizes the installed systemd dependency
closure after the transaction. The Debian path copies files and symlinks only,
removes files tracked by wslarc that are no longer present, and runs
`ldconfig` in the ext4 root. Hook failures are reported as warnings so an
already-completed package transaction is not turned into an APT failure.

Failed synchronization retains pending work for a later hook invocation.
On Arch, archive selection uses the installed package's exact name, version,
and architecture, including architecture-independent (`any`) packages.
Required archives that are no longer cached produce an actionable error;
wslarc does not silently substitute another version or upgrade the host to
obtain them. Restore the required archives and rerun
`sudo wslarc hook-sync-systemd </dev/null` to retry.

On Debian/Ubuntu, held but installed packages remain eligible for syncing.
Dry-run does not consume pending work, and failed stale-file removal does not
discard the information needed for a retry.

If `/mnt/ext4-root` exists but is empty, verify that the ext4 mount unit is
active:

```bash
sudo systemctl enable --now 'mnt-ext4\x2droot.mount'
findmnt /mnt/ext4-root
```

The `\x2d` is required because systemd escapes the hyphen in
`/mnt/ext4-root`; an older `mnt-ext4-root.mount` file may be invalid and
should not be used.

### Disable wslarc mount units

```bash
# Disable wslarc-managed mounts and timer
sudo wslarc umount
```

### Status, snapshots, and restore

```bash
# Show status
wslarc status

# Create snapshot
sudo wslarc snapshot run

# List snapshots
wslarc snapshot list

# Preview retention cleanup
sudo wslarc snapshot prune --dry-run

# Remove snapshots outside the retention policy
sudo wslarc snapshot prune

# Restore interactively from a snapshot
sudo wslarc restore

# Restore a specific snapshot
sudo wslarc restore --snapshot home.20260629T0323
```

Restoring a regular backup subvolume restores that subvolume and remounts its
configured mount point. Restoring a `snapshot_only` subvolume also syncs the
restored content back to its configured `source`, such as `/etc`.

Restore checks its dependencies and volume identity before changing the live
layout. It stages the replacement before unmounting, preserves existing restore
backups, and carries live nested subvolumes into the replacement. If cutover or
remount fails, it attempts to restore the previous layout and reports any
rollback failure with the retained recovery paths.

Snapshot-only write-back uses rsync and is **not atomic**. A failed write-back
can leave a partially updated live source; recovery material is retained and
the command reports failure rather than claiming the source was rolled back.
Stop applications that write to the restored paths before restoring.

### Snapshot retention and cleanup

The generated `btrbk.timer` runs `wslarc snapshot run` on the configured
schedule (daily at 03:00 by default). The command first syncs every configured
`snapshot_only` source, such as `/etc`, into its Btrfs subvolume, then runs
`btrbk` with the current wslarc-generated configuration. `btrbk run` creates snapshots and removes
snapshots that are no longer covered by the configured retention policy. The
default policy is:

```toml
[btrbk]
preserve_min = "latest"
preserve = "2d 1w 2m"
```

`preserve_min = "latest"` always keeps the newest snapshot. `preserve =
"2d 1w 2m"` keeps daily snapshots for two days, weekly snapshots for one week,
and monthly snapshots for two months. `snapshot run` and `snapshot prune`
regenerate `/etc/btrbk/btrbk.conf` from the selected wslarc configuration
before invoking btrbk. Each invocation uses its own temporary configuration
so it cannot accidentally read another invocation's configuration.
Use `wslarc snapshot prune --dry-run` to review the deletion set without
overwriting the installed configuration. Declining the cleanup confirmation
also leaves that file unchanged. Use `--yes` to confirm non-interactive cleanup.

Snapshot commands require the configured Btrfs volume to be mounted with the
expected UUID. Snapshot-only synchronization also verifies its destination
subvolumes before copying; an ordinary directory is not a substitute.

## Status Behavior

- `Subvolumes`
  - When the system allows reading the live Btrfs subvolume list, `wslarc` shows the actual subvolumes
  - When `/mnt/btrfs` is mounted but `btrfs subvolume list` fails because of permissions or capability limits, `wslarc` shows:
    - `mounted`
    - the failure reason
    - a subvolume overview derived from configuration
- `Failed mounts`
  - Only checks mount units managed by `wslarc`
  - Does not scan every failed mount unit on the system

## Configuration

Configuration file: `/etc/wslarc/config.toml`

When the file does not exist, `wslarc init` detects the current distribution
and writes its complete subvolume template. Once saved, the subvolume entries
are the source of truth; loading the file does not regenerate them and the
distribution is not stored as a selector.
Other commands require an existing configuration file and do not silently
fall back to a fresh template when it is missing.

```toml
[vhdx]
# Full Windows path to pre-created VHDX (required)
path = 'C:\Users\YourName\.local\share\wsl\btrfs.vhdx'
label = "ArchBtrfs"

[user]
# Linux username (required, will be created if not exists)
name = "yourname"
# useradd options (default: "-M -G wheel")
# options = "-M -G wheel"

[mount]
base = "/mnt/btrfs"
# Mount options (default: compress=zstd:3,noatime,nofail)
# options = "compress=zstd:3,noatime,nofail"

# `wslarc init` detects the distribution and writes the complete template.
# The saved subvolume sections are the source of truth and can be edited.

# A-class: Backup targets (simple form)
[subvolumes.backup]
"@usr" = "/usr"
"@opt" = "/opt"
"@home" = "/home/$USER"
# Arch adds @var_lib_pacman; Debian adds @var_lib_dpkg and @var_lib_apt.

# Snapshot-only subvolume
[subvolumes.snapshot_only."@etc"]
snapshot_name = "etc"
source = "/etc"

# A-class: Backup targets with custom options (full form)
# [subvolumes.backup."@data"]
# mount = "/data"
# options = "compress=zstd:1,noatime,nofail"

# B-class: Excluded paths (nested subvolumes)
[subvolumes.exclude]
parent = "@home"
paths = [".cache", ".local", ".npm", ".bun", ".vscode-server-insiders"]

# C-class: Transfer subvolumes (high I/O)
[subvolumes.transfer."@containers"]
mount = "/var/lib/containers"
nodatacow = true
# options = "noatime,nofail"  # custom options override defaults

[subvolumes.transfer."@var_cache"]
mount = "/var/cache"
nodatacow = true

[subvolumes.transfer."@var_log"]
mount = "/var/log"
nodatacow = false

[subvolumes.transfer."@var_tmp"]
mount = "/var/tmp"
nodatacow = true

# btrbk configuration
[btrbk]
snapshot_dir = ".snapshots"
preserve_min = "latest"
preserve = "2d 1w 2m"
timer_schedule = "*-*-* 03:00:00"
```

## Subvolume Classes

| Class | Purpose           | Snapshot       | nodatacow |
| ----- | ----------------- | -------------- | --------- |
| A     | Backup targets    | ✓              | -         |
| B     | Excluded paths    | Nested under A | -         |
| C     | High I/O transfer | ✗              | Optional  |

## License

MIT

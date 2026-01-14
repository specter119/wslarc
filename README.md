# wslarc

WSL2 Btrfs backup and restore tool.

## Features

- **Btrfs initialization**: Format VHDX and create subvolumes via interactive wizard
- **Subvolume management**: A/B/C class subvolumes with different backup strategies
- **Systemd integration**: Generate mount units and btrbk timer
- **Snapshot management**: Create and list snapshots via btrbk

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

### Generate systemd mount units

```bash
# Generate and enable mounts
sudo wslarc mount

# Preview only
sudo wslarc mount --dry-run
```

### Status and snapshots

```bash
# Show status
wslarc status

# Create snapshot
sudo wslarc snapshot run

# List snapshots
wslarc snapshot list
```

## Configuration

Configuration file: `/etc/wslarc/config.toml`

```toml
[vhdx]
# Full Windows path to pre-created VHDX (required)
path = 'C:\Users\YourName\.local\share\wsl\btrfs.vhdx'
label = "ArchBtrfs"

[user]
# Linux username (required, will be created if not exists)
name = "yourname"

[mount]
base = "/mnt/btrfs"
# Mount options (default: compress=zstd:3,noatime,nofail)
# options = "compress=zstd:3,noatime,nofail"

# A-class: Backup targets (simple form)
[subvolumes.backup]
"@etc" = "/etc"
"@usr" = "/usr"
"@opt" = "/opt"
"@home" = "/home/$USER"

# A-class: Backup targets with custom options (full form)
# [subvolumes.backup."@data"]
# mount = "/data"
# options = "compress=zstd:1,noatime,nofail"

# B-class: Excluded paths (nested subvolumes)
[subvolumes.exclude]
parent = "@home"
paths = [".cache", ".local", ".npm", ".bun"]

# C-class: Transfer subvolumes (high I/O)
[subvolumes.transfer."@containers"]
mount = "/var/lib/containers"
nodatacow = true
# options = "noatime,nofail"  # custom options override defaults

[subvolumes.transfer."@var_cache"]
mount = "/var/cache"
nodatacow = true

# btrbk configuration
[btrbk]
snapshot_dir = ".snapshots"
preserve_min = "2d"
preserve = "14d 4w 2m"
timer_schedule = "*-*-* 03:00:00"
```

## Subvolume Classes

| Class | Purpose | Snapshot | nodatacow |
|-------|---------|----------|-----------|
| A | Backup targets | ✓ | - |
| B | Excluded paths | Nested under A | - |
| C | High I/O transfer | ✗ | Optional |

## License

MIT

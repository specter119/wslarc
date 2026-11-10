# WSLArc

WSL2 Btrfs backup and restore tool.

This branch is the Python implementation. The previous Rust implementation is
preserved in the separate `rust` branch for reference.

## Runtime requirements

`wslarc` is designed for Arch or Debian/Ubuntu WSL distributions. The Python
source requires Python 3.11 or newer. A packaged binary includes the Python
runtime and does not require Python to be installed on the target machine.

The system operations still require these external tools:

- `wslarc init`: `btrfs-progs`, `rsync`, `findmnt`, and conditional `e2fsprogs`
- `wslarc mount`: `btrbk`
- `wslarc snapshot *`: `btrbk`
- `wslarc boot-usr`: `findmnt`, `mount`, `umount`, `systemctl`, `systemd-tmpfiles`,
  and `ldconfig`
- `wslarc restore`: `btrfs-progs`, and conditional `rsync`
- `wslarc attach`: WSL Windows interop and `wsl.exe`
- `wslarc hook-sync-systemd`: `pacman` and `ldconfig` on Arch; `dpkg-query`,
  `dpkg-deb`, and `rsync` on Debian/Ubuntu

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
```

`--yes` and `--verbose` are global options. `--yes` is accepted for saved
configurations, but it never bypasses the safety confirmations required by a
first initialization.

## Internal commands

Two subcommands are entry points for generated automation rather than
interactive use, and are hidden from `--help`:

- `wslarc hook-sync-systemd [--dry-run] [--apt-pre|--apt-post]` is called by
  the generated APT/pacman hooks when systemd-related packages change.
- `wslarc boot-usr --pre|--post` is called by `wslarc-usr-pre.service` and
  `wslarc-usr-post.service` during boot (see below).

Both remain runnable for debugging: `sudo wslarc boot-usr --post` repeats the
mount restore and repair step by hand after a failed boot, and
`sudo wslarc hook-sync-systemd --dry-run` previews the ext4 sync without
touching it.

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
`rsync -aAX --relative --no-implied-dirs`, prunes synced files that are no longer owned
by any installed package according to the `.wslarc-systemd-sync-manifest` manifest
stored on the ext4 root. `--no-implied-dirs` keeps rsync from recreating path
components it did not copy: on a merged-usr root, where `/lib` is a symlink to
`usr/lib`, recreating them would replace that symlink with a real directory. The hook
does not run `ldconfig` against the ext4
root, because `/etc` is shared and `/etc/ld.so.cache` would be rewritten from
the ext4-only view.

On Arch, the hook queries the installed package metadata, skips packages
whose name, version, and architecture already match inside the ext4 root,
copies the matching archives from `/var/cache/pacman/pkg` into the ext4
root, and installs them with `pacman --sysroot <root> -U --noconfirm`. A
guard environment variable prevents the nested pacman hook from recursing
into another sync. pacman runs `ldconfig` inside the ext4 root, which
rewrites the shared `/etc/ld.so.cache` from the ext4-only view, so the hook
rebuilds the live cache with `ldconfig -X` afterwards.

## Boot sequence when `/usr` is a Btrfs subvolume

WSL starts systemd from the ext4 root, mounts its own filesystems below `/usr`
(`/usr/lib/wsl/drivers`, `/usr/lib/wsl/lib`, `/usr/lib/modules/<kernel>`, the
kernel headers, `linux-tools`, and `/usr/bin/perf`), and only then does the
`@usr` subvolume cover `/usr`. `wslarc mount` therefore installs two units:

- `wslarc-usr-pre.service` runs before `usr.mount` and bind-mounts each of those
  WSL directory mounts under `/run/wslarc`. File mounts such as `/usr/bin/perf`
  stay hidden below `@usr`, because dpkg cannot replace a mounted file when the
  owning package is upgraded.
- `wslarc-usr-post.service` runs after all managed mounts. It bind-mounts the
  WSL mounts back on top of `@usr`, then runs `systemctl daemon-reload`,
  `systemd-tmpfiles --create`, rebuilds the shared linker cache with
  `ldconfig -X` (WSL and `ldconfig.service` rebuilt it from the ext4 view of
  `/usr` during boot), restarts `systemd-sysctl` and `systemd-modules-load`,
  reloads the D-Bus policy, and starts every unit wanted by `default.target`
  that has not been started yet, one `systemctl --job-mode=fail` call per unit.
  The last step covers services whose unit files exist only in `@usr`, because
  systemd built the boot transaction from the ext4 root. Units that already ran,
  were skipped by an unmet condition, or conflict with an active unit are left
  alone.

Both steps are resumable. `--post` persists the removal of a recovered path before it
drops that path's stash, so an interrupted boot can only leave a mounted stash behind,
never a record pointing at an empty directory. A stash that is no longer a mount is
treated as already restored. When a parent mount cannot be restored, its descendants are
left stashed and recorded so a later run of the post unit retries them instead of being
bound into a tree that the parent mount would hide again. The stash lives on `/run`, so a
new boot starts by stashing whatever it finds again; nothing is carried across boots.

Removing the `/usr` backup entry from the configuration and re-running `wslarc mount`
also disables and deletes the generated `usr.mount`, provided its `What=UUID=` is the
configured filesystem; a unit that mounts another disk at `/usr` is left alone. The
running system keeps the backup `/usr` until the next boot, and the command says so.

`wslarc mount` also writes `/etc/systemd/system/user@.service.d/wslarc.conf`
so that user managers start after the managed home mount and the post unit.
(`user@.service` is a template, so a `Before=` on it cannot be declared from
the mount unit.) `wslarc umount` disables the units and removes this drop-in.

Services that start before `usr.mount` inside a private mount namespace keep
seeing the ext4 `/usr` forever, because mount propagation below `/` is private
under WSL. This applies to `systemd-udevd` (its udev rules are not reloaded,
since a reload would re-read the ext4 rules), `systemd-logind`, and Arch's
`dbus-broker`; the post unit skips the D-Bus policy reload with a warning when
the daemon runs in such a namespace. If udev rules, D-Bus policy, or an early
service such as a firewall (`Before=network-pre.target`) must see `/usr`
content at boot, add its package to `ext4_sync.extra_packages` so it is
installed on the ext4 root.

`wslarc init` rsyncs each source through a read-only, non-recursive bind view,
so mounts below a source (the WSL filesystems above, container storage, and the
managed Btrfs mounts themselves) are never copied into a subvolume, while the
real files hidden below them still are. File mounts such as the empty
`/usr/bin/perf` placeholder that WSL creates are excluded instead. The wslarc
binary is installed to the ext4 root as well, because boot-time `attach` and
`boot-usr` run from it. The copies below the Btrfs base and on the ext4 root are only
written when that base is mounted with the configured `UUID` and the `@usr` subvolume
directory exists; otherwise `wslarc mount` fails instead of leaving a copy in a plain
directory that the real subvolume will hide at the next boot.

The ext4 root only has to provide the early-boot packages. The pre unit runs the ext4 copy of the binary, because it runs before `@usr` is
mounted; the post unit runs the `@usr` copy. The early-boot package set must therefore
also
provide `findmnt`, `mount`, and `umount` (util-linux), `systemctl`,
`systemd-tmpfiles`, and `ldconfig` there. If `attach` fails, the system boots
with that minimal root. Log in as root in that case, for example with
`wsl -d <distro> -u root`, because a login shell installed only in `@usr` is
not available.

## Development checks

```bash
pytest
ruff check src tests
mypy
```

Tests use fake command runners and temporary directories. They do not attach
VHDX files, format filesystems, mount subvolumes, run real rsync, or change
systemd state.

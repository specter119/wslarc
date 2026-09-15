# WSL Mount Architecture and a Lighter wslarc Design

Research date: 2026-08-20

## Executive conclusion

WSL was substantially open-sourced in 2025, but the release does not include
every Windows-side component. The public source is sufficient to explain the
mount behavior that matters to wslarc:

1. `wslservice.exe` exposes Windows GPU libraries through read-only Plan 9
   shares.
2. `mini_init` mounts those shares temporarily and moves them into the distro
   mount namespace.
3. WSL's Linux-side `init` moves the temporary mounts to
   `/usr/lib/wsl/drivers` and `/usr/lib/wsl/lib`, then updates the dynamic
   linker configuration.

The lighter wslarc design should cooperate with this mechanism instead of
copying WSL files or adding another overlay over `/usr`. `wslarc attach`
should attach the VHDX, recursively bind the already-installed WSL mounts to
stable staging paths, mount the `@usr` Btrfs subvolume directly at `/usr`, and
bind the saved mounts back into the new `/usr`. The generated systemd
configuration must not independently own `/usr`; otherwise the asynchronous
WSL boot command can race with `usr.mount`.

This removes the repeated overlay/copy layer from wslarc. WSL may still use
its own internal overlay for the GPU library share, but wslarc does not need
to reproduce or wrap it.

## Scope and source provenance

The primary WSL source was checked out at:

```text
https://github.com/microsoft/WSL
revision 13c097da103f9a15e7c8019169d45e99c76932ae
```

Relevant source excerpts are archived under
`research/wsl-architecture/_sources/`. Line numbers below refer to those
archived files. The source links and revision are also listed in
`_sources/README.md`.

## What "open-source WSL" means

Microsoft's official open-source documentation confirms that the WSL project
was released under an open-source license in May 2025. The public repository
contains the WSL user-mode and guest-side implementation, including the
`init` and `mini_init` code involved in this mount flow. The WSL2 Linux kernel
and WSLg are maintained in separate public repositories.

The release is not a complete publication of the Windows kernel integration.
Microsoft identifies these components as remaining closed source:

- `Lxcore.sys`
- `P9rdr.sys`
- `p9np.dll`

That boundary does not prevent this design work. The public source exposes the
mount handoff contract and the paths used by the guest, while the closed
components implement lower-level Windows integration behind that contract.

Source:

- [Microsoft: Open Source WSL](https://learn.microsoft.com/en-us/windows/wsl/opensource)

## WSL's actual GPU mount pipeline

### 1. Windows creates the shares

When GPU support is enabled, `WslCoreVm.cpp` adds read-only Plan 9 shares to
the utility VM. The relevant code is
`_sources/WslCoreVm.cpp:383-419`:

- `drivers` maps to
  `%SystemRoot%\System32\DriverStore\FileRepository`.
- `lib_inbox` maps, when present, to
  `%SystemRoot%\System32\lxss\lib`.
- `lib_packaged` maps to the WSL installation's `lib` directory.

The code explicitly notes that these GPU shares are not served by the normal
out-of-process DrvFs Plan 9 server, so they remain available when DrvFs is
disabled.

### 2. WSL mounts them temporarily

`_sources/lxfsshares.h:24-36` defines the guest destinations:

```text
LXSS_LIB_PREFIX       /usr/lib/wsl
LXSS_LIB_PATH         /usr/lib/wsl/lib
LXSS_GPU_DRIVERS_PATH /usr/lib/wsl/drivers
g_gpuShares            drivers -> /usr/lib/wsl/drivers
                      lib     -> /usr/lib/wsl/lib
```

In `_sources/wsl-init-main.cpp:1488-1610`, `mini_init`:

1. creates temporary mount directories,
2. mounts or assembles the GPU library view,
3. moves each GPU share into a temporary location with `MS_MOVE`, and
4. passes the temporary paths to the distro `init` through environment
   variables.

The library share can contain an overlay assembled by WSL from inbox and
packaged libraries. That is WSL's own implementation detail; wslarc should
leave that mount tree intact.

### 3. WSL moves them to their final paths

`_sources/wsl-init-config.cpp:83-166` defines
`RemoveMountAndEnvironmentOnScopeExit::MoveMount`. It calls
`UtilMount(..., MS_MOVE | MS_REC, ...)`, then removes the temporary directory.

`ConfigInitializeVmMode` at
`_sources/wsl-init-config.cpp:1034-1058` consumes the temporary GPU mount
environment variables and moves the mounts to the paths from `g_gpuShares`.
When GPU support is enabled, `ConfigApplyWindowsLibPath` also writes
`/etc/ld.so.conf.d/ld.wsl.conf` with `/usr/lib/wsl/lib` and runs `ldconfig`
(`_sources/wsl-init-config.cpp:2069-2124`).

This is important evidence for wslarc: `MS_MOVE` is not an unusual workaround.
It is the primitive WSL itself uses to hand private temporary mounts across
initialization stages. It does not mean that wslarc can move the final mounts
in place: the live final WSL mounts are shared propagation mounts, as shown
below.

### Live validation: final WSL mounts are shared

On the target WSL instance, the final mount entries are:

```text
/usr/lib/wsl/drivers  9p       shared:289
/usr/lib/wsl/lib      overlay  shared:290
```

The parent mounts `/`, `/run`, and `/usr` are also `shared`. A direct probe
with:

```text
mount --move /usr/lib/wsl/drivers /run/wslarc-mount-probe.../drivers
```

was rejected by the kernel with:

```text
moving a mount residing under a shared mount is unsupported
```

Therefore direct `mount --move` of these final WSL mounts is not the wslarc
design. A follow-up probe in an isolated mount namespace with private root
propagation made both source mounts private, but moving the 9P
`/usr/lib/wsl/drivers` mount to a temporary directory still failed with a
filesystem/invalid-argument error. This means the limitation is not only the
shared propagation state; the live WSL 9P mount also cannot be treated as a
normal movable filesystem mount in this environment. Making `/` globally
private would alter WSL's mount propagation contract and would not solve the
filesystem limitation.

A later recursive-bind probe in a private tmpfs returned Btrfs
`/dev/sdd[/@usr/lib/wsl/drivers]` and
`/dev/sdd[/@usr/lib/wsl/lib]`, not `9p` and `overlay`. This was not a valid
copy of the WSL mounts. The current `/usr` Btrfs mount overlays the path,
while the WSL entries remain recorded under the original root mount in
`mountinfo`; mount metadata and normal path lookup therefore disagree. A
valid staging operation must capture the WSL mounts before mounting `@usr`.
A diagnostic probe must first detach the temporary `@usr` mount in its
isolated namespace.

## Boot-order facts

The relevant order in `ConfigInitializeInstance` is:

1. Process `/etc/fstab` with `mount -a`
   (`_sources/wsl-init-config.cpp:704-714`).
2. Run `ConfigInitializeVmMode`, which finalizes the WSL GPU mounts
   (`_sources/wsl-init-config.cpp:717-726` and `1034-1058`).
3. Send the initialization response.
4. If `[boot] command` is configured, create a child process that executes
   `/bin/sh -c <command>` (`_sources/wsl-init-config.cpp:988-1011`).

The boot command is created asynchronously. WSL does not turn the command
into a synchronous prerequisite for systemd mount units. The systemd-enabled
path also launches distro init as a separate child process in
`_sources/wsl-init-systemd.cpp:2354-2410`. Therefore, a `[boot]` command is a
useful early hook, but it is not a sufficient systemd ordering primitive.

### Why an `@usr` fstab entry is not enough for the current workflow

An `/etc/fstab` entry would run before the boot command, which is useful for
ordering but not for this project: the VHDX is attached by `wslarc attach`, and
the WSL fstab pass has already happened before that command runs. Unless the
host attaches the VHDX before distro initialization, fstab cannot mount the
Btrfs `@usr` device in the normal wslarc workflow.

Consequently, `attach` must both attach the VHDX and mount `@usr`. The fstab
ordering still explains why WSL's own GPU finalization cannot be treated as
something wslarc should repeat later.

### Preferred alternative: mount `@usr` before WSL finalizes GPU mounts

The fstab limitation above applies to the current workflow, where the VHDX is
attached by the `[boot]` command. If the host attaches the VHDX before the
distro starts, an `/etc/fstab` entry can mount Btrfs `@usr` during WSL's fstab
phase. WSL then performs its normal GPU handoff after that mount:

```text
host attaches VHDX
    -> WSL processes fstab and mounts @usr at /usr
    -> mini_init prepares temporary GPU mounts
    -> init moves them to /usr/lib/wsl/drivers and /usr/lib/wsl/lib
    -> boot command/systemd starts
```

This is lighter than any wslarc rehoming scheme because WSL's own temporary
mounts are moved by WSL before the user boot hook, while `/usr` already points
at the Btrfs subvolume. It also avoids trying to move or bind-clone the final
shared 9P mount from the boot command.

This variant requires:

- a host-side or otherwise pre-distro mechanism to attach the VHDX;
- `/usr/lib/wsl/drivers` and `/usr/lib/wsl/lib` to exist in `@usr` before WSL
  finalization;
- an fstab entry that identifies the already-attached Btrfs filesystem; and
- no competing systemd `usr.mount`.

It is not implementable solely by the current `wslarc attach` `[boot]`
command, because that command runs after WSL has already processed fstab.
There is also no WSL configuration switch that delays GPU injection to an
arbitrary later path. The native target paths remain fixed in
`g_gpuShares[]`.

## Proposed wslarc design

### Ownership model

Use one owner for each mount:

| Mount | Owner |
| --- | --- |
| VHDX attachment | `wslarc attach` |
| `/usr` Btrfs subvolume | `wslarc attach` |
| `/usr/lib/wsl/drivers` | WSL, temporarily staged by wslarc only during `attach` |
| `/usr/lib/wsl/lib` | WSL, temporarily staged by wslarc only during `attach` |
| `/mnt/btrfs` and other configured subvolumes | systemd units |

There should be no persistent wslarc overlay for `/usr` and no persistent
systemd `usr.mount` competing with `attach`.

### Attach sequence

The intended `wslarc attach` sequence is:

1. Run the existing binfmt setup.
2. Detect whether the labelled Btrfs device is already available.
3. If necessary, call `wsl.exe --mount --vhd ... --bare`.
4. Discover all WSL-managed mount entries dynamically from
   `/proc/self/mountinfo` (or an equivalent structured mount query). At
   minimum, preserve `/usr/lib/wsl/drivers`, `/usr/lib/wsl/lib`, and the
   WSL-managed kernel-modules mount when present. Do not assume a fixed source
   string for the 9P or overlay filesystem.
5. For each discovered mount, create a recursive bind at a staging directory
   under `/run/wslarc/`. Keep the original WSL mounts attached to the old root
   mount tree. Make only the staging bind tree private if required; do not
   make `/` globally private.
6. Create the destination directories in the `@usr` subvolume and mount
   `subvol=@usr` directly at `/usr` using the Btrfs UUID.
7. Recursively bind each staged WSL mount back to its original logical path
   under the new `/usr`, then unmount only the temporary staging bind trees.
8. Verify that `/usr` is the expected Btrfs subvolume and that every
   preserved WSL mount target is visible below it.
9. On failure, unmount the new bind views, unmount the newly mounted `@usr`,
   and unmount the temporary staging binds. The original WSL mounts remain
   attached to the old root mount tree and can be restored when the `@usr`
   mount is removed.

The operation should be idempotent. If `/usr` is already the expected
subvolume and the WSL mounts are already beneath it, a subsequent attach must
not move or remount them unnecessarily.

### Why bind rehoming is lighter

Recursive bind rehoming does not copy the Windows driver files into ext4 or
Btrfs, does not create an additional union filesystem, and does not change
WSL's read-only share or overlay semantics. It creates a second mount view of
the existing live WSL mount, allowing the original shared mount to remain
anchored to the old root while the new view is attached below `@usr`.

This is slightly more mount bookkeeping than a successful `mount --move`, but
it avoids changing global propagation state and matches the live WSL mount
contract. The temporary bind views are removed after the new paths are
established.

### Systemd changes

The `@usr -> /usr` generated mount unit should be removed from the normal
generation and enablement path. Keeping it with
`ConditionPathIsMountPoint` is not a complete solution: if the unit starts
before `attach`, it can still mount `/usr`; if it starts after `attach`, the
condition only avoids one duplicate attempt. It does not establish the
required ordering.

The remaining subvolume units can continue to be generated and enabled. The
base unit and mounts such as `/home` or container storage remain systemd's
responsibility.

Existing installations need a migration path that disables the stale
`usr.mount` unit before the new design is used. The migration should avoid
deleting a unit that was not generated by wslarc.

### Restore behavior

Restoring `@usr` must use the same staging helper:

1. stage `/usr/lib/wsl/drivers` and `/usr/lib/wsl/lib`,
2. unmount `/usr`,
3. replace the Btrfs subvolume,
4. mount the restored `@usr` at `/usr`,
5. recreate the WSL mount views from staging, and
6. roll back the mount topology if any step fails.

Without staging, child mounts below `/usr` can prevent a clean unmount or
remain attached to the old mount tree.

### Umount behavior

Disabling systemd units is not enough after this change. The user-visible
disable flow must also explain that `/etc/wsl.conf`'s `[boot] command` owns the
attach-time `/usr` mount and must be removed or changed to disable that
behavior completely.

## Risks, limits, and fallback policy

1. **Mount capability:** `mount --move` requires root and a mount namespace in
   which the source and destination are visible. The live WSL 9P mount still
   rejects the operation after propagation is made private, so direct move
   must not be used as a production fallback.
2. **Mount-tree behavior:** WSL may change the number or type of child mounts.
   Discovery must use mount metadata rather than matching only `9p`.
3. **Propagation:** the final WSL mounts are shared and cannot be directly
   moved in the current instance. Do not globally run
   `mount --make-rprivate /`; if staging propagation must be changed, apply it
   only to the temporary bind clone.
4. **Partial failure:** maintain an explicit transaction record of staged
   bind views. Never continue with `/usr` mounted while a WSL mount is
   silently lost.
5. **Mount lifetime:** the original WSL mounts remain hidden below the old
   root mount while `@usr` is mounted. The implementation must unmount only
   its bind clones and must not accidentally unmount WSL's authoritative
   mounts.
6. **Hidden mountpoints:** once `@usr` is mounted, path-based commands such as
   `findmnt -T /usr/lib/wsl/drivers` can resolve to the Btrfs `@usr` tree even
   though `/proc/self/mountinfo` still contains the original 9P entry. Capture
   or open the WSL mount paths before the overmount; do not discover them
   afterward by ordinary path lookup.
7. **WSL implementation changes:** the exact Windows source paths and
   environment-variable names are implementation details. The wslarc
   interface should depend primarily on the stable guest target paths and
   mount metadata.

## Validation plan

### Local/static validation

- Unit-test mount-entry discovery and path filtering with synthetic
  `/proc/self/mountinfo` data.
- Unit-test transaction rollback without performing privileged mounts.
- Run a privileged recursive-bind probe for the live 9P and overlay mounts,
  then unmount only the bind clones.
- The simple bind probe must use a private tmpfs staging root. A bind placed
  directly below shared `/run` was observed as the existing Btrfs `@usr`
  mount, so that result is invalid and cannot prove bind rehoming.
- If `@usr` is already mounted, detach it inside an isolated test namespace
  before resolving the WSL source paths; otherwise the test binds the visible
  Btrfs tree rather than the hidden WSL mount.
- Run `cargo fmt --check`, `cargo test`, and `cargo clippy -- -D warnings`.
- Verify generated systemd output contains no wslarc-owned `usr.mount`.

### WSL integration validation

On a disposable or recoverable installation:

1. Capture `findmnt -R /usr` and `/proc/self/mountinfo`.
2. Recursively bind `/usr/lib/wsl/drivers` and `/usr/lib/wsl/lib` to temporary
   staging directories, verify their `9p` and `overlay` identities, and
   unmount only those bind clones.
3. Run `wsl --shutdown`, boot the distro, and verify:
   `findmnt -R /usr`, the Btrfs `subvol=@usr` option, and both WSL mount
   targets.
4. Run `ldconfig -p` and `nvidia-smi` where GPU support is available.
5. Repeat with the VHDX already attached, with GPU support disabled, and with
   one expected WSL mount absent.
6. Exercise a failed Btrfs mount and confirm the old mount topology is
   restored.

The source research establishes that this design is compatible with WSL's
mount primitives. The live `mount --move` experiment and cold-boot validation
remain prerequisites before shipping the implementation.

## Decision

Prefer the pre-attached-VHDX plus fstab design if the host-side attach
workflow is acceptable:

- attach the VHDX before launching the distro;
- mount `@usr` from fstab before WSL GPU finalization;
- pre-create the WSL target directories in `@usr`; and
- let WSL perform its native GPU mount handoff.

Only if pre-attach is unavailable should wslarc pursue a direct-mount plus
bind-rehoming design, and only after a cold-boot test proves that the WSL
mounts can be captured before the `@usr` overmount:

- implement mount staging in `attach`,
- share the staging transaction with `restore`,
- stop generating/enabling the `@usr` systemd mount,
- retain systemd ownership for the other configured subvolumes, and
- keep the existing WSL GPU mount tree intact.

Do not implement another copy-to-ext4 or union-overlay layer for `/usr`.

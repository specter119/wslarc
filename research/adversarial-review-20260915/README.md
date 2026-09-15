# WSLArc adversarial review and ablation experiments

## Scope and safety boundary

This review covers the current binary, including uncommitted changes and the
existing initialization, mounting, snapshot, restore, and package-hook flows.
Candidate generation was followed by an independent validation pass; candidates
were not accepted merely because a reviewer proposed them.

Experiments use temporary directories and injected command effects. No real
WSL reboot, VHDX formatting, Btrfs mount, live restore, or package installation
is part of this validation. Filesystem fixtures establish orchestration and
failure handling, not kernel-level Btrfs semantics.

## Confirmed failure mechanisms

- Binary replacement could delete its own source through an aliased `/usr`
  path. A matching pathname also did not establish that the hidden ext4
  boot binary was current.
- Initial home seeding preceded nested-subvolume creation. Existing exclusion
  directories were subsequently mistaken for already-created subvolumes.
- Transfer volumes did not preserve existing source content before overlaying
  the source directory.
- Initialization could leave a setup mount after a configuration-save error
  and silently accept incomplete copied data on retry.
- Restore renamed/unmounted the live volume before proving that the
  replacement could be created, did not roll back failures, and left nested
  subvolumes in the old backup.
- Arch synchronization constructed archive names from the machine
  architecture rather than package metadata and assumed all dependency
  archives remained in the cache.
- APT removed its pending record before successful synchronization. Stale-file
  removal errors could also lose their manifest entry.
- Debian's installed-state parser rejected held but installed packages.
- Generated automation dropped the selected configuration path; configuration
  loading expanded user templates before the interactive user selection.
- Preview commands could modify persistent configuration or pending state.
- Snapshot copying lacked an upfront volume/subvolume check, and status omitted
  the managed ext4 mount.

Nested restore data was left in the old backup, not proven permanently erased.
Expected destructive restore behavior (`rsync --delete`) is not itself a bug.
The host package database remains the authority for the Debian file mirror;
switching ownership checks to an unsynchronized ext4 database is not a fix.
Unverified WSL boot-order and driver-mount claims are not reported as confirmed
failures.

## Reproducible ablation runner

From the repository root:

```bash
python -B research/adversarial-review-20260915/ablate.py
```

The runner copies the current source and Cargo manifests to a temporary Linux
directory. It first requires the baseline tests to pass. For each mechanism it
then changes one source factor, requires the exact regression test to fail
(a compilation failure does **not** count), restores the source, and requires
that test to pass again. The original working tree is never mutated.

Two early attempted runs captured concurrently edited, incomplete source
snapshots and failed to compile their baselines. Neither attempt is counted as
evidence that an ablation reproduced a defect.

## Completed production-source experiments

The fixed baseline passed. Each of these sixteen single-factor mutants compiled,
failed its exact regression test, and passed again after restoring the fix:

| Removed mechanism | Result |
| --- | --- |
| Preserve the binary source before replacing an aliased target | Mutant rejected; restored test passed |
| Stage binary output before replacing the old destination | Mutant rejected; restored test passed |
| Avoid persistent configuration writes during preview | Mutant rejected; restored test passed |
| Keep the selected configuration in the generated timer service | Mutant rejected; restored test passed |
| Include the managed ext4 mount in status | Mutant rejected; restored test passed |
| Match Arch archives by their real architecture | Mutant rejected; restored test passed |
| Retain APT pending work until synchronization succeeds | Mutant rejected; restored test passed |
| Accept held but installed Debian packages | Mutant rejected; restored test passed |
| Remove uninstalled roots from the dependency closure | Mutant rejected; restored test passed |
| Register the parent seed before creating nested subvolumes | Mutant rejected; restored test passed |
| Seed existing transfer-source data | Mutant rejected; restored test passed |
| Verify existing paths really are subvolumes | Mutant rejected; restored test passed |
| Avoid writing initialization progress in dry-run | Mutant rejected; restored test passed |
| Stage a restore before unmounting | Mutant rejected; restored test passed |
| Roll back failed restore cutover/remount | Mutant rejected; restored test passed |
| Carry nested subvolumes into the restored parent | Mutant rejected; restored test passed |

The runner records a hash of the tested Rust source snapshot. Separate
standalone Python demonstrations of the old package behavior are not counted
as production-source ablations.

## External command contract check

A read-only probe found that `pacman -Qp --print-format=...` is invalid on the
installed pacman. The adapter now uses supported `pacman -Qip` output with
`LC_ALL=C`. A disposable archive containing `.PKGINFO` was queried with the real
pacman binary; its name, version, and `any` architecture were verified without
installing it.

The installed pacman 7.1 manual says sysroot prefixes configuration paths but
does not modify target paths. Older chroot-style sysroot behavior needs
guest-relative archive paths instead. The adapter selects the argument form
by pacman version, with regression tests for both forms. No actual installation
or chroot was run to test these branches.

## Final validation

- `cargo test --all-targets`: **112 passed**, zero failed.
- `cargo clippy --all-targets --all-features -- -D warnings`: passed.
- `cargo check --all-targets --all-features`: passed.
- `cargo fmt --all -- --check`: passed.
- `prek -a`: all hooks passed.
- `git diff --check`: passed.
- `ablate.py`: **16/16 mutants rejected**, **16/16 restored controls passed**.

Machine-readable results and the tested source hash are in `results.json`.
Earlier compilation/lint failures during integration were corrected; they are
not counted as successful mutation tests.

The restore worker was stopped after its main implementation and regression
tests were available. Final integration, source review, additional guards,
and production-source restore mutations were completed in the main thread.

No commit was created. Existing unrelated work and research were retained.
Live WSL boot, actual Btrfs kernel operations, and real package transactions
remain untested. Snapshot-only rsync write-back is deliberately documented as
non-atomic, and legacy ordinary exclusion directories are not silently
converted or deleted.

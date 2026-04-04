# AGENTS.md

## Documentation Boundary

- `README.md` is user-facing and should document command-specific dependencies, runtime expectations, and visible behavior
- This file is for developers and agents and records implementation boundaries and evolution rules
- Keep repository documentation, comments, and developer-facing notes in English

## CLI and Parsing Boundary

- `src/utils/cli.rs` is responsible for:
  - external CLI calls and executable dependency checks
  - structured parsing for commands such as `lsblk`, `findmnt`, `systemctl`, and `pacman`
  - reusable system-state helpers shared by multiple commands
- `src/commands/*.rs` is responsible for:
  - business-flow orchestration
  - user-visible output and prompt wording
  - command-specific fallback and degrade behavior

## Helper Placement Rules

- Prefer `src/utils/cli.rs` when a helper:
  - directly invokes an external CLI
  - parses CLI output into fields or structs
  - is reused by at least two commands
- Prefer keeping a helper inside a command file when it:
  - mainly formats user-visible output
  - only serves one subcommand
  - implements fallback behavior specific to that command

## Current Conventions

- Prefer structured output when possible:
  - use JSON first when a CLI supports it
  - otherwise prefer stable scalar or property-style output
  - prefer Rust `std::fs` for directory listing instead of shelling out to `ls`
- Do not over-abstract for the sake of uniformity:
  - `btrfs subvolume list` currently keeps text parsing plus a degrade path
  - only move command-local helpers into `src/utils/cli.rs` after a second command needs the same status model

## Dependency Checks

- Check real command-specific dependencies instead of doing a blanket global preflight
- Current command-level expectations:
  - `wslarc init`: `btrfs-progs`, `rsync`, and conditional `e2fsprogs`
  - `wslarc mount`: `btrbk`
  - `wslarc snapshot *`: `btrbk`

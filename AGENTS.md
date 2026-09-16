# AGENTS.md

## Documentation Boundary

- `README.md` is user-facing and documents command dependencies, runtime expectations,
  configuration, and visible behavior.
- Keep developer-facing documentation and comments in English.

## Python Boundaries

- `src/wslarc/utils/` owns external command execution, structured parsing, and reusable
  system-state helpers.
- `src/wslarc/commands/` owns business-flow orchestration and user-visible prompts.
- `src/wslarc/generators/` owns generated systemd, btrbk, and package-hook artifacts.
- Keep the CLI parser thin; command modules should be callable and testable without
  invoking the parser.

## Runtime Dependencies

- `wslarc init`: `btrfs-progs`, `rsync`, and conditional `e2fsprogs`
- `wslarc mount`: `btrbk`
- `wslarc snapshot *`: `btrbk`

Do not execute real VHDX attach, mount, rsync, snapshot, restore, or package-hook
operations in unit tests.

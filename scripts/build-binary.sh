#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

uv run nuitka \
  --onefile \
  --follow-imports \
  --include-package=wslarc \
  --output-dir=build-binary \
  --output-filename=wslarc \
  src/wslarc/cli.py

file build-binary/wslarc
build-binary/wslarc --version

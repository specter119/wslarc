#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

if ! command -v uv >/dev/null 2>&1; then
  echo "uv is required to build the binary: https://docs.astral.sh/uv/" >&2
  exit 1
fi

# The build dependency group provides Nuitka (standalone onefile) and the
# patchelf copy Nuitka needs on Linux. ccache is optional but speeds up
# repeated builds when installed with the system package manager.
uv run --group build patchelf --version >/dev/null

uv run --group build nuitka \
  --onefile \
  --follow-imports \
  --include-package=wslarc \
  --output-dir=build-binary \
  --output-filename=wslarc \
  src/wslarc/cli.py

file build-binary/wslarc
build-binary/wslarc --version

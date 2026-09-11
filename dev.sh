#!/bin/sh
# Use the existing scoped toolchain on this workstation; otherwise use PATH.
set -eu
project_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
local_tools="$project_dir/../../work/toolchain"
if [ -x "$local_tools/cargo/bin/cargo" ] && [ -d "$local_tools/rustup" ]; then
  export CARGO_HOME="$local_tools/cargo"
  export RUSTUP_HOME="$local_tools/rustup"
  export PATH="$CARGO_HOME/bin:$PATH"
fi
cd "$project_dir"
if ! command -v cargo >/dev/null 2>&1; then
  echo 'Install Rust from https://rustup.rs or run the included bin/runtime-gatekeeper.' >&2
  exit 1
fi
exec cargo "$@"

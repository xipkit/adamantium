#!/bin/sh
# Build from this directory so Cargo reads the isolated toolchain and linker config.
set -eu
cd "$(dirname "$0")"
exec cargo +nightly-2026-08-20 build --locked --release "$@"

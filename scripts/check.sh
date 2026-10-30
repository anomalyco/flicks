#!/bin/sh
# The one gate: formatting, Clippy with warnings as errors, the dependency policy, and every test.
# CI sets FLICKS_REQUIRE_ORACLES=1 with a stock ffmpeg (libx264, libx265, libvpx, libsvtav1, libdav1d), so oracle
# tests fail there instead of skipping; seeds that need libaom or libkvazaar skip everywhere that lacks them.
set -eu
cd "$(dirname "$0")/.."
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo deny --log-level error check
cargo test --workspace --locked --no-fail-fast

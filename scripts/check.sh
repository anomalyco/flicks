#!/bin/sh
# The one gate: formatting, Clippy with warnings as errors, and every test.
set -eu
cd "$(dirname "$0")/.."
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked --no-fail-fast

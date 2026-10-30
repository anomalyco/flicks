#!/bin/sh
# The one gate: formatting, Clippy with warnings as errors, the dependency policy, and every test.
# CI sets FLICKS_REQUIRE_ORACLES=1 with a full ffmpeg build (libaom-av1, libkvazaar), so oracle tests
# fail there instead of skipping; locally they skip what this ffmpeg can't encode.
set -eu
cd "$(dirname "$0")/.."
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo deny --log-level error check
cargo test --workspace --locked --no-fail-fast

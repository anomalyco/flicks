#!/bin/sh
# Builds the web app into a folder for `flicks serve --web <folder>` (default target/web).
# Needs the wasm32-unknown-unknown target and the wasm-bindgen CLI at Cargo.lock's version.
set -eu
cd "$(dirname "$0")/../.."
out=${1:-target/web}
version=$(sed -n '/^name = "wasm-bindgen"$/{n;s/^version = "\(.*\)"$/\1/p;}' Cargo.lock)
if [ "$(wasm-bindgen --version 2>/dev/null)" != "wasm-bindgen $version" ]; then
    echo "Install the matching CLI: cargo install wasm-bindgen-cli --version $version --locked" >&2
    exit 2
fi
CARGO_PROFILE_RELEASE_OPT_LEVEL=s CARGO_PROFILE_RELEASE_STRIP=debuginfo cargo build -p flicks-web --target wasm32-unknown-unknown --release --locked
rm -rf "$out"
wasm-bindgen --target web --no-typescript --remove-name-section --out-dir "$out" --out-name app target/wasm32-unknown-unknown/release/flicks_web.wasm
cp apps/web/index.html apps/web/start.js "$out/"
cp assets/brand/mark.svg "$out/favicon.svg"
echo "Built $out ($(wc -c < "$out/app_bg.wasm" | tr -d ' ') bytes of wasm)"

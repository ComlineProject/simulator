#!/usr/bin/env bash
# npm `prepare` hook. Runs when this repo is consumed as a git dependency
# (`"comline-simulator": "github:ComlineProject/simulator#<sha>"`) — pnpm/npm
# clone it, run this, then keep only `pkg/` (see `files` in package.json).
#
# Needs a Rust toolchain. Installs the wasm32 target and the wasm-bindgen CLI
# pinned to the crate's version (a one-time few-minute compile per machine,
# cached in ~/.cargo afterwards). `wasm-opt`, if on PATH, trims the binary.
set -euo pipefail
cd "$(dirname "$0")/.."

if ! command -v cargo >/dev/null 2>&1; then
	echo "comline-simulator: a Rust toolchain (cargo) is required to build the WASM engine." >&2
	echo "  install from https://rustup.rs and re-run the install." >&2
	exit 1
fi

rustup target add wasm32-unknown-unknown >/dev/null 2>&1 || true

wb=$(sed -n '/^name = "wasm-bindgen"$/{n;s/^version = "\(.*\)"/\1/p;}' Cargo.lock)
if ! wasm-bindgen --version 2>/dev/null | grep -qx "wasm-bindgen $wb"; then
	echo "comline-simulator: installing wasm-bindgen-cli $wb (one-time, a few minutes)…" >&2
	cargo install wasm-bindgen-cli --version "=$wb" --locked
fi

exec bash scripts/build-wasm.sh pkg comline_simulator --no-default-features

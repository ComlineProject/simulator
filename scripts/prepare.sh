#!/usr/bin/env bash
# The build hook run when this repo is consumed as a git dependency
# (`"comline-simulator": "github:ComlineProject/simulator#<sha>"`): the package
# manager clones it, runs this (via `prepare` for npm/pnpm, `prepack` for Yarn),
# then keeps only `pkg/` + `pkg-script/` (see `files` in package.json).
#
# Needs a Rust toolchain. Installs the wasm32 target and the wasm-bindgen CLI
# pinned to the crate's version (a one-time few-minute compile per machine,
# cached in ~/.cargo afterwards). `wasm-opt`, if on PATH, trims the binary.
#
# Builds both feature sets — `pkg/` (lean, no Rhai) and `pkg-script/`
# (scripted). Set COMLINE_SIMULATOR_SCRIPT=0 to skip the scripted one if you
# only need the lean module.
set -euo pipefail
cd "$(dirname "$0")/.."

# `prepare` and `prepack` both point here (npm / pnpm run one, Yarn the other);
# skip a redundant second pass in the same install.
if [ -f pkg/comline_simulator.js ] && [ pkg/comline_simulator.js -nt Cargo.toml ]; then
	echo "comline-simulator: pkg/ is already built — skipping" >&2
	exit 0
fi

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

bash scripts/build-wasm.sh pkg comline_simulator --no-default-features

if [ "${COMLINE_SIMULATOR_SCRIPT:-1}" != "0" ]; then
	bash scripts/build-wasm.sh pkg-script comline_simulator
fi

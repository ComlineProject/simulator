#!/usr/bin/env bash
# Build the crate to a wasm-bindgen web module — no wasm-pack (that lives under
# the sunset rustwasm org; wasm-bindgen itself is maintained). wasm-pack was
# only ever cargo build + wasm-bindgen + wasm-opt + a package.json, and this
# does the first three.
#
#   scripts/build-wasm.sh <out-dir> <out-name> [extra cargo build flags...]
#
# e.g.  scripts/build-wasm.sh pkg comline_simulator --no-default-features
#
# Produces <out-dir>/<out-name>{.js,.d.ts}, <out-dir>/<out-name>_bg.wasm[.d.ts].
# Needs: the wasm32-unknown-unknown target, `wasm-bindgen` (the CLI, pinned to
# the crate version), and — optionally, for size — `wasm-opt`.
set -euo pipefail

out_dir=${1:?out-dir}
out_name=${2:?out-name}
shift 2

target=wasm32-unknown-unknown
stem=comline_simulator # the cdylib artifact (crate name, - → _)

cargo build --release --target "$target" "$@"

rm -rf "$out_dir"
wasm-bindgen \
	--target web \
	--out-dir "$out_dir" \
	--out-name "$out_name" \
	"target/$target/release/$stem.wasm"

wasm="$out_dir/${out_name}_bg.wasm"
if command -v wasm-opt >/dev/null 2>&1; then
	before=$(wc -c <"$wasm")
	wasm-opt -Os "$wasm" -o "$wasm.opt" && mv "$wasm.opt" "$wasm"
	printf 'wasm-opt: %s → %s bytes\n' "$before" "$(wc -c <"$wasm")" >&2
else
	echo "note: wasm-opt not found — shipping unoptimised wasm ($(wc -c <"$wasm") bytes)" >&2
fi

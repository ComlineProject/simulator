# comline-simulator

The simulation engine behind the Comline
[playground](https://github.com/ComlineProject/playground) and
[tutorial](https://github.com/ComlineProject/tutorial): wire protocol instances
together over the **real `comline-runtime`**, in-memory, and watch every frame —
with fault injection, a steppable virtual clock, forwarding gateways, record &
replay, and user-scripted server behaviours.

Rust → WASM. The host (playground / tutorial / docs) provides the UI and calls in
through the `Sim` `wasm-bindgen` surface; the engine owns the wire, the
behaviours, the clock and the session model.

## Why a separate crate

Phase 1–2 of the simulation lived in the playground as TypeScript over a
*vendored copy* of `@comline/runtime` plus a re-implementation of the generated
client / dispatch glue (guarded by a conformance test). Compiling the engine from
Rust against `comline-runtime` directly removes all of that: no vendored port, no
re-implementation, no drift guard. The runtime's `no_std`, allocation-free,
**synchronous** contract also makes the engine a plain discrete-event pump — the
virtual clock is just the event queue's time, and there are no
promise-interleaving races.

The schema itself crosses in as JSON — the `Shape` projection the playground's
editor wasm already emits from `describe_project` (see `src/shape.rs`). Two
separate wasm modules can't share `FrozenUnit` values, so linking `comline-core`
here would only add a second copy of the compiler; the stable projection is the
better boundary. See `ComlineProject/docs` → Design → *Playground simulation*.

## Layout

```
src/rng.rs           seeded PRNG (mulberry32), bit-for-bit with the JS reference
src/faults.rs        the unreliable-wire spec + transforms
src/frame.rs         the frame tap the inspector reads
src/format.rs        the JSON WireFormat
src/shape.rs         the compiled-project projection (describe_project mirror)
src/clock.rs         the virtual clock + its event queue
src/wire.rs          one connection's tapped, fault-injecting channel
src/behavior.rs      the 8 server behaviours (reply … forward, script)
src/generic.rs       a dispatcher driven by a ProtocolShape, no codegen
src/model.rs         the Session: nodes, instances, connections, ops
src/session_codec.rs the Session ⇄ #s=… shareable link
src/record.rs        record & replay
src/engine.rs        many connections over one clock; the discrete-event pump
src/framedecode.rs   a raw frame → the inspector's decoded view
src/facade.rs        the #[wasm_bindgen] Sim surface
```

## Build

```sh
cargo test                                            # native
cargo test --no-default-features                       # without scripting

scripts/build-wasm.sh pkg comline_simulator --no-default-features   # lean → pkg/
scripts/build-wasm.sh pkg-script comline_simulator                  # scripted
```

`build-wasm.sh` is `cargo build --target wasm32-unknown-unknown` → `wasm-bindgen
--target web` → `wasm-opt` (no wasm-pack — that's under the sunset rustwasm
org). It needs `wasm-bindgen` (the CLI, matching the crate's version) and,
optionally, `wasm-opt`.

## Consuming it

The playground and tutorial take this as a **git dependency** and build it on
install:

```jsonc
"comline-simulator": "github:ComlineProject/simulator#<sha>"
```

The install clones the repo at that SHA and runs the build hook — `scripts/
prepare.sh` (via `prepare` for npm/pnpm, `prepack` for Yarn), which provisions
the wasm target + `wasm-bindgen-cli` and calls `build-wasm.sh`. It leaves two
builds:

- `pkg/` — lean. `import init, { Sim } from "comline-simulator"`
- `pkg-script/` — Rhai scripting. `import … from "comline-simulator/pkg-script/comline_simulator.js"`
  (the playground lazy-loads this)

Set `COMLINE_SIMULATOR_SCRIPT=0` before the install to skip `pkg-script/`. A
Rust toolchain has to be on the machine / CI doing the install.

## Dev note — the `script` feature & wasm size

`BehaviorKind::Script` runs a user-written Rhai script (sandboxed: no I/O, capped
operations / string / collection sizes). It's behind the `script` cargo feature,
**on by default**.

Rhai is heavy. It roughly **5× the wasm**:

| build | wasm (post `wasm-opt`) | gzipped |
| --- | --- | --- |
| default (`script`) | ~2.1 MB | ~580 KB |
| `--no-default-features` | ~445 KB | ~165 KB |

For now this is accepted. Before / during the playground rewire, decide between:

1. **Default on** — simplest; the playground page carries ~580 KB gz of extra
   wasm whether or not anyone opens the script editor.
2. **Ship lean, lazy-load** — build `--no-default-features` as the main artifact
   and fetch a second `script`-enabled wasm only when a `Script` behaviour is
   selected. Keeps the common path light.

The tutorial embed (2i) should build `--no-default-features` regardless.

# comline-simulator

The simulation engine behind the Comline
[playground](https://github.com/ComlineProject/playground) and
[tutorial](https://github.com/ComlineProject/tutorial): wire two protocol
instances together over the **real `comline-runtime`**, in-memory, and watch
every frame — with fault injection, a steppable virtual clock, and record &
replay.

Rust → WASM. The host (playground / tutorial / docs) provides the UI and calls
in through a small `wasm-bindgen` surface; the engine owns the wire, the
behaviours, the clock and the session model.

## Why a separate crate

Phase 1–2 of the simulation lived in the playground as TypeScript over a
*vendored copy* of `@comline/runtime` plus a re-implementation of the generated
client / dispatch glue (guarded by a conformance test). Compiling the engine
from Rust against `comline-core` + `comline-runtime` directly removes all of
that: no vendored port, no re-implementation, no drift guard, no
`describe_project` shim. The runtime's `no_std`, allocation-free, **synchronous**
contract also makes the engine a plain single-threaded pump — the virtual clock
is just the pump's time variable, and there are no promise-interleaving races.

See `ComlineProject/docs` → Design → *Playground simulation*.

## Layout

```
Cargo.toml       the Rust engine crate → WASM (`wasm-pack build --target web`)
src/lib.rs       the `Sim` wasm-bindgen facade (currently: the spike)
```

## Build

```sh
cargo test                                  # native
wasm-pack build --release --target web      # → pkg/
```

The spike (`smoke()`) does one `send` round-trip over `DatagramFraming` /
`Envelope` / `Dispatch` from `comline-runtime`, with the crate supplying only a
JSON `WireFormat`, a tapped in-memory channel, and the pump.

## Status

Scaffold + spike. Porting the engine (faults, clock, behaviours, record/replay,
session serde) from the playground's TypeScript, module by module.

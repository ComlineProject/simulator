# Published reports

This branch is written by CI only; don't push to it by hand. It holds the
latest verification reports of `main`, which the
[Comline Observatory](https://comlineproject.github.io/observatory/) charts
from this branch's history:

- `simulator.size.json`: bytes and gzip bytes of the simulator's wasm module,
  before and after `wasm-opt` (`wasm32` and `wasm32-opt`, the lean build; the
  `-script` ids are the build with scripting), from
  `.github/workflows/reports.yml` on every push to `main`.

The report names the `main` commit it measured. Every publish is one commit
here, so `git log -- simulator.size.json` is the series. The publishing
action is `ComlineProject/observatory/.github/actions/publish-report`.

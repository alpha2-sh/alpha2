# Release gate checklist

The gate runs once per release, at the release commit, on the three gate machines (`lin`, `win`,
`mac` in `machines.json`). Every command runs from the repository root of a clean checkout. Each
`run.sh` invocation writes `target/release-gate/<machine>-<UTC stamp>/` with `summary.txt`, one log
per step and `report.json`; a failure that `known-failures.json` does not excuse makes the verdict
`blocked` and blocks the release.

Prerequisites on every machine: Node 22 and npm, the pinned Rust toolchain (`rustup toolchain
install`), git, and the Python environment of the end-to-end tests:

```sh
cd python/alpha2 && uv venv -p 3.11 .venv && uv pip install --group dev && uv pip install --no-deps -e . && cd ../..
```

Windows: Git for Windows (run the commands in Git Bash), the D3D12 debug layer
(`Add-WindowsCapability -Online -Name Tools.Graphics.DirectX~~~~0.0.1.0`), Chrome and Edge.
macOS: Chrome, Safari with remote automation enabled (`safaridriver --enable`). Linux: Chrome,
weston and bwrap (`scripts/run-with-gpu-display.sh`), rootless podman with access to `/dev/dri`.

## Commands per machine

Linux (NVIDIA RTX 4090 and an AMD GPU):

```sh
scripts/release-gate/run.sh env p1 validation radv browser e2e stress perf
```

Windows (Intel Iris Xe and NVIDIA MX450 laptop, someone logged on at the console for the stock
browser pass):

```sh
scripts/release-gate/run.sh env p1 adapters corruption validation browser e2e stress perf
```

macOS (Apple silicon, console unlocked for Safari):

```sh
scripts/release-gate/run.sh env p1 validation browser e2e stress perf
```

With the previous release's gate tarball unpacked, add `--previous <its run directory>` to compare
skip records (`compare-skips.mjs`) and perf ratios. Run `perf` twice in a row once per release: the
per-configuration medians of the two runs must lie within each other's min-max.

Expected per phase:

- `env`: versions of the toolchain, GPU drivers and browsers, and every adapter of every backend.
- `p1`: the workspace suites (debug, release with `ALPHA2_GPU_STRICT=1`, default features) and
  `npm run test:js`, with `ALPHA2_REQUIRE` set to the machine's capabilities: no skip of a declared
  capability.
- `adapters` (Windows): render and engine suites on each adapter, the host suite on the encoder
  adapter.
- `corruption` (Windows): `y4m-blocks.mjs` prints `corrupt 0` for each of the three runs.
- `validation`: `scripts/test-gpu-validation.mjs` on the machine's backend (macOS with Metal shader
  validation); Linux also `npm run test:vulkan-validation`.
- `radv` (Linux): the native media suites in a current Mesa container with Vulkan Video required.
- `browser`: the WGSL, wasm-browser, editor package and host parity harnesses on the machine's real
  GPU (Linux: once per GPU through `run-with-gpu-display.sh`), then the wasm-browser run with
  `ALPHA2_BROWSER_SETTINGS=stock` (Windows: in the console session through
  `windows-console-run.sh`).
- `e2e`: Node and Python end-to-end tests against the release host, and `npm run test:gpu-export`.
- `stress`: the heavy runtime effect and the heaviest built-ins at 1920x1080 either finish or end in
  a classified GPU-work error within ten minutes.
- `perf`: `perf.mjs` summary per configuration; ratios and stage shares are compared with the
  previous release, never absolute numbers across machines.

Keep each run directory: `target/release-gate/*` goes into the gate tarball attached to the
release, where the next release finds the skip logs and perf summaries.

## Person-run items

These need a person at the machine and are not part of `run.sh`:

- Windows console-session browser run (about 45 minutes at the laptop): log on at the console, then
  `scripts/release-gate/run.sh browser` from Git Bash on that desktop.
- Sleep during an export (about 90 seconds per laptop, Windows and macOS): start a long export with
  the release host (`perf.mjs --config <a p1 configuration>`), put the laptop to sleep for at least
  30 seconds, wake it; the export finishes or fails with a classified error, and its timings do not
  count the sleep.
- Safari at the unlocked Mac (about 15 minutes): `node scripts/test-wgsl-browser.mjs --browser
  safari` and `node scripts/test-wasm-browser.mjs --browser safari` with the console unlocked.
- Install journeys from the release artifacts and from the registries (npm, PyPI, crates.io): the
  packaging journeys under `scripts/packaging/`, once per machine.

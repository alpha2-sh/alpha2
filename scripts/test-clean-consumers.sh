#!/bin/sh
# Packs the npm packages and installs them into fresh projects (plain Node, React) and the Rust
# crates into a fresh crate, so a published entry point that only resolves inside the workspace
# fails here.
set -eu

workspace=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT HUP INT TERM

cd "$workspace"
npm run build
node scripts/check-npm-package.mjs
package=$(npm pack --workspace alpha2 --pack-destination "$scratch" --silent)
node_package=$(npm pack --workspace @alpha2-sh/node --pack-destination "$scratch" --silent)
react_package=$(npm pack --workspace @alpha2-sh/react --pack-destination "$scratch" --silent)
editor_package=$(npm pack --workspace @alpha2-sh/editor --pack-destination "$scratch" --silent)
mkdir "$scratch/npm-consumer"
cd "$scratch/npm-consumer"
npm init --yes --silent >/dev/null
npm install --ignore-scripts "$scratch/$package" "$scratch/$node_package" "$scratch/$editor_package" --silent
node --input-type=module -e '
  await Promise.all([
    import("alpha2"), import("alpha2/player"), import("alpha2/exporter"),
    import("alpha2/media"), import("alpha2/text"), import("alpha2/robustness"),
    import("alpha2/audio"), import("alpha2/audio-worker"),
    import("alpha2/audio-timeline"), import("alpha2/project"),
    import("alpha2/audio-worklet"), import("alpha2/effects"),
    import("alpha2/worker"), import("alpha2/wasm"), import("alpha2/builder"),
    import("alpha2/edit"), import("alpha2/captions")
  ]);
'
node --input-type=module -e 'import("@alpha2-sh/node")'
node --input-type=module -e 'const { createEditor } = await import("@alpha2-sh/editor"); createEditor().dispose(); await import("@alpha2-sh/editor/browser");'
(cd "$workspace" && npm run test:editor:package)

# A React application, rendered on the server (the canvas itself needs a browser).
mkdir "$scratch/react-consumer"
cd "$scratch/react-consumer"
npm init --yes --silent >/dev/null
npm install --ignore-scripts "$scratch/$package" "$scratch/$react_package" react react-dom --silent
node --input-type=module -e '
  const { createElement } = await import("react");
  const { renderToString } = await import("react-dom/server");
  const { Alpha2Canvas } = await import("@alpha2-sh/react");
  const html = renderToString(createElement(Alpha2Canvas, {
    createPlayer: () => ({ initialize: async () => {}, present: async () => undefined, dispose() {} }),
    timeTicks: 0n, width: 16, height: 9,
  }));
  if (!html.includes("aspect-ratio:16 / 9")) throw new Error(html);
'
cd "$scratch/npm-consumer"
# The packed modules start with wasm's magic bytes; read as bytes since GNU and BSD od space text
# differently.
node -e '
  const { readFileSync } = require("node:fs");
  for (const file of process.argv.slice(1)) {
    const magic = readFileSync(file).subarray(0, 4).toString("hex");
    if (magic !== "0061736d") throw new Error(`${file} starts with ${magic}, not the wasm magic 0061736d`);
  }
' node_modules/alpha2/wasm/alpha2_wasm_bg.wasm node_modules/alpha2/wasm/alpha2_audio_worklet.wasm

mkdir "$scratch/rust-consumer"
cd "$scratch/rust-consumer"
cargo init --lib --name alpha2_consumer --quiet
cat >> Cargo.toml <<EOF
alpha2-engine = { path = "$workspace/crates/alpha2-engine" }
alpha2-native = { path = "$workspace/bindings/alpha2-native" }
EOF
cargo check --quiet

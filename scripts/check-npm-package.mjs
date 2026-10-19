// Checks the built npm packages before they are packed: every export target exists, the required
// files and notices ship, each wasm module is valid, built with SIMD and within its size budget,
// and declares only target features whose browser support is known (the oldest browsers that run
// them all are printed), the packaged wasm resolves the pinned rate table, and package and crate
// versions agree.

import { existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

const root = resolve(import.meta.dirname, "../packages/alpha2");
const manifest = JSON.parse(
  readFileSync(resolve(root, "package.json"), "utf8"),
);
for (const [subpath, target] of Object.entries(manifest.exports)) {
  if (typeof target === "string") {
    if (!existsSync(resolve(root, target))) {
      throw new Error(`missing export target for ${subpath}: ${target}`);
    }
    continue;
  }
  for (const field of ["types", "import"]) {
    const condition = target[field];
    if (
      typeof condition !== "string" ||
      !existsSync(resolve(root, condition))
    ) {
      throw new Error(`missing ${field} target for ${subpath}: ${condition}`);
    }
  }
}

for (const requiredPath of [
  "dist",
  "wasm",
  "README.md",
  "LICENSE.md",
  "THIRD_PARTY_NOTICES.md",
]) {
  if (!manifest.files.includes(requiredPath)) {
    throw new Error(`npm package files must include ${requiredPath}`);
  }
}

const workspaceNotices = readFileSync(
  resolve(root, "../../THIRD_PARTY_NOTICES.md"),
  "utf8",
);
const packageNotices = readFileSync(
  resolve(root, "THIRD_PARTY_NOTICES.md"),
  "utf8",
);
if (packageNotices !== workspaceNotices) {
  throw new Error("npm third-party notices differ from the workspace notices");
}

// Raise a budget when a feature needs it; do not contort code to fit. Keep each budget at least 5%
// over what the pinned toolchain builds, so another compiler release does not fail here.
const wasmBudgets = {
  // Includes the text host's Unicode tables (about 90 KB). Rust 1.99.0 builds 5,773,851 bytes.
  "alpha2_wasm_bg.wasm": 6_062_544,
  // Includes the band-limited resampling converter. Rust 1.99.0 builds 113,964 bytes.
  "alpha2_audio_worklet.wasm": 119_662,
  // Loaded only for SVG, animated image, Lottie, OpenEXR or image sequence sources. Rust 1.99.0
  // builds 3,549,335 bytes.
  "alpha2_extensions_bg.wasm": 3_726_802,
};
// Every target feature a packaged module may declare, with the first Chrome, Firefox and Safari
// that run it (webassembly.org/features.json), so a toolchain default turning on a newer feature
// fails here instead of raising the browser floor unseen. A feature that raises the floor also
// needs a probe in `detectBrowserCapabilities` (packages/alpha2/src/robustness.ts).
const featureFloors = {
  "+mutable-globals": { Chrome: "74", Firefox: "61", Safari: "13.1" },
  "+nontrapping-fptoint": { Chrome: "75", Firefox: "64", Safari: "15" },
  "+sign-ext": { Chrome: "74", Firefox: "62", Safari: "14.1" },
  "+bulk-memory": { Chrome: "75", Firefox: "79", Safari: "15" },
  "+bulk-memory-opt": { Chrome: "75", Firefox: "79", Safari: "15" },
  "+multivalue": { Chrome: "85", Firefox: "78", Safari: "13.1" },
  "+reference-types": { Chrome: "96", Firefox: "79", Safari: "15" },
  "+call-indirect-overlong": { Chrome: "96", Firefox: "79", Safari: "15" },
  "+simd128": { Chrome: "91", Firefox: "89", Safari: "16.4" },
};
const versionParts = (version) => version.split(".").map(Number);
const later = (a, b) => {
  const [x, y] = [versionParts(a), versionParts(b)];
  for (let i = 0; i < Math.max(x.length, y.length); i += 1) {
    if ((x[i] ?? 0) !== (y[i] ?? 0)) return (x[i] ?? 0) > (y[i] ?? 0) ? a : b;
  }
  return a;
};
const floor = { Chrome: "0", Firefox: "0", Safari: "0" };

for (const [filename, budget] of Object.entries(wasmBudgets)) {
  const bytes = readFileSync(resolve(root, "wasm", filename));
  if (bytes.length < 8 || !bytes.subarray(0, 4).equals(Buffer.from("\0asm"))) {
    throw new Error(`invalid WebAssembly package asset: ${filename}`);
  }
  if (bytes.length > budget) {
    throw new Error(
      `${filename} is ${bytes.length} bytes, over its ${budget}-byte budget`,
    );
  }
  const features = targetFeatures(new WebAssembly.Module(bytes));
  if (!features.includes("+simd128")) {
    throw new Error(
      `${filename} was built without SIMD (target features: ${features.join(" ")})`,
    );
  }
  for (const feature of features) {
    const floors = featureFloors[feature];
    if (floors === undefined) {
      throw new Error(
        `${filename} declares ${feature}, whose browser support the floor does not account for; add it to featureFloors`,
      );
    }
    for (const browser of Object.keys(floor)) {
      floor[browser] = later(floor[browser], floors[browser]);
    }
  }
}

console.log(
  `the packaged modules need Chrome ${floor.Chrome}, Firefox ${floor.Firefox}, Safari ${floor.Safari}`,
);

/** The `target_features` custom section rustc writes, as `+name` entries. */
function targetFeatures(module) {
  const [section] = WebAssembly.Module.customSections(
    module,
    "target_features",
  );
  if (section === undefined) return [];
  const bytes = new Uint8Array(section);
  let at = 0;
  const leb = () => {
    let value = 0;
    for (let shift = 0; ; shift += 7) {
      const byte = bytes[at++];
      value |= (byte & 0x7f) << shift;
      if (byte < 0x80) return value;
    }
  };
  const features = [];
  for (let count = leb(); count > 0; count -= 1) {
    const prefix = String.fromCharCode(bytes[at++]);
    const length = leb();
    features.push(
      prefix + new TextDecoder().decode(bytes.subarray(at, at + length)),
    );
    at += length;
  }
  return features;
}

const wasm = await import(
  pathToFileURL(resolve(root, "wasm", "alpha2_wasm.js")).href
);
wasm.initSync({
  module: readFileSync(resolve(root, "wasm", "alpha2_wasm_bg.wasm")),
});
const rateTable = JSON.parse(
  readFileSync(
    resolve(root, "../../tests/cases/video-rate-table.json"),
    "utf8",
  ),
);
for (const rateCase of rateTable.cases) {
  const resolved = JSON.parse(
    wasm.resolveRateControl(
      rateCase.codec,
      rateCase.width,
      rateCase.height,
      rateCase.frameRate.num,
      rateCase.frameRate.den,
      rateCase.level,
    ),
  );
  if (JSON.stringify(resolved) !== JSON.stringify(rateCase.expected)) {
    throw new Error(
      `wasm resolves ${JSON.stringify(rateCase)} to ${JSON.stringify(resolved)}`,
    );
  }
}
console.log(
  `validated ${rateTable.cases.length} pinned rate-control cases through the packaged wasm`,
);

const cargoVersion = (relativeManifest) => {
  const cargo = readFileSync(resolve(root, relativeManifest), "utf8");
  const direct = cargo.match(/^version = "([^"]+)"$/m)?.[1];
  const workspace = cargo.includes("version.workspace = true")
    ? readFileSync(resolve(root, "../../Cargo.toml"), "utf8").match(
        /^version = "([^"]+)"$/m,
      )?.[1]
    : undefined;
  const version = direct ?? workspace;
  if (version === undefined)
    throw new Error(`missing version in ${relativeManifest}`);
  return version;
};
for (const cargoManifest of [
  "../../bindings/alpha2-wasm/Cargo.toml",
  "../../bindings/alpha2-audio-worklet/Cargo.toml",
]) {
  const version = cargoVersion(cargoManifest);
  if (version !== manifest.version) {
    throw new Error(
      `version mismatch: alpha2@${manifest.version} and ${cargoManifest}@${version}`,
    );
  }
}
console.log(
  `validated ${Object.keys(manifest.exports).length} alpha2 npm exports and wasm assets`,
);

const nodeRoot = resolve(root, "../alpha2-node");
const nodeManifest = JSON.parse(
  readFileSync(resolve(nodeRoot, "package.json"), "utf8"),
);
for (const [subpath, target] of Object.entries(nodeManifest.exports)) {
  for (const field of ["types", "import"]) {
    const condition = target[field];
    if (
      typeof condition !== "string" ||
      !existsSync(resolve(nodeRoot, condition))
    ) {
      throw new Error(
        `missing @alpha2-sh/node ${field} target for ${subpath}: ${condition}`,
      );
    }
  }
}
for (const requiredPath of ["dist", "README.md", "LICENSE.md"]) {
  if (!nodeManifest.files.includes(requiredPath)) {
    throw new Error(
      `@alpha2-sh/node package files must include ${requiredPath}`,
    );
  }
}
if (
  nodeManifest.version !== manifest.version ||
  nodeManifest.dependencies?.alpha2 !== manifest.version
) {
  throw new Error("@alpha2-sh/node and alpha2 package versions must match");
}
console.log(
  `validated ${Object.keys(nodeManifest.exports).length} @alpha2-sh/node npm export and package metadata`,
);

const reactRoot = resolve(root, "../alpha2-react");
const reactManifest = JSON.parse(
  readFileSync(resolve(reactRoot, "package.json"), "utf8"),
);
for (const [subpath, target] of Object.entries(reactManifest.exports)) {
  for (const field of ["types", "import"]) {
    const condition = target[field];
    if (
      typeof condition !== "string" ||
      !existsSync(resolve(reactRoot, condition))
    ) {
      throw new Error(
        `missing @alpha2-sh/react ${field} target for ${subpath}: ${condition}`,
      );
    }
  }
}
for (const requiredPath of ["dist", "README.md", "LICENSE.md"]) {
  if (!reactManifest.files.includes(requiredPath)) {
    throw new Error(
      `@alpha2-sh/react package files must include ${requiredPath}`,
    );
  }
}
if (
  reactManifest.version !== manifest.version ||
  reactManifest.peerDependencies?.alpha2 !== manifest.version
) {
  throw new Error("@alpha2-sh/react and alpha2 package versions must match");
}
console.log(
  `validated ${Object.keys(reactManifest.exports).length} @alpha2-sh/react npm export and package metadata`,
);

const editorRoot = resolve(root, "../alpha2-editor");
const editorManifest = JSON.parse(
  readFileSync(resolve(editorRoot, "package.json"), "utf8"),
);
for (const [subpath, target] of Object.entries(editorManifest.exports)) {
  for (const field of ["types", "import"]) {
    if (!existsSync(resolve(editorRoot, target[field])))
      throw new Error(`Missing editor ${field} export: ${subpath}`);
  }
}
if (editorManifest.peerDependencies.alpha2 !== manifest.version)
  throw new Error("Editor peer version does not match tested SDK");
console.log(
  `validated ${Object.keys(editorManifest.exports).length} @alpha2-sh/editor npm exports`,
);

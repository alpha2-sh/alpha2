// Sets every npm, Cargo and Python version and pin, then updates the lock files. Packages pin each
// other exactly because they only fit when built from one commit.
//
//   npm run version:set -- 0.2.0
import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import {
  pluginsSections,
  setCargoVersions,
  workspaceSections,
} from "./lib/cargo-versions.mjs";

const version = process.argv[2];
if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version ?? "")) {
  console.error(
    "usage: node scripts/set-version.mjs <major.minor.patch[-prerelease]>",
  );
  process.exit(2);
}

const root = resolve(import.meta.dirname, "..");
const readJson = (path) => JSON.parse(readFileSync(path, "utf8"));
const manifestPaths = readJson(join(root, "package.json")).workspaces.map(
  (workspace) => join(root, workspace, "package.json"),
);
const manifests = manifestPaths.map(readJson);
const names = new Set(manifests.map((manifest) => manifest.name));
const dependencyFields = [
  "dependencies",
  "peerDependencies",
  "devDependencies",
  "optionalDependencies",
];

manifests.forEach((manifest, index) => {
  manifest.version = version;
  for (const field of dependencyFields)
    for (const name of Object.keys(manifest[field] ?? {}))
      if (names.has(name)) manifest[field][name] = version;
  writeFileSync(manifestPaths[index], `${JSON.stringify(manifest, null, 2)}\n`);
  console.log(`${manifest.name}@${version}`);
});

// The plugin SDK depends on `alpha2-plugin-abi` by path, so plugins take the engine version.
const writeCargoVersions = (manifest, sections, minimum) => {
  const path = join(root, manifest);
  const { text, lines } = setCargoVersions(
    readFileSync(path, "utf8"),
    sections,
  );
  if (lines < minimum)
    throw new Error(`${manifest}: no workspace version to set`);
  writeFileSync(path, text);
  console.log(`${manifest} workspace@${version} (${lines} lines)`);
};
writeCargoVersions("Cargo.toml", workspaceSections(version), 2);
writeCargoVersions("plugins/Cargo.toml", pluginsSections(version), 1);

// The Python package's own version comes from Cargo; only its `alpha2-host` pin is written here.
const pyprojectPath = join(root, "python", "alpha2", "pyproject.toml");
const pyproject = readFileSync(pyprojectPath, "utf8");
const pinned = pyproject.replace(
  /"alpha2-host==[^;"]+;/g,
  `"alpha2-host==${version};`,
);
if (pinned === pyproject && !pyproject.includes(`"alpha2-host==${version};`))
  throw new Error("python/alpha2/pyproject.toml: no alpha2-host pin to set");
writeFileSync(pyprojectPath, pinned);
console.log(`alpha2 (Python) depends on alpha2-host==${version}`);

for (const manifest of ["Cargo.toml", "fuzz/Cargo.toml", "plugins/Cargo.toml"])
  execFileSync(
    "cargo",
    [
      "metadata",
      "--offline",
      "--format-version",
      "1",
      "--manifest-path",
      manifest,
    ],
    { cwd: root, stdio: ["ignore", "ignore", "inherit"] },
  );

execFileSync(
  "npm",
  [
    "install",
    "--package-lock-only",
    "--offline",
    "--ignore-scripts",
    "--silent",
  ],
  { cwd: root, stdio: "inherit", shell: process.platform === "win32" },
);

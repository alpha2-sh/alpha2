// Sets the version of every npm package and inter-package dependency, the Cargo workspace's
// version with its crates in `[workspace.dependencies]`, and the Python package's pin of
// `alpha2-host` (its own version is the workspace's), then updates the lock files:
//
//   npm run version:set -- 0.2.0
//
// The packages pin each other exactly (they only fit when built from one commit), so a release
// changes all of them together. Only the root Cargo.toml carries a crate version.
import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

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

// `version` in `[workspace.package]` and each `{ version, path }` in `[workspace.dependencies]`.
const cargoPath = join(root, "Cargo.toml");
const cargoVersions = {
  "workspace.package": /^(version = )"[^"]*"$/,
  "workspace.dependencies":
    /^(alpha2[\w-]* = \{ version = )"[^"]*"(?=, path = )/,
};
let section = "";
let cargoLines = 0;
const cargo = readFileSync(cargoPath, "utf8")
  .split("\n")
  .map((line) => {
    section = line.match(/^\[([^\]]+)\]$/)?.[1] ?? section;
    const pattern = cargoVersions[section];
    if (!pattern?.test(line)) return line;
    cargoLines += 1;
    return line.replace(pattern, `$1"${version}"`);
  })
  .join("\n");
if (cargoLines < 2) throw new Error("Cargo.toml: no workspace version to set");
writeFileSync(cargoPath, cargo);
console.log(`Cargo workspace and its crates@${version} (${cargoLines} lines)`);

// The Python package takes its version from the Cargo workspace; only its `alpha2-host` pin is
// written here. PEP 440 reads `0.5.0-rc.1` as `0.5.0rc1`, the wheel's version.
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

// Resolving again writes the new versions of the path crates into each lock file.
for (const manifest of ["Cargo.toml", "fuzz/Cargo.toml"])
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

// Writes the license texts of the third-party Rust crates compiled into a build, with the crates
// each text covers, as Markdown (cargo-about, offline). alpha2's own crates are left out.
//   node scripts/crate-licenses.mjs --target <triple> --out <file> <package>...

import { execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { parseArgs } from "node:util";

const CARGO_ABOUT_VERSION = "0.9.2";

const workspace = resolve(import.meta.dirname, "..");
const { values, positionals } = parseArgs({
  options: { target: { type: "string" }, out: { type: "string" } },
  allowPositionals: true,
});
if (!values.target || !values.out || positionals.length === 0) {
  console.error(
    "usage: crate-licenses.mjs --target <triple> --out <file> <package>...",
  );
  process.exit(1);
}

const cargo = (args) =>
  execFileSync("cargo", args, {
    cwd: workspace,
    encoding: "utf8",
    maxBuffer: 256 * 1024 * 1024,
  });

let aboutVersion;
try {
  aboutVersion = cargo(["about", "--version"]).trim().split(/\s+/)[1];
} catch {
  aboutVersion = "none";
}
if (aboutVersion !== CARGO_ABOUT_VERSION) {
  console.error(
    `cargo-about ${CARGO_ABOUT_VERSION} is required; found ${aboutVersion} ` +
      `(cargo install cargo-about --version ${CARGO_ABOUT_VERSION} --locked --features cli)`,
  );
  process.exit(1);
}

const manifests = new Map(
  JSON.parse(
    cargo(["metadata", "--no-deps", "--format-version", "1"]),
  ).packages.map((pkg) => [pkg.name, pkg.manifest_path]),
);

// cargo-about's `cargo metadata` resolves every platform's crates, while a build fetches only its
// target's: fetch the rest so the offline runs below find them.
cargo(["fetch", "--locked"]);

const texts = new Map();
for (const name of positionals) {
  const manifest = manifests.get(name);
  if (!manifest) {
    console.error(`no workspace package ${name}`);
    process.exit(1);
  }
  const report = JSON.parse(
    cargo([
      "about",
      "generate",
      "--format",
      "json",
      "--config",
      join(workspace, "release/licenses/about.toml"),
      "--manifest-path",
      manifest,
      "--target",
      values.target,
      "--locked",
      "--offline",
      "--fail",
    ]),
  );
  for (const license of report.licenses) {
    // A crate without a registry source is one of the workspace's own.
    const crates = license.used_by
      .map(({ crate }) => crate)
      .filter((crate) => crate.source !== null);
    if (crates.length === 0) continue;
    const key = `${license.id}\n${license.text}`;
    const entry = texts.get(key) ?? {
      id: license.id,
      name: license.name,
      text: license.text,
      crates: new Map(),
    };
    for (const crate of crates)
      entry.crates.set(`${crate.name} ${crate.version}`, crate);
    texts.set(key, entry);
  }
}

const byName = (a, b) => (a < b ? -1 : a > b ? 1 : 0);
const entries = [...texts.values()]
  .map((entry) => ({ ...entry, crates: [...entry.crates.keys()].sort(byName) }))
  .sort((a, b) => byName(a.id, b.id) || byName(a.crates[0], b.crates[0]));

const lines = [
  "# Rust crate licenses",
  "",
  `The third-party Rust crates compiled into this build (\`${positionals.join("`, `")}\` for \`${values.target}\`), under the license texts below. alpha2's own crates are under [LICENSE.md](../LICENSE.md).`,
  "",
];
for (const entry of entries) {
  // A fence longer than any backtick run in the text.
  const longest = Math.max(
    0,
    ...(entry.text.match(/`+/g) ?? []).map((run) => run.length),
  );
  const fence = "`".repeat(Math.max(3, longest + 1));
  lines.push(
    `## ${entry.name} (${entry.id})`,
    "",
    `Used by ${entry.crates.map((crate) => `\`${crate}\``).join(", ")}.`,
    "",
    fence,
    entry.text.trimEnd(),
    fence,
    "",
  );
}

mkdirSync(dirname(values.out), { recursive: true });
writeFileSync(values.out, lines.join("\n"));
console.log(
  `${values.out}: ${entries.length} license texts for ${new Set(entries.flatMap((entry) => entry.crates)).size} crates`,
);

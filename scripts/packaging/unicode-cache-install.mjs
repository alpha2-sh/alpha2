#!/usr/bin/env node
// Installs alpha2-host from a mirror into caches whose paths leave the ANSI code page (Hangul, an
// emoji, an accented Latin letter) with the Node client's install command, then asks the host it
// installed for its CPU-renderer capabilities through the Node client, and, with --python, installs
// into the same caches with `python -m alpha2 install`. The names are built here from escapes, so
// no shell or console code page touches them. Prints PASS/FAIL per cache; exits 1 on a failure.
//
// usage: unicode-cache-install.mjs <base-dir> <mirror-url> [--python <python>]
import { spawnSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

const root = resolve(fileURLToPath(new URL("../..", import.meta.url)));
const cli = join(root, "packages/alpha2-node/dist/cli.js");

const { values, positionals } = parseArgs({
  allowPositionals: true,
  options: { python: { type: "string" } },
});
if (positionals.length !== 2) {
  console.error(
    "usage: unicode-cache-install.mjs <base-dir> <mirror-url> [--python <python>]",
  );
  process.exit(2);
}
const [base, mirror] = positionals;

const hangul = "\uc54c\ud30c2 \uce90\uc2dc";
const caches = [
  ["ascii", "ascii-cache"],
  ["hangul", hangul],
  ["latin-1", "caf\u00e9 cache"],
  ["hangul-emoji", `${hangul} \u{1F600}`],
];

const run = (command, args, env) =>
  spawnSync(command, args, {
    encoding: "utf8",
    env: { ...process.env, ...env },
  });

let failed = false;
for (const [label, name] of caches) {
  const cache = join(resolve(base), name);
  mkdirSync(cache, { recursive: true });
  const env = {
    ALPHA2_HOST_CACHE_DIR: cache,
    ALPHA2_HOST_DOWNLOAD_URL: mirror,
    ALPHA2_HOST_PATH: "",
  };
  const problems = [];
  const install = run(process.execPath, [cli, "install"], env);
  if (install.status !== 0 || !install.stdout.startsWith("installed "))
    problems.push(`node install: ${install.stdout}${install.stderr}`.trim());

  const probe = run(
    process.execPath,
    [
      "--input-type=module",
      "-e",
      `import { Alpha2NodeClient } from ${JSON.stringify(new URL("../../packages/alpha2-node/dist/index.js", import.meta.url).href)};\n` +
        "const client = new Alpha2NodeClient({ renderer: 'cpu' });\n" +
        "const report = await client.getCapabilities();\n" +
        "console.log(JSON.stringify(Object.keys(report)));",
    ],
    env,
  );
  if (probe.status !== 0)
    problems.push(`capabilities: ${probe.stdout}${probe.stderr}`.trim());

  if (values.python) {
    const python = run(values.python, ["-m", "alpha2", "install"], env);
    if (
      python.status !== 0 ||
      !/^(installed|already installed)/.test(python.stdout)
    )
      problems.push(`python install: ${python.stdout}${python.stderr}`.trim());
  }
  if (problems.length) {
    failed = true;
    console.log(`FAIL ${label}\n  ${problems.join("\n  ")}`);
  } else console.log(`PASS ${label}`);
}
process.exitCode = failed ? 1 : 0;

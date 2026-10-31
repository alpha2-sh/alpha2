#!/usr/bin/env node
// Builds alpha2-host and the Node client, then runs the Node end-to-end tests against that host
// with a per-test timeout. Works from cmd.exe, PowerShell and POSIX shells alike.
// usage: run-node-e2e.mjs [--release] [-- <node --test args>]

import { spawnSync } from "node:child_process";
import { readdirSync } from "node:fs";
import { join, resolve } from "node:path";

import { executable } from "./lib/cargo-target.mjs";

const root = resolve(import.meta.dirname, "..");
const argv = process.argv.slice(2);
const separator = argv.indexOf("--");
const own = separator < 0 ? argv : argv.slice(0, separator);
const passed = separator < 0 ? [] : argv.slice(separator + 1);
for (const arg of own)
  if (arg !== "--release") {
    process.stderr.write(
      `unknown argument ${arg}\nusage: run-node-e2e.mjs [--release] [-- <node --test args>]\n`,
    );
    process.exit(2);
  }
const profile = own.includes("--release") ? "release" : "debug";

function run(file, args, env = process.env) {
  const result = spawnSync(file, args, { cwd: root, stdio: "inherit", env });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}

run("cargo", [
  "build",
  ...(profile === "release" ? ["--release"] : []),
  "-p",
  "alpha2-host",
]);
// npm is a `.cmd` on Windows, which spawn cannot start without a shell, so the npm running this
// script (`npm_execpath`) is run with Node; outside npm a plain `npm` is started through a shell.
const npmArgs = ["run", "build", "--workspace", "@alpha2-sh/node"];
if (/\.[cm]?js$/.test(process.env.npm_execpath ?? ""))
  run(process.execPath, [process.env.npm_execpath, ...npmArgs]);
else {
  const result = spawnSync("npm", npmArgs, {
    cwd: root,
    stdio: "inherit",
    shell: process.platform === "win32",
  });
  if (result.status !== 0) process.exit(result.status ?? 1);
}

// cmd.exe does not expand globs, so the test files are listed here.
const directory = join("packages", "alpha2-node", "test", "e2e");
const files = readdirSync(join(root, directory))
  .filter((name) => name.endsWith(".test.mjs"))
  .sort()
  .map((name) => join(directory, name));
run(
  process.execPath,
  ["--test", "--test-timeout=180000", ...passed, ...files],
  {
    ...process.env,
    ALPHA2_HOST_BINARY: executable("alpha2-host", profile),
  },
);

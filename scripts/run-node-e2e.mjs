#!/usr/bin/env node
// Builds alpha2-host and the Node client and runs the Node end-to-end tests from any shell.
// --host runs them against that alpha2-host (such as a downloaded archive's) and builds none.
// usage: run-node-e2e.mjs [--release | --host <alpha2-host>] [-- <node --test args>]

import { spawnSync } from "node:child_process";
import { readdirSync } from "node:fs";
import { join, resolve } from "node:path";

import { executable } from "./lib/cargo-target.mjs";

const root = resolve(import.meta.dirname, "..");
const usage =
  "usage: run-node-e2e.mjs [--release | --host <alpha2-host>] [-- <node --test args>]";
const argv = process.argv.slice(2);
const separator = argv.indexOf("--");
const own = separator < 0 ? argv : argv.slice(0, separator);
const passed = separator < 0 ? [] : argv.slice(separator + 1);
let profile = "debug";
let host;
for (let index = 0; index < own.length; index += 1) {
  if (own[index] === "--release" && host === undefined) profile = "release";
  else if (own[index] === "--host" && profile === "debug" && own[index + 1]) {
    index += 1;
    host = resolve(own[index]);
  } else {
    process.stderr.write(`unknown argument ${own[index]}\n${usage}\n`);
    process.exit(2);
  }
}

function run(file, args, env = process.env) {
  const result = spawnSync(file, args, { cwd: root, stdio: "inherit", env });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}

if (host === undefined)
  run("cargo", [
    "build",
    ...(profile === "release" ? ["--release"] : []),
    "-p",
    "alpha2-host",
  ]);
// npm is a `.cmd` on Windows, which spawn cannot start without a shell, so run `npm_execpath`
// with Node.
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
    ALPHA2_HOST_BINARY: host ?? executable("alpha2-host", profile),
  },
);

// How the parity harness runs a host or the media CLI (to its exit, or to its JSON result), and
// loads the packaged wasm binding into Node.

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

import { root } from "./fixtures.mjs";

export function run(command, args, environment = {}) {
  return new Promise((resolveRun, reject) => {
    const child = spawn(command, args, {
      cwd: root,
      env: { ...process.env, ...environment },
      stdio: ["ignore", "ignore", "inherit"],
    });
    child.once("error", reject);
    child.once("exit", (code) => {
      if (code === 0) resolveRun();
      else reject(new Error(`${command} exited ${code}`));
    });
  });
}

export async function packagedWasm() {
  const directory = join(root, "packages", "alpha2", "wasm");
  if (!existsSync(join(directory, "alpha2_wasm_bg.wasm"))) {
    throw new Error("the packaged wasm is missing; run `npm run build` first");
  }
  const binding = await import(
    pathToFileURL(join(directory, "alpha2_wasm.js")).href
  );
  binding.initSync({
    module: await readFile(join(directory, "alpha2_wasm_bg.wasm")),
  });
  return binding;
}

export function runJson(command, args, environment = {}) {
  return new Promise((resolveRun, reject) => {
    const child = spawn(command, args, {
      cwd: root,
      env: { ...process.env, ...environment },
      stdio: ["ignore", "pipe", "inherit"],
    });
    let stdout = "";
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk) => {
      stdout += chunk;
    });
    child.once("error", reject);
    child.once("exit", (code) => {
      if (code !== 0) reject(new Error(`${command} exited ${code}`));
      else resolveRun(JSON.parse(stdout));
    });
  });
}

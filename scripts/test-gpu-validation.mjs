#!/usr/bin/env node

// Runs GPU test suites behind the Khronos validation layer, with synchronization validation, and
// fails on a failing test or on any validation message not in `TEST_KNOWN`
// (vulkan-validation-log.mjs). Only the layer sees Vulkan API misuse wgpu lets through.
//
//   node scripts/test-gpu-validation.mjs [-p <crate>]... [-- <test binary args>]
//
// The crates default to alpha2-render and alpha2-engine; the test binaries get `--test-threads=1`
// unless arguments follow `--`. Output goes to target/gpu-validation/<driver>/ (or
// `ALPHA2_VALIDATION_LOGS`), <driver> being the VK_ICD_FILENAMES manifest's name. Point
// VK_LAYER_PATH at a 1.4 layer (scripts/fetch-vulkan-validation-layer.sh prints one); without one
// this fails rather than skips.
//
// The layer writes to stdout: it truncates its log file at every vkCreateInstance. A run where no
// binary printed the layer's info-level report never loaded the layer.

import { spawnSync } from "node:child_process";
import { closeSync, mkdirSync, openSync, readFileSync } from "node:fs";
import { basename, dirname, join, resolve } from "node:path";

import {
  checkTestOutput,
  findValidationLayer,
  TEST_KNOWN,
} from "./lib/vulkan-validation-log.mjs";

const root = resolve(import.meta.dirname, "..");
const args = process.argv.slice(2);
const separator = args.indexOf("--");
const own = separator === -1 ? args : args.slice(0, separator);
const testArgs =
  separator === -1 ? ["--test-threads=1"] : args.slice(separator + 1);
const crates = own.flatMap((arg, index) =>
  own[index - 1] === "-p" ? [arg] : [],
);
if (own.some((arg, index) => arg !== "-p" && own[index - 1] !== "-p")) {
  process.stdout.write(
    "usage: test-gpu-validation.mjs [-p <crate>]... [-- <test binary args>]\n",
  );
  process.exit(2);
}
if (crates.length === 0) crates.push("alpha2-render", "alpha2-engine");
// One binary's limit; behind the layer the suites run a few times slower than without it.
const TIMEOUT_MS = Number(process.env.ALPHA2_GPU_TEST_TIMEOUT_MS ?? 1_800_000);

const layer = findValidationLayer(process.env.VK_LAYER_PATH);
if (layer.problem !== undefined) {
  process.stdout.write(`FAIL ${layer.problem}\n`);
  process.exit(1);
}

const build = spawnSync(
  "cargo",
  [
    "test",
    ...crates.flatMap((name) => ["-p", name]),
    "--no-run",
    "--message-format=json-render-diagnostics",
  ],
  { cwd: root, encoding: "utf8", maxBuffer: 256 * 1024 * 1024 },
);
if (build.status !== 0) {
  process.stdout.write(
    `FAIL cargo test --no-run exited ${build.status ?? build.signal}\n${build.stderr.slice(-4000)}\n`,
  );
  process.exit(1);
}
// The test harness binaries (unit and integration tests), not examples cargo builds alongside.
const binaries = build.stdout
  .split("\n")
  .filter((line) => line.startsWith("{"))
  .map((line) => JSON.parse(line))
  .filter(
    (message) =>
      message.reason === "compiler-artifact" &&
      message.executable &&
      message.profile?.test,
  )
  .map((message) => ({
    executable: message.executable,
    directory: dirname(message.manifest_path),
  }));

const driver = basename(
  process.env.VK_ICD_FILENAMES?.split(":")[0] ?? "default",
  ".json",
);
const logs = join(
  process.env.ALPHA2_VALIDATION_LOGS ?? join(root, "target/gpu-validation"),
  driver,
);
mkdirSync(logs, { recursive: true });
const environment = {
  ...process.env,
  VK_INSTANCE_LAYERS: "VK_LAYER_KHRONOS_validation",
  VK_KHRONOS_VALIDATION_VALIDATE_SYNC: "true",
  VK_KHRONOS_VALIDATION_DEBUG_ACTION: "VK_DBG_LAYER_ACTION_LOG_MSG",
  VK_KHRONOS_VALIDATION_REPORT_FLAGS: "error,warn,perf,info",
};
// A log file named here would be truncated by each instance.
delete environment.VK_KHRONOS_VALIDATION_LOG_FILENAME;

let failures = 0;
let loaded = 0;
for (const { executable, directory } of binaries) {
  const name = basename(executable);
  const log = join(logs, `${name}.log`);
  // One file for both streams keeps the layer's messages next to the test that caused them.
  const fd = openSync(log, "w");
  const result = spawnSync(executable, testArgs, {
    cwd: directory,
    env: environment,
    stdio: ["ignore", fd, fd],
    timeout: TIMEOUT_MS,
    killSignal: "SIGKILL",
  });
  closeSync(fd);
  const output = readFileSync(log, "utf8");
  const { problems, counts, layerLoaded } = checkTestOutput(output, TEST_KNOWN);
  if (layerLoaded) loaded += 1;
  const summary =
    [...counts]
      .sort((a, b) => b[1] - a[1])
      .map(([id, count]) => `${count} ${id}`)
      .join(", ") || "the layer reported nothing: no Vulkan instance";
  if (result.error?.code === "ETIMEDOUT")
    problems.unshift(`did not finish within ${TIMEOUT_MS / 1000} s`);
  else if (result.status !== 0)
    problems.unshift(
      `exited ${result.status ?? result.signal}: ${output.match(/^test result: .*$/m)?.[0] ?? "no test result"}`,
    );
  if (problems.length !== 0) {
    failures += 1;
    process.stdout.write(`FAIL ${name}: ${problems.join("; ")} (log ${log})\n`);
  } else {
    process.stdout.write(`ok ${name} (${summary})\n`);
  }
}
if (binaries.length === 0) {
  process.stdout.write(`FAIL no test binaries for ${crates.join(", ")}\n`);
  failures += 1;
} else if (loaded === 0) {
  process.stdout.write(
    `FAIL the layer reported no instance in any binary: it did not load (VK_LAYER_PATH=${process.env.VK_LAYER_PATH})\n`,
  );
  failures += 1;
}
process.exit(failures === 0 ? 0 : 1);

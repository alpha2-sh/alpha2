#!/usr/bin/env node

// Runs GPU test suites with a backend's API validation on, and fails on a failing test, on a
// validation problem the backend's route reports, or on a binary that ran on another adapter or
// backend than the run asked for.
//
//   node scripts/test-gpu-validation.mjs [--backend vulkan|metal|dx12|gl] [--shader-validation]
//     [--gpu-based-validation] [-p <crate>]... [-- <test binary args>]
//
// The backend defaults to the product's on this OS: vulkan on Linux, metal on macOS, dx12 on
// Windows. The crates default to alpha2-render and alpha2-engine, and on metal and dx12 also to
// alpha2-native-gpu, alpha2-native and alpha2-host, whose tests share devices with the platform's
// video APIs. The test binaries get `--test-threads=1 --show-output` unless arguments follow
// `--`, so every test's device line is in the log. Output goes to
// target/gpu-validation/<backend>-<ICD manifest or adapter>/ (or under `ALPHA2_VALIDATION_LOGS`).
//
// Every backend runs with WGPU_VALIDATION=1 and the validation collector (alpha2_gpu's
// validation_log) writing <binary>.validation.jsonl next to the log; ALPHA2_VALIDATION_LOG
// defaults to report here, and a lane that fails on the collector's errors sets it to fail.
//
// - vulkan: behind the Khronos validation layer with synchronization validation; any layer
//   message not in `TEST_KNOWN` (vulkan-validation-log.mjs) fails. Point VK_LAYER_PATH at a 1.4
//   layer (scripts/fetch-vulkan-validation-layer.sh prints one); without one this fails rather
//   than skips. The layer writes to stdout: it truncates its log file at every vkCreateInstance.
// - metal: the Metal API validation layer, which aborts the process on misuse
//   (MTL_DEBUG_LAYER_ERROR_MODE=assert); `--shader-validation` adds MTL_SHADER_VALIDATION.
// - dx12: the D3D12 debug layer, read by the collector from each device's info queue;
//   `--gpu-based-validation` adds WGPU_GPU_BASED_VALIDATION. Needs the Graphics Tools feature.
// - gl: GL debug output through the collector; Mesa's software rasterizer on Linux.

import { spawnSync } from "node:child_process";
import {
  closeSync,
  existsSync,
  mkdirSync,
  openSync,
  readFileSync,
  rmSync,
} from "node:fs";
import { basename, dirname, join, resolve } from "node:path";

import {
  backendEnvironment,
  backendProblem,
  countReport,
  defaultBackend,
  defaultCrates,
  judgeDevices,
  logDirectory,
  metalAssertions,
  summaryLine,
} from "./lib/gpu-validation-summary.mjs";
import {
  checkTestOutput,
  findValidationLayer,
  TEST_KNOWN,
} from "./lib/vulkan-validation-log.mjs";

const USAGE =
  "usage: test-gpu-validation.mjs [--backend vulkan|metal|dx12|gl] [--shader-validation] " +
  "[--gpu-based-validation] [-p <crate>]... [-- <test binary args>]\n";

const root = resolve(import.meta.dirname, "..");
const args = process.argv.slice(2);
const separator = args.indexOf("--");
const own = separator === -1 ? args : args.slice(0, separator);
const testArgs =
  separator === -1
    ? ["--test-threads=1", "--show-output"]
    : args.slice(separator + 1);
const crates = [];
let backend = defaultBackend(process.platform);
let shaderValidation = false;
let gpuBasedValidation = false;
for (let index = 0; index < own.length; index += 1) {
  const arg = own[index];
  if (arg === "-p" && index + 1 < own.length) crates.push(own[++index]);
  else if (arg === "--backend" && index + 1 < own.length)
    backend = own[++index];
  else if (arg === "--shader-validation") shaderValidation = true;
  else if (arg === "--gpu-based-validation") gpuBasedValidation = true;
  else {
    process.stdout.write(USAGE);
    process.exit(2);
  }
}
const unavailable = backendProblem(backend, process.platform);
if (unavailable !== undefined) {
  process.stdout.write(`FAIL ${unavailable}\n`);
  process.exit(1);
}
if (crates.length === 0) crates.push(...defaultCrates(backend));
// One binary's limit; behind the layer the suites run a few times slower than without it.
const TIMEOUT_MS = Number(process.env.ALPHA2_GPU_TEST_TIMEOUT_MS ?? 1_800_000);

if (backend === "vulkan") {
  const layer = findValidationLayer(process.env.VK_LAYER_PATH);
  if (layer.problem !== undefined) {
    process.stdout.write(`FAIL ${layer.problem}\n`);
    process.exit(1);
  }
}
if (backend === "dx12") {
  const layers = join(
    process.env.SystemRoot ?? "C:\\Windows",
    "System32",
    "d3d12SDKLayers.dll",
  );
  if (!existsSync(layers)) {
    process.stdout.write(
      "FAIL the D3D12 debug layer is not installed: Add-WindowsCapability -Online -Name " +
        "Tools.Graphics.DirectX~~~~0.0.1.0\n",
    );
    process.exit(1);
  }
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
// The test harness binaries (unit and integration tests), not examples cargo builds alongside. A
// crate's own test binary holds its GPU tests, so it must open a device: its library's, or its
// binary's when it has no library (the host). A tool binary beside a library, and an
// integration test binary, may not.
const artifacts = build.stdout
  .split("\n")
  .filter((line) => line.startsWith("{"))
  .map((line) => JSON.parse(line))
  .filter(
    (message) =>
      message.reason === "compiler-artifact" &&
      message.executable &&
      message.profile?.test,
  );
const withLibrary = new Set(
  artifacts
    .filter((message) => message.target.kind.includes("lib"))
    .map((message) => message.manifest_path),
);
const binaries = artifacts.map((message) => ({
  executable: message.executable,
  directory: dirname(message.manifest_path),
  opensDevice:
    message.target.kind.includes("lib") ||
    (message.target.kind.includes("bin") &&
      !withLibrary.has(message.manifest_path)),
}));

const logs = logDirectory(
  process.env.ALPHA2_VALIDATION_LOGS ?? join(root, "target/gpu-validation"),
  backend,
  process.env,
);
mkdirSync(logs, { recursive: true });
const manifest = process.env.VK_ICD_FILENAMES?.split(/[:;]/)[0];

let failures = 0;
let loaded = 0;
for (const { executable, directory, opensDevice } of binaries) {
  const name = basename(executable);
  const log = join(logs, `${name}.log`);
  const report = join(logs, `${name}.validation.jsonl`);
  rmSync(report, { force: true });
  // One file for both streams keeps each validation message next to the test that caused it.
  const fd = openSync(log, "w");
  const result = spawnSync(executable, testArgs, {
    cwd: directory,
    env: backendEnvironment(backend, {
      base: process.env,
      platform: process.platform,
      report,
      shaderValidation,
      gpuBasedValidation,
    }),
    stdio: ["ignore", fd, fd],
    timeout: TIMEOUT_MS,
    killSignal: "SIGKILL",
  });
  closeSync(fd);
  const output = readFileSync(log, "utf8");
  const problems = [];
  const counted = [];
  if (result.error?.code === "ETIMEDOUT")
    problems.push(`did not finish within ${TIMEOUT_MS / 1000} s`);
  else if (result.status !== 0)
    problems.push(
      `exited ${result.status ?? result.signal}: ${output.match(/^test result: .*$/m)?.[0] ?? "no test result"}`,
    );
  if (backend === "vulkan") {
    const layer = checkTestOutput(output, TEST_KNOWN);
    if (layer.layerLoaded) loaded += 1;
    problems.push(...layer.problems);
    counted.push(
      [...layer.counts]
        .sort((a, b) => b[1] - a[1])
        .map(([id, count]) => `${count} ${id}`)
        .join(", ") || "the layer reported nothing: no Vulkan instance",
    );
  }
  if (backend === "metal")
    problems.push(
      ...metalAssertions(output).map((line) => `Metal: ${line.trim()}`),
    );
  const devices = judgeDevices({ output, backend, opensDevice, manifest });
  problems.push(...devices.problems);
  if (existsSync(report)) {
    const { errors, warnings, tests } = countReport(
      readFileSync(report, "utf8"),
    );
    counted.push(
      `collector: ${errors} error(s), ${warnings} warning(s) in ${tests.length} test(s), ${basename(report)}`,
    );
  } else counted.push("collector: nothing");
  if (problems.length !== 0) failures += 1;
  process.stdout.write(
    `${summaryLine(name, devices.devices, problems, counted.join("; "))}${
      problems.length === 0 ? "" : ` (log ${log})`
    }\n`,
  );
}
if (binaries.length === 0) {
  process.stdout.write(`FAIL no test binaries for ${crates.join(", ")}\n`);
  failures += 1;
} else if (backend === "vulkan" && loaded === 0) {
  process.stdout.write(
    `FAIL the layer reported no instance in any binary: it did not load (VK_LAYER_PATH=${process.env.VK_LAYER_PATH})\n`,
  );
  failures += 1;
}
process.exit(failures === 0 ? 0 : 1);

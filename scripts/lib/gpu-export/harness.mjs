// What every case of test-gpu-export.mjs shares: the fixture, the host runs and check reporting.

import { spawnSync } from "node:child_process";
import { readFileSync, rmSync } from "node:fs";
import { basename, dirname, extname, join, resolve } from "node:path";

import { hostCommand } from "../host-command.mjs";
import { hostGpuError } from "../host-gpu-errors.mjs";

export const root = resolve(import.meta.dirname, "..", "..", "..");
// Enough that a cancel lands mid-flight with the pipeline full rather than during its ramp-up.
export const FRAMES = 100;
export const CANCEL_FRAME = 10;
// The 64x48 corpus fixture is below `h264_vulkan`'s minimum coded extent, so output is scaled up.
export const WIDTH = 256;
export const HEIGHT = 192;
export const TICKS_PER_FRAME = 120_000 / FRAMES;
// Luma sample frames for the Windows quality check: the first (no reference) and three more.
export const SAMPLE_FRAMES = [0, 33, 66, 99];
export const START_SAMPLES = 3;
// How long one host run may take before it is killed and reported as hung
// (ALPHA2_GPU_EXPORT_TIMEOUT_MS), so a deadlocked teardown does not hold the run until CI's limit.
export const TIMEOUT_MS = Number(
  process.env.ALPHA2_GPU_EXPORT_TIMEOUT_MS ?? 300_000,
);

/**
 * One run of the harness against `options.host`, writing under `scratch`: the host runs, the check
 * reporter and its failure count, and the default codec for each export.
 */
export function createHarness(options, scratch) {
  let failures = 0;
  const harness = {
    options,
    scratch,
    videoCodec: "h264",
    get failures() {
      return failures;
    },
    check(name, body) {
      try {
        body();
        process.stdout.write(`ok ${name}\n`);
      } catch (error) {
        failures += 1;
        process.stdout.write(`FAIL ${name}\n  ${error.message}\n`);
      }
    },
    capabilities() {
      const { answer, line, stderr } = harness.serve(capabilitiesRequest());
      if (!answer?.ok) {
        throw new Error(
          `${options.host} did not answer a capabilities request: ${
            stderr.slice(-500) || line
          }`,
        );
      }
      return answer.result;
    },
    /**
     * Sends `request` to a serving host and returns its answer (null when there is none), the line
     * it came on, the host's stderr, and how long the host took from its start to its exit.
     */
    serve(request, env = {}) {
      const started = performance.now();
      const result = spawnSync(...hostCommand(options.host, ["serve"]), {
        encoding: "utf8",
        input: `${JSON.stringify({ protocolVersion: 1, ...request })}\n`,
        env: { ...process.env, ...env },
        timeout: TIMEOUT_MS,
        killSignal: "SIGKILL",
      });
      const ms = performance.now() - started;
      const stderr = result.stderr ?? "";
      if (result.error?.code === "ETIMEDOUT") {
        throw new Error(
          `the ${request.operation} request to ${options.host} did not finish within ${TIMEOUT_MS / 1000} s and was killed: ${stderr.slice(-1000)}`,
        );
      }
      // A capabilities request does no GPU work to report on.
      const gpuError =
        request.operation === "export"
          ? hostGpuError(withoutStartReport(stderr))
          : null;
      if (gpuError !== null) {
        throw new Error(
          `the ${request.operation} request reported ${gpuError}: ${stderr.slice(-1000)}`,
        );
      }
      const lines = (result.stdout ?? "").trim().split("\n");
      for (const line of lines) {
        let parsed = null;
        try {
          parsed = JSON.parse(line);
        } catch {
          parsed = null;
        }
        if (parsed?.requestId === request.requestId) {
          return { answer: parsed, line, stderr, ms };
        }
      }
      return { answer: null, line: lines[0] ?? "", stderr, ms };
    },
    runExport({
      project,
      assets,
      output,
      env,
      codec = harness.videoCodec,
      extra = [],
      audio = false,
      startTicks,
      endTicks = 120_000,
      frameRate = `${FRAMES}/1`,
      terminateAfterMs,
    }) {
      rmSync(output, { force: true });
      // A host to terminate runs under a small Node parent that sends the signal, so its pipes stay open
      // to the end (`spawnSync`'s own timeout closes them first and loses the host's last lines).
      const [host, hostPrefix] = hostCommand(options.host, []);
      const [command, prefix] =
        terminateAfterMs === undefined
          ? [host, hostPrefix]
          : [
              process.execPath,
              [
                "-e",
                `const child = require("node:child_process").spawn(process.argv[1], process.argv.slice(2), { stdio: "inherit" });
setTimeout(() => child.kill("SIGTERM"), ${terminateAfterMs});
setTimeout(() => { child.kill("SIGKILL"); process.stderr.write("killed: did not finish\\n"); }, ${TIMEOUT_MS}).unref();
child.on("exit", (code, signal) => process.exit(code ?? (signal === null ? 1 : 128)));`,
                host,
                ...hostPrefix,
              ],
            ];
      const result = spawnSync(
        command,
        [
          ...prefix,
          "export",
          "--project",
          project,
          ...assets,
          "--out",
          output,
          ...(startTicks === undefined
            ? []
            : ["--start-ticks", String(startTicks)]),
          "--end-ticks",
          String(endTicks),
          "--frame-rate",
          frameRate,
          audio ? "--include-audio" : "--no-audio",
          "--video-codec",
          codec,
          ...extra,
        ],
        {
          encoding: "utf8",
          env: { ...process.env, ...env },
          // Backstop for the wrapper, which kills its host at TIMEOUT_MS.
          timeout: TIMEOUT_MS + (terminateAfterMs === undefined ? 0 : 5_000),
          killSignal: "SIGKILL",
        },
      );
      const stderr = result.stderr ?? "";
      if (
        result.error?.code === "ETIMEDOUT" ||
        stderr.endsWith("killed: did not finish\n")
      ) {
        throw new Error(
          `the export of ${basename(output)} did not finish within ${TIMEOUT_MS / 1000} s and was killed; its stderr ended: ${stderr.slice(-1000)}`,
        );
      }
      // Every run fails on a GPU error it reports, whatever its exit status says.
      const gpuError = hostGpuError(withoutStartReport(stderr));
      if (gpuError !== null) {
        throw new Error(
          `the export of ${basename(output)} reported ${gpuError}: ${(result.stderr ?? "").slice(-1000)}`,
        );
      }
      let parsed = null;
      try {
        parsed = JSON.parse(result.stdout);
      } catch {
        parsed = null;
      }
      return {
        status: result.status,
        signal: result.signal,
        stdout: result.stdout ?? "",
        stderr: result.stderr ?? "",
        result: parsed?.result ?? parsed ?? {},
      };
    },
  };
  return harness;
}

export function assertEqual(actual, expected, label) {
  if (actual !== expected) {
    throw new Error(
      `${label} is ${JSON.stringify(actual)}, expected ${JSON.stringify(expected)}`,
    );
  }
}

// H.264 where the readback path has it, and the MPEG-4 sink every build has otherwise.
export function readbackCodecOf(reported) {
  return (reported?.encode?.video ?? []).some((codec) => codec.id === "h264")
    ? "h264"
    : "mpeg4-part2";
}

export function capabilitiesRequest() {
  return { operation: "capabilities", requestId: "gpu-export-capabilities" };
}

/**
 * `stderr` without the host's start report, which quotes the GPU encode probe's refusals: those
 * are not GPU errors of this run.
 */
export function withoutStartReport(stderr) {
  return stderr
    .split("\n")
    .filter(
      (line) =>
        !/^alpha2-host GPU (?:encode: unavailable|passed over):/.test(line),
    )
    .join("\n");
}

export function partialOf(output) {
  return join(
    dirname(output),
    `${basename(output, extname(output))}.a2partial${extname(output)}`,
  );
}

export function exportFixture() {
  const project = JSON.parse(
    readFileSync(
      join(root, "tests", "cases", "host-video-export-project.json"),
      "utf8",
    ),
  );
  // One second of media at 100 fps is 100 output frames.
  project.settings.width = WIDTH;
  project.settings.height = HEIGHT;
  project.settings.frame_rate = { num: FRAMES, den: 1 };
  return project;
}

export function audioFixture() {
  const project = JSON.parse(
    readFileSync(
      join(root, "tests", "cases", "host-audio-export-project.json"),
      "utf8",
    ),
  );
  project.settings.width = WIDTH;
  project.settings.height = HEIGHT;
  return project;
}

export function align(value, granularity) {
  return Math.ceil(value / granularity) * granularity;
}

// The cancel contract: a cancel in flight, repeated, a cancel line mid-export (how a client
// cancels) and a SIGTERM mid-export each leave nothing behind and report no GPU error.

import { spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { basename, dirname, extname, join } from "node:path";

import { hostCommand } from "../host-command.mjs";
import { hostGpuError } from "../host-gpu-errors.mjs";
import {
  CANCEL_FRAME,
  FRAMES,
  TIMEOUT_MS,
  withoutStartReport,
} from "./harness.mjs";

/**
 * A Node script that runs `serve` (its arguments), writes the request it reads on stdin, sends a
 * cancel line for it after the first frame, and prints `{ state, code, status }`: the cancel's
 * answer, the export's error code and the host's exit status. The host's stderr is passed through.
 */
const CANCEL_LINE_CLIENT = `const { spawn } = require("node:child_process");
const { createInterface } = require("node:readline");
const request = JSON.parse(require("node:fs").readFileSync(0, "utf8"));
const host = spawn(process.argv[1], process.argv.slice(2), { stdio: ["pipe", "pipe", "inherit"] });
const summary = { state: null, code: null, status: null };
let sent = false;
createInterface({ input: host.stdout }).on("line", (line) => {
  let message;
  try { message = JSON.parse(line); } catch { return; }
  if (message.type === "progress" && message.completed > 0 && !sent) {
    sent = true;
    host.stdin.write(JSON.stringify({ protocolVersion: 1, requestId: request.requestId + ":cancel", operation: "cancel", target: request.requestId }) + "\\n");
  } else if (message.requestId === request.requestId + ":cancel") {
    summary.state = message.result?.state ?? null;
  } else if (message.requestId === request.requestId && "ok" in message) {
    summary.code = message.ok ? "ok" : message.error.code;
    host.stdin.end();
  }
});
host.on("exit", (code, signal) => {
  summary.status = code ?? signal;
  process.stdout.write(JSON.stringify(summary) + "\\n");
});
host.stdin.write(JSON.stringify(request) + "\\n");`;

export function cancelChecks(harness, { projectPath, assets }) {
  const { check, options, runExport, scratch } = harness;
  // Repeat in-flight cancellation to catch intermittent release-order failures.
  check(
    `a cancel at frame ${CANCEL_FRAME} leaves nothing behind, ${options.repeats} times`,
    () => {
      for (let attempt = 1; attempt <= options.repeats; attempt += 1) {
        const output = join(scratch, `cancelled-${attempt}.mp4`);
        const cancelled = runExport({
          project: projectPath,
          assets,
          output,
          env: { ALPHA2_EXPORT_CANCEL_AT_FRAME: String(CANCEL_FRAME) },
        });
        const context = `attempt ${attempt}`;
        // A bad teardown exits as a clean cancel does, so only its own report tells them apart.
        const teardown = cancelled.stderr.match(
          /^.*(?:left running|panicked|cleanup failed|unknown parked decoder surfaces).*$/m,
        );
        if (teardown) {
          throw new Error(
            `${context}: the cancel's teardown failed: ${teardown[0]}`,
          );
        }
        if (cancelled.status !== 1 || cancelled.signal !== null) {
          throw new Error(
            `${context}: a cancelled export exited ${cancelled.signal ?? cancelled.status}, expected status 1: ${cancelled.stderr.slice(-500)}`,
          );
        }
        // Failures go to stderr and successes to stdout. One drain can retire more than one frame, so the
        // stop frame is a floor, not an equality.
        const reported = `${cancelled.stdout}${cancelled.stderr}`;
        const stopped = reported.match(/cancelled at frame (\d+)/);
        if (!stopped) {
          throw new Error(
            `${context}: the error did not name the cancelled frame: ${reported.slice(-500)}`,
          );
        }
        const frame = Number(stopped[1]);
        if (frame < CANCEL_FRAME || frame >= FRAMES) {
          throw new Error(
            `${context}: stopped at frame ${frame}, expected one in [${CANCEL_FRAME}, ${FRAMES})`,
          );
        }
        if (!/\b\d+ parked decoder surfaces released/.test(cancelled.stderr)) {
          throw new Error(
            `${context}: the run did not report its parked decoder surfaces`,
          );
        }
        if (existsSync(output)) {
          throw new Error(
            `${context}: a cancelled export left ${basename(output)} behind`,
          );
        }
        const partial = join(
          dirname(output),
          `${basename(output, extname(output))}.a2partial${extname(output)}`,
        );
        if (existsSync(partial)) {
          throw new Error(
            `${context}: a cancelled export left ${basename(partial)} behind`,
          );
        }
      }
    },
  );

  // How a client cancels: a cancel line, answered `running`; the export stops, removes its partial
  // and is answered CANCELLED, and the host exits 0 once its input ends. Every OS.
  check(
    "a cancel line mid-export cancels it, and it leaves nothing behind",
    () => {
      const output = join(scratch, "cancelled-by-line.mp4");
      const assetPaths = {};
      for (let at = 0; at + 1 < assets.length; at += 2) {
        const [id, ...path] = assets[at + 1].split("=");
        assetPaths[id] = path.join("=");
      }
      const request = {
        protocolVersion: 1,
        operation: "export",
        requestId: "cancel-line",
        project: JSON.parse(readFileSync(projectPath, "utf8")),
        assets: assetPaths,
        startTicks: 0,
        // Half an hour of frames, so the cancel lands long before the last one.
        endTicks: 1_800 * 120_000,
        frameRate: { num: FRAMES, den: 1 },
        includeAudio: false,
        videoCodec: harness.videoCodec,
        output,
      };
      const [host, args] = hostCommand(options.host, ["serve"]);
      const run = spawnSync(
        process.execPath,
        ["-e", CANCEL_LINE_CLIENT, host, ...args],
        {
          encoding: "utf8",
          input: JSON.stringify(request),
          timeout: TIMEOUT_MS,
          killSignal: "SIGKILL",
        },
      );
      const stderr = run.stderr ?? "";
      const last = (run.stdout ?? "").trim().split("\n").at(-1) ?? "";
      let summary = null;
      try {
        summary = JSON.parse(last);
      } catch {
        summary = null;
      }
      if (
        summary?.state !== "running" ||
        summary?.code !== "CANCELLED" ||
        summary?.status !== 0
      ) {
        throw new Error(
          `expected a running cancel, CANCELLED and exit 0, got ${last || run.error}: ${stderr.slice(-1000)}`,
        );
      }
      const partial = join(
        dirname(output),
        `${basename(output, extname(output))}.a2partial${extname(output)}`,
      );
      for (const left of [output, partial]) {
        if (existsSync(left))
          throw new Error(`a cancelled export left ${basename(left)} behind`);
      }
      const gpuError = hostGpuError(withoutStartReport(stderr));
      if (gpuError !== null) throw new Error(`the run reported ${gpuError}`);
    },
  );

  // A termination signal cancels too. Not on Windows, where Node's `kill()` is an unhandleable
  // `TerminateProcess` and a console control event cannot target one host.
  if (process.platform === "win32") {
    process.stdout.write(
      "skipped: a SIGTERM mid-export (Node's kill() is TerminateProcess on Windows)\n",
    );
  } else
    check(
      "a SIGTERM mid-export cancels it, and it leaves nothing behind",
      () => {
        const output = join(scratch, "terminated.mp4");
        const terminated = runExport({
          project: projectPath,
          assets,
          output,
          // Half an hour of frames, so the signal lands long before the last one.
          endTicks: 1_800 * 120_000,
          terminateAfterMs: 3_000,
        });
        if (terminated.status !== 143) {
          throw new Error(
            `the host exited ${terminated.status ?? terminated.signal}, expected 143: ${terminated.stderr.slice(-1000)}`,
          );
        }
        if (!/alpha2-host export: frames [1-9]/.test(terminated.stderr)) {
          throw new Error(
            `the signal landed before any frame: ${terminated.stderr.slice(-1000)}`,
          );
        }
        if (!/native export was cancelled/.test(terminated.stderr)) {
          throw new Error(
            `the export was not cancelled: ${terminated.stderr.slice(-1000)}`,
          );
        }
        const partial = join(
          dirname(output),
          `${basename(output, extname(output))}.a2partial${extname(output)}`,
        );
        for (const left of [output, partial]) {
          if (existsSync(left))
            throw new Error(
              `a terminated export left ${basename(left)} behind`,
            );
        }
        const gpuError = terminated.stderr.match(
          /VK_ERROR_[A-Z_]+|VUID-[\w-]+|validation layer|did not finish within/i,
        );
        if (gpuError) throw new Error(`the run reported ${gpuError[0]}`);
      },
    );
}

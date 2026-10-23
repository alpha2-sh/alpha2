// The GPU export harness itself, without a GPU: where it looks for the host, and how it reports a
// missing one.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";

const root = resolve(import.meta.dirname, "..");
const script = join(root, "scripts", "test-gpu-export.mjs");

/**
 * Runs the harness as on `platform` (by redefining `process.platform` before it loads), with no
 * host override.
 */
function harness(platform, args = []) {
  const env = { ...process.env };
  delete env.ALPHA2_HOST_BINARY;
  delete env.ALPHA2_REQUIRE_GPU_EXPORT;
  // os.tmpdir() on win32 reads TEMP.
  env.TEMP = tmpdir();
  const preload = `data:text/javascript,Object.defineProperty(process,"platform",{value:${JSON.stringify(platform)}})`;
  return spawnSync(process.execPath, ["--import", preload, script, ...args], {
    cwd: root,
    env,
    encoding: "utf8",
  });
}

const windowsHost = join(root, "target", "release", "alpha2-host.exe");

test(
  "the default host on Windows is the alpha2-host.exe cargo builds",
  {
    skip:
      existsSync(windowsHost) &&
      "a release alpha2-host.exe is built here, so the harness runs it",
  },
  () => {
    // No alpha2-host.exe is built here, so the harness stops at its precondition, naming the path.
    const run = harness("win32");
    assert.equal(run.status, 1, run.stderr);
    const expected = windowsHost;
    assert.equal(
      run.stdout.split("\n")[0],
      `FAIL ${expected} does not exist. Run \`cargo build --release -p alpha2-host\` first, or pass --host.`,
      run.stderr,
    );
  },
);

test("a precondition that does not hold is a FAIL line, not an uncaught error", () => {
  for (const args of [["--host", join(root, "no-such-host")], ["--bogus"]]) {
    const run = harness(process.platform, args);
    assert.equal(run.status, 1);
    assert.match(run.stdout, /^FAIL /, `${args}: ${run.stderr}`);
    assert.doesNotMatch(run.stderr, /\n\s+at /, "a stack trace");
  }
});

test("a GPU error the host reports fails the run on any backend, not only Vulkan's", async () => {
  const { hostGpuError } = await import("./lib/host-gpu-errors.mjs");
  for (const [line, expected] of [
    // alpha2-gpu's GpuDiagnostic display, which every backend's host prints.
    [
      "alpha2-host uncaptured GPU validation error: Texture usage is invalid",
      "uncaptured GPU validation error",
    ],
    [
      "alpha2-host uncaptured GPU out-of-memory error: allocation failed",
      "uncaptured GPU out-of-memory error",
    ],
    [
      "alpha2-host GPU device lost (Unknown): Metal command buffer error",
      "GPU device lost",
    ],
    // The render crate's dropped-work and timeout messages.
    [
      "ALPHA2_ERROR: GPU device lost: it completed submitted work without running it",
      "GPU device lost",
    ],
    [
      "GPU_TIMEOUT: GPU work did not complete within the wait timeout",
      "GPU work did not complete within",
    ],
    [
      "Validation Error: [ VUID-vkDestroyDevice-device-05137 ]",
      "VUID-vkDestroyDevice-device-05137",
    ],
    ["vkQueueSubmit failed: VK_ERROR_DEVICE_LOST", "VK_ERROR_DEVICE_LOST"],
  ]) {
    assert.equal(
      hostGpuError(`alpha2-host export: frames 10/100\n${line}\n`),
      expected,
      line,
    );
  }
  assert.equal(
    hostGpuError(
      "alpha2-host export: frames 100/100\nalpha2 native export encoder: h264_vulkan (hardware, GPU-resident)\n",
    ),
    null,
  );
});

/**
 * Runs the harness against the protocol fixture, which ends a cancel the way `scenario` says, and
 * returns its output and the line of the cancel check.
 */
function againstFixture(scenario, env = {}) {
  const started = Date.now();
  const run = spawnSync(
    process.execPath,
    [
      script,
      "--host",
      join(root, "scripts", "fixtures", "gpu-export-host.mjs"),
      "--repeats",
      "1",
    ],
    {
      cwd: root,
      env: { ...process.env, GPU_EXPORT_FIXTURE: scenario, ...env },
      encoding: "utf8",
      // This bounds the test, not the harness: a harness waiting forever on a hung host is the bug.
      timeout: 60_000,
      killSignal: "SIGKILL",
    },
  );
  const lines = (run.stdout ?? "").split("\n");
  const index = lines.findIndex((line) => / a cancel at frame 10 /.test(line));
  return {
    run,
    seconds: (Date.now() - started) / 1000,
    cancel: index < 0 ? null : lines.slice(index, index + 2).join("\n"),
  };
}

test("a clean cancel passes the cancel check", () => {
  const { cancel } = againstFixture("clean");
  assert.match(cancel ?? "", /^ok a cancel at frame 10 leaves nothing behind/);
});

test("a cancel whose teardown went wrong fails the check, whatever its status", () => {
  for (const [scenario, reason] of [
    ["decode-left-running", /decode worker left running/],
    ["encode-panicked", /panicked/],
    ["cleanup-failed", /cleanup failed/],
    ["unknown-parked", /unknown parked decoder surfaces/],
    // A crash is a signal on POSIX and an access violation's status (0xC0000005) on Windows.
    ["crash", process.platform === "win32" ? /exited 3221225477\b/ : /SIGSEGV/],
  ]) {
    const { cancel, run } = againstFixture(scenario);
    assert.match(
      cancel ?? "",
      /^FAIL a cancel at frame 10/,
      `${scenario}: ${run.stdout}`,
    );
    assert.match(cancel, reason, scenario);
  }
});

test("a host that hangs is killed and reported, and the run goes on", () => {
  const { cancel, run, seconds } = againstFixture("hang", {
    ALPHA2_GPU_EXPORT_TIMEOUT_MS: "2000",
  });
  assert.equal(run.error, undefined, `the harness itself hung: ${run.stdout}`);
  assert.ok(seconds < 45, `${seconds} s`);
  assert.match(cancel ?? "", /^FAIL a cancel at frame 10/, run.stdout);
  assert.match(cancel, /did not finish within 2 s.*frames 10\/100/s);
  // Windows has no SIGTERM to send a host, so the harness skips that check there.
  assert.match(
    run.stdout,
    process.platform === "win32"
      ? /^skipped: a SIGTERM mid-export/m
      : /a SIGTERM mid-export cancels it/,
  );
});

test("a host that never answers its capabilities is killed and reported", () => {
  const { run, seconds } = againstFixture("hang-capabilities", {
    ALPHA2_GPU_EXPORT_TIMEOUT_MS: "2000",
  });
  assert.equal(run.error, undefined, "the harness itself hung");
  assert.ok(seconds < 45, `${seconds} s`);
  assert.equal(run.status, 1);
  assert.match(run.stdout, /^FAIL .*capabilities.*did not finish within 2 s/m);
});

test("--expect-unavailable and --require-gpu contradict each other, from a flag or the environment", () => {
  for (const [args, env] of [
    [["--expect-unavailable", "--require-gpu"], {}],
    [["--expect-unavailable"], { ALPHA2_REQUIRE_GPU_EXPORT: "1" }],
  ]) {
    const run = spawnSync(
      process.execPath,
      [
        script,
        "--host",
        join(root, "scripts", "fixtures", "gpu-export-host.mjs"),
        ...args,
      ],
      { cwd: root, env: { ...process.env, ...env }, encoding: "utf8" },
    );
    assert.equal(run.status, 1, `${args}: ${run.stderr}`);
    assert.match(
      run.stdout,
      /^FAIL --expect-unavailable asserts there is no GPU-resident path/,
      `${args}`,
    );
  }
});

/** Runs `--expect-unavailable` against the protocol fixture's `scenario`. */
function unavailableAgainstFixture(scenario) {
  return spawnSync(
    process.execPath,
    [
      script,
      "--host",
      join(root, "scripts", "fixtures", "gpu-export-host.mjs"),
      "--expect-unavailable",
    ],
    {
      cwd: root,
      env: { ...process.env, GPU_EXPORT_FIXTURE: scenario },
      encoding: "utf8",
      timeout: 60_000,
      killSignal: "SIGKILL",
    },
  );
}

test("--expect-unavailable passes a host that refuses the path, says why, and falls back", () => {
  const run = unavailableAgainstFixture("unavailable");
  assert.equal(run.status, 0, run.stdout);
  for (const name of [
    "the host reports no GPU-resident output path, and why",
    "an auto export completes on the readback path and says why",
    "a gpu-required export fails GPU_OUTPUT_UNAVAILABLE before any output exists",
    "the GPU encode probe keeps the host's start within its bound",
  ]) {
    assert.ok(run.stdout.includes(`ok ${name}\n`), `${name}: ${run.stdout}`);
  }
  assert.doesNotMatch(run.stdout, /^FAIL/m);
});

test("--expect-unavailable fails a fallback that does not say why and a refusal that left its output", () => {
  const run = unavailableAgainstFixture("unavailable-broken");
  assert.equal(run.status, 1, run.stdout);
  assert.match(
    run.stdout,
    /^FAIL an auto export completes on the readback path and says why\n {2}gpuOutputPathUnavailable is null/m,
  );
  assert.match(
    run.stdout,
    /^FAIL a gpu-required export fails GPU_OUTPUT_UNAVAILABLE before any output exists\n {2}the refused export left required\.mp4 behind/m,
  );
  assert.match(run.stdout, /^ok the GPU encode probe keeps the host's start/m);
});

test("--expect-unavailable on a host that has the path fails at once, naming the mode to run", () => {
  const run = unavailableAgainstFixture("clean");
  assert.equal(run.status, 1, run.stdout);
  assert.match(
    run.stdout,
    /^FAIL the host reports no GPU-resident output path, and why\n {2}gpuOutputPath\.available is true, expected false: .*--require-gpu/m,
  );
  // Nothing else ran: an export there would only fail for the same reason.
  assert.doesNotMatch(run.stdout, /auto export|gpu-required|start within/);
});

test("the copy ledger is two plane copies per frame, of the frame's NV12 bytes, on gpu-copy only", async () => {
  const { copyLedgerProblem, encoderInputCopyLedger, nv12FrameBytes } =
    await import("./lib/gpu-export-checks.mjs");
  assert.equal(nv12FrameBytes(256, 192), 73_728);
  // EncoderInputLayout::Nv12.frame_bytes: luma plus half of it, rounded down.
  assert.equal(nv12FrameBytes(3, 3), 13);
  const ledger = encoderInputCopyLedger("gpu-copy", 100, 256, 192);
  assert.deepEqual(ledger, { operations: 200, bytes: 7_372_800 });
  assert.equal(encoderInputCopyLedger("direct-target", 100, 256, 192), null);
  assert.equal(encoderInputCopyLedger(null, 100, 256, 192), null);
  assert.equal(
    copyLedgerProblem(
      { encoderInputCopyOperations: 200, encoderInputCopyBytes: 7_372_800 },
      ledger,
    ),
    null,
  );
  assert.equal(copyLedgerProblem({}, null), null);
  for (const performance of [
    { encoderInputCopyOperations: 202, encoderInputCopyBytes: 7_446_528 },
    { encoderInputCopyOperations: 199, encoderInputCopyBytes: 7_372_800 },
    { encoderInputCopyOperations: 0, encoderInputCopyBytes: 0 },
    {},
  ]) {
    assert.match(
      copyLedgerProblem(performance, ledger) ?? "",
      /expected 200 of 7372800 B$/,
      JSON.stringify(performance),
    );
  }
});

test("PSNR-Y is the luma's PSNR, and every sampled frame must hold the floor", async () => {
  const { PSNR_Y_FLOOR_DB, psnr, psnrFloorProblems } =
    await import("./lib/gpu-export-checks.mjs");
  const reference = Uint8Array.from({ length: 64 }, (_, index) => index * 3);
  assert.equal(psnr(reference, reference), Infinity);
  // Every sample one level off: an MSE of 1.
  const off = reference.map((value) => value + 1);
  assert.equal(
    psnr(off, reference).toFixed(4),
    (10 * Math.log10(255 * 255)).toFixed(4),
  );
  assert.throws(
    () => psnr(off.subarray(1), reference),
    /63 samples against 64/,
  );
  assert.throws(() => psnr(new Uint8Array(), new Uint8Array()), /0 samples/);

  assert.deepEqual(
    psnrFloorProblems([
      { frame: 0, psnr: PSNR_Y_FLOOR_DB },
      { frame: 33, psnr: Infinity },
    ]),
    [],
  );
  assert.deepEqual(
    psnrFloorProblems([
      { frame: 0, psnr: 43.8 },
      { frame: 33, psnr: 29.52 },
      { frame: 66, psnr: Number.NaN },
    ]),
    [
      `frame 33 is 29.52 dB PSNR-Y, below the ${PSNR_Y_FLOOR_DB} dB floor`,
      `frame 66 is NaN dB PSNR-Y, below the ${PSNR_Y_FLOOR_DB} dB floor`,
    ],
  );
  assert.deepEqual(psnrFloorProblems([]), ["no frame was sampled"]);
  assert.deepEqual(psnrFloorProblems([{ frame: 1, psnr: 30 }], 25), []);
});

test("a render-cache frame's luma is read from its header and planes, and a bad one is refused", async () => {
  const { createHash } = await import("node:crypto");
  const { deflateRawSync } = await import("node:zlib");
  const { renderCacheLuma } = await import("./lib/gpu-export-checks.mjs");
  // As render_cache.rs writes one: a 5x3 frame has 15 luma samples and two 3x2 chroma planes.
  const frame = ({ version = 3, planes, digestOf = planes }) => {
    const header = Buffer.alloc(64);
    header.write("A2RC", 0, "latin1");
    header.writeUInt32LE(version, 4);
    header.writeUInt32LE(5, 8);
    header.writeUInt32LE(3, 12);
    header.writeBigUInt64LE(27n, 24);
    createHash("sha256").update(digestOf).digest().copy(header, 32);
    return Buffer.concat([header, deflateRawSync(planes)]);
  };
  const planes = Buffer.from(Array.from({ length: 27 }, (_, index) => index));
  const luma = renderCacheLuma(frame({ planes }));
  assert.equal(luma.width, 5);
  assert.equal(luma.height, 3);
  assert.deepEqual([...luma.y], [...planes.subarray(0, 15)]);
  assert.throws(
    () => renderCacheLuma(frame({ planes, version: 4 })),
    /version 4; this harness reads version 3/,
  );
  assert.throws(
    () => renderCacheLuma(frame({ planes, digestOf: Buffer.alloc(27) })),
    /checksum/,
  );
  assert.throws(
    () => renderCacheLuma(frame({ planes: planes.subarray(0, 26) })),
    /of 26 B \(header 27 B\), expected 27 B/,
  );
  assert.throws(
    () => renderCacheLuma(Buffer.alloc(10)),
    /shorter than its header/,
  );
});

test("the unavailable checks hold on a host's refusal and fail on each way it can go wrong", async () => {
  const {
    gpuRequiredRefusalProblem,
    gpuWaitTimeoutMs,
    readbackFallbackProblem,
    startupProblem,
    unavailableCapabilityProblem,
  } = await import("./lib/gpu-export-checks.mjs");
  const reason = "Microsoft Basic Render Driver is a software adapter";

  assert.equal(
    unavailableCapabilityProblem({ available: false, reason }),
    null,
  );
  for (const gpu of [
    { available: true, reason: null },
    undefined,
    { available: false, reason: "" },
    { available: false, reason: "  " },
    { available: false },
  ]) {
    assert.notEqual(
      unavailableCapabilityProblem(gpu),
      null,
      JSON.stringify(gpu),
    );
  }

  const fallback = {
    status: 0,
    outputExists: true,
    result: {
      videoOutputPath: "readback",
      performance: { videoOutputPath: "readback" },
      gpuOutputPathUnavailable: reason,
      frames: 100,
    },
  };
  assert.equal(readbackFallbackProblem(fallback, 100), null);
  const result = (change) => ({ result: { ...fallback.result, ...change } });
  for (const [change, expected] of [
    [{ status: 1 }, /exited 1/],
    [{ outputExists: false }, /left no output/],
    [
      result({ videoOutputPath: "gpu-resident" }),
      /videoOutputPath is "gpu-resident"/,
    ],
    [result({ performance: {} }), /performance\.videoOutputPath/],
    [
      result({ gpuOutputPathUnavailable: null }),
      /gpuOutputPathUnavailable is null/,
    ],
    [result({ frames: 99 }), /wrote 99 frames, expected 100/],
  ]) {
    assert.match(
      readbackFallbackProblem({ ...fallback, ...change }, 100) ?? "",
      expected,
    );
  }

  const refusal = {
    ok: false,
    error: {
      code: "GPU_OUTPUT_UNAVAILABLE",
      message: `GPU-resident output is required but unavailable: ${reason}`,
    },
  };
  const failed = (code, message) => ({ ok: false, error: { code, message } });
  assert.equal(gpuRequiredRefusalProblem(refusal, []), null);
  for (const [answer, leftBehind, expected] of [
    [{ ok: true, result: {} }, [], /expected a refusal/],
    [null, [], /expected a refusal/],
    [
      failed("INVALID_INPUT", "x"),
      [],
      /"INVALID_INPUT", expected "GPU_OUTPUT_UNAVAILABLE"/,
    ],
    [
      failed(
        "GPU_OUTPUT_UNAVAILABLE",
        "GPU-resident output is required but unavailable: ",
      ),
      [],
      /did not say why/,
    ],
    [
      refusal,
      ["required.mp4", "required.alpha2-partial.mp4"],
      /left required\.mp4, required\.alpha2-partial\.mp4 behind/,
    ],
  ]) {
    assert.match(gpuRequiredRefusalProblem(answer, leftBehind) ?? "", expected);
  }

  const start = { baselineMs: 2_000, timeoutMs: 30_000, slackMs: 5_000 };
  assert.equal(startupProblem({ ...start, probedMs: 37_000 }), null);
  assert.match(
    startupProblem({ ...start, probedMs: 37_001 }) ?? "",
    /37001 ms to answer with the GPU encode probe and 2000 ms without it, over the 30000 ms probe bound plus 5000 ms/,
  );
  // The host's own reading of ALPHA2_GPU_TIMEOUT_MS: a positive whole number, else 30 s.
  for (const [value, expected] of [
    [undefined, 30_000],
    ["", 30_000],
    ["0", 30_000],
    ["-5", 30_000],
    ["1.5", 30_000],
    ["abc", 30_000],
    ["1500", 1_500],
  ]) {
    assert.equal(gpuWaitTimeoutMs(value), expected, String(value));
  }
});

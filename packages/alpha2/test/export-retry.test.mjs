import assert from "node:assert/strict";
import test from "node:test";

import {
  decideRetry,
  isRetryableFailure,
  RETRY_PROFILES,
} from "../dist/export-retry.js";
import { Alpha2Exporter, exportToSink } from "../dist/exporter.js";
import { classifyBrowserFailure } from "../dist/export-failure.js";

const [DEFAULT, REDUCED, MINIMAL] = RETRY_PROFILES;
const failed = (code, profile = DEFAULT, extra = {}, attempt = {}) => ({
  failure: {
    code,
    message: code,
    origin: "native-host",
    operation: "export",
    backend: "native",
    phase: "render",
    resource: "unknown",
    frameIndex: 12,
    ...extra,
  },
  profile,
  renderer: "gpu",
  videoEncoder: "default",
  ...attempt,
});

test("a failure a retry cannot help is final", () => {
  for (const code of [
    "CANCELLED",
    "INVALID_INPUT",
    "VALIDATION_FAILED",
    "RESOURCE_LIMIT",
    "STORAGE_QUOTA_EXCEEDED",
    "OUTPUT_IO_FAILED",
  ]) {
    const decision = decideRetry([failed(code)]);
    assert.equal(decision.retry, false, code);
    assert.match(decision.reason, new RegExp(code));
  }
  // A finished file that did not download is downloaded again, not rendered again.
  assert.equal(
    isRetryableFailure(
      failed("UNKNOWN", DEFAULT, { phase: "download" }).failure,
    ),
    false,
  );
});

test("resource busy retries after the hint", () => {
  const busy = (profile, retryAfterMs) =>
    failed("RESOURCE_BUSY", profile, {
      phase: "prepare",
      resource: "gpu",
      hostCode: "ENCODER_BUSY",
      frameIndex: null,
      ...(retryAfterMs === undefined ? {} : { retryAfterMs }),
    });
  const first = decideRetry([busy(DEFAULT, 2_000)]);
  assert.equal(first.retry, true);
  assert.equal(first.profile, DEFAULT, "a busy device keeps the profile");
  assert.equal(
    first.delayMs,
    2_000,
    "the hint is longer than the first backoff",
  );
  // Again at the same point: still the same profile, the backoff now longer than the hint.
  const second = decideRetry([busy(DEFAULT, 2_000), busy(DEFAULT, 500)]);
  assert.equal(second.retry, true);
  assert.equal(second.profile, DEFAULT);
  assert.equal(second.delayMs, 2_000);
  assert.equal(decideRetry([busy(DEFAULT)]).delayMs, 1_000);
  assert.equal(
    decideRetry([busy(DEFAULT), busy(DEFAULT), busy(DEFAULT)]).retry,
    false,
    "attempts still run out",
  );
  assert.equal(isRetryableFailure(busy(DEFAULT).failure), true);
});

test("memory exhaustion steps down, and stops when nothing is left to reduce", () => {
  const first = decideRetry([failed("OUT_OF_MEMORY")]);
  assert.equal(first.retry, true);
  assert.equal(first.profile, REDUCED);
  assert.equal(first.delayMs, 1_000);
  const second = decideRetry([
    failed("OUT_OF_MEMORY"),
    failed("ALLOCATION_FAILED", REDUCED),
  ]);
  assert.equal(second.profile, MINIMAL);
  assert.equal(second.delayMs, 2_000);
  const exhausted = decideRetry([failed("OUT_OF_MEMORY", MINIMAL)], {
    maxAttempts: 5,
  });
  assert.equal(exhausted.retry, false);
  assert.match(exhausted.reason, /nothing left to reduce/);
});

test("a transient failure retries as it was, and steps down only when it recurs at the same point", () => {
  const lost = decideRetry([failed("DEVICE_LOST")]);
  assert.equal(lost.retry, true);
  assert.equal(lost.profile, DEFAULT);
  const elsewhere = decideRetry([
    failed("HOST_EXITED"),
    failed("HOST_EXITED", DEFAULT, { frameIndex: 40 }),
  ]);
  assert.equal(elsewhere.profile, DEFAULT);
  const same = decideRetry([
    failed("READBACK_FAILED"),
    failed("READBACK_FAILED"),
  ]);
  assert.equal(same.profile, REDUCED);
  const stuck = decideRetry(
    [failed("READBACK_FAILED", MINIMAL), failed("READBACK_FAILED", MINIMAL)],
    { maxAttempts: 5 },
  );
  assert.equal(stuck.retry, false);
  assert.match(stuck.reason, /stopping early/);
});

test("device hung retries on the cpu when the caller has one", () => {
  const decision = decideRetry([failed("DEVICE_HUNG")], { cpuRenderer: true });
  assert.equal(decision.retry, true);
  assert.equal(decision.renderer, "cpu");
  assert.equal(decision.profile, DEFAULT, "the profile is not the problem");
  // Failing on the CPU too ends it.
  const again = decideRetry(
    [
      failed("DEVICE_HUNG"),
      failed("UNKNOWN", DEFAULT, {}, { renderer: "cpu" }),
    ],
    { cpuRenderer: true },
  );
  assert.equal(
    again.retry,
    true,
    "an unrelated CPU failure retries on the CPU",
  );
  assert.equal(again.renderer, "cpu");
});

test("device hung stops without a cpu renderer", () => {
  const decision = decideRetry([failed("DEVICE_HUNG")]);
  assert.equal(decision.retry, false);
  assert.equal(decision.failure.code, "DEVICE_HUNG");
  assert.match(decision.reason, /no CPU renderer/);
  assert.equal(isRetryableFailure(failed("DEVICE_HUNG").failure), false);
});

test("a loss twice at the same frame becomes device hung", () => {
  for (const [first, second] of [
    ["DEVICE_LOST", "DEVICE_LOST"],
    ["GPU_TIMEOUT", "DEVICE_LOST"],
    ["DEVICE_LOST", "GPU_TIMEOUT"],
  ]) {
    const stopped = decideRetry([failed(first), failed(second)]);
    assert.equal(stopped.retry, false, `${first} then ${second}`);
    assert.equal(stopped.failure.code, "DEVICE_HUNG");
    assert.equal(stopped.failure.frameIndex, 12);
    const onCpu = decideRetry([failed(first), failed(second)], {
      cpuRenderer: true,
    });
    assert.equal(onCpu.renderer, "cpu");
  }
  // At another frame it is a second transient loss: another GPU attempt.
  const elsewhere = decideRetry([
    failed("DEVICE_LOST"),
    failed("DEVICE_LOST", DEFAULT, { frameIndex: 30 }),
  ]);
  assert.equal(elsewhere.retry, true);
  assert.equal(elsewhere.renderer, "gpu");
});

test("a browser loss after two page losses goes to the cpu", () => {
  const browserLoss = failed("DEVICE_LOST", DEFAULT, { origin: "browser" });
  const withCpu = decideRetry([browserLoss], {
    pageGpuLosses: 2,
    cpuRenderer: true,
  });
  assert.equal(withCpu.retry, true);
  assert.equal(withCpu.renderer, "cpu");
  const withoutCpu = decideRetry([browserLoss], { pageGpuLosses: 2 });
  assert.equal(withoutCpu.retry, false);
  assert.match(withoutCpu.reason, /withdraws GPU access/);
  // One earlier reset still leaves the GPU.
  assert.equal(
    decideRetry([browserLoss], { pageGpuLosses: 1, cpuRenderer: true })
      .renderer,
    "gpu",
  );
});

test("an encode failure retries with the software encoder", () => {
  const decision = decideRetry([
    failed("UNKNOWN", DEFAULT, { phase: "encode", hostCode: "ENCODE_ERROR" }),
  ]);
  assert.equal(decision.retry, true);
  assert.equal(decision.videoEncoder, "software");
  assert.equal(decision.renderer, "gpu");
  // Already on the software encoder: retried as any other failure.
  const again = decideRetry([
    failed("UNKNOWN", DEFAULT, { phase: "encode" }),
    failed(
      "UNKNOWN",
      DEFAULT,
      { phase: "encode" },
      { videoEncoder: "software" },
    ),
  ]);
  assert.equal(again.videoEncoder, "software");
  assert.equal(again.profile, REDUCED);
});

test("attempts are bounded, and the backoff is the render server's", () => {
  const history = [
    failed("DEVICE_LOST"),
    failed("UNKNOWN", DEFAULT, { frameIndex: 3 }),
  ];
  assert.equal(decideRetry(history).delayMs, 2_000);
  const all = decideRetry([...history, failed("DEVICE_LOST")]);
  assert.equal(all.retry, false);
  assert.match(all.reason, /all 3 attempts failed/);
  assert.equal(
    decideRetry([failed("DEVICE_LOST")], { backoffMs: () => 0 }).delayMs,
    0,
  );
});

/** The failure `exportToSink` ends with when `refill` does what `refill` here does. */
async function exportFailure(refill) {
  const backend = {
    begin: async () => ({
      kind: "waiting",
      missingSources: ["a"],
      missingFrames: [{ clipId: "c", sourceId: "a", mediaTimeTicks: 0n }],
    }),
    isDeviceLost: () => false,
    abort() {},
  };
  const exporter = new Alpha2Exporter(
    backend,
    { num: 30n, den: 1n },
    0n,
    12_000n,
  );
  const sink = { async writeVideo() {}, async finish() {}, abort() {} };
  try {
    await exportToSink(exporter, sink, refill);
  } catch (error) {
    return classifyBrowserFailure(error, {
      phase: "render",
      backend: "browser-webgpu",
    });
  }
  throw new Error("the export did not fail");
}

test("an export whose refills do not fill a frame is not retried: another attempt refills alike", async () => {
  // Classed UNKNOWN, it would be retried (the second time with less in flight, which the refill
  // does not read) only to fail the same way.
  const failure = await exportFailure(async () => {});
  assert.equal(failure.code, "RESOURCE_LIMIT");
  assert.equal(failure.phase, "upload");
  assert.equal(failure.stage, "sources-not-held");
  assert.deepEqual(
    decideRetry([
      { failure, profile: DEFAULT, renderer: "gpu", videoEncoder: "default" },
    ]),
    {
      retry: false,
      reason: "RESOURCE_LIMIT at upload is not helped by a retry",
      failure,
    },
  );
});

test("an export whose frame outgrows the upload budget is not retried", async () => {
  // As the binding refuses the upload that would overflow the budget, which `refill` rejects with.
  const failure = await exportFailure(async () => {
    throw Object.assign(new Error("over the uploaded-frame budget"), {
      name: "Alpha2Error",
      code: "RESOURCE_LIMIT",
      fatal: false,
      failure: {
        code: "RESOURCE_LIMIT",
        phase: "upload",
        stage: "working-set-over-budget",
        resource: "unknown",
        requestedBytes: 1_115_258_880,
      },
    });
  });
  assert.equal(failure.code, "RESOURCE_LIMIT");
  assert.equal(failure.stage, "working-set-over-budget");
  assert.equal(failure.requestedBytes, 1_115_258_880);
  assert.equal(
    decideRetry([
      { failure, profile: DEFAULT, renderer: "gpu", videoEncoder: "default" },
    ]).retry,
    false,
  );
});

test("planar decode unavailable is not retried", () => {
  const refusal = Object.assign(
    new Error(
      "clip.mov is HDR video (HEVC, 10-bit, HLG): this browser decodes it only to frames",
    ),
    {
      name: "PlanarDecodeUnavailableError",
      code: "PLANAR_DECODE_UNAVAILABLE",
      failure: {
        code: "INVALID_INPUT",
        phase: "decode",
        stage: "planar-decode-unavailable",
      },
    },
  );
  const failure = classifyBrowserFailure(refusal, {
    phase: "render",
    backend: "browser-webgpu",
  });
  assert.equal(failure.code, "INVALID_INPUT");
  assert.equal(failure.stage, "planar-decode-unavailable");
  assert.equal(failure.phase, "decode");
  const decision = decideRetry([
    { failure, profile: DEFAULT, renderer: "gpu", videoEncoder: "default" },
  ]);
  assert.equal(decision.retry, false);
  assert.equal(isRetryableFailure(failure), false);
});

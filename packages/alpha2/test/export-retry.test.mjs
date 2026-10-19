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
const failed = (code, profile = DEFAULT, extra = {}) => ({
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
    [failed("GPU_TIMEOUT", MINIMAL), failed("GPU_TIMEOUT", MINIMAL)],
    { maxAttempts: 5 },
  );
  assert.equal(stuck.retry, false);
  assert.match(stuck.reason, /stopping early/);
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
  assert.deepEqual(decideRetry([{ failure, profile: DEFAULT }]), {
    retry: false,
    reason: "RESOURCE_LIMIT at upload is not helped by a retry",
  });
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
  assert.equal(decideRetry([{ failure, profile: DEFAULT }]).retry, false);
});

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  classifyBrowserFailure,
  ExportFailureError,
  failureClassOfCode,
  failureForCode,
  failureOf,
  withSecondaryFailure,
} from "../dist/export-failure.js";

const at = (phase, extra = {}) => ({
  phase,
  backend: "browser-webgpu",
  ...extra,
});

test("V8's allocation failure is a JavaScript allocation, not a GPU one", () => {
  const failure = classifyBrowserFailure(
    new RangeError("Array buffer allocation failed"),
    at("encode", { frameIndex: 41, requestedBytes: 3_110_400 }),
  );
  assert.equal(failure.code, "ALLOCATION_FAILED");
  assert.equal(failure.resource, "js");
  assert.equal(failure.phase, "encode");
  assert.equal(failure.frameIndex, 41);
  assert.equal(failure.requestedBytes, 3_110_400);
  // A refused length is not an allocation failure.
  assert.equal(
    classifyBrowserFailure(new RangeError("Invalid array length"), at("encode"))
      .code,
    "UNKNOWN",
  );
});

test("JavaScriptCore's and SpiderMonkey's allocation failures are JavaScript allocations too", () => {
  for (const error of [
    // JavaScriptCore, for an ArrayBuffer or typed array it could not allocate.
    new RangeError("Out of memory"),
    // Relayed from a worker, which keeps the name and not the type.
    Object.assign(new Error("Out of memory"), { name: "RangeError" }),
    // SpiderMonkey throws the string itself.
    "out of memory",
  ]) {
    const failure = classifyBrowserFailure(error, at("encode"));
    assert.equal(failure.code, "ALLOCATION_FAILED", String(error));
    assert.equal(failure.resource, "js", String(error));
  }
  // The same words in another form, or a refused length, are not that evidence.
  for (const error of [
    new Error("Out of memory"),
    new RangeError("out of memory while parsing"),
    "Out of memory",
    new RangeError("Invalid typed array length: 4294967296"),
    new RangeError("length too large"),
  ])
    assert.equal(
      classifyBrowserFailure(error, at("encode")).code,
      "UNKNOWN",
      String(error),
    );
});

test("an engine readback error keeps its class, stage, and frame", () => {
  const error = Object.assign(
    new Error("export frame readback failed to map: map-failed"),
    {
      code: "ALPHA2_ERROR",
      failure: {
        code: "READBACK_FAILED",
        phase: "readback",
        stage: "map-failed",
        resource: "unknown",
        frameIndex: 7,
        timeTicks: "28000",
        gpu: { events: [], deviceLost: false, evidence: null },
      },
    },
  );
  const failure = classifyBrowserFailure(
    error,
    at("render", { frameIndex: 9 }),
  );
  assert.equal(failure.code, "READBACK_FAILED");
  assert.equal(failure.resource, "unknown");
  assert.equal(failure.phase, "readback");
  assert.equal(failure.stage, "map-failed");
  assert.equal(failure.frameIndex, 7);
  assert.equal(failure.timeTicks, "28000");
  assert.deepEqual(failure.diagnostics.gpu.events, []);
  assert.equal(failure.origin, "browser");
});

test("an upload the export frame's working set does not fit keeps its refusal and the bytes it needs", () => {
  // As the binding refuses it: an Alpha2Error coded RESOURCE_LIMIT with a partial failure.
  const error = Object.assign(
    new Error(
      "the export frame needs at least 36864 bytes of uploaded sources at once",
    ),
    {
      name: "Alpha2Error",
      code: "RESOURCE_LIMIT",
      fatal: false,
      failure: {
        code: "RESOURCE_LIMIT",
        phase: "upload",
        stage: "working-set-over-budget",
        resource: "unknown",
        requestedBytes: 36_864,
      },
    },
  );
  const failure = classifyBrowserFailure(
    error,
    at("decode", { frameIndex: 3, requestedBytes: 1 }),
  );
  assert.equal(failure.code, "RESOURCE_LIMIT");
  assert.equal(failure.phase, "upload");
  assert.equal(failure.stage, "working-set-over-budget");
  assert.equal(failure.requestedBytes, 36_864);
  assert.equal(failure.frameIndex, 3);
});

test("a quota error is storage only where the export writes", () => {
  const quota = new DOMException("quota", "QuotaExceededError");
  assert.equal(
    classifyBrowserFailure(quota, at("publish")).code,
    "STORAGE_QUOTA_EXCEEDED",
  );
  // WebCodecs reports a codec reclaimed under memory pressure the same way.
  assert.equal(
    classifyBrowserFailure(quota, at("encode")).code,
    "ALLOCATION_FAILED",
  );
});

test("cancellation and device loss classify; anything else stays unknown", () => {
  assert.equal(
    classifyBrowserFailure(new DOMException("x", "AbortError"), at("render"))
      .code,
    "CANCELLED",
  );
  assert.equal(
    classifyBrowserFailure(
      Object.assign(new Error("GPU device was lost"), {
        code: "GPU_DEVICE_LOST",
      }),
      at("render"),
    ).code,
    "DEVICE_LOST",
  );
  assert.equal(
    classifyBrowserFailure(new Error("out of memory?"), at("render")).code,
    "UNKNOWN",
  );
});

test("an engine code the request would meet again is invalid input, as the native host classifies it", () => {
  // The engine's codes are one table for the host and the browser; the host's failure classes
  // INVALID_INPUT for these four.
  for (const code of [
    "INVALID_INPUT",
    "MISSING_INPUT",
    "EFFECT_ERROR",
    "INCOMPLETE_COMPOSITION",
  ]) {
    assert.equal(
      classifyBrowserFailure(
        Object.assign(new Error("refused"), { code }),
        at("prepare"),
      ).code,
      "INVALID_INPUT",
      code,
    );
  }
  for (const code of ["ALPHA2_ERROR", "ENCODE_ERROR"])
    assert.equal(
      classifyBrowserFailure(
        Object.assign(new Error("failed"), { code }),
        at("prepare"),
      ).code,
      "UNKNOWN",
      code,
    );
});

test("a classified failure passes through, and later events are secondary", () => {
  const first = classifyBrowserFailure(new Error("boom"), at("mux"));
  const error = new ExportFailureError(first);
  assert.equal(error.code, "UNKNOWN");
  assert.equal(failureOf(error), first);
  assert.equal(classifyBrowserFailure(error, at("cleanup")), first);
  const both = withSecondaryFailure(first, {
    code: "UNKNOWN",
    phase: "cleanup",
    message: "abort threw",
  });
  assert.equal(both.phase, "mux");
  assert.deepEqual(
    both.secondary.map((note) => note.phase),
    ["cleanup"],
  );
});

test("a code classifies as the shared table says, the host's own table included (S13f D6)", async () => {
  const table = JSON.parse(
    await readFile(
      new URL("../../../tests/cases/failure-classes.json", import.meta.url),
      "utf8",
    ),
  );
  const entries = [
    ...Object.entries(table.protocol),
    ...Object.entries(table.node),
  ];
  assert.ok(entries.length > 25);
  for (const [code, expected] of entries) {
    assert.deepEqual({ ...failureClassOfCode(code) }, expected, code);
    const failure = failureForCode(code, {
      message: `${code} happened`,
      origin: "node",
      phase: "prepare",
      backend: "native",
      stderr: "tail",
    });
    assert.equal(failure.code, expected.code, code);
    assert.equal(failure.resource, expected.resource, code);
    assert.equal(failure.hostCode, code);
    assert.equal(failure.operation, "export");
    assert.equal(failure.stderr, "tail");
    assert.equal(failureOf({ failure })?.code, expected.code, code);
  }
  assert.equal(failureClassOfCode("SOMETHING_NEW").code, "UNKNOWN");
  assert.equal(failureClassOfCode(undefined).code, "UNKNOWN");
  assert.equal(failureClassOfCode("toString").code, "UNKNOWN");
});

test("a browser error with a protocol code classifies by the same table (S13f D6)", () => {
  for (const [code, expected] of [
    ["RESOURCE_LIMIT", "RESOURCE_LIMIT"],
    ["GPU_TIMEOUT", "GPU_TIMEOUT"],
    ["GPU_DEVICE_LOST", "DEVICE_LOST"],
    ["INCOMPLETE_COMPOSITION", "INVALID_INPUT"],
    ["CANCELLED", "CANCELLED"],
    ["ALPHA2_ERROR", "UNKNOWN"],
  ]) {
    const error = Object.assign(new Error(code), { code });
    assert.equal(
      classifyBrowserFailure(error, { phase: "render" }).code,
      expected,
      code,
    );
  }
  const own = Object.assign(new Error("empty"), {
    code: "empty_export",
    failureCode: "INVALID_INPUT",
  });
  assert.equal(
    classifyBrowserFailure(own, { phase: "prepare" }).code,
    "INVALID_INPUT",
  );
});

import assert from "node:assert/strict";
import test from "node:test";

import {
  intakeVideoFrame,
  readbackCounters,
  SoftwareDecodeRequiredError,
  softwareDecodeRequest,
} from "../dist/frame-intake.js";
import {
  NonPlanarVideoFrameError,
  PlanarCopyPool,
} from "../dist/hdr-frames.js";
import { Alpha2WasmError, serializeError } from "../dist/worker-error.js";

/** A CPU-backed NV12 frame whose first `failures` reads fail as a browser's GPU-backed ones can. */
function frame({ failures = 0, format = "NV12", transfer = "bt709" } = {}) {
  let failed = 0;
  const value = {
    format,
    codedWidth: 4,
    codedHeight: 2,
    displayWidth: 4,
    displayHeight: 2,
    timestamp: 0,
    duration: 40_000,
    visibleRect: { x: 0, y: 0, width: 4, height: 2 },
    colorSpace: {
      primaries: "bt709",
      transfer,
      matrix: "bt709",
      fullRange: false,
    },
    closed: false,
    allocationSize: () => 12,
    async copyTo(buffer) {
      if (failed < failures) {
        failed += 1;
        throw new DOMException(
          "Failed to read VideoFrame data.",
          "InvalidStateError",
        );
      }
      buffer.fill(7);
      return [
        { offset: 0, stride: 4 },
        { offset: 8, stride: 4 },
      ];
    },
    close() {
      value.closed = true;
    },
  };
  return value;
}

const options = (overrides = {}) => ({
  required: false,
  declared: null,
  identity: {},
  pool: new PlanarCopyPool(),
  counters: readbackCounters(),
  unreadable: "redecode",
  takesPresented: true,
  ...overrides,
});

test("a transient read failure is read again", async () => {
  const counters = readbackCounters();
  const source = frame({ failures: 2 });
  const intake = await intakeVideoFrame(source, options({ counters }));
  assert.equal(intake.kind, "planar");
  assert.equal(intake.planar.format, "NV12");
  assert.equal(counters.readbackRetries, 2);
  assert.equal(counters.unreadableFrames, 0);
  assert.equal(source.closed, false, "the caller owns the frame");
});

test("an unreadable sdr frame is presented when allowed", async () => {
  const counters = readbackCounters();
  const intake = await intakeVideoFrame(
    frame({ failures: Infinity }),
    options({ counters, unreadable: "present" }),
  );
  assert.deepEqual(intake, { kind: "present" });
  assert.deepEqual(counters, {
    unreadableFrames: 1,
    readbackRetries: 2,
    redecodedFrames: 0,
    presentedFrames: 1,
  });
});

test("an unreadable frame the engine must convert is decoded again, never presented", async () => {
  const intake = await intakeVideoFrame(
    frame({ failures: Infinity, transfer: "hlg" }),
    options({ required: true, unreadable: "present" }),
  );
  assert.equal(intake.kind, "redecode");
  assert.equal(intake.cause.name, "InvalidStateError");
});

test("a frame with no planar format is decoded again at once, without reading it again", async () => {
  const counters = readbackCounters();
  const intake = await intakeVideoFrame(
    frame({ format: null }),
    options({ counters, unreadable: "present", required: true }),
  );
  assert.equal(intake.kind, "redecode");
  assert.ok(intake.cause instanceof NonPlanarVideoFrameError);
  assert.equal(counters.readbackRetries, 0);
  assert.equal(counters.unreadableFrames, 1);
});

test("a copy that fails otherwise rejects with its error", async () => {
  const source = frame();
  source.copyTo = async () => {
    throw new TypeError("bad layout");
  };
  await assert.rejects(intakeVideoFrame(source, options()), TypeError);
});

test("a software decode request survives a worker boundary", () => {
  const error = new SoftwareDecodeRequiredError({
    sourceId: "clip",
    mediaTimeTicks: "4000",
    reason: "non-planar",
  });
  const request = {
    sourceId: "clip",
    mediaTimeTicks: "4000",
    reason: "non-planar",
  };
  assert.deepEqual(softwareDecodeRequest(error), request);
  const relayed = new Alpha2WasmError(serializeError(error));
  assert.equal(relayed.code, "SOFTWARE_DECODE_REQUIRED");
  assert.equal(relayed.fatal, false);
  assert.deepEqual(softwareDecodeRequest(relayed), request);
  assert.equal(softwareDecodeRequest(new Error("other")), undefined);
});

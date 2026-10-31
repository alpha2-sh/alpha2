import assert from "node:assert/strict";
import test from "node:test";

import { Alpha2Exporter, exportToSink } from "../dist/exporter.js";

/** A frame that records whether it was closed. */
const frame = () => ({
  closed: false,
  close() {
    this.closed = true;
  },
  codedWidth: 16,
  codedHeight: 16,
  format: "I420",
  allocationSize: () => 384,
});

/** A backend whose frames render at once and whose readbacks settle when the test says. */
function backend() {
  const readbacks = [];
  return {
    readbacks,
    begin: async () => {
      let settle;
      const readback = new Promise((resolve) => (settle = resolve));
      readbacks.push({ settle, frame: frame() });
      return { kind: "rendered", retainedBytes: 1_024, readback };
    },
    isDeviceLost: () => false,
    abort() {},
  };
}

function sink() {
  const written = [];
  return {
    written,
    aborted: [],
    async writeVideo(output) {
      written.push(output);
      output.frame.close();
    },
    async finish() {},
    abort(reason) {
      this.aborted.push(reason);
    },
  };
}

test("abort while a readback never settles rejects cancelled at once", async () => {
  const host = backend();
  const out = sink();
  const exporter = new Alpha2Exporter(host, { num: 30n, den: 1n }, 0n, 12_000n);
  const cancel = new AbortController();
  const done = exportToSink(exporter, out, async () => {}, {
    signal: cancel.signal,
    maxInFlightFrames: 2,
  });
  // Let the export submit its frames and start waiting on the first readback, which never maps.
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.ok(host.readbacks.length >= 1);
  const started = performance.now();
  cancel.abort();
  await assert.rejects(done, (error) => {
    assert.equal(error.name, "ExportAbortedError");
    assert.equal(error.code, "cancelled");
    return true;
  });
  assert.ok(
    performance.now() - started < 1_000,
    "the cancel did not wait for the GPU",
  );
  assert.equal(out.aborted.length, 1);
});

test("a late readback frame is closed", async () => {
  const host = backend();
  const exporter = new Alpha2Exporter(host, { num: 30n, den: 1n }, 0n, 12_000n);
  const cancel = new AbortController();
  const done = exportToSink(exporter, sink(), async () => {}, {
    signal: cancel.signal,
    maxInFlightFrames: 2,
  });
  await new Promise((resolve) => setTimeout(resolve, 10));
  cancel.abort();
  await assert.rejects(done);
  // The GPU answers after the export gave up: every frame it hands over is closed.
  for (const { settle, frame } of host.readbacks) settle(frame);
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.ok(host.readbacks.length >= 1);
  assert.ok(host.readbacks.every(({ frame }) => frame.closed));
});

test("an export asks for a frame sent back for a software decode again in software, once", async () => {
  let held = false;
  const host = {
    begin: async () =>
      held
        ? {
            kind: "rendered",
            retainedBytes: 1_024,
            readback: Promise.resolve(frame()),
          }
        : {
            kind: "waiting",
            missingSources: ["hdr"],
            missingFrames: [
              { clipId: "clip", sourceId: "hdr", mediaTimeTicks: 0n },
            ],
          },
    isDeviceLost: () => false,
    abort() {},
  };
  const asked = [];
  const exporter = new Alpha2Exporter(host, { num: 30n, den: 1n }, 0n, 4_000n);
  const result = await exportToSink(exporter, sink(), async (waiting) => {
    const [missing] = waiting.missingFrames;
    asked.push(missing.decode ?? "default");
    if (missing.decode !== "software")
      throw Object.assign(new Error("decode it again in software"), {
        code: "SOFTWARE_DECODE_REQUIRED",
        failure: {
          code: "UNKNOWN",
          diagnostics: {
            sourceId: "hdr",
            mediaTimeTicks: "0",
            reason: "non-planar",
          },
        },
      });
    held = true;
  });
  assert.equal(result.frames, 1);
  assert.deepEqual(asked, ["default", "software"]);
});

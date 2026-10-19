import test from "node:test";
import assert from "node:assert/strict";

import { TICKS_PER_SECOND } from "../dist/project.js";
import {
  ExportFailureError,
  classifyBrowserFailure,
} from "../dist/export-failure.js";
import { Alpha2Exporter, exportToSink } from "../dist/exporter.js";

const RATES = [
  { label: "29.97", num: 30000n, den: 1001n },
  { label: "23.976", num: 24000n, den: 1001n },
  { label: "59.94", num: 60000n, den: 1001n },
  { label: "30", num: 30n, den: 1n },
  { label: "25", num: 25n, den: 1n },
];

test("the exporter hands the sink frames one exact interval apart at every rate", async () => {
  // A muxer keeps a uniform sample duration only if the timestamps are; the sink's own track is
  // pinned in mp4-sink.test.mjs.
  for (const { label, num, den } of RATES) {
    const interval = (BigInt(TICKS_PER_SECOND) * den) / num;
    const frames = 2_000n;
    const exporter = new Alpha2Exporter(
      {
        begin: async () => ({
          kind: "rendered",
          retainedBytes: 8,
          readback: Promise.resolve({ close() {} }),
        }),
        isDeviceLost: () => false,
        abort: () => {},
      },
      { num, den },
      0n,
      frames * interval,
    );
    const times = [];
    await exportToSink(
      exporter,
      {
        writeVideo: async ({ timeTicks, frame }) => {
          times.push(timeTicks);
          frame.close();
        },
        finish: async () => {},
        abort: () => {},
      },
      async () => {},
    );
    assert.equal(BigInt(times.length), frames, label);
    const deltas = new Set(
      times.slice(1).map((time, index) => time - times[index]),
    );
    assert.deepEqual(
      [...deltas],
      [interval],
      `${label} has one frame interval`,
    );
    assert.equal(times[0], 0n, `${label} starts at the output's zero`);
  }
});

test("the TypeScript exporter includes a frame intersecting an unaligned range", async () => {
  const requested = [];
  const backend = {
    begin: async (timeTicks) => {
      requested.push(timeTicks);
      return {
        kind: "waiting",
        missingSources: ["source"],
        missingFrames: [
          { clipId: "clip", sourceId: "source", mediaTimeTicks: 9_000n },
        ],
      };
    },
    isDeviceLost: () => false,
    abort: () => {},
  };
  const exporter = new Alpha2Exporter(
    backend,
    { num: 30n, den: 1n },
    2_000n,
    4_000n,
  );
  assert.deepEqual(exporter.progress, { completed: 0n, total: 1n });
  // The range widens to the frame grid; audio for this export starts at 0.
  assert.deepEqual(exporter.effectiveRange, {
    startTicks: 0n,
    endTicks: 4_000n,
  });
  assert.deepEqual(await exporter.next(), {
    kind: "waiting",
    index: 0n,
    timelineTicks: 0n,
    missingSources: ["source"],
    missingFrames: [
      { clipId: "clip", sourceId: "source", mediaTimeTicks: 9_000n },
    ],
  });
  assert.deepEqual(requested, [0n]);
});

test("concurrent next calls reserve distinct timeline frames", async () => {
  const requested = [];
  let releaseFirstStart;
  const firstStart = new Promise((resolve) => {
    releaseFirstStart = resolve;
  });
  const backend = {
    begin: async (timeTicks) => {
      requested.push(timeTicks);
      if (requested.length === 1) await firstStart;
      return {
        kind: "rendered",
        retainedBytes: 10,
        readback: Promise.resolve({ close() {} }),
      };
    },
    isDeviceLost: () => false,
    abort: () => {},
  };
  const exporter = new Alpha2Exporter(
    backend,
    { num: 30n, den: 1n },
    0n,
    8_000n,
  );
  const first = exporter.next();
  const second = exporter.next();
  await new Promise(setImmediate);
  assert.deepEqual(requested, [0n]);
  releaseFirstStart();
  const firstTicket = await first;
  const secondTicket = await second;
  assert.equal(firstTicket.kind, "rendered");
  assert.equal(secondTicket.kind, "rendered");
  assert.deepEqual(requested, [0n, 4_000n]);
  assert.equal((await firstTicket.output).index, 0n);
  assert.equal((await secondTicket.output).index, 1n);
});

test("browser export overlaps bounded rendering with ordered encoding", async () => {
  const requested = [];
  const written = [];
  const readbacks = [];
  let releaseFirst;
  const firstWrite = new Promise((resolve) => {
    releaseFirst = resolve;
  });
  const backend = {
    begin: async (timeTicks) => {
      requested.push(timeTicks);
      let resolveReadback;
      const readback = new Promise((resolve) => {
        resolveReadback = resolve;
      });
      readbacks.push(() => resolveReadback({ close() {} }));
      return {
        kind: "rendered",
        retainedBytes: 10,
        readback,
      };
    },
    isDeviceLost: () => false,
    abort: () => {},
  };
  const exporter = new Alpha2Exporter(
    backend,
    { num: 30n, den: 1n },
    0n,
    16_000n,
  );
  let finished = false;
  const sink = {
    writeVideo: async ({ index }) => {
      written.push(index);
      if (index === 0n) await firstWrite;
    },
    finish: async () => {
      finished = true;
    },
    abort: async () => {},
  };

  const running = exportToSink(exporter, sink, async () => {});
  while (requested.length < 3) await new Promise(setImmediate);
  assert.deepEqual(requested, [0n, 4_000n, 8_000n]);
  assert.equal(readbacks.length, 3);
  assert.deepEqual(exporter.progress, { completed: 0n, total: 4n });
  assert.deepEqual(written, [], "mapping remains independent from submission");
  readbacks[1]();
  readbacks[2]();
  await new Promise(setImmediate);
  assert.deepEqual(written, [], "completed frames remain ordered");
  readbacks[0]();
  while (written.length < 1) await new Promise(setImmediate);
  assert.deepEqual(written, [0n], "sink calls stay serial");
  assert.equal(finished, false);
  releaseFirst();
  while (requested.length < 4) await new Promise(setImmediate);
  readbacks[3]();
  const result = await running;
  assert.deepEqual(written, [0n, 1n, 2n, 3n]);
  assert.equal(finished, true);
  assert.equal(result.frames, 4);
  assert.ok(result.renderMs >= 0);
  assert.ok(result.fps > 0);
  assert.equal(result.performance.pipelineEnabled, true);
  assert.equal(result.performance.inFlightDepth, 3);
  assert.equal(result.performance.inFlightByteBudget, 128 * 1024 * 1024);
  assert.equal(result.performance.stagingBytesPerFrame, 10);
  assert.equal(result.performance.peakReadbackBytes, 30);
  assert.equal(
    result.performance.outputReadbackBytes,
    40,
    "every frame's mapped staging bytes, once",
  );
  assert.equal(result.performance.peakEncodeQueueBytes, 30);
  assert.equal(result.performance.maxInFlightReadbacks, 3);
  assert.equal(result.performance.maxEncodeQueueEvents, 3);
  assert.ok(result.performance.encodeBackpressureCount > 0);
  assert.ok(result.performance.readbackMapLatencyMs >= 0);
  assert.ok(result.performance.videoEncodeMs >= 0);
  assert.ok(result.performance.muxFinishMs >= 0);
});

test("browser export bounds readback and encoder ownership by bytes", async () => {
  const requested = [];
  const written = [];
  const readbacks = [];
  let releaseFirst;
  const firstWrite = new Promise((resolve) => {
    releaseFirst = resolve;
  });
  const backend = {
    begin: async (timeTicks) => {
      requested.push(timeTicks);
      let resolveReadback;
      const readback = new Promise((resolve) => {
        resolveReadback = resolve;
      });
      readbacks.push(() => resolveReadback({ close() {} }));
      return { kind: "rendered", retainedBytes: 10, readback };
    },
    isDeviceLost: () => false,
    abort: () => {},
  };
  const exporter = new Alpha2Exporter(
    backend,
    { num: 30n, den: 1n },
    0n,
    12_000n,
  );
  const sink = {
    writeVideo: async ({ index }) => {
      written.push(index);
      if (index === 0n) await firstWrite;
    },
    finish: async () => {},
    abort: async () => {},
  };

  const running = exportToSink(exporter, sink, async () => {}, {
    maxInFlightFrames: 3,
    maxInFlightBytes: 20,
  });
  while (requested.length < 2) await new Promise(setImmediate);
  await new Promise(setImmediate);
  assert.deepEqual(requested, [0n, 4_000n]);
  readbacks[0]();
  readbacks[1]();
  while (written.length < 1) await new Promise(setImmediate);
  await new Promise(setImmediate);
  assert.deepEqual(
    requested,
    [0n, 4_000n],
    "a queued encoder frame keeps its byte reservation",
  );
  releaseFirst();
  while (requested.length < 3) await new Promise(setImmediate);
  readbacks[2]();
  await running;
  assert.deepEqual(written, [0n, 1n, 2n]);
});

test("a budget smaller than one frame is refused, not ignored for a first frame", async () => {
  let closed = 0;
  let aborted;
  const backend = {
    begin: async () => ({
      kind: "rendered",
      retainedBytes: 64,
      readback: Promise.resolve({
        close() {
          closed += 1;
        },
      }),
    }),
    isDeviceLost: () => false,
    abort: () => {},
  };
  const exporter = new Alpha2Exporter(
    backend,
    { num: 30n, den: 1n },
    0n,
    12_000n,
  );
  const written = [];
  const sink = {
    writeVideo: async ({ index }) => written.push(index),
    finish: async () => {},
    abort: async (reason) => {
      aborted = reason;
    },
  };
  await assert.rejects(
    exportToSink(exporter, sink, async () => {}, { maxInFlightBytes: 32 }),
    (error) => {
      assert.equal(error.failure?.code, "RESOURCE_LIMIT");
      assert.equal(error.failure?.requestedBytes, 64);
      return true;
    },
  );
  await new Promise(setImmediate);
  assert.deepEqual(written, []);
  assert.equal(closed, 1, "the refused frame is closed");
  assert.equal(aborted?.failure?.code, "RESOURCE_LIMIT");
});

test("a sink whose abort fails does not replace the export's failure", async () => {
  const backend = {
    begin: async () => ({
      kind: "rendered",
      retainedBytes: 64,
      readback: Promise.resolve({ close() {} }),
    }),
    isDeviceLost: () => false,
    abort: () => {},
  };
  const exporter = new Alpha2Exporter(
    backend,
    { num: 30n, den: 1n },
    0n,
    12_000n,
  );
  const quota = new ExportFailureError({
    code: "STORAGE_QUOTA_EXCEEDED",
    message: "The export file could not be written: the disk is full",
    origin: "browser",
    operation: "export",
    backend: "unknown",
    phase: "publish",
    resource: "unknown",
  });
  const sink = {
    writeVideo: async () => {
      throw quota;
    },
    finish: async () => {},
    abort: async () => {
      throw new TypeError("Invalid state: WritableStream is closed");
    },
  };
  await assert.rejects(
    exportToSink(exporter, sink, async () => {}),
    (error) => {
      assert.equal(error, quota);
      assert.equal(error.code, "STORAGE_QUOTA_EXCEEDED");
      assert.deepEqual(error.failure.secondary, [
        {
          code: "UNKNOWN",
          phase: "cleanup",
          message: "Invalid state: WritableStream is closed",
        },
      ]);
      return true;
    },
  );
});

/**
 * Runs `start(signal)`, cancelling it once `ms` pass, so an export that never ends fails its test
 * as a cancel instead of running on.
 */
async function within(start, ms = 2_000) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), ms);
  try {
    return await start(controller.signal);
  } finally {
    clearTimeout(timer);
  }
}

test("a frame the refills never fill fails the export instead of retrying for ever", async () => {
  // A frame whose sources together exceed the upload budget: each refill's upload drops another of
  // them, so the engine always reports one missing.
  let refills = 0;
  let aborted;
  const backend = {
    begin: async (timeTicks) =>
      timeTicks === 0n
        ? {
            kind: "rendered",
            retainedBytes: 8,
            readback: Promise.resolve({ close() {} }),
          }
        : {
            kind: "waiting",
            missingSources: [refills % 2 === 0 ? "a" : "b"],
            missingFrames: [
              {
                clipId: "clip",
                sourceId: refills % 2 === 0 ? "a" : "b",
                mediaTimeTicks: timeTicks,
              },
            ],
          },
    isDeviceLost: () => false,
    abort: () => {},
  };
  const exporter = new Alpha2Exporter(
    backend,
    { num: 30n, den: 1n },
    0n,
    12_000n,
  );
  const sink = {
    writeVideo: async () => {},
    finish: async () => {},
    abort: async (reason) => {
      aborted = reason;
    },
  };
  await assert.rejects(
    within((signal) =>
      exportToSink(
        exporter,
        sink,
        async () => {
          refills += 1;
          await new Promise(setImmediate);
        },
        { signal },
      ),
    ),
    (error) => {
      assert.ok(error instanceof ExportFailureError, String(error));
      assert.equal(error.failure.code, "RESOURCE_LIMIT");
      assert.equal(error.failure.phase, "upload");
      assert.equal(error.failure.stage, "sources-not-held");
      assert.equal(error.failure.frameIndex, 1);
      assert.equal(error.failure.timeTicks, "4000");
      assert.match(error.message, /still waits for a after 4 refills/);
      return true;
    },
  );
  assert.equal(refills, 4, "as many refills as the editor's export allows");
  assert.equal(aborted?.failure?.stage, "sources-not-held");
});

test("after warm-up a fast sink leaves the readbacks overlapping", async () => {
  // Readbacks take a while and the encoder takes frames at once, as with a hardware encoder behind
  // mediabunny's queue: frames should keep rendering and mapping while earlier ones map.
  let mapping = 0;
  const depths = [];
  const backend = {
    begin: async () => {
      mapping += 1;
      depths.push(mapping);
      return {
        kind: "rendered",
        retainedBytes: 8,
        readback: new Promise((resolve) =>
          setTimeout(() => {
            mapping -= 1;
            resolve({ close() {} });
          }, 5),
        ),
      };
    },
    isDeviceLost: () => false,
    abort: () => {},
  };
  const exporter = new Alpha2Exporter(
    backend,
    { num: 30n, den: 1n },
    0n,
    120_000n,
  );
  const sink = {
    writeVideo: async () => {},
    finish: async () => {},
    abort: async () => {},
  };
  await exportToSink(exporter, sink, async () => {});
  const steady = depths.slice(6);
  assert.equal(steady.length, 24);
  assert.ok(
    steady.every((depth) => depth >= 2),
    `each frame after warm-up is submitted while another maps: ${depths}`,
  );
});

test("a cancel after the last frame was submitted still cancels the export", async () => {
  const controller = new AbortController();
  const calls = [];
  const backend = {
    begin: async () => ({
      kind: "rendered",
      retainedBytes: 8,
      readback: Promise.resolve({ close() {} }),
    }),
    isDeviceLost: () => false,
    abort: () => {},
  };
  // Two frames, both submitted before the first write.
  const exporter = new Alpha2Exporter(
    backend,
    { num: 30n, den: 1n },
    0n,
    8_000n,
  );
  const sink = {
    writeVideo: async ({ index }) => {
      calls.push(`write ${index}`);
      if (index === 0n) controller.abort(new Error("user cancelled"));
    },
    finish: async () => calls.push("finish"),
    abort: async (reason) => calls.push(`abort ${reason.code}`),
  };
  await assert.rejects(
    exportToSink(exporter, sink, async () => {}, {
      signal: controller.signal,
    }),
    (error) => {
      assert.equal(error.name, "ExportAbortedError");
      assert.equal(error.code, "cancelled");
      assert.equal(
        classifyBrowserFailure(error, { phase: "encode" }).code,
        "CANCELLED",
      );
      return true;
    },
  );
  assert.equal(
    calls.includes("finish"),
    false,
    "a cancelled export is not finished",
  );
  assert.equal(calls.at(-1), "abort cancelled");
});

test("a cancel during finish rejects the export and aborts its sink", async () => {
  const controller = new AbortController();
  const calls = [];
  const backend = {
    begin: async () => ({
      kind: "rendered",
      retainedBytes: 8,
      readback: Promise.resolve({ close() {} }),
    }),
    isDeviceLost: () => false,
    abort: () => {},
  };
  const exporter = new Alpha2Exporter(
    backend,
    { num: 30n, den: 1n },
    0n,
    4_000n,
  );
  const sink = {
    writeVideo: async () => {},
    finish: async () => {
      calls.push("finish");
      controller.abort();
    },
    abort: async (reason) => calls.push(`abort ${reason.code}`),
  };
  await assert.rejects(
    exportToSink(exporter, sink, async () => {}, {
      signal: controller.signal,
    }),
    { name: "ExportAbortedError", code: "cancelled" },
  );
  assert.deepEqual(calls, ["finish", "abort cancelled"]);
});

/**
 * A backend whose readbacks resolve when the test says, with frames that record being closed.
 * `ticket(index)` may override a frame's answer.
 */
function manualBackend(ticket = () => ({})) {
  const frames = [];
  const resolvers = [];
  const backend = {
    aborted: [],
    begin: async () => {
      const index = frames.length;
      const frame = {
        index,
        closed: 0,
        close() {
          frame.closed += 1;
        },
      };
      frames.push(frame);
      return {
        kind: "rendered",
        retainedBytes: 8,
        readback: new Promise((resolve) =>
          resolvers.push(() => resolve(frame)),
        ),
        ...ticket(index),
      };
    },
    deviceLostAt: Infinity,
    isDeviceLost: () => frames.length >= backend.deviceLostAt,
    abort: (reason) => backend.aborted.push(reason),
  };
  const resolveAll = () => {
    for (const resolve of resolvers.splice(0)) resolve();
  };
  return { backend, frames, resolvers, resolveAll };
}

/** A sink that closes what it is given and records its calls. */
function recordingSink(writeVideo = async () => {}) {
  const sink = {
    calls: [],
    writeVideo: async (output) => {
      sink.calls.push(`write ${output.index}`);
      try {
        await writeVideo(output);
      } finally {
        output.frame.close();
      }
    },
    finish: async () => sink.calls.push("finish"),
    abort: async (reason) => sink.calls.push(reason),
  };
  return sink;
}

const settle = () => new Promise(setImmediate);

test("a cancel while frames map closes each of them and aborts the sink", async () => {
  const { backend, frames, resolvers, resolveAll } = manualBackend();
  const exporter = new Alpha2Exporter(
    backend,
    { num: 30n, den: 1n },
    0n,
    40_000n,
  );
  const sink = recordingSink();
  const controller = new AbortController();
  const running = exportToSink(exporter, sink, async () => {}, {
    signal: controller.signal,
  });
  void running.catch(() => {});
  while (resolvers.length < 3) await settle();
  controller.abort(new Error("user cancelled"));
  resolveAll();
  await assert.rejects(running, (error) => {
    assert.equal(error.name, "ExportAbortedError");
    assert.equal(error.code, "cancelled");
    assert.equal(error.cause.message, "user cancelled");
    assert.equal(
      classifyBrowserFailure(error, { phase: "render" }).code,
      "CANCELLED",
    );
    return true;
  });
  assert.equal(frames.length, 3);
  assert.deepEqual(
    frames.map(({ closed }) => closed),
    [1, 1, 1],
    "every frame read back is closed once",
  );
  assert.equal(sink.calls.includes("finish"), false);
  assert.equal(sink.calls.filter((call) => call instanceof Error).length, 1);
  assert.equal(sink.calls.at(-1).code, "cancelled");
});

test("a lost device stops the export, aborts the backend and is classified DEVICE_LOST", async () => {
  const { backend, frames, resolveAll } = manualBackend();
  backend.deviceLostAt = 2;
  const exporter = new Alpha2Exporter(
    backend,
    { num: 30n, den: 1n },
    0n,
    40_000n,
  );
  const sink = recordingSink();
  const running = exportToSink(exporter, sink, async () => {});
  void running.catch(() => {});
  while (frames.length < 2) await settle();
  resolveAll();
  await assert.rejects(running, (error) => {
    assert.equal(error.name, "ExportAbortedError");
    assert.equal(error.code, "device-lost");
    const failure = classifyBrowserFailure(error, { phase: "render" });
    assert.equal(failure.code, "DEVICE_LOST");
    assert.equal(failure.resource, "gpu");
    return true;
  });
  assert.equal(backend.aborted.length, 1);
  assert.equal(backend.aborted[0].code, "device-lost");
  assert.deepEqual(
    frames.map(({ closed }) => closed),
    [1, 1],
  );
  assert.equal(sink.calls.at(-1).code, "device-lost");
});

test("a frame whose readback size changes fails the export and is closed", async () => {
  const { backend, frames, resolveAll } = manualBackend((index) =>
    index === 1 ? { retainedBytes: 16 } : {},
  );
  const exporter = new Alpha2Exporter(
    backend,
    { num: 30n, den: 1n },
    0n,
    40_000n,
  );
  const sink = recordingSink();
  const running = exportToSink(exporter, sink, async () => {});
  void running.catch(() => {});
  while (frames.length < 2) await settle();
  resolveAll();
  await assert.rejects(running, /export frame retained byte size changed/);
  await settle();
  assert.deepEqual(
    frames.map(({ closed }) => closed),
    [1, 1],
  );
  assert.match(sink.calls.at(-1).message, /retained byte size changed/);
});

test("a ticket with an invalid readback size is refused and its frame closed", async () => {
  for (const retainedBytes of [0, -8, 1.5, Number.NaN]) {
    const { backend, frames, resolveAll } = manualBackend(() => ({
      retainedBytes,
    }));
    const exporter = new Alpha2Exporter(
      backend,
      { num: 30n, den: 1n },
      0n,
      4_000n,
    );
    const sink = recordingSink();
    const running = exportToSink(exporter, sink, async () => {});
    void running.catch(() => {});
    while (frames.length < 1) await settle();
    resolveAll();
    await assert.rejects(running, RangeError, String(retainedBytes));
    await settle();
    assert.equal(frames[0].closed, 1, String(retainedBytes));
    assert.deepEqual(sink.calls.length, 1, "aborted, never written");
  }
});

test("a sink write that fails closes the frames queued behind it", async () => {
  const { backend, frames, resolveAll } = manualBackend();
  const exporter = new Alpha2Exporter(
    backend,
    { num: 30n, den: 1n },
    0n,
    40_000n,
  );
  let failFirst;
  const sink = recordingSink(({ index }) =>
    index === 0n
      ? new Promise((_, reject) => {
          failFirst = () => reject(new Error("encoder failed"));
        })
      : undefined,
  );
  const running = exportToSink(exporter, sink, async () => {});
  void running.catch(() => {});
  while (frames.length < 3) await settle();
  resolveAll();
  while (failFirst === undefined) await settle();
  // The next two frames wait behind the first write.
  await settle();
  failFirst();
  await assert.rejects(running, /encoder failed/);
  assert.deepEqual(
    frames.map(({ closed }) => closed),
    [1, 1, 1],
    "the failed frame and those queued behind it are closed",
  );
  assert.deepEqual(sink.calls.slice(0, 1), ["write 0"]);
  assert.equal(sink.calls.at(-1).message, "encoder failed");
});

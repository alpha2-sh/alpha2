import assert from "node:assert/strict";
import test from "node:test";

import { renderOfflineAudio } from "../dist/audio.js";
import { AudioWorkerHost } from "../dist/audio-worker.js";
import {
  classifyBrowserFailure,
  ExportFailureError,
} from "../dist/export-failure.js";
import { decideRetry, RETRY_PROFILES } from "../dist/export-retry.js";
import { Alpha2WasmError } from "../dist/worker.js";

function block(startSample, frameCount) {
  return {
    sourceId: "mix",
    startSample,
    generation: 0,
    sampleRate: 48_000,
    channels: [new Float32Array(frameCount)],
  };
}

/** Collects the block schedule the host pulled and pushed. */
function recordingSink() {
  const written = [];
  return {
    written,
    finished: false,
    aborted: null,
    async writeAudio(value) {
      written.push(value.channels[0].length);
    },
    async finish() {
      this.finished = true;
    },
    abort(reason) {
      this.aborted = reason;
    },
  };
}

test("a render request drains the schedule and completes", async () => {
  const pulled = [];
  const sink = recordingSink();
  const host = new AudioWorkerHost(
    async (startSample, frameCount) => {
      pulled.push([startSample, frameCount]);
      return block(startSample, frameCount);
    },
    () => sink,
  );

  const response = await host.handle({
    type: "render-audio",
    id: 7,
    totalFrames: 2_500n,
    blockSizes: [1_024],
  });

  assert.deepEqual(response, { type: "audio-complete", id: 7 });
  assert.deepEqual(pulled, [
    [0n, 1_024],
    [1_024n, 1_024],
    [2_048n, 452],
  ]);
  assert.deepEqual(sink.written, [1_024, 1_024, 452]);
  assert.equal(sink.finished, true);
});

test("irregular block sizes repeat in order", async () => {
  const sink = recordingSink();
  const host = new AudioWorkerHost(
    async (startSample, frameCount) => block(startSample, frameCount),
    () => sink,
  );
  await host.handle({
    type: "render-audio",
    id: 1,
    totalFrames: 1_000n,
    blockSizes: [128, 256],
  });
  assert.deepEqual(sink.written, [128, 256, 128, 256, 128, 104]);
});

test("an invalid schedule is reported as an error, not a throw", async () => {
  const sink = recordingSink();
  const host = new AudioWorkerHost(
    async () => block(0n, 0),
    () => sink,
  );
  const response = await host.handle({
    type: "render-audio",
    id: 3,
    totalFrames: 100n,
    blockSizes: [],
  });
  assert.equal(response.type, "audio-error");
  assert.equal(response.id, 3);
  assert.equal(response.cancelled, false);
  assert.match(response.error.message, /offline audio schedule/);
});

test("a renderer failure is surfaced with its message", async () => {
  const sink = recordingSink();
  const host = new AudioWorkerHost(
    async () => {
      throw new Error("decoder went away");
    },
    () => sink,
  );
  const response = await host.handle({
    type: "render-audio",
    id: 4,
    totalFrames: 64n,
    blockSizes: [64],
  });
  assert.equal(response.type, "audio-error");
  assert.equal(response.error.message, "decoder went away");
  assert.equal(response.cancelled, false);
});

test("cancelling a running render reports it as cancelled", async () => {
  const sink = recordingSink();
  let cancel;
  const started = new Promise((resolve) => {
    cancel = resolve;
  });
  const host = new AudioWorkerHost(
    async (startSample, frameCount) => {
      cancel();
      // Yield so the cancel request is handled before the next pull.
      await new Promise((resolve) => setTimeout(resolve, 0));
      return block(startSample, frameCount);
    },
    () => sink,
  );

  const pending = host.handle({
    type: "render-audio",
    id: 9,
    totalFrames: 4_096n,
    blockSizes: [64],
  });
  await started;
  assert.equal(await host.handle({ type: "cancel-audio", id: 9 }), undefined);

  const response = await pending;
  assert.equal(response.type, "audio-error");
  assert.equal(response.id, 9);
  assert.equal(response.cancelled, true);
});

test("cancelling an unknown request is a no-op", async () => {
  const host = new AudioWorkerHost(async () => block(0n, 0), recordingSink);
  assert.equal(await host.handle({ type: "cancel-audio", id: 404 }), undefined);
});

test("a classified sink failure crosses the audio worker whole", async () => {
  const overflow = new ExportFailureError({
    code: "RESOURCE_LIMIT",
    message:
      "The export passed the 1000-byte limit set for an output kept in memory",
    origin: "browser",
    operation: "export",
    backend: "unknown",
    phase: "mux",
    resource: "unknown",
  });
  const sink = recordingSink();
  sink.writeAudio = async () => {
    throw overflow;
  };
  const host = new AudioWorkerHost(
    async (startSample, frameCount) => block(startSample, frameCount),
    () => sink,
  );
  const response = structuredClone(
    await host.handle({
      type: "render-audio",
      id: 5,
      totalFrames: 64n,
      blockSizes: [64],
    }),
  );
  assert.equal(response.type, "audio-error");
  assert.equal(response.cancelled, false);
  assert.equal(response.error.name, "ExportFailureError");
  assert.equal(response.error.code, "RESOURCE_LIMIT");
  assert.equal(response.error.fatal, false);
  const error = new Alpha2WasmError(response.error);
  const failure = classifyBrowserFailure(error, { phase: "encode" });
  assert.equal(failure.code, "RESOURCE_LIMIT");
  assert.equal(failure.phase, "mux");
  assert.equal(
    decideRetry([
      {
        failure,
        profile: RETRY_PROFILES[0],
        renderer: "gpu",
        videoEncoder: "default",
      },
    ]).retry,
    false,
  );
});

test("a platform error and a wasm trap keep their name and fatality across the audio worker", async () => {
  for (const [make, fatal, expected] of [
    [
      () =>
        new DOMException("The quota has been exceeded.", "QuotaExceededError"),
      false,
      "STORAGE_QUOTA_EXCEEDED",
    ],
    [() => new WebAssembly.RuntimeError("unreachable"), true, "UNKNOWN"],
  ]) {
    const host = new AudioWorkerHost(async () => {
      throw make();
    }, recordingSink);
    const response = structuredClone(
      await host.handle({
        type: "render-audio",
        id: 6,
        totalFrames: 64n,
        blockSizes: [64],
      }),
    );
    assert.equal(response.error.fatal, fatal);
    const failure = classifyBrowserFailure(
      new Alpha2WasmError(response.error),
      {
        phase: "publish",
      },
    );
    assert.equal(failure.code, expected);
  }
});

/** A sink that records its calls, tagged with its render, into one shared log. */
function loggingSink(log, tag, firstWriteMs = 0) {
  let writes = 0;
  return {
    async writeAudio(value) {
      writes += 1;
      if (writes === 1 && firstWriteMs > 0)
        await new Promise((resolve) => setTimeout(resolve, firstWriteMs));
      log.push(`${tag} write ${value.startSample}`);
    },
    async finish() {
      log.push(`${tag} finish`);
    },
    abort(reason) {
      log.push(`${tag} abort ${reason.name}`);
    },
  };
}

test("a render asked for right after a cancel waits for the cancelled one and writes its own sink", async () => {
  const log = [];
  const sinks = [];
  const host = new AudioWorkerHost(
    async (startSample, frameCount) => block(startSample, frameCount),
    (request) => {
      sinks.push(request.id);
      // The cancelled render's first write is still in the encoder when the next one is asked for.
      return loggingSink(log, `r${request.id}`, request.id === 1 ? 50 : 0);
    },
  );
  const first = host.handle({
    type: "render-audio",
    id: 1,
    totalFrames: 4_096n,
    blockSizes: [1_024],
  });
  await new Promise((resolve) => setTimeout(resolve, 10));
  await host.handle({ type: "cancel-audio", id: 1 });
  const second = host.handle({
    type: "render-audio",
    id: 2,
    totalFrames: 4_096n,
    blockSizes: [1_024],
  });
  const [cancelled, completed] = await Promise.all([first, second]);
  assert.equal(cancelled.cancelled, true);
  assert.deepEqual(completed, { type: "audio-complete", id: 2 });
  assert.deepEqual(sinks, [1, 2]);
  assert.deepEqual(log, [
    "r1 write 0",
    "r1 abort AbortError",
    "r2 write 0",
    "r2 write 1024",
    "r2 write 2048",
    "r2 write 3072",
    "r2 finish",
  ]);
});

test("a render cancelled while it waits its turn never opens a sink", async () => {
  const log = [];
  const sinks = [];
  let release;
  const gate = new Promise((resolve) => {
    release = resolve;
  });
  const host = new AudioWorkerHost(
    async (startSample, frameCount) => {
      await gate;
      return block(startSample, frameCount);
    },
    (request) => {
      sinks.push(request.id);
      return loggingSink(log, `r${request.id}`);
    },
  );
  const first = host.handle({
    type: "render-audio",
    id: 1,
    totalFrames: 1_024n,
    blockSizes: [1_024],
  });
  const second = host.handle({
    type: "render-audio",
    id: 2,
    totalFrames: 1_024n,
    blockSizes: [1_024],
  });
  await host.handle({ type: "cancel-audio", id: 2 });
  release();
  assert.deepEqual(await first, { type: "audio-complete", id: 1 });
  const refused = await second;
  assert.equal(refused.type, "audio-error");
  assert.equal(refused.cancelled, true);
  assert.deepEqual(sinks, [1]);
});

test("a request id that is already rendering is refused and the first stays cancellable", async () => {
  let release;
  const gate = new Promise((resolve) => {
    release = resolve;
  });
  const sinks = [];
  const host = new AudioWorkerHost(
    async (startSample, frameCount) => {
      await gate;
      return block(startSample, frameCount);
    },
    (request) => {
      sinks.push(request.id);
      return recordingSink();
    },
  );
  const first = host.handle({
    type: "render-audio",
    id: 5,
    totalFrames: 4_096n,
    blockSizes: [64],
  });
  const duplicate = await host.handle({
    type: "render-audio",
    id: 5,
    totalFrames: 64n,
    blockSizes: [64],
  });
  assert.equal(duplicate.type, "audio-error");
  assert.equal(duplicate.cancelled, false);
  assert.match(duplicate.error.message, /already/);
  await host.handle({ type: "cancel-audio", id: 5 });
  release();
  const response = await first;
  assert.equal(response.cancelled, true);
  assert.deepEqual(sinks, [5]);
});

test("a sink that cannot be opened fails only its own render", async () => {
  const host = new AudioWorkerHost(
    async (startSample, frameCount) => block(startSample, frameCount),
    async (request) => {
      if (request.id === 1) throw new Error("encoder refused the config");
      return recordingSink();
    },
  );
  const failed = await host.handle({
    type: "render-audio",
    id: 1,
    totalFrames: 64n,
    blockSizes: [64],
  });
  assert.equal(failed.type, "audio-error");
  assert.equal(failed.error.message, "encoder refused the config");
  assert.deepEqual(
    await host.handle({
      type: "render-audio",
      id: 2,
      totalFrames: 64n,
      blockSizes: [64],
    }),
    { type: "audio-complete", id: 2 },
  );
});

/** A sink that records every call it gets, in order. */
function callSink(overrides = {}) {
  const calls = [];
  return {
    calls,
    async writeAudio(value) {
      calls.push(["write", value.channels[0].length]);
    },
    async finish() {
      calls.push(["finish"]);
    },
    async abort(reason) {
      calls.push(["abort", reason]);
    },
    ...overrides,
  };
}

const fill = async (startSample, frameCount) => block(startSample, frameCount);

test("a failed render aborts its sink once with the failure and never finishes it", async () => {
  const broken = new Error("decoder went away");
  const sink = callSink();
  let pulls = 0;
  await assert.rejects(
    renderOfflineAudio(
      256n,
      [64],
      async (startSample, frameCount) => {
        pulls += 1;
        if (pulls === 2) throw broken;
        return block(startSample, frameCount);
      },
      sink,
    ),
    (error) => error === broken,
  );
  assert.deepEqual(sink.calls, [
    ["write", 64],
    ["abort", broken],
  ]);
});

test("a cancelled render aborts its sink with the signal's reason and never finishes it", async () => {
  const controller = new AbortController();
  const reason = new DOMException("Cancelled", "AbortError");
  const sink = callSink({
    async writeAudio(value) {
      sink.calls.push(["write", value.channels[0].length]);
      controller.abort(reason);
    },
  });
  await assert.rejects(
    renderOfflineAudio(256n, [64], fill, sink, controller.signal),
    (error) => error === reason,
  );
  assert.deepEqual(sink.calls, [
    ["write", 64],
    ["abort", reason],
  ]);
});

test("a cancel during the last block or during finish still cancels the render", async () => {
  // Cancelled while the last block renders, while it is written, and while the sink finishes: the
  // loop has no block left to check before, and the render must not report success.
  for (const at of ["render", "write", "finish"]) {
    const controller = new AbortController();
    const reason = new DOMException("Cancelled", "AbortError");
    const sink = callSink({
      async writeAudio(value) {
        sink.calls.push(["write", value.channels[0].length]);
        if (at === "write" && sink.calls.length === 2) controller.abort(reason);
      },
      async finish() {
        sink.calls.push(["finish"]);
        if (at === "finish") controller.abort(reason);
      },
    });
    let pulls = 0;
    await assert.rejects(
      renderOfflineAudio(
        128n,
        [64],
        async (startSample, frameCount) => {
          pulls += 1;
          if (at === "render" && pulls === 2) controller.abort(reason);
          return block(startSample, frameCount);
        },
        sink,
        controller.signal,
      ),
      (error) => error === reason,
      at,
    );
    assert.deepEqual(
      sink.calls,
      [
        ["write", 64],
        ...(at === "render" ? [] : [["write", 64]]),
        ...(at === "finish" ? [["finish"]] : []),
        ["abort", reason],
      ],
      at,
    );
  }
});

test("a block of the wrong length fails the render and aborts the sink", async () => {
  const sink = callSink();
  await assert.rejects(
    renderOfflineAudio(
      128n,
      [64],
      async (startSample) => block(startSample, 63),
      sink,
    ),
    (error) => {
      assert.ok(error instanceof RangeError);
      assert.match(error.message, /invalid audio block/);
      assert.deepEqual(sink.calls, [["abort", error]]);
      return true;
    },
  );
});

test("an invalid schedule is refused before the sink is touched", async () => {
  for (const [totalFrames, blockSizes] of [
    [-1n, [64]],
    [64n, []],
    [64n, [0]],
    [64n, [-64]],
    [64n, [32.5]],
    [64n, [64, Number.NaN]],
    [64n, [2 ** 53]],
  ]) {
    const sink = callSink();
    await assert.rejects(
      renderOfflineAudio(totalFrames, blockSizes, fill, sink),
      RangeError,
      `${totalFrames} ${blockSizes}`,
    );
    assert.deepEqual(sink.calls, [], `${totalFrames} ${blockSizes}`);
  }
});

test("a sink whose abort fails does not replace the render's failure", async () => {
  // A destination that refuses a write leaves its stream errored, so closing it in `abort` throws
  // too; the export must still report the full disk, not the closed stream.
  const fullDisk = () =>
    new ExportFailureError({
      code: "STORAGE_QUOTA_EXCEEDED",
      message: "The export file could not be written: the disk is full",
      origin: "browser",
      operation: "export",
      backend: "unknown",
      phase: "publish",
      resource: "unknown",
    });
  const quota = fullDisk();
  const sink = callSink({
    async writeAudio() {
      throw quota;
    },
    async abort() {
      throw new TypeError("Invalid state: WritableStream is closed");
    },
  });
  await assert.rejects(renderOfflineAudio(128n, [64], fill, sink), (error) => {
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
  });

  // An unclassified failure has no record to note the cleanup in; it is still the one reported.
  const broken = new Error("decoder went away");
  const failing = callSink({
    async abort() {
      throw new Error("stream already closed");
    },
  });
  await assert.rejects(
    renderOfflineAudio(
      128n,
      [64],
      async () => {
        throw broken;
      },
      failing,
    ),
    (error) => error === broken && error.failure === undefined,
  );

  const host = new AudioWorkerHost(fill, () =>
    callSink({
      async writeAudio() {
        throw fullDisk();
      },
      async abort() {
        throw new TypeError("Invalid state: WritableStream is closed");
      },
    }),
  );
  const response = structuredClone(
    await host.handle({
      type: "render-audio",
      id: 8,
      totalFrames: 64n,
      blockSizes: [64],
    }),
  );
  assert.equal(response.error.code, "STORAGE_QUOTA_EXCEEDED");
  const failure = classifyBrowserFailure(new Alpha2WasmError(response.error), {
    phase: "encode",
  });
  assert.equal(failure.code, "STORAGE_QUOTA_EXCEEDED");
  assert.equal(failure.secondary?.[0]?.phase, "cleanup");
});

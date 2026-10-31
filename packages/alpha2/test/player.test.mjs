import assert from "node:assert/strict";
import test from "node:test";

import { Alpha2Player } from "../dist/player.js";
import { GpuRecoveryLadder } from "../dist/gpu-recovery.js";

/** A loss count of its own, so one test's losses do not move another's ladder. */
function losses() {
  let count = 0;
  return { count: () => count, add: () => void (count += 1) };
}

/** A ladder with a loss count of its own. */
const ladder = () => new GpuRecoveryLadder({ losses: losses() });

/** Records every backend call so ordering and generation bumps are observable. */
class FakeBackend {
  constructor() {
    this.calls = [];
    this.uploads = [];
    this.deviceLost = false;
    this.missingSources = [];
    // Source time is never the timeline time: each missing clip starts 1000 ticks in.
    this.sourceOffset = 1_000n;
    this.disposed = false;
  }

  async initialize(canvas) {
    this.calls.push(["initialize", canvas]);
  }

  async uploadVideoFrame(frame) {
    this.calls.push(["uploadVideoFrame", frame.sourceId]);
    this.uploads.push(frame);
  }

  async present(timeTicks) {
    this.calls.push(["present", timeTicks]);
    return {
      requestedTimeTicks: timeTicks,
      presentedTimeTicks: timeTicks,
      missingSources: this.missingSources,
      missingFrames: this.missingSources.map((sourceId) => ({
        clipId: `${sourceId}-clip`,
        sourceId,
        mediaTimeTicks: timeTicks + this.sourceOffset,
      })),
      repeated: false,
    };
  }

  async reset() {
    this.calls.push(["reset"]);
  }

  isDeviceLost() {
    return this.deviceLost;
  }

  lossReport() {
    return this.report;
  }

  async probeSurface() {
    return this.surface ?? { kind: "webgpu", webgpuAdapter: true };
  }

  async recover(step, canvas) {
    this.calls.push(["recover", step, canvas]);
    this.deviceLost = false;
    return {
      renderer: step.renderer,
      rendererReason: null,
      backend: "webgpu",
      adapter: null,
    };
  }

  dispose() {
    this.calls.push(["dispose"]);
    this.disposed = true;
  }
}

function frame(sourceId, generation) {
  return {
    sourceId,
    requestedTimeTicks: 0n,
    decodedTimeTicks: 0n,
    frame: {
      closed: false,
      close() {
        this.closed = true;
      },
    },
    generation,
  };
}

test("the player refuses work before initialization", async () => {
  const player = new Alpha2Player(new FakeBackend());
  await assert.rejects(() => player.present(0n), /must be initialized/);
});

test("presentation forwards to the backend once initialized", async () => {
  const backend = new FakeBackend();
  const player = new Alpha2Player(backend);
  const canvas = { width: 2, height: 2 };
  await player.initialize(canvas);
  const presented = await player.present(120n);
  assert.equal(presented?.presentedTimeTicks, 120n);
  assert.deepEqual(backend.calls, [
    ["initialize", canvas],
    ["present", 120n],
  ]);
});

test("missing frames become refill requests at their source time, with the generation", async () => {
  const backend = new FakeBackend();
  backend.missingSources = ["clip-a"];
  const refills = [];
  const player = new Alpha2Player(backend, {
    onRefill: (requests) => {
      refills.push(...requests);
    },
  });
  await player.initialize({});
  await player.present(240n);
  assert.deepEqual(refills, [
    {
      sourceId: "clip-a",
      mediaTimeTicks: 1_240n,
      reason: "prefetch",
      generation: 0,
      consumerId: "clip-a-clip",
    },
  ]);

  refills.length = 0;
  await player.seek(480n);
  assert.deepEqual(refills, [
    {
      sourceId: "clip-a",
      mediaTimeTicks: 1_480n,
      reason: "seek",
      generation: 1,
      consumerId: "clip-a-clip",
    },
  ]);
});

test("a lost device is recovered before the frame is presented", async () => {
  const backend = new FakeBackend();
  backend.deviceLost = true;
  backend.missingSources = ["clip-a"];
  const refills = [];
  const player = new Alpha2Player(backend, {
    onRefill: (requests) => {
      refills.push(...requests);
    },
    recovery: ladder(),
  });
  await player.initialize({});
  await player.present(0n);
  assert.deepEqual(
    backend.calls.map(([name]) => name),
    ["initialize", "recover", "present"],
  );
  // A first external loss on a WebGPU canvas whose host still gets an adapter keeps the canvas.
  assert.equal(backend.calls[1][1].renderer, "gpu");
  assert.equal(backend.calls[1][1].surface, "same");
  assert.equal(refills[0].reason, "device-recovery");
  assert.equal(refills[0].generation, 1);
});

test("a seek invalidates frames decoded for the previous generation", async () => {
  const backend = new FakeBackend();
  const player = new Alpha2Player(backend);
  await player.initialize({});
  const stale = frame("clip-a", 0);
  await player.seek(0n);
  await player.uploadVideoFrame(stale);
  assert.equal(stale.frame.closed, true);
  assert.deepEqual(backend.uploads, []);

  const current = frame("clip-a", 1);
  await player.uploadVideoFrame(current);
  assert.equal(current.frame.closed, false);
  assert.deepEqual(backend.uploads, [current]);
});

test("disposal releases the backend and locks the player", async () => {
  const backend = new FakeBackend();
  const player = new Alpha2Player(backend);
  await player.initialize({});
  player.dispose();
  assert.equal(backend.disposed, true);
  await assert.rejects(() => player.present(0n), /must be initialized/);
});

/**
 * A backend that behaves as the engine does: a source frame is found only under the exact (source,
 * media time) it was uploaded at, a presentation missing any frame shows the last complete one
 * again (HoldPrevious), and a reset forgets it.
 */
class ExactFrameBackend {
  constructor(sources) {
    this.sources = sources;
    this.uploaded = new Set();
    this.presents = [];
    this.shown = undefined;
  }
  async initialize() {}
  async uploadVideoFrame(frame) {
    this.uploaded.add(`${frame.sourceId}@${frame.requestedTimeTicks}`);
  }
  async present(timeTicks) {
    this.presents.push(timeTicks);
    const missingFrames = this.sources
      .map((sourceId) => ({
        clipId: `${sourceId}-clip`,
        sourceId,
        mediaTimeTicks: timeTicks,
      }))
      .filter(({ sourceId }) => !this.uploaded.has(`${sourceId}@${timeTicks}`));
    const repeated = missingFrames.length > 0;
    if (!repeated) this.shown = timeTicks;
    return {
      requestedTimeTicks: timeTicks,
      presentedTimeTicks: this.shown,
      missingSources: [...new Set(missingFrames.map((f) => f.sourceId))],
      missingFrames,
      repeated: repeated && this.shown !== undefined,
    };
  }
  async reset() {
    this.shown = undefined;
  }
  isDeviceLost() {
    return false;
  }
  async recover() {}
  dispose() {}
}

/**
 * A host refill that decodes what it is asked for, except the sources in `offline`, and uploads it
 * before resolving.
 */
function refillingPlayer(backend, offline = new Set()) {
  const asked = [];
  const player = new Alpha2Player(backend, {
    onRefill: async (requests) => {
      asked.push(requests.map((request) => request.sourceId));
      for (const request of requests) {
        if (offline.has(request.sourceId)) continue;
        await player.uploadVideoFrame({
          ...frame(request.sourceId, request.generation),
          requestedTimeTicks: request.mediaTimeTicks,
          decodedTimeTicks: request.mediaTimeTicks,
        });
      }
    },
  });
  return { player, asked };
}

test("a presentation shows its own time once the frames it asked for arrive", async () => {
  const backend = new ExactFrameBackend(["cam"]);
  const { player } = refillingPlayer(backend);
  await player.initialize({});
  // Every display refresh asks for a time no earlier one uploaded, as an audio clock does.
  for (const time of [0n, 2_001n, 4_002n]) {
    const presented = await player.present(time);
    assert.equal(presented?.presentedTimeTicks, time);
    assert.equal(presented?.repeated, false);
    assert.deepEqual(presented?.missingFrames, []);
  }
});

test("a seek shows its target once the frames it asked for arrive", async () => {
  const backend = new ExactFrameBackend(["cam"]);
  const { player } = refillingPlayer(backend);
  await player.initialize({});
  const presented = await player.seek(90_000n);
  assert.equal(presented?.presentedTimeTicks, 90_000n);
  assert.deepEqual(presented?.missingSources, []);
});

test("a source that never arrives is asked for once and the presentation ends", async () => {
  const backend = new ExactFrameBackend(["cam", "offline"]);
  const { player, asked } = refillingPlayer(backend, new Set(["offline"]));
  await player.initialize({});
  const presented = await player.present(500n);
  assert.deepEqual(presented?.missingSources, ["offline"]);
  // Asking for `offline` again uploads nothing, so there is no third presentation.
  assert.deepEqual(asked, [["cam", "offline"], ["offline"]]);
  assert.deepEqual(backend.presents, [500n, 500n]);

  const alone = new ExactFrameBackend(["offline"]);
  const nothing = refillingPlayer(alone, new Set(["offline"]));
  await nothing.player.initialize({});
  await nothing.player.present(0n);
  assert.deepEqual(alone.presents, [0n], "no upload, no second presentation");
});

test("a presentation superseded while its frames decode is not presented again", async () => {
  const backend = new ExactFrameBackend(["cam"]);
  let later;
  const player = new Alpha2Player(backend, {
    onRefill: async (requests) => {
      later ??= player.present(9_000n);
      for (const request of requests)
        await player.uploadVideoFrame({
          ...frame(request.sourceId, request.generation),
          requestedTimeTicks: request.mediaTimeTicks,
        });
    },
  });
  await player.initialize({});
  assert.equal(await player.present(1_000n), undefined);
  assert.equal((await later)?.presentedTimeTicks, 9_000n);
  assert.deepEqual(backend.presents, [1_000n, 9_000n, 9_000n]);
});

test("presentations that overlap a device recovery share it, and the generation moves once", async () => {
  const backend = new FakeBackend();
  backend.deviceLost = true;
  backend.missingSources = ["clip-a"];
  let finish;
  let recoveries = 0;
  backend.recover = () => {
    recoveries += 1;
    backend.calls.push(["recover"]);
    return new Promise((resolve) => {
      finish = () => {
        backend.deviceLost = false;
        resolve();
      };
    });
  };
  const refills = [];
  const player = new Alpha2Player(backend, {
    onRefill: (requests) => {
      refills.push(...requests);
    },
    recovery: ladder(),
  });
  await player.initialize({});
  // An animation-frame loop presents without awaiting.
  const presented = [0n, 4_000n, 8_000n, 12_000n].map((time) =>
    player.present(time),
  );
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(recoveries, 1);
  finish();
  const results = await Promise.all(presented);
  assert.equal(recoveries, 1);
  assert.deepEqual(
    results.map((result) => result?.requestedTimeTicks),
    [undefined, undefined, undefined, 12_000n],
  );
  assert.deepEqual(
    refills.map(({ reason, generation }) => [reason, generation]),
    [["device-recovery", 1]],
  );
});

test("a failed recovery rejects every presentation that waited for it, and later ones without trying again", async () => {
  const backend = new FakeBackend();
  backend.deviceLost = true;
  let attempts = 0;
  backend.recover = async () => {
    attempts += 1;
    await new Promise((resolve) => setTimeout(resolve, 5));
    throw new Error("context not restored");
  };
  const player = new Alpha2Player(backend, { recovery: ladder() });
  await player.initialize({});
  const results = await Promise.allSettled([
    player.present(0n),
    player.present(4_000n),
    player.present(8_000n),
  ]);
  // The same canvas failed; the next rung needs a fresh one, which this player cannot get.
  assert.equal(attempts, 1);
  for (const result of results) {
    assert.equal(result.reason.code, "GPU_DEVICE_LOST");
    assert.equal(result.reason.fatal, true);
    assert.match(result.reason.message, /replaceSurface/);
  }
  await assert.rejects(player.present(12_000n), { code: "GPU_DEVICE_LOST" });
  assert.equal(attempts, 1);
});

test("a fresh surface step asks replaceSurface once", async () => {
  const backend = new FakeBackend();
  backend.deviceLost = true;
  // A WebGL2 canvas keeps its lost context: the GPU comes back on a fresh one.
  backend.surface = { kind: "webgl2", webgpuAdapter: false };
  const surfaces = [];
  const changes = [];
  const player = new Alpha2Player(backend, {
    recovery: ladder(),
    replaceSurface: async () => {
      const canvas = { fresh: surfaces.length + 1 };
      surfaces.push(canvas);
      return canvas;
    },
    onRendererChange: (choice) => changes.push(choice.renderer),
  });
  await player.initialize({});
  await Promise.all([player.present(0n), player.present(1_000n)]);
  assert.equal(surfaces.length, 1);
  const recovers = backend.calls.filter(([name]) => name === "recover");
  assert.equal(recovers.length, 1);
  assert.equal(recovers[0][1].renderer, "gpu");
  assert.equal(recovers[0][1].surface, "fresh");
  assert.equal(recovers[0][2], surfaces[0]);
  assert.deepEqual(changes, ["gpu"]);
});

test("a hung loss recovers on the cpu on a fresh surface", async () => {
  const backend = new FakeBackend();
  backend.deviceLost = true;
  backend.report = {
    hung: true,
    frameIndex: 3,
    timeTicks: 12_000n,
    gpuMs: 1800,
    message: "hung",
  };
  const player = new Alpha2Player(backend, {
    recovery: ladder(),
    replaceSurface: async () => ({ fresh: true }),
  });
  await player.initialize({});
  await player.present(0n);
  const [recover] = backend.calls.filter(([name]) => name === "recover");
  assert.equal(recover[1].renderer, "cpu");
  assert.deepEqual(recover[2], { fresh: true });
});

test("without replaceSurface a fresh step rejects with a typed fatal loss", async () => {
  const backend = new FakeBackend();
  backend.deviceLost = true;
  backend.surface = { kind: "webgl2", webgpuAdapter: false };
  const player = new Alpha2Player(backend, { recovery: ladder() });
  await player.initialize({});
  await assert.rejects(player.present(0n), (error) => {
    assert.equal(error.code, "GPU_DEVICE_LOST");
    assert.equal(error.fatal, true);
    assert.match(
      error.message,
      /the canvas cannot take another renderer: give Alpha2Player a replaceSurface option/,
    );
    return true;
  });
  assert.equal(backend.calls.filter(([name]) => name === "recover").length, 0);
});

test("a terminal ladder stops calling recover", async () => {
  const backend = new FakeBackend();
  backend.deviceLost = true;
  const steps = [];
  backend.recover = async (step) => {
    steps.push(`${step.renderer}/${step.surface}`);
    throw new Error(`${step.renderer} unavailable`);
  };
  let replaced = 0;
  const player = new Alpha2Player(backend, {
    recovery: ladder(),
    replaceSurface: async () => ({ fresh: ++replaced }),
  });
  await player.initialize({});
  await assert.rejects(player.present(0n), (error) => {
    assert.equal(error.code, "GPU_DEVICE_LOST");
    assert.match(
      error.message,
      /no renderer could be made again \(cpu unavailable\)/,
    );
    assert.equal(error.cause.message, "cpu unavailable");
    return true;
  });
  // Each rung once, a fresh surface for each fresh one.
  assert.deepEqual(steps, ["gpu/same", "gpu/fresh", "cpu/fresh"]);
  assert.equal(replaced, 2);
  await assert.rejects(player.present(1_000n), { code: "GPU_DEVICE_LOST" });
  assert.equal(steps.length, 3);
});

test("a frame delivered to a player that is not initialized, or was disposed, is closed", async () => {
  const early = frame("clip-a", 0);
  const player = new Alpha2Player(new FakeBackend());
  await assert.rejects(player.uploadVideoFrame(early), /must be initialized/);
  assert.equal(early.frame.closed, true);

  await player.initialize({});
  // A refill was still decoding when the canvas went away.
  player.dispose();
  const late = frame("clip-a", 0);
  await assert.rejects(player.uploadVideoFrame(late), /must be initialized/);
  assert.equal(late.frame.closed, true);
});

/** A backend whose calls take a few milliseconds, as a worker's round trips do. */
class SlowBackend extends FakeBackend {
  async present(timeTicks) {
    await new Promise((resolve) => setTimeout(resolve, 3));
    return super.present(timeTicks);
  }

  async reset() {
    await new Promise((resolve) => setTimeout(resolve, 3));
    return super.reset();
  }
}

test("only the latest of overlapping seeks and presentations asks for frames", async () => {
  const backend = new SlowBackend();
  backend.missingSources = ["clip-a"];
  backend.sourceOffset = 0n;
  const refills = [];
  const player = new Alpha2Player(backend, {
    onRefill: (requests) => {
      refills.push(
        ...requests.map(({ reason, mediaTimeTicks, generation }) => [
          reason,
          mediaTimeTicks,
          generation,
        ]),
      );
    },
  });
  await player.initialize({});
  await Promise.all([
    player.seek(1_000n),
    player.seek(2_000n),
    player.seek(3_000n),
  ]);
  assert.deepEqual(refills, [["seek", 3_000n, 3]]);
  refills.length = 0;
  await Promise.all([player.present(500n), player.seek(9_000n)]);
  assert.deepEqual(refills, [["seek", 9_000n, 4]]);
});

/** Sends every non-software frame of `nonPlanar` sources back for a software decode, once held. */
class SoftwareBackend extends FakeBackend {
  constructor(reason) {
    super();
    this.reason = reason;
    this.held = new Set();
  }

  async uploadVideoFrame(frame) {
    this.calls.push([
      "uploadVideoFrame",
      frame.sourceId,
      frame.decode ?? "default",
    ]);
    if (frame.decode !== "software") {
      frame.frame.close();
      throw Object.assign(new Error("decode it again in software"), {
        code: "SOFTWARE_DECODE_REQUIRED",
        failure: {
          code: "UNKNOWN",
          diagnostics: {
            sourceId: frame.sourceId,
            mediaTimeTicks: String(frame.requestedTimeTicks),
            reason: this.reason,
          },
        },
      });
    }
    this.held.add(frame.sourceId);
  }

  async present(timeTicks) {
    this.missingSources = ["hdr"].filter((source) => !this.held.has(source));
    return super.present(timeTicks);
  }

  async reset() {
    this.held.clear();
    return super.reset();
  }
}

test("software decode required reissues the refill in software once", async () => {
  for (const reason of ["non-planar", "unreadable"]) {
    const backend = new SoftwareBackend(reason);
    backend.sourceOffset = 0n;
    const asked = [];
    let player;
    player = new Alpha2Player(backend, {
      onRefill: async (requests) => {
        for (const request of requests) {
          asked.push([request.mediaTimeTicks, request.decode ?? "default"]);
          await player.uploadVideoFrame({
            ...frame(request.sourceId, request.generation),
            requestedTimeTicks: request.mediaTimeTicks,
            ...(request.decode === "software" ? { decode: "software" } : {}),
          });
        }
      },
    });
    await player.initialize({});
    const shown = await player.seek(0n);
    assert.deepEqual(shown.missingSources, [], reason);
    assert.deepEqual(asked, [
      [0n, "default"],
      [0n, "software"],
    ]);
    // A source whose frames have no planes is asked for in software from then on; a frame the
    // browser could not read back is asked of its decoder again.
    asked.length = 0;
    backend.held.clear();
    await player.seek(4_000n);
    assert.deepEqual(
      asked,
      reason === "non-planar"
        ? [[4_000n, "software"]]
        : [
            [4_000n, "default"],
            [4_000n, "software"],
          ],
    );
  }
});

test("a frame sent back for a software decode twice fails the refill", async () => {
  const backend = new SoftwareBackend("unreadable");
  backend.sourceOffset = 0n;
  let player;
  player = new Alpha2Player(backend, {
    // A refill that ignores `decode` hands the hardware frame over again.
    onRefill: async (requests) => {
      for (const request of requests)
        await player.uploadVideoFrame({
          ...frame(request.sourceId, request.generation),
          requestedTimeTicks: request.mediaTimeTicks,
        });
    },
  });
  await player.initialize({});
  await assert.rejects(player.seek(0n), { code: "SOFTWARE_DECODE_REQUIRED" });
});

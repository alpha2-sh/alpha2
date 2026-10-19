import assert from "node:assert/strict";
import test from "node:test";

import { Alpha2Player } from "../dist/player.js";

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

  async recover() {
    this.calls.push(["recover"]);
    this.deviceLost = false;
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
  const player = new Alpha2Player(backend, (requests) => {
    refills.push(...requests);
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
  const player = new Alpha2Player(backend, (requests) => {
    refills.push(...requests);
  });
  await player.initialize({});
  await player.present(0n);
  assert.deepEqual(
    backend.calls.map(([name]) => name),
    ["initialize", "recover", "present"],
  );
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
  const player = new Alpha2Player(backend, async (requests) => {
    asked.push(requests.map((request) => request.sourceId));
    for (const request of requests) {
      if (offline.has(request.sourceId)) continue;
      await player.uploadVideoFrame({
        ...frame(request.sourceId, request.generation),
        requestedTimeTicks: request.mediaTimeTicks,
        decodedTimeTicks: request.mediaTimeTicks,
      });
    }
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
  const player = new Alpha2Player(backend, async (requests) => {
    later ??= player.present(9_000n);
    for (const request of requests)
      await player.uploadVideoFrame({
        ...frame(request.sourceId, request.generation),
        requestedTimeTicks: request.mediaTimeTicks,
      });
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
  const player = new Alpha2Player(backend, (requests) => {
    refills.push(...requests);
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

test("a failed recovery rejects every presentation that waited for it, and the next one tries again", async () => {
  const backend = new FakeBackend();
  backend.deviceLost = true;
  let attempts = 0;
  backend.recover = async () => {
    attempts += 1;
    await new Promise((resolve) => setTimeout(resolve, 5));
    if (attempts === 1) throw new Error("context not restored");
    backend.deviceLost = false;
  };
  const player = new Alpha2Player(backend);
  await player.initialize({});
  const results = await Promise.allSettled([
    player.present(0n),
    player.present(4_000n),
    player.present(8_000n),
  ]);
  assert.equal(attempts, 1);
  assert.deepEqual(
    results.map((result) => result.reason?.message),
    ["context not restored", "context not restored", "context not restored"],
  );
  assert.equal((await player.present(12_000n))?.requestedTimeTicks, 12_000n);
  assert.equal(attempts, 2);
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
  const player = new Alpha2Player(backend, (requests) => {
    refills.push(
      ...requests.map(({ reason, mediaTimeTicks, generation }) => [
        reason,
        mediaTimeTicks,
        generation,
      ]),
    );
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

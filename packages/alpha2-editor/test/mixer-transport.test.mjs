import { register } from "node:module";
import test from "node:test";
import assert from "node:assert/strict";

register("./support/worklet-hook.mjs", import.meta.url);
globalThis.crossOriginIsolated = true;
globalThis.AudioContext = class extends EventTarget {
  constructor({ sampleRate }) {
    super();
    this.sampleRate = sampleRate;
    this.state = "running";
    this.destination = {};
    this.currentTime = 0;
    this.outputLatency = 0;
    this.baseLatency = 0;
  }
  async resume() {}
  async close() {}
};

const { EngineAudioMixer } = await import("../dist/browser/runtime/audio.js");

test("a stop while the first block is still decoding ends Play's start without an error", async () => {
  const mixer = new EngineAudioMixer({}, { enabled: true });
  let supplying = null;
  // The first block waits on a decode the transport's signal aborts, like the real supplier's.
  const supplier = {
    supply: (start, frames, { signal }) =>
      new Promise((_, reject) => {
        supplying = signal;
        signal.addEventListener("abort", () => reject(signal.reason), {
          once: true,
        });
      }),
  };
  const continuity = [];
  const engine = {
    resetAudio() {},
    setAudioContinuity(policy) {
      continuity.push(policy);
    },
  };
  const errors = [];
  await mixer.attach(
    engine,
    { settings: { sample_rate: 48_000 } },
    supplier,
    null,
    (error) => errors.push(error),
  );
  assert.deepEqual(continuity, ["continue-small-gaps"]);
  const starting = mixer.start(0);
  for (let i = 0; i < 100 && !supplying; i += 1)
    await new Promise((resolve) => setTimeout(resolve, 1));
  assert.ok(supplying, "the first block is being supplied");
  mixer.stop();
  await starting;
  assert.equal(mixer.tick, null, "nothing plays");
  assert.deepEqual(errors, []);
  await mixer.close();
});

test("an output suspended under playback stops the mixer and is reported once", async () => {
  const interrupted = [];
  const mixer = new EngineAudioMixer(
    {},
    { enabled: true, onInterrupted: (state) => interrupted.push(state) },
  );
  const engine = {
    resetAudio() {},
    setAudioContinuity() {},
    renderAudio: (start, frames) => new Float32Array(2 * frames),
    audioChannels: () => 2,
    audioMissingSources: () => "[]",
  };
  const supplier = { supply: async () => {} };
  await mixer.attach(engine, { settings: { sample_rate: 48_000 } }, supplier);
  mixer.setLatencyOffset(30);
  await mixer.start(0);
  assert.notEqual(mixer.tick, null, "playing");
  assert.equal(mixer.latencySource, "base-only");
  const context = mixer.context;
  context.state = "suspended";
  context.dispatchEvent(new Event("statechange"));
  context.dispatchEvent(new Event("statechange"));
  assert.deepEqual(interrupted, ["suspended"]);
  assert.equal(mixer.tick, null, "stopped");
  assert.equal(mixer.latencySource, null);
  await mixer.close();
});

test("a source that never arrives plays silent after a few mixes, reported once, and the rest plays", async () => {
  const mixer = new EngineAudioMixer({}, { enabled: true });
  const supplied = [];
  const supplier = {
    supply: async (start, frames, { silentSources = new Set() } = {}) => {
      supplied.push([...silentSources]);
      if (silentSources.has("gone")) silent = true;
    },
  };
  let silent = false,
    mixes = 0;
  const engine = {
    resetAudio() {},
    setAudioContinuity() {},
    renderAudio: (start, frames) => {
      mixes += 1;
      return new Float32Array(2 * frames).fill(0.25);
    },
    audioChannels: () => 2,
    audioMissingSources: () => (silent ? "[]" : '["gone"]'),
  };
  const errors = [];
  await mixer.attach(
    engine,
    { settings: { sample_rate: 48_000 } },
    supplier,
    null,
    (error) => errors.push(error),
  );
  await mixer.start(0);
  for (let i = 0; i < 100 && !silent; i += 1)
    await new Promise((resolve) => setTimeout(resolve, 10));
  assert.ok(silent, "the missing source was given silence");
  await new Promise((resolve) => setTimeout(resolve, 100));
  assert.deepEqual(
    errors.map((e) => [e.code, e.entityId]),
    [["audio_unavailable", "gone"]],
  );
  const afterSilence = supplied.findIndex((ids) => ids.includes("gone"));
  assert.ok(afterSilence >= 6, `silenced after ${afterSilence} supplies`);
  assert.ok(supplied.slice(afterSilence).every((ids) => ids.includes("gone")));
  assert.ok(mixes > afterSilence / 2, "mixing goes on");
  await mixer.close();
});

test("a start whose first block fails stops the mixer before it rethrows", async () => {
  const mixer = new EngineAudioMixer({}, { enabled: true });
  let transport = null;
  const supplier = {
    supply: async (start, frames, { signal }) => {
      transport = signal;
      throw new Error("the decoder failed");
    },
  };
  const engine = { resetAudio() {}, setAudioContinuity() {} };
  await mixer.attach(engine, { settings: { sample_rate: 48_000 } }, supplier);
  await assert.rejects(mixer.start(0), /the decoder failed/);
  assert.equal(mixer.tick, null);
  assert.equal(transport?.aborted, true, "the start's transport is stopped");
  await mixer.close();
});

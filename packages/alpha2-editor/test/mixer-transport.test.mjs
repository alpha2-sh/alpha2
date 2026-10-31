// The preview mixer's transport: stopping it while Play is still priming the first block ends that
// start quietly, as a stop between blocks does; an output the system suspends under playback stops
// it and is reported once.
import { register } from "node:module";
import test from "node:test";
import assert from "node:assert/strict";

register("./support/worklet-hook.mjs", import.meta.url);
// Preview sound needs shared memory, which a cross-origin isolated page has.
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
  // The first block waits on a decode, which the transport's signal aborts, as the real supplier's
  // decode does.
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
  // Preview mixes small steps without replaying the effects' history.
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
  // The stand-in context reports no output latency and no output timestamp.
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

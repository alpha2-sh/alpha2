import assert from "node:assert/strict";
import test, { afterEach } from "node:test";

import { workletWasm } from "./worklet-ring-reader.mjs";

// Each fake port is closed per test, pass or fail: the file's top-level awaits let a root `after`
// run before later tests, whose ports it would never see.
const ports = [];
afterEach(() => {
  for (const port of ports.splice(0)) port.close();
});

function channel() {
  const pair = new MessageChannel();
  ports.push(pair.port1, pair.port2);
  return pair;
}

/** Messages the next `count` posts from `port` deliver, or none after 1 s. */
function nextMessages(port, count = 1) {
  return new Promise((resolve) => {
    const seen = [];
    const timeout = setTimeout(() => resolve(seen), 1_000);
    port.onmessage = (event) => {
      seen.push(event.data);
      if (seen.length === count) {
        clearTimeout(timeout);
        resolve(seen);
      }
    };
  });
}

// Stand-ins for the AudioWorkletGlobalScope globals the processor module registers against.
let Processor;
globalThis.sampleRate = 48_000;
globalThis.AudioWorkletProcessor = class {
  constructor() {
    const pair = channel();
    this.port = pair.port1;
    this.hostPort = pair.port2;
  }
};
globalThis.registerProcessor = (_name, constructor) => {
  Processor = constructor;
};
await import("../dist/audio-worklet-processor.js");

async function readyProcessor(channels, wasm, sourceRate) {
  const processor = new Processor();
  const ready = new Promise((resolve, reject) => {
    processor.hostPort.onmessage = (event) => {
      if (event.data.type === "alpha2-ready") resolve();
      if (event.data.type === "alpha2-error")
        reject(new Error(event.data.message));
    };
  });
  processor.port.onmessage({
    data: { type: "alpha2-init", wasm: wasm.slice(0), channels, sourceRate },
  });
  await ready;
  return processor;
}

test(
  "an output with a different channel count reports instead of going silent",
  { timeout: 10_000 },
  async (t) => {
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    const processor = await readyProcessor(2, wasm);
    const reported = nextMessages(processor.hostPort);
    const mono = [[new Float32Array(128)]];
    assert.equal(processor.process([[]], mono), true);
    const [message] = await reported;
    assert.equal(
      message?.type,
      "alpha2-error",
      "the mismatch must be reported",
    );
    assert.match(message.message, /1 channel.*2/);
  },
);

test(
  "a trap in the DSP module reports its panic message and stops the node",
  { timeout: 10_000 },
  async (t) => {
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    // Nothing reachable from the host panics, so the trap is planted: `process` is replaced by one
    // that writes a panic message and traps.
    const instantiate = WebAssembly.instantiate;
    WebAssembly.instantiate = async (bytes, imports) => {
      const { instance } = await instantiate(bytes, imports);
      const exports = { ...instance.exports };
      exports.alpha2_worklet_process = () => {
        const text = new TextEncoder().encode("panicked at audio.rs:1:1: boom");
        new Uint8Array(
          exports.memory.buffer,
          exports.alpha2_worklet_panic_message(),
          text.length,
        ).set(text);
        exports.alpha2_worklet_panic_length = () => text.length;
        throw new WebAssembly.RuntimeError("unreachable");
      };
      return { instance: { exports } };
    };
    let processor;
    try {
      processor = await readyProcessor(2, wasm);
    } finally {
      WebAssembly.instantiate = instantiate;
    }
    const reported = nextMessages(processor.hostPort);
    const stereo = [[new Float32Array(128), new Float32Array(128)]];
    assert.equal(processor.process([[]], stereo), false);
    const [message] = await reported;
    assert.equal(message?.type, "alpha2-error", "the trap must be reported");
    assert.match(message.message, /boom/);
  },
);

test(
  "the processor's ring reader carries out a clear the writer asked for",
  { timeout: 10_000 },
  async (t) => {
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    const { AUDIO_RING_CONTROL_BYTES, AudioRingBuffer } =
      await import("../dist/audio-timeline.js");
    const ring = AudioRingBuffer.attach({
      control: new SharedArrayBuffer(AUDIO_RING_CONTROL_BYTES),
      samples: new SharedArrayBuffer(256 * Float32Array.BYTES_PER_ELEMENT),
      channels: 1,
      capacity: 256,
    });
    const processor = await readyProcessor(1, wasm);
    processor.port.onmessage({
      data: { type: "alpha2-ring", ring: ring.transfer },
    });
    ring.write([new Float32Array(100).fill(0.25)], 100);
    ring.clear();
    ring.write([new Float32Array(128).fill(0.5)], 128);
    const output = [[new Float32Array(128)]];
    assert.equal(processor.process([[]], output), true);
    // The default master chain limits to 0 dBFS, which 0.5 is under.
    assert.ok(output[0][0].every((sample) => Math.abs(sample - 0.5) < 1e-6));
    assert.equal(ring.available, 0);
    assert.equal(ring.writable, 256);
  },
);

/** An `AudioWorkletNode` whose processor answers initialization at once. */
class FakeNode extends EventTarget {
  static created = [];
  constructor(_context, _name, options) {
    super();
    const pair = channel();
    this.port = pair.port1;
    this.processorPort = pair.port2;
    this.options = options;
    this.processorPort.onmessage = (event) => {
      if (event.data.type === "alpha2-init") {
        this.channels = event.data.channels;
        this.sourceRate = event.data.sourceRate;
        this.processorPort.postMessage({ type: "alpha2-ready" });
      }
    };
    FakeNode.created.push(this);
  }
  disconnect() {}
}
globalThis.AudioWorkletNode = FakeNode;
const audioWorklet = await import("../dist/audio-worklet.js");
const { Alpha2AudioWorkletHost } = audioWorklet;
const context = {
  sampleRate: 48_000,
  audioWorklet: { addModule: async () => {} },
};

test(
  "the node's output has the module's channel count unless asked otherwise",
  { timeout: 10_000 },
  async () => {
    const node = await Alpha2AudioWorkletHost.create(
      context,
      new ArrayBuffer(8),
      {
        onError: () => {},
      },
    );
    assert.deepEqual(node.options.outputChannelCount, [2]);
    assert.equal(node.channels, 2);
  },
);

test(
  "the node's input is mixed to the module's channels by the speaker rules",
  { timeout: 10_000 },
  async () => {
    for (const outputChannelCount of [undefined, [1]]) {
      const node = await Alpha2AudioWorkletHost.create(
        context,
        new ArrayBuffer(8),
        { outputChannelCount, onError: () => {} },
      );
      // "explicit" makes WebAudio up-mix a mono connection to both speakers and down-mix stereo
      // into a mono node; the default "max" hands the processor whatever is connected.
      assert.equal(node.options.channelCount, outputChannelCount?.[0] ?? 2);
      assert.equal(node.options.channelCountMode, "explicit");
      assert.equal(node.options.channelInterpretation, "speakers");
    }
  },
);

test(
  "input the node was not mixed for is folded as WebAudio's speaker rules fold it",
  { timeout: 10_000 },
  async (t) => {
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    for (const master of [undefined, { limiter: null }]) {
      // Mono input, as a node whose channel mode a host set back to "max" receives it.
      const stereo = await readyProcessor(2, wasm);
      if (master !== undefined) await configure(stereo, master);
      const both = [[new Float32Array(128), new Float32Array(128)]];
      assert.equal(
        stereo.process([[new Float32Array(128).fill(0.5)]], both),
        true,
      );
      assert.ok(both[0][0].every((sample) => Math.abs(sample - 0.5) < 1e-6));
      assert.ok(both[0][1].every((sample) => Math.abs(sample - 0.5) < 1e-6));

      // Stereo input into a mono node hears the average, not the left channel alone.
      const mono = await readyProcessor(1, wasm);
      if (master !== undefined) await configure(mono, master);
      const one = [[new Float32Array(128)]];
      const input = [[new Float32Array(128), new Float32Array(128).fill(0.5)]];
      assert.equal(mono.process(input, one), true);
      assert.ok(one[0][0].every((sample) => Math.abs(sample - 0.25) < 1e-6));
    }
  },
);

test(
  "a stereo ring read by a mono node is heard as the average of its channels",
  { timeout: 10_000 },
  async (t) => {
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    const ring = await ringOf(2);
    const processor = await readyProcessor(1, wasm);
    processor.port.onmessage({
      data: { type: "alpha2-ring", ring: ring.transfer },
    });
    await configure(processor, { limiter: null });
    ring.write([new Float32Array(128), new Float32Array(128).fill(0.5)], 128);
    const output = [[new Float32Array(128)]];
    assert.equal(processor.process([[]], output), true);
    assert.ok(output[0][0].every((sample) => sample === 0.25));

    ring.write(
      [new Float32Array(4).fill(0.5), new Float32Array(4).fill(-0.25)],
      4,
    );
    const out = [new Float32Array(4)];
    assert.equal(ring.read(out, 4), 4);
    assert.deepEqual([...out[0]], [0.125, 0.125, 0.125, 0.125]);
  },
);

test(
  "errors after initialization still reach the host",
  { timeout: 10_000 },
  async () => {
    const errors = [];
    const node = await Alpha2AudioWorkletHost.create(
      context,
      new ArrayBuffer(8),
      {
        onError: (error) => errors.push(error.message),
      },
    );
    node.dispatchEvent(new Event("processorerror"));
    node.processorPort.postMessage({
      type: "alpha2-error",
      message: "AudioWorklet processing failed",
    });
    // The port delivers on a later turn of the event loop, and a loaded machine may take longer
    // than any fixed wait.
    const deadline = Date.now() + 5_000;
    while (errors.length < 2 && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
    assert.equal(errors.length, 2);
    assert.match(errors[0], /processor/);
    assert.equal(errors[1], "AudioWorklet processing failed");
  },
);

/**
 * A ready processor whose module is the real one, with `alpha2_worklet_process` counted and the
 * instance's exports kept, so a test can grow its memory.
 */
async function instrumentedProcessor(channels, wasm) {
  const instantiate = WebAssembly.instantiate;
  const module = { calls: 0 };
  WebAssembly.instantiate = async (bytes, imports) => {
    const { instance } = await instantiate(bytes, imports);
    const exports = { ...instance.exports };
    exports.alpha2_worklet_process = (...args) => {
      module.calls += 1;
      return instance.exports.alpha2_worklet_process(...args);
    };
    module.exports = exports;
    return { instance: { exports } };
  };
  try {
    module.processor = await readyProcessor(channels, wasm);
  } finally {
    WebAssembly.instantiate = instantiate;
  }
  return module;
}

async function ringOf(channels) {
  const { AUDIO_RING_CONTROL_BYTES, AudioRingBuffer } =
    await import("../dist/audio-timeline.js");
  return AudioRingBuffer.attach({
    control: new SharedArrayBuffer(AUDIO_RING_CONTROL_BYTES),
    samples: new SharedArrayBuffer(
      channels * 4096 * Float32Array.BYTES_PER_ELEMENT,
    ),
    channels,
    capacity: 4096,
  });
}

async function configure(processor, master) {
  const { encodeMasterAudio } = await import("../dist/audio-timeline.js");
  processor.port.onmessage({
    data: { type: "alpha2-config", config: encodeMasterAudio(master) },
  });
}

/** Typed arrays made while `run` runs: constructions and `subarray` views. */
function typedArraysMadeDuring(run) {
  const made = { count: 0 };
  const constructors = [Float32Array, Uint8Array, Int32Array];
  const originals = constructors.map((constructor) => constructor);
  const subarray = Object.getPrototypeOf(Float32Array.prototype).subarray;
  for (const constructor of constructors) {
    globalThis[constructor.name] = new Proxy(constructor, {
      construct(target, args, newTarget) {
        made.count += 1;
        return Reflect.construct(target, args, newTarget);
      },
    });
  }
  Object.getPrototypeOf(Float32Array.prototype).subarray = function (...args) {
    made.count += 1;
    return subarray.apply(this, args);
  };
  try {
    run();
  } finally {
    constructors.forEach((constructor, index) => {
      globalThis[constructor.name] = originals[index];
    });
    Object.getPrototypeOf(Float32Array.prototype).subarray = subarray;
  }
  return made.count;
}

test(
  "a render quantum makes no typed arrays once the views exist",
  { timeout: 10_000 },
  async (t) => {
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    for (const transparent of [false, true]) {
      for (const fromRing of [false, true]) {
        const { processor } = await instrumentedProcessor(2, wasm);
        if (transparent) await configure(processor, { limiter: null });
        const ring = fromRing ? await ringOf(2) : undefined;
        if (ring !== undefined)
          processor.port.onmessage({
            data: { type: "alpha2-ring", ring: ring.transfer },
          });
        const block = new Float32Array(128).fill(0.25);
        const input = [[block, block]];
        const output = [[new Float32Array(128), new Float32Array(128)]];
        const quantum = () => {
          ring?.write([block, block], 128);
          assert.equal(processor.process(input, output), true);
        };
        quantum();
        const made = typedArraysMadeDuring(() => {
          for (let index = 0; index < 200; index += 1) quantum();
        });
        assert.equal(
          made,
          0,
          `transparent=${transparent} ring=${fromRing}: ${made} typed arrays in 200 quanta`,
        );
        assert.ok(output[0][0][0] > 0.2 && output[0][1][127] > 0.2);
      }
    }
  },
);

test(
  "the views follow wasm memory when it grows",
  { timeout: 10_000 },
  async (t) => {
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    const { processor, exports } = await instrumentedProcessor(2, wasm);
    const input = [[new Float32Array(128), new Float32Array(128)]];
    const output = [[new Float32Array(128), new Float32Array(128)]];
    input[0][0].fill(0.5);
    input[0][1].fill(-0.5);
    assert.equal(processor.process(input, output), true);
    const before = exports.memory.buffer;
    // Growing detaches the buffer every cached view was made over.
    exports.memory.grow(1);
    assert.notEqual(exports.memory.buffer, before);
    input[0][0].fill(0.25);
    input[0][1].fill(-0.25);
    assert.equal(processor.process(input, output), true);
    assert.ok(output[0][0].every((sample) => Math.abs(sample - 0.25) < 1e-6));
    assert.ok(output[0][1].every((sample) => Math.abs(sample + 0.25) < 1e-6));
  },
);

test(
  "a transparent master bypasses the module and copies its input exactly",
  { timeout: 10_000 },
  async (t) => {
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    const module = await instrumentedProcessor(2, wasm);
    const { processor } = module;
    const ring = await ringOf(2);
    processor.port.onmessage({
      data: { type: "alpha2-ring", ring: ring.transfer },
    });
    // Over full scale, so the default limiter would have changed it.
    const left = Float32Array.from({ length: 128 }, (_, i) => Math.sin(i) * 3);
    const right = Float32Array.from({ length: 128 }, (_, i) => -i / 64);
    const output = [[new Float32Array(128), new Float32Array(128)]];
    await configure(processor, { limiter: null });
    const calls = module.calls;
    ring.write([left, right], 128);
    assert.equal(processor.process([[]], output), true);
    assert.equal(module.calls, calls, "a transparent master skips the module");
    assert.deepEqual(output[0][0], left);
    assert.deepEqual(output[0][1], right);

    await configure(processor, {
      limiter: { threshold: 1, release_ms: 50 },
    });
    ring.write([left, right], 128);
    assert.equal(processor.process([[]], output), true);
    assert.equal(module.calls, calls + 1);
    assert.ok(output[0][0].every((sample) => Math.abs(sample) <= 1));
  },
);

/** Resolves after `ms` milliseconds. */
const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** `frames` distinct samples under full scale, from `seed`. */
function signal(frames, seed) {
  return Float32Array.from(
    { length: frames },
    (_, index) => Math.sin(index * 0.37 + seed) * 0.5,
  );
}

test(
  "a sample from WebAudio that is not a number plays as silence",
  { timeout: 10_000 },
  async (t) => {
    // The input path must zero it as the ring reader does, or a transparent master copies it out.
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    for (const master of [{ limiter: null }, undefined]) {
      const processor = await readyProcessor(2, wasm);
      if (master !== undefined) await configure(processor, master);
      const left = signal(128, 1);
      const right = signal(128, 2);
      left[5] = Number.NaN;
      right[9] = Number.POSITIVE_INFINITY;
      const output = [[new Float32Array(128), new Float32Array(128)]];
      assert.equal(processor.process([[left, right]], output), true);
      assert.ok(
        output[0].every((plane) => plane.every(Number.isFinite)),
        `master ${JSON.stringify(master)}`,
      );
      assert.equal(output[0][0][5], 0);
      assert.equal(output[0][1][9], 0);
      assert.ok(Math.abs(output[0][0][6] - left[6]) < 1e-6);
    }
  },
);

test(
  "a context's render quantum sizes the module, one call per quantum",
  { timeout: 10_000 },
  async (t) => {
    // A context made with `renderSizeHint: 256` hands every processor 256 frames.
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    globalThis.renderQuantumSize = 256;
    let module;
    try {
      module = await instrumentedProcessor(2, wasm);
    } finally {
      delete globalThis.renderQuantumSize;
    }
    const { processor } = module;
    const ring = await ringOf(2);
    processor.port.onmessage({
      data: { type: "alpha2-ring", ring: ring.transfer },
    });
    const left = signal(256, 1);
    const right = signal(256, 2);
    ring.write([left, right], 256);
    const output = [[new Float32Array(256), new Float32Array(256)]];
    const calls = module.calls;
    assert.equal(processor.process([[]], output), true);
    assert.equal(module.calls, calls + 1);
    assert.equal(ring.available, 0, "the whole quantum came from the ring");
    assert.deepEqual(output[0][0], left);
    assert.deepEqual(output[0][1], right);
  },
);

test(
  "a quantum longer than the module runs through it in pieces",
  { timeout: 10_000 },
  async (t) => {
    // The spec allows a quantum up to six times the rate, past the module's 4096-frame limit; a
    // scope that does not say its quantum gets the 128-frame module.
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    for (const transparent of [false, true]) {
      const processor = await readyProcessor(2, wasm);
      if (transparent) await configure(processor, { limiter: null });
      const left = signal(5_000, 1);
      const right = signal(5_000, 2);
      const output = [[new Float32Array(5_000), new Float32Array(5_000)]];
      const reported = nextMessages(processor.hostPort);
      assert.equal(processor.process([[left, right]], output), true);
      assert.deepEqual(output[0][0], left, `transparent=${transparent}`);
      assert.deepEqual(output[0][1], right, `transparent=${transparent}`);
      assert.deepEqual(await reported, [], "nothing to report");
    }
  },
);

test(
  "a reset forgets the limiter's envelope",
  { timeout: 10_000 },
  async (t) => {
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    const quiet = new Float32Array(128).fill(0.25);
    const loud = new Float32Array(128).fill(1);
    const run = (processor, block) => {
      const output = [[new Float32Array(128)]];
      assert.equal(processor.process([[block]], output), true);
      return output[0][0];
    };
    for (const reset of [false, true]) {
      const processor = await readyProcessor(1, wasm);
      await configure(processor, {
        limiter: { threshold: 0.5, release_ms: 50 },
      });
      run(processor, loud);
      if (reset) processor.port.onmessage({ data: { type: "alpha2-reset" } });
      const first = run(processor, quiet)[0];
      if (reset) assert.equal(first, 0.25, "a reset starts the envelope over");
      else assert.ok(first < 0.2, "the envelope carries without a reset");
    }
  },
);

test(
  "a disposed processor frees its module and retires the node",
  { timeout: 10_000 },
  async (t) => {
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    const module = await instrumentedProcessor(2, wasm);
    const { processor, exports } = module;
    let destroyed = 0;
    const destroy = exports.alpha2_worklet_destroy;
    exports.alpha2_worklet_destroy = (handle) => {
      destroyed += 1;
      return destroy(handle);
    };
    const output = [[new Float32Array(128), new Float32Array(128)]];
    assert.equal(processor.process([[]], output), true);
    processor.port.onmessage({ data: { type: "alpha2-dispose" } });
    assert.equal(destroyed, 1);
    assert.equal(processor.process([[]], output), false);
    await configure(processor, undefined);
    processor.port.onmessage({ data: { type: "alpha2-reset" } });
    assert.equal(processor.process([[]], output), false);
  },
);

test(
  "a processor disposed while it starts never starts",
  { timeout: 10_000 },
  async (t) => {
    // The host gave up (timeout, abort) while the module compiled.
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    const instantiate = WebAssembly.instantiate;
    let created = 0;
    WebAssembly.instantiate = async (bytes, imports) => {
      const { instance } = await instantiate(bytes, imports);
      const exports = { ...instance.exports };
      exports.alpha2_worklet_create = (...args) => {
        created += 1;
        return instance.exports.alpha2_worklet_create(...args);
      };
      return { instance: { exports } };
    };
    try {
      const processor = new Processor();
      const seen = [];
      processor.hostPort.onmessage = (event) => seen.push(event.data.type);
      processor.port.onmessage({
        data: { type: "alpha2-init", wasm: wasm.slice(0), channels: 2 },
      });
      processor.port.onmessage({ data: { type: "alpha2-dispose" } });
      await delay(300);
      assert.equal(created, 0, "no DSP instance was made");
      assert.ok(!seen.includes("alpha2-ready"), seen.join());
      const output = [[new Float32Array(128), new Float32Array(128)]];
      assert.equal(processor.process([[]], output), false);
    } finally {
      WebAssembly.instantiate = instantiate;
    }
  },
);

test(
  "a processor that fails to start retires itself",
  { timeout: 10_000 },
  async (t) => {
    // The host rejects `create` and drops the node, so nothing would ever stop it.
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    await assert.rejects(readyProcessor(3, wasm), /channel configuration/);
    const processor = new Processor();
    const failed = new Promise((resolve) => {
      processor.hostPort.onmessage = (event) => {
        if (event.data.type === "alpha2-error") resolve(event.data.message);
      };
    });
    processor.port.onmessage({
      data: { type: "alpha2-init", wasm: wasm.slice(0), channels: 3 },
    });
    assert.match(await failed, /channel configuration/);
    const output = [[new Float32Array(128), new Float32Array(128)]];
    assert.equal(processor.process([[]], output), false);
  },
);

test(
  "a node whose start fails is told to stop before its port closes",
  { timeout: 10_000 },
  async () => {
    class FailingNode extends FakeNode {
      constructor(...args) {
        super(...args);
        this.received = [];
        this.disconnected = false;
        this.processorPort.onmessage = (event) => {
          this.received.push(event.data.type);
          if (event.data.type === "alpha2-init")
            this.processorPort.postMessage({
              type: "alpha2-error",
              message: "no module",
            });
        };
      }
      disconnect() {
        this.disconnected = true;
      }
    }
    globalThis.AudioWorkletNode = FailingNode;
    try {
      await assert.rejects(
        Alpha2AudioWorkletHost.create(context, new ArrayBuffer(8), {}),
        /no module/,
      );
    } finally {
      globalThis.AudioWorkletNode = FakeNode;
    }
    const node = FakeNode.created.at(-1);
    assert.ok(node instanceof FailingNode);
    await delay(50);
    assert.deepEqual(node.received, ["alpha2-init", "alpha2-dispose"]);
    assert.ok(node.disconnected);
  },
);

test("disposeAudioWorklet stops the processor and disconnects the node", () => {
  const { disposeAudioWorklet } = audioWorklet;
  const posted = [];
  let disconnected = false;
  disposeAudioWorklet({
    port: { postMessage: (message) => posted.push(message) },
    disconnect: () => {
      disconnected = true;
    },
  });
  assert.deepEqual(posted, [{ type: "alpha2-dispose" }]);
  assert.ok(disconnected);
});

// The host's timers are replaced while it starts (see `withTimers`); waits use the real ones.
const realSetTimeout = globalThis.setTimeout;
const pause = (ms) => new Promise((resolve) => realSetTimeout(resolve, ms));

/** Waits until `ready()` holds, or 5 s pass: a port delivers on a later turn of the event loop. */
async function eventually(ready) {
  const deadline = Date.now() + 5_000;
  while (!ready() && Date.now() < deadline) await pause(5);
}

/**
 * A node whose processor never answers `alpha2-init` unless `answer` says how, recording the
 * listeners it holds, what its processor receives, and whether it was disconnected and closed.
 */
class SilentNode extends FakeNode {
  static answer = undefined;
  constructor(...args) {
    super(...args);
    this.received = [];
    this.disconnected = false;
    this.closed = false;
    this.listeners = new Map();
    const track = (target, key) => {
      const add = target.addEventListener.bind(target);
      const remove = target.removeEventListener.bind(target);
      target.addEventListener = (type, listener, options) => {
        this.#held(`${key}:${type}`).add(listener);
        add(type, listener, options);
      };
      target.removeEventListener = (type, listener, options) => {
        this.#held(`${key}:${type}`).delete(listener);
        remove(type, listener, options);
      };
    };
    track(this, "node");
    track(this.port, "port");
    const close = this.port.close.bind(this.port);
    this.port.close = () => {
      this.closed = true;
      close();
    };
    const answer = SilentNode.answer;
    this.processorPort.onmessage = (event) => {
      this.received.push(event.data.type);
      if (event.data.type === "alpha2-init") answer?.(this);
    };
  }
  #held(key) {
    if (!this.listeners.has(key)) this.listeners.set(key, new Set());
    return this.listeners.get(key);
  }
  held(key) {
    return this.listeners.get(key)?.size ?? 0;
  }
  disconnect() {
    this.disconnected = true;
  }
}

/** Replaces the timer functions while `run` runs, recording the timers set and cleared. */
async function withTimers(run) {
  const real = { set: globalThis.setTimeout, clear: globalThis.clearTimeout };
  const timers = { set: [], cleared: new Set() };
  globalThis.setTimeout = (callback, ms) => {
    timers.set.push({ callback, ms });
    return timers.set.length;
  };
  globalThis.clearTimeout = (id) => timers.cleared.add(id);
  try {
    return await run(timers);
  } finally {
    globalThis.setTimeout = real.set;
    globalThis.clearTimeout = real.clear;
  }
}

/**
 * Runs `create` against a {@link SilentNode} that answers as `answer` says, and checks what every
 * failed start must leave behind: the initialization timer cleared, no listener left on the node,
 * its port or `signal`, and the processor told to stop, the node disconnected and its port closed.
 */
async function failedStart({
  answer,
  signal,
  act,
  expected,
  Node = SilentNode,
  received = ["alpha2-init", "alpha2-dispose"],
}) {
  SilentNode.answer = answer;
  globalThis.AudioWorkletNode = Node;
  let signalListeners = 0;
  if (signal !== undefined) {
    const add = signal.addEventListener.bind(signal);
    const remove = signal.removeEventListener.bind(signal);
    signal.addEventListener = (...args) => {
      signalListeners += 1;
      add(...args);
    };
    signal.removeEventListener = (...args) => {
      signalListeners -= 1;
      remove(...args);
    };
  }
  try {
    await withTimers(async (timers) => {
      const created = Alpha2AudioWorkletHost.create(
        context,
        new ArrayBuffer(8),
        { signal, onError: () => {} },
      );
      created.catch(() => {});
      await new Promise(setImmediate);
      const node = FakeNode.created.at(-1);
      assert.ok(node instanceof Node);
      await act?.(node, timers);
      await assert.rejects(created, expected);
      assert.equal(timers.set.length, 1, "one initialization timer");
      assert.equal(timers.set[0].ms, 5_000);
      assert.ok(timers.cleared.has(1), "the timer is cleared");
      assert.equal(node.held("port:message"), 0, "no message listener left");
      assert.equal(node.held("node:processorerror"), 0, "no error listener");
      assert.equal(signalListeners, 0, "no abort listener left");
      assert.ok(node.disconnected, "the node is disconnected");
      assert.ok(node.closed, "its port is closed");
    });
  } finally {
    globalThis.AudioWorkletNode = FakeNode;
    SilentNode.answer = undefined;
  }
  const node = FakeNode.created.at(-1);
  await eventually(() => node.received.includes("alpha2-dispose"));
  assert.deepEqual(node.received, received);
}

test(
  "a processor that never answers times out after 5 s, naming its last stage",
  { timeout: 10_000 },
  async () => {
    await failedStart({
      answer: (node) =>
        node.processorPort.postMessage({
          type: "alpha2-stage",
          stage: "init-received",
        }),
      act: async (node, timers) => {
        await eventually(() => node.received.includes("alpha2-init"));
        // Let the stage reach the host before the timer fires.
        await pause(50);
        timers.set[0].callback();
      },
      expected: /timed out \(last stage: init-received\)/,
    });
  },
);

test(
  "a start aborted while the processor starts rejects with the reason",
  { timeout: 10_000 },
  async () => {
    const controller = new AbortController();
    const reason = new Error("the user left the page");
    await failedStart({
      signal: controller.signal,
      act: async (node) => {
        await eventually(() => node.received.includes("alpha2-init"));
        controller.abort(reason);
      },
      expected: (error) => error === reason,
    });
  },
);

test(
  "a processor that throws while it starts fails the start",
  { timeout: 10_000 },
  async () => {
    await failedStart({
      act: async (node) => {
        await eventually(() => node.received.includes("alpha2-init"));
        node.dispatchEvent(new Event("processorerror"));
      },
      expected: /processor failed during initialization/,
    });
  },
);

test(
  "a processor that reports an error while it starts fails the start",
  { timeout: 10_000 },
  async () => {
    await failedStart({
      answer: (node) =>
        node.processorPort.postMessage({
          type: "alpha2-error",
          message: "no module",
        }),
      expected: /no module/,
    });
  },
);

test(
  "an initialization message the port refuses fails the start",
  { timeout: 10_000 },
  async () => {
    const refused = new DOMException("could not be cloned", "DataCloneError");
    class RefusingNode extends SilentNode {
      constructor(...args) {
        super(...args);
        const post = this.port.postMessage.bind(this.port);
        this.port.postMessage = (message, transfer) => {
          if (message.type === "alpha2-init") throw refused;
          post(message, transfer);
        };
      }
    }
    await failedStart({
      Node: RefusingNode,
      expected: (error) => error === refused,
      received: ["alpha2-dispose"],
    });
  },
);

test(
  "a signal aborted before the start creates no node",
  { timeout: 10_000 },
  async () => {
    const before = FakeNode.created.length;
    let added = 0;
    const counting = {
      sampleRate: 48_000,
      audioWorklet: {
        addModule: async () => {
          added += 1;
        },
      },
    };
    const reason = new Error("gone");
    await assert.rejects(
      Alpha2AudioWorkletHost.create(counting, new ArrayBuffer(8), {
        signal: AbortSignal.abort(reason),
      }),
      (error) => error === reason,
    );
    assert.equal(added, 0);
    assert.equal(FakeNode.created.length, before);
  },
);

/** Collects the type of every message `processor` posts to its host. */
function postedTypes(processor) {
  const types = [];
  processor.hostPort.onmessage = (event) => types.push(event.data.type);
  return types;
}

test(
  "a ring underrun is reported once per run of short quanta, and again after a recovery, a reset or a new ring",
  { timeout: 10_000 },
  async (t) => {
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    const processor = await readyProcessor(1, wasm);
    await configure(processor, { limiter: null });
    const ring = await ringOf(1);
    processor.port.onmessage({
      data: { type: "alpha2-ring", ring: ring.transfer },
    });
    const types = postedTypes(processor);
    const underruns = async () => {
      // A port delivers in order: what the processor posted has arrived once a later marker has.
      processor.port.postMessage({ type: "marker" });
      await eventually(() => types.includes("marker"));
      types.splice(types.indexOf("marker"), 1);
      return types.filter((type) => type === "alpha2-underrun").length;
    };
    const quantum = (frames) => {
      if (frames > 0) ring.write([new Float32Array(frames).fill(0.5)], frames);
      const output = [[new Float32Array(128)]];
      assert.equal(processor.process([[]], output), true);
    };
    quantum(0);
    quantum(64);
    quantum(0);
    assert.equal(await underruns(), 1, "one report for a run of three");
    quantum(128);
    quantum(0);
    assert.equal(await underruns(), 2, "a full quantum ends the run");
    processor.port.onmessage({ data: { type: "alpha2-reset" } });
    quantum(0);
    assert.equal(await underruns(), 3, "a reset starts a new run");
    const next = await ringOf(1);
    processor.port.onmessage({
      data: { type: "alpha2-ring", ring: next.transfer },
    });
    const output = [[new Float32Array(128)]];
    assert.equal(processor.process([[]], output), true);
    assert.equal(await underruns(), 4, "a new ring starts a new run");
  },
);

test(
  "a node told to drop its ring plays its WebAudio input again and reports no underrun",
  { timeout: 10_000 },
  async (t) => {
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    const processor = await readyProcessor(1, wasm);
    await configure(processor, { limiter: null });
    const ring = await ringOf(1);
    processor.port.onmessage({
      data: { type: "alpha2-ring", ring: ring.transfer },
    });
    ring.write([new Float32Array(128).fill(0.25)], 128);
    const input = [[new Float32Array(128).fill(0.5)]];
    const output = [[new Float32Array(128)]];
    assert.equal(processor.process(input, output), true);
    assert.ok(
      output[0][0].every((sample) => sample === 0.25),
      "the ring",
    );
    const types = postedTypes(processor);
    processor.port.onmessage({
      data: { type: "alpha2-ring", ring: undefined },
    });
    for (let quantum = 0; quantum < 3; quantum += 1) {
      assert.equal(processor.process(input, output), true);
      assert.ok(
        output[0][0].every((sample) => sample === 0.5),
        "the input",
      );
    }
    await pause(50);
    assert.deepEqual(types, []);
  },
);

test(
  "a master that delays its input reports by how much, and its output is that late",
  { timeout: 10_000 },
  async (t) => {
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    for (const [lookahead_ms, expected] of [
      [10, 480],
      [50, 2_400],
    ]) {
      const processor = await readyProcessor(1, wasm);
      const posted = [];
      processor.hostPort.onmessage = (event) => posted.push(event.data);
      await configure(processor, {
        effects: [
          {
            type: "compressor",
            threshold_db: 0,
            ratio: 2,
            attack_ms: 10,
            release_ms: 100,
            lookahead_ms,
          },
        ],
      });
      await eventually(() =>
        posted.some((message) => message.type === "alpha2-latency"),
      );
      assert.deepEqual(
        posted.filter((message) => message.type === "alpha2-latency"),
        [{ type: "alpha2-latency", latencySamples: expected }],
      );
      // An impulse through the node's input comes out `expected` samples later.
      const heard = [];
      for (let quantum = 0; quantum < 24; quantum += 1) {
        const input = new Float32Array(128);
        if (quantum === 0) input[0] = 0.5;
        const output = [[new Float32Array(128)]];
        assert.equal(processor.process([[input]], output), true);
        heard.push(...output[0][0]);
      }
      assert.equal(
        heard.findIndex((sample) => Math.abs(sample) > 0.25),
        expected,
      );
      await configure(processor, { limiter: null });
      await eventually(() => posted.length > 1);
      assert.deepEqual(posted.at(-1), {
        type: "alpha2-latency",
        latencySamples: 0,
      });
    }
  },
);

test(
  "the host keeps the delay its node's processor reports",
  { timeout: 10_000 },
  async () => {
    const node = await Alpha2AudioWorkletHost.create(
      context,
      new ArrayBuffer(8),
      { onError: () => {} },
    );
    const delay = Alpha2AudioWorkletHost.latency(node);
    assert.equal(delay.latencySamples, 0);
    node.processorPort.postMessage({
      type: "alpha2-latency",
      latencySamples: 480,
    });
    await eventually(() => delay.latencySamples === 480);
    assert.equal(delay.latencySamples, 480);
    assert.equal(Alpha2AudioWorkletHost.latency(node).latencySamples, 480);
    assert.equal(
      Alpha2AudioWorkletHost.latency(new FakeNode(context, "x", {}))
        .latencySamples,
      0,
    );
  },
);

test(
  "a clock given the node's delay says the sample being heard when a worklet master delays it",
  { timeout: 10_000 },
  async (t) => {
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    const { AudioPlaybackClock, AudioPump } =
      await import("../dist/audio-timeline.js");
    const processor = await readyProcessor(1, wasm);
    const delay = { latencySamples: 0 };
    processor.hostPort.onmessage = (event) => {
      if (event.data.type === "alpha2-latency")
        delay.latencySamples = event.data.latencySamples;
    };
    await configure(processor, {
      effects: [
        {
          type: "compressor",
          threshold_db: 0,
          ratio: 2,
          attack_ms: 10,
          release_ms: 100,
          lookahead_ms: 10,
        },
      ],
    });
    await eventually(() => delay.latencySamples === 480);
    const ring = await ringOf(1);
    processor.port.onmessage({
      data: { type: "alpha2-ring", ring: ring.transfer },
    });
    const impulse = 1_000;
    const pump = new AudioPump(
      {
        sampleRate: 48_000,
        channels: 1,
        mix: async (start, frames) => ({
          kind: "mixed",
          planes: [
            Float32Array.from({ length: frames }, (_, index) =>
              start + index === impulse ? 0.5 : 0,
            ),
          ],
        }),
      },
      ring,
      0,
      { blockFrames: 512, targetFrames: 4096 },
    );
    await pump.fill();
    const clock = new AudioPlaybackClock(
      pump,
      ring,
      { sampleRate: 48_000 },
      delay,
    );
    let heardAt;
    for (let quantum = 0; quantum < 24 && heardAt === undefined; quantum += 1) {
      // What the clock says is heard at the start of this quantum's output.
      const now = clock.sample;
      const output = [[new Float32Array(128)]];
      assert.equal(processor.process([[]], output), true);
      const index = output[0][0].findIndex((sample) => Math.abs(sample) > 0.25);
      if (index >= 0) heardAt = now + index;
    }
    assert.equal(heardAt, impulse);
  },
);

test(
  "a context at another rate than the project's converts the ring and says so",
  { timeout: 10_000 },
  async () => {
    const at44 = { ...context, sampleRate: 44_100 };
    const diagnostics = [];
    const node = await Alpha2AudioWorkletHost.create(at44, new ArrayBuffer(8), {
      projectSampleRate: 48_000,
      onError: () => {},
      onDiagnostic: (diagnostic) => diagnostics.push(diagnostic),
    });
    assert.equal(node.sourceRate, 48_000);
    assert.deepEqual(diagnostics, [
      {
        kind: "resampled",
        projectSampleRate: 48_000,
        contextSampleRate: 44_100,
      },
    ]);
    // At the project's rate, or without one (the context's), nothing is converted or reported.
    for (const projectSampleRate of [44_100, undefined]) {
      const same = await Alpha2AudioWorkletHost.create(
        at44,
        new ArrayBuffer(8),
        {
          projectSampleRate,
          onError: () => {},
          onDiagnostic: (d) => diagnostics.push(d),
        },
      );
      assert.equal(same.sourceRate, 44_100);
    }
    assert.equal(diagnostics.length, 1);
    for (const projectSampleRate of [0, -48_000, 44_100.5, Number.NaN])
      await assert.rejects(
        Alpha2AudioWorkletHost.create(at44, new ArrayBuffer(8), {
          projectSampleRate,
        }),
        RangeError,
      );
  },
);

/**
 * The frequency of a tone from its first and last rising zero crossings, placed between samples.
 */
function frequency(samples, rate) {
  const crossings = [];
  for (let index = 1; index < samples.length; index += 1)
    if (samples[index - 1] < 0 && samples[index] >= 0)
      crossings.push(
        index - samples[index] / (samples[index] - samples[index - 1]),
      );
  return ((crossings.length - 1) * rate) / (crossings.at(-1) - crossings[0]);
}

/**
 * Plays `seconds` of a ring the pump fills with `mix` (at 48 kHz) through a processor in a context
 * at `contextRate`, one 128-sample quantum at a time, calling `onQuantum(clockSample, output)`.
 */
async function playAcrossRates(
  wasm,
  contextRate,
  mix,
  seconds,
  onQuantum = () => {},
) {
  const { AudioPlaybackClock, AudioPump } =
    await import("../dist/audio-timeline.js");
  globalThis.sampleRate = contextRate;
  try {
    const processor = await readyProcessor(1, wasm, 48_000);
    const delay = { latencySamples: 0 };
    processor.hostPort.onmessage = (event) => {
      if (event.data.type === "alpha2-latency")
        delay.latencySamples = event.data.latencySamples;
    };
    // Engine-mixed audio: the master is transparent, and the ring is all there is to convert.
    await configure(processor, { limiter: null });
    const ring = await ringOf(1);
    processor.port.onmessage({
      data: { type: "alpha2-ring", ring: ring.transfer },
    });
    const pump = new AudioPump(
      {
        sampleRate: 48_000,
        channels: 1,
        mix: async (start, frames) => ({
          kind: "mixed",
          planes: [
            Float32Array.from({ length: frames }, (_, index) =>
              mix(start + index),
            ),
          ],
        }),
      },
      ring,
      0,
      { blockFrames: 512, targetFrames: 2048 },
    );
    await pump.fill();
    await eventually(() => delay.latencySamples > 0);
    const clock = new AudioPlaybackClock(
      pump,
      ring,
      { sampleRate: contextRate },
      delay,
    );
    const played = [];
    for (let quantum = 0; quantum * 128 < seconds * contextRate; quantum += 1) {
      const now = clock.sample;
      const output = [[new Float32Array(128)]];
      assert.equal(processor.process([[]], output), true);
      onQuantum(now, output[0][0]);
      played.push(...output[0][0]);
      await pump.fill();
    }
    return { played, pump, ring, delay };
  } finally {
    globalThis.sampleRate = 48_000;
  }
}

test(
  "a ring at the project's rate plays at its own speed and pitch in a context at another rate",
  { timeout: 20_000 },
  async (t) => {
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    // A 1 kHz tone mixed at 48 kHz, heard in a 44.1 kHz context (one that did not grant the
    // project's rate) and a 96 kHz one.
    for (const contextRate of [44_100, 96_000]) {
      const { played, pump, ring, delay } = await playAcrossRates(
        wasm,
        contextRate,
        (sample) => Math.sin((2 * Math.PI * 1_000 * sample) / 48_000) * 0.5,
        1,
      );
      const heard = frequency(played.slice(contextRate / 10), contextRate);
      assert.ok(
        Math.abs(heard - 1_000) < 0.05,
        `a 1 kHz tone is heard at ${heard} Hz in a ${contextRate} Hz context`,
      );
      const taken = pump.position - ring.available;
      const seconds = played.length / contextRate;
      assert.ok(
        Math.abs(taken - seconds * 48_000) < 64,
        `${taken} project samples taken in ${seconds} s`,
      );
      assert.ok(delay.latencySamples > 0, "the conversion's delay is reported");
    }
  },
);

test(
  "the playback clock says the project sample heard when the context runs at another rate",
  { timeout: 20_000 },
  async (t) => {
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    // An impulse at project sample 10 100 (0.21 s), late in a quantum, heard in a 44.1 kHz context:
    // the clock, given the node's delay, names that sample when it comes out.
    const impulse = 10_100;
    let heardAt;
    await playAcrossRates(
      wasm,
      44_100,
      (sample) => (sample === impulse ? 0.9 : 0),
      0.3,
      (now, output) => {
        if (heardAt !== undefined) return;
        let peak = 0;
        let at = -1;
        output.forEach((sample, index) => {
          if (Math.abs(sample) > peak) [peak, at] = [Math.abs(sample), index];
        });
        if (peak > 0.3) heardAt = now + (at * 48_000) / 44_100;
      },
    );
    assert.ok(heardAt !== undefined, "the impulse was heard");
    // Within three project samples (0.06 ms): the clock counts whole samples, the delay comes in
    // whole samples of the context's, and the peak falls on the context sample nearest the impulse.
    assert.ok(
      Math.abs(heardAt - impulse) <= 3,
      `the clock said ${heardAt} when sample ${impulse} was heard`,
    );
  },
);

/** An `AudioWorkletNode` whose processor is the real one, its ports linked to the node's. */
class ProcessorNode extends EventTarget {
  constructor() {
    super();
    this.processor = new Processor();
    this.port = this.processor.hostPort;
  }
  disconnect() {}
}

/** A node `create` made whose processor is the real one running the real DSP module. */
async function processorNode(wasm, options = {}) {
  globalThis.AudioWorkletNode = ProcessorNode;
  try {
    return await Alpha2AudioWorkletHost.create(context, wasm, {
      onError: () => {},
      ...options,
    });
  } finally {
    globalThis.AudioWorkletNode = FakeNode;
  }
}

test(
  "a configuration has reached the processor once its promise resolves",
  { timeout: 10_000 },
  async (t) => {
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    const {
      configureEngineMixedAudio,
      configureMasterAudio,
      resetAudioWorklet,
      useAudioRing,
    } = audioWorklet;
    const node = await processorNode(wasm, { outputChannelCount: [1] });
    const ring = await ringOf(1);
    ring.write([new Float32Array(128).fill(0.5)], 128);
    // An `OfflineAudioContext` may render its first quantum before a message posted the turn before
    // is delivered (Safari does), so the configuration must be awaited.
    await useAudioRing(node, ring.transfer);
    await configureEngineMixedAudio(node);
    const output = [[new Float32Array(128)]];
    assert.equal(node.processor.process([[]], output), true);
    assert.ok(
      output[0][0].every((sample) => sample === 0.5),
      "the first quantum after the promises resolve plays the ring",
    );
    await configureMasterAudio(node, {
      effects: [
        {
          type: "compressor",
          threshold_db: 0,
          ratio: 2,
          attack_ms: 10,
          release_ms: 100,
          lookahead_ms: 10,
        },
      ],
    });
    assert.equal(Alpha2AudioWorkletHost.latency(node).latencySamples, 480);
    await configureEngineMixedAudio(node);
    assert.equal(Alpha2AudioWorkletHost.latency(node).latencySamples, 0);
    await resetAudioWorklet(node);
    await useAudioRing(node, undefined);
    const input = [[new Float32Array(128).fill(0.25)]];
    assert.equal(node.processor.process(input, output), true);
    assert.ok(
      output[0][0].every((sample) => sample === 0.25),
      "back on its input once the ring is dropped",
    );
  },
);

test(
  "a configuration the processor refuses rejects its promise instead of reaching onError",
  { timeout: 10_000 },
  async (t) => {
    const wasm = await workletWasm();
    if (wasm === undefined) return t.skip("the worklet wasm is not built");
    const processor = await readyProcessor(1, wasm);
    const posted = [];
    processor.hostPort.onmessage = (event) => posted.push(event.data);
    processor.port.onmessage({
      data: {
        type: "alpha2-config",
        id: 7,
        config: new Uint8Array(1 << 20),
      },
    });
    await eventually(() => posted.length > 0);
    assert.equal(posted.length, 1);
    assert.equal(posted[0].type, "alpha2-applied");
    assert.equal(posted[0].id, 7);
    assert.match(posted[0].error, /exceeds/);

    // The host rejects that request's promise with it; onError hears nothing of it.
    const errors = [];
    const node = await Alpha2AudioWorkletHost.create(
      context,
      new ArrayBuffer(8),
      { onError: (error) => errors.push(error) },
    );
    node.processorPort.onmessage = (event) => {
      if (event.data.type === "alpha2-config")
        node.processorPort.postMessage({
          type: "alpha2-applied",
          id: event.data.id,
          error: "master configuration was rejected (3)",
        });
    };
    await assert.rejects(
      audioWorklet.configureEngineMixedAudio(node),
      /rejected \(3\)/,
    );
    await pause(20);
    assert.deepEqual(errors, []);
  },
);

test(
  "a keyframed master gain rejects the configuration with a RangeError and posts nothing",
  { timeout: 10_000 },
  async () => {
    const posted = [];
    const node = {
      port: { postMessage: (message) => posted.push(message) },
    };
    const configured = audioWorklet.configureMasterAudio(node, {
      gain: {
        mode: "keyframes",
        keyframes: [
          { time: { num: 0, den: 1 }, value: 1, easing: "linear" },
          { time: { num: 1, den: 1 }, value: 0, easing: "linear" },
        ],
      },
    });
    assert.ok(configured instanceof Promise);
    await assert.rejects(configured, RangeError);
    assert.deepEqual(posted, []);
  },
);

test(
  "a disposed or failed node rejects the configurations it will never answer",
  { timeout: 10_000 },
  async () => {
    const { disposeAudioWorklet, resetAudioWorklet, useAudioRing } =
      audioWorklet;
    // The fake processor never answers a configuration.
    const disposed = await Alpha2AudioWorkletHost.create(
      context,
      new ArrayBuffer(8),
      { onError: () => {} },
    );
    const pending = resetAudioWorklet(disposed);
    disposeAudioWorklet(disposed);
    await assert.rejects(pending, /disposed/);
    await assert.rejects(useAudioRing(disposed, undefined), /disposed/);

    const failed = await Alpha2AudioWorkletHost.create(
      context,
      new ArrayBuffer(8),
      { onError: () => {} },
    );
    const waiting = resetAudioWorklet(failed);
    failed.dispatchEvent(new Event("processorerror"));
    await assert.rejects(waiting, /processor threw/);
    await assert.rejects(resetAudioWorklet(failed), /processor threw/);
  },
);

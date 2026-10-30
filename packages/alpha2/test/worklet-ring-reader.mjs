// Loads the built worklet processor outside a worklet and drives its ring reader through
// `process()`, so a test can run it next to `AudioRingBuffer.read`.

import assert from "node:assert/strict";
import { existsSync, statSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { join, resolve } from "node:path";

import { recordSkip, suiteOf } from "../../../scripts/lib/capabilities.mjs";

const root = resolve(import.meta.dirname, "..", "..", "..");

/**
 * The newer of the packaged DSP module and the plain cargo build CI makes before these tests, so a
 * stale package never stands in for the source. Without either it returns undefined and the
 * caller skips, and the skip is recorded (a failure when ALPHA2_REQUIRE names wasm).
 */
export async function workletWasm() {
  const newest = [
    join(root, "packages", "alpha2", "wasm", "alpha2_audio_worklet.wasm"),
    join(
      root,
      "target",
      "wasm32-unknown-unknown",
      "debug",
      "alpha2_audio_worklet.wasm",
    ),
  ]
    .filter((path) => existsSync(path))
    .sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs)[0];
  if (newest !== undefined) return (await readFile(newest)).buffer;
  // Throws when the run requires the wasm.
  recordSkip({
    suite: suiteOf(import.meta.url),
    capability: "wasm",
    reason: "the worklet wasm is not built",
  });
  return undefined;
}

let processorClass;

/**
 * The processor class, registered against stand-ins for the AudioWorkletGlobalScope globals. Its
 * port is a plain object: `onmessage` is called directly and posts are kept in `posted`.
 */
async function processorConstructor() {
  if (processorClass !== undefined) return processorClass;
  globalThis.sampleRate = 48_000;
  globalThis.AudioWorkletProcessor = class {
    constructor() {
      this.posted = [];
      this.port = {
        onmessage: null,
        postMessage: (message) => this.posted.push(message),
      };
    }
  };
  globalThis.registerProcessor = (_name, constructor) => {
    processorClass = constructor;
  };
  await import("../dist/audio-worklet-processor.js");
  return processorClass;
}

/**
 * A ready processor reading `transfer` with a transparent master, so each sample it plays is the
 * ring's sample exactly. `read(planes)` runs one quantum the length of the planes.
 */
export async function workletRingReader(transfer, wasm) {
  const Processor = await processorConstructor();
  const processor = new Processor();
  processor.port.onmessage({
    data: {
      type: "alpha2-init",
      wasm: wasm.slice(0),
      channels: transfer.channels,
    },
  });
  const deadline = Date.now() + 10_000;
  for (;;) {
    const answer = processor.posted.find(
      (message) =>
        message.type === "alpha2-ready" || message.type === "alpha2-error",
    );
    if (answer?.type === "alpha2-error") throw new Error(answer.message);
    if (answer !== undefined) break;
    if (Date.now() > deadline)
      throw new Error("the worklet processor never started");
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
  const { encodeMasterAudio } = await import("../dist/audio-timeline.js");
  processor.port.onmessage({
    data: {
      type: "alpha2-config",
      config: encodeMasterAudio({ limiter: null }),
    },
  });
  processor.port.onmessage({ data: { type: "alpha2-ring", ring: transfer } });
  return {
    read(planes) {
      assert.equal(processor.process([[]], [planes]), true);
    },
  };
}

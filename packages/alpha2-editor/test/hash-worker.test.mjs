// Hashing a whole file on the page thread spends the CPU preview, playback and input share, so a
// host bundling `@alpha2-sh/editor/hash-worker` hashes each file in a worker of its own. Workers
// are faked here: the entry's handler runs in this thread, with messages crossing as tasks.
import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { sha256Blob } from "alpha2/assets";

import {
  createMediaAdapter,
  hashInWorker,
  installHashWorker,
} from "../dist/browser/index.js";

const root = resolve(import.meta.dirname, "..", "..", "..");
const wasmDirectory = join(root, "packages", "alpha2", "wasm");
const skip =
  !existsSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")) &&
  process.env.ALPHA2_REQUIRE_WASM !== "1" &&
  "the wasm package is not built";

/**
 * A dedicated worker running the hash worker's handler, or, with `broken`, one whose script never
 * loads (an error event and nothing else). `answer` replaces what it posts back.
 */
function fakeWorker({ broken = false, answer } = {}) {
  const worker = {
    posted: [],
    terminated: false,
    onmessage: null,
    onerror: null,
    terminate() {
      worker.terminated = true;
    },
    postMessage(message) {
      worker.posted.push(message.type);
      if (broken) {
        setTimeout(() =>
          worker.onerror?.({ preventDefault() {}, message: undefined }),
        );
        return;
      }
      setTimeout(() => scope.onmessage({ data: message }));
    },
  };
  const scope = {
    onmessage: null,
    postMessage(message) {
      if (worker.terminated) return;
      setTimeout(() =>
        worker.onmessage?.({ data: answer?.(message) ?? message }),
      );
    },
  };
  installHashWorker(scope);
  return worker;
}

const bytes = new Uint8Array(3 * 1024 * 1024).map((_, index) => index * 7);
const file = new File([bytes], "clip.mov");

test("a file is hashed in the worker the factory makes, and the worker ends with the hash", async () => {
  const expected = await sha256Blob(file);
  const workers = [];
  const progress = [];
  const sha256 = await hashInWorker(
    () => {
      const worker = fakeWorker();
      workers.push(worker);
      return worker;
    },
    file,
    { onProgress: (read) => progress.push(read) },
  );
  assert.equal(sha256, expected);
  assert.equal(workers.length, 1);
  assert.deepEqual(workers[0].posted, ["hash"]);
  assert.equal(workers[0].terminated, true);
  assert.equal(progress.at(-1), file.size);
});

test("a cancelled hash terminates its worker and rejects with the reason", async () => {
  const controller = new AbortController();
  let worker;
  const pending = hashInWorker(
    () =>
      (worker = fakeWorker({ answer: () => ({ type: "progress", bytes: 1 }) })),
    file,
    { signal: controller.signal },
  );
  controller.abort(new Error("import cancelled"));
  await assert.rejects(pending, { message: "import cancelled" });
  assert.equal(worker.terminated, true);
});

test("a worker that cannot start leaves the hash to the page's thread", async () => {
  assert.equal(
    await hashInWorker(() => fakeWorker({ broken: true }), file),
    await sha256Blob(file),
  );
});

test(
  "the default adapter hashes an imported file in the worker the host provides",
  { skip },
  async () => {
    const wasm = await import("alpha2/wasm");
    wasm.initSync({
      module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
    });
    const fromWorker = "f".repeat(64);
    let made = 0;
    const adapter = createMediaAdapter({
      hashWorkerFactory: () => {
        made += 1;
        // The worker's answer is the one recorded: nothing is hashed on this thread.
        return fakeWorker({
          answer: (message) =>
            message.type === "hashed"
              ? { type: "hashed", sha256: fromWorker }
              : message,
        });
      },
    });
    const grade = readFileSync(
      join(root, "tests", "corpus", "parity-grade.cube"),
    );
    const staged = await adapter.stage(
      new File([grade], "Grade.cube"),
      new AbortController().signal,
    );
    assert.equal(made, 1);
    assert.equal(staged.fingerprint.content_sha256, fromWorker);
    staged.dispose();
  },
);

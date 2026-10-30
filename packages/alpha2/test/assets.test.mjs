import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import test from "node:test";
import { pathToFileURL } from "node:url";

import {
  blockingIssues,
  colorPipelineFingerprint,
  compareFingerprints,
  conformProject,
} from "../dist/assets.js";
import { lintProject } from "../dist/project.js";
import {
  missingCapability,
  suiteOf,
} from "../../../scripts/lib/capabilities.mjs";

const wasmDirectory = join(resolve(import.meta.dirname, ".."), "wasm");
const packaged = existsSync(join(wasmDirectory, "alpha2_wasm_bg.wasm"));
// Skipped before `npm run build` packages the wasm; ALPHA2_REQUIRE=wasm (set by CI after the
// build) makes a missing package fail them.
const skip =
  !packaged &&
  missingCapability(
    "wasm",
    suiteOf(import.meta.url),
    "the wasm package is not built",
  );

// The decisions are the engine's; these check that the wrappers reach it and read its answers, so
// they need the packaged binding.
async function binding() {
  const wasm = await import(
    pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
  );
  wasm.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
  });
  return wasm;
}

const recorded = {
  content_sha256: "a".repeat(64),
  byte_length: 1000,
  streams: { video: { width: 64, height: 48 } },
};

const project = {
  schema_version: 1,
  id: "p",
  name: "P",
  settings: {
    width: 64,
    height: 48,
    frame_rate: { num: 24, den: 1 },
    sample_rate: 48000,
  },
  sources: [
    { id: "video", locator: "asset://v", kind: "video", fingerprint: recorded },
  ],
  timeline: {
    tracks: [
      {
        id: "v1",
        name: "V1",
        kind: "video",
        clips: [
          {
            id: "a",
            source_id: "video",
            timeline_range: { start: 0, end: 120000 },
            source_range: { start: 0, end: 120000 },
          },
        ],
      },
    ],
  },
};

test(
  "the engine compares fingerprints and conforms a catalog",
  { skip },
  async () => {
    const wasm = await binding();
    assert.deepEqual(
      compareFingerprints(wasm, recorded, { ...recorded, modified_ms: 5 }),
      { kind: "identical" },
    );
    assert.deepEqual(
      compareFingerprints(wasm, { byte_length: 1000 }, { byte_length: 999 }),
      { kind: "mismatch", fields: ["byte_length"] },
    );
    const offline = {
      assets: [
        {
          source_id: "video",
          revision: 0,
          original: { state: "offline" },
          proxies: [
            {
              id: "half",
              source_revision: 0,
              made_from: recorded,
              width: 32,
              height: 24,
              state: { state: "ready" },
            },
          ],
        },
      ],
    };
    const preview = conformProject(wasm, project, offline, "preview");
    assert.equal(preview.ready, true);
    assert.deepEqual(preview.sources[0].selection, {
      media: "proxy",
      id: "half",
    });
    const original = conformProject(wasm, project, offline, "export_original");
    assert.equal(original.ready, false);
    assert.deepEqual(blockingIssues(original), [
      { sourceId: "video", issue: { kind: "offline", blocking: true } },
    ]);
    assert.throws(() =>
      conformProject(
        wasm,
        project,
        { assets: [{ ...offline.assets[0], original: { state: "online" } }] },
        "preview",
      ),
    );
    // Evidence that is not well formed is refused as input, not read as other media: a hash in
    // uppercase is no SHA-256 a fingerprint records.
    const upper = { ...recorded, content_sha256: "A".repeat(64) };
    assert.throws(
      () => compareFingerprints(wasm, recorded, upper),
      /found fingerprint has a malformed content_sha256/,
    );
    assert.throws(
      () =>
        conformProject(
          wasm,
          project,
          {
            assets: [
              {
                ...offline.assets[0],
                original: { state: "online", fingerprint: upper },
              },
            ],
          },
          "export_original",
        ),
      /source video: the original's fingerprint has a malformed content_sha256/,
    );
    assert.equal(
      colorPipelineFingerprint(wasm, { hdr: "tone_map", hdr_peak_nits: 1000 }),
      "hdr=tone_map;peak=1000",
    );
  },
);

test(
  "a composition scale with a zero denominator is refused as input, and the binding keeps working",
  { skip },
  async () => {
    const wasm = await binding();
    // Validation must refuse the zero before it divides by it: a division by zero traps in wasm,
    // which leaves the module unusable for every later call.
    const zero = structuredClone(project);
    zero.compositions = [
      {
        id: "g",
        nodes: [{ id: "clip", op: { type: "clip" } }],
        output: "clip",
      },
    ];
    zero.timeline.tracks[0].clips[0].composition = {
      id: "g",
      scale: { num: 1, den: 0 },
    };
    const refused = {
      name: "Alpha2Error",
      code: "INVALID_INPUT",
      message: "invalid project: clip a has an invalid composition reference",
    };
    assert.throws(() => lintProject(wasm, zero), refused);
    assert.throws(
      () => conformProject(wasm, zero, { assets: [] }, "preview"),
      refused,
    );
    assert.deepEqual(lintProject(wasm, project), []);
  },
);

test("the streaming SHA-256 is SHA-256, across chunk boundaries", async () => {
  const { createHash } = await import("node:crypto");
  const { Sha256, sha256Blob } = await import("../dist/assets.js");
  for (const length of [0, 1, 55, 56, 63, 64, 65, 119, 128, 1000, 100_003]) {
    const bytes = new Uint8Array(length).map((_, i) => (i * 131 + 7) & 255);
    const expected = createHash("sha256").update(bytes).digest("hex");
    assert.equal(new Sha256().update(bytes).hex(), expected, `${length}`);
    // Uneven chunks exercise the carried partial block.
    const chunked = new Sha256();
    for (let at = 0; at < length; at += 37)
      chunked.update(bytes.subarray(at, at + 37));
    assert.equal(chunked.hex(), expected, `${length} in chunks`);
    let seen = 0;
    assert.equal(
      await sha256Blob(new Blob([bytes]), {
        onProgress: (bytes) => (seen = bytes),
      }),
      expected,
    );
    assert.equal(seen, length);
  }
  assert.equal(
    new Sha256().update(new TextEncoder().encode("abc")).hex(),
    "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
  );
});

/** A blob whose stream hands over `chunks` chunks of `size` bytes, each ready as soon as asked. */
function readyBlob(chunks, size) {
  const chunk = new Uint8Array(size).map((_, i) => (i * 31 + 5) & 255);
  return {
    stream() {
      let left = chunks;
      return new ReadableStream({
        pull(controller) {
          if (left-- > 0) controller.enqueue(chunk.slice());
          else controller.close();
        },
      });
    },
  };
}

test("hashing a blob lets timers run, so an abort lands before the hash ends", async () => {
  const { sha256Blob } = await import("../dist/assets.js");
  // Chunks that are always ready resolve each read as a microtask, so without a yield no timer
  // could run until the whole blob was hashed; 128 MiB takes the better part of a second.
  for (const [chunks, size] of [
    [128, 1 << 20],
    [1, 128 << 20],
  ]) {
    const controller = new AbortController();
    const reason = new Error("cancelled");
    let progress = 0;
    setTimeout(() => controller.abort(reason), 20);
    await assert.rejects(
      sha256Blob(readyBlob(chunks, size), {
        signal: controller.signal,
        onProgress: (bytes) => (progress = bytes),
      }),
      (error) => error === reason,
      `${chunks} chunks of ${size} bytes`,
    );
    assert.ok(progress < chunks * size, "the abort came before the end");
  }
});

test("hashing yields with a task even where scheduler.yield runs ahead of timers", async () => {
  const { sha256Blob } = await import("../dist/assets.js");
  // Chrome runs a `scheduler.yield()` continuation ahead of timers and frames, starving both; a
  // resolved promise stands in, running ahead of every task the same way.
  const had = Object.getOwnPropertyDescriptor(globalThis, "scheduler");
  let continuations = 0;
  Object.defineProperty(globalThis, "scheduler", {
    configurable: true,
    value: { yield: () => (continuations++, Promise.resolve()) },
  });
  try {
    const controller = new AbortController();
    const reason = new Error("cancelled");
    let progress = 0;
    setTimeout(() => controller.abort(reason), 20);
    await assert.rejects(
      sha256Blob(readyBlob(128, 1 << 20), {
        signal: controller.signal,
        onProgress: (bytes) => (progress = bytes),
      }),
      (error) => error === reason,
    );
    assert.ok(progress < 128 << 20, "the abort came before the end");
    assert.equal(continuations, 0, "scheduler.yield is not used");
  } finally {
    if (had) Object.defineProperty(globalThis, "scheduler", had);
    else delete globalThis.scheduler;
  }
});

test("a chunk larger than one hashing slice hashes as SHA-256", async () => {
  const { createHash } = await import("node:crypto");
  const { sha256Blob } = await import("../dist/assets.js");
  const bytes = new Uint8Array((7 << 19) + 3).map(
    (_, i) => (i * 131 + 7) & 255,
  );
  const expected = createHash("sha256").update(bytes).digest("hex");
  const seen = [];
  assert.equal(
    await sha256Blob(new Blob([bytes]), { onProgress: (n) => seen.push(n) }),
    expected,
  );
  assert.equal(seen.at(-1), bytes.length);
});

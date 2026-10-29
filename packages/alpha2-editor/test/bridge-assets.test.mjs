// The bridge's media work outside the preview: imports, waveform peaks, proxies, and what an export
// or a native upload leases. Run in Node with the packaged wasm engine deciding.
import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { createEditor } from "../dist/index.js";
import {
  createBrowserBridge,
  createMediaAdapter,
} from "../dist/browser/index.js";
import {
  MAX_LOTTIE_DOCUMENT_BYTES,
  MAX_PROVIDER_DOCUMENT_BYTES,
  MAX_SVG_DOCUMENT_BYTES,
} from "alpha2/project";

const wasmDirectory = join(
  resolve(import.meta.dirname, "..", ".."),
  "alpha2",
  "wasm",
);
const packaged = existsSync(join(wasmDirectory, "alpha2_wasm_bg.wasm"));
const skip =
  !packaged &&
  process.env.ALPHA2_REQUIRE_WASM !== "1" &&
  "the wasm package is not built";

let wasm = null;
async function engineWasm() {
  if (wasm) return wasm;
  wasm = await import(
    pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
  );
  wasm.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
  });
  return wasm;
}

const ok = (result) => assert.ok(result.ok, result.error?.message);
const settle = (ms = 0) => new Promise((resolve) => setTimeout(resolve, ms));

/** A stereo 16-bit PCM WAV of a 440 Hz tone, which mediabunny decodes without WebCodecs. */
function toneWav(seconds, sampleRate = 48000, name = "tone.wav") {
  const frames = Math.round(seconds * sampleRate);
  const bytes = new DataView(new ArrayBuffer(44 + frames * 4));
  const text = (offset, value) =>
    [...value].forEach((c, i) => bytes.setUint8(offset + i, c.charCodeAt(0)));
  text(0, "RIFF");
  bytes.setUint32(4, 36 + frames * 4, true);
  text(8, "WAVEfmt ");
  bytes.setUint32(16, 16, true);
  bytes.setUint16(20, 1, true);
  bytes.setUint16(22, 2, true);
  bytes.setUint32(24, sampleRate, true);
  bytes.setUint32(28, sampleRate * 4, true);
  bytes.setUint16(32, 4, true);
  bytes.setUint16(34, 16, true);
  text(36, "data");
  bytes.setUint32(40, frames * 4, true);
  for (let i = 0; i < frames; i++) {
    const sample = Math.round(
      8000 * Math.sin((2 * Math.PI * 440 * i) / sampleRate),
    );
    bytes.setInt16(44 + i * 4, sample, true);
    bytes.setInt16(46 + i * 4, sample, true);
  }
  return new File([bytes.buffer], name, { type: "audio/wav" });
}

/** Stages each file as audio as it is, without decoding: what the bridge does with it is tested. */
const audioAdapter = {
  async stage(file) {
    return {
      runtime: {
        id: `src-${file.name}`,
        kind: "audio",
        name: file.name,
        file,
        duration: 1,
        hasAudio: true,
      },
      description: { id: `src-${file.name}`, kind: "audio", name: file.name },
      dispose() {},
    };
  },
};

test(
  "waveform analysis of media the reopened document still reads finishes",
  { skip },
  async () => {
    const editor = createEditor();
    const bridge = createBrowserBridge({
      editor,
      audio: false,
      assetDecisions: await engineWasm(),
      mediaAdapter: audioAdapter,
    });
    let notified = 0;
    bridge.subscribeAssets(() => notified++);
    const { assetIds } = await bridge.importFiles([toneWav(2)]);
    // What an application's restore does once its relinks are in: open the same document again.
    editor.open(editor.document);
    for (let i = 0; i < 100 && !bridge.getAsset(assetIds[0])?.peaks; i++)
      await settle(10);
    const peaks = bridge.getAsset(assetIds[0])?.peaks;
    assert.ok(peaks instanceof Float32Array, "the runtime has its peaks");
    assert.ok(peaks.some((peak) => peak > 0.1));
    assert.ok(notified >= 1);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "waveform analysis of media no document reads any more is dropped",
  { skip },
  async () => {
    const editor = createEditor();
    const bridge = createBrowserBridge({
      editor,
      audio: false,
      assetDecisions: await engineWasm(),
      mediaAdapter: audioAdapter,
    });
    const before = editor.document;
    const { assetIds } = await bridge.importFiles([toneWav(2)]);
    const runtime = bridge.getAsset(assetIds[0]);
    // Another project: its history cannot return to the import.
    editor.open(before);
    await settle(200);
    assert.equal(runtime.peaks ?? null, null);
    assert.equal(bridge.resourceCount, 0);
    await bridge.dispose();
    editor.dispose();
  },
);

/** A file of `size` bytes starting with `head`, whose whole content is never there to read. */
function hugeFile(name, head, size) {
  const file = {
    name,
    type: "",
    size,
    lastModified: 0,
    wholeReads: 0,
    slice: (start, end) =>
      new Blob([new TextEncoder().encode(head)]).slice(start, end),
    arrayBuffer: async () => {
      file.wholeReads += 1;
      throw new Error("the whole file was read");
    },
  };
  return file;
}

test("a ZIP, JSON or SVG file past what the engine parses is refused before it is read", async () => {
  const adapter = createMediaAdapter();
  for (const [name, head, limit] of [
    ["footage.zip", "PK\u0003\u0004", MAX_PROVIDER_DOCUMENT_BYTES],
    ["export.json", '{"items": []}', MAX_LOTTIE_DOCUMENT_BYTES],
    [
      "drawing.svg",
      '<svg xmlns="http://www.w3.org/2000/svg">',
      MAX_SVG_DOCUMENT_BYTES,
    ],
  ]) {
    const file = hugeFile(name, head, limit + 1);
    await assert.rejects(
      adapter.stage(file, new AbortController().signal),
      (error) =>
        error.code === "too_large" &&
        error.message.includes(name) &&
        error.message.includes(String(limit)),
      name,
    );
    assert.equal(file.wholeReads, 0, name);
  }
});

test("a file that does not import says why: not media, or what its decoder found", async () => {
  const adapter = createMediaAdapter();
  const signal = new AbortController().signal;
  await assert.rejects(
    adapter.stage(new File(["notes"], "notes.txt"), signal),
    {
      code: "unsupported_media",
      message: "Cannot import notes.txt: not an image, video or audio file",
    },
  );
  const damaged = new File([new Uint8Array(4096).fill(7)], "voice.wav", {
    type: "audio/wav",
  });
  await assert.rejects(adapter.stage(damaged, signal), (error) => {
    assert.equal(error.code, "unreadable_media");
    assert.ok(error.cause, "the decoder's error is the cause");
    assert.ok(
      error.message.startsWith("Cannot read voice.wav as audio: ") &&
        error.message.includes(String(error.cause.message)),
      error.message,
    );
    return true;
  });
  const staged = await adapter.stage(toneWav(0.5), signal);
  assert.equal(staged.runtime.kind, "audio");
  staged.dispose();
});

const hash = (digit) => digit.repeat(64);

/** A project whose clip is recorded with `digit`'s hash but whose file is not linked. */
function offlineClip(editor, digit) {
  ok(
    editor.dispatch({
      type: "asset.put",
      asset: {
        id: "clip",
        kind: "video",
        name: "clip.mp4",
        contentRevision: 0,
        missing: true,
        fingerprint: { content_sha256: hash(digit), byte_length: 10 },
      },
      source: {
        id: "clip",
        kind: "video",
        locator: "browser://clip",
        fingerprint: { content_sha256: hash(digit), byte_length: 10 },
      },
    }),
  );
}

test(
  "a kept proxy is adopted without hashing or probing its file",
  { skip },
  async () => {
    const editor = createEditor();
    const staged = [];
    const bridge = createBrowserBridge({
      editor,
      audio: false,
      assetDecisions: await engineWasm(),
      mediaAdapter: {
        async stage(file, signal, options) {
          staged.push(options ?? {});
          return {
            runtime: { id: file.name, kind: "video", name: file.name, file },
            description: {},
            dispose() {},
          };
        },
      },
      proxyStore: {
        hold: async () => ({ release: async () => {} }),
        find: async (wanted) =>
          wanted === hash("e")
            ? {
                record: {
                  made_from: { content_sha256: hash("e"), byte_length: 10 },
                  width: 32,
                  height: 18,
                },
                file: new File(["proxy"], "clip.proxy.mp4"),
              }
            : null,
      },
    });
    offlineClip(editor, "e");
    await bridge.conform("preview");
    assert.equal(bridge.proxyStatus("clip").available, true);
    assert.deepEqual(staged, [{ fingerprint: false }]);
    await bridge.dispose();
    editor.dispose();
  },
);

test("the default adapter computes no fingerprint when none is wanted", async () => {
  const workers = countedHashWorkers();
  const adapter = createMediaAdapter({ hashWorkerFactory: workers.factory });
  const signal = new AbortController().signal;
  const plain = await adapter.stage(toneWav(0.5), signal);
  assert.match(plain.fingerprint?.content_sha256 ?? "", /^[0-9a-f]{64}$/);
  plain.dispose();
  const bare = await adapter.stage(toneWav(0.5), signal, {
    fingerprint: false,
  });
  assert.equal(bare.fingerprint, undefined);
  assert.equal(bare.runtime.kind, "audio");
  bare.dispose();
  assert.equal(workers.posts, 1);
});

/** Hash workers that answer with a fixed hash and count the files posted to them. */
function countedHashWorkers() {
  const workers = { posts: 0, terminated: 0 };
  workers.factory = () => {
    const worker = {
      onmessage: null,
      onerror: null,
      postMessage() {
        workers.posts++;
        setTimeout(() =>
          worker.onmessage?.({ data: { type: "hashed", sha256: hash("c") } }),
        );
      },
      terminate() {
        workers.terminated++;
      },
    };
    return worker;
  };
  return workers;
}

test("the default adapter hashes each file once, from the start of its import", async () => {
  const workers = countedHashWorkers();
  const adapter = createMediaAdapter({ hashWorkerFactory: workers.factory });
  const staging = adapter.stage(toneWav(0.5), new AbortController().signal);
  // Posted before the file is read for anything else.
  assert.equal(workers.posts, 1);
  const staged = await staging;
  assert.equal(staged.fingerprint?.content_sha256, hash("c"));
  assert.equal(workers.posts, 1);
  assert.equal(workers.terminated, 1);
  staged.dispose();
});

test("a file the default adapter refuses stops its hash", async () => {
  const workers = { posts: 0, terminated: 0 };
  const adapter = createMediaAdapter({
    hashWorkerFactory: () => ({
      onmessage: null,
      onerror: null,
      postMessage() {
        workers.posts++;
      },
      terminate() {
        workers.terminated++;
      },
    }),
  });
  await assert.rejects(
    adapter.stage(
      new File(["notes"], "notes.txt"),
      new AbortController().signal,
    ),
    { code: "unsupported_media" },
  );
  assert.equal(workers.posts, 1);
  assert.equal(workers.terminated, 1);
});

test(
  "making a proxy notifies asset listeners once per whole percent, not per frame",
  { skip },
  async () => {
    const editor = createEditor();
    const bridge = createBrowserBridge({
      editor,
      audio: false,
      assetDecisions: await engineWasm(),
      mediaAdapter: {
        async stage(file) {
          return {
            runtime: { id: "clip", kind: "video", name: file.name, file },
            description: {},
            fingerprint: { content_sha256: hash("a"), byte_length: 10 },
            dispose() {},
          };
        },
      },
      proxyStore: {
        hold: async () => ({ release: async () => {} }),
        find: async () => null,
        async generate(file, madeFrom, { onProgress }) {
          for (let frame = 1; frame <= 3000; frame++) onProgress(frame / 3000);
          return {
            record: { made_from: madeFrom, width: 32, height: 18 },
            file: new File(["proxy"], "clip.proxy.mp4"),
          };
        },
      },
    });
    await bridge.importFiles([new File(["video"], "clip.mp4")]);
    let notified = 0;
    const seen = [];
    bridge.subscribeAssets(() => {
      notified++;
      seen.push(bridge.proxyStatus("clip").generating);
    });
    await bridge.generateProxy("clip");
    // Its start, each whole percent from 1 to 100, and its end.
    assert.ok(notified <= 102, `${notified} notifications`);
    assert.ok(seen.includes(0.5));
    assert.equal(bridge.proxyStatus("clip").available, true);
    assert.equal(bridge.proxyStatus("clip").generating, null);
    await bridge.dispose();
    editor.dispose();
  },
);

/** Puts a one-second clip of source `id` on a new track. */
function show(editor, id) {
  const trackId = crypto.randomUUID();
  ok(
    editor.dispatch({
      type: "transaction",
      commands: [
        {
          type: "track.add",
          track: { id: trackId, kind: "video", name: "V", clips: [] },
        },
        {
          type: "clip.insert",
          trackId,
          clip: {
            id: crypto.randomUUID(),
            source_id: id,
            timeline_range: { start: 0, end: 120000 },
            source_range: { start: 0, end: 120000 },
            time_map: { type: "forward" },
          },
        },
      ],
    }),
  );
}

function addColor(editor, id = "color") {
  ok(
    editor.dispatch({
      type: "asset.put",
      asset: { id, kind: "generated", name: "Color", contentRevision: 0 },
      source: {
        id,
        kind: "generated",
        generator: {
          type: "fill",
          paint: {
            type: "solid",
            color: { red: 1, green: 0, blue: 0, alpha: 1 },
          },
        },
      },
    }),
  );
  show(editor, id);
}

async function plainSession() {
  const editor = createEditor();
  const bridge = createBrowserBridge({
    editor,
    audio: false,
    assetDecisions: await engineWasm(),
    mediaAdapter: audioAdapter,
  });
  return { editor, bridge };
}

test(
  "a refused export names the media by the asset's name, its ID kept for programs",
  { skip },
  async () => {
    const { editor, bridge } = await plainSession();
    offlineClip(editor, "c");
    show(editor, "clip");
    assert.throws(
      () => bridge.startExport(),
      (error) =>
        error.code === "conform_failed" &&
        error.message === "The media is not ready: clip.mp4 offline" &&
        error.entityId === "clip" &&
        error.report.sources[0].source_id === "clip",
    );
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "a font no caption or title is linked to is left out of an export, as preview leaves it out",
  { skip },
  async () => {
    const { editor, bridge } = await plainSession();
    addColor(editor);
    // A font the project's bin records but this session has no file for.
    ok(
      editor.dispatch({
        type: "asset.put",
        asset: {
          id: "brand",
          kind: "font",
          name: "Brand.ttf",
          contentRevision: 0,
          missing: true,
        },
      }),
    );
    assert.equal((await bridge.conform("export_original")).ready, true);
    const job = bridge.startExport();
    job.result.catch(() => {});
    await job.cancel();
    const native = await bridge.nativeAssets(editor.document);
    assert.deepEqual(
      native.filter((entry) => entry.kind === "caption-fonts"),
      [],
    );
    await bridge.dispose();
    editor.dispose();
  },
);

/** A text asset whose authoring is `text`, as a project file may give it. */
function textAsset(id, text) {
  return {
    type: "asset.put",
    asset: {
      id,
      kind: "text",
      name: "Lower third",
      contentRevision: 0,
      authoring: { text },
    },
    source: { id, kind: "image", locator: `browser://${id}` },
  };
}

test(
  "a text asset that cannot be drawn fails only what shows it, naming it",
  { skip },
  async () => {
    const { editor, bridge } = await plainSession();
    addColor(editor);
    for (const text of [
      { content: "Hi", fontFamily: ["Inter"] },
      { content: 42 },
    ]) {
      const refused = editor.dispatch(textAsset("bad", text));
      assert.equal(refused.ok, false);
      assert.equal(refused.error.code, "invalid_asset");
    }
    // A style in another shape enters it, and is not drawn here (Node has no text rasterizer):
    // unused, it leaves conform, export and native assets alone.
    ok(editor.dispatch(textAsset("odd", "Hi")));
    assert.equal((await bridge.conform("export_original")).ready, true);
    const job = bridge.startExport();
    job.result.catch(() => {});
    await job.cancel();
    await bridge.nativeAssets(editor.document);
    show(editor, "odd");
    await assert.rejects(bridge.conform("export_original"), (error) => {
      assert.equal(error.code, "text_unrenderable");
      assert.equal(error.entityId, "odd");
      assert.match(error.message, /^Text "Lower third" cannot be drawn: /);
      return true;
    });
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "the default adapter imports a font, a .cube LUT and audio through the bridge",
  { skip },
  async () => {
    const editor = createEditor();
    const bridge = createBrowserBridge({
      editor,
      audio: false,
      assetDecisions: await engineWasm(),
    });
    const repository = new URL("../../../", import.meta.url);
    const font = new File(
      [readFileSync(new URL("tests/fonts/Alpha2Fixture.ttf", repository))],
      "Alpha2Fixture.ttf",
    );
    const lut = new File(
      [readFileSync(new URL("tests/corpus/parity-grade.cube", repository))],
      "parity-grade.cube",
    );
    const { assetIds } = await bridge.importFiles([font, lut, toneWav(0.5)]);
    const assets = assetIds.map((id) => editor.document.assets[id]);
    assert.deepEqual(
      assets.map((asset) => [asset.kind, asset.name]),
      [
        ["font", "Alpha2Fixture.ttf"],
        ["lut3d", "parity-grade.cube"],
        ["audio", "tone.wav"],
      ],
    );
    assert.ok(assets.every((a) => /^[0-9a-f]{64}$/.test(hashOf(a))));
    assert.deepEqual(
      editor.document.project.effect_resources?.map((r) => r.id),
      [assetIds[1]],
    );
    assert.deepEqual(
      editor.document.project.sources.map((s) => [s.id, s.has_audio]),
      [[assetIds[2], true]],
    );
    assert.equal(bridge.getAsset(assetIds[0]).bytes.length, font.size);
    await bridge.dispose();
    editor.dispose();
  },
);

function hashOf(asset) {
  return asset.fingerprint?.content_sha256 ?? "";
}

test(
  "a staged asset whose dispose is a class method is released as a method, and a failed batch keeps its own error",
  { skip },
  async () => {
    const disposed = [];
    class Staged {
      constructor(file) {
        this.file = file;
        this.runtime = {
          id: file.name,
          kind: "image",
          file,
          width: 4,
          height: 4,
        };
        this.description = {};
      }
      dispose() {
        disposed.push(this.file.name);
      }
    }
    const editor = createEditor();
    const bridge = createBrowserBridge({
      editor,
      audio: false,
      assetDecisions: await engineWasm(),
      mediaAdapter: {
        async stage(file) {
          if (file.name === "bad") throw new Error("decode failed");
          return new Staged(file);
        },
      },
    });
    await assert.rejects(
      bridge.importFiles([
        new File(["1"], "good1"),
        new File(["2"], "good2"),
        new File(["3"], "bad"),
      ]),
      /decode failed/,
    );
    assert.deepEqual(disposed.sort(), ["good1", "good2"]);
    await bridge.importFiles([new File(["4"], "kept")]);
    await bridge.dispose();
    assert.deepEqual(disposed.sort(), ["good1", "good2", "kept"]);
    editor.dispose();
  },
);

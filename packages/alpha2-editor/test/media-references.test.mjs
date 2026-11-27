import test from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { sha256Blob } from "alpha2/assets";
import { createEditor } from "../dist/index.js";
import { createNativeClient } from "../dist/native-client/index.js";
import {
  RangeFile,
  reviveRangeFile,
} from "../dist/browser/runtime/range-file.js";
import { openMediaReference } from "../dist/browser/runtime/media-reference.js";
import {
  extractInWorker,
  installExtractWorker,
} from "../dist/browser/extract-worker.js";
import { probeEvidence } from "../dist/browser/runtime/fingerprint.js";
import { wasmDirectory, wasmSkip } from "./support/wasm.mjs";

const skip = wasmSkip(import.meta.url);
const { createBrowserBridge } = skip
  ? {}
  : await import("../dist/browser/index.js");

const video = new Uint8Array(
  readFileSync(
    join(import.meta.dirname, "../../../tests/corpus/h264-bt709-copy.mp4"),
  ),
);

async function serve(files, { ranges = true } = {}) {
  const requests = [];
  const server = createServer((request, response) => {
    requests.push(request.headers.range ?? "whole");
    const bytes = files[request.url];
    if (!bytes) return void response.writeHead(404).end();
    const range = /^bytes=(\d+)-(\d+)$/.exec(request.headers.range ?? "");
    if (!ranges || !range)
      return void response
        .writeHead(200, { "content-type": "video/mp4" })
        .end(bytes);
    const first = Number(range[1]);
    const last = Math.min(Number(range[2]), bytes.length - 1);
    response
      .writeHead(206, {
        "content-type": "video/mp4",
        "content-range": `bytes ${first}-${last}/${bytes.length}`,
        "last-modified": "Tue, 24 Nov 2026 10:00:00 GMT",
      })
      .end(bytes.subarray(first, last + 1));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return {
    url: (path) => `http://127.0.0.1:${server.address().port}${path}`,
    requests,
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}

test("a range file reads, slices and streams the bytes its URL serves", async () => {
  const server = await serve({ "/a.mp4": video });
  try {
    const file = await openMediaReference({
      url: server.url("/a.mp4"),
      name: "a.mp4",
    });
    assert.equal(file.size, video.length);
    assert.equal(file.type, "video/mp4");
    assert.equal(file.lastModified, Date.parse("2026-11-24T10:00:00Z"));
    assert.deepEqual(await file.read(10, 20), video.subarray(10, 20));
    const middle = file.slice(100, 300).slice(50, -50);
    assert.equal(middle.size, 100);
    assert.deepEqual(await middle.bytes(), video.subarray(150, 250));
    assert.deepEqual(
      new Uint8Array(await new Response(file.stream()).arrayBuffer()),
      video,
    );
    assert.equal((await file.blob()).size, video.length);
    const cloned = reviveRangeFile(structuredClone(middle));
    assert.ok(cloned instanceof RangeFile);
    assert.deepEqual(await cloned.bytes(), video.subarray(150, 250));
    assert.equal(
      await sha256Blob(file),
      await sha256Blob(new Blob([video])),
      "hashed as its bytes",
    );
    assert.deepEqual(
      await probeEvidence(file),
      await probeEvidence(new Blob([video])),
      "demuxed as its bytes",
    );
  } finally {
    await server.close();
  }
});

const AUDIO_DESCRIPTOR = {
  id: "alpha2.test.blocks",
  version: 1,
  input: { media: "audio", from: ["source"], format: "f32" },
  params_schema: { type: "object" },
  outputs: [{ name: "blocks", kind: "scalars", channels: ["frames"] }],
};

const VIDEO_DESCRIPTOR = {
  ...AUDIO_DESCRIPTOR,
  id: "alpha2.test.frames",
  input: { media: "video", from: ["source"], format: "luma8" },
};

async function testPlugin(descriptor = AUDIO_DESCRIPTOR) {
  const binaryen = (await import("binaryen")).default;
  const { loadPlugin } = await import("alpha2/plugins");
  const json = JSON.stringify(descriptor);
  const packed = (BigInt(json.length) << 32n).toString();
  const module = binaryen.parseText(`(module
    (import "alpha2" "emit" (func $emit (param i32 i32) (result i32)))
    (memory (export "memory") 128)
    (data (i32.const 0) "${json.replaceAll('"', "\\22")}")
    (func (export "alpha2_abi") (result i32) i32.const 1)
    (func (export "alpha2_describe") (result i64) i64.const ${packed})
    (func (export "alpha2_alloc") (param i32 i32) (result i32) i32.const 65536)
    (func (export "alpha2_free") (param i32 i32 i32))
    (func (export "alpha2_begin") (param i32 i32) (result i32) i32.const 0)
    (func (export "alpha2_pass") (param i32 i32) (result i32) i32.const 0)
    (func (export "alpha2_video") (param i64 i32 i32 i32 i32 i32) (result i32) i32.const 0)
    (func (export "alpha2_audio") (param i64 i64 i32 i32 i32 i32 i32) (result i32)
      (i32.store8 (i32.const 4096) (i32.const 1))
      (i32.store16 (i32.const 4098) (i32.const 0))
      (i32.store (i32.const 4100) (i32.const 20))
      (i64.store (i32.const 4104) (local.get 0))
      (i32.store (i32.const 4112) (i32.const 1))
      (i32.store (i32.const 4116) (i32.const 0))
      (f32.store (i32.const 4120) (f32.convert_i32_s (local.get 2)))
      (call $emit (i32.const 4096) (i32.const 28)))
    (func (export "alpha2_end") (result i32) i32.const 0))`);
  const wasm = module.emitBinary();
  module.dispose();
  const hex = async (bytes) =>
    Buffer.from(await crypto.subtle.digest("SHA-256", bytes)).toString("hex");
  const manifest = new TextEncoder().encode(
    JSON.stringify({
      id: descriptor.id,
      version: "0.1.0",
      abi: "alpha2-plugin/1",
      kind: "extractor",
      artifacts: [
        {
          target: "wasm32",
          type: "wasm",
          url: "m.wasm",
          sha256: await hex(wasm),
          bytes: wasm.byteLength,
        },
      ],
      descriptor,
    }),
  );
  const files = { "manifest.json": manifest, "m.wasm": wasm };
  return loadPlugin("https://plugins.test/manifest.json", await hex(manifest), {
    fetch: async (url) => new Response(files[new URL(url).pathname.slice(1)]),
  });
}

function fakeExtractWorker() {
  const worker = {
    terminated: false,
    onmessage: null,
    onerror: null,
    terminate() {
      worker.terminated = true;
    },
    postMessage(message) {
      const data = structuredClone(message);
      setTimeout(() => scope.onmessage({ data }));
    },
  };
  const scope = {
    onmessage: null,
    postMessage(message) {
      if (worker.terminated) return;
      const data = structuredClone(message);
      setTimeout(() => worker.onmessage?.({ data }));
    },
  };
  installExtractWorker(scope);
  return worker;
}

test(
  "the extraction worker reads a referenced source over range requests",
  { skip },
  async () => {
    const wav = new Uint8Array(
      readFileSync(
        join(import.meta.dirname, "../../../tests/corpus/audio-3.0.wav"),
      ),
    );
    const server = await serve({ "/a.wav": wav });
    try {
      const wasm = await import("alpha2/wasm");
      wasm.initSync({
        module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
      });
      const file = await openMediaReference({
        url: server.url("/a.wav"),
        name: "a.wav",
      });
      const plugin = await testPlugin();
      const result = await extractInWorker(fakeExtractWorker, {
        plugin,
        project: {
          schema_version: 1,
          id: "p",
          name: "P",
          settings: {
            width: 320,
            height: 180,
            frame_rate: { num: 30, den: 1 },
            sample_rate: 48000,
          },
          sources: [{ id: "a", kind: "audio", locator: "a.wav" }],
          timeline: { tracks: [] },
        },
        request: {
          extractor: AUDIO_DESCRIPTOR.id,
          version: 1,
          input: {
            from: "source",
            source_id: "a",
            range: { start: 0, end: 24000 },
          },
          outputs: { blocks: "blocks" },
        },
        media: { a: file },
        signal: new AbortController().signal,
      });
      assert.ok(result, "ran in the worker");
      const [set] = result.dataSets;
      assert.equal(set.id, "blocks");
      assert.ok(set.times.length > 0);
      assert.ok(server.requests.some((range) => range.startsWith("bytes=")));
    } finally {
      await server.close();
    }
  },
);

test(
  "a decode refusal keeps its facts across the extraction worker",
  { skip },
  async () => {
    const wasm = await import("alpha2/wasm");
    wasm.initSync({
      module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
    });
    const plugin = await testPlugin(VIDEO_DESCRIPTOR);
    await assert.rejects(
      extractInWorker(fakeExtractWorker, {
        plugin,
        project: {
          schema_version: 1,
          id: "p",
          name: "P",
          settings: {
            width: 320,
            height: 180,
            frame_rate: { num: 30, den: 1 },
            sample_rate: 48000,
          },
          sources: [{ id: "v", kind: "video", locator: "v.mp4" }],
          timeline: { tracks: [] },
        },
        request: {
          extractor: VIDEO_DESCRIPTOR.id,
          version: 1,
          input: {
            from: "source",
            source_id: "v",
            range: { start: 0, end: 40000 },
          },
          outputs: { blocks: "blocks" },
        },
        media: { v: new Blob([video], { type: "video/mp4" }) },
        signal: new AbortController().signal,
      }),
      (error) => {
        assert.equal(error.code, "DECODE_UNAVAILABLE");
        assert.equal(error.facts.kind, "video");
        assert.match(error.facts.codec, /^avc1/);
        return true;
      },
    );
  },
);

const stillAdapter = {
  async stage(file) {
    const id = `src-${crypto.randomUUID()}`;
    return {
      runtime: { id, kind: "image", name: file.name, file },
      description: { id, kind: "image", name: file.name },
      dispose() {},
    };
  },
};

async function session() {
  const engine = await import(
    pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
  );
  engine.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
  });
  const editor = createEditor();
  const bridge = createBrowserBridge({
    editor,
    audio: false,
    assetDecisions: engine,
    mediaAdapter: stillAdapter,
  });
  return { editor, bridge };
}

function show(editor, sourceId) {
  const result = editor.dispatch({
    type: "transaction",
    commands: [
      {
        type: "track.add",
        track: { id: "v", kind: "video", name: "V", clips: [] },
      },
      {
        type: "clip.insert",
        trackId: "v",
        clip: {
          id: "c",
          source_id: sourceId,
          timeline_range: { start: 0, end: 120000 },
          source_range: { start: 0, end: 120000 },
          time_map: { type: "forward" },
        },
      },
    ],
  });
  assert.ok(result.ok, result.error?.message);
}

test(
  "a referenced file imports and relinks by URL, and a native export sends its path",
  { skip },
  async () => {
    const server = await serve({ "/a.mp4": video, "/b.mp4": video });
    const { editor, bridge } = await session();
    try {
      const {
        assetIds: [id],
      } = await bridge.importFiles([
        { url: server.url("/a.mp4"), name: "a.mp4", path: "/media/a.mp4" },
      ]);
      const held = bridge.getAsset(id).file;
      assert.ok(held instanceof RangeFile);
      assert.equal(held.size, video.length);
      show(editor, id);
      await bridge.conform("export_original");
      assert.deepEqual(await bridge.nativeAssets(), [
        { id, kind: "assets", path: "/media/a.mp4", name: "a.mp4" },
      ]);

      await bridge.relink(id, { url: server.url("/b.mp4"), name: "b.mp4" });
      assert.equal(bridge.getAsset(id).file.url, server.url("/b.mp4"));
      const [uploaded] = await bridge.nativeAssets();
      assert.equal(uploaded.path, undefined);
      assert.deepEqual(
        new Uint8Array(await uploaded.blob.arrayBuffer()),
        video,
      );
    } finally {
      await bridge.dispose();
      editor.dispose();
      await server.close();
    }
  },
);

test(
  "a URL that does not answer range requests is refused and nothing is added",
  { skip },
  async () => {
    const server = await serve({ "/a.mp4": video }, { ranges: false });
    const { editor, bridge } = await session();
    try {
      await assert.rejects(
        bridge.importFiles([{ url: server.url("/a.mp4"), name: "a.mp4" }]),
        { code: "unreadable_media", message: /does not answer range/ },
      );
      await assert.rejects(
        bridge.importFiles([{ url: server.url("/gone.mp4"), name: "x.mp4" }]),
        { code: "unreadable_media", message: /answered 404/ },
      );
      assert.equal(Object.keys(editor.document.assets).length, 0);
    } finally {
      await bridge.dispose();
      editor.dispose();
      await server.close();
    }
  },
);

test("a native export sends a file by its path instead of uploading it", async () => {
  const calls = [];
  let sent;
  const client = createNativeClient({
    baseUrl: "http://host",
    fetch: async (url, init) => {
      const method = init?.method ?? "GET";
      calls.push(`${method} ${new URL(url).pathname}`);
      if (method === "POST") sent = JSON.parse(init.body);
      return Response.json({
        phase: "completed",
        fraction: 1,
        artifact: { name: "a.mp4", bytes: 1, sha256: "", createdAt: 0 },
      });
    },
  });
  const editor = createEditor();
  const job = client.startExport(editor.getSnapshot(), [
    { id: "a", kind: "assets", path: "/media/a.mp4", name: "a.mp4" },
    { id: "b", kind: "assets", blob: new Blob(["b"]), name: "b.mp4" },
  ]);
  await job.result;
  assert.deepEqual(
    calls.filter((call) => call.startsWith("PUT")),
    [`PUT /jobs/${job.id}`, `PUT /jobs/${job.id}/assets/b`],
  );
  assert.deepEqual(sent.paths, { assets: { a: "/media/a.mp4" } });
  await client.dispose();
  editor.dispose();
});

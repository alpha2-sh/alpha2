import { FakeCanvas } from "./support/preview-stubs.mjs";
import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { createEditor, createEmptyDocument } from "../dist/index.js";
import {
  missingCapability,
  suiteOf,
} from "../../../scripts/lib/capabilities.mjs";

// Loaded after the stubs' module hook is registered, which a static import would precede.
const { createBrowserBridge } = await import("../dist/browser/index.js");

const wasmDirectory = join(
  resolve(import.meta.dirname, "..", ".."),
  "alpha2",
  "wasm",
);
const packaged = existsSync(join(wasmDirectory, "alpha2_wasm_bg.wasm"));
const skip =
  !packaged &&
  missingCapability(
    "wasm",
    suiteOf(import.meta.url),
    "the wasm package is not built",
  );

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
const latestEngine = () => globalThis.stubEngine.engines.at(-1);
const hash = (digit) => digit.repeat(64);

function addColor(editor) {
  const id = crypto.randomUUID();
  const trackId = crypto.randomUUID();
  ok(
    editor.dispatch({
      type: "transaction",
      commands: [
        {
          type: "asset.put",
          asset: { id, kind: "generated", name: "Color", contentRevision: 0 },
          source: {
            id,
            kind: "generated",
            generator: {
              type: "fill",
              paint: {
                type: "solid",
                color: { red: 0.8, green: 0.3, blue: 0.2, alpha: 1 },
              },
            },
          },
        },
        {
          type: "track.add",
          track: { id: trackId, kind: "video", name: "V1", clips: [] },
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

/**
 * A bridge on the recording renderer. Its adapter stages any object with a fingerprint as video
 * (a kept proxy through `stageProxy` when given), its proxy store counts lookups and holds, finds
 * `kept` proxies by original hash and holds each conversion until the test finishes it, and each
 * engine conform is counted.
 */
async function session({
  document,
  kept = {},
  stageProxy,
  resourceLimit,
} = {}) {
  const reported = [];
  const engine = await engineWasm();
  const counts = { conform: 0, find: 0, proxyStages: 0, holds: 0, released: 0 };
  const assetDecisions = {
    ...engine,
    conformProject: (...args) => {
      counts.conform += 1;
      return engine.conformProject(...args);
    },
  };
  const conversions = [];
  const editor = createEditor({
    document: document ?? createEmptyDocument({ width: 64, height: 36 }),
  });
  const bridge = createBrowserBridge({
    editor,
    audio: false,
    assetDecisions,
    resourceLimit,
    onError: (error) => reported.push(error),
    mediaAdapter: {
      async stage(file, signal, options) {
        if (options?.fingerprint === false) {
          counts.proxyStages += 1;
          if (stageProxy) return stageProxy(file);
        }
        return {
          runtime: {
            id: file.name,
            kind: "video",
            file,
            width: 64,
            height: 36,
          },
          description: {},
          fingerprint: file.fingerprint,
          dispose() {},
        };
      },
    },
    proxyStore: {
      async hold() {
        counts.holds += 1;
        return {
          release: async () => {
            counts.released += 1;
          },
        };
      },
      async find(hash) {
        counts.find += 1;
        return kept[hash] ?? null;
      },
      generate(file, made_from, { signal }) {
        return new Promise((resolve, reject) => {
          const conversion = {
            file,
            signal,
            finish: () =>
              resolve({
                record: { made_from, width: 32, height: 18 },
                file: { name: `${file.name}.proxy`, fingerprint: {} },
              }),
          };
          conversions.push(conversion);
          signal?.addEventListener("abort", () => reject(signal.reason), {
            once: true,
          });
        });
      },
    },
  });
  return { editor, bridge, reported, counts, conversions };
}

function place(editor, id) {
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

async function importClip(bridge, name, digit) {
  await bridge.importFiles([
    { name, fingerprint: { content_sha256: hash(digit), byte_length: 10 } },
  ]);
}

const keptProxy = (sha) => ({
  record: {
    made_from: { content_sha256: sha, byte_length: 10 },
    width: 32,
    height: 18,
  },
  file: { name: `${sha.slice(-4)}.proxy`, fingerprint: {} },
});

const until = async (condition, what) => {
  for (let i = 0; i < 200 && !condition(); i += 1) await settle(2);
  assert.ok(condition(), what);
};

test(
  "a scrub over an unchanged document checks its media once, and an offline bin source is looked up once",
  { skip },
  async () => {
    const { editor, bridge, counts } = await session();
    addColor(editor);
    await importClip(bridge, "clip", "a");
    const asset = editor.document.assets.clip;
    ok(
      editor.dispatch({
        type: "asset.put",
        asset: { ...asset, missing: true },
      }),
    );
    await bridge.bindPreview(new FakeCanvas(64, 36));
    await settle(40);
    assert.equal(await bridge.seek(0), true);
    const conformed = counts.conform;
    const looked = counts.find;
    for (let frame = 1; frame <= 5; frame += 1)
      assert.equal(await bridge.seek(frame * 4000), true);
    assert.equal(counts.conform, conformed, "no conform per scrub step");
    assert.equal(counts.find, looked, "no lookup per scrub step");
    ok(editor.dispatch({ type: "project.update", patch: { name: "edited" } }));
    assert.equal(await bridge.seek(0), true);
    assert.equal(counts.conform, conformed + 1);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "a seek overtaken by an edit that changes nothing drawn still shows its frame",
  { skip },
  async () => {
    const { editor, bridge, reported } = await session();
    addColor(editor);
    await bridge.bindPreview(new FakeCanvas(64, 36));
    assert.equal(await bridge.seek(0), true);
    let release;
    const held = new Promise((resolve) => (release = resolve));
    globalThis.stubEngine.render = () => held;
    const inFlight = bridge.seek(4000);
    await settle(5);
    const step = bridge.seek(8000);
    ok(
      editor.dispatch({
        type: "marker.add",
        marker: { id: "m", time: 8000, name: "M" },
      }),
    );
    globalThis.stubEngine.render = null;
    release();
    await inFlight;
    assert.equal(await step, true, "the step renders");
    await settle(40);
    assert.equal(latestEngine().renders.at(-1).seconds, 8000 / 120000);
    assert.deepEqual(reported, []);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "proxies are made one at a time, and an export takes the place of the one running",
  { skip },
  async () => {
    const { editor, bridge, conversions } = await session();
    await importClip(bridge, "one", "a");
    await importClip(bridge, "two", "b");
    const first = bridge.generateProxy("one");
    const second = bridge.generateProxy("two");
    await until(() => conversions.length >= 1, "the first conversion starts");
    await settle(10);
    assert.equal(conversions.length, 1, "the second waits for the first");
    await bridge.conform("export_original");
    const job = bridge.startExport();
    assert.equal(conversions[0].signal.aborted, true, "the proxy gives way");
    await job.result.catch(() => {});
    await until(() => conversions.length >= 2, "a conversion starts again");
    assert.equal(conversions[1].file.name, "one", "the one that gave way");
    await settle(10);
    assert.equal(conversions.length, 2, "still one at a time");
    conversions[1].finish();
    await first;
    await until(() => conversions.length >= 3, "the second proxy starts");
    conversions[2].finish();
    await second;
    assert.equal(bridge.proxyStatus("one").available, true);
    assert.equal(bridge.proxyStatus("two").available, true);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "a proxy finished in the background leaves playback playing",
  { skip },
  async () => {
    const { editor, bridge, conversions, reported } = await session();
    addColor(editor);
    await importClip(bridge, "clip", "a");
    await bridge.bindPreview(new FakeCanvas(64, 36));
    assert.equal(await bridge.seek(0), true);
    const making = bridge.generateProxy("clip");
    await until(() => conversions.length === 1, "the conversion starts");
    await bridge.play();
    assert.equal(bridge.playing, true);
    conversions[0].finish();
    await making;
    await settle(40);
    assert.equal(bridge.playing, true, "playback goes on");
    bridge.pause();
    assert.deepEqual(reported, []);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "preview prefers an online source's kept proxy only when asked, and the host still sees the original",
  { skip },
  async () => {
    const { editor, bridge, reported } = await session({
      kept: {
        [hash("a")]: {
          record: {
            made_from: { content_sha256: hash("a"), byte_length: 10 },
            width: 32,
            height: 18,
          },
          file: { name: "clip.proxy", fingerprint: {} },
        },
      },
    });
    await importClip(bridge, "clip", "a");
    place(editor, "clip");
    await bridge.bindPreview(new FakeCanvas(64, 36));
    assert.equal(await bridge.seek(0), true);
    assert.equal(bridge.previewMedia, "original");
    assert.equal(bridge.proxyStatus("clip").reading, "original");
    assert.equal(
      bridge.proxyStatus("clip").available,
      false,
      "not looked up while the original is online",
    );
    let heard = 0;
    bridge.subscribeAssets(() => (heard += 1));
    bridge.setPreviewMedia("proxies");
    assert.equal(bridge.previewMedia, "proxies");
    assert.equal(await bridge.seek(0), true);
    assert.equal(bridge.proxyStatus("clip").available, true);
    assert.equal(bridge.proxyStatus("clip").reading, "proxy");
    assert.ok(heard > 0, "asset listeners hear what preview reads");
    assert.equal(bridge.getAsset("clip").file.name, "clip");
    bridge.setPreviewMedia("original");
    assert.equal(await bridge.seek(0), true);
    assert.equal(bridge.proxyStatus("clip").reading, "original");
    assert.throws(() => bridge.setPreviewMedia("half"));
    assert.deepEqual(reported, []);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "a kept proxy that cannot be staged is reported once, and preview, conform and native assets go on",
  { skip },
  async () => {
    const { editor, bridge, reported, counts } = await session({
      kept: { [hash("e")]: keptProxy(hash("e")) },
      stageProxy: async () => {
        throw new Error("this session cannot read the proxy file");
      },
    });
    addColor(editor);
    await importClip(bridge, "clip", "e");
    ok(
      editor.dispatch({
        type: "asset.put",
        asset: { ...editor.document.assets.clip, missing: true },
      }),
    );
    await bridge.bindPreview(new FakeCanvas(64, 36));
    assert.equal(await bridge.seek(0), true);
    assert.equal(await bridge.seek(4000), true);
    assert.ok(await bridge.conform("preview"));
    await bridge.nativeAssets();
    ok(editor.dispatch({ type: "project.update", patch: { name: "edited" } }));
    assert.equal(await bridge.seek(0), true);
    assert.deepEqual(
      reported.map((e) => [e.code, e.entityId, e.recoverable]),
      [["proxy_unavailable", "clip", true]],
    );
    assert.match(reported[0].cause.message, /cannot read the proxy file/);
    assert.equal(counts.proxyStages, 1, "not staged again on later loads");
    assert.equal(counts.released, counts.holds);
    assert.equal(bridge.proxyStatus("clip").available, false);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "a kept proxy the runtime has no room for is reported, and preview draws the rest",
  { skip },
  async () => {
    const { editor, bridge, reported } = await session({
      kept: { [hash("e")]: keptProxy(hash("e")) },
      resourceLimit: 1,
    });
    await importClip(bridge, "shown", "a");
    place(editor, "shown");
    ok(
      editor.dispatch({
        type: "asset.put",
        asset: {
          id: "clip",
          kind: "video",
          name: "clip.mp4",
          contentRevision: 0,
          missing: true,
          fingerprint: { content_sha256: hash("e"), byte_length: 10 },
        },
        source: {
          id: "clip",
          kind: "video",
          locator: "browser://clip",
          fingerprint: { content_sha256: hash("e"), byte_length: 10 },
        },
      }),
    );
    await bridge.bindPreview(new FakeCanvas(64, 36));
    assert.equal(await bridge.seek(0), true);
    assert.ok(await bridge.conform("preview"));
    assert.deepEqual(
      reported.map((e) => [e.code, e.entityId, e.cause?.code]),
      [["proxy_unavailable", "clip", "resource_budget"]],
    );
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "proxy preview of more sources than the runtime holds reads the originals of the rest",
  { skip },
  async () => {
    const sha = (n) => n.toString(16).padStart(64, "0");
    const count = 70;
    const kept = {};
    for (let n = 1; n <= count; n += 1) kept[sha(n)] = keptProxy(sha(n));
    const { editor, bridge, reported, counts } = await session({ kept });
    await bridge.importFiles(
      Array.from({ length: count }, (_, i) => ({
        name: `f${i + 1}`,
        fingerprint: { content_sha256: sha(i + 1), byte_length: 10 },
      })),
    );
    place(editor, "f1");
    place(editor, `f${count}`);
    await bridge.bindPreview(new FakeCanvas(64, 36));
    assert.equal(await bridge.seek(0), true);
    bridge.setPreviewMedia("proxies");
    assert.equal(await bridge.seek(0), true);
    assert.equal(bridge.proxyStatus("f1").reading, "proxy");
    assert.equal(bridge.proxyStatus(`f${count}`).reading, "original");
    const refused = reported.map((e) => e.entityId);
    assert.ok(refused.length > 0);
    assert.ok(reported.every((e) => e.code === "proxy_unavailable"));
    assert.equal(new Set(refused).size, refused.length, "once per proxy");
    assert.ok(refused.includes(`f${count}`));
    const staged = counts.proxyStages;
    ok(editor.dispatch({ type: "project.update", patch: { name: "edited" } }));
    assert.equal(await bridge.seek(4000), true);
    assert.equal(counts.proxyStages, staged);
    assert.equal(reported.length, refused.length);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "the monitor level is kept from before a preview is bound and across bindings",
  { skip },
  async () => {
    const { editor, bridge } = await session();
    bridge.setMonitorVolume(0.25);
    assert.equal(bridge.monitorVolume, 0.25);
    await bridge.bindPreview(new FakeCanvas(64, 36));
    assert.equal(globalThis.stubEngine.mixers.at(-1).volume, 0.25);
    bridge.setMonitorVolume(0);
    await bridge.bindPreview(new FakeCanvas(64, 36));
    assert.equal(globalThis.stubEngine.mixers.at(-1).volume, 0);
    assert.equal(bridge.monitorVolume, 0);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "a font the renderer refuses is reported once by name, and preview goes on drawing",
  { skip },
  async () => {
    const font = {
      name: "Broken",
      bytes: new Uint8Array([0x77, 0x4f, 0x46, 0x32]),
      faceIndex: 0,
    };
    const fallback = {
      name: "Fallback",
      bytes: new Uint8Array(4),
      faceIndex: 0,
    };
    globalThis.stubEngine.setCaptionFonts = (fonts) =>
      fonts
        .filter((f) => f.name === "Broken")
        .map((f) => ({ font: f, error: new Error("invalid text font") }));
    const reported = [];
    const editor = createEditor({
      document: createEmptyDocument({ width: 64, height: 36 }),
    });
    const bridge = createBrowserBridge({
      editor,
      audio: false,
      assetDecisions: await engineWasm(),
      runtimeAssets: { fonts: [font, fallback] },
      onError: (error) => reported.push(error),
    });
    try {
      addColor(editor);
      await bridge.bindPreview(new FakeCanvas(64, 36));
      assert.equal(await bridge.seek(0), true);
      assert.equal(await bridge.seek(4000), true);
      ok(editor.dispatch({ type: "project.update", patch: { name: "x" } }));
      assert.equal(await bridge.seek(0), true);
      assert.deepEqual(
        reported.map((e) => e.code),
        ["invalid_font"],
        "reported once",
      );
      assert.match(reported[0].message, /Broken/);
    } finally {
      globalThis.stubEngine.setCaptionFonts = null;
      await bridge.dispose();
      editor.dispose();
    }
  },
);

test(
  "a seek while Play is still starting stops that Play, without an error",
  { skip },
  async () => {
    const { editor, bridge, reported } = await session();
    addColor(editor);
    await bridge.bindPreview(new FakeCanvas(64, 36));
    assert.equal(await bridge.seek(0), true);
    let release;
    const held = new Promise((resolve) => (release = resolve));
    globalThis.stubEngine.render = () => held;
    const inFlight = bridge.seek(4000);
    await settle(5);
    const playing = bridge.play();
    await settle(5);
    const step = bridge.seek(8000);
    globalThis.stubEngine.render = null;
    release();
    await inFlight;
    await playing;
    assert.equal(await step, true);
    await settle(40);
    assert.equal(bridge.playing, false, "the seek stopped the Play");
    assert.equal(bridge.tick, 8000);
    assert.deepEqual(reported, []);
    await bridge.dispose();
    editor.dispose();
  },
);

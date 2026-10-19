// The bridge's media identity and readiness: every decision is the engine's (the packaged wasm),
// fed with this session's originals and kept proxies.
import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { createEditor, TICKS_PER_SECOND } from "../dist/index.js";
import { ConformError, createBrowserBridge } from "../dist/browser/index.js";

const wasmDirectory = join(
  resolve(import.meta.dirname, "..", ".."),
  "alpha2",
  "wasm",
);
const packaged = existsSync(join(wasmDirectory, "alpha2_wasm_bg.wasm"));
// Skips without the packaged wasm (before `npm run build`); ALPHA2_REQUIRE_WASM=1 fails instead.
const skip =
  !packaged &&
  process.env.ALPHA2_REQUIRE_WASM !== "1" &&
  "the wasm package is not built";

async function engine() {
  const wasm = await import(
    pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
  );
  wasm.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
  });
  return wasm;
}

const fingerprint = (digit, extra = {}) => ({
  content_sha256: digit.repeat(64),
  byte_length: 10,
  ...extra,
});

/**
 * A bridge whose media adapter stages any object carrying a fingerprint, and whose proxy store
 * keeps what the test gives it.
 */
async function session({ kept = new Map() } = {}) {
  // Before the editor exists: a missing package then fails the test instead of leaving an editor
  // that keeps the process alive.
  const assetDecisions = await engine();
  const editor = createEditor();
  const bridge = createBrowserBridge({
    editor,
    audio: false,
    assetDecisions,
    mediaAdapter: {
      async stage(file) {
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
      // Numbered frames as one source, as the default adapter stages them: an `image_sequence`
      // provider with no single file and no content hash.
      async stageSequence(files, { frameRate, missing, id }) {
        const runtimeId = id ?? `sequence-${files.length}`;
        const frames = files.map(
          (_, index) => `browser://${runtimeId}/frame/${index + 1}`,
        );
        return {
          runtime: {
            id: runtimeId,
            kind: "video",
            provider: {
              format: "image_sequence",
              frames,
              frame_rate: frameRate,
              start_number: 1,
              missing,
            },
            frameFiles: new Map(
              frames.map((locator, i) => [locator, files[i]]),
            ),
            width: 64,
            height: 36,
          },
          name: "frame[1-n].png",
          description: {},
          fingerprint: {
            streams: {
              video: { width: 64, height: 36, frame_rate: frameRate },
            },
            duration_ticks: Math.round(
              (files.length * frameRate.den * TICKS_PER_SECOND) / frameRate.num,
            ),
          },
          dispose() {},
        };
      },
    },
    proxyStore: {
      async hold() {
        return { release: async () => {} };
      },
      async find(hash) {
        return kept.get(hash) ?? null;
      },
      async generate(file, made_from) {
        const found = {
          record: { made_from, width: 32, height: 18 },
          file: { name: `${file.name}.proxy`, fingerprint: {} },
        };
        kept.set(made_from.content_sha256, found);
        return found;
      },
    },
  });
  return { editor, bridge, kept };
}

function show(editor, sourceId) {
  const result = editor.dispatch({
    type: "track.add",
    track: {
      id: "v1",
      name: "V1",
      kind: "video",
      clips: [
        {
          id: "c",
          source_id: sourceId,
          timeline_range: { start: 0, end: 120000 },
          source_range: { start: 0, end: 120000 },
        },
      ],
    },
  });
  assert.ok(result.ok, result.error?.message);
}

/** The asset as a reopened project has it: recorded, with no file this session. */
function goOffline(editor, id) {
  const asset = editor.document.assets[id];
  const result = editor.dispatch({
    type: "asset.put",
    asset: { ...asset, missing: true },
  });
  assert.ok(result.ok, result.error?.message);
}

test(
  "a relink is refused when the engine finds other media",
  { skip },
  async () => {
    const { editor, bridge } = await session();
    await bridge.importFiles([{ name: "clip", fingerprint: fingerprint("a") }]);
    assert.deepEqual(
      editor.document.project.sources[0].fingerprint,
      fingerprint("a"),
      "the project records the evidence",
    );
    await assert.rejects(
      bridge.relink("clip", { name: "clip", fingerprint: fingerprint("b") }),
      (error) =>
        error.code === "relink_conflict" && /content/.test(error.message),
    );
    // The same bytes, copied: a new date does not make other media.
    await bridge.relink("clip", {
      name: "clip",
      fingerprint: fingerprint("a", { modified_ms: 9 }),
    });
    assert.equal(editor.document.assets.clip.contentRevision, 1);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "without both hashes a relink compares what both recorded, and nothing shared accepts it",
  { skip },
  async () => {
    const { editor, bridge } = await session();
    const recorded = { byte_length: 10, modified_ms: 1 };
    await bridge.importFiles([{ name: "clip", fingerprint: recorded }]);
    // No hash on the recorded side: the length both recorded decides, not the date.
    await assert.rejects(
      bridge.relink("clip", {
        name: "clip",
        fingerprint: fingerprint("a", { byte_length: 11, modified_ms: 1 }),
      }),
      (error) =>
        error.code === "relink_conflict" && /byte length/.test(error.message),
    );
    await bridge.relink("clip", {
      name: "clip",
      fingerprint: fingerprint("a", { modified_ms: 2 }),
    });
    assert.equal(editor.document.assets.clip.contentRevision, 1);
    // The two now record no part in common: unverified, accepted, and the new file's evidence is
    // what the asset records afterwards.
    const unrelated = { duration_ticks: 5 * TICKS_PER_SECOND };
    await bridge.relink("clip", { name: "clip", fingerprint: unrelated });
    assert.equal(editor.document.assets.clip.contentRevision, 2);
    assert.deepEqual(editor.document.assets.clip.fingerprint, unrelated);
    assert.deepEqual(editor.document.project.sources[0].fingerprint, unrelated);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "offline originals preview from kept proxies and never export in their place",
  { skip },
  async () => {
    const { editor, bridge } = await session();
    await bridge.importFiles([{ name: "clip", fingerprint: fingerprint("a") }]);
    show(editor, "clip");
    assert.equal((await bridge.conform("export_original")).ready, true);

    await bridge.generateProxy("clip");
    assert.equal(bridge.proxyStatus("clip").available, true);
    goOffline(editor, "clip");

    const preview = await bridge.conform("preview");
    assert.equal(preview.ready, true);
    assert.deepEqual(preview.sources[0].selection, {
      media: "proxy",
      id: `proxy:${"a".repeat(64)}`,
    });
    const original = await bridge.conform("export_original");
    assert.equal(original.ready, false);
    assert.throws(
      () => bridge.startExport(),
      (error) =>
        error instanceof ConformError &&
        error.report.sources[0].issues[0].kind === "offline",
    );
    // Asked for, a proxy export reads the proxy; the native host receives it as one.
    const native = await bridge.nativeAssets(editor.document, {
      media: "proxies",
    });
    assert.deepEqual(
      native.map(({ id, kind, madeFrom }) => ({ id, kind, madeFrom })),
      [{ id: "clip", kind: "proxies", madeFrom: fingerprint("a") }],
    );
    await assert.rejects(bridge.nativeAssets(editor.document), ConformError);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "a reopened session finds kept proxies by the recorded hash",
  { skip },
  async () => {
    const first = await session();
    await first.bridge.importFiles([
      { name: "clip", fingerprint: fingerprint("a") },
    ]);
    show(first.editor, "clip");
    await first.bridge.generateProxy("clip");
    const saved = first.editor.document;
    await first.bridge.dispose();
    first.editor.dispose();

    const second = await session({ kept: first.kept });
    second.editor.open({
      ...saved,
      assets: { clip: { ...saved.assets.clip, missing: true } },
    });
    const report = await second.bridge.conform("preview");
    assert.equal(report.ready, true);
    assert.equal(report.sources[0].selection.media, "proxy");
    const third = await session({ kept: first.kept });
    third.editor.open({
      ...saved,
      assets: {
        clip: {
          ...saved.assets.clip,
          missing: true,
          fingerprint: fingerprint("c"),
        },
      },
    });
    assert.equal((await third.bridge.conform("preview")).ready, false);
    for (const { bridge, editor } of [second, third]) {
      await bridge.dispose();
      editor.dispose();
    }
  },
);

test(
  "an image sequence relinks to its frames under the same asset",
  { skip },
  async () => {
    const frames = (n) =>
      Array.from({ length: n }, (_, i) => ({ name: `frame_${i + 1}.png` }));
    const first = await session();
    const rate = { num: 24, den: 1 };
    const { assetIds } = await first.bridge.importSequence(frames(4), {
      frameRate: rate,
      missing: "hold",
    });
    const [id] = assetIds;
    show(first.editor, id);
    const saved = first.editor.document;
    await first.bridge.dispose();
    first.editor.dispose();

    const second = await session();
    second.editor.open(saved);
    assert.equal(second.bridge.getAsset(id), null);
    await assert.rejects(
      second.bridge.relinkSequence(id, frames(6)),
      (error) =>
        error.code === "relink_conflict" && /duration/.test(error.message),
    );
    const relinked = await second.bridge.relinkSequence(id, frames(4));
    assert.deepEqual(relinked.assetIds, [id]);
    assert.ok(second.bridge.getAsset(id), "the sequence is online");
    const source = second.editor.document.project.sources.find(
      (candidate) => candidate.id === id,
    );
    assert.deepEqual(source.provider.frame_rate, rate);
    assert.equal(source.provider.missing, "hold");
    assert.equal(second.editor.document.assets[id].contentRevision, 1);
    assert.equal((await second.bridge.conform("export_original")).ready, true);
    await second.bridge.importFiles([
      { name: "clip", fingerprint: fingerprint("a") },
    ]);
    await assert.rejects(
      second.bridge.relinkSequence("clip", frames(4)),
      (error) => error.code === "relink_conflict",
    );
    await second.bridge.dispose();
    second.editor.dispose();
  },
);

test(
  "a source read through a provider has no proxy to make",
  { skip },
  async () => {
    const { editor, bridge } = await session();
    const {
      assetIds: [id],
    } = await bridge.importSequence([{ name: "a.png" }, { name: "b.png" }], {
      frameRate: { num: 24, den: 1 },
    });
    await assert.rejects(
      bridge.generateProxy(id),
      (error) => error.code === "unsupported_media",
    );
    await bridge.dispose();
    editor.dispose();
  },
);

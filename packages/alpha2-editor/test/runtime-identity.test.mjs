// Which runtime the bridge draws for an asset, and how long it keeps it: what the document holds
// now decides both, whatever the edit history did to reach it.
import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { createEditor } from "../dist/index.js";
import * as browser from "../dist/browser/index.js";
import {
  missingCapability,
  suiteOf,
} from "../../../scripts/lib/capabilities.mjs";

const { createBrowserBridge } = browser;

const wasmDirectory = join(
  resolve(import.meta.dirname, "..", ".."),
  "alpha2",
  "wasm",
);
const packaged = existsSync(join(wasmDirectory, "alpha2_wasm_bg.wasm"));
// As in assets.test.mjs: skipped before `npm run build`, failed when ALPHA2_REQUIRE=wasm finds no
// package.
const skip =
  !packaged &&
  missingCapability(
    "wasm",
    suiteOf(import.meta.url),
    "the wasm package is not built",
  );

async function engine() {
  const wasm = await import(
    pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
  );
  wasm.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
  });
  return wasm;
}

/** Enough of OffscreenCanvas for the text rasterizer to lay out and fill text in Node. */
class FakeCanvas {
  constructor(width, height) {
    this.width = width;
    this.height = height;
  }
  getContext() {
    return {
      font: "",
      measureText: (text) => ({
        width: text.length * 10,
        actualBoundingBoxLeft: 0,
        actualBoundingBoxRight: text.length * 10,
        actualBoundingBoxAscent: 8,
        actualBoundingBoxDescent: 2,
      }),
      fillText() {},
      putImageData() {},
      drawImage() {},
      getImageData: (x, y, w, h) => ({
        data: new Uint8ClampedArray(w * h * 4),
      }),
    };
  }
  toDataURL() {
    return "data:,";
  }
}
globalThis.OffscreenCanvas ??= FakeCanvas;
globalThis.ImageData ??= class {
  constructor(data, width, height) {
    Object.assign(this, { data, width, height });
  }
};
// The text thumbnail is a DOM canvas.
globalThis.document ??= {
  createElement: () => new FakeCanvas(1, 1),
  addEventListener() {},
  removeEventListener() {},
};

const fingerprint = (digit) => ({
  content_sha256: digit.repeat(64),
  byte_length: 10,
});

/**
 * Stages what the test describes on the file object: a font, a provider file (Lottie package or
 * OpenEXR still), or plain video.
 */
const mediaAdapter = {
  async stage(file) {
    if (file.kind === "font") {
      const id = crypto.randomUUID();
      return {
        runtime: {
          id,
          kind: "font",
          name: file.name,
          bytes: file.bytes ?? new Uint8Array(1),
          faceIndex: 0,
          family: `alpha2-font-${id}`,
        },
        description: { id, kind: "font", name: file.name },
        fingerprint: file.fingerprint,
        dispose() {},
      };
    }
    const provider =
      file.kind === "lottie"
        ? { format: "lottie", animation: file.items[0] }
        : file.kind === "exr" || file.kind === "svg"
          ? { format: file.kind }
          : undefined;
    const report =
      file.kind === "lottie"
        ? { items: file.items }
        : file.kind === "exr"
          ? { items: [], color: { primaries: "bt709" } }
          : file.kind === "svg"
            ? { items: [], diagnostics: file.probed }
            : undefined;
    return {
      runtime: {
        id: file.name,
        kind: file.kind === "exr" || file.kind === "svg" ? "image" : "video",
        name: file.name,
        file,
        width: 64,
        height: 36,
        ...(provider ? { provider, report } : {}),
      },
      description: provider ? { provider } : {},
      fingerprint: file.fingerprint,
      dispose() {},
    };
  },
};

async function session({ kept = new Map(), runtimeAssets } = {}) {
  const assetDecisions = await engine();
  const editor = createEditor();
  const bridge = createBrowserBridge({
    editor,
    audio: false,
    assetDecisions,
    mediaAdapter,
    ...(runtimeAssets ? { runtimeAssets } : {}),
    proxyStore: {
      async hold() {
        return { release: async () => {} };
      },
      async find(hash) {
        return kept.get(hash) ?? null;
      },
    },
  });
  return { editor, bridge };
}

const ok = (result) => assert.ok(result.ok, result.error?.message);

function addText(editor, id, text) {
  ok(
    editor.dispatch({
      type: "asset.put",
      asset: {
        id,
        kind: "text",
        name: text.content,
        contentRevision: 0,
        authoring: { text },
      },
      source: { id, kind: "image", locator: `browser://${id}` },
    }),
  );
}

/** A Text inspector commit: the next revision counted from the document. */
function editText(editor, id, patch) {
  const asset = editor.document.assets[id];
  ok(
    editor.dispatch({
      type: "asset.put",
      asset: {
        ...asset,
        contentRevision: asset.contentRevision + 1,
        authoring: {
          ...asset.authoring,
          text: { ...asset.authoring.text, ...patch },
        },
      },
    }),
  );
}

/** Lets the bridge finish the work an edit schedules. */
const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

const TEXT = {
  content: "Title",
  fontSize: 72,
  color: "#ffffff",
  fontFamily: "sans-serif",
  align: "center",
};

test(
  "a text edit after an undo draws what the document holds, not the undone edit",
  { skip },
  async () => {
    const { editor, bridge } = await session();
    addText(editor, "t", TEXT);
    await bridge.conform("preview");
    editText(editor, "t", { content: "Hello" });
    await bridge.conform("preview");
    assert.equal(bridge.getAsset("t").text.content, "Hello");
    editor.undo();
    await bridge.conform("preview");
    assert.equal(bridge.getAsset("t").text.content, "Title");
    editText(editor, "t", { content: "World" });
    assert.equal(editor.document.assets.t.contentRevision, 1);
    await bridge.conform("preview");
    assert.equal(bridge.getAsset("t").text.content, "World");
    // Redo is gone, and so is every way back to "Hello": a second undo/edit pair draws the new text
    // too.
    editor.undo();
    editText(editor, "t", { fontSize: 96 });
    await bridge.conform("preview");
    assert.equal(bridge.getAsset("t").text.fontSize, 96);
    assert.equal(bridge.getAsset("t").text.content, "Title");
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "a relink after undoing an earlier relink takes a revision of its own",
  { skip },
  async () => {
    const { editor, bridge } = await session();
    const clip = () => ({ name: "clip", fingerprint: fingerprint("a") });
    await bridge.importFiles([clip()]);
    await bridge.relink("clip", clip());
    editor.undo();
    ok(
      editor.dispatch({
        type: "project.update",
        patch: { name: "unrelated" },
      }),
    );
    await bridge.relink("clip", clip());
    const revision = editor.document.assets.clip.contentRevision;
    assert.ok(revision > 1, `relinked at revision ${revision}`);
    assert.ok(bridge.getAsset("clip"));
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "a title drawn before its font was linked is drawn again with that font",
  { skip },
  async () => {
    const { editor, bridge } = await session();
    // A reopened project: the font is recorded, its file not linked yet.
    ok(
      editor.dispatch({
        type: "asset.put",
        asset: {
          id: "f",
          kind: "font",
          name: "Brand.ttf",
          contentRevision: 0,
          missing: true,
          fingerprint: fingerprint("f"),
        },
      }),
    );
    addText(editor, "t", { ...TEXT, fontFamily: "Brand" });
    await bridge.conform("preview");
    assert.equal(bridge.getAsset("t").text.fontFamily, "Brand");
    await bridge.relink("f", {
      kind: "font",
      name: "Brand.ttf",
      fingerprint: fingerprint("f"),
    });
    await bridge.conform("preview");
    assert.match(bridge.getAsset("t").text.fontFamily, /^alpha2-font-/);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "text edits release what no document can show, so the budget is not spent by editing",
  { skip },
  async () => {
    const { editor, bridge } = await session();
    addText(editor, "t", TEXT);
    for (let edit = 0; edit < 200; edit++) {
      editText(editor, "t", { fontSize: 20 + edit });
      await bridge.conform("preview");
    }
    await settle();
    assert.ok(bridge.resourceCount <= 2, `${bridge.resourceCount} held`);
    assert.equal(bridge.getAsset("t").text.fontSize, 219);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "opening another document releases the media of the one before",
  { skip },
  async () => {
    const { editor, bridge } = await session();
    await bridge.importFiles([{ name: "a", fingerprint: fingerprint("a") }]);
    await bridge.importFiles([{ name: "b", fingerprint: fingerprint("b") }]);
    assert.equal(bridge.resourceCount, 2);
    ok(editor.dispatch({ type: "asset.remove", id: "a" }));
    await settle();
    assert.equal(bridge.resourceCount, 2);
    editor.open(createEditor().document);
    await settle();
    assert.equal(bridge.resourceCount, 0);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "undoing an animation choice shows the animation the document names again",
  { skip },
  async () => {
    const { editor, bridge } = await session();
    await bridge.importFiles([
      {
        name: "pack",
        kind: "lottie",
        items: ["intro", "outro"],
        fingerprint: fingerprint("c"),
      },
    ]);
    const before = bridge.getAsset("pack");
    bridge.selectAnimation("pack", "outro");
    const chosen = bridge.getAsset("pack");
    assert.equal(chosen.provider.animation, "outro");
    // A new value for a new choice: what a preview worker holds by identity is resent, and nothing
    // that held the old one sees it change.
    assert.notEqual(chosen, before);
    assert.equal(before.provider.animation, "intro");
    editor.undo();
    assert.equal(bridge.getAsset("pack").provider.animation, "intro");
    editor.redo();
    assert.equal(bridge.getAsset("pack").provider.animation, "outro");
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "relinking the same file keeps the animation and primaries the project chose",
  { skip },
  async () => {
    const { editor, bridge } = await session();
    const pack = () => ({
      name: "pack",
      kind: "lottie",
      items: ["intro", "outro"],
      fingerprint: fingerprint("c"),
    });
    const still = () => ({
      name: "still",
      kind: "exr",
      fingerprint: fingerprint("d"),
    });
    await bridge.importFiles([pack(), still()]);
    bridge.selectAnimation("pack", "outro");
    bridge.setLinearPrimaries("still", "aces_ap1");
    await bridge.relink("pack", pack());
    await bridge.relink("still", still());
    const provider = (id) =>
      editor.document.project.sources.find((s) => s.id === id).provider;
    assert.equal(provider("pack").animation, "outro");
    assert.equal(bridge.getAsset("pack").provider.animation, "outro");
    assert.equal(provider("still").primaries, "aces_ap1");
    assert.equal(bridge.getAsset("still").provider.primaries, "aces_ap1");
    // A package without the chosen animation falls back to its first.
    await bridge.relink("pack", { ...pack(), items: ["intro", "loop"] });
    assert.equal(provider("pack").animation, "intro");
    assert.equal(bridge.getAsset("pack").provider.animation, "intro");
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "a kept proxy standing in for an offline original does not count as the original",
  { skip },
  async () => {
    const kept = new Map([
      [
        "e".repeat(64),
        {
          record: {
            made_from: fingerprint("e"),
            width: 32,
            height: 18,
          },
          file: { name: "clip.proxy", fingerprint: {} },
        },
      ],
    ]);
    const { editor, bridge } = await session({ kept });
    // A reopened project: the clip is recorded, its file not linked yet.
    ok(
      editor.dispatch({
        type: "asset.put",
        asset: {
          id: "clip",
          kind: "video",
          name: "clip",
          contentRevision: 0,
          missing: true,
          fingerprint: fingerprint("e"),
        },
        source: {
          id: "clip",
          kind: "video",
          locator: "browser://clip",
          fingerprint: fingerprint("e"),
        },
      }),
    );
    await bridge.conform("preview");
    assert.equal(bridge.proxyStatus("clip").available, true);
    assert.equal(bridge.hasOriginal("clip"), false);
    await bridge.relink("clip", {
      name: "clip",
      fingerprint: fingerprint("e"),
    });
    assert.equal(bridge.hasOriginal("clip"), true);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "a text too large to draw fails only the renders that show it, by its name",
  { skip },
  async () => {
    const { editor, bridge } = await session();
    addText(editor, "t", TEXT);
    // A line wider than any texture: the rasterizer refuses it.
    addText(editor, "wide", { ...TEXT, content: `Wide ${"x".repeat(2_000)}` });
    await bridge.conform("preview");
    assert.equal(bridge.getAsset("t").text.content, "Title");
    assert.deepEqual(await bridge.nativeAssets(editor.document), []);
    ok(
      editor.dispatch({
        type: "track.add",
        track: {
          id: "v1",
          name: "V1",
          kind: "video",
          clips: [
            {
              id: "c",
              source_id: "wide",
              timeline_range: { start: 0, end: 120000 },
              source_range: { start: 0, end: 120000 },
            },
          ],
        },
      }),
    );
    await assert.rejects(
      bridge.nativeAssets(editor.document),
      (error) =>
        error.code === "text_unrenderable" &&
        error.entityId === "wide" &&
        /^Text "Wide x+" cannot be drawn: text texture dimension is unsupported$/.test(
          error.message,
        ),
    );
    editText(editor, "wide", { maxWidth: 400 });
    await bridge.conform("preview");
    assert.equal(bridge.getAsset("wide").text.maxWidth, 400);
    await bridge.dispose();
    editor.dispose();
  },
);

/**
 * Workers running the extensions worker's handler in this thread, with messages crossing as tasks:
 * each lists the calls posted to it.
 */
function extensionsWorkers() {
  const made = [];
  const factory = () => {
    const worker = {
      posted: [],
      terminated: false,
      terminate() {
        worker.terminated = true;
      },
      postMessage(message) {
        worker.posted.push(message.call);
        setTimeout(() => scope.onmessage({ data: message }));
      },
    };
    const scope = {
      onmessage: null,
      postMessage(answer) {
        if (!worker.terminated)
          setTimeout(() => worker.onmessage?.({ data: answer }));
      },
    };
    browser.installExtensionsWorker?.(scope);
    made.push(worker);
    return worker;
  };
  return { factory, made };
}

for (const where of ["the page's thread", "an extensions worker"])
  test(
    `an SVG's report says whether the fonts its text draws with draw it, opened on ${where}`,
    { skip },
    async () => {
      // The import probe's SVG report must count the document's fonts.
      const glue = await import("alpha2/extensions-wasm");
      glue.initSync({
        module: readFileSync(join(wasmDirectory, "alpha2_extensions_bg.wasm")),
      });
      const workers = extensionsWorkers();
      const { editor, bridge } = await session(
        where === "an extensions worker"
          ? { runtimeAssets: { extensionsWorkerFactory: workers.factory } }
          : {},
      );
      let heard = 0;
      bridge.subscribeAssets(() => (heard += 1));
      const svg = new TextEncoder().encode(
        '<svg xmlns="http://www.w3.org/2000/svg" width="64" height="16"><text x="0" y="12" font-family="DejaVu Sans" font-size="12">Ab</text></svg>',
      );
      const missing = [{ kind: "missing_font", families: "DejaVu Sans" }];
      await bridge.importFiles([
        {
          name: "logo",
          kind: "svg",
          probed: missing,
          fingerprint: fingerprint("e"),
          arrayBuffer: async () => svg.slice().buffer,
        },
      ]);
      /** Waits for the bridge's report of the SVG to say `expected`. */
      const reportSays = async (expected) => {
        for (let wait = 0; wait < 200; wait++) {
          const { diagnostics } = bridge.getAsset("logo").report;
          if (JSON.stringify(diagnostics) === JSON.stringify(expected)) return;
          await new Promise((resolve) => setTimeout(resolve, 5));
        }
        assert.deepEqual(bridge.getAsset("logo").report.diagnostics, expected);
      };
      await reportSays(missing);
      const font = new Uint8Array(
        readFileSync(
          join(
            resolve(import.meta.dirname, "..", "..", ".."),
            "tests/fonts/Alpha2Fixture.ttf",
          ),
        ),
      );
      await bridge.importFiles([
        {
          name: "DejaVuSans.ttf",
          kind: "font",
          bytes: font,
          fingerprint: fingerprint("d"),
        },
      ]);
      const before = heard;
      await reportSays([]);
      assert.ok(heard > before, "listeners hear of the new report");
      const fontId = Object.values(editor.document.assets).find(
        (asset) => asset.kind === "font",
      ).id;
      ok(editor.dispatch({ type: "asset.remove", id: fontId }));
      await reportSays(missing);
      // Each report was opened in a worker of its own, terminated once it answered.
      if (where === "an extensions worker") {
        assert.ok(workers.made.length >= 2, `${workers.made.length} workers`);
        for (const worker of workers.made) {
          assert.deepEqual(worker.posted, ["svg-diagnostics"]);
          assert.equal(worker.terminated, true);
        }
      } else assert.equal(workers.made.length, 0);
      await bridge.dispose();
      editor.dispose();
    },
  );

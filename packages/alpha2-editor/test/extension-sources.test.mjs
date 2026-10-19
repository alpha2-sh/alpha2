// How the browser runtime opens extension sources when opening fails or the provider module
// restarts. Node cannot fetch the packaged module, so the test hands it over.
import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { SourcePixels } from "../dist/browser/runtime/sources.js";

const wasmDirectory = join(
  resolve(import.meta.dirname, "..", ".."),
  "alpha2",
  "wasm",
);
const packaged = existsSync(join(wasmDirectory, "alpha2_extensions_bg.wasm"));
// Skips without the packaged wasm (before `npm run build`); ALPHA2_REQUIRE_WASM=1 fails instead.
const skip =
  !packaged &&
  process.env.ALPHA2_REQUIRE_WASM !== "1" &&
  "the extension wasm package is not built";

const svg = (color) =>
  new TextEncoder().encode(
    `<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8" fill="${color}"/></svg>`,
  );

/** A file whose first `failures` reads fail, as a revoked or busy file does. */
function file(bytes, failures = 0) {
  let reads = 0;
  return {
    async arrayBuffer() {
      reads += 1;
      if (reads <= failures) throw new Error("the file could not be read");
      return bytes.slice().buffer;
    },
  };
}

test(
  "an extension source that failed to load opens on the next frame, and one the module restart left behind opens again",
  { skip },
  async () => {
    const glue = await import("alpha2/extensions-wasm");
    const pixels = new SourcePixels(null, {
      extensionsWasmUrl: new URL("file:///alpha2/missing.wasm"),
    });
    pixels.setOutputSize(8, 8);
    const red = {
      id: "red",
      provider: { format: "svg" },
      file: file(svg("#f00"), 1),
    };

    // The module does not load: that is not the asset's answer, and is not kept.
    await assert.rejects(pixels.pixelsFor(red, 0));
    glue.initSync({
      module: readFileSync(join(wasmDirectory, "alpha2_extensions_bg.wasm")),
    });
    await assert.rejects(pixels.pixelsFor(red, 0), /could not be read/);
    const frame = await pixels.pixelsFor(red, 0);
    assert.deepEqual([...frame.pixels.subarray(0, 4)], [255, 0, 0, 255]);

    const blue = {
      id: "blue",
      provider: { format: "svg" },
      file: file(svg("#00f")),
    };
    await pixels.pixelsFor(blue, 0);
    // At another size each source draws again, rather than answering with the frame it showed last.
    pixels.setOutputSize(16, 16);
    // The next registry call reads past the end of the instance's memory while it holds the
    // registry borrowed, as a panic inside a provider leaves it.
    const render = glue.ExtensionRegistry.prototype.render;
    glue.ExtensionRegistry.prototype.render = function (...args) {
      glue.ExtensionRegistry.prototype.render = render;
      glue
        .initSync()
        .extensionregistry_open(this.__wbg_ptr, 0xffffff00, 200, 0, 0);
      return render.apply(this, args);
    };
    try {
      await assert.rejects(pixels.pixelsFor(red, 0), {
        name: "ExtensionModuleError",
      });
    } finally {
      glue.ExtensionRegistry.prototype.render = render;
    }
    const again = await pixels.pixelsFor(blue, 0);
    assert.deepEqual([...again.pixels.subarray(0, 4)], [0, 0, 255, 255]);
    await assert.rejects(pixels.pixelsFor(red, 0), {
      name: "ExtensionModuleError",
    });
  },
);

test(
  "an image sequence reads the frame it shows, and leaves nothing registered",
  { skip },
  async () => {
    // Opening a sequence must release its frame files from the shared provider module.
    const glue = await import("alpha2/extensions-wasm");
    glue.initSync({
      module: readFileSync(join(wasmDirectory, "alpha2_extensions_bg.wasm")),
    });
    const { loadExtensions } = await import("alpha2/extensions");
    const extensions = await loadExtensions();
    const corpus = join(
      resolve(import.meta.dirname, "..", "..", ".."),
      "tests/corpus/extensions",
    );
    const reads = [];
    const frameFile = (locator, number) => ({
      async arrayBuffer() {
        reads.push(locator);
        return Uint8Array.from(
          readFileSync(
            join(corpus, `sequence_${String(number).padStart(4, "0")}.png`),
          ),
        ).buffer;
      },
    });
    const locators = [7, 8, 9, 10].map(
      (number) => `browser://s/frame/${number}`,
    );
    const asset = {
      id: "sequence",
      provider: {
        format: "image_sequence",
        frames: locators,
        frame_rate: { num: 10, den: 1 },
        start_number: 7,
        missing: "reject",
      },
      frameFiles: new Map(
        locators.map((locator, index) => [
          locator,
          frameFile(locator, index + 7),
        ]),
      ),
    };
    const preview = new SourcePixels(null);
    const exporting = new SourcePixels(null);
    preview.setOutputSize(48, 32);
    exporting.setOutputSize(48, 32);
    const frame = await preview.pixelsFor(asset, 0.25);
    assert.equal(frame.width, 48);
    assert.deepEqual(reads.splice(0), [locators[0], locators[2]]);
    assert.equal(extensions.resourceBytes, 0);
    // An export's own source reads its own frames, the preview's untouched.
    await exporting.pixelsFor(asset, 0.35);
    assert.deepEqual(reads.splice(0), [locators[0], locators[3]]);
    assert.equal(extensions.resourceBytes, 0);
    const again = await preview.pixelsFor(asset, 0.25);
    assert.deepEqual(reads, [], "the frame shown is kept decoded");
    assert.deepEqual(again.pixels, frame.pixels);
    await preview.dispose();
    await exporting.dispose();
    assert.equal(extensions.resourceBytes, 0);
  },
);

test(
  "a long image sequence is not serialized again for each frame it shows, and a new provider setting still opens it again",
  { skip },
  async () => {
    // Reads must not serialize the provider settings (6 MB per frame for 100,000 frames).
    const glue = await import("alpha2/extensions-wasm");
    glue.initSync({
      module: readFileSync(join(wasmDirectory, "alpha2_extensions_bg.wasm")),
    });
    const corpus = join(
      resolve(import.meta.dirname, "..", "..", ".."),
      "tests/corpus/extensions",
    );
    const png = readFileSync(join(corpus, "sequence_0007.png"));
    let reads = 0;
    const frameFile = {
      async arrayBuffer() {
        reads += 1;
        return Uint8Array.from(png).buffer;
      },
    };
    const frames = Array.from(
      { length: 100_000 },
      (_, i) => `browser://0f5e1c7a-5b3e-4c39-9d0e-2b6f0f7e1a11/frame/${i + 1}`,
    );
    const asset = {
      id: "long",
      kind: "video",
      provider: {
        format: "image_sequence",
        frames,
        frame_rate: { num: 30, den: 1 },
        start_number: 1,
        missing: "reject",
      },
      frameFiles: new Map(frames.map((locator) => [locator, frameFile])),
    };
    const pixels = new SourcePixels(null);
    pixels.setOutputSize(48, 32);
    await pixels.pixelsFor(asset, 0);
    const stringify = JSON.stringify;
    let large = 0;
    JSON.stringify = function (...args) {
      const text = stringify.apply(this, args);
      if (text?.length > 100_000) large += 1;
      return text;
    };
    try {
      for (let frame = 1; frame <= 60; frame++)
        await pixels.pixelsFor(asset, frame / 30);
    } finally {
      JSON.stringify = stringify;
    }
    assert.equal(large, 0);

    // The frame shown last answers again while the source stays open; an equal provider keeps it
    // open, and a changed setting opens it again, which reads the frame again.
    const at = 60 / 30;
    reads = 0;
    await pixels.pixelsFor(asset, at);
    assert.equal(reads, 0);
    asset.provider = { ...asset.provider };
    await pixels.pixelsFor(asset, at);
    assert.equal(reads, 0);
    asset.provider = { ...asset.provider, missing: "hold" };
    await pixels.pixelsFor(asset, at);
    assert.ok(reads > 0);
    await pixels.dispose();
  },
);

/**
 * Starts the editor's `Engine` on `renderer`, in place of a WebGPU canvas, with the real
 * `SourcePixels` for `library`.
 */
async function startEngine(renderer, library) {
  const wasm = await import("alpha2/wasm");
  wasm.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
  });
  const { Engine } = await import("../dist/browser/runtime/engine.js");
  const restore = [];
  for (const [name, value] of [
    ["OffscreenCanvas", class OffscreenCanvas {}],
    ["VideoFrame", class VideoFrame {}],
  ]) {
    if (!(name in globalThis)) {
      globalThis[name] = value;
      restore.push(() => delete globalThis[name]);
    }
  }
  const create = wasm.WebEffectRegistry.createForCanvas;
  wasm.WebEffectRegistry.createForCanvas = async () => renderer;
  try {
    const engine = new Engine(
      new globalThis.OffscreenCanvas(),
      new SourcePixels(library),
      library,
    );
    await engine.start();
    return { engine, restore };
  } finally {
    wasm.WebEffectRegistry.createForCanvas = create;
  }
}

/**
 * A renderer that holds one still per source until the page releases it, as the wasm frame store
 * does, and keeps what was uploaded.
 */
function stillRenderer(sourceId) {
  let generation = 0;
  const held = new Map();
  const renderer = {
    uploads: [],
    captionFonts: [],
    deviceLost: false,
    supportsExternalVideoFrame: false,
    builtinCatalog: () => JSON.stringify({ effects: [], transitions: [] }),
    setResultCacheBudget() {},
    loadProject() {},
    resetPlayer() {},
    clearFonts() {
      renderer.captionFonts = [];
    },
    addFont(name) {
      renderer.captionFonts.push(name);
    },
    frameNeeds: () =>
      JSON.stringify([{ clipId: sourceId, sourceId, mediaTimeTicks: "0" }]),
    uploadPresentedRgbaImage(id, width, height, pixels) {
      generation += 1;
      held.set(id, generation);
      renderer.uploads.push({ id, width, height, pixels: pixels.slice() });
      return { generation: BigInt(generation), retainedBytes: pixels.length };
    },
    holdsImage: (id, held_) => held.get(id) === Number(held_),
    releaseSourceFrames(id) {
      held.delete(id);
    },
    releaseFrameGeneration() {},
    renderAt: () =>
      JSON.stringify({
        requestedTimeTicks: 0,
        presentedTimeTicks: 0,
        missingSources: [],
        repeated: false,
      }),
  };
  return renderer;
}

/** How many pixels of a premultiplied RGBA8 raster something covers. */
const covered = (pixels) => {
  let count = 0;
  for (let index = 3; index < pixels.length; index += 4)
    if (pixels[index] > 0) count += 1;
  return count;
};

test(
  "an SVG's text draws with the fonts the engine draws captions with, and again when they change",
  { skip },
  async () => {
    // SVG text needs the caption fonts registered with the provider module, as natively.
    const glue = await import("alpha2/extensions-wasm");
    glue.initSync({
      module: readFileSync(join(wasmDirectory, "alpha2_extensions_bg.wasm")),
    });
    const font = {
      name: "DejaVu Sans",
      bytes: new Uint8Array(
        readFileSync(
          join(
            resolve(import.meta.dirname, "..", "..", ".."),
            "tests/fonts/Alpha2Fixture.ttf",
          ),
        ),
      ),
      faceIndex: 0,
    };
    const label = {
      id: "label",
      kind: "image",
      provider: { format: "svg" },
      file: file(
        new TextEncoder().encode(
          '<svg xmlns="http://www.w3.org/2000/svg" width="64" height="32"><text x="2" y="26" font-family="DejaVu Sans" font-size="26" fill="#000">Ag</text></svg>',
        ),
      ),
    };
    const library = new Map([["label", label]]);
    const renderer = stillRenderer("label");
    const { engine, restore } = await startEngine(renderer, library);
    try {
      const context = {
        width: 64,
        height: 32,
        liveSourceIds: new Set(["label"]),
        getLayer: (id) => library.get(id),
      };
      engine.loadProject({ effect_resources: [] });
      engine.setCaptionFonts([font]);
      assert.equal(await engine.render(0, { context }), true);
      assert.equal(renderer.uploads.length, 1);
      const drawn = covered(renderer.uploads[0].pixels);
      assert.ok(drawn > 50, `the text is drawn: ${drawn} pixels`);

      // Without the font the text is left out, as natively.
      engine.setCaptionFonts([]);
      assert.equal(await engine.render(0, { context }), true);
      assert.equal(renderer.uploads.length, 2, "drawn again");
      assert.equal(covered(renderer.uploads[1].pixels), 0);

      engine.setCaptionFonts([font]);
      assert.equal(await engine.render(0, { context }), true);
      assert.equal(renderer.uploads.length, 3);
      assert.deepEqual(renderer.uploads[2].pixels, renderer.uploads[0].pixels);
      // The SVG's content names its fonts; unchanged fonts are the same content.
      const content = engine.sources.contentOf(label);
      engine.setCaptionFonts([font]);
      assert.equal(engine.sources.contentOf(label), content);
      engine.setCaptionFonts([]);
      assert.notEqual(engine.sources.contentOf(label), content);
      const video = { id: "video", kind: "video" };
      assert.equal(
        engine.sources.contentOf(video),
        video,
        "fonts draw no video",
      );
    } finally {
      await engine.sources.dispose();
      for (const undo of restore) undo();
    }
  },
);

test(
  "a provider frame answers every time it covers as the same pixels, until the size changes",
  { skip },
  async () => {
    // A provider still renders once and stays cached across preview frames.
    const glue = await import("alpha2/extensions-wasm");
    glue.initSync({
      module: readFileSync(join(wasmDirectory, "alpha2_extensions_bg.wasm")),
    });
    const pixels = new SourcePixels(null);
    pixels.setOutputSize(8, 8);
    const red = {
      id: "red-still",
      provider: { format: "svg" },
      file: file(svg("#f00")),
    };
    let renders = 0;
    const render = glue.ExtensionRegistry.prototype.render;
    glue.ExtensionRegistry.prototype.render = function (...args) {
      renders += 1;
      return render.apply(this, args);
    };
    try {
      const first = await pixels.pixelsFor(red, 0);
      const later = await pixels.pixelsFor(red, 12.5);
      assert.equal(later.pixels, first.pixels, "the same pixels");
      assert.equal(renders, 1);
      pixels.setOutputSize(16, 16);
      const larger = await pixels.pixelsFor(red, 12.5);
      assert.equal(renders, 2);
      assert.equal(larger.width, 16);
    } finally {
      glue.ExtensionRegistry.prototype.render = render;
    }
  },
);

test(
  "open extension sources are held to a byte budget: the least recently shown close, and open again when shown",
  { skip },
  async () => {
    // Open provider sources are bounded by providerSourceBytes, like the native host.
    const glue = await import("alpha2/extensions-wasm");
    glue.initSync({
      module: readFileSync(join(wasmDirectory, "alpha2_extensions_bg.wasm")),
    });
    let opens = 0;
    let releases = 0;
    const { open, release } = glue.ExtensionRegistry.prototype;
    glue.ExtensionRegistry.prototype.open = function (...args) {
      opens += 1;
      return open.apply(this, args);
    };
    glue.ExtensionRegistry.prototype.release = function (...args) {
      releases += 1;
      return release.apply(this, args);
    };
    try {
      const asset = (id, color) => ({
        id,
        provider: { format: "svg" },
        file: file(svg(color)),
      });
      const sources = [
        asset("a", "#f00"),
        asset("b", "#0f0"),
        asset("c", "#00f"),
      ];
      const live = new Set(sources.map((s) => s.id));
      // Two 32x32 sources fit (raster in the module plus last frame here); three do not.
      const pixels = new SourcePixels(null, { providerSourceBytes: 20_000 });
      pixels.setOutputSize(32, 32);
      const show = async (source) => {
        await pixels.release(live);
        return pixels.pixelsFor(source, 0);
      };
      for (const source of sources) await show(source);
      assert.equal(opens, 3);
      assert.equal(pixels.diagnostics.openProviderSources, 2);
      assert.ok(pixels.diagnostics.providerSourceBytes <= 20_000);
      await new Promise((resolve) => setTimeout(resolve, 0));
      assert.equal(releases, 1);
      const again = await show(sources[0]);
      assert.deepEqual([...again.pixels.subarray(0, 4)], [255, 0, 0, 255]);
      assert.equal(opens, 4);
      assert.equal(pixels.diagnostics.providerReopens, 1);
      // A frame's own sources and the previous frame's stay open, even past the budget.
      const tight = new SourcePixels(null, { providerSourceBytes: 1 });
      tight.setOutputSize(32, 32);
      await tight.release(live);
      for (const source of sources) await tight.pixelsFor(source, 0);
      assert.equal(tight.diagnostics.openProviderSources, 3);
    } finally {
      glue.ExtensionRegistry.prototype.open = open;
      glue.ExtensionRegistry.prototype.release = release;
    }
  },
);

test(
  "each clip reading an animated image has a source of its own, which it reads forward; other providers' sources are shared by their asset",
  { skip },
  async () => {
    // Each clip of one animated image needs its own source, or far-apart clips redecode from 0.
    const glue = await import("alpha2/extensions-wasm");
    glue.initSync({
      module: readFileSync(join(wasmDirectory, "alpha2_extensions_bg.wasm")),
    });
    const corpus = join(
      resolve(import.meta.dirname, "..", "..", ".."),
      "tests/corpus/extensions",
    );
    const opened = [];
    const renders = new Map();
    const released = [];
    const { open, render, release } = glue.ExtensionRegistry.prototype;
    glue.ExtensionRegistry.prototype.open = function (...args) {
      const handle = open.apply(this, args);
      opened.push({ handle, format: JSON.parse(args[0]).format });
      return handle;
    };
    glue.ExtensionRegistry.prototype.render = function (
      handle,
      ticks,
      ...rest
    ) {
      if (!renders.has(handle)) renders.set(handle, []);
      renders.get(handle).push(Number(ticks));
      return render.call(this, handle, ticks, ...rest);
    };
    glue.ExtensionRegistry.prototype.release = function (handle) {
      released.push(handle);
      return release.call(this, handle);
    };
    try {
      const gif = {
        id: "gif",
        kind: "video",
        provider: { format: "animated_image" },
        file: file(new Uint8Array(readFileSync(join(corpus, "animation.gif")))),
      };
      const label = {
        id: "label",
        kind: "image",
        provider: { format: "svg" },
        file: file(svg("#f00")),
      };
      const live = new Set(["gif", "label"]);
      const pixels = new SourcePixels(null);
      pixels.setOutputSize(48, 32);
      // Two clips of the GIF, the second 0.2 s ahead (frames of 100, 50, 200, and 100 ms), both
      // over the same SVG, frame after frame.
      for (let step = 0; step < 5; step += 1) {
        await pixels.release(live);
        const time = step * 0.05;
        await pixels.pixelsFor(gif, time, { consumerId: "clipA" });
        await pixels.pixelsFor(gif, time + 0.2, { consumerId: "clipB" });
        await pixels.pixelsFor(label, time, { consumerId: "clipA" });
        await pixels.pixelsFor(label, time, { consumerId: "clipB" });
      }
      assert.equal(
        opened.filter(({ format }) => format === "svg").length,
        1,
        "one SVG source for both clips",
      );
      const svgHandle = opened.find(({ format }) => format === "svg").handle;
      const gifHandles = [...renders.keys()].filter(
        (handle) => handle !== svgHandle,
      );
      assert.equal(gifHandles.length, 2, "each GIF clip draws from its own");
      for (const handle of gifHandles) {
        const ticks = renders.get(handle);
        assert.deepEqual(
          ticks,
          [...ticks].sort((a, b) => a - b),
          `source ${handle} is read forward: ${ticks}`,
        );
      }
      assert.equal(pixels.diagnostics.openProviderSources, 3);
      await pixels.release(new Set(["label"]));
      await new Promise((resolve) => setTimeout(resolve, 0));
      for (const handle of gifHandles)
        assert.ok(released.includes(handle), `source ${handle} is released`);
      assert.ok(!released.includes(svgHandle));
      assert.equal(pixels.diagnostics.openProviderSources, 1);
      await pixels.dispose();
    } finally {
      Object.assign(glue.ExtensionRegistry.prototype, {
        open,
        render,
        release,
      });
    }
  },
);

test(
  "the readers of one animated image share what opening it decoded: the file opens once and its kept frames count once",
  { skip },
  async () => {
    // Readers of one file share its decode and kept frames.
    const glue = await import("alpha2/extensions-wasm");
    glue.initSync({
      module: readFileSync(join(wasmDirectory, "alpha2_extensions_bg.wasm")),
    });
    const corpus = join(
      resolve(import.meta.dirname, "..", "..", ".."),
      "tests/corpus/extensions",
    );
    let opens = 0;
    const renders = new Map();
    const released = [];
    const { open, render, release } = glue.ExtensionRegistry.prototype;
    glue.ExtensionRegistry.prototype.open = function (...args) {
      opens += 1;
      return open.apply(this, args);
    };
    glue.ExtensionRegistry.prototype.render = function (
      handle,
      ticks,
      ...rest
    ) {
      if (!renders.has(handle)) renders.set(handle, []);
      renders.get(handle).push(Number(ticks));
      return render.call(this, handle, ticks, ...rest);
    };
    glue.ExtensionRegistry.prototype.release = function (handle) {
      released.push(handle);
      return release.call(this, handle);
    };
    try {
      const bytes = new Uint8Array(readFileSync(join(corpus, "animation.gif")));
      const gif = {
        id: "gif",
        kind: "video",
        provider: { format: "animated_image" },
        file: file(bytes),
      };
      const live = new Set(["gif"]);
      const one = new SourcePixels(null);
      one.setOutputSize(48, 32);
      await one.release(live);
      await one.pixelsFor(gif, 0, { consumerId: "clipA" });
      const alone = one.diagnostics.providerSourceBytes;
      await one.dispose();
      opens = 0;
      renders.clear();
      released.length = 0;

      const pixels = new SourcePixels(null);
      pixels.setOutputSize(48, 32);
      const clips = ["clipA", "clipB", "clipC"];
      for (let step = 0; step < 4; step += 1) {
        await pixels.release(live);
        for (const [index, clip] of clips.entries())
          await pixels.pixelsFor(gif, step * 0.05 + index * 0.2, {
            consumerId: clip,
          });
      }
      assert.equal(opens, 1, "the file opens once for every reader");
      assert.equal(renders.size, 3, "each clip draws from a reader of its own");
      for (const [handle, ticks] of renders)
        assert.deepEqual(
          ticks,
          [...ticks].sort((a, b) => a - b),
          `reader ${handle} is read forward: ${ticks}`,
        );
      assert.equal(pixels.diagnostics.openProviderSources, 3);
      const held = pixels.diagnostics.providerSourceBytes;
      assert.ok(
        held < 2 * alone,
        `three readers hold ${held} bytes, one source ${alone}`,
      );
      await pixels.release(new Set());
      await new Promise((resolve) => setTimeout(resolve, 0));
      for (const handle of renders.keys())
        assert.ok(released.includes(handle), `reader ${handle} is released`);
      assert.equal(released.length, 4, "three readers and what made them");
      assert.equal(pixels.diagnostics.openProviderSources, 0);
    } finally {
      Object.assign(glue.ExtensionRegistry.prototype, {
        open,
        render,
        release,
      });
    }
  },
);

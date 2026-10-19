import { PlaybackClock, WebEffectRegistry } from "alpha2/wasm";
import { loadEngineWasm } from "./wasm.js";
import { FrameSupply } from "alpha2/frame-supply";
import { secondsToTicks } from "alpha2/project";
import { registerCaptionFonts } from "./caption-fonts.js";
import { drawnEffectResources } from "../../core/compositions.js";
import { EditorError } from "../../core/document.js";

/** Frames' worth of rendered results the preview keeps; see `setResultCacheBudget`. */
const RESULT_CACHE_FRAMES = 6;

/** The diagnostics panel refreshes at this rate, not at the render rate. */
const DIAGNOSTICS_INTERVAL_MS = 250;

/**
 * The wasm renderer of one preview or export: owns the engine instance, its frame supply, and a
 * render queue that collapses to the newest request.
 */
export class Engine {
  #registry = null;
  #project = null;
  #supply = null;
  #revision = 0;
  #generation = 0;
  #pending = null;
  #loop = null;
  #lastFrameAt = 0;
  #lastDiagnosticsAt = 0;
  #fps = [];
  #effectResources = new Map();
  /** What `supply` uploaded for the export frame the engine has not rendered yet. */
  #exportUploads = [];

  constructor(canvas, sources, library, runtimeAssets = {}) {
    this.runtimeAssets = runtimeAssets;
    this.canvas = canvas;
    this.sources = sources;
    this.library = library;
    this.kind = "wasm";
    this.traceUploads = false;
    this.lastDiagnostics = null;
    this.lastRenderMs = 0;
    this.onDiagnostics = null;
  }

  async start() {
    if (!("VideoFrame" in globalThis) || !("OffscreenCanvas" in globalThis)) {
      throw new Error(
        "WebCodecs and OffscreenCanvas are required by this demo",
      );
    }
    await loadEngineWasm(this.runtimeAssets.wasmUrl);
    this.#registry =
      this.canvas instanceof OffscreenCanvas
        ? await WebEffectRegistry.createForCanvas(this.canvas)
        : await WebEffectRegistry.createForHtmlCanvas(this.canvas);
    this.#effectResources = new Map();
    this.#sizeResultCache();
    this.#supply = new FrameSupply(this.#registry);
    // `engine.traceUploads = true` in the console logs every source upload, to tell a host decode
    // bug from an engine one when a clip shows another clip's picture.
    this.#supply.onUpload = (entry) => {
      if (this.traceUploads) console.debug("[alpha2 upload]", entry);
    };
    const effectCatalog = JSON.parse(this.#registry.builtinCatalog());
    this.catalog = effectCatalog;
    return {
      registeredEffects:
        effectCatalog.effects.length + effectCatalog.transitions.length,
      failures: [],
      effectCatalog,
    };
  }

  /**
   * Rendered results kept for revisits (a still, a paused or scrubbed-back frame): about six frames
   * at the canvas size in half float, sized again whenever the canvas is.
   */
  #sizeResultCache() {
    this.#registry.setResultCacheBudget(
      RESULT_CACHE_FRAMES * this.canvas.width * this.canvas.height * 8,
    );
  }

  /** The renderer's part of the capability report: GPU, color, and playback. */
  capabilityReport() {
    return this.#registry
      ? JSON.parse(this.#registry.capabilityReport())
      : null;
  }

  /** A clock for one run of playback; see the engine's `PlaybackPolicy`. */
  playbackClock(policy, frameRate, rate, tick, now) {
    return new PlaybackClock(
      policy,
      frameRate.num,
      frameRate.den,
      rate,
      tick,
      now,
    );
  }

  /** Whether sound plays under `policy` at `rate`. */
  audible(policy, rate) {
    return PlaybackClock.audible(policy, rate);
  }

  get ready() {
    return this.#registry !== null;
  }

  get deviceLost() {
    return this.#registry?.deviceLost ?? false;
  }

  /** Raw source video the frame supply copied through script memory, cumulative. */
  get sourceTransfer() {
    return (
      this.#supply?.sourceTransfer ?? {
        planarCopyFrames: 0,
        planarCopyBytes: 0,
      }
    );
  }

  /** The wasm object, for callers that drive audio directly. */
  get wasm() {
    return this.#registry;
  }

  /** Engine instance used by the realtime browser audio transport. */
  get audioEngine() {
    return this.#registry;
  }

  /** The project document most recently handed to the engine. */
  get project() {
    return this.#project;
  }

  /**
   * Loads `project` into the engine and syncs its LUT resources with the library: those the project
   * draws (`drawnEffectResources`), the only ones a render asks for; a resource nothing drawn names
   * is neither required nor registered. Bumps the revision so in-flight renders for the old project
   * are dropped.
   *
   * Throws `missing_asset` naming a drawn resource the library does not hold, and
   * `invalid_effect_resource` naming one the engine cannot parse; resources registered before the
   * failure stay known, so the next load neither leaks nor repeats them.
   */
  loadProject(project, operation = "preview") {
    if (!this.#registry) throw new Error("the renderer is not initialized");
    const drawn = project.effect_resources?.length
      ? drawnEffectResources(project)
      : new Set();
    // Parsing a LUT is not free and edits rarely change one, so an unchanged resource stays.
    for (const resource of project.effect_resources ?? []) {
      if (!drawn.has(resource.id)) continue;
      const asset = this.library.get(resource.id);
      if (!asset || asset.kind !== "lut3d")
        throw new EditorError(
          "missing_asset",
          `Relink ${resource.id} before ${operation}`,
          operation,
          resource.id,
        );
      if (this.#effectResources.get(resource.id) === asset.text) continue;
      try {
        this.#registry.registerLut3d(resource.id, asset.text);
      } catch (error) {
        throw new EditorError(
          "invalid_effect_resource",
          `${asset.name ?? resource.id} is not a 3D LUT the engine reads: ${error instanceof Error ? error.message : String(error)}`,
          operation,
          resource.id,
          true,
          { cause: error },
        );
      }
      this.#effectResources.set(resource.id, asset.text);
    }
    for (const id of [...this.#effectResources.keys()]) {
      if (drawn.has(id)) continue;
      this.#registry.unregisterEffectResource(id);
      this.#effectResources.delete(id);
    }
    this.#registry.loadProject(JSON.stringify(project));
    this.#project = project;
    this.#revision += 1;
    this.#registry.resetPlayer();
  }

  /**
   * Registers the fonts caption tracks draw with, in fallback order. An SVG's text draws with the
   * same fonts, as the native host draws it with its caption fonts. Returns the fonts the engine
   * could not parse, as `{ font, error }`: they are left out, and the others draw.
   */
  setCaptionFonts(fonts) {
    if (!this.#registry) throw new Error("the renderer is not initialized");
    const refused = registerCaptionFonts(this.#registry, fonts);
    const left = new Set(refused.map(({ font }) => font));
    this.sources.setFonts?.(fonts.filter((font) => !left.has(font)));
    this.#revision += 1;
    return refused;
  }

  resize(width, height) {
    if (!this.#registry) return;
    // Assigning `canvas.width` resets the drawing buffer and surface, so skip an unchanged size.
    if (this.canvas.width === width && this.canvas.height === height) return;
    this.canvas.width = width;
    this.canvas.height = height;
    this.#registry.resizeSurface?.(width, height);
    this.#sizeResultCache();
    this.#revision += 1;
  }

  /**
   * Uploads the sources the engine needs at a timestamp without rendering.
   *
   * Export uses this: the engine owns its frame schedule, so the host only satisfies `frameNeeds`.
   * The uploads stay until `beginExportFrame` renders their frame; retired earlier, a frame whose
   * uploads outgrow the retained budget would evict its own first ones and wait forever.
   */
  async supply(seconds, { context }) {
    if (!this.#registry) throw new Error("the renderer is not initialized");
    await this.sources.release(context.liveSourceIds);
    const supplied = await this.#supply.supply(
      BigInt(secondsToTicks(seconds)),
      {
        width: context.width,
        height: context.height,
        contentOf: this.#contentOf,
        liveSourceIds: context.liveSourceIds,
        getLayer: context.getLayer,
        pixelsFor: this.sources.pixelsFor,
        retainVideoConsumers: this.sources.retainConsumers,
        isCurrent: () => true,
      },
    );
    if (supplied) this.#exportUploads.push(...supplied.uploaded);
    return Boolean(supplied);
  }

  /**
   * Renders the next export frame (see `beginExport` on the engine). Once a ticket is not
   * `waiting`, the engine has consumed what `supply` uploaded for it, so those uploads are retired
   * here.
   */
  beginExportFrame() {
    if (!this.#registry) throw new Error("the renderer is not initialized");
    const ticket = this.#registry.beginExportFrame();
    if (ticket.kind !== "waiting") {
      this.#supply.retire(this.#exportUploads);
      this.#exportUploads = [];
    }
    return ticket;
  }

  /**
   * A source's pixels are the runtime asset the library opens for it: a relink, a provider setting,
   * or a proxy is another object, and only that source is uploaded again. An SVG's are also the
   * fonts its text draws with (see `SourcePixels.contentOf`).
   */
  #contentOf = (id) => {
    const asset = this.library.get(id) ?? null;
    return this.sources.contentOf ? this.sources.contentOf(asset) : asset;
  };

  /** Invalidates outstanding playback renders, for example on seek or pause. */
  invalidate() {
    this.#generation += 1;
    return this.#generation;
  }

  get generation() {
    return this.#generation;
  }

  /**
   * Requests one frame. Concurrent requests collapse to the most recent one so that scrubbing never
   * queues a backlog of stale frames.
   */
  render(seconds, { generation = null, context } = {}) {
    if (!this.#registry)
      return Promise.reject(new Error("the renderer is not initialized"));
    return new Promise((resolve, reject) => {
      if (this.#pending) {
        this.#pending.seconds = seconds;
        this.#pending.generation = generation;
        this.#pending.context = context;
        this.#pending.waiters.push({ resolve, reject });
      } else {
        this.#pending = {
          seconds,
          generation,
          context,
          waiters: [{ resolve, reject }],
        };
      }
      if (!this.#loop) this.#loop = this.#drain();
    });
  }

  async #drain() {
    try {
      while (this.#pending) {
        const request = this.#pending;
        this.#pending = null;
        try {
          const rendered = await this.#renderOnce(request);
          request.waiters.forEach(({ resolve }) => resolve(rendered));
        } catch (error) {
          request.waiters.forEach(({ reject }) => reject(error));
        }
      }
    } finally {
      this.#loop = null;
      if (this.#pending) this.#loop = this.#drain();
    }
  }

  async #renderOnce({ seconds, generation, context }) {
    const revision = this.#revision;
    const isCurrent = () =>
      revision === this.#revision &&
      (generation === null || generation === this.#generation);
    if (!isCurrent()) return false;
    await this.sources.release(context.liveSourceIds);
    const started = performance.now();
    const playback = await this.#supply.render(
      BigInt(secondsToTicks(seconds)),
      {
        width: context.width,
        height: context.height,
        contentOf: this.#contentOf,
        liveSourceIds: context.liveSourceIds,
        getLayer: context.getLayer,
        pixelsFor: this.sources.pixelsFor,
        retainVideoConsumers: this.sources.retainConsumers,
        isCurrent,
      },
    );
    if (!playback) return false;
    this.lastRenderMs = performance.now() - started;
    this.#sampleDiagnostics();
    return true;
  }

  #sampleDiagnostics() {
    const now = performance.now();
    if (this.#lastFrameAt)
      this.#fps.push(1000 / Math.max(1e-3, now - this.#lastFrameAt));
    this.#lastFrameAt = now;
    if (this.#fps.length > 45) this.#fps.shift();
    if (!this.#registry?.diagnostics) return;
    // Serializing a snapshot every frame would only slow the critical path.
    if (now - this.#lastDiagnosticsAt < DIAGNOSTICS_INTERVAL_MS) return;
    this.#lastDiagnosticsAt = now;
    const diagnostics = JSON.parse(this.#registry.diagnostics());
    diagnostics.preview = { surface: "html-canvas" };
    diagnostics.host = {
      fps: Number(this.fps.toFixed(1)),
      renderMs: Number(this.lastRenderMs.toFixed(2)),
      uploadedFrames: this.#registry.uploadedFrameCount,
    };
    this.lastDiagnostics = diagnostics;
    this.onDiagnostics?.(diagnostics);
  }

  get fps() {
    if (this.#fps.length === 0) return 0;
    return (
      this.#fps.reduce((total, value) => total + value, 0) / this.#fps.length
    );
  }

  resetFpsWindow() {
    this.#fps = [];
    this.#lastFrameAt = 0;
  }

  async dispose() {
    this.invalidate();
    this.#revision += 1;
    await this.#loop;
    this.free();
  }

  free() {
    this.#registry?.free();
    this.#registry = null;
  }
}

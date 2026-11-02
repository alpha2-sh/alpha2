import { PlaybackClock, WebEffectRegistry } from "alpha2/wasm";
import { loadEngineWasm } from "./wasm.js";
import { FrameSupply } from "alpha2/frame-supply";
import { lossReportFromJson } from "alpha2/gpu-recovery";
import { createRenderer } from "alpha2/renderer-choice";
import { secondsToTicks } from "alpha2/project";
import { registerCaptionFonts } from "./caption-fonts.js";
import { drawnEffectResources } from "../../core/compositions.js";
import { EditorError } from "../../core/document.js";

/** Frames' worth of rendered results the preview keeps; see `setResultCacheBudget`. */
const RESULT_CACHE_FRAMES = 6;

/** The diagnostics panel refreshes at this rate, not at the render rate. */
const DIAGNOSTICS_INTERVAL_MS = 250;

const effectKey = (definition) => `${definition.id}@${definition.version}`;

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
  #effects = new Set();
  /** Cumulative: frames the player drew, and frames equal to the one shown. */
  #renders = 0;
  #unchanged = 0;
  /** What `supply` uploaded for the export frame not rendered yet. */
  #exportUploads = [];

  constructor(
    canvas,
    sources,
    library,
    runtimeAssets = {},
    {
      unreadableFrames,
      videoConversion,
      opaqueEngineFrames,
      renderer = "auto",
    } = {},
  ) {
    /** `"auto"` (hardware GPU, else CPU), `"gpu"` or `"cpu"`, as `createRenderer` takes them. */
    this.renderer = renderer;
    this.rendererChoice = null;
    this.runtimeAssets = runtimeAssets;
    this.unreadableFrames = unreadableFrames;
    this.videoConversion = videoConversion;
    this.opaqueEngineFrames = opaqueEngineFrames;
    this.canvas = canvas;
    this.sources = sources;
    this.library = library;
    this.kind = "wasm";
    this.traceUploads = false;
    /** Which preview draws with this renderer, as its diagnostics report it. */
    this.backend = "main-thread";
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
    const { wasm, choice } = await createRenderer(
      WebEffectRegistry,
      this.canvas,
      this.renderer,
    );
    this.#registry = wasm;
    this.rendererChoice = choice;
    this.#effectResources = new Map();
    this.#sizeResultCache();
    this.#supply = new FrameSupply(this.#registry, {
      ...(this.unreadableFrames === undefined
        ? {}
        : { unreadableFrames: this.unreadableFrames }),
      ...(this.videoConversion === undefined
        ? {}
        : { videoConversion: this.videoConversion }),
      ...(this.opaqueEngineFrames === undefined
        ? {}
        : { opaqueEngineFrames: this.opaqueEngineFrames }),
    });
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
   * Sizes the result cache for revisits (stills, paused or scrubbed-back frames) to about six
   * half-float frames at the canvas size.
   */
  #sizeResultCache() {
    this.#registry.setResultCacheBudget(
      RESULT_CACHE_FRAMES * this.canvas.width * this.canvas.height * 8,
    );
  }

  /**
   * Turns per-pass GPU and CPU timing on or off (off by default); passes appear in
   * `lastDiagnostics`.
   */
  setPassTiming(enabled) {
    this.#registry?.setPassTiming(enabled);
  }

  /**
   * Sets the preview resolution scale (1 renders at the document's size) and returns the loaded
   * project's render size as `[width, height]`, or null with no project. Throws for a scale the
   * engine refuses.
   */
  setPreviewScale(scale) {
    if (!this.#registry) throw new Error("the renderer is not initialized");
    this.#registry.setPreviewScale(scale);
    this.#revision += 1;
    return this.previewSize;
  }

  get previewSize() {
    if (!this.#registry || !this.#project) return null;
    return Array.from(this.#registry.previewSize);
  }

  /**
   * The renderer's part of the capability report: GPU, color, playback, and `renderer` (which
   * renderer draws and whether it runs runtime effects).
   */
  capabilityReport() {
    if (!this.#registry) return null;
    const choice = this.rendererChoice;
    return {
      ...JSON.parse(this.#registry.capabilityReport()),
      renderer: {
        renderer: choice?.renderer ?? "gpu",
        rendererReason: choice?.rendererReason ?? null,
        backend: choice?.backend ?? null,
        adapter: choice?.adapter ?? null,
        runtimeEffects: (choice?.renderer ?? "gpu") === "gpu",
      },
    };
  }

  /** The `GpuLossReport` after a GPU loss; undefined before a loss and on the CPU renderer. */
  lossReport() {
    const registry = this.#registry;
    if (!registry?.deviceLost) return undefined;
    return lossReportFromJson(registry.lossReport);
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

  audible(policy, rate) {
    return PlaybackClock.audible(policy, rate);
  }

  get ready() {
    return this.#registry !== null;
  }

  get deviceLost() {
    return this.#registry?.deviceLost ?? false;
  }

  /**
   * Cumulative: raw source video copied through script memory, and frames that could not be read
   * back and how they were recovered.
   */
  get sourceTransfer() {
    return {
      ...(this.#supply?.sourceTransfer ?? {
        planarCopyFrames: 0,
        planarCopyBytes: 0,
      }),
      ...this.#readbackRecovery(),
    };
  }

  #readbackRecovery() {
    return (
      this.#supply?.readbackRecovery ?? {
        unreadableFrames: 0,
        readbackRetries: 0,
        redecodedFrames: 0,
        presentedFrames: 0,
      }
    );
  }

  get wasm() {
    return this.#registry;
  }

  get audioEngine() {
    return this.#registry;
  }

  get project() {
    return this.#project;
  }

  /**
   * Loads `project` and syncs its LUT resources with the library, registering only those the
   * project draws (`drawnEffectResources`). Bumps the revision so in-flight renders for the old
   * project are dropped.
   *
   * Throws `missing_asset` for a drawn resource the library lacks and `invalid_effect_resource` for
   * one the engine cannot parse. Resources registered before the failure stay known, so the next
   * load neither leaks nor repeats them.
   */
  loadProject(project, operation = "preview") {
    if (!this.#registry) throw new Error("the renderer is not initialized");
    const drawn = project.effect_resources?.length
      ? drawnEffectResources(project)
      : new Set();
    // Parsing a LUT is costly and edits rarely change one, so unchanged resources stay.
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
   * The registered runtime effects the loaded project can draw, as `id@version`. The CPU renderer
   * registers them but refuses to render or export a project that draws one.
   */
  projectRuntimeEffects() {
    if (!this.#registry) throw new Error("the renderer is not initialized");
    return this.#registry.projectRuntimeEffects();
  }

  /**
   * Compiles runtime effect definitions on this renderer's device and registers them, all or none.
   * An `id@version` already held is skipped, since a definition never changes under its version.
   * Throws `invalid_effect` naming the first refused definition, with its diagnostics.
   */
  async registerEffects(definitions, operation = "preview") {
    if (!this.#registry) throw new Error("the renderer is not initialized");
    const fresh = definitions.filter(
      (definition) => !this.#effects.has(effectKey(definition)),
    );
    const refusal = (definition, error) =>
      new EditorError(
        "invalid_effect",
        `Effect ${effectKey(definition)} is not one the engine runs: ${error instanceof Error ? error.message : String(error)}`,
        operation,
        effectKey(definition),
        true,
        { cause: error },
      );
    // `compileEffect` throws for definitions it refuses before compiling; otherwise its promise
    // rejects with the shader's diagnostics.
    const compiled = await Promise.allSettled(
      fresh.map(async (definition) =>
        this.#registry.compileEffect(JSON.stringify(definition)),
      ),
    );
    const failed = compiled.findIndex(({ status }) => status === "rejected");
    if (failed !== -1) {
      for (const result of compiled)
        if (result.status === "fulfilled") result.value.free?.();
      throw refusal(fresh[failed], compiled[failed].reason);
    }
    const registered = [];
    for (const [index, definition] of fresh.entries()) {
      try {
        this.#registry.registerCompiledEffect(compiled[index].value);
      } catch (error) {
        for (const later of compiled.slice(index + 1)) later.value.free?.();
        for (const done of registered)
          this.#registry.unregisterEffect(done.id, done.version);
        throw refusal(definition, error);
      }
      registered.push(definition);
    }
    for (const definition of registered)
      this.#effects.add(effectKey(definition));
  }

  /**
   * Registers caption fonts in fallback order; SVG text uses them too, as on the native host.
   * Returns the fonts the engine could not parse as `{ font, error }`; those are left out.
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
   * Uploads the sources the engine needs at a timestamp without rendering, for export, where the
   * engine owns the frame schedule. Uploads stay until `beginExportFrame` renders their frame;
   * retiring them earlier could make a frame evict its own first uploads and wait forever.
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
   * Renders the next export frame. Once a ticket is not `waiting`, the engine has consumed the
   * frame's uploads, so they are retired here.
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
   * A source's content identity is the runtime asset the library opens for it, so a relink,
   * provider setting or proxy re-uploads only that source. An SVG's also includes its fonts (see
   * `SourcePixels.contentOf`).
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
   * Requests one frame. Concurrent requests collapse to the newest so scrubbing never queues stale
   * frames. Resolves `{ rendered, unchanged }`: `rendered` is false when superseded, and
   * `unchanged` means the frame equals the one shown, so nothing was drawn or presented.
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
    if (!isCurrent()) return { rendered: false, unchanged: false };
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
    if (!playback) return { rendered: false, unchanged: false };
    const unchanged = playback.unchanged === true;
    if (unchanged) this.#unchanged += 1;
    else this.#renders += 1;
    this.lastRenderMs = performance.now() - started;
    this.#sampleDiagnostics();
    return {
      rendered: true,
      unchanged,
      degradedSources: playback.degradedSources ?? [],
    };
  }

  #sampleDiagnostics() {
    const now = performance.now();
    if (this.#lastFrameAt)
      this.#fps.push(1000 / Math.max(1e-3, now - this.#lastFrameAt));
    this.#lastFrameAt = now;
    if (this.#fps.length > 45) this.#fps.shift();
    if (!this.#registry?.diagnostics) return;
    // Serializing a snapshot every frame would slow the critical path.
    if (now - this.#lastDiagnosticsAt < DIAGNOSTICS_INTERVAL_MS) return;
    this.#lastDiagnosticsAt = now;
    const diagnostics = JSON.parse(this.#registry.diagnostics());
    diagnostics.preview = { surface: "html-canvas" };
    diagnostics.host = {
      fps: Number(this.fps.toFixed(1)),
      renderMs: Number(this.lastRenderMs.toFixed(2)),
      uploadedFrames: this.#registry.uploadedFrameCount,
      renders: this.#renders,
      unchanged: this.#unchanged,
      renderSize: [this.canvas.width, this.canvas.height],
      backend: this.backend,
      readbackRecovery: this.#readbackRecovery(),
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

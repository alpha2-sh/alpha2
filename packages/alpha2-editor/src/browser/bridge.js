import { frameAt, frameStart } from "../core/time.js";
import {
  compileEditorDocument,
  EditorError,
  sequenceEnd,
} from "../core/document.js";
import { captionsAsset } from "../core/captions.js";
import { MAX_CAPTION_FILE_BYTES } from "alpha2/project";
import { containerPolicy } from "alpha2/containers";
import { drawnEffectResources, shownSources } from "../core/compositions.js";
import { ResourceRegistry } from "./resources.js";
import { ExportJob, requireExportFrameRate } from "./jobs.js";
import { attemptOptions } from "./runtime/budget.js";
import { createWorkerPreview } from "./worker.js";
import { registerCaptionFonts } from "./runtime/caption-fonts.js";
import { AssetLibrary, streamPeaks } from "./runtime/assets.js";
import {
  SourcePixels,
  providerSettings,
  sameProviderSettings,
} from "./runtime/sources.js";
import { fingerprintFile } from "./runtime/fingerprint.js";
import { hashInWorker } from "./hash-worker.js";
import { callExtensions } from "./extensions-worker.js";
import {
  keepProviderChoices,
  mayBeExtension,
  providerDocumentLimit,
  stageProviderFile,
  stageSequence,
} from "./runtime/extension-import.js";
import { ProxyStore } from "./runtime/proxies.js";
import {
  blockingIssues,
  compareFingerprints,
  conformProject,
  sha256Blob,
} from "alpha2/assets";

/**
 * An export refused before it rendered: the engine's conform report says why. The message names
 * each source as `nameOf` does (the document's asset names), the report and `entityId` by ID.
 */
export class ConformError extends EditorError {
  /**
   * @param {import("alpha2/assets").ConformReport} report
   * @param {{ operation?: string, nameOf?: (sourceId: string) => string | undefined }} [options]
   */
  constructor(report, { operation = "export", nameOf } = {}) {
    const issues = blockingIssues(report);
    super(
      "conform_failed",
      `The media is not ready: ${issues
        .map(
          ({ sourceId, issue }) =>
            `${nameOf?.(sourceId) ?? sourceId} ${issue.kind.replace(/_/g, " ")}`,
        )
        .join("; ")}`,
      operation,
      issues[0]?.sourceId,
    );
    this.name = "ConformError";
    this.report = report;
  }
}
import { EngineAudioMixer, AudioWindowSupplier } from "./runtime/audio.js";

/** The engine's playback policies (`PlaybackPolicy`), in its order. */
const PLAYBACK_POLICIES = Object.freeze([
  "drop_video_keep_audio",
  "preserve_every_video_frame",
  "silent_scrub",
  "offline_exact",
]);

/**
 * Files of one import staged at once. Each hashes its whole file in its own worker, at about
 * 200 MB/s: four read about 800 MB/s, below a local SSD, and one core stays with the page.
 */
const IMPORT_STAGE_CONCURRENCY = 4;

/**
 * How long Play waits for sound to start before playing the picture without it. A first start loads
 * and compiles the audio worklet and decodes the first block, about 1.5 s on Firefox.
 */
const AUDIO_START_TIMEOUT_MS = 3000;

/**
 * The default media adapter: stages each file in its own `AssetLibrary`, apart from the
 * bridge's live one, so a failed or cancelled import leaves nothing behind. Files an extension
 * provider reads (SVG, animated images, Lottie, OpenEXR) are probed, opened and drawn for their
 * thumbnail by `alpha2/extensions`, loaded from `extensionsWasmUrl`: in a worker
 * `extensionsWorkerFactory` makes for each file, which a cancelled import or a call past
 * `EXTENSIONS_CALL_TIMEOUT_MS` terminates, or, without one, on this thread. A font is checked with
 * the engine's parser (`alpha2/wasm`, from `wasmUrl`) and also registered with the page's
 * `FontFace` set. A file's content hash is made in a worker `hashWorkerFactory` makes, where there
 * is one.
 * @param {{ extensionsWasmUrl?: string | URL, wasmUrl?: string | URL, hashWorkerFactory?: () => Worker, extensionsWorkerFactory?: () => Worker }} [options]
 * @returns {import("./types.js").BrowserMediaAdapter}
 */
export function createMediaAdapter({
  extensionsWasmUrl,
  wasmUrl,
  hashWorkerFactory,
  extensionsWorkerFactory,
} = {}) {
  const hash = hashWorkerFactory
    ? (file, options) => hashInWorker(hashWorkerFactory, file, options)
    : sha256Blob;
  const module = {
    factory: extensionsWorkerFactory,
    wasmUrl: extensionsWasmUrl,
  };
  /**
   * What the provider module makes of `file` with `call`. A file it refuses is `unreadable_media`
   * with its reason; a cancellation or a stall is passed on as it is.
   */
  const read = async (call, file, signal) => {
    try {
      return await callExtensions(module, { call, file }, { signal });
    } catch (error) {
      if (signal.aborted || error instanceof EditorError) throw error;
      throw new EditorError(
        "unreadable_media",
        `Cannot read ${file.name}: ${error instanceof Error ? error.message : String(error)}`,
        "import",
        undefined,
        true,
        { cause: error },
      );
    }
  };
  return {
    /** Stages numbered PNGs or OpenEXR images as one image sequence. */
    async stageSequence(files, options, signal) {
      signal.throwIfAborted();
      const item = await stageSequence(files, {
        ...options,
        openExr: (file) => read("exr-first-frame", file, signal),
      });
      signal.throwIfAborted();
      return item;
    },
    async stage(file, signal, { fingerprint: wanted = true } = {}) {
      signal.throwIfAborted();
      // The hash reads the whole file, so it starts now and runs while the file is probed.
      const hashing = new AbortController();
      const stop = () => hashing.abort(signal.reason);
      signal.addEventListener("abort", stop, { once: true });
      const started = wanted ? hash(file, { signal: hashing.signal }) : null;
      started?.catch(() => {}); // awaited below when used; a failed stage aborts it
      /** The file's fingerprint, unless the caller has no use for it (a proxy being adopted). */
      const fingerprintOf = (kind, options) =>
        wanted
          ? fingerprintFile(file, kind, {
              signal,
              hash: () => started,
              ...options,
            })
          : undefined;
      try {
        return await stageFile(file, signal, fingerprintOf);
      } catch (error) {
        hashing.abort(error);
        throw error;
      } finally {
        signal.removeEventListener("abort", stop);
      }
    },
  };
  /** Stages `file` as the media its bytes or name say it is; `fingerprintOf` gives its evidence. */
  async function stageFile(file, signal, fingerprintOf) {
    // Recognized by its bytes; the provider module loads only for bytes that could be one.
    const head = new Uint8Array(await file.slice(0, 4096).arrayBuffer());
    if (mayBeExtension(head)) {
      // Parsed whole in provider memory that never shrinks, so an oversize file is refused unread.
      const document = providerDocumentLimit(head);
      if (document !== null && file.size > document.limit)
        throw new EditorError(
          "too_large",
          `${file.name} is ${file.size} bytes; a ${document.what} is read up to ${document.limit}`,
          "import",
        );
      const found = await read("read", file, signal);
      signal.throwIfAborted();
      if (found)
        return stageProviderFile(file, found, await fingerprintOf("image"));
    }
    if (/\.(ttf|otf|ttc|otc)$/i.test(file.name)) {
      const bytes = new Uint8Array(await file.arrayBuffer());
      signal.throwIfAborted();
      // The engine's parser decides: FontFace also loads WOFF and WOFF2, which the engine refuses.
      const { loadEngineWasm } = await import("./runtime/wasm.js");
      const { checkFont } = await loadEngineWasm(wasmUrl);
      signal.throwIfAborted();
      try {
        checkFont(bytes, 0);
      } catch (error) {
        throw new EditorError(
          "unreadable_media",
          `Cannot read ${file.name} as a font: ${error instanceof Error ? error.message : String(error)}`,
          "import",
          undefined,
          true,
          { cause: error },
        );
      }
      const id = crypto.randomUUID();
      let face;
      if (typeof FontFace !== "undefined") {
        face = new FontFace(`alpha2-font-${id}`, bytes);
        await face.load();
        signal.throwIfAborted();
        globalThis.document?.fonts.add(face);
      }
      return {
        runtime: {
          id,
          kind: "font",
          name: file.name,
          bytes,
          faceIndex: 0,
          family: `alpha2-font-${id}`,
        },
        description: { id, kind: "font", name: file.name },
        fingerprint: await fingerprintOf("font"),
        dispose() {
          if (face) globalThis.document?.fonts.delete(face);
        },
      };
    }
    const library = new AssetLibrary();
    try {
      const runtime = file.name.toLowerCase().endsWith(".cube")
        ? await library.importEffectResource(file, "lut3d")
        : await library.importFile(file, { signal });
      signal.throwIfAborted();
      if (runtime.kind === "lut3d") {
        // The engine's parser validates the LUT once here rather than at every load.
        const { loadEngineWasm } = await import("./runtime/wasm.js");
        const { checkLut3d } = await loadEngineWasm(wasmUrl);
        signal.throwIfAborted();
        try {
          checkLut3d(runtime.text);
        } catch (error) {
          throw new EditorError(
            "unreadable_media",
            `Cannot read ${file.name} as a 3D LUT: ${error instanceof Error ? error.message : String(error)}`,
            "import",
            undefined,
            true,
            { cause: error },
          );
        }
      }
      const description = library.projectManifest()[0];
      const fingerprint = await fingerprintOf(runtime.kind, {
        size: runtime.width
          ? { width: runtime.width, height: runtime.height }
          : undefined,
      });
      signal.throwIfAborted();
      return {
        runtime,
        description,
        fingerprint,
        dispose: () => library.dispose(),
      };
    } catch (error) {
      library.dispose();
      throw error;
    }
  }
}

/**
 * Creates the browser runtime for an editor session: preview, playback, import, proxies and
 * export. No GPU device, AudioContext, worker or decoder is created until `bindPreview`, an
 * import, `play` or `startExport` needs one. The bridge registers itself as an owner of
 * `editor`, so disposing the session disposes it.
 * @param {import("./types.js").BrowserBridgeOptions} options
 * @returns {import("./types.js").BrowserBridge}
 */
export function createBrowserBridge(options) {
  const {
    editor,
    runtimeAssets = {},
    mediaAdapter = createMediaAdapter(runtimeAssets),
    resourceLimit = 128,
    audio = true,
    onError = () => {},
    assetDecisions,
    previewScale: initialPreviewScale = 0.5,
    videoConversion: initialVideoConversion = "engine",
  } = options;
  let previewScale = checkedPreviewScale(initialPreviewScale);
  const videoConversion = checkedVideoConversion(
    initialVideoConversion,
    "bridge",
  );
  // A test's stand-in for origin storage, kept out of the public options.
  const proxyStore =
    /** @type {{ proxyStore?: ProxyStore }} */ (options).proxyStore ??
    new ProxyStore();
  let decisionBinding = assetDecisions ?? null;
  /**
   * The wasm module's authoring functions, loaded on first use: the same instance the preview
   * engine renders with.
   */
  const loadWasm = async () => {
    const { loadEngineWasm } = await import("./runtime/wasm.js");
    return loadEngineWasm(runtimeAssets.wasmUrl);
  };
  /** The engine's asset decisions, from the wasm module, loaded on first use. */
  const decisions = async () => (decisionBinding ??= await loadWasm());
  /**
   * What `measureText` and `measureLayout` lay text out with, made on first use, and the font
   * stack it holds.
   */
  let measurer = null,
    measuredFonts = null;
  const registry = new ResourceRegistry(resourceLimit);
  /**
   * The runtimes this session holds, by {@link ownerKey}: a lease and the registry revision it was
   * put under. Media is keyed by its content revision, which this bridge issues; text by what it
   * draws, since an edit after an undo may give other text a revision an undone edit already used.
   */
  const owners = new Map();
  /**
   * The highest registry revision issued for each runtime ID: none is issued twice, so a runtime
   * released while an export still leases it never collides with the one that replaces it.
   */
  const issued = new Map();
  const issue = (id, atLeast = 0) => {
    const revision = Math.max(atLeast, (issued.get(id) ?? -1) + 1);
    issued.set(id, revision);
    return revision;
  };
  /**
   * Proxies by their original's hash: the kept record, a lease on the decoded runtime and its
   * registry revision.
   */
  const proxies = new Map();
  /** Proxies being made, by source ID: their progress from 0 to 1. */
  const proxyJobs = new Map();
  /** What preview reads for each source, as the engine last selected it. */
  let previewSelection = new Map();
  /** What that selection was decided from (see `loaded`). */
  let previewConformKey = null;
  /** What preview prefers where a source has a fresh proxy (see `setPreviewMedia`). */
  let preferredPreviewMedia = "original";
  const hashOf = (asset) => asset?.fingerprint?.content_sha256;
  const originalOf = (document, id) => {
    const descriptor = document.assets[id];
    if (!descriptor || descriptor.missing) return null;
    return ownerOf(document, descriptor)?.lease.value ?? null;
  };
  /**
   * The host's view of each source's media, for the engine to decide with: the original is online
   * when this session holds its file, and a kept proxy is offered when it was made from the media
   * the project recorded.
   */
  const catalogOf = (document) => ({
    assets: document.project.sources
      .filter((source) => !drawnByEngine(source))
      .map((source) => {
        const descriptor = document.assets[source.id];
        const record = proxies.get(hashOf(descriptor))?.record;
        const revision = descriptor?.contentRevision ?? 0;
        return {
          source_id: source.id,
          revision,
          original: originalOf(document, source.id)
            ? { state: "online", fingerprint: descriptor?.fingerprint ?? {} }
            : { state: "offline" },
          proxies: record
            ? [
                {
                  id: `proxy:${hashOf(descriptor)}`,
                  // Kept by the original's hash, whichever revision relinked these bytes.
                  source_revision: revision,
                  ...record,
                  state: { state: "ready" },
                },
              ]
            : [],
        };
      }),
  });
  /** Proxies being adopted, by hash, so two callers share one adoption. */
  const adopting = new Map();
  /**
   * Adopts a kept proxy: its file staged like any import, under its own ID. `hold` (the store's
   * `hold`) keeps the proxy in use until the runtime is disposed: once no document and no export
   * reads it.
   */
  function adoptProxy(hash, found, hold) {
    if (proxies.has(hash)) {
      void hold.release();
      return Promise.resolve();
    }
    let pending = adopting.get(hash);
    if (pending) {
      void hold.release();
      return pending;
    }
    pending = (async () => {
      let staged;
      try {
        // A proxy is known by its original's hash; hashing its own bytes again would be wasted.
        staged = await mediaAdapter.stage(
          found.file,
          new AbortController().signal,
          { fingerprint: false },
        );
      } catch (error) {
        void hold.release();
        throw error;
      }
      const proxy = {
        record: found.record,
        lease: null,
        revision: 0,
        unlocked: null,
      };
      try {
        const runtime = { ...staged.runtime, id: `proxy:${hash}` };
        proxy.revision = issue(runtime.id);
        proxy.lease = registry.put(runtime.id, proxy.revision, runtime, () => {
          staged.dispose();
          proxy.unlocked = hold.release();
        });
        proxies.set(hash, proxy);
      } catch (error) {
        staged.dispose();
        void hold.release();
        throw error;
      }
    })().finally(() => adopting.delete(hash));
    adopting.set(hash, pending);
    return pending;
  }
  /**
   * Hashes the store had no proxy for, this session: not asked again until the session changes.
   * A proxy this session makes is adopted directly.
   */
  let absentProxies = new Set();
  /**
   * Finds the kept proxies of sources this session has no original for, and of every source while
   * preview prefers proxies.
   */
  async function discoverProxies(document) {
    for (const source of document.project.sources) {
      const descriptor = document.assets[source.id];
      const hash = hashOf(descriptor);
      if (
        !hash ||
        proxies.has(hash) ||
        absentProxies.has(hash) ||
        (preferredPreviewMedia === "original" &&
          originalOf(document, source.id))
      )
        continue;
      // Held before it is read, so no page removes it between.
      const hold = await proxyStore.hold(hash);
      const found = await proxyStore.find(hash).catch(() => null);
      if (found) await adoptProxy(hash, found, hold);
      else {
        absentProxies.add(hash);
        void hold.release();
      }
    }
  }
  /** Whether `document`'s media is ready for `purpose`, as the engine decides. */
  async function conform(purpose, document = editor.document) {
    await discoverProxies(document);
    const binding = await decisions();
    materializeText(document);
    const compiled = compileEditorDocument(document);
    if (compiled.diagnostics.length) throw compiled.diagnostics[0];
    requireShownText(document, compiled.project);
    return conformProject(
      binding,
      compiled.project,
      catalogOf(document),
      purpose,
    );
  }
  const pendingImports = new Set();
  const assetListeners = new Set();
  let analysis = Promise.resolve();
  const notifyAssets = () => {
    for (const listener of assetListeners) listener();
  };
  /** The waveform analysis running for each runtime, to abort when the runtime is let go. */
  const analyses = new Map();
  /** Whether this session still holds `runtime` for some document it can show. */
  const held = (runtime) =>
    [...owners.values()].some((owner) => owner.lease.value === runtime);
  /**
   * Waveform peaks are derived after import, one file at a time, so an import commits as soon as
   * the media is readable. The analysis belongs to the runtime, not to the document that imported
   * it: a document opened again over the same media keeps it, and only letting the runtime go (no
   * reachable document reads it) drops it.
   */
  const analyze = (runtime) => {
    if (
      !runtime.file ||
      runtime.peaks ||
      !(
        runtime.kind === "audio" ||
        (runtime.kind === "video" && runtime.hasAudio)
      )
    )
      return;
    analysis = analysis
      .then(async () => {
        if (disposed || runtime.peaks || !held(runtime)) return;
        const controller = new AbortController();
        analyses.set(runtime, controller);
        try {
          const peaks = await streamPeaks(runtime.file, {
            signal: controller.signal,
          });
          if (peaks && !disposed && held(runtime)) {
            runtime.peaks = peaks;
            notifyAssets();
          }
        } finally {
          analyses.delete(runtime);
        }
      })
      .catch(() => {});
  };
  const jobs = new Set();
  let disposed = false,
    engine = null,
    workerPreview = null,
    boundCanvas = null,
    copySurface = false,
    // The worker failed for a reason other than a lost GPU: later previews for this binding stay on
    // the main thread. A new binding tries a worker again.
    workerGivenUp = false,
    sources = null,
    mixer = null,
    supplier = null,
    binding = 0,
    queue = Promise.resolve(),
    renderKey = null,
    tick = 0,
    playing = false,
    // How the current or last playback was asked for, so a recovered preview plays on alike.
    playRequest = null,
    renderScheduled = false,
    raf = 0,
    playClock = null,
    loadRevision = 0,
    loadedAssets = new Map(),
    loadedSelection = "",
    loadedFonts = null,
    audioKey = null,
    // What the preview worker holds from its last load.
    workerSent = new Map(),
    workerFonts = null,
    refreshFrame = 0,
    monitorVolume = 1,
    // Tracing the drawing renderer applies, kept for every renderer a binding makes.
    passTiming = false,
    traceUploads = false,
    // The worker's last renderer diagnostics snapshot, and counters since `bindPreview`: playback
    // refreshes that asked for no frame, and frames copied to the bound canvas.
    workerDiagnostics = null,
    skippedRequests = 0,
    bitmapTransfers = 0,
    playGeneration = 0,
    sessionGeneration = editor.generation;
  /**
   * Preview's media for a source of `document`: what the engine selected, the original unless it is
   * offline, opened as the document's source says.
   */
  function previewMedia(document, id) {
    const descriptor = document.assets[id];
    const original = descriptor
      ? sourceView(document, id, ownerOf(document, descriptor)?.lease.value)
      : null;
    const selection = previewSelection.get(id);
    if (selection?.media === "proxy")
      return proxies.get(hashOf(descriptor))?.lease.value ?? original;
    return descriptor?.missing ? null : original;
  }
  const library = { get: (id) => previewMedia(editor.draft, id) };
  const key = (id, revision) => JSON.stringify([id, revision]);
  /**
   * The font families a text style draws with: each imported font's by the family its runtime
   * registered, other names as written.
   */
  function familiesOf(document, text) {
    return (text?.fontFamily ?? "sans-serif").split(",").map((name) => {
      const plain = name.trim().replace(/^["']|["']$/g, "");
      const font = Object.values(document.assets).find(
        (a) => a.kind === "font" && a.name.replace(/\.[^.]+$/, "") === plain,
      );
      return (font && ownerOf(document, font)?.lease.value.family) ?? plain;
    });
  }
  /**
   * Which runtime `asset` of `document` reads: media by its content revision, text by its authoring
   * and the fonts it resolves to.
   */
  function ownerKey(document, asset) {
    return asset.kind === "text"
      ? JSON.stringify([
          asset.id,
          "text",
          asset.authoring?.text ?? null,
          familiesOf(document, asset.authoring?.text),
        ])
      : key(asset.id, asset.contentRevision);
  }
  function ownerOf(document, asset) {
    return owners.get(ownerKey(document, asset)) ?? null;
  }
  /**
   * A lease on the runtime `asset` of `document` reads, for work that outlives the owner's hold on
   * it.
   */
  function acquireOwned(document, asset) {
    const owner = asset && ownerOf(document, asset);
    if (!owner && asset?.kind === "text") throw undrawnText(document, asset);
    if (!owner)
      throw new EditorError(
        "missing_asset",
        `Relink ${asset?.name ?? asset?.id}: this session has no file for it`,
        "resource",
        asset?.id,
      );
    return registry.acquire(asset.id, owner.revision);
  }
  function acquireProxy(hash, id) {
    const proxy = proxies.get(hash);
    if (!proxy)
      throw new EditorError(
        "missing_asset",
        `${editor.document.assets[id]?.name ?? id} has no proxy in this session`,
        "resource",
        id,
      );
    return registry.acquire(`proxy:${hash}`, proxy.revision);
  }
  /** Runtime views by runtime, then by provider settings, so one choice is one value. */
  const views = new WeakMap();
  /**
   * `runtime` opened as source `id` of `document` says: a provider's settings (a package's
   * animation, an OpenEXR still's primaries) are the document's, so undo and redo change what is
   * drawn. A view reads everything else from the runtime itself, and a new choice is a new value.
   */
  function sourceView(document, id, runtime) {
    if (!runtime?.provider) return runtime ?? null;
    const provider = document.project.sources.find(
      (source) => source.id === id,
    )?.provider;
    if (!provider || sameProviderSettings(provider, runtime.provider))
      return runtime;
    const settings = providerSettings(provider);
    let byProvider = views.get(runtime);
    if (!byProvider) views.set(runtime, (byProvider = new Map()));
    let view = byProvider.get(settings);
    if (!view) {
      view = Object.create(runtime, {
        provider: { value: provider, enumerable: true },
      });
      byProvider.set(settings, view);
    }
    return view;
  }
  /**
   * Runs `fn` after the preview work already queued. Its failure goes to the caller, which reports
   * it: to whoever awaits it, or to `onError` for work the bridge started itself, never to both.
   */
  const enqueue = (fn) => {
    const next = queue.then(() => {
      if (disposed)
        throw new EditorError("disposed", "Bridge is disposed", "browser");
      return fn();
    });
    queue = next.catch(() => {});
    return next;
  };
  const context = (project, lib) => ({
    width: project.settings.width,
    height: project.settings.height,
    liveSourceIds: new Set(project.sources.map((s) => s.id)),
    getLayer: (id) => {
      const asset = lib.get(id);
      return asset?.kind === "text" ? { id, kind: "image", asset } : asset;
    },
  });
  const features = Object.freeze({
    preview: { available: typeof OffscreenCanvas !== "undefined" },
    audioPreview: {
      available:
        typeof SharedArrayBuffer !== "undefined" &&
        globalThis.crossOriginIsolated === true,
    },
    transportRate: { available: true, reason: "Audio is silent outside 1x" },
    worker: {
      available:
        typeof Worker !== "undefined" &&
        typeof runtimeAssets.workerFactory === "function",
      reason:
        "Provide a bundled workerFactory; otherwise the main-thread adapter is used",
    },
    export: {
      available: typeof VideoEncoder !== "undefined",
      reason:
        "Codec availability is checked at export; output streams to a temporary origin-private file unless a file or memory output is chosen",
    },
  });
  /** The page's capability report; the GPU and color are known once a preview is bound. */
  let capabilities = capabilityReport(null);
  function capabilityReport(rendered) {
    return Object.freeze({
      schema: "alpha2-capabilities",
      version: 1,
      runtime: "browser",
      gpu: rendered?.gpu ?? null,
      color: rendered?.color ?? null,
      playback: rendered?.playback ?? {
        policies: PLAYBACK_POLICIES,
        defaultPolicy: "drop_video_keep_audio",
        audioAtNonUnitRate: false,
      },
      decode: null,
      encode: null,
      features,
    });
  }
  /**
   * Why each text could not be drawn, by {@link ownerKey}: a failed raster is not retried until its
   * authoring or fonts change.
   */
  const textFailures = new Map();
  /**
   * The error a render that shows `asset`, a text of `document` that could not be rasterized, fails
   * with.
   */
  function undrawnText(document, asset) {
    const failure = textFailures.get(ownerKey(document, asset));
    return new EditorError(
      "text_unrenderable",
      `Text "${asset.name}" cannot be drawn: ${failure?.message ?? "it was not rasterized"}`,
      "resource",
      asset.id,
    );
  }
  /**
   * Fails with {@link undrawnText} when `project`, compiled from `document`, shows a text that
   * could not be drawn.
   */
  function requireShownText(document, project) {
    const used = shownSources(project);
    for (const asset of Object.values(document.assets))
      if (
        asset.kind === "text" &&
        used.has(asset.id) &&
        textFailures.has(ownerKey(document, asset))
      )
        throw undrawnText(document, asset);
  }
  /**
   * Rasterizes each text of `document` that no held runtime draws yet: new authoring, or a font
   * that resolves to another family now that it is linked. A text that cannot be drawn (too large
   * for a texture, say) is recorded, not thrown: it fails only the renders that show it.
   */
  function materializeText(document) {
    for (const asset of Object.values(document.assets)) {
      if (asset.kind !== "text" || !asset.authoring?.text) continue;
      const resourceKey = ownerKey(document, asset);
      if (owners.has(resourceKey) || textFailures.has(resourceKey)) continue;
      const staged = new AssetLibrary();
      try {
        const runtime = staged.createText(
          {
            ...asset.authoring.text,
            fontFamily: familiesOf(document, asset.authoring.text).join(", "),
          },
          { id: asset.id },
        );
        const revision = issue(asset.id);
        owners.set(resourceKey, {
          lease: registry.put(asset.id, revision, runtime, () =>
            staged.dispose(),
          ),
          revision,
        });
      } catch (error) {
        staged.dispose();
        textFailures.set(resourceKey, error);
      }
    }
  }
  const hasContent = (project) =>
    project?.timeline.tracks.some(
      (track) =>
        (track.clips?.length ?? 0) + (track.captions?.cues.length ?? 0) > 0,
    );
  // A node the viewer shows instead of a composition's output; preview only.
  let previewOutput = null;
  const withPreviewOutput = (project) => {
    if (!previewOutput) return project;
    const { compositionId, nodeId } = previewOutput;
    const composition = project.compositions?.find(
      (c) => c.id === compositionId,
    );
    if (!composition?.nodes?.some((n) => n.id === nodeId)) return project;
    return {
      ...project,
      compositions: project.compositions.map((c) =>
        c.id === compositionId ? { ...c, output: nodeId } : c,
      ),
    };
  };
  /**
   * The fonts captions and an SVG's text draw with, in fallback order: the document's font assets,
   * then the runtime's own. Native exports get the same stack (`nativeAssets`).
   */
  /**
   * Reports each font of a registered stack the engine could not parse, by name: it was left out,
   * and captions and SVG text draw with the fonts after it.
   */
  function reportRefusedFonts(refused, operation) {
    for (const { font, error } of refused ?? [])
      onError(
        new EditorError(
          "invalid_font",
          `Font ${font.name} cannot be drawn (${error instanceof Error ? error.message : String(error)}); text draws with the other fonts`,
          operation,
          font.id,
          true,
          { cause: error },
        ),
      );
  }
  const fontStackOf = (document) => [
    ...heldFonts(document).map((a) => previewMedia(document, a.id)),
    ...(runtimeAssets.fonts ?? []),
  ];
  /**
   * The document's font assets this session holds a file for. A font is no source the engine
   * checks: one not linked is left out of the stack, by preview and exports alike, never refused.
   */
  function heldFonts(document) {
    return Object.values(document.assets).filter(
      (a) => a.kind === "font" && !a.missing && ownerOf(document, a),
    );
  }
  /**
   * Readies `measurer` with the fonts preview draws the draft with, and answers the draft's canvas
   * size, which preview draws it at.
   */
  async function draftMeasurer() {
    const { WasmTextMeasurer } = await loadWasm();
    if (disposed)
      throw new EditorError("disposed", "Bridge is disposed", "browser");
    const document = editor.draft;
    const fonts = fontStackOf(document);
    measurer ??= new WasmTextMeasurer();
    if (!sameList(measuredFonts, fonts)) {
      // A font the engine refuses is left out here as preview leaves it out, and reported there.
      registerCaptionFonts(measurer, fonts);
      measuredFonts = fonts;
    }
    return document.project.settings;
  }
  const sameList = (a, b) =>
    a !== null &&
    a.length === b.length &&
    a.every((item, index) => item === b[index]);
  const renderKeyOf = (document) =>
    compileEditorDocument(document).revisionKey +
    JSON.stringify(previewOutput) +
    JSON.stringify(
      Object.values(document.assets).map((a) => ownerKey(document, a)),
    );
  const loaded = async () => {
    const document = editor.draft;
    materializeText(document);
    const checked = compileEditorDocument(document);
    if (checked.diagnostics.length) throw checked.diagnostics[0];
    const compiled = {
      ...checked,
      project: withPreviewOutput(checked.project),
    };
    if (!engine || !hasContent(compiled.project)) return compiled.project;
    // The engine selects original or proxy. The answer depends only on the projection, the shown
    // node and the catalog, so a scrub over an unchanged document asks once.
    await discoverProxies(document);
    const catalog = catalogOf(document);
    const conformKey =
      checked.revisionKey +
      JSON.stringify(previewOutput) +
      preferredPreviewMedia +
      JSON.stringify(catalog);
    const before = JSON.stringify([...previewSelection]);
    if (conformKey !== previewConformKey) {
      const selected = conformProject(
        await decisions(),
        compiled.project,
        catalog,
        preferredPreviewMedia === "proxies" ? "preview_proxy" : "preview",
      );
      previewSelection = new Map(
        selected.sources.map((source) => [source.source_id, source.selection]),
      );
      previewConformKey = conformKey;
    }
    const selectionKey = JSON.stringify([...previewSelection]);
    // `proxyStatus` reports what preview reads.
    if (selectionKey !== before) notifyAssets();
    const used = shownSources(compiled.project);
    for (const source of compiled.project.sources)
      if (
        !drawnByEngine(source) &&
        used.has(source.id) &&
        !previewMedia(document, source.id)
      )
        throw document.assets[source.id]?.kind === "text"
          ? undrawnText(document, document.assets[source.id])
          : new EditorError(
              "missing_asset",
              `Relink ${document.assets[source.id]?.name ?? source.id} before preview`,
              "preview",
              source.id,
            );
    const drawnResources = drawnEffectResources(compiled.project);
    for (const resource of compiled.project.effect_resources ?? [])
      if (
        drawnResources.has(resource.id) &&
        !previewMedia(document, resource.id)
      )
        throw new EditorError(
          "missing_asset",
          `Relink ${document.assets[resource.id]?.name ?? resource.id} before preview`,
          "preview",
          resource.id,
        );
    const allFonts = fontStackOf(document);
    const key = renderKeyOf(document);
    // Audio is configured once the edit settles: sound only plays after a gesture.
    const settled = document === editor.document;
    if (renderKey !== key || loadedSelection !== selectionKey) {
      mixer.stop();
      engine.invalidate();
      // Keep decoders warm: only a source whose runtime changed is reopened.
      const reads = new Map(
        Object.values(document.assets).map((a) => [
          a.id,
          previewMedia(document, a.id),
        ]),
      );
      const unchanged = new Set(
        Object.values(document.assets)
          .filter(
            (a) =>
              reads.get(a.id) && loadedAssets.get(a.id) === reads.get(a.id),
          )
          .flatMap((a) =>
            previewSelection.get(a.id)?.media === "proxy"
              ? [a.id, `proxy:${hashOf(a)}`]
              : [a.id],
          ),
      );
      await sources.release(unchanged);
      loadedAssets = reads;
      loadedSelection = selectionKey;
      if (!sameList(loadedFonts, allFonts)) {
        const refused = engine.setCaptionFonts(allFonts);
        loadedFonts = allFonts;
        // Reported only when the list changes.
        reportRefusedFonts(refused, "preview");
      }
      engine.loadProject(compiled.project);
      // Frames render at the document's size times the preview scale, and the surfaces are that
      // size: the engine rounds it.
      let renderSize = engine.previewSize;
      engine.resize(renderSize[0], renderSize[1]);
      // Sources are fitted in document space; the render scales them.
      sources.setOutputSize(
        compiled.project.settings.width,
        compiled.project.settings.height,
      );
      audioKey = null;
      if (workerPreview) {
        const used = [
          ...compiled.project.sources,
          ...(compiled.project.effect_resources ?? []),
        ]
          .map((source) => [source.id, previewMedia(document, source.id)])
          .filter(([, asset]) => asset);
        // The worker keeps runtime assets it already holds; only new or changed ones are sent.
        const keep = used
          .filter(([id, asset]) => workerSent.get(id) === asset)
          .map(([id]) => id);
        const kept = new Set(keep);
        const workerAssets = used
          .filter(([id]) => !kept.has(id))
          // Keyed by the source it serves: a proxy's runtime has an ID of its own.
          .map(([id, asset]) => {
            const data = { id };
            for (const field of [
              "kind",
              "name",
              "file",
              "raster",
              "width",
              "height",
              "duration",
              "hasAudio",
              "audioDecodable",
              "text",
              "fit",
              "provider",
              "frameFiles",
            ])
              if (asset[field] !== undefined) data[field] = asset[field];
            return data;
          });
        const sendFonts = !sameList(workerFonts, allFonts);
        // Until the worker confirms, what it holds is unknown: a failed load resends everything.
        workerSent = new Map();
        workerFonts = null;
        try {
          const reply = await workerPreview.replaceProject(
            compiled.project,
            ++loadRevision,
            workerAssets,
            sendFonts ? allFonts : null,
            keep,
          );
          workerSent = new Map(used);
          workerFonts = allFonts;
          if (reply?.renderSize) renderSize = reply.renderSize;
        } catch (error) {
          // The main renderer has just loaded the same project.
          await abandonWorker(error);
        }
      }
      sizeBoundCanvas(renderSize);
      renderKey = key;
    }
    if (settled && audioKey !== key) {
      supplier.configure(engine.audioEngine, compiled.project);
      await mixer.attach(
        engine.audioEngine,
        compiled.project,
        supplier,
        null,
        onError,
      );
      audioKey = key;
    }
    return compiled.project;
  };
  const off = editor.subscribe(() => {
    if (editor.generation !== sessionGeneration) {
      sessionGeneration = editor.generation;
      for (const abort of pendingImports) abort.abort();
      pause();
      renderKey = null;
      workerSent = new Map();
      workerFonts = null;
      loadedAssets = new Map();
      loadedFonts = null;
      loadedSelection = "";
      previewSelection = new Map();
      previewConformKey = null;
      absentProxies = new Set();
    }
    scheduleSweep();
    scheduleTextReports();
    // A drag changes the draft on every pointer move; the preview follows at most once a frame.
    if (engine && !refreshFrame) refreshFrame = nextFrame(refresh);
  });
  /** The font stack each SVG runtime's report was last computed with. */
  const reportedFonts = new WeakMap();
  /** Aborted when the bridge is disposed: work it started for no caller (text reports) stops. */
  const lifetime = new AbortController();
  let textReports = Promise.resolve(),
    textReportsQueued = false;
  /**
   * An SVG's report says which of its text no font draws (`missing_font`). Import probes it with no
   * fonts at all, so it is computed again with the stack its text draws with whenever that stack or
   * the SVG changes; `subscribeAssets` listeners hear of a report that changed.
   */
  function scheduleTextReports() {
    if (textReportsQueued || disposed) return;
    textReportsQueued = true;
    textReports = textReports
      .then(() => {
        textReportsQueued = false;
        return refreshTextReports();
      })
      .catch(() => {});
  }
  async function refreshTextReports() {
    const document = editor.document;
    const fonts = fontStackOf(document);
    const stale = Object.values(document.assets)
      .map((asset) => ownerOf(document, asset)?.lease.value)
      .filter(
        (runtime) =>
          runtime?.provider?.format === "svg" &&
          runtime.report &&
          runtime.file &&
          !sameList(reportedFonts.get(runtime) ?? null, fonts),
      );
    if (!stale.length) return;
    let changed = false;
    for (const runtime of new Set(stale)) {
      let diagnostics;
      try {
        diagnostics = await callExtensions(
          {
            factory: runtimeAssets.extensionsWorkerFactory,
            wasmUrl: runtimeAssets.extensionsWasmUrl,
          },
          {
            call: "svg-diagnostics",
            file: runtime.file,
            provider: runtime.provider,
            fonts: fonts.map((font) => font.bytes),
          },
          { signal: lifetime.signal, operation: "report" },
        );
        if (disposed) return;
      } catch {
        if (disposed) return;
        // Unreadable now: the report stays and the next change tries again.
        continue;
      }
      reportedFonts.set(runtime, fonts);
      if (
        JSON.stringify(diagnostics) !==
        JSON.stringify(runtime.report.diagnostics)
      ) {
        runtime.report = { ...runtime.report, diagnostics };
        changed = true;
      }
    }
    if (changed) notifyAssets();
  }
  let sweepQueued = false;
  /** Releases, after the preview work already queued, what no reachable document reads any more. */
  function scheduleSweep() {
    if (sweepQueued || disposed) return;
    sweepQueued = true;
    void enqueue(() => {
      sweepQueued = false;
      sweep();
    }).catch((error) => {
      if (!disposed) onError(error);
    });
  }
  /**
   * Media and proxies stay held while any document the session can return to reads them: a relink
   * or import undone, or a removed asset, comes back with its runtime. Text is held only while the
   * document or the draft draws it, since its authoring makes it again. Work that leased a runtime
   * (an export, a native upload) keeps it until it releases the lease.
   */
  function sweep() {
    const media = new Set(),
      hashes = new Set(),
      seen = new Set();
    for (const document of editor.reachableDocuments()) {
      // Shared by commands that leave assets alone, so a long history is read once per media change.
      if (seen.has(document.assets)) continue;
      seen.add(document.assets);
      for (const asset of Object.values(document.assets)) {
        if (asset.kind !== "text")
          media.add(key(asset.id, asset.contentRevision));
        const hash = hashOf(asset);
        if (hash) hashes.add(hash);
      }
    }
    const shown = new Set();
    for (const document of new Set([editor.document, editor.draft]))
      for (const asset of Object.values(document.assets))
        if (asset.kind === "text") shown.add(ownerKey(document, asset));
    for (const [resourceKey, owner] of owners)
      if (!media.has(resourceKey) && !shown.has(resourceKey)) {
        owners.delete(resourceKey);
        analyses.get(owner.lease.value)?.abort();
        owner.lease.release();
      }
    for (const resourceKey of textFailures.keys())
      if (!shown.has(resourceKey)) textFailures.delete(resourceKey);
    for (const [hash, proxy] of proxies)
      if (!hashes.has(hash)) {
        proxies.delete(hash);
        proxy.lease.release();
      }
  }
  const nextFrame = (fn) =>
    typeof requestAnimationFrame === "function"
      ? requestAnimationFrame(fn)
      : setTimeout(fn, 16);
  function refresh() {
    refreshFrame = 0;
    if ((!engine && !recoveryPending) || disposed) return;
    if (engine && renderKeyOf(editor.draft) === renderKey) return;
    if (!playing) {
      refreshPreview();
      return;
    }
    // Playback reads the project it started with, so an edit restarts it from where it stood, at the
    // same rate, loop and policy, unless a seek, pause or Play comes first.
    const resume = playRequest;
    pause();
    refreshPreview();
    const stopped = playGeneration;
    void Promise.resolve()
      .then(() =>
        disposed || playing || playGeneration !== stopped
          ? undefined
          : play(resume.rate, { loop: resume.loop, policy: resume.policy }),
      )
      .catch(onError);
  }
  // At most one refresh waits, reading the draft when it starts. The render in flight is not
  // cancelled, so a continuous drag keeps showing frames.
  let refreshQueued = false;
  function refreshPreview() {
    if (refreshQueued) return;
    refreshQueued = true;
    const requested = tick;
    void enqueue(async () => {
      refreshQueued = false;
      if (requested !== tick || !(await previewReady())) return false;
      const project = await loaded();
      return requested === tick ? renderAt(requested, project) : false;
    }).catch(onError);
  }
  let previewIssue = null;
  /** A lost GPU left no renderer and making it again failed; the next render tries again. */
  let recoveryPending = false;
  /** How many times a lost GPU's preview was made again. */
  let previewRecoveries = 0;
  const previewListeners = new Set();
  const setPreviewIssue = (issue) => {
    if (issue === previewIssue) return;
    previewIssue = issue;
    for (const listener of previewListeners) listener();
  };
  /**
   * Renders the frame at `at`. A composition that cannot draw its output yet is not an error here:
   * the previous frame stays up and `previewIssue` says why, until a render succeeds.
   */
  async function renderAt(at, project, recovered = false) {
    try {
      const rendered = await renderFrame(at, project);
      setPreviewIssue(null);
      return rendered;
    } catch (error) {
      if (error?.code === "INCOMPLETE_COMPOSITION") {
        setPreviewIssue(error.message ?? String(error));
        return false;
      }
      const lost =
        error?.code === "GPU_DEVICE_LOST" || engine?.deviceLost === true;
      if (recovered || !lost || !boundCanvas || disposed) throw error;
      if (!(await recoverPreview())) return false;
      return renderAt(at, await loaded(), true);
    }
  }
  async function renderFrame(at, project) {
    if (boundCanvas?.style)
      boundCanvas.style.visibility = hasContent(project) ? "visible" : "hidden";
    if (!hasContent(project)) return false;
    if (workerPreview) {
      try {
        const result = await workerPreview.seek(at);
        if (result.engineDiagnostics)
          workerDiagnostics = result.engineDiagnostics;
        // An unchanged frame comes without a bitmap: the canvas still shows it.
        if (result.bitmap) {
          boundCanvas
            .getContext("bitmaprenderer")
            .transferFromImageBitmap(result.bitmap);
          bitmapTransfers += 1;
        }
        return result.rendered;
      } catch (error) {
        if (error?.code === "INCOMPLETE_COMPOSITION") throw error;
        // The main renderer has the same project loaded and draws this frame instead.
        await abandonWorker(error);
      }
    }
    const { rendered, unchanged } = await engine.render(at / 120000, {
      generation: engine.generation,
      context: context(project, library),
    });
    // An unchanged frame was not presented: the bound canvas still shows the last copy.
    if (rendered && !unchanged && copySurface) {
      boundCanvas
        .getContext("bitmaprenderer")
        .transferFromImageBitmap(engine.canvas.transferToImageBitmap());
      bitmapTransfers += 1;
    }
    return rendered;
  }
  /** Gives the bound canvas the size frames render at, which `size` is, unless it has it. */
  function sizeBoundCanvas(size) {
    if (!boundCanvas || !size) return;
    if (boundCanvas.width !== size[0]) boundCanvas.width = size[0];
    if (boundCanvas.height !== size[1]) boundCanvas.height = size[1];
  }
  /** Applies the bridge's tracing to the renderers drawing now. */
  function applyTracing() {
    if (engine) {
      engine.traceUploads = traceUploads;
      engine.setPassTiming?.(passTiming);
    }
    void workerPreview
      ?.setTracing({ passTiming, traceUploads })
      .catch(() => {});
  }
  /**
   * Gives up the preview worker after `error`, whether it failed to start, to load a project or to
   * draw: the main renderer, which loads every project the worker does, draws from then on, off
   * screen, and each frame is copied to the bound surface (a canvas element or an OffscreenCanvas
   * alike); a preview made again after a lost GPU does too, until the next binding. `onError` hears
   * once why, unless the worker lost its GPU: that is recovered from, as the main renderer recovers
   * from its own, without a report, and a preview made again tries a worker again.
   */
  async function abandonWorker(error) {
    const failed = workerPreview;
    if (!failed) return;
    workerPreview = null;
    workerSent = new Map();
    workerFonts = null;
    copySurface = true;
    await failed.dispose().catch(() => {});
    if (error?.code === "GPU_DEVICE_LOST") return;
    workerGivenUp = true;
    const reason = error instanceof Error ? error.message : String(error ?? "");
    onError(
      new EditorError(
        "preview_worker_failed",
        `The preview worker failed (${reason || "it did not start"}); the preview renders on the main thread`,
        "preview",
        undefined,
        true,
        { cause: error },
      ),
    );
  }
  async function seek(nextTick) {
    if (!Number.isSafeInteger(nextTick) || nextTick < 0)
      throw new EditorError(
        "invalid_time",
        "Seek requires non-negative integer ticks",
        "seek",
      );
    if (playing) pause();
    else playGeneration++;
    tick = nextTick;
    engine?.invalidate();
    mixer?.stop();
    const requested = tick;
    // Only a newer seek supersedes this one: `loaded` reads the draft as it is when the task runs.
    return enqueue(async () => {
      if (requested !== tick) return false;
      if (!(await previewReady())) return false;
      const project = await loaded();
      if (requested !== tick) return false;
      return renderAt(requested, project);
    });
  }
  function pause() {
    playing = false;
    playGeneration++;
    if (raf) cancelAnimationFrame(raf);
    raf = 0;
    playClock?.free();
    playClock = null;
    mixer?.stop();
    engine?.invalidate();
    if (workerPreview) void workerPreview.cancel().catch(onError);
  }
  // An audio start that missed its deadline and has not settled. A context that never resumes
  // (Firefox without usable output) or a stuck worklet would hold Play forever; video plays silent.
  let stalledAudio = null;
  /** Whether `starting` settled within the deadline; a rejection propagates. */
  async function audioStarts(starting) {
    let timer = 0;
    const started = await Promise.race([
      starting.then(() => true),
      new Promise((resolve) => {
        timer = setTimeout(() => resolve(false), AUDIO_START_TIMEOUT_MS);
      }),
    ]).finally(() => clearTimeout(timer));
    if (started) return true;
    stalledAudio = starting;
    const settle = () => {
      if (stalledAudio === starting) stalledAudio = null;
    };
    starting.then(settle, settle);
    onError(
      new EditorError(
        "audio_unavailable",
        `Audio output did not start within ${AUDIO_START_TIMEOUT_MS / 1000} s; playing without sound`,
        "play",
      ),
    );
    return false;
  }
  /**
   * Plays from the current time at `rate` (negative plays backwards) under a playback `policy`: the
   * engine's clock decides which frame shows when and whether sound plays. Sound that cannot start
   * in time is left out rather than holding the picture.
   */
  async function play(
    rate = 1,
    { loop = false, policy = "drop_video_keep_audio" } = {},
  ) {
    if (!Number.isFinite(rate) || rate === 0)
      throw new EditorError(
        "invalid_rate",
        "Rate must be finite and non-zero",
        "play",
      );
    if (!PLAYBACK_POLICIES.includes(policy))
      throw new EditorError(
        "invalid_policy",
        `Unknown playback policy ${policy}`,
        "play",
      );
    pause();
    const generation = playGeneration;
    let audible =
      (engine?.audible(policy, rate) ?? false) && stalledAudio === null;
    if (audible) audible = await audioStarts(mixer.resume());
    // Plays the compiled projection preview shows, never a project the renderer loaded earlier.
    let project = null;
    await enqueue(async () => {
      if (!engine || generation !== playGeneration) return;
      project = await loaded();
      if (!hasContent(project)) return;
      if (generation === playGeneration && audible) {
        audible = await audioStarts(mixer.start(tick, rate));
        if (!audible) mixer.stop();
      }
    });
    if (!engine || generation !== playGeneration) return;
    if (!hasContent(project)) {
      pause();
      return;
    }
    playing = true;
    playRequest = { rate, loop, policy };
    const clock = (playClock = engine.playbackClock(
      policy,
      project.settings.frame_rate,
      rate,
      tick,
      performance.now(),
    ));
    // The tick last asked for, and for a policy that waits for frames, whether it has been shown.
    let requested = null,
      presented = false;
    const step = (now) => {
      if (!playing || disposed || generation !== playGeneration) return;
      // At 1x under the default policy the audio clock leads while output consumes samples; the
      // engine's clock falls back to the wall clock while it stands still.
      const heard = audible ? (mixer.tick ?? undefined) : undefined;
      const wanted = clock.step(now, heard, presented);
      tick = wanted.tick;
      const waits = wanted.waitForFrame;
      wanted.free();
      const end =
        editor.draft.metadata.exportRange?.end ?? sequenceEnd(project);
      const lower = editor.draft.metadata.exportRange?.start ?? 0;
      if ((rate > 0 && tick >= end) || (rate < 0 && tick <= lower)) {
        pause();
        const last = frameStart(
          frameAt(Math.max(0, end - 1), project.settings.frame_rate),
          project.settings.frame_rate,
        );
        tick = rate > 0 ? last : lower;
        const stoppedGeneration = playGeneration;
        void enqueue(() => renderAt(tick, project))
          .then(() => {
            if (loop && !disposed && playGeneration === stoppedGeneration) {
              tick = rate > 0 ? lower : last;
              void play(rate, { loop, policy }).catch(onError);
            }
          })
          .catch(onError);
        return;
      }
      // The clock asks for frame starts, so every policy asks for each frame once however often
      // the display refreshes; a waiting policy also moves on only after it is shown, and the
      // others let late frames go.
      const due = requested !== tick;
      if (!due) skippedRequests += 1;
      else if (!renderScheduled) {
        renderScheduled = true;
        requested = tick;
        if (waits) presented = false;
        const at = tick;
        void enqueue(() =>
          generation !== playGeneration ? false : renderAt(at, project),
        )
          .then(() => {
            if (requested === at) presented = true;
          })
          .catch((error) => {
            pause();
            onError(error);
          })
          .finally(() => {
            renderScheduled = false;
          });
      }
      raf = requestAnimationFrame(step);
    };
    raf = requestAnimationFrame(step);
  }
  const visibility = () => {
    if (document.hidden) pause();
  };
  /**
   * Makes the preview's renderer, audio, and worker for `canvas`, disposing what the last one made.
   * Runs inside the render queue. Returns false when a later binding or disposal superseded
   * `token`.
   */
  async function buildPreview(canvas, token) {
    pause();
    await mixer?.close();
    await supplier?.close();
    await engine?.dispose();
    await sources?.dispose();
    sources = new SourcePixels(library, {
      extensionsWasmUrl: runtimeAssets.extensionsWasmUrl,
    });
    const { Engine } = await import("./runtime/engine.js");
    // Until a project loads and gives the render size (see `loaded`).
    canvas.width = editor.document.project.settings.width;
    canvas.height = editor.document.project.settings.height;
    boundCanvas = canvas;
    copySurface = false;
    await workerPreview?.dispose();
    workerPreview = null;
    workerDiagnostics = null;
    const useWorker =
      features.worker.available && typeof canvas.getContext === "function";
    engine = new Engine(
      useWorker ? new OffscreenCanvas(canvas.width, canvas.height) : canvas,
      sources,
      library,
      runtimeAssets,
      // A frame shown at once matters more in a preview than the engine's conversion of it.
      { unreadableFrames: "present", videoConversion },
    );
    mixer = new EngineAudioMixer(runtimeAssets, { enabled: audio });
    mixer.setMasterVolume(monitorVolume);
    stalledAudio = null;
    supplier = new AudioWindowSupplier(library);
    try {
      await engine.start();
      engine.setPreviewScale(previewScale);
      capabilities = capabilityReport(engine.capabilityReport());
      await registerKeptEffects();
      // The surface already shows copied frames (its context is a bitmap renderer's).
      if (useWorker && workerGivenUp) copySurface = true;
      else if (useWorker) {
        const workerRuntime = {
          ...(runtimeAssets.wasmUrl
            ? { wasmUrl: String(runtimeAssets.wasmUrl) }
            : {}),
          ...(runtimeAssets.extensionsWasmUrl
            ? { extensionsWasmUrl: String(runtimeAssets.extensionsWasmUrl) }
            : {}),
        };
        workerPreview = createWorkerPreview(runtimeAssets.workerFactory(), {
          runtimeAssets: workerRuntime,
          bitmapOutput: true,
          width: canvas.width,
          height: canvas.height,
          videoConversion,
        });
        // A worker that cannot start leaves the surface to the main renderer.
        try {
          await workerPreview.ready;
          await workerPreview.setPreviewScale(previewScale);
          if (effectDefinitions.size)
            await workerPreview.registerEffects(keptEffects());
        } catch (error) {
          await abandonWorker(error);
        }
      }
      if (passTiming || traceUploads) applyTracing();
      if (disposed || token !== binding) {
        await engine.dispose();
        engine = null;
        return false;
      }
      renderKey = null;
      workerSent = new Map();
      workerFonts = null;
      loadedAssets = new Map();
      loadedFonts = null;
    } catch (error) {
      await workerPreview?.dispose().catch(() => {});
      workerPreview = null;
      await engine.dispose();
      engine = null;
      throw error;
    }
    return true;
  }
  /**
   * A lost GPU (a WebGPU device, a WebGL context) is recovered here rather than reported: the
   * preview is made again on the same canvas, as binding it did, and the frame drawn once more. The
   * binding waits for the browser to restore a WebGL context on that canvas. When that fails the
   * error is reported and the next render tries again.
   *
   * Since nothing is reported, what the user sees goes on: playback that was running plays on from
   * where it stood, at the same rate, loop and policy, once the preview is made again, unless a
   * seek, a pause or another Play came first. Sound that had not started in time is not waited for
   * again: the audio output is not what was lost.
   */
  async function recoverPreview() {
    recoveryPending = true;
    const resume = playing ? playRequest : null;
    const stalled = stalledAudio;
    if (!(await buildPreview(boundCanvas, binding))) return false;
    stalledAudio = stalled;
    recoveryPending = false;
    previewRecoveries += 1;
    if (resume) {
      // Queued behind the render that is recovering, which draws the frame first.
      const stopped = playGeneration;
      void Promise.resolve()
        .then(() =>
          disposed || playing || playGeneration !== stopped
            ? undefined
            : play(resume.rate, { loop: resume.loop, policy: resume.policy }),
        )
        .catch(onError);
    }
    return true;
  }
  /**
   * Runtime effects `registerEffects` kept, by `id@version`, with their JSON text, in the order they
   * came: every renderer the bridge makes registers them.
   */
  const effectDefinitions = new Map();
  const keptEffects = () =>
    [...effectDefinitions.values()].map(({ definition }) => definition);
  function registerEffects(definitions) {
    return enqueue(async () => {
      const added = new Map();
      for (const definition of definitions) {
        if (
          typeof definition?.id !== "string" ||
          !Number.isInteger(definition.version)
        )
          throw new EditorError(
            "invalid_effect",
            "An effect definition needs a string id and an integer version",
            "effects",
          );
        const key = `${definition.id}@${definition.version}`;
        const text = JSON.stringify(definition);
        const known = (added.get(key) ?? effectDefinitions.get(key))?.text;
        if (known === text) continue;
        if (known !== undefined)
          throw new EditorError(
            "effect_conflict",
            `Effect ${key} is registered with another definition; a changed effect takes another version`,
            "effects",
            key,
          );
        added.set(key, { definition, text });
      }
      if (!added.size) return;
      const fresh = [...added.values()].map(({ definition }) => definition);
      // The main renderer loads every project the worker does, so both hold every effect.
      if (engine) await engine.registerEffects(fresh);
      if (workerPreview) {
        try {
          await workerPreview.registerEffects(fresh);
        } catch (error) {
          // The main renderer has just registered the same definitions.
          await abandonWorker(error);
        }
      }
      for (const [key, entry] of added) effectDefinitions.set(key, entry);
      // A frame that drew one of them failed; it draws now.
      if (engine) refreshPreview();
    });
  }
  /**
   * Registers the kept runtime effects on the main renderer `buildPreview` just started, one by
   * one: one its device refuses (kept while no preview was bound, so never compiled) is dropped and
   * reported, and the others draw. A device lost meanwhile drops none: the next render makes the
   * preview again, which registers them again.
   */
  async function registerKeptEffects() {
    const kept = [...effectDefinitions];
    const results = await Promise.allSettled(
      kept.map(([, { definition }]) => engine.registerEffects([definition])),
    );
    if (engine.deviceLost) return;
    for (const [index, result] of results.entries()) {
      if (result.status === "fulfilled") continue;
      effectDefinitions.delete(kept[index][0]);
      onError(result.reason);
    }
  }
  /** Whether a renderer is ready, making it again first if a lost GPU left none. */
  async function previewReady() {
    if (engine) return true;
    if (!recoveryPending || !boundCanvas || disposed) return false;
    return recoverPreview();
  }
  /**
   * Renders the preview at `scale` times the document's size from now on, sizing the surfaces to
   * it, and draws the current frame again. Resolves once that frame is drawn.
   */
  async function setPreviewScale(scale) {
    const next = checkedPreviewScale(scale);
    if (next === previewScale) return;
    previewScale = next;
    await enqueue(async () => {
      if (!engine || disposed) return;
      engine.setPreviewScale(previewScale);
      let size = renderKey === null ? null : engine.previewSize;
      if (size) engine.resize(size[0], size[1]);
      if (workerPreview) {
        try {
          const reply = await workerPreview.setPreviewScale(previewScale);
          if (reply?.renderSize) size = reply.renderSize;
        } catch (error) {
          await abandonWorker(error);
        }
      }
      sizeBoundCanvas(size);
      await renderAt(tick, await loaded());
    });
  }
  async function bindPreview(canvas) {
    const token = ++binding;
    await enqueue(async () => {
      recoveryPending = false;
      workerGivenUp = false;
      skippedRequests = 0;
      bitmapTransfers = 0;
      if (!(await buildPreview(canvas, token))) return;
      // The surface is bound; a failing first frame is reported like any later one.
      try {
        await renderAt(tick, await loaded());
      } catch (error) {
        onError(error);
      }
    });
    globalThis.document?.addEventListener("visibilitychange", visibility);
    let released = false;
    return {
      get canvas() {
        return boundCanvas;
      },
      async dispose() {
        if (released) return;
        released = true;
        if (token !== binding) return;
        ++binding;
        pause();
        globalThis.document?.removeEventListener(
          "visibilitychange",
          visibility,
        );
        await enqueue(async () => {
          await mixer?.close();
          await supplier?.close();
          await workerPreview?.dispose();
          workerPreview = null;
          await engine?.dispose();
          await sources?.dispose();
          engine = null;
          sources = null;
          renderKey = null;
          workerSent = new Map();
          workerFonts = null;
          loadedAssets = new Map();
          loadedFonts = null;
        });
      },
    };
  }
  function importFiles(files, options = {}) {
    return importUnits(
      files.map((file) => ({
        name: file.name,
        stage: (signal) => mediaAdapter.stage(file, signal),
      })),
      options,
    );
  }
  /** Imports numbered PNGs as one image sequence at `frameRate` (the project's by default). */
  function importSequence(
    files,
    { frameRate, missing = "reject", signal } = {},
  ) {
    const rate = frameRate ?? editor.document.project.settings.frame_rate;
    return importUnits([sequenceUnit(files, { frameRate: rate, missing })], {
      signal,
    });
  }
  /** Relinks the image sequence `id` to `files`, staged as the project recorded it. */
  function relinkSequence(id, files, { signal } = {}) {
    const provider = editor.document.project.sources.find(
      (source) => source.id === id,
    )?.provider;
    if (provider?.format !== "image_sequence")
      return Promise.reject(
        new EditorError(
          "relink_conflict",
          "Only an image sequence relinks to frames",
          "relink",
          id,
        ),
      );
    return importUnits(
      [
        sequenceUnit(files, {
          frameRate: provider.frame_rate,
          missing: provider.missing,
          id,
        }),
      ],
      { signal, relinkId: id },
    );
  }
  function sequenceUnit(files, options) {
    if (!mediaAdapter.stageSequence)
      throw new EditorError(
        "unsupported_media",
        "This media adapter cannot import image sequences",
        "import",
      );
    return {
      name: null,
      stage: (signal) => mediaAdapter.stageSequence(files, options, signal),
    };
  }
  /**
   * Stages `units` (at most {@link IMPORT_STAGE_CONCURRENCY} at once) and adds them to the document
   * in their order, as one undo step; a relink has exactly one.
   */
  async function importUnits(units, { signal, relinkId } = {}) {
    if (disposed)
      throw new EditorError("disposed", "Bridge is disposed", "import");
    const generation = editor.generation,
      controller = new AbortController();
    pendingImports.add(controller);
    const abort = () => controller.abort(signal?.reason);
    signal?.addEventListener("abort", abort, { once: true });
    if (signal?.aborted) abort();
    // Index `i` is unit `i`'s staged asset; a unit never started leaves its slot empty.
    const staged = new Array(units.length),
      leases = [],
      commands = [];
    try {
      if (relinkId && units.length !== 1)
        throw new EditorError(
          "relink_conflict",
          "Relink accepts exactly one file",
          "relink",
          relinkId,
        );
      // Staged together, committed in order. The first file that fails stops the rest, and its
      // error is the import's, not the cancellation the others then see.
      let failure = null;
      let next = 0;
      const stageNext = async () => {
        while (next < units.length && failure === null) {
          const index = next++;
          try {
            staged[index] = await units[index].stage(controller.signal);
          } catch (error) {
            failure ??= { error };
            controller.abort(error);
          }
        }
      };
      const limit = Math.max(
        1,
        Math.min(
          IMPORT_STAGE_CONCURRENCY,
          (globalThis.navigator?.hardwareConcurrency ?? 2) - 1,
        ),
      );
      await Promise.all(
        Array.from({ length: Math.min(limit, units.length) }, stageNext),
      );
      if (failure) throw failure.error;
      for (const [index, unit] of units.entries()) {
        const item = staged[index];
        const fileName = unit.name ?? item.name ?? item.runtime.name;
        controller.signal.throwIfAborted();
        if (disposed || editor.disposed || editor.generation !== generation)
          throw new EditorError(
            "stale_import",
            "Project changed during import",
            "import",
          );
        const old = relinkId ? editor.document.assets[relinkId] : null;
        if (relinkId && !old)
          throw new EditorError(
            "missing_asset",
            "Unknown relink target",
            "relink",
            relinkId,
          );
        // The engine decides whether the file is the recorded media: equal hashes match whatever else
        // differs; without them any differing recorded part means other media.
        if (old?.fingerprint && item.fingerprint) {
          const match = compareFingerprints(
            await decisions(),
            old.fingerprint,
            item.fingerprint,
          );
          if (match.kind === "mismatch")
            throw new EditorError(
              "relink_conflict",
              `${fileName} is not the saved media: its ${match.fields
                .map((field) => field.replace(/_/g, " "))
                .join(", ")} differ`,
              "relink",
              relinkId,
            );
        }
        if (old && old.kind !== item.runtime.kind)
          throw new EditorError(
            "relink_conflict",
            "Media kind differs",
            "relink",
            relinkId,
          );
        const id = relinkId ?? item.runtime.id;
        // Above every revision this bridge issued: after an undo the next one may be an undone relink's.
        const contentRevision = issue(id, (old?.contentRevision ?? -1) + 1);
        // A relink reads the same media, so it opens as the project chose, not as a fresh import would.
        const provider = relinkId
          ? keepProviderChoices(
              editor.document.project.sources.find((s) => s.id === relinkId)
                ?.provider,
              item.runtime.provider,
              item.runtime.report,
            )
          : item.runtime.provider;
        const runtime = {
          ...item.runtime,
          id,
          ...(provider ? { provider } : {}),
        };
        const authoring = JSON.parse(JSON.stringify(item.description));
        if (provider && authoring.provider) authoring.provider = provider;
        const asset = {
          id,
          kind: runtime.kind,
          name: fileName,
          contentRevision,
          missing: false,
          locator: `browser://${id}`,
          ...(item.fingerprint ? { fingerprint: item.fingerprint } : {}),
          authoring,
        };
        // Called as a method: an adapter's staged asset may be a class instance.
        const lease = registry.put(id, contentRevision, runtime, () =>
          item.dispose(),
        );
        leases.push({ id, contentRevision, lease });
        commands.push({
          type: "asset.put",
          asset,
          ...(runtime.kind === "font"
            ? {}
            : runtime.kind === "lut3d"
              ? { resource: { id, kind: "lut3d", locator: asset.locator } }
              : {
                  source: {
                    id,
                    kind: runtime.kind === "text" ? "image" : runtime.kind,
                    locator: asset.locator,
                    has_audio: !!runtime.hasAudio,
                    color_metadata: runtime.colorMetadata ?? null,
                    ...(item.fingerprint
                      ? { fingerprint: item.fingerprint }
                      : {}),
                    ...(runtime.provider ? { provider: runtime.provider } : {}),
                  },
                }),
        });
      }
      for (const entry of leases)
        owners.set(key(entry.id, entry.contentRevision), {
          lease: entry.lease,
          revision: entry.contentRevision,
        });
      const result = editor.dispatch({ type: "transaction", commands });
      if (!result.ok) throw result.error;
      for (const entry of leases) analyze(entry.lease.value);
      return { assetIds: leases.map((e) => e.id) };
    } catch (error) {
      for (const entry of leases) {
        owners.delete(key(entry.id, entry.contentRevision));
        entry.lease.release();
      }
      for (const item of staged.slice(leases.length)) item?.dispose();
      throw error;
    } finally {
      pendingImports.delete(controller);
      signal?.removeEventListener("abort", abort);
    }
  }
  /**
   * The proxy conversion running: `preempt` stops it for an export, and `done` settles when it
   * ends.
   */
  let proxyRun = null;
  /**
   * Waits until no export runs and no other proxy is being made, or `signal` aborts, and takes the
   * turn: `signal` aborts when an export takes its place, and `end()` gives the turn back.
   */
  async function proxyTurn(signal) {
    for (;;) {
      signal?.throwIfAborted();
      if (jobs.size > 0) await settledOrAborted(exportSettled, signal);
      else if (proxyRun) await settledOrAborted(proxyRun.done, signal);
      else break;
    }
    let finish;
    const run = {
      preempt: new AbortController(),
      done: new Promise((resolve) => (finish = resolve)),
    };
    proxyRun = run;
    return {
      signal: run.preempt.signal,
      end() {
        if (proxyRun === run) proxyRun = null;
        finish();
      },
    };
  }
  let sweptExports = false;
  /** Settles when the running export does. */
  let exportSettled = Promise.resolve();
  function startExport(options = {}) {
    if (disposed)
      throw new EditorError("disposed", "Bridge is disposed", "export");
    if (!sweptExports) {
      sweptExports = true;
      // Temporary files a closed page left: a day old at least and held by no page.
      void import("./runtime/output.js").then(({ sweepStaleExports }) =>
        sweepStaleExports(),
      );
    }
    if (jobs.size)
      throw new EditorError(
        "export_busy",
        "Only one export may run per bridge",
        "export",
      );
    requireExportFrameRate(options.frameRate);
    const videoDecode = options.videoDecode ?? "auto";
    if (videoDecode !== "auto" && videoDecode !== "software")
      throw new EditorError(
        "invalid_video_decode",
        `Export videoDecode must be "auto" or "software"; received ${JSON.stringify(videoDecode)}`,
        "export",
      );
    const exportConversion = checkedVideoConversion(
      options.videoConversion ?? videoConversion,
      "export",
    );
    materializeText(editor.document);
    const snapshot = editor.document,
      revision = editor.revision,
      effects = keptEffects();
    const compiled = compileEditorDocument(snapshot);
    if (compiled.diagnostics.length) throw compiled.diagnostics[0];
    requireShownText(snapshot, compiled.project);
    const selection = exportSelection(snapshot, compiled.project, options);
    const held = [],
      map = new Map(),
      fontStack = [];
    const used = shownSources(compiled.project);
    const drawnResources = drawnEffectResources(compiled.project);
    try {
      for (const source of [
        ...compiled.project.sources.filter((s) => used.has(s.id)),
        ...(compiled.project.effect_resources ?? []).filter((r) =>
          drawnResources.has(r.id),
        ),
      ]) {
        if (drawnByEngine(source)) continue;
        const asset = snapshot.assets[source.id];
        const lease =
          selection.get(source.id)?.media === "proxy"
            ? acquireProxy(hashOf(asset), source.id)
            : acquireOwned(snapshot, asset);
        held.push(lease);
        map.set(source.id, sourceView(snapshot, source.id, lease.value));
      }
      for (const font of heldFonts(snapshot)) {
        const lease = acquireOwned(snapshot, font);
        held.push(lease);
        fontStack.push(lease.value);
      }
      fontStack.push(...(runtimeAssets.fonts ?? []));
    } catch (error) {
      held.forEach((l) => l.release());
      throw error;
    }
    let fontsReported = false;
    const backend = {
      // Each attempt builds its own engine, device, decoders and output; nothing failed is reused.
      async run(document, signal, progress, attempt) {
        const { Engine } = await import("./runtime/engine.js");
        const { exportTimeline } = await import("./runtime/exporter.js");
        const { openExportOutput } = await import("./runtime/output.js");
        const project = compiled.project,
          lib = { get: (id) => map.get(id) };
        const pixels = new SourcePixels(lib, {
          extensionsWasmUrl: runtimeAssets.extensionsWasmUrl,
          videoDecode,
        });
        const renderer = new Engine(
          new OffscreenCanvas(project.settings.width, project.settings.height),
          pixels,
          lib,
          runtimeAssets,
          { videoConversion: exportConversion },
        );
        const audio = new AudioWindowSupplier(lib);
        try {
          signal.throwIfAborted();
          await renderer.start();
          signal.throwIfAborted();
          await renderer.registerEffects(effects, "export");
          signal.throwIfAborted();
          renderer.loadProject(project, "export");
          const refused = renderer.setCaptionFonts(fontStack);
          // Heard once per job, not once per attempt.
          if (!fontsReported) reportRefusedFonts(refused, "export");
          fontsReported = true;
          pixels.setOutputSize(project.settings.width, project.settings.height);
          audio.configure(renderer.audioEngine, project);
          const range = document.metadata.exportRange ?? {
            start: 0,
            end: sequenceEnd(project),
          };
          if (range.end <= range.start)
            throw new EditorError(
              "empty_export",
              "Project has no exportable range",
              "export",
            );
          const settings = project.settings;
          const { written, output, result } = await exportTimeline({
            engine: renderer.wasm,
            project,
            doc: {
              settings: {
                width: settings.width,
                height: settings.height,
                fpsNum: settings.frame_rate.num,
                fpsDen: settings.frame_rate.den,
                sampleRate: settings.sample_rate,
              },
            },
            library: lib,
            audioSupplier: audio,
            range: { start: range.start / 120000, end: range.end / 120000 },
            options: attemptOptions(options, attempt, project.settings),
            openOutput: ({ container, ...estimate }) =>
              openExportOutput({
                jobId: attempt.id.replace(/[^\w-]/g, "_"),
                name: `${project.name}.${containerPolicy(container).extensions[0]}`,
                container,
                target: options.output,
                ...estimate,
              }),
            signal,
            onProgress: (p) =>
              progress({
                phase: p.phase === "finalizing" ? "finalizing" : "rendering",
                fraction: p.ratio,
                ...(p.frameCount === undefined
                  ? {}
                  : { frames: { completed: p.frame, total: p.frameCount } }),
              }),
            supplyFrame: (seconds) =>
              renderer.supply(seconds, { context: context(project, lib) }),
            renderFrame: () => renderer.beginExportFrame(),
            sourceTransfer: () => renderer.sourceTransfer,
          });
          progress({ phase: "publishing", fraction: 0.99 });
          let artifact;
          try {
            signal.throwIfAborted();
            artifact = await output.finish(written);
          } catch (error) {
            await output.abort(error).catch(() => {});
            throw error;
          }
          return {
            ...artifact,
            media: Object.fromEntries(selection),
            report: {
              ...result,
              videoDecode,
              videoConversion: exportConversion,
              decodeFallbacks: pixels.decodeFallbacks,
            },
          };
        } finally {
          await audio.close();
          await renderer.dispose();
          await pixels.dispose();
        }
      },
    };
    const job = new ExportJob(revision, snapshot, backend, () => {
      held.forEach((l) => l.release());
      jobs.delete(job);
    });
    jobs.add(job);
    exportSettled = job.result.then(
      () => {},
      () => {},
    );
    // A proxy being made yields (encoder, GPU, memory) and is made again once the export ends.
    proxyRun?.preempt.abort(
      new DOMException("An export takes the proxy's place", "AbortError"),
    );
    return job;
  }
  /**
   * What an export of `snapshot` reads for each source, as the engine selects it: originals, unless
   * `options.media` asks for proxies. Refuses before anything is held when the media is not ready.
   */
  function exportSelection(snapshot, project, options) {
    if (!decisionBinding)
      throw new EditorError(
        "conform_pending",
        "Check the media with conform() before exporting",
        "export",
      );
    const report = conformProject(
      decisionBinding,
      project,
      catalogOf(snapshot),
      options.media === "proxies" ? "export_proxy" : "export_original",
    );
    if (!report.ready)
      throw new ConformError(report, {
        nameOf: (id) => snapshot.assets[id]?.name,
      });
    return new Map(
      report.sources.map((source) => [source.source_id, source.selection]),
    );
  }
  let disposal = null;
  function dispose() {
    if (disposal) return disposal;
    disposed = true;
    lifetime.abort();
    disposal = (async () => {
      pause();
      off();
      if (refreshFrame)
        (typeof cancelAnimationFrame === "function"
          ? cancelAnimationFrame
          : clearTimeout)(refreshFrame);
      ++binding;
      for (const controller of pendingImports) controller.abort();
      for (const controller of analyses.values()) controller.abort();
      globalThis.document?.removeEventListener("visibilitychange", visibility);
      await Promise.all([...jobs].map((job) => job.cancel()));
      await queue;
      await mixer?.close();
      await supplier?.close();
      await workerPreview?.dispose();
      workerPreview = null;
      await engine?.dispose();
      await sources?.dispose();
      owners.forEach((owner) => owner.lease.release());
      owners.clear();
      proxies.forEach((proxy) => proxy.lease.release());
      proxies.clear();
      registry.dispose();
      engine = null;
      measurer?.free();
      measurer = null;
      detachOwner();
    })();
    return disposal;
  }
  const detachOwner = editor.own({ dispose });
  return {
    get capabilities() {
      return capabilities;
    },
    bindPreview,
    importFiles,
    relink: (id, file, options) =>
      importFiles([file], { ...options, relinkId: id }),
    importSequence,
    relinkSequence,
    /**
     * Shows another animation of a package source (a dotLottie's), keeping its clips: an undoable
     * document change.
     */
    selectAnimation(id, animation) {
      const descriptor = editor.document.assets[id];
      const source = editor.document.project.sources.find(
        (candidate) => candidate.id === id,
      );
      if (!descriptor || source?.provider?.format !== "lottie")
        throw new EditorError(
          "unsupported_media",
          "Only a Lottie source has animations to choose",
          "provider",
          id,
        );
      const items =
        ownerOf(editor.document, descriptor)?.lease.value.report?.items ?? [];
      if (!items.includes(animation))
        throw new EditorError(
          "unsupported_media",
          `The package has no animation ${animation}`,
          "provider",
          id,
        );
      const provider = { format: "lottie", animation };
      const result = editor.dispatch({
        type: "asset.put",
        asset: descriptor,
        source: { ...source, provider },
      });
      if (!result.ok) throw result.error;
      notifyAssets();
    },
    setLinearPrimaries(id, primaries) {
      const descriptor = editor.document.assets[id];
      const source = editor.document.project.sources.find(
        (candidate) => candidate.id === id,
      );
      const runtime = descriptor
        ? ownerOf(editor.document, descriptor)?.lease.value
        : undefined;
      if (!descriptor || !source?.provider || !runtime?.report?.color)
        throw new EditorError(
          "unsupported_media",
          "Only an OpenEXR source has primaries to choose",
          "provider",
          id,
        );
      const { primaries: _previous, ...rest } = source.provider;
      const provider = primaries === null ? rest : { ...rest, primaries };
      const result = editor.dispatch({
        type: "asset.put",
        asset: descriptor,
        source: { ...source, provider },
      });
      if (!result.ok) throw result.error;
      notifyAssets();
    },
    seek,
    play,
    pause,
    startExport,
    async importCaptions(file) {
      const generation = editor.generation;
      // A larger file holds more cues than a project can, so it is not read at all.
      if (file.size > MAX_CAPTION_FILE_BYTES)
        throw new EditorError(
          "invalid_captions",
          `${file.name} is ${file.size} bytes; a subtitle file is read up to ${MAX_CAPTION_FILE_BYTES}`,
          "import",
        );
      const { parseCaptions } = await loadWasm();
      const text = await file.text();
      let imported;
      try {
        imported = JSON.parse(
          parseCaptions(text, /\.vtt$/i.test(file.name) ? "webvtt" : "srt"),
        );
      } catch (error) {
        throw new EditorError(
          "invalid_captions",
          `${file.name}: ${error instanceof Error ? error.message : String(error)}`,
          "import",
          undefined,
          true,
          { cause: error },
        );
      }
      if (disposed || editor.generation !== generation)
        throw Error("Project changed during caption import");
      const assetId = crypto.randomUUID();
      const result = editor.dispatch({
        type: "asset.put",
        asset: captionsAsset(assetId, file.name, imported.rows),
      });
      if (!result.ok) throw result.error;
      return { assetId, skipped: imported.skipped };
    },
    async captionFontFamilies() {
      const { fontFamily } = await loadWasm();
      const families = [];
      for (const font of [
        ...Object.values(editor.document.assets)
          .filter((a) => a.kind === "font")
          .map((a) => library.get(a.id))
          .filter(Boolean),
        ...(runtimeAssets.fonts ?? []),
      ]) {
        const family = fontFamily(font.bytes, font.faceIndex ?? 0);
        if (family && !families.includes(family)) families.push(family);
      }
      return families;
    },
    async measureText(generator) {
      const { width, height } = await draftMeasurer();
      return JSON.parse(
        measurer.measure(JSON.stringify(generator), width, height),
      );
    },
    async measureLayout(generator, time = 0) {
      const { width, height } = await draftMeasurer();
      // Measured at the canvas size, which its pixel lengths are pixels of.
      return JSON.parse(
        measurer.measureLayout(
          JSON.stringify(generator),
          width,
          height,
          time,
          width,
          height,
        ),
      );
    },
    conform,
    /**
     * Makes a proxy of a source whose original this session holds, keeps it, and previews from it
     * wherever the original goes offline, or while preview prefers proxies.
     */
    async generateProxy(id, { signal } = {}) {
      const descriptor = editor.document.assets[id];
      // No single video file for the proxy maker to read.
      const provider = editor.document.project.sources.find(
        (source) => source.id === id,
      )?.provider;
      if (provider)
        throw new EditorError(
          "unsupported_media",
          "Proxies are made from video files; this source has none",
          "proxy",
          id,
        );
      const runtime = originalOf(editor.document, id);
      const hash = hashOf(descriptor);
      if (!runtime?.file || !hash)
        throw new EditorError(
          "missing_asset",
          "Relink the original to make its proxy",
          "proxy",
          id,
        );
      if (proxyJobs.has(id))
        throw new EditorError(
          "proxy_busy",
          "A proxy of this source is being made",
          "proxy",
          id,
        );
      proxyJobs.set(id, 0);
      notifyAssets();
      let hold = null;
      try {
        for (;;) {
          // One proxy at a time, none while an export runs; an export that starts takes its place.
          const turn = await proxyTurn(signal);
          try {
            // Held until its runtime is let go, so no page removes it meanwhile.
            hold = await proxyStore.hold(hash);
            // Listeners hear each whole percent once.
            let percent = 0;
            const found = await proxyStore.generate(
              runtime.file,
              descriptor.fingerprint,
              {
                signal: signal
                  ? AbortSignal.any([signal, turn.signal])
                  : turn.signal,
                onProgress: (fraction) => {
                  proxyJobs.set(id, fraction);
                  const next = Math.floor(fraction * 100);
                  if (next === percent) return;
                  percent = next;
                  notifyAssets();
                },
              },
            );
            const adopted = adoptProxy(hash, found, hold);
            hold = null;
            await adopted;
            break;
          } catch (error) {
            if (!turn.signal.aborted || signal?.aborted) throw error;
            void hold?.release();
            hold = null;
            proxyJobs.set(id, 0);
            notifyAssets();
          } finally {
            turn.end();
          }
        }
      } finally {
        void hold?.release();
        proxyJobs.delete(id);
        notifyAssets();
      }
      // The next load asks the engine again. Playback is left alone: every source it reads is online.
      if (engine && !playing) refreshPreview();
    },
    /**
     * Shows node `nodeId` of composition `compositionId` in the viewer instead of the composition's
     * output, rendered by the same engine compiler; `null` shows the outputs again. Preview only:
     * the document and export are unchanged.
     */
    setPreviewOutput(target) {
      const next = target
        ? { compositionId: target.compositionId, nodeId: target.nodeId }
        : null;
      if (JSON.stringify(next) === JSON.stringify(previewOutput)) return;
      previewOutput = next;
      renderKey = null;
      if (engine) void seek(tick).catch(onError);
    },
    get previewOutput() {
      return previewOutput;
    },
    /**
     * Makes preview read a fresh proxy wherever a source has one (`proxies`), or the original
     * wherever it is online (`original`, the default). Preview only: an export reads what its own
     * `media` says.
     */
    setPreviewMedia(media) {
      if (media !== "original" && media !== "proxies")
        throw Error("Preview media must be original or proxies");
      if (media === preferredPreviewMedia) return;
      preferredPreviewMedia = media;
      renderKey = null;
      if (engine) void seek(tick).catch(onError);
    },
    get previewMedia() {
      return preferredPreviewMedia;
    },
    hasOriginal: (id) => !!originalOf(editor.document, id),
    /**
     * Which video codecs this browser encodes for an export of the current document at
     * `videoQuality`: the question an export asks before it opens its encoder.
     */
    async videoEncoders({ videoQuality } = {}) {
      await decisions();
      const { probeEncoders } = await import("./runtime/exporter.js");
      const { width, height, frame_rate } = editor.document.project.settings;
      return probeEncoders(width, height, {
        frameRate: frame_rate,
        videoQuality,
      });
    },
    /** Whether a source has a kept proxy, and the progress of one being made. */
    proxyStatus(id) {
      const descriptor = editor.document.assets[id];
      const hash = hashOf(descriptor);
      return {
        available: !!hash && proxies.has(hash),
        generating: proxyJobs.get(id) ?? null,
        reading: previewSelection.get(id)?.media ?? "original",
      };
    },
    /**
     * The proxies this browser keeps, for every project: each original's hash and the bytes its
     * proxy takes.
     */
    listProxies: () => proxyStore.list(),
    /**
     * Removes the kept proxies of `hashes`. This session lets go of its own first and refreshes the
     * preview; a proxy still read after that (by the preview, for a source whose original is
     * offline, by an export of this session, or by another page of the origin) is left and listed
     * in `inUse`.
     */
    async removeProxies(hashes) {
      const wanted = new Set(hashes);
      const unlocked = [];
      for (const [hash, proxy] of proxies)
        if (wanted.has(hash)) {
          proxies.delete(hash);
          proxy.lease.release();
          if (proxy.unlocked) unlocked.push(proxy.unlocked);
        }
      await Promise.all(unlocked);
      notifyAssets();
      renderKey = null;
      if (engine) await seek(tick).catch(onError);
      const result = await proxyStore.remove([...wanted]);
      return result;
    },
    async nativeAssets(snapshot = editor.document, options = {}) {
      const compiled = compileEditorDocument(snapshot);
      if (compiled.diagnostics.length) throw compiled.diagnostics[0];
      await discoverProxies(snapshot);
      await decisions();
      // Leased after the waits and before the next, so no release can come between.
      materializeText(snapshot);
      requireShownText(snapshot, compiled.project);
      const selection = exportSelection(snapshot, compiled.project, options);
      const used = shownSources(compiled.project);
      for (const resource of compiled.project.effect_resources ?? [])
        used.add(resource.id);
      const held = [];
      try {
        const result = [];
        const fonts = new Set(heldFonts(snapshot));
        for (const asset of Object.values(snapshot.assets)) {
          if (
            drawnByEngine(asset) ||
            (asset.kind === "font" ? !fonts.has(asset) : !used.has(asset.id))
          )
            continue;
          const proxied = selection.get(asset.id)?.media === "proxy";
          const lease = proxied
            ? acquireProxy(hashOf(asset), asset.id)
            : acquireOwned(snapshot, asset);
          held.push(lease);
          const runtime = lease.value;
          // An image sequence is its frames' files, each uploaded under its locator.
          if (!proxied && runtime.frameFiles) {
            for (const [locator, file] of runtime.frameFiles)
              result.push({
                id: `${asset.id}:${result.length}`,
                kind: "frames",
                blob: file,
                name: file.name,
                locator,
              });
            continue;
          }
          if (proxied) {
            result.push({
              id: asset.id,
              kind: "proxies",
              blob: runtime.file,
              name: `${asset.id}.proxy.mp4`,
              madeFrom: proxies.get(hashOf(asset)).record.made_from,
            });
            continue;
          }
          let blob = runtime.file;
          if (asset.kind === "font") blob = new Blob([runtime.bytes]);
          if (asset.kind === "lut3d") blob = new Blob([runtime.text]);
          if (asset.kind === "text") {
            const raster = runtime.raster;
            const rgba = new Uint8ClampedArray(raster.rgba8);
            for (let i = 0; i < rgba.length; i += 4)
              if (rgba[i + 3]) {
                const factor = 255 / rgba[i + 3];
                rgba[i] *= factor;
                rgba[i + 1] *= factor;
                rgba[i + 2] *= factor;
              }
            const canvas = new OffscreenCanvas(
              snapshot.project.settings.width,
              snapshot.project.settings.height,
            );
            canvas
              .getContext("2d")
              .putImageData(
                new ImageData(
                  rgba,
                  raster.texture.width,
                  raster.texture.height,
                ),
                Math.round((canvas.width - raster.texture.width) / 2),
                Math.round((canvas.height - raster.texture.height) / 2),
              );
            blob = await canvas.convertToBlob({ type: "image/png" });
          }
          if (!blob) throw Error(`Missing native bytes: ${asset.name}`);
          result.push({
            id: asset.id,
            kind:
              asset.kind === "font"
                ? "caption-fonts"
                : asset.kind === "lut3d"
                  ? "effect-assets"
                  : "assets",
            blob,
            name: asset.kind === "text" ? "text.png" : asset.name,
          });
        }
        result.push(
          ...(runtimeAssets.fonts ?? []).map((font) => ({
            id: font.id,
            kind: "caption-fonts",
            blob: new Blob([font.bytes]),
            name: `${font.name}.ttf`,
          })),
        );
        return result;
      } finally {
        held.forEach((lease) => lease.release());
      }
    },
    dispose,
    setMonitorVolume(value) {
      if (!Number.isFinite(value) || value < 0 || value > 1)
        throw Error("Monitor volume must be between 0 and 1");
      // The bridge's, not one mixer's: each preview binding's mixer starts at it.
      monitorVolume = value;
      mixer?.setMasterVolume(value);
    },
    get monitorVolume() {
      return monitorVolume;
    },
    setTraceUploads(value) {
      traceUploads = value === true;
      applyTracing();
    },
    setPassTiming(enabled) {
      passTiming = enabled === true;
      applyTracing();
    },
    get previewScale() {
      return previewScale;
    },
    setPreviewScale,
    get diagnostics() {
      // The renderer that draws: in worker mode the main-thread one does not.
      const snapshot = workerPreview
        ? workerDiagnostics
        : (engine?.lastDiagnostics ?? null);
      if (!snapshot) return null;
      return {
        ...snapshot,
        host: {
          ...snapshot.host,
          skippedRequests,
          bitmapTransfers,
          previewScale,
        },
      };
    },
    get tick() {
      return tick;
    },
    get playing() {
      return playing;
    },
    get previewBackend() {
      return !engine ? "unbound" : workerPreview ? "worker" : "main-thread";
    },
    get previewRecoveries() {
      return previewRecoveries;
    },
    get resourceCount() {
      return registry.size;
    },
    subscribeAssets(listener) {
      assetListeners.add(listener);
      return () => assetListeners.delete(listener);
    },
    get previewIssue() {
      return previewIssue;
    },
    subscribePreview(listener) {
      previewListeners.add(listener);
      return () => previewListeners.delete(listener);
    },
    get catalog() {
      return engine?.catalog ?? null;
    },
    registerEffects,
    // A source's own media while it is online, whichever preview reads: its size, file and peaks
    // are the original's.
    getAsset: (id) =>
      (preferredPreviewMedia === "proxies" &&
        sourceView(editor.draft, id, originalOf(editor.draft, id))) ||
      library.get(id),
  };
}

/**
 * `scale` as a preview resolution scale: finite, above 0 and at most 8, as the engine's quality
 * profile takes it.
 */
function checkedPreviewScale(scale) {
  if (typeof scale !== "number" || !(scale > 0 && scale <= 8))
    throw new EditorError(
      "invalid_preview_scale",
      "The preview scale must be a number above 0 and at most 8",
      "preview",
    );
  return scale;
}

/** `value` as who converts decoded SDR frames: `engine` or `browser`. */
function checkedVideoConversion(value, operation) {
  if (value !== "engine" && value !== "browser")
    throw new EditorError(
      "invalid_video_conversion",
      `videoConversion must be "engine" or "browser"; received ${JSON.stringify(value)}`,
      operation,
    );
  return value;
}

/**
 * Whether the engine draws a source itself, so no file backs it: a generated source or a
 * composition.
 */
function drawnByEngine(source) {
  return source.kind === "generated" || source.kind === "composition";
}

/** Waits for `promise`, or rejects when `signal` aborts first. */
function settledOrAborted(promise, signal) {
  if (!signal) return promise;
  return new Promise((resolve, reject) => {
    const abort = () =>
      reject(signal.reason ?? new DOMException("Aborted", "AbortError"));
    if (signal.aborted) return abort();
    signal.addEventListener("abort", abort, { once: true });
    promise.then(
      () => {
        signal.removeEventListener("abort", abort);
        resolve();
      },
      (error) => {
        signal.removeEventListener("abort", abort);
        reject(error);
      },
    );
  });
}

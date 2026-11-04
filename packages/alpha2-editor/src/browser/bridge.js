import { frameAt, frameStart } from "../core/time.js";
import {
  compileEditorDocument,
  EditorError,
  sequenceEnd,
} from "../core/document.js";
import { captionsAsset } from "../core/captions.js";
import { MAX_CAPTION_FILE_BYTES } from "alpha2/project";
import { containerPolicy } from "alpha2/containers";
import { PlanarDecodeUnavailableError } from "alpha2/hdr-frames";
import { drawnEffectResources, shownSources } from "../core/compositions.js";
import { ResourceRegistry } from "./resources.js";
import { ExportJob, requireExportFrameRate } from "./jobs.js";
import {
  GpuRecoveryLadder,
  lossReportFromJson,
  pageGpuLosses,
} from "alpha2/gpu-recovery";
import { attemptOptions } from "./runtime/budget.js";
import { readUserText } from "./runtime/text.js";
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
 * An export refused before it rendered, with the engine's conform report. The message names sources
 * by `nameOf` (the document's asset names); the report and `entityId` use IDs.
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

const PLAYBACK_POLICIES = Object.freeze([
  "drop_video_keep_audio",
  "preserve_every_video_frame",
  "silent_scrub",
  "offline_exact",
]);

/**
 * Files of one import staged at once. Each hashes in its own worker at about 200 MB/s, so four stay
 * below a local SSD's read rate and leave one core for the page.
 */
const IMPORT_STAGE_CONCURRENCY = 4;
/** `previewIssue` while the frame shows an HDR source this browser hands over only degraded. */
const DEGRADED_PREVIEW_ISSUE =
  "HDR shown with the browser's tone mapping; the browser cannot export it: export with the native host";

/**
 * How long Play waits for sound before playing the picture without it. A first start loads the
 * audio worklet and decodes the first block, about 1.5 s on Firefox.
 */
const AUDIO_START_TIMEOUT_MS = 3000;

/**
 * The provider modules' `.wasm` URLs by module name, as strings a worker can be sent. The core
 * module's comes from `extensionsWasmUrl`, any other's from `extensionModuleUrls`.
 * @param {{ extensionsWasmUrl?: string | URL, extensionModuleUrls?: import("alpha2/extensions").ExtensionWasmUrls } | undefined} assets
 * @returns {Record<string, string>}
 */
function extensionWasmUrls(assets) {
  const urls = { ...assets?.extensionModuleUrls };
  if (assets?.extensionsWasmUrl) urls.core = assets.extensionsWasmUrl;
  return Object.fromEntries(
    Object.entries(urls)
      .filter(([, url]) => url)
      .map(([name, url]) => [name, String(url)]),
  );
}

/**
 * The default media adapter. Each file stages in its own `AssetLibrary`, apart from the bridge's
 * live one, so a failed or cancelled import leaves nothing behind. Extension provider files (SVG,
 * animated images, Lottie, OpenEXR) are probed and thumbnailed by `alpha2/extensions` in a worker
 * per file from `extensionsWorkerFactory` (terminated on cancel or past
 * `EXTENSIONS_CALL_TIMEOUT_MS`), or on this thread without one. Fonts are checked with the engine's
 * parser and also registered with the page's `FontFace` set. Content hashes run in a
 * `hashWorkerFactory` worker when given. `hdrWithoutPlanes` says whether HDR video without planes
 * imports degraded or is refused.
 * @param {{ extensionsWasmUrl?: string | URL, extensionModuleUrls?: import("alpha2/extensions").ExtensionWasmUrls, wasmUrl?: string | URL, hashWorkerFactory?: () => Worker, extensionsWorkerFactory?: () => Worker, hdrWithoutPlanes?: "refuse" | "preview-degraded" }} [options]
 * @returns {import("./types.js").BrowserMediaAdapter}
 */
export function createMediaAdapter({
  extensionsWasmUrl,
  extensionModuleUrls,
  wasmUrl,
  hashWorkerFactory,
  extensionsWorkerFactory,
  hdrWithoutPlanes = "preview-degraded",
} = {}) {
  const hash = hashWorkerFactory
    ? (file, options) => hashInWorker(hashWorkerFactory, file, options)
    : sha256Blob;
  const module = {
    factory: extensionsWorkerFactory,
    wasmUrl: extensionWasmUrls({ extensionsWasmUrl, extensionModuleUrls }),
  };
  /**
   * Runs provider `call` on `file`. A refusal becomes `unreadable_media` with its reason;
   * cancellation and stalls pass through unchanged.
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
      // Start hashing now so it overlaps probing.
      const hashing = new AbortController();
      const stop = () => hashing.abort(signal.reason);
      signal.addEventListener("abort", stop, { once: true });
      const started = wanted ? hash(file, { signal: hashing.signal }) : null;
      started?.catch(() => {}); // awaited below when used; a failed stage aborts it
      /** Skipped when the caller does not need it (a proxy being adopted). */
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
  /** Stages `file` as the media its bytes or name indicate. */
  async function stageFile(file, signal, fingerprintOf) {
    // Sniff the bytes first; the provider module loads only for bytes that could match.
    const head = new Uint8Array(await file.slice(0, 4096).arrayBuffer());
    if (mayBeExtension(head)) {
      // Providers parse whole files into memory that never shrinks; refuse oversize files unread.
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
    const library = new AssetLibrary({ hdrWithoutPlanes });
    try {
      const runtime = file.name.toLowerCase().endsWith(".cube")
        ? await library.importEffectResource(file, "lut3d")
        : await library.importFile(file, { signal });
      signal.throwIfAborted();
      if (runtime.kind === "lut3d") {
        // Validate the LUT once here rather than at every load.
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
 * export. GPU devices, AudioContexts, workers and decoders are created lazily. The bridge registers
 * as an owner of `editor`, so disposing the session disposes it.
 * @param {import("./types.js").BrowserBridgeOptions} options
 * @returns {import("./types.js").BrowserBridge}
 */
export function createBrowserBridge(options) {
  const {
    editor,
    runtimeAssets = {},
    hdrWithoutPlanes = "preview-degraded",
    mediaAdapter = createMediaAdapter({ ...runtimeAssets, hdrWithoutPlanes }),
    resourceLimit = 128,
    audio = true,
    onError = () => {},
    assetDecisions,
    previewScale: initialPreviewScale = 0.5,
    videoConversion: initialVideoConversion = "engine",
  } = options;
  let previewScale = checkedPreviewScale(initialPreviewScale);
  if (hdrWithoutPlanes !== "refuse" && hdrWithoutPlanes !== "preview-degraded")
    throw new RangeError(
      'hdrWithoutPlanes must be "refuse" or "preview-degraded"',
    );
  // How preview frame supplies treat an HDR source imported degraded.
  const opaqueEngineFrames =
    hdrWithoutPlanes === "preview-degraded" ? "present-degraded" : "refuse";
  const videoConversion = checkedVideoConversion(
    initialVideoConversion,
    "bridge",
  );
  const rendererMode = options.renderer ?? "auto";
  if (
    rendererMode !== "auto" &&
    rendererMode !== "gpu" &&
    rendererMode !== "cpu"
  )
    throw new RangeError('renderer must be "auto", "gpu" or "cpu"');
  // Test-only stand-in for origin storage, kept out of the public options.
  const proxyStore =
    /** @type {{ proxyStore?: ProxyStore }} */ (options).proxyStore ??
    new ProxyStore();
  let decisionBinding = assetDecisions ?? null;
  /** The wasm module's authoring functions, loaded on first use; the preview engine's instance. */
  const loadWasm = async () => {
    const { loadEngineWasm } = await import("./runtime/wasm.js");
    return loadEngineWasm(runtimeAssets.wasmUrl);
  };
  /** The engine's asset decisions, from the wasm module, loaded on first use. */
  const decisions = async () => (decisionBinding ??= await loadWasm());
  /** Text measurer for `measureText` and `measureLayout`, made on first use, and its font stack. */
  let measurer = null,
    measuredFonts = null;
  const registry = new ResourceRegistry(resourceLimit);
  /**
   * Runtimes this session holds, by {@link ownerKey}: a lease and its registry revision. Media is
   * keyed by content revision; text by what it draws, since an edit after an undo may reuse an
   * undone edit's revision for other text.
   */
  const owners = new Map();
  /**
   * The highest registry revision issued per runtime ID. Never reused, so a runtime released while
   * an export still leases it cannot collide with its replacement.
   */
  const issued = new Map();
  const issue = (id, atLeast = 0) => {
    const revision = Math.max(atLeast, (issued.get(id) ?? -1) + 1);
    issued.set(id, revision);
    return revision;
  };
  /** Proxies by their original's hash: kept record, runtime lease and registry revision. */
  const proxies = new Map();
  /** Progress (0 to 1) of proxies being made, by source ID. */
  const proxyJobs = new Map();
  let previewSelection = new Map();
  /** The key `previewSelection` was decided from (see `loaded`). */
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
   * Each source's media as the engine should see it: the original is online when this session holds
   * its file, and a kept proxy is offered when made from the media the project recorded.
   */
  const catalogOf = (document) => ({
    assets: document.project.sources
      .filter((source) => !drawnByEngine(source))
      .map((source) => {
        const descriptor = document.assets[source.id];
        const record = proxies.get(hashOf(descriptor))?.record;
        const revision = descriptor?.contentRevision ?? 0;
        const original = originalOf(document, source.id);
        return {
          source_id: source.id,
          revision,
          original: original
            ? { state: "online", fingerprint: descriptor?.fingerprint ?? {} }
            : { state: "offline" },
          ...(original?.audioStreams
            ? { audio_streams: original.audioStreams }
            : {}),
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
   * Adopts a kept proxy, staging its file like any import under its own ID. `hold` (the store's)
   * keeps the proxy in use until the runtime is disposed.
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
        // Proxies are keyed by their original's hash, so skip hashing their own bytes.
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
  /** Hashes with no stored proxy, not asked again this session. */
  let absentProxies = new Set();
  /** Finds kept proxies for offline sources, and for every source while preview prefers proxies. */
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
      // Hold before reading so no other page removes it in between.
      const hold = await proxyStore.hold(hash);
      const found = await proxyStore.find(hash).catch(() => null);
      if (found) await adoptProxy(hash, found, hold);
      else {
        absentProxies.add(hash);
        void hold.release();
      }
    }
  }
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
  /** Running waveform analyses by runtime, aborted when the runtime is released. */
  const analyses = new Map();
  /** Whether this session still holds `runtime` for some document it can show. */
  const held = (runtime) =>
    [...owners.values()].some((owner) => owner.lease.value === runtime);
  /**
   * Derives waveform peaks after import, one file at a time, so imports commit as soon as media is
   * readable. The analysis belongs to the runtime, not the importing document, and is dropped only
   * when the runtime is released.
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
    // Set when the worker failed for a reason other than GPU loss; later previews for this binding
    // stay on the main thread.
    workerGivenUp = false,
    sources = null,
    mixer = null,
    supplier = null,
    binding = 0,
    queue = Promise.resolve(),
    renderKey = null,
    tick = 0,
    playing = false,
    // How playback was requested, so a recovered preview resumes it the same way.
    playRequest = null,
    renderScheduled = false,
    raf = 0,
    playClock = null,
    loadRevision = 0,
    loadedAssets = new Map(),
    loadedSelection = "",
    loadedFonts = null,
    audioKey = null,
    workerSent = new Map(),
    workerFonts = null,
    refreshFrame = 0,
    monitorVolume = 1,
    // User A/V offset for output delay the browser does not report.
    audioOffsetMs = 0,
    // Tracing settings, applied to every renderer a binding makes.
    passTiming = false,
    traceUploads = false,
    // The worker's last renderer diagnostics, plus counters since `bindPreview`: refreshes that
    // needed no frame, and frames copied to the bound canvas.
    workerDiagnostics = null,
    skippedRequests = 0,
    bitmapTransfers = 0,
    playGeneration = 0,
    sessionGeneration = editor.generation;
  /**
   * Preview's media for a source: what the engine selected (the original unless offline), opened
   * with the document's provider settings.
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
  /** Font families a text style draws with: imported fonts by their registered family. */
  function familiesOf(document, text) {
    return (text?.fontFamily ?? "sans-serif").split(",").map((name) => {
      const plain = name.trim().replace(/^["']|["']$/g, "");
      const font = Object.values(document.assets).find(
        (a) => a.kind === "font" && a.name.replace(/\.[^.]+$/, "") === plain,
      );
      return (font && ownerOf(document, font)?.lease.value.family) ?? plain;
    });
  }
  /** Which runtime `asset` reads: media by content revision, text by authoring and fonts. */
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
  /** A lease on the runtime `asset` reads, for work that outlives the owner's hold. */
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
  /** Runtime views by runtime, then provider settings, so each choice maps to one value. */
  const views = new WeakMap();
  /**
   * `runtime` opened with document `id`'s provider settings (a package's animation, an OpenEXR
   * still's primaries), so undo and redo change what is drawn.
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
   * Runs `fn` after queued preview work. Its failure goes only to the caller, which reports it to
   * whoever awaits it, or to `onError` for work the bridge started itself.
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
  // Preview needs the engine module and canvases, not a GPU: without one it renders on the CPU.
  const features = Object.freeze({
    preview: {
      available:
        typeof WebAssembly !== "undefined" &&
        typeof OffscreenCanvas !== "undefined" &&
        typeof VideoFrame !== "undefined",
      reason:
        "Preview needs WebAssembly, OffscreenCanvas and WebCodecs; it renders on the CPU where the browser offers no hardware GPU",
    },
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
      available:
        typeof WebAssembly !== "undefined" &&
        typeof OffscreenCanvas !== "undefined" &&
        typeof VideoEncoder !== "undefined",
      reason:
        "Codec availability is checked at export, which renders on the CPU where the browser offers no hardware GPU; output streams to a temporary origin-private file unless a file or memory output is chosen",
    },
  });
  /** The page's capability report; GPU and color are known once a preview is bound. */
  let capabilities = capabilityReport(null);
  function capabilityReport(rendered) {
    return Object.freeze({
      schema: "alpha2-capabilities",
      version: 2,
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
      adapters: null,
      renderer: rendered?.renderer ?? null,
    });
  }
  /** Raster failures by {@link ownerKey}; not retried until authoring or fonts change. */
  const textFailures = new Map();
  /** The error a render fails with when it shows text `asset` that could not be rasterized. */
  function undrawnText(document, asset) {
    const failure = textFailures.get(ownerKey(document, asset));
    return new EditorError(
      "text_unrenderable",
      `Text "${asset.name}" cannot be drawn: ${failure?.message ?? "it was not rasterized"}`,
      "resource",
      asset.id,
    );
  }
  /** Throws {@link undrawnText} when compiled `project` shows text that could not be drawn. */
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
   * Rasterizes each text of `document` that no held runtime draws yet (new authoring, or a font
   * that now resolves to another family). A text that cannot be drawn (too large for a texture,
   * say) is recorded, not thrown, and fails only renders that show it.
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
  // Node shown instead of a composition's output; preview only.
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
   * Reports each font of a registered stack the engine could not parse. It was left out, and
   * captions and SVG text fall back to the fonts after it.
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
   * The document's font assets this session holds a file for. Unlinked fonts are left out of the
   * stack by preview and exports alike, never refused.
   */
  function heldFonts(document) {
    return Object.values(document.assets).filter(
      (a) => a.kind === "font" && !a.missing && ownerOf(document, a),
    );
  }
  /** Loads `measurer` with preview's fonts for the draft and returns the draft's canvas size. */
  async function draftMeasurer() {
    const { WasmTextMeasurer } = await loadWasm();
    if (disposed)
      throw new EditorError("disposed", "Bridge is disposed", "browser");
    const document = editor.draft;
    const fonts = fontStackOf(document);
    measurer ??= new WasmTextMeasurer();
    if (!sameList(measuredFonts, fonts)) {
      // Refused fonts are left out here as preview does, and reported there.
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
    // Configure audio once the edit settles; sound only plays after a gesture anyway.
    const settled = document === editor.document;
    if (renderKey !== key || loadedSelection !== selectionKey) {
      mixer.stop();
      engine.invalidate();
      // Keep decoders warm by reopening only sources whose runtime changed.
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
      if (onCpu()) reportCpuEffects(engine.projectRuntimeEffects());
      // A worker given up for one project is tried again for the next.
      if (renderKey !== null && !workerPreview) workerGivenUp = false;
      // Frames and surfaces are the document size times the preview scale, rounded by the engine.
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
        // The worker keeps assets it already holds; only new or changed ones are sent.
        const keep = used
          .filter(([id, asset]) => workerSent.get(id) === asset)
          .map(([id]) => id);
        const kept = new Set(keep);
        const workerAssets = used
          .filter(([id]) => !kept.has(id))
          // Keyed by the source it serves, since a proxy's runtime has its own ID.
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
        // Unknown until the worker confirms, so a failed load resends everything.
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
    // Drags change the draft on every pointer move; follow at most once a frame.
    if (engine && !refreshFrame) refreshFrame = nextFrame(refresh);
  });
  const reportedFonts = new WeakMap();
  /** Aborted on dispose, stopping work started for no caller (text reports). */
  const lifetime = new AbortController();
  let textReports = Promise.resolve(),
    textReportsQueued = false;
  /**
   * An SVG's report lists text no font draws (`missing_font`). Import probes with no fonts, so the
   * report is recomputed whenever the SVG or its font stack changes, and `subscribeAssets` hears of
   * changes.
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
            wasmUrl: extensionWasmUrls(runtimeAssets),
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
  /** Releases, after queued preview work, what no reachable document reads. */
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
   * Media and proxies stay held while any document the session can return to reads them, so undone
   * imports, relinks and removals come back with their runtime. Text is held only while the
   * document or draft draws it, since authoring recreates it. Leased runtimes (exports, native
   * uploads) stay until released.
   */
  function sweep() {
    const media = new Set(),
      hashes = new Set(),
      seen = new Set();
    for (const document of editor.reachableDocuments()) {
      // Commands that leave assets alone share them, so history is read once per media change.
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
    // Playback reads the project it started with, so an edit restarts it from the same point, rate,
    // loop and policy, unless a seek, pause or Play comes first.
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
  /** Sources the last frame showed through the browser's own tone mapping. */
  let degradedShown = [];
  /** Rebuilding after GPU loss failed; the next render retries. */
  let recoveryPending = false;
  /** The recovery rung to try next while `recoveryPending`. */
  let pendingStep = null;
  let previewLadder = new GpuRecoveryLadder();
  /** All recovery rungs failed; nothing draws until the preview is bound again. */
  let previewUnavailable = false;
  let previewRecoveries = 0;
  const previewListeners = new Set();
  const setPreviewIssue = (issue) => {
    if (issue === previewIssue) return;
    previewIssue = issue;
    for (const listener of previewListeners) listener();
  };
  /**
   * Renders the frame at `at`. A composition that cannot draw its output yet keeps the previous
   * frame up with `previewIssue` set, rather than failing.
   */
  async function renderAt(at, project, recovered = false) {
    try {
      const rendered = await renderFrame(at, project, recovered);
      setPreviewIssue(degradedShown.length > 0 ? DEGRADED_PREVIEW_ISSUE : null);
      return rendered;
    } catch (error) {
      if (error?.code === "INCOMPLETE_COMPOSITION") {
        setPreviewIssue(error.message ?? String(error));
        return false;
      }
      // A source the preview cannot draw fails every frame it is in; report it once with its own
      // code and keep the frame up.
      if (isSourceError(error)) {
        const issue = error.message ?? String(error);
        if (issue === previewIssue) return false;
        setPreviewIssue(issue);
        throw error;
      }
      const lost =
        error?.code === "GPU_DEVICE_LOST" ||
        error?.code === "GPU_DEVICE_HUNG" ||
        engine?.deviceLost === true;
      if (recovered || !lost || !boundCanvas || disposed) throw error;
      // The worker's report travels with its error; the main renderer reads its own.
      const report =
        error?.lossReport ??
        engine?.lossReport() ??
        lossReportFromJson(undefined, error?.message ?? String(error));
      // Every rebuild draws on a fresh internal canvas, so no rung needs the lost one.
      const step = previewLadder.onLoss(report, {
        kind: "none",
        webgpuAdapter: false,
      });
      if (!(await recoverPreview(step))) return false;
      return renderAt(at, await loaded(), true);
    }
  }
  async function renderFrame(at, project, recovered = false) {
    if (boundCanvas?.style)
      boundCanvas.style.visibility = hasContent(project) ? "visible" : "hidden";
    if (!hasContent(project)) return false;
    if (workerPreview) {
      try {
        const result = await workerPreview.seek(at);
        if (result.engineDiagnostics)
          workerDiagnostics = result.engineDiagnostics;
        degradedShown = result.degradedSources ?? [];
        // Unchanged frames come without a bitmap; the canvas still shows them.
        if (result.bitmap) {
          boundCanvas
            .getContext("bitmaprenderer")
            .transferFromImageBitmap(result.bitmap);
          bitmapTransfers += 1;
        }
        return result.rendered;
      } catch (error) {
        // Give up the worker only for its own faults; source errors would hit the main renderer
        // too. GPU loss goes to the recovery ladder, and a worker that loses it again on the
        // recovered frame is let go.
        if (!isWorkerFault(error) || (isGpuLoss(error) && !recovered))
          throw error;
        // The main renderer has the same project loaded and draws this frame instead.
        await abandonWorker(error);
      }
    }
    const {
      rendered,
      unchanged,
      degradedSources = [],
    } = await engine.render(at / 120000, {
      generation: engine.generation,
      context: context(project, library),
    });
    degradedShown = degradedSources;
    // Unchanged frames were not presented. Copy the frame instead of transferring the canvas:
    // Chrome on macOS crashes the page when an engine WebGL2 canvas gives up its buffer with
    // `transferToImageBitmap`.
    if (rendered && !unchanged && copySurface) {
      boundCanvas
        .getContext("bitmaprenderer")
        .transferFromImageBitmap(await createImageBitmap(engine.canvas));
      bitmapTransfers += 1;
    }
    return rendered;
  }
  /** Resizes the bound canvas to the render size if needed. */
  function sizeBoundCanvas(size) {
    if (!boundCanvas || !size) return;
    if (boundCanvas.width !== size[0]) boundCanvas.width = size[0];
    if (boundCanvas.height !== size[1]) boundCanvas.height = size[1];
  }
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
   * Whether a preview worker draw `error` is the worker's fault: fatal (wasm trap, lost or hung
   * GPU), a codec its decoder refused (falls back to the main thread's media elements), or uncoded.
   * A source's own coded error is not.
   */
  function isWorkerFault(error) {
    const code = error?.code;
    return (
      error?.fatal === true ||
      typeof code !== "string" ||
      code === "worker_error" ||
      code === "unsupported_codec" ||
      code === "GPU_DEVICE_LOST" ||
      code === "GPU_DEVICE_HUNG"
    );
  }
  /** Whether `error` says the GPU was lost or hung on its work. */
  function isGpuLoss(error) {
    return (
      error?.code === "GPU_DEVICE_LOST" || error?.code === "GPU_DEVICE_HUNG"
    );
  }
  /** Whether `error` is a source's coded error, which a redraw would hit again. */
  function isSourceError(error) {
    return (
      !isWorkerFault(error) &&
      error?.code !== "INCOMPLETE_COMPOSITION" &&
      !String(error.code).startsWith("GPU_")
    );
  }
  /**
   * Gives up the preview worker after `error` (failed start, load or draw). Until the next binding
   * the main renderer, which loads every project the worker does, draws off screen and each frame
   * is copied to the bound surface. `onError` hears once why, unless the worker lost its GPU, which
   * recovers silently and lets a rebuilt preview try a worker again.
   */
  async function abandonWorker(error) {
    const failed = workerPreview;
    if (!failed) return;
    workerPreview = null;
    workerSent = new Map();
    workerFonts = null;
    copySurface = true;
    await failed.dispose().catch(() => {});
    if (isGpuLoss(error)) return;
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
    // Only a newer seek supersedes this; `loaded` reads the draft when the task runs.
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
  // (Firefox without usable output) or a stuck worklet would block Play, so video plays silent.
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
   * Plays from the current time at `rate` (negative plays backwards) under playback `policy`; the
   * engine's clock decides which frame shows when and whether sound plays. Sound that cannot start
   * in time is dropped rather than holding the picture.
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
    // Play the compiled projection preview shows, never a project the renderer loaded earlier.
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
    // The last requested tick and, for a policy that waits for frames, whether it was shown.
    let requested = null,
      presented = false;
    const step = (now) => {
      if (!playing || disposed || generation !== playGeneration) return;
      // At 1x under the default policy the audio clock leads while output consumes samples; the
      // engine falls back to the wall clock while it stands still.
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
      // The clock requests frame starts, so each frame is asked for once at any refresh rate. A
      // waiting policy moves on only after the frame is shown; the others drop late frames.
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
   * Makes the preview's renderer, audio and worker for `canvas`, disposing the previous ones. Runs
   * in the render queue. Returns false when a later binding or disposal superseded `token`.
   */
  async function buildPreview(canvas, token) {
    pause();
    await mixer?.close();
    await supplier?.close();
    await engine?.dispose();
    await sources?.dispose();
    sources = new SourcePixels(library, {
      extensionsWasmUrl: extensionWasmUrls(runtimeAssets),
    });
    const { Engine } = await import("./runtime/engine.js");
    // Until a project loads and sets the render size (see `loaded`).
    canvas.width = editor.document.project.settings.width;
    canvas.height = editor.document.project.settings.height;
    boundCanvas = canvas;
    copySurface = false;
    await workerPreview?.dispose();
    workerPreview = null;
    workerDiagnostics = null;
    // The main renderer draws on its own canvas and each frame is copied to the bound one, so a
    // renderer rebuilt after GPU loss (or the CPU renderer) needs nothing from the page.
    const copyable = typeof canvas.getContext === "function";
    const useWorker = features.worker.available && copyable;
    engine = new Engine(
      copyable ? new OffscreenCanvas(canvas.width, canvas.height) : canvas,
      sources,
      library,
      runtimeAssets,
      // In a preview, showing a frame at once matters more than the engine's conversion of it.
      {
        unreadableFrames: "present",
        videoConversion,
        opaqueEngineFrames,
        renderer: previewLadder.renderer === "cpu" ? "cpu" : rendererMode,
      },
    );
    mixer = new EngineAudioMixer(runtimeAssets, {
      enabled: audio,
      onInterrupted: (state) => {
        if (playing) pause();
        onError(
          new EditorError(
            "audio_unavailable",
            `Audio output was ${state} by the browser or the system; playback paused`,
            "play",
          ),
        );
      },
    });
    mixer.setMasterVolume(monitorVolume);
    mixer.setLatencyOffset(audioOffsetMs);
    stalledAudio = null;
    supplier = new AudioWindowSupplier(library);
    try {
      await engine.start();
      engine.setPreviewScale(previewScale);
      capabilities = capabilityReport(engine.capabilityReport());
      await registerKeptEffects();
      // The worker posts frames to the same bitmap renderer, so copying stays on either way.
      copySurface = copyable;
      if (useWorker && !workerGivenUp) {
        const workerRuntime = {
          ...(runtimeAssets.wasmUrl
            ? { wasmUrl: String(runtimeAssets.wasmUrl) }
            : {}),
          extensionsWasmUrl: extensionWasmUrls(runtimeAssets),
        };
        workerPreview = createWorkerPreview(runtimeAssets.workerFactory(), {
          runtimeAssets: workerRuntime,
          bitmapOutput: true,
          width: canvas.width,
          height: canvas.height,
          videoConversion,
          opaqueEngineFrames,
          // Match the main renderer: the CPU once recovery or `auto` moved there.
          renderer: onCpu() ? "cpu" : rendererMode,
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
   * Recovers a lost GPU (WebGPU device or WebGL context) silently: the preview is rebuilt on fresh
   * internal canvases with the renderer `step` names (the binding's `GpuRecoveryLadder` picks the
   * GPU again after a first external loss, the CPU after a self-caused loss or a second page loss),
   * and the frame is redrawn. A failed rung moves down the ladder; with none left the preview is
   * unavailable for this binding, reported once (`preview_unavailable`).
   *
   * Running playback resumes from the same point, rate, loop and policy once rebuilt, unless a
   * seek, pause or Play came first. Sound that missed its start deadline is not waited for again.
   */
  async function recoverPreview(step) {
    recoveryPending = true;
    pendingStep = step;
    const resume = playing ? playRequest : null;
    const stalled = stalledAudio;
    for (;;) {
      try {
        if (!(await buildPreview(boundCanvas, binding))) return false;
        break;
      } catch (error) {
        const next = previewLadder.onRecoveryFailed(pendingStep, error);
        if (next !== "terminal") {
          pendingStep = next;
          continue;
        }
        recoveryPending = false;
        previewUnavailable = true;
        onError(
          new EditorError(
            "preview_unavailable",
            `The GPU was lost and no renderer could be made again (${error instanceof Error ? error.message : String(error)}); bind the preview again to retry`,
            "preview",
            undefined,
            false,
            { cause: error },
          ),
        );
        return false;
      }
    }
    stalledAudio = stalled;
    recoveryPending = false;
    pendingStep = null;
    previewRecoveries += 1;
    if (resume) {
      // Queued behind the recovering render, which draws the frame first.
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
  /** Whether the main renderer draws on the CPU, where projects drawing runtime effects fail. */
  function onCpu() {
    return engine?.rendererChoice?.renderer === "cpu";
  }
  /** Drawn runtime effects already reported on the CPU renderer, by `id@version`. */
  const cpuRefusedEffects = new Set();
  function reportCpuEffects(keys) {
    for (const key of keys) {
      if (cpuRefusedEffects.has(key)) continue;
      cpuRefusedEffects.add(key);
      onError(
        new EditorError(
          "effect_unavailable",
          `Effect ${key} is not drawn: the preview renders on the CPU (${engine?.rendererChoice?.rendererReason ?? "no GPU"}), which runs no runtime effect`,
          "effects",
          key,
        ),
      );
    }
  }
  /**
   * Effects `registerEffects` kept, by `id@version`, with their JSON text in arrival order. Every
   * renderer the bridge makes registers them.
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
      // A frame that used one of them failed; redraw it.
      if (engine) refreshPreview();
    });
  }
  /**
   * Registers kept effects on the main renderer `buildPreview` just started, one at a time. One its
   * device refuses (kept unbound, so never compiled) is dropped and reported. A device lost
   * meanwhile drops none, since the next render rebuilds and re-registers.
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
  /** Whether a renderer is ready, rebuilding it first if GPU loss left none. */
  async function previewReady() {
    if (engine) return true;
    if (previewUnavailable || !recoveryPending || !boundCanvas || disposed)
      return false;
    return recoverPreview(pendingStep);
  }
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
      pendingStep = null;
      previewLadder = new GpuRecoveryLadder();
      previewUnavailable = false;
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
  function importSequence(
    files,
    { frameRate, missing = "reject", signal } = {},
  ) {
    const rate = frameRate ?? editor.document.project.settings.frame_rate;
    return importUnits([sequenceUnit(files, { frameRate: rate, missing })], {
      signal,
    });
  }
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
   * in order as one undo step. A relink has exactly one unit.
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
      // Staged together, committed in order. The first failure stops the rest and is reported as
      // the import's error, not the cancellation the others then see.
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
        // Equal hashes match whatever else differs. Without hashes, any recorded part that differs
        // means other media.
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
        // Above every revision issued, since after an undo the next may match an undone relink's.
        const contentRevision = issue(id, (old?.contentRevision ?? -1) + 1);
        // A relink opens as the project chose, not as a fresh import would.
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
        // Call as a method, since a staged asset may be a class instance.
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
  /** The running proxy conversion: `preempt` stops it for an export, `done` settles at its end. */
  let proxyRun = null;
  /**
   * Waits until no export or other proxy runs (or `signal` aborts) and takes the turn. `signal`
   * aborts when an export preempts it, and `end()` gives the turn back.
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
  /**
   * Refuses an export up front when it shows a source imported for a degraded preview: the engine
   * would refuse its first frame, and browser tone mapping must not reach a file.
   */
  function refuseDegradedSources(document, project, used) {
    const degraded = project.sources
      .filter(
        (source) =>
          used.has(source.id) &&
          originalOf(document, source.id)?.planarDecode === "unavailable",
      )
      .map((source) => document.assets[source.id]?.name ?? source.id);
    if (degraded.length === 0) return;
    const facts = { kind: "video", codec: null, softwareDecoder: "none" };
    const names = degraded.join(", ");
    throw new EditorError(
      "unsupported_media",
      `This browser cannot export ${names}: it decodes this HDR video only to frames the engine cannot read, so the preview shows it with the browser's tone mapping. Export with the native host.`,
      "export",
      undefined,
      true,
      {
        cause: new PlanarDecodeUnavailableError(
          `${names}: no planar decode in this browser`,
          facts,
        ),
      },
    );
  }
  let sweptExports = false;
  /** Settles when the running export does. */
  let exportSettled = Promise.resolve();
  function startExport(options = {}) {
    if (disposed)
      throw new EditorError("disposed", "Bridge is disposed", "export");
    if (!sweptExports) {
      sweptExports = true;
      // Temporary files closed pages left behind, at least a day old and held by no page.
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
    refuseDegradedSources(snapshot, compiled.project, used);
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
          extensionsWasmUrl: extensionWasmUrls(runtimeAssets),
          videoDecode,
        });
        // The retry policy picks the renderer: CPU after a self-caused loss or a second page loss.
        const renderer = new Engine(
          new OffscreenCanvas(project.settings.width, project.settings.height),
          pixels,
          lib,
          runtimeAssets,
          {
            videoConversion: exportConversion,
            renderer: attempt.renderer === "cpu" ? "cpu" : rendererMode,
          },
        );
        const audio = new AudioWindowSupplier(lib);
        try {
          signal.throwIfAborted();
          await renderer.start();
          signal.throwIfAborted();
          await renderer.registerEffects(effects, "export");
          signal.throwIfAborted();
          renderer.loadProject(project, "export");
          // Only an export that draws no runtime effect can move to the CPU renderer.
          retryPolicy.cpuRenderer =
            renderer.projectRuntimeEffects().length === 0;
          const refused = renderer.setCaptionFonts(fontStack);
          // Reported once per job, not per attempt.
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
            videoEncoder: attempt.videoEncoder,
            onSoftwareEncoder: (available) => {
              retryPolicy.softwareVideoEncoder = available;
            },
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
              renderer: renderer.rendererChoice,
            },
          };
        } catch (error) {
          // Count this loss for the page too; the next attempt's policy reads it.
          if (renderer.deviceLost) pageGpuLosses.add();
          throw error;
        } finally {
          await audio.close();
          await renderer.dispose();
          await pixels.dispose();
        }
      },
    };
    // Narrowed by each attempt once its renderer has loaded the project.
    const retryPolicy = { cpuRenderer: true };
    const job = new ExportJob(
      revision,
      snapshot,
      backend,
      () => {
        held.forEach((l) => l.release());
        jobs.delete(job);
      },
      { retry: retryPolicy },
    );
    jobs.add(job);
    exportSettled = job.result.then(
      () => {},
      () => {},
    );
    // A proxy being made yields the encoder, GPU and memory, and restarts after the export.
    proxyRun?.preempt.abort(
      new DOMException("An export takes the proxy's place", "AbortError"),
    );
    return job;
  }
  /**
   * What an export reads for each source, as the engine selects it: originals unless
   * `options.media` asks for proxies. Refuses before holding anything when the media is not ready.
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
      // A larger file holds more cues than a project can, so refuse it unread.
      if (file.size > MAX_CAPTION_FILE_BYTES)
        throw new EditorError(
          "invalid_captions",
          `${file.name} is ${file.size} bytes; a subtitle file is read up to ${MAX_CAPTION_FILE_BYTES}`,
          "import",
        );
      const { parseCaptions } = await loadWasm();
      const text = await readUserText(file);
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
      // Measured at the canvas size, whose pixels its lengths are in.
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
          // One proxy at a time and none during an export; a starting export preempts it.
          const turn = await proxyTurn(signal);
          try {
            // Held until its runtime is released so no page removes it meanwhile.
            hold = await proxyStore.hold(hash);
            // Notify listeners once per whole percent.
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
      // The next load asks the engine again. Playback reads only online sources, so leave it alone.
      if (engine && !playing) refreshPreview();
    },
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
    async videoEncoders({ videoQuality } = {}) {
      await decisions();
      const { probeEncoders } = await import("./runtime/exporter.js");
      const { width, height, frame_rate } = editor.document.project.settings;
      return probeEncoders(width, height, {
        frameRate: frame_rate,
        videoQuality,
      });
    },
    proxyStatus(id) {
      const descriptor = editor.document.assets[id];
      const hash = hashOf(descriptor);
      return {
        available: !!hash && proxies.has(hash),
        generating: proxyJobs.get(id) ?? null,
        reading: previewSelection.get(id)?.media ?? "original",
      };
    },
    listProxies: () => proxyStore.list(),
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
      // Leased after the waits and before the next await, so no release can come between.
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
          // An image sequence uploads each frame file under its locator.
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
      // Bridge-wide: each preview binding's mixer starts at it.
      monitorVolume = value;
      mixer?.setMasterVolume(value);
    },
    get monitorVolume() {
      return monitorVolume;
    },
    setAudioOffset(ms) {
      if (!Number.isFinite(ms) || Math.abs(ms) > 2000)
        throw Error("Audio offset must be between -2000 and 2000 ms");
      audioOffsetMs = ms;
      mixer?.setLatencyOffset(ms);
    },
    get audioOffsetMs() {
      return audioOffsetMs;
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
      // The renderer that draws; in worker mode the main-thread one does not.
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
          audioLatencySource: mixer?.latencySource ?? null,
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
    // A source's own media while online, whichever one preview reads, so the size, file and peaks
    // come from the original.
    getAsset: (id) =>
      (preferredPreviewMedia === "proxies" &&
        sourceView(editor.draft, id, originalOf(editor.draft, id))) ||
      library.get(id),
  };
}

/** `scale` checked as a preview resolution scale: finite, above 0 and at most 8. */
function checkedPreviewScale(scale) {
  if (typeof scale !== "number" || !(scale > 0 && scale <= 8))
    throw new EditorError(
      "invalid_preview_scale",
      "The preview scale must be a number above 0 and at most 8",
      "preview",
    );
  return scale;
}

function checkedVideoConversion(value, operation) {
  if (value !== "engine" && value !== "browser")
    throw new EditorError(
      "invalid_video_conversion",
      `videoConversion must be "engine" or "browser"; received ${JSON.stringify(value)}`,
      operation,
    );
  return value;
}

/** Whether the engine draws a source itself with no backing file (generated or a composition). */
function drawnByEngine(source) {
  return source.kind === "generated" || source.kind === "composition";
}

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

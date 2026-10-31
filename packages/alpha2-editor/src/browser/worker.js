import { Alpha2WasmError, serializeError } from "alpha2/worker-error";
import { SourcePixels } from "./runtime/sources.js";
import { AudioWindowSupplier, mixBlock } from "./runtime/audio.js";

/**
 * Serves the editor's preview protocol (`createWorkerPreview`) on `scope`, a dedicated worker's
 * global. Importing this module has no effect until this is called. Requests run one at a
 * time in arrival order. A `cancel` or `replace-project` stops the render in flight and drops the
 * seeks that arrived before it, not those sent after it. Requests from another session or an
 * older revision are answered with an error.
 * @param {import("./types.js").EditorWorkerScope} scope
 * @returns {{ dispose(): Promise<void> }} Waits for queued requests, releases the engine and
 *   decoders, and detaches the message handler.
 */
export function installEditorWorker(scope) {
  let engine = null,
    pixels = null,
    audio = null,
    sessionId = null,
    bitmapOutput = false,
    currentProject = null,
    revision = -1,
    // The renderer's diagnostics snapshot last posted: a seek reply carries a new one only.
    sentDiagnostics = null,
    tail = Promise.resolve();
  const assets = new Map(),
    library = { get: (id) => assets.get(id) };
  const hasContent = (project) =>
    project?.timeline.tracks.some(
      (track) =>
        (track.clips?.length ?? 0) + (track.captions?.cues.length ?? 0),
    ) ?? false;
  // The canvas is the size frames render at, the document's size times the preview scale; with
  // nothing loaded it stays as it is.
  const sizeCanvas = () => {
    const size = hasContent(currentProject) ? engine.previewSize : null;
    if (size) engine.resize(size[0], size[1]);
    return size;
  };
  const context = () => ({
    width: engine.project.settings.width,
    height: engine.project.settings.height,
    liveSourceIds: new Set(engine.project.sources.map((s) => s.id)),
    getLayer: (id) => {
      const asset = library.get(id);
      return asset?.kind === "text" ? { id, kind: "image", asset } : asset;
    },
  });
  // Counts cancels and project replacements. A request remembers the count on arrival, so work
  // queued before a cancel or replacement is dropped however long it waited.
  let epoch = 0;
  scope.onmessage = ({ data: request }) => {
    if (request.type === "cancel" || request.type === "replace-project") {
      epoch += 1;
      // The render in flight is for what this replaces: it stops.
      engine?.invalidate();
    }
    const arrived = epoch;
    tail = tail.then(async () => {
      const { requestId, type } = request;
      const response = {
        sessionId: request.sessionId,
        revision: request.revision,
        requestId,
      };
      try {
        if (type === "init") {
          if (engine) throw Error("Worker already initialized");
          sessionId = request.sessionId;
          bitmapOutput = request.bitmapOutput === true;
          pixels = new SourcePixels(library, {
            extensionsWasmUrl: request.runtimeAssets?.extensionsWasmUrl,
          });
          const { Engine } = await import("./runtime/engine.js");
          engine = new Engine(
            request.canvas ??
              new OffscreenCanvas(request.width ?? 16, request.height ?? 16),
            pixels,
            library,
            request.runtimeAssets,
            {
              unreadableFrames: "present",
              videoConversion: request.videoConversion,
              opaqueEngineFrames: request.opaqueEngineFrames,
              renderer: request.renderer ?? "auto",
            },
          );
          audio = new AudioWindowSupplier(library);
          engine.backend = "worker";
          await engine.start();
        } else {
          if (!engine || request.sessionId !== sessionId)
            throw Error("Stale worker session");
          if (request.revision < revision) throw Error("Stale worker revision");
          if (type === "replace-project") {
            // What the previous project sent and still uses stays, decoders included.
            const keep = new Set(request.keep ?? []);
            for (const id of [...assets.keys()])
              if (!keep.has(id)) assets.delete(id);
            await pixels.release(keep);
            for (const asset of request.assets ?? [])
              assets.set(asset.id, asset);
            if (request.fonts) engine.setCaptionFonts(request.fonts);
            currentProject = request.project;
            if (hasContent(currentProject)) engine.loadProject(request.project);
            const renderSize = sizeCanvas();
            // Sources are fitted in document space; the render scales them.
            pixels.setOutputSize(
              request.project.settings.width,
              request.project.settings.height,
            );
            audio.configure(engine.audioEngine, request.project);
            revision = request.revision;
            scope.postMessage({ ...response, ok: true, value: { renderSize } });
            return;
          } else if (type === "seek") {
            const generation = engine.generation;
            const {
              rendered,
              unchanged,
              degradedSources = [],
            } = arrived !== epoch || !hasContent(currentProject)
              ? { rendered: false, unchanged: false }
              : await engine.render(request.tick / 120000, {
                  generation,
                  context: context(),
                });
            // An unchanged frame was not presented: the surface it was copied to still shows it.
            const bitmap =
              rendered && !unchanged && bitmapOutput
                ? engine.canvas.transferToImageBitmap()
                : null;
            const engineDiagnostics =
              engine.lastDiagnostics !== sentDiagnostics
                ? engine.lastDiagnostics
                : null;
            sentDiagnostics = engine.lastDiagnostics;
            scope.postMessage(
              {
                ...response,
                ok: true,
                value: {
                  rendered,
                  unchanged,
                  degradedSources,
                  diagnostics: pixels.diagnostics,
                  ...(engineDiagnostics ? { engineDiagnostics } : {}),
                  ...(bitmap ? { bitmap } : {}),
                },
              },
              bitmap ? [bitmap] : [],
            );
            return;
          } else if (type === "set-preview-scale") {
            engine.setPreviewScale(request.scale);
            const renderSize = sizeCanvas();
            scope.postMessage({ ...response, ok: true, value: { renderSize } });
            return;
          } else if (type === "set-tracing") {
            if (request.passTiming !== undefined)
              engine.setPassTiming(request.passTiming);
            if (request.traceUploads !== undefined)
              engine.traceUploads = request.traceUploads;
          } else if (type === "audio-mix") {
            await audio.supply(request.startSample, request.frames);
            const planes = mixBlock(
              engine.audioEngine,
              request.startSample,
              request.frames,
              false,
            );
            const owned = planes.map((p) => p.slice());
            scope.postMessage(
              { ...response, ok: true, value: owned },
              owned.map((p) => p.buffer),
            );
            return;
          } else if (type === "resize")
            engine.resize(request.width, request.height);
          else if (type === "register-fonts")
            engine.setCaptionFonts(request.fonts);
          else if (type === "register-effects")
            await engine.registerEffects(request.effects);
          else if (type === "dispose") {
            await audio.close();
            await engine.dispose();
            await pixels.dispose();
            assets.clear();
            engine = null;
          } else if (type !== "cancel")
            throw Error(`Unknown editor worker request ${type}`);
        }
        scope.postMessage({ ...response, ok: true });
      } catch (error) {
        // The code, fatality, name and failure the bridge decides by, and the cause's code: a
        // source the browser cannot hand over arrives wrapped in the frame that needed it.
        const causeCode = error?.cause?.code;
        // What a lost GPU said about its loss, read before the engine is freed: the page's
        // recovery ladder decides by it.
        let lossReport;
        try {
          lossReport = engine?.lossReport();
        } catch {
          lossReport = undefined;
        }
        scope.postMessage({
          ...response,
          ok: false,
          error: {
            ...serializeError(error, false, "worker_error"),
            ...(typeof causeCode === "string" ? { causeCode } : {}),
            ...(lossReport === undefined ? {} : { lossReport }),
          },
        });
      }
    });
  };
  return {
    async dispose() {
      await tail;
      await audio?.close();
      await engine?.dispose();
      await pixels?.dispose();
      assets.clear();
      scope.onmessage = null;
    },
  };
}

/**
 * Drives a preview renderer in `worker`, which must run `installEditorWorker`. The caller owns
 * the worker; `dispose()` terminates it. This port renders picture and mixes audio blocks only:
 * it is separate from a bridge's main-thread audio output. Every call waits for `ready`, which
 * rejects if the worker cannot start its engine; a disposed preview rejects new calls, and a
 * worker error rejects every pending one.
 * @param {Worker} worker
 * @param {import("./types.js").WorkerPreviewOptions} [options]
 * @returns {import("./types.js").WorkerPreview}
 */
export function createWorkerPreview(
  worker,
  {
    sessionId = crypto.randomUUID(),
    runtimeAssets = {},
    canvas,
    bitmapOutput = false,
    width = 16,
    height = 16,
    videoConversion,
    opaqueEngineFrames,
    renderer,
  } = {},
) {
  let nextId = 0,
    revision = 0,
    disposed = false;
  const pending = new Map();
  const rejectAll = (error) => {
    for (const waiter of pending.values()) waiter.reject(error);
    pending.clear();
  };
  worker.onmessage = ({ data }) => {
    if (data.sessionId !== sessionId) {
      data.value?.bitmap?.close();
      return;
    }
    const waiter = pending.get(data.requestId);
    if (!waiter) {
      data.value?.bitmap?.close();
      return;
    }
    pending.delete(data.requestId);
    if (data.ok) waiter.resolve(data.value);
    else {
      const error = new Alpha2WasmError(data.error);
      if (typeof data.error.causeCode === "string")
        Object.defineProperty(error, "cause", {
          value: { code: data.error.causeCode },
          configurable: true,
        });
      if (data.error.lossReport !== undefined)
        Object.defineProperty(error, "lossReport", {
          value: data.error.lossReport,
          configurable: true,
        });
      waiter.reject(error);
    }
  };
  worker.onerror = (event) => rejectAll(new Error(event.message));
  const call = (type, body = {}, transfer = []) => {
    if (disposed) return Promise.reject(new Error("Worker preview disposed"));
    const requestId = ++nextId;
    return new Promise((resolve, reject) => {
      pending.set(requestId, { resolve, reject });
      try {
        worker.postMessage(
          { type, sessionId, revision, requestId, ...body },
          transfer,
        );
      } catch (error) {
        pending.delete(requestId);
        reject(error);
      }
    });
  };
  const ready = call(
    "init",
    {
      runtimeAssets,
      bitmapOutput,
      width,
      height,
      ...(videoConversion === undefined ? {} : { videoConversion }),
      ...(opaqueEngineFrames === undefined ? {} : { opaqueEngineFrames }),
      ...(renderer === undefined ? {} : { renderer }),
      ...(canvas ? { canvas } : {}),
    },
    canvas ? [canvas] : [],
  );
  return {
    ready,
    async replaceProject(
      project,
      nextRevision,
      assets = [],
      fonts = [],
      keep = [],
    ) {
      await ready;
      revision = nextRevision;
      return call("replace-project", { project, assets, fonts, keep });
    },
    async seek(tick) {
      await ready;
      return call("seek", { tick });
    },
    async setPreviewScale(scale) {
      await ready;
      return call("set-preview-scale", { scale });
    },
    async setTracing(options) {
      await ready;
      return call("set-tracing", options);
    },
    async registerEffects(effects) {
      await ready;
      return call("register-effects", { effects });
    },
    async mix(startSample, frames) {
      await ready;
      return call("audio-mix", { startSample, frames });
    },
    cancel: () => call("cancel"),
    async dispose() {
      if (disposed) return;
      try {
        await ready;
        await call("dispose");
      } finally {
        disposed = true;
        worker.terminate();
        rejectAll(new Error("Worker disposed"));
      }
    },
  };
}

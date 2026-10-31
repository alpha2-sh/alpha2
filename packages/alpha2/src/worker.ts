import type {
  DecodedVideoFrame,
  MissingFrame,
  PresentedFrame,
  RenderBackend,
} from "./player.js";
import type {
  EffectDefinition,
  EffectResourceData,
  FrameUploadReceipt,
  WasmCanvasRenderer,
  WasmEffectRegistryBinding,
  WasmMissingFrame,
} from "./effects.js";
import type { ExportBackend, ExportTicket } from "./exporter.js";
import {
  lossReportFromJson,
  type GpuLossReport,
  type RecoveryStep,
  type SurfaceProbe,
} from "./gpu-recovery.js";
import {
  createGpuRenderer,
  createRenderer,
  gpuChoice,
  type RendererChoice,
  type RendererMode,
} from "./renderer-choice.js";
import {
  type BrowserCapabilities,
  detectBrowserCapabilities,
} from "./robustness.js";
import type { EngineVideoCodec } from "./rate-control.js";
import {
  FrameGenerationRetainer,
  type FrameRetentionOptions,
  type SourceTransferStats,
} from "./frame-supply.js";
import {
  intakeVideoFrame,
  readbackCounters,
  type ReadbackRecoveryStats,
  SoftwareDecodeRequiredError,
} from "./frame-intake.js";
import type { ColorMetadata } from "./generated/project-schema.js";
import { videoFrameFromI420, videoFrameRepackedBytes } from "./video-frames.js";
import { recordRepackedBytes } from "./repacked-bytes.js";
import {
  type ChromaSiting,
  decodeRefusalMessage,
  NonPlanarVideoFrameError,
  PlanarCopyPool,
  PlanarDecodeUnavailableError,
  isHdrVideoFrame,
  isPlanarVideoFrame,
  uploadPlanarYuvFrame,
  type VideoConversion,
} from "./hdr-frames.js";
import {
  Alpha2WasmError,
  type SerializedAlpha2Error,
  serializeError,
  wasmTrapCode,
} from "./worker-error.js";
import {
  PhysicalFrameAliases,
  type PhysicalFrameIdentity,
} from "./physical-frames.js";
import { sendsBitmaps, uploadPresentedFrame } from "./presented-frames.js";

/**
 * A message the main thread posts to the render worker. Every request carries an `id` the worker
 * echoes in its answer; the worker handles requests one at a time, in arrival order.
 */
export type RenderWorkerRequest =
  | {
      readonly id: number;
      readonly type: "initialize";
      readonly canvas: OffscreenCanvas;
    }
  | {
      readonly id: number;
      readonly type: "upload-video-frame";
      readonly sourceId: string;
      readonly requestedTimeTicks: bigint;
      readonly decodedTimeTicks: bigint;
      readonly generation: number;
      readonly frame: VideoFrame;
      readonly rotationDegrees?: 0 | 90 | 180 | 270;
      readonly mirror?: boolean;
      readonly chromaSiting?: ChromaSiting;
      /** Set when the page decoded the frame in software, as a host asked it to. */
      readonly decode?: "software";
    }
  | {
      readonly id: number;
      readonly type: "present";
      readonly timeTicks: bigint;
    }
  | { readonly id: number; readonly type: "reset" }
  | {
      readonly id: number;
      readonly type: "begin-export-frame";
      readonly timeTicks: bigint;
      readonly codec: EngineVideoCodec;
      /** The frame's time in the output, which its `VideoFrame.timestamp` is stamped with. */
      readonly outputTimeTicks: bigint;
      /** The output pictures by FFmpeg pixel format name; absent, I420. */
      readonly pixels?: string;
    }
  | {
      readonly id: number;
      readonly type: "recover";
      readonly step: RecoveryStep;
      /** The fresh surface of a `"fresh"` step, transferred with the request. */
      readonly canvas?: OffscreenCanvas;
    }
  | { readonly id: number; readonly type: "probe-surface" }
  | { readonly id: number; readonly type: "detect-capabilities" }
  | { readonly id: number; readonly type: "dispose" };

/**
 * A message the render worker posts back. A plain `{ id, ok }` answers the request with that `id`;
 * `export-frame-ready` later delivers the readback of a `begin-export-frame` request that answered
 * `rendered`; `device-lost` is posted before the answer of any request that found the device lost,
 * with what the lost renderer reported about the loss.
 */
export type RenderWorkerResponse =
  | { readonly id: number; readonly ok: true; readonly result?: unknown }
  | {
      readonly id: number;
      readonly ok: false;
      readonly error: SerializedAlpha2Error;
    }
  | {
      readonly type: "export-frame-ready";
      readonly id: number;
      readonly ok: true;
      readonly frame: VideoFrame;
      /**
       * Bytes the worker copied to pack the frame's planes tight (`videoFrameRepackedBytes`), which
       * the frame object that arrives cannot carry itself.
       */
      readonly repackedBytes: number;
    }
  | {
      readonly type: "export-frame-ready";
      readonly id: number;
      readonly ok: false;
      readonly error: SerializedAlpha2Error;
    }
  | { readonly type: "device-lost"; readonly report?: GpuLossReport };

export { Alpha2WasmError, type SerializedAlpha2Error } from "./worker-error.js";

/**
 * The GPU/wasm implementation installed inside the owning render worker, which
 * {@link installRenderWorker} drives. Its methods mirror `RenderBackend`'s; a thrown error or
 * rejection is serialized back to the caller as an `Alpha2WasmError`.
 */
export interface RenderWorkerHost {
  /** Takes ownership of `frame.frame`. */
  uploadVideoFrame(frame: DecodedVideoFrame): Promise<void> | void;
  present(timeTicks: bigint): Promise<PresentedFrame>;
  reset(): Promise<void> | void;
  /**
   * Renders timeline time `timeTicks` for export, its chroma packed where `codec` sites it, and
   * stamps the frame it reads back at `outputTimeTicks`, its time in the output.
   */
  beginExport(
    timeTicks: bigint,
    codec: EngineVideoCodec,
    outputTimeTicks: bigint,
    pixels?: string,
  ): ExportTicket;
  /**
   * The renderer the host draws with; `initialize` and each `recover` answer it. Absent for a host
   * that does not say.
   */
  readonly rendererChoice?: RendererChoice;
  /** Raw source video the host copied through script memory, cumulative; absent if it does not say. */
  readonly sourceTransfer?: SourceTransferStats;
  /**
   * Frames the browser could not read back or that had no planar format, and how the host
   * recovered them; cumulative, absent if it does not say.
   */
  readonly readbackRecovery?: ReadbackRecoveryStats;
  /** Checked after every request; must not throw (a throw is read as lost). */
  isDeviceLost(): boolean;
  /** What the lost renderer reported about its loss, read before `recover` frees it. */
  lossReport?(): GpuLossReport | undefined;
  /** The lost renderer's surface kind, and whether this context still gets a WebGPU adapter. */
  probeSurface?(): Promise<SurfaceProbe>;
  /**
   * Makes the renderer `step` names on `canvas` (a fresh surface) or the current one. The request
   * loop calls it while `isDeviceLost()` is true, or with a fresh surface.
   */
  recover(step: RecoveryStep, canvas?: OffscreenCanvas): Promise<void>;
  dispose(): void;
}

/** The worker global the request loop listens on; a `DedicatedWorkerGlobalScope` satisfies it. */
export interface RenderWorkerScope {
  onmessage: ((event: MessageEvent<RenderWorkerRequest>) => void) | null;
  postMessage(message: RenderWorkerResponse, transfer?: Transferable[]): void;
}

/** An OpenType font caption tracks and text generators draw with. */
export interface TextFont {
  readonly family: string;
  readonly bytes: Uint8Array;
  /**
   * One face of a font collection; without it every face of the file is registered, as the export
   * host registers a font file.
   */
  readonly faceIndex?: number;
}

/** What {@link createWasmRenderWorkerHost} registers on every binding it makes. */
export interface WasmRenderWorkerHostOptions {
  /** Custom effects, compiled and registered before the project loads. */
  readonly effects?: readonly EffectDefinition[];
  /** Effect resources (3D LUTs) the effects and project reference. */
  readonly resources?: readonly EffectResourceData[];
  /**
   * Fonts caption tracks and text generators draw with, in fallback order. Give the export host
   * the same files and preview text matches exported text.
   */
  readonly fonts?: readonly TextFont[];
  /** How long uploaded frame generations stay resident before the engine releases them. */
  readonly uploadRetention?: FrameRetentionOptions;
  /**
   * Who converts a decoded SDR `VideoFrame`: the engine, from its planes (`"engine"`, the
   * default), or the browser (`"browser"`); see {@link VideoConversion}.
   */
  readonly videoConversion?: VideoConversion;
  /**
   * Which renderer draws (see {@link RendererMode}). `"auto"` (the default) draws on a hardware GPU,
   * through WebGPU or else WebGL2, and on the engine's CPU renderer in wasm when the browser offers
   * neither or only a software adapter (Chrome with acceleration off, a blocklisted GPU,
   * SwiftShader); `"gpu"` accepts any adapter and never falls back, and `"cpu"` draws on the CPU and
   * opens no GPU context. The CPU renderer previews at half resolution by default and refuses
   * runtime effects (`effects`) with `EFFECT_ERROR`; it needs the binding's `createCpu`, without
   * which `"auto"` is `"gpu"`. The host reports the renderer it chose ({@link RendererChoice}).
   */
  readonly renderer?: RendererMode;
  /**
   * What becomes of a frame the engine converts from planes whose planes the browser cannot read
   * back, once reading them again has failed: `"redecode"` (the default) rejects its upload with
   * `SoftwareDecodeRequiredError` (code `SOFTWARE_DECODE_REQUIRED`), which `Alpha2Player` and
   * `exportToSink` answer by asking their refill for the same picture decoded in software;
   * `"present"` gives it to the browser's conversion instead, unless the engine must convert it
   * (HDR, or color the project declares), for a preview where a frame shown at once matters more
   * than the engine's conversion. A frame the engine must convert that has no planar format it
   * reads (an opaque hardware frame) is sent back for a software decode either way; the same
   * frame decoded in software and still without planes rejects with
   * `PlanarDecodeUnavailableError`.
   */
  readonly unreadableFrames?: "redecode" | "present";
}

/**
 * Installs a serial request loop in the worker that owns the GPU device, replacing
 * `scope.onmessage`.
 *
 * `initialize` calls `createHost` with the transferred canvas; any other request but `dispose`
 * before it is answered with an error. Each answer is preceded by a `device-lost` message when the
 * host reports a lost device. A wasm trap is latched: every later error is reported as fatal,
 * because the module is no longer usable even when a later call fails with an ordinary-looking
 * error.
 */
export function installRenderWorker(
  scope: RenderWorkerScope,
  createHost: (canvas: OffscreenCanvas) => Promise<RenderWorkerHost>,
): void {
  let host: RenderWorkerHost | undefined;
  let serial = Promise.resolve();
  // Latch traps: later calls may report the same plain error as recoverable re-entrancy, so only
  // the original trap reliably marks the module unusable.
  let trapped = false;
  // Posted before the answer that found the loss, because a host checks isDeviceLost() as soon as
  // its request settles.
  const reportDeviceLoss = (): void => {
    if (host !== undefined && isLost(host))
      scope.postMessage({ type: "device-lost", report: lossReportOf(host) });
  };
  scope.onmessage = (event): void => {
    serial = serial.then(async () => {
      const request = event.data;
      try {
        let result: unknown;
        switch (request.type) {
          case "initialize":
            host = await createHost(request.canvas);
            result = host.rendererChoice;
            break;
          case "upload-video-frame":
            // The frame was transferred here; with no host to take it over, it is closed here.
            if (host === undefined) request.frame.close();
            await requiredHost(host).uploadVideoFrame({
              sourceId: request.sourceId,
              requestedTimeTicks: request.requestedTimeTicks,
              decodedTimeTicks: request.decodedTimeTicks,
              generation: request.generation,
              frame: request.frame,
              rotationDegrees: request.rotationDegrees,
              mirror: request.mirror,
              ...(request.chromaSiting === undefined
                ? {}
                : { chromaSiting: request.chromaSiting }),
              ...(request.decode === undefined
                ? {}
                : { decode: request.decode }),
            });
            break;
          case "present":
            result = await requiredHost(host).present(request.timeTicks);
            break;
          case "reset":
            await requiredHost(host).reset();
            break;
          case "begin-export-frame": {
            const ticket = requiredHost(host).beginExport(
              request.timeTicks,
              request.codec,
              request.outputTimeTicks,
              request.pixels,
            );
            reportDeviceLoss();
            if (ticket.kind === "waiting") {
              scope.postMessage({
                id: request.id,
                ok: true,
                result: {
                  kind: "waiting",
                  missingSources: ticket.missingSources,
                  missingFrames: ticket.missingFrames,
                },
              });
              return;
            }
            scope.postMessage({
              id: request.id,
              ok: true,
              result: {
                kind: "rendered",
                retainedBytes: ticket.retainedBytes,
              },
            });
            void ticket.readback.then(
              (frame) => {
                reportDeviceLoss();
                try {
                  scope.postMessage(
                    {
                      type: "export-frame-ready",
                      id: request.id,
                      ok: true,
                      frame,
                      repackedBytes: videoFrameRepackedBytes(frame),
                    },
                    [frame],
                  );
                } catch (error) {
                  frame.close();
                  scope.postMessage({
                    type: "export-frame-ready",
                    id: request.id,
                    ok: false,
                    error: serializeError(error, trapped),
                  });
                }
              },
              (error: unknown) => {
                if (wasmTrapCode(error) !== undefined) trapped = true;
                reportDeviceLoss();
                scope.postMessage({
                  type: "export-frame-ready",
                  id: request.id,
                  ok: false,
                  error: serializeError(error, trapped),
                });
              },
            );
            return;
          }
          case "recover": {
            // A recovery asked for twice, or after another made the renderer again, has nothing to
            // replace: making it again would drop every upload and recompile every pipeline.
            // A fresh surface is used whatever the state: it was transferred here for this.
            const lost = requiredHost(host);
            if (isLost(lost) || request.canvas !== undefined)
              await lost.recover(request.step, request.canvas);
            result = lost.rendererChoice;
            break;
          }
          case "probe-surface": {
            const current = requiredHost(host);
            result =
              (await current.probeSurface?.()) ??
              ({
                kind: current.rendererChoice?.backend ?? "none",
                webgpuAdapter: false,
              } satisfies SurfaceProbe);
            break;
          }
          case "detect-capabilities":
            result = await detectBrowserCapabilities();
            break;
          case "dispose":
            host?.dispose();
            host = undefined;
            break;
        }
        reportDeviceLoss();
        scope.postMessage({ id: request.id, ok: true, result });
      } catch (error) {
        if (wasmTrapCode(error) !== undefined) trapped = true;
        reportDeviceLoss();
        scope.postMessage({
          id: request.id,
          ok: false,
          error: serializeError(error, trapped),
        });
      }
    });
  };
}

/**
 * Returns the `createHost` callback for {@link installRenderWorker}, backed by the alpha2 wasm
 * binding. The callback makes a canvas renderer, registers `options`' effects, resources and
 * fonts, then loads `projectJson`.
 *
 * The renderer is the one `options.renderer` picks (see {@link WasmRenderWorkerHostOptions}); the
 * host's `rendererChoice` says which, and `initialize` answers it.
 *
 * The callback rejects when the binding lacks the constructor the renderer needs
 * (`createForCanvas`, or `createCpu` for `"cpu"`), or when making the renderer, compiling an
 * effect or loading the project fails. After a device loss, `recover(step)` repeats the same
 * registration on the renderer the recovery step names.
 */
export function createWasmRenderWorkerHost(
  binding: WasmEffectRegistryBinding,
  projectJson: string,
  options: WasmRenderWorkerHostOptions = {},
): (canvas: OffscreenCanvas) => Promise<RenderWorkerHost> {
  const effects = options.effects ?? [];
  const resources = options.resources ?? [];
  const videoConversion = options.videoConversion ?? "engine";
  if (videoConversion !== "engine" && videoConversion !== "browser")
    throw new RangeError('videoConversion must be "engine" or "browser"');
  const renderer = options.renderer ?? "auto";
  if (renderer !== "auto" && renderer !== "gpu" && renderer !== "cpu")
    throw new RangeError('renderer must be "auto", "gpu" or "cpu"');
  const unreadableFrames = options.unreadableFrames ?? "redecode";
  if (unreadableFrames !== "redecode" && unreadableFrames !== "present")
    throw new RangeError('unreadableFrames must be "redecode" or "present"');
  return async (canvas) => {
    return WasmRenderWorkerHost.create(
      binding,
      canvas,
      projectJson,
      effects,
      resources,
      options.fonts ?? [],
      options.uploadRetention,
      videoConversion,
      renderer,
      unreadableFrames,
    );
  };
}

class WasmRenderWorkerHost implements RenderWorkerHost {
  readonly #binding: WasmEffectRegistryBinding;
  #canvas: OffscreenCanvas;
  readonly #mode: RendererMode;
  readonly #projectJson: string;
  readonly #effects: readonly EffectDefinition[];
  readonly #resources: readonly EffectResourceData[];
  readonly #fonts: readonly TextFont[];
  readonly #outputSize: { readonly width: number; readonly height: number };
  readonly #uploadedFrames: FrameGenerationRetainer;
  readonly #physicalFrames = new PhysicalFrameAliases();
  readonly #videoConversion: VideoConversion;
  readonly #unreadableFrames: "redecode" | "present";
  readonly #readback = readbackCounters();
  #planarCopyFrames = 0;
  #planarCopyBytes = 0;
  rendererChoice: RendererChoice;
  #pendingUploads: FrameUploadReceipt[] = [];
  /** Buffers the planar copies reuse; each is given back once its upload has taken it. */
  readonly #planarCopies = new PlanarCopyPool();
  /** `undefined` while recovery has freed the lost binding and not yet made another. */
  #registry: WasmCanvasRenderer | undefined;

  /** The live binding; with none, every call fails as the lost device it is. */
  get #wasm(): WasmCanvasRenderer {
    if (this.#registry === undefined) throw lostRenderer();
    return this.#registry;
  }

  private constructor(
    binding: WasmEffectRegistryBinding,
    canvas: OffscreenCanvas,
    projectJson: string,
    effects: readonly EffectDefinition[],
    resources: readonly EffectResourceData[],
    fonts: readonly TextFont[],
    wasm: WasmCanvasRenderer,
    uploadRetention: FrameRetentionOptions | undefined,
    videoConversion: VideoConversion,
    rendererChoice: RendererChoice,
    unreadableFrames: "redecode" | "present",
    mode: RendererMode,
  ) {
    this.#binding = binding;
    this.#mode = mode;
    this.#unreadableFrames = unreadableFrames;
    this.rendererChoice = rendererChoice;
    this.#canvas = canvas;
    this.#projectJson = projectJson;
    this.#effects = effects;
    this.#resources = resources;
    this.#fonts = fonts;
    this.#videoConversion = videoConversion;
    const document = JSON.parse(projectJson) as {
      settings: { width: number; height: number };
    };
    this.#outputSize = document.settings;
    this.#registry = wasm;
    this.#uploadedFrames = new FrameGenerationRetainer((generation) => {
      this.#registry?.releaseFrameGeneration(generation);
    }, uploadRetention);
  }

  static async create(
    binding: WasmEffectRegistryBinding,
    canvas: OffscreenCanvas,
    projectJson: string,
    effects: readonly EffectDefinition[],
    resources: readonly EffectResourceData[],
    fonts: readonly TextFont[],
    uploadRetention: FrameRetentionOptions | undefined,
    videoConversion: VideoConversion,
    mode: RendererMode,
    unreadableFrames: "redecode" | "present",
  ): Promise<WasmRenderWorkerHost> {
    const { wasm, choice } = await createRenderer(binding, canvas, mode);
    await registerEffects(wasm, effects);
    registerResources(wasm, resources);
    registerFonts(wasm, fonts);
    wasm.loadProject(projectJson);
    return new WasmRenderWorkerHost(
      binding,
      canvas,
      projectJson,
      effects,
      resources,
      fonts,
      wasm,
      uploadRetention,
      videoConversion,
      choice,
      unreadableFrames,
      mode,
    );
  }

  get sourceTransfer(): SourceTransferStats {
    return {
      planarCopyFrames: this.#planarCopyFrames,
      planarCopyBytes: this.#planarCopyBytes,
    };
  }

  get readbackRecovery(): ReadbackRecoveryStats {
    return { ...this.#readback };
  }

  async uploadVideoFrame(frame: DecodedVideoFrame): Promise<void> {
    const rotation = frame.rotationDegrees ?? 0;
    const mirror = frame.mirror ?? false;
    const identity = workerFrameIdentity(
      frame,
      this.#outputSize.width,
      this.#outputSize.height,
      rotation,
      mirror,
    );
    const exact = {
      sourceId: frame.sourceId,
      mediaTimeTicks: frame.requestedTimeTicks,
    };
    let declared: string | undefined;
    let required: boolean;
    let convert: boolean;
    try {
      if (
        this.#physicalFrames.alias(identity, exact, (existing, requested) =>
          this.#wasm.aliasVideoFrame(
            requested.sourceId,
            existing.mediaTimeTicks,
            requested.mediaTimeTicks,
          ),
        )
      ) {
        frame.frame.close();
        return;
      }
      // A project-declared color wins over the frame's own, so such frames, and all HDR ones, are
      // converted by the engine; other planar frames too unless the host leaves SDR to the browser.
      declared = this.#wasm.sourceColorMetadata(frame.sourceId);
      required =
        declared !== undefined ||
        this.#wasm.hdrSource(frame.sourceId) ||
        isHdrVideoFrame(frame.frame);
      convert =
        required ||
        (this.#videoConversion === "engine" && isPlanarVideoFrame(frame.frame));
    } catch (error) {
      // The frame is this host's from the start: a binding that refuses it before an upload takes
      // it over (a lost renderer, a trap) must not leave it holding a decoder surface.
      frame.frame.close();
      throw error;
    }
    if (convert) {
      let intake: Awaited<ReturnType<typeof intakeVideoFrame>>;
      try {
        intake = await intakeVideoFrame(frame.frame, {
          required,
          declared:
            declared === undefined
              ? null
              : (JSON.parse(declared) as ColorMetadata),
          identity: {
            rotation,
            mirror,
            ...(frame.chromaSiting === undefined
              ? {}
              : { chromaSiting: frame.chromaSiting }),
          },
          pool: this.#planarCopies,
          counters: this.#readback,
          unreadable: this.#unreadableFrames,
          takesPresented: true,
        });
      } catch (error) {
        frame.frame.close();
        throw error;
      }
      if (intake.kind === "redecode") {
        const transfer: string | null =
          frame.frame.colorSpace?.transfer ?? null;
        frame.frame.close();
        throw redecodeRequest(frame, intake.cause, transfer);
      }
      if (intake.kind === "planar") {
        frame.frame.close();
        const planar = intake.planar;
        if (frame.decode === "software") this.#readback.redecodedFrames += 1;
        this.#planarCopyFrames += 1;
        this.#planarCopyBytes += planar.data.byteLength;
        try {
          this.#pendingUploads.push(
            uploadPlanarYuvFrame(
              this.#wasm,
              frame.sourceId,
              frame.requestedTimeTicks,
              planar,
              this.#outputSize.width,
              this.#outputSize.height,
            ),
          );
        } finally {
          // The upload copied the planes into the engine before it returned.
          this.#planarCopies.release(planar.data);
        }
        this.#physicalFrames.record(identity, exact);
        return;
      }
    }
    const upload = [
      frame.sourceId,
      frame.requestedTimeTicks,
      frame.frame,
      this.#outputSize.width,
      this.#outputSize.height,
      rotation,
      mirror,
    ] as const;
    // A frame the engine takes directly still uploads synchronously.
    this.#pendingUploads.push(
      sendsBitmaps(this.#wasm)
        ? await uploadPresentedFrame(this.#wasm, ...upload)
        : this.#wasm.uploadPresentedVideoFrame(...upload),
    );
    this.#physicalFrames.record(identity, exact);
  }

  // The backend contract is asynchronous because a worker-side implementation
  // must be; this in-process one answers without awaiting anything.
  // eslint-disable-next-line @typescript-eslint/require-await
  async present(timeTicks: bigint): Promise<PresentedFrame> {
    const result = JSON.parse(this.#wasm.renderAt(timeTicks)) as {
      requestedTimeTicks: number;
      /** `null` until the engine has presented a frame, and after a reset (every seek). */
      presentedTimeTicks: number | null;
      missingSources: string[];
      missingFrames: WasmMissingFrame[];
      repeated: boolean;
      unchanged: boolean;
    };
    if (result.missingSources.length === 0) this.#retirePendingUploads();
    return {
      requestedTimeTicks: BigInt(result.requestedTimeTicks),
      presentedTimeTicks:
        result.presentedTimeTicks === null
          ? undefined
          : BigInt(result.presentedTimeTicks),
      missingSources: result.missingSources,
      missingFrames: result.missingFrames.map(missingFrame),
      repeated: result.repeated,
      unchanged: result.unchanged,
    };
  }

  beginExport(
    timeTicks: bigint,
    codec: EngineVideoCodec,
    outputTimeTicks: bigint,
    pixels?: string,
  ): ExportTicket {
    const ticket = this.#wasm.beginExportAt(timeTicks, codec, pixels);
    if (ticket.kind === "waiting")
      return {
        kind: "waiting",
        missingSources: ticket.missingSources,
        missingFrames: ticket.missingFrames.map(missingFrame),
      };
    this.#retirePendingUploads();
    const readback = ticket.readback.then((result) =>
      videoFrameFromI420(result.pixels, {
        // The 10-bit formats are newer than the DOM types this package builds with. A binding
        // that names no format answers I420.
        format: (result.format ?? "I420") as VideoPixelFormat,
        codedWidth: result.width,
        codedHeight: result.height,
        // The output's time, as `ExportedFrame.timeTicks` counts it, not the timeline's: a host
        // that encodes the frame as it arrives starts its file at 0.
        timestamp: Number((outputTimeTicks * 1_000_000n) / 120_000n),
        layout: [...result.layout],
        colorSpace: result.colorSpace,
      }),
    );
    void readback.catch(() => {});
    return {
      kind: "rendered",
      retainedBytes: ticket.retainedBytes,
      readback,
    };
  }

  reset(): void {
    this.#physicalFrames.clear();
    this.#wasm.resetPlayer();
  }

  isDeviceLost(): boolean {
    return this.#registry?.deviceLost ?? true;
  }

  lossReport(): GpuLossReport | undefined {
    const wasm = this.#registry;
    if (wasm === undefined || !wasm.deviceLost) return undefined;
    return lossReportFromJson(wasm.lossReport);
  }

  async probeSurface(): Promise<SurfaceProbe> {
    return {
      kind: this.#registry === undefined ? "none" : this.rendererChoice.backend,
      webgpuAdapter: await webgpuAdapterAvailable(),
    };
  }

  /**
   * Makes the binding `step` names, on `canvas` when the step brings a fresh surface, else on the
   * current one. The lost binding is freed first and never used again: when making the new one
   * fails (a WebGL context the browser did not restore, a GPU the browser withholds), this host
   * stays lost and the next step tries. A GPU step refuses software adapters unless the host was
   * asked for `"gpu"`; a CPU step refuses the host's runtime effects, as the CPU renderer does.
   */
  async recover(step: RecoveryStep, canvas?: OffscreenCanvas): Promise<void> {
    this.#discardUploadedFrames();
    const lost = this.#registry;
    this.#registry = undefined;
    lost?.free?.();
    const surface = canvas ?? this.#canvas;
    // A transferred canvas is the host's surface from now on, whether or not a renderer takes it.
    this.#canvas = surface;
    let wasm: WasmCanvasRenderer;
    let choice: RendererChoice;
    if (step.renderer === "cpu") {
      const made = await createRenderer(this.#binding, surface, "cpu");
      wasm = made.wasm;
      choice = {
        ...made.choice,
        rendererReason: `the GPU was lost: ${step.reason}`,
      };
    } else {
      wasm = await createGpuRenderer(
        this.#binding,
        surface,
        this.#mode === "gpu" ? "allow" : "refuse",
      );
      choice = gpuChoice(wasm);
    }
    try {
      await registerEffects(wasm, this.#effects);
      registerResources(wasm, this.#resources);
      registerFonts(wasm, this.#fonts);
      wasm.loadProject(this.#projectJson);
    } catch (error) {
      wasm.free?.();
      throw error;
    }
    this.#registry = wasm;
    this.rendererChoice = choice;
  }

  dispose(): void {
    this.#discardUploadedFrames();
    this.#registry?.free?.();
    this.#registry = undefined;
  }

  #retirePendingUploads(): void {
    this.#uploadedFrames.retire(this.#pendingUploads);
    this.#pendingUploads = [];
  }

  #discardUploadedFrames(): void {
    this.#pendingUploads = [];
    this.#uploadedFrames.discardAll();
    this.#physicalFrames.clear();
  }
}

/** Whether `navigator.gpu` gives an adapter in this context now; a refused request gives none. */
async function webgpuAdapterAvailable(): Promise<boolean> {
  const gpu = (
    globalThis.navigator as
      (Navigator & { gpu?: { requestAdapter(): Promise<unknown> } }) | undefined
  )?.gpu;
  if (gpu === undefined) return false;
  try {
    return (await gpu.requestAdapter()) != null;
  } catch {
    return false;
  }
}

/** What `host` reports about its loss; a host whose report throws reports nothing. */
function lossReportOf(host: RenderWorkerHost): GpuLossReport | undefined {
  try {
    return host.lossReport?.();
  } catch {
    return undefined;
  }
}

/** The engine's name for a missing frame, with its source time as a tick count. */
function missingFrame(frame: WasmMissingFrame): MissingFrame {
  return {
    clipId: frame.clipId,
    ...(frame.readerId ? { readerId: frame.readerId } : {}),
    sourceId: frame.sourceId,
    mediaTimeTicks: BigInt(frame.mediaTimeTicks),
  };
}

function workerFrameIdentity(
  source: DecodedVideoFrame,
  outputWidth: number,
  outputHeight: number,
  rotation: 0 | 90 | 180 | 270,
  mirror: boolean,
): PhysicalFrameIdentity {
  return {
    sourceId: source.sourceId,
    contentGeneration: source.generation,
    timestamp: source.decodedTimeTicks.toString(),
    duration: source.frame.duration,
    sourceWidth: source.frame.displayWidth,
    sourceHeight: source.frame.displayHeight,
    outputWidth,
    outputHeight,
    rotation,
    mirror,
  };
}

/**
 * What a host answers for a frame it must have decoded again in software (`cause` from
 * `intakeVideoFrame`): the request for that decode, or, for a frame that already is one, the
 * refusal. A software frame still without planes means this browser cannot hand the source over
 * (`PlanarDecodeUnavailableError`); one still unreadable fails as `FrameSupply` fails it
 * (`FRAME_SUPPLY_FAILED`).
 */
function redecodeRequest(
  frame: DecodedVideoFrame,
  cause: unknown,
  transfer: string | null,
): Error {
  const nonPlanar = cause instanceof NonPlanarVideoFrameError;
  if (frame.decode !== "software")
    return new SoftwareDecodeRequiredError(
      {
        sourceId: frame.sourceId,
        mediaTimeTicks: frame.requestedTimeTicks.toString(),
        reason: nonPlanar ? "non-planar" : "unreadable",
      },
      { cause },
    );
  if (nonPlanar) {
    const facts = {
      kind: "video" as const,
      codec: null,
      transfer,
      frameFormat: cause.format,
      softwareDecoder: "opaque" as const,
      browserError: cause.message,
    };
    return new PlanarDecodeUnavailableError(
      decodeRefusalMessage(frame.sourceId, facts),
      facts,
      { cause },
    );
  }
  return Object.assign(
    new Error(
      `the browser could not read back the frame of ${frame.sourceId} at ${frame.requestedTimeTicks}, decoded again in software`,
      { cause },
    ),
    { code: "FRAME_SUPPLY_FAILED" },
  );
}

function registerFonts(
  wasm: WasmCanvasRenderer,
  fonts: readonly TextFont[],
): void {
  for (const font of fonts) {
    if (font.faceIndex === undefined) wasm.addFontFile(font.family, font.bytes);
    else wasm.addFont(font.family, font.bytes, font.faceIndex);
  }
}

async function registerEffects(
  wasm: WasmCanvasRenderer,
  effects: readonly EffectDefinition[],
): Promise<void> {
  for (const effect of effects) {
    wasm.registerCompiledEffect(
      await wasm.compileEffect(JSON.stringify(effect)),
    );
  }
}

function registerResources(
  wasm: WasmCanvasRenderer,
  resources: readonly EffectResourceData[],
): void {
  for (const resource of resources)
    wasm.registerLut3d(resource.id, resource.data);
}

/** Whether `host` lost its device; one whose check throws is taken to have lost it. */
function isLost(host: RenderWorkerHost): boolean {
  try {
    return host.isDeviceLost();
  } catch {
    return true;
  }
}

function requiredHost(host: RenderWorkerHost | undefined): RenderWorkerHost {
  if (host === undefined) throw new Error("render worker is not initialized");
  return host;
}

interface PendingRequest {
  readonly resolve: (value: unknown) => void;
  readonly reject: (reason: Error) => void;
}

interface PendingExportReadback {
  readonly resolve: (frame: VideoFrame) => void;
  readonly reject: (reason: Error) => void;
}

type ExportStart =
  | Extract<ExportTicket, { kind: "waiting" }>
  | { readonly kind: "rendered"; readonly retainedBytes: number };

type RequestWithoutId = RenderWorkerRequest extends infer Request
  ? Request extends { readonly id: number }
    ? Omit<Request, "id">
    : never
  : never;

/**
 * A `RenderBackend` that forwards every call to a worker running {@link installRenderWorker}, which
 * exclusively owns the wasm and GPU state.
 *
 * Calls reject with `Alpha2WasmError` for an error the worker reports, and with a plain `Error`
 * after {@link WorkerRenderBackend.dispose}. Once the worker raises an uncaught error, every
 * pending and later call rejects with a fatal `RENDER_WORKER_FAILED` and the worker must be
 * replaced.
 */
export class WorkerRenderBackend implements RenderBackend {
  readonly #worker: Worker;
  readonly #pending = new Map<number, PendingRequest>();
  readonly #pendingExportReadbacks = new Map<number, PendingExportReadback>();
  #nextId = 1;
  #rendererChoice: RendererChoice | undefined;
  #deviceLost = false;
  #lossReport: GpuLossReport | undefined;
  #disposed = false;
  /** Set once the worker raised an uncaught error; see `#onWorkerError`. */
  #failure: Alpha2WasmError | undefined;

  constructor(worker: Worker) {
    this.#worker = worker;
    worker.addEventListener("message", this.#onMessage);
    worker.addEventListener("error", this.#onWorkerError);
  }

  /**
   * The renderer the worker's host draws with, once {@link WorkerRenderBackend.initialize} has
   * resolved, and as each recovery changed it; `undefined` before, or for a host that does not say.
   */
  get rendererChoice(): RendererChoice | undefined {
    return this.#rendererChoice;
  }

  /** Transfers `canvas` to the worker and creates its host there. */
  async initialize(canvas: OffscreenCanvas): Promise<void> {
    const choice = await this.#request({ type: "initialize", canvas }, [
      canvas,
    ]);
    this.#rendererChoice = choice as RendererChoice | undefined;
  }

  /**
   * Transfers `frame.frame` to the worker; it is closed here if it cannot be posted, including
   * after {@link WorkerRenderBackend.dispose} or a worker failure.
   */
  async uploadVideoFrame(frame: DecodedVideoFrame): Promise<void> {
    await this.#request(
      {
        type: "upload-video-frame",
        sourceId: frame.sourceId,
        requestedTimeTicks: frame.requestedTimeTicks,
        decodedTimeTicks: frame.decodedTimeTicks,
        generation: frame.generation,
        frame: frame.frame,
        rotationDegrees: frame.rotationDegrees,
        mirror: frame.mirror,
        chromaSiting: frame.chromaSiting,
        ...(frame.decode === undefined ? {} : { decode: frame.decode }),
      },
      [frame.frame],
      () => frame.frame.close(),
    );
  }

  async present(timeTicks: bigint): Promise<PresentedFrame> {
    return (await this.#request({
      type: "present",
      timeTicks,
    })) as PresentedFrame;
  }

  async reset(): Promise<void> {
    await this.#request({ type: "reset" });
  }

  /**
   * Renders timeline time `timeTicks` offscreen for an export to `codec`, which decides where its
   * chroma is packed, stamping the frame at `outputTimeTicks`, its time in the output. A `rendered`
   * ticket's `readback` settles when the worker posts the frame.
   */
  beginExport(
    timeTicks: bigint,
    codec: EngineVideoCodec,
    outputTimeTicks: bigint,
    pixels?: string,
  ): Promise<ExportTicket> {
    if (this.#disposed) {
      return Promise.reject(new Error("render backend disposed"));
    }
    if (this.#failure !== undefined) return Promise.reject(this.#failure);
    const id = this.#nextId++;
    let resolveReadback!: (frame: VideoFrame) => void;
    let rejectReadback!: (reason: Error) => void;
    const readback = new Promise<VideoFrame>((resolve, reject) => {
      resolveReadback = resolve;
      rejectReadback = reject;
    });
    void readback.catch(() => {});
    this.#pendingExportReadbacks.set(id, {
      resolve: resolveReadback,
      reject: rejectReadback,
    });
    const started = new Promise<unknown>((resolve, reject) => {
      this.#pending.set(id, { resolve, reject });
      try {
        this.#worker.postMessage({
          id,
          type: "begin-export-frame",
          timeTicks,
          codec,
          outputTimeTicks,
          ...(pixels === undefined ? {} : { pixels }),
        });
      } catch (error) {
        const failure =
          error instanceof Error ? error : new Error(String(error));
        this.#pending.delete(id);
        this.#pendingExportReadbacks.delete(id);
        rejectReadback(failure);
        reject(failure);
      }
    });
    return started.then(
      (value) => {
        const result = value as ExportStart;
        if (result.kind === "waiting") {
          this.#pendingExportReadbacks.delete(id);
          return result;
        }
        return {
          kind: "rendered",
          retainedBytes: result.retainedBytes,
          readback,
        };
      },
      (error: unknown) => {
        this.#pendingExportReadbacks.delete(id);
        const failure =
          error instanceof Error ? error : new Error(String(error));
        rejectReadback(failure);
        throw failure;
      },
    );
  }

  isDeviceLost(): boolean {
    return this.#deviceLost;
  }

  /** What the worker's host reported with its last loss; cleared by a recovery. */
  lossReport(): GpuLossReport | undefined {
    return this.#lossReport;
  }

  /** Asks the worker for its lost renderer's surface kind and whether it gets a WebGPU adapter. */
  async probeSurface(): Promise<SurfaceProbe> {
    return (await this.#request({ type: "probe-surface" })) as SurfaceProbe;
  }

  /**
   * Has the worker's host make the renderer `step` names, transferring `canvas` (a fresh surface)
   * to it, and resolves to the renderer it made. Rejects with the host's error; the backend then
   * stays lost.
   */
  async recover(
    step: RecoveryStep,
    canvas?: OffscreenCanvas,
  ): Promise<RendererChoice | undefined> {
    const choice = (await this.#request(
      { type: "recover", step, ...(canvas === undefined ? {} : { canvas }) },
      canvas === undefined ? [] : [canvas],
    )) as RendererChoice | undefined;
    this.#deviceLost = false;
    this.#lossReport = undefined;
    if (choice !== undefined) this.#rendererChoice = choice;
    return choice;
  }

  /**
   * The browser's capabilities probed inside the worker, where its host renders: what
   * `renderer: "auto"` does there, which can differ from the page's answer.
   */
  async detectCapabilities(): Promise<BrowserCapabilities> {
    return (await this.#request({
      type: "detect-capabilities",
    })) as BrowserCapabilities;
  }

  /** Terminates the worker and rejects every pending call. Idempotent. */
  dispose(): void {
    if (this.#disposed) return;
    this.#disposed = true;
    this.#postWithoutReply({ type: "dispose" });
    this.#worker.removeEventListener("message", this.#onMessage);
    this.#worker.removeEventListener("error", this.#onWorkerError);
    this.#worker.terminate();
    const error = new Error("alpha2 render worker disposed");
    for (const request of this.#pending.values()) request.reject(error);
    this.#pending.clear();
    for (const readback of this.#pendingExportReadbacks.values()) {
      readback.reject(error);
    }
    this.#pendingExportReadbacks.clear();
  }

  readonly #onMessage = (event: MessageEvent<RenderWorkerResponse>): void => {
    const message = event.data;
    if ("type" in message) {
      if (message.type === "device-lost") {
        this.#deviceLost = true;
        if (message.report !== undefined) this.#lossReport = message.report;
        return;
      }
      const readback = this.#pendingExportReadbacks.get(message.id);
      if (readback === undefined) {
        if (message.ok) message.frame.close();
        return;
      }
      this.#pendingExportReadbacks.delete(message.id);
      if (message.ok) {
        recordRepackedBytes(message.frame, message.repackedBytes);
        readback.resolve(message.frame);
      } else readback.reject(new Alpha2WasmError(message.error));
      return;
    }
    const request = this.#pending.get(message.id);
    if (request === undefined) return;
    this.#pending.delete(message.id);
    if (message.ok) request.resolve(message.result);
    else request.reject(new Alpha2WasmError(message.error));
  };

  /**
   * An uncaught error in the worker is a failure outside any request, such as a wasm trap in a
   * future the module drove itself. The worker's serial queue may now wait forever on the call that
   * trapped, so a request posted after it would never be answered: the backend stays failed,
   * rejecting what is pending and everything later as fatal, and the host must replace the worker.
   */
  readonly #onWorkerError = (event: ErrorEvent): void => {
    const error = new Alpha2WasmError({
      code: "RENDER_WORKER_FAILED",
      message: `${event.message || "alpha2 render worker failed"}; the render worker must be replaced`,
      fatal: true,
    });
    this.#failure ??= error;
    for (const request of this.#pending.values()) request.reject(error);
    this.#pending.clear();
    for (const readback of this.#pendingExportReadbacks.values()) {
      readback.reject(error);
    }
    this.#pendingExportReadbacks.clear();
  };

  #request(
    request: RequestWithoutId,
    transfer: Transferable[] = [],
    onPostFailure?: () => void,
  ): Promise<unknown> {
    // A request refused before posting releases its transfers as a failed post would.
    if (this.#disposed) {
      onPostFailure?.();
      return Promise.reject(new Error("render backend disposed"));
    }
    if (this.#failure !== undefined) {
      onPostFailure?.();
      return Promise.reject(this.#failure);
    }
    const id = this.#nextId++;
    return new Promise((resolve, reject) => {
      this.#pending.set(id, { resolve, reject });
      try {
        this.#worker.postMessage({ ...request, id }, transfer);
      } catch (error) {
        this.#pending.delete(id);
        onPostFailure?.();
        reject(error instanceof Error ? error : new Error(String(error)));
      }
    });
  }

  #postWithoutReply(request: RequestWithoutId): void {
    try {
      this.#worker.postMessage({ ...request, id: this.#nextId++ });
    } catch {
      // Disposal is best effort; the worker is terminated right after.
    }
  }
}

/** What a worker host without a live binding answers until a recovery makes one. */
function lostRenderer(): Alpha2WasmError {
  return new Alpha2WasmError({
    code: "GPU_DEVICE_LOST",
    message:
      "the GPU device was lost and no renderer has been made again yet; recover(step) makes one",
    fatal: true,
  });
}

/**
 * An `ExportBackend` that renders through the same worker-owned renderer as preview. After
 * `abort`, `begin` rejects with the abort reason.
 */
export class WorkerExportBackend implements ExportBackend {
  readonly #renderer: WorkerRenderBackend;
  readonly #codec: EngineVideoCodec;
  readonly #pixels?: string;
  #aborted?: Error;

  /**
   * Exports through `renderer`, packing frames for the `codec` the sink encodes, in `pixels`
   * (`resolvePixels(...).format` from `alpha2/containers`; absent, I420). Pictures with alpha are
   * drawn without the project background.
   */
  constructor(
    renderer: WorkerRenderBackend,
    codec: EngineVideoCodec,
    pixels?: string,
  ) {
    this.#renderer = renderer;
    this.#codec = codec;
    if (pixels !== undefined && pixels !== "yuv420p") this.#pixels = pixels;
  }

  async begin(
    timeTicks: bigint,
    outputTimeTicks: bigint,
  ): Promise<ExportTicket> {
    if (this.#aborted !== undefined) throw this.#aborted;
    return this.#renderer.beginExport(
      timeTicks,
      this.#codec,
      outputTimeTicks,
      this.#pixels,
    );
  }

  isDeviceLost(): boolean {
    return this.#renderer.isDeviceLost();
  }

  abort(reason: Error): void {
    this.#aborted = reason;
  }
}

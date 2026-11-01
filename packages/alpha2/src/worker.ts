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

/** A request to the render worker. The worker handles requests serially and echoes each `id`. */
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
      /** Stamped as the frame's `VideoFrame.timestamp`. */
      readonly outputTimeTicks: bigint;
      /** FFmpeg pixel format name; I420 when absent. */
      readonly pixels?: string;
    }
  | {
      readonly id: number;
      readonly type: "recover";
      readonly step: RecoveryStep;
      /** Transferred surface for a `"fresh"` step. */
      readonly canvas?: OffscreenCanvas;
    }
  | { readonly id: number; readonly type: "probe-surface" }
  | { readonly id: number; readonly type: "detect-capabilities" }
  | { readonly id: number; readonly type: "dispose" };

/**
 * A message from the render worker. `export-frame-ready` later delivers the readback of a
 * `begin-export-frame` that answered `rendered`; `device-lost` precedes the answer of any request
 * that found the device lost.
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
      /** The worker's `videoFrameRepackedBytes`, which the transferred frame cannot carry. */
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
 * The GPU/wasm implementation inside the render worker, driven by {@link installRenderWorker}.
 * Methods mirror `RenderBackend`; errors reach the caller as `Alpha2WasmError`.
 */
export interface RenderWorkerHost {
  /** Takes ownership of `frame.frame`. */
  uploadVideoFrame(frame: DecodedVideoFrame): Promise<void> | void;
  present(timeTicks: bigint): Promise<PresentedFrame>;
  reset(): Promise<void> | void;
  /** Renders `timeTicks` for export with chroma sited for `codec`, stamped at `outputTimeTicks`. */
  beginExport(
    timeTicks: bigint,
    codec: EngineVideoCodec,
    outputTimeTicks: bigint,
    pixels?: string,
  ): ExportTicket;
  /** Answered by `initialize` and each `recover`. */
  readonly rendererChoice?: RendererChoice;
  /** Cumulative raw source video copied through script memory. */
  readonly sourceTransfer?: SourceTransferStats;
  /** Cumulative unreadable or non-planar frames and how they were recovered. */
  readonly readbackRecovery?: ReadbackRecoveryStats;
  /** Checked after every request; a throw is read as lost. */
  isDeviceLost(): boolean;
  /** Read before `recover` frees the lost renderer. */
  lossReport?(): GpuLossReport | undefined;
  probeSurface?(): Promise<SurfaceProbe>;
  /** Called while `isDeviceLost()` is true, or with a fresh surface in `canvas`. */
  recover(step: RecoveryStep, canvas?: OffscreenCanvas): Promise<void>;
  dispose(): void;
}

/** Satisfied by a `DedicatedWorkerGlobalScope`. */
export interface RenderWorkerScope {
  onmessage: ((event: MessageEvent<RenderWorkerRequest>) => void) | null;
  postMessage(message: RenderWorkerResponse, transfer?: Transferable[]): void;
}

/** An OpenType font caption tracks and text generators draw with. */
export interface TextFont {
  readonly family: string;
  readonly bytes: Uint8Array;
  /** One face of a collection; without it every face of the file is registered. */
  readonly faceIndex?: number;
}

/** What {@link createWasmRenderWorkerHost} registers on every binding it makes. */
export interface WasmRenderWorkerHostOptions {
  /** Compiled and registered before the project loads. */
  readonly effects?: readonly EffectDefinition[];
  /** Effect resources (3D LUTs) the effects and project reference. */
  readonly resources?: readonly EffectResourceData[];
  /** Fonts in fallback order. Give the export host the same files so preview text matches. */
  readonly fonts?: readonly TextFont[];
  /** How long uploaded frame generations stay resident. */
  readonly uploadRetention?: FrameRetentionOptions;
  /** Who converts decoded SDR frames (default `"engine"`); see {@link VideoConversion}. */
  readonly videoConversion?: VideoConversion;
  /**
   * Renderer selection (default `"auto"`; see {@link RendererMode}). The CPU renderer previews at
   * half resolution by default and refuses runtime `effects` with `EFFECT_ERROR`. Without the
   * binding's `createCpu`, `"auto"` behaves as `"gpu"`.
   */
  readonly renderer?: RendererMode;
  /**
   * Handling of a frame whose planes the browser cannot read back. `"redecode"` (default) rejects
   * the upload with `SOFTWARE_DECODE_REQUIRED` so `Alpha2Player` and `exportToSink` refill it in
   * software. `"present"` hands it to the browser's conversion instead, unless the engine must
   * convert it (HDR or project-declared color). A software-decoded frame still without planes
   * rejects with `PlanarDecodeUnavailableError`.
   */
  readonly unreadableFrames?: "redecode" | "present";
}

/**
 * Installs a serial request loop in the worker that owns the GPU device, replacing
 * `scope.onmessage`.
 *
 * Requests other than `dispose` fail until `initialize` has called `createHost`. After a wasm trap
 * every later error is reported as fatal.
 */
export function installRenderWorker(
  scope: RenderWorkerScope,
  createHost: (canvas: OffscreenCanvas) => Promise<RenderWorkerHost>,
): void {
  let host: RenderWorkerHost | undefined;
  let serial = Promise.resolve();
  // Later calls after a trap can fail with ordinary-looking errors, so the trap is latched.
  let trapped = false;
  // Posted before the answer because the caller checks isDeviceLost() as soon as it settles.
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
            // The frame was transferred here and has no host to take it.
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
            // A duplicate recovery would drop every upload and recompile every pipeline for
            // nothing, so skip it unless a fresh surface was transferred.
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
 * Returns a `createHost` callback for {@link installRenderWorker} backed by the wasm binding. It
 * creates the renderer `options.renderer` picks, registers effects, resources and fonts, then
 * loads `projectJson`; `recover` repeats the same registration.
 *
 * The callback rejects when the binding lacks the needed constructor, or when creating the
 * renderer, compiling an effect or loading the project fails.
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
  readonly #planarCopies = new PlanarCopyPool();
  // `undefined` between freeing the lost binding and making a new one.
  #registry: WasmCanvasRenderer | undefined;

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
      // Project-declared color and HDR frames always need the engine's conversion.
      declared = this.#wasm.sourceColorMetadata(frame.sourceId);
      required =
        declared !== undefined ||
        this.#wasm.hdrSource(frame.sourceId) ||
        isHdrVideoFrame(frame.frame);
      convert =
        required ||
        (this.#videoConversion === "engine" && isPlanarVideoFrame(frame.frame));
    } catch (error) {
      // Close the frame so a refused upload does not hold a decoder surface.
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

  // Async to match the worker contract, though this in-process host never awaits.
  // eslint-disable-next-line @typescript-eslint/require-await
  async present(timeTicks: bigint): Promise<PresentedFrame> {
    const result = JSON.parse(this.#wasm.renderAt(timeTicks)) as {
      requestedTimeTicks: number;
      // `null` until the engine has presented a frame, and after each reset.
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
        // The 10-bit formats are newer than this package's DOM types.
        format: (result.format ?? "I420") as VideoPixelFormat,
        codedWidth: result.width,
        codedHeight: result.height,
        // Output time, not timeline time, so a host encoding frames as they arrive starts at 0.
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
   * Frees the lost binding and makes the one `step` names. On failure the host stays lost and the
   * next step tries. A GPU step refuses software adapters unless the host was asked for `"gpu"`.
   */
  async recover(step: RecoveryStep, canvas?: OffscreenCanvas): Promise<void> {
    this.#discardUploadedFrames();
    const lost = this.#registry;
    this.#registry = undefined;
    lost?.free?.();
    const surface = canvas ?? this.#canvas;
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

function lossReportOf(host: RenderWorkerHost): GpuLossReport | undefined {
  try {
    return host.lossReport?.();
  } catch {
    return undefined;
  }
}

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
 * Requests a software decode, or refuses a frame that already is one: still without planes means
 * `PlanarDecodeUnavailableError`, still unreadable means `FRAME_SUPPLY_FAILED`.
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
 * A `RenderBackend` that forwards every call to a worker running {@link installRenderWorker}.
 *
 * Calls reject with `Alpha2WasmError` for worker errors, and with a plain `Error` after
 * {@link WorkerRenderBackend.dispose}. After an uncaught worker error, every pending and later call
 * rejects with a fatal `RENDER_WORKER_FAILED` and the worker must be replaced.
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
  #failure: Alpha2WasmError | undefined;

  constructor(worker: Worker) {
    this.#worker = worker;
    worker.addEventListener("message", this.#onMessage);
    worker.addEventListener("error", this.#onWorkerError);
  }

  /** Set by {@link WorkerRenderBackend.initialize} and each recovery; `undefined` before. */
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

  /** Transfers `frame.frame` to the worker, closing it here if it cannot be posted. */
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
   * Renders `timeTicks` for export with chroma sited for `codec`, stamped at `outputTimeTicks`. A
   * `rendered` ticket's `readback` settles when the worker posts the frame.
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

  /** Cleared by a recovery. */
  lossReport(): GpuLossReport | undefined {
    return this.#lossReport;
  }

  async probeSurface(): Promise<SurfaceProbe> {
    return (await this.#request({ type: "probe-surface" })) as SurfaceProbe;
  }

  /** Resolves to the renderer the worker made. On rejection the backend stays lost. */
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

  /** Capabilities probed inside the worker, which can differ from the page's. */
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

  // An uncaught worker error (such as a trap in a wasm-driven future) can leave the serial queue
  // waiting forever, so the backend fails every pending and later call.
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
   * `pixels` is `resolvePixels(...).format` from `alpha2/containers` (I420 when absent). Pictures
   * with alpha are drawn without the project background.
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

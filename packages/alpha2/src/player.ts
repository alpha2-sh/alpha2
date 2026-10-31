import type { ChromaSiting } from "./hdr-frames.js";
import {
  GpuRecoveryLadder,
  type GpuLossReport,
  type RecoveryStep,
  type SurfaceProbe,
} from "./gpu-recovery.js";
import type { RendererChoice } from "./renderer-choice.js";
import { Alpha2WasmError } from "./worker-error.js";
import {
  type SoftwareDecodeReason,
  softwareDecodeRequest,
} from "./frame-intake.js";
import {
  type DecoderRefillReason,
  type DecoderRefillRequest,
  decoderConsumerId,
} from "./media.js";

/** One decoded frame transferred to the worker-owned render backend. */
export interface DecodedVideoFrame {
  readonly sourceId: string;
  /** Exact source time requested by the immutable render snapshot. */
  readonly requestedTimeTicks: bigint;
  /** PTS of the physical frame selected by the decoder. */
  readonly decodedTimeTicks: bigint;
  readonly frame: VideoFrame;
  /** Clockwise display rotation, applied after `mirror`. */
  readonly rotationDegrees?: 0 | 90 | 180 | 270;
  /** Horizontal mirror applied before rotation. */
  readonly mirror?: boolean;
  /** Chroma sample position, which WebCodecs does not report; see `chromaSitingForStream`. */
  readonly chromaSiting?: ChromaSiting;
  /** Seek/recovery generation; stale frames are closed without upload. */
  readonly generation: number;
  /**
   * Set when the frame was decoded in software on request ({@link DecoderRefillRequest.decode}). A
   * render host that cannot take it refuses the source instead of asking again.
   */
  readonly decode?: "software";
}

/** A source frame a presentation needed and did not have. */
export interface MissingFrame {
  readonly clipId: string;
  /**
   * The composition step reading the source when it is not the clip itself. Each reader keeps its
   * own decode cursor.
   */
  readonly readerId?: string;
  readonly sourceId: string;
  /** The source time to decode; differs from timeline time for trimmed or retimed clips. */
  readonly mediaTimeTicks: bigint;
  /** `"software"` when the refill must decode this frame in software. */
  readonly decode?: "software";
}

/** Result of one real-time presentation attempt. */
export interface PresentedFrame {
  readonly requestedTimeTicks: bigint;
  /** The time whose picture is on screen; absent when nothing was presented. */
  readonly presentedTimeTicks?: bigint;
  readonly missingSources: readonly string[];
  readonly missingFrames: readonly MissingFrame[];
  /** Whether the previous picture was shown again instead of a new one. */
  readonly repeated: boolean;
  /** Whether the frame equals the one on screen, so nothing was rendered or presented. */
  readonly unchanged: boolean;
}

/** The worker-side renderer an {@link Alpha2Player} drives. */
export interface RenderBackend {
  /** Starts rendering into `canvas`. Called once, before any other method. */
  initialize(canvas: OffscreenCanvas): Promise<void>;
  /** Takes ownership of `frame.frame` and closes it once uploaded. */
  uploadVideoFrame(frame: DecodedVideoFrame): Promise<void>;
  present(timeTicks: bigint): Promise<PresentedFrame>;
  reset(): Promise<void>;
  /** Whether {@link RenderBackend.recover} must run before the next presentation. */
  isDeviceLost(): boolean;
  lossReport(): GpuLossReport | undefined;
  probeSurface(): Promise<SurfaceProbe>;
  /**
   * Makes the renderer `step` names, on `canvas` for a fresh-surface step, and resolves to it
   * (`undefined` if the backend does not say). Drops uploaded frames. Rejects when the renderer
   * cannot be made; the backend stays lost.
   */
  recover(
    step: RecoveryStep,
    canvas?: OffscreenCanvas,
  ): Promise<RendererChoice | undefined>;
  dispose(): void;
}

export interface Alpha2PlayerOptions {
  /** Called with the frames a presentation lacked; resolve after uploading what it decoded. */
  readonly onRefill?: (
    requests: readonly DecoderRefillRequest[],
  ) => void | Promise<void>;
  /**
   * Returns a new same-size canvas, already transferred to an `OffscreenCanvas`, to replace the one
   * on screen. Recoveries that need a fresh surface call it once per step; without it they fail
   * with a fatal `GPU_DEVICE_LOST`.
   */
  readonly replaceSurface?: () => Promise<OffscreenCanvas>;
  readonly onRendererChange?: (choice: RendererChoice) => void;
  readonly recovery?: GpuRecoveryLadder;
}

/**
 * Real-time browser runner that serializes presentation and recovers device loss.
 *
 * It keeps no clock: `present` shows the time it is given. For playback with audio, pass
 * `AudioPlaybackClock.timelineTicks` so the picture follows the audio device.
 *
 * A presentation that lacks source frames asks `onRefill` for them, then presents the same time
 * again. Frames uploaded after `onRefill` resolves show only on the next presentation.
 *
 * A frame the backend rejects with `SOFTWARE_DECODE_REQUIRED` is requested once more with
 * `decode: "software"`; a source with no planar format is requested in software from then on.
 * `onRefill` must honor `decode` (a `DecoderRefillCoordinator` does).
 *
 * A lost GPU is recovered on the next presentation through a {@link GpuRecoveryLadder}. When every
 * step fails, that presentation and all later ones reject with the same fatal error.
 */
export class Alpha2Player {
  readonly #backend: RenderBackend;
  #initialized = false;
  #latestRequest = 0;
  #generation = 0;
  // Skips re-presenting after a refill that uploaded nothing.
  #uploads = 0;
  // Shared by every presentation that finds the device lost.
  #recovering?: Promise<void>;
  #terminal?: Error;
  readonly #replaceSurface?: () => Promise<OffscreenCanvas>;
  readonly #onRendererChange?: (choice: RendererChoice) => void;
  readonly #ladder: GpuRecoveryLadder;
  readonly #onRefill?: (
    requests: readonly DecoderRefillRequest[],
  ) => void | Promise<void>;
  readonly #softwareSources = new Set<string>();
  // Frames sent back for a software decode and not yet requested again.
  readonly #softwareFrames = new Map<string, SoftwareDecodeReason>();
  // Frames already requested in software since the last seek; each is requested once.
  readonly #reissued = new Set<string>();

  constructor(backend: RenderBackend, options: Alpha2PlayerOptions = {}) {
    this.#backend = backend;
    this.#onRefill = options.onRefill;
    this.#replaceSurface = options.replaceSurface;
    this.#onRendererChange = options.onRendererChange;
    this.#ladder = options.recovery ?? new GpuRecoveryLadder();
  }

  /** Initializes the backend on `canvas`; every other method throws before this resolves. */
  async initialize(canvas: OffscreenCanvas): Promise<void> {
    await this.#backend.initialize(canvas);
    this.#initialized = true;
  }

  /**
   * Uploads a decoded frame, taking ownership. Frames from an earlier generation (before the last
   * seek or recovery) are closed and dropped.
   *
   * @throws Before {@link Alpha2Player.initialize} has resolved and after
   *   {@link Alpha2Player.dispose}, having closed the frame.
   */
  async uploadVideoFrame(frame: DecodedVideoFrame): Promise<void> {
    if (!this.#initialized) {
      // A refill still decoding when the player is disposed delivers here.
      frame.frame.close();
      this.#assertInitialized();
    }
    if (frame.generation !== this.#generation) {
      frame.frame.close();
      return;
    }
    try {
      await this.#backend.uploadVideoFrame(frame);
    } catch (error) {
      // The backend closed the frame; it is requested again in software after the refill.
      const request = softwareDecodeRequest(error);
      if (request === undefined || frame.decode === "software") throw error;
      const key = frameKey(request.sourceId, request.mediaTimeTicks);
      if (this.#reissued.has(key)) throw error;
      if (request.reason === "non-planar")
        this.#softwareSources.add(request.sourceId);
      this.#softwareFrames.set(key, request.reason);
      return;
    }
    this.#uploads += 1;
  }

  /**
   * Presents `timeTicks`, recovering a lost device first and refilling missing frames through
   * `onRefill`. Resolves `undefined` when a later `present` or `seek` superseded this one.
   *
   * Rejects with a fatal `GPU_DEVICE_LOST` when no renderer can be made (or a fresh surface was
   * needed without `replaceSurface`); every later presentation rejects the same way.
   *
   * @throws Before {@link Alpha2Player.initialize} has resolved.
   */
  async present(timeTicks: bigint): Promise<PresentedFrame | undefined> {
    this.#assertInitialized();
    if (this.#terminal !== undefined) throw this.#terminal;
    const request = ++this.#latestRequest;
    let reason: DecoderRefillReason = "prefetch";
    if (this.#recovering !== undefined || this.#backend.isDeviceLost()) {
      await this.#recover();
      reason = "device-recovery";
    }
    const result = await this.#presentFilled(timeTicks, request, reason);
    return request === this.#latestRequest ? result : undefined;
  }

  /**
   * Seeks to `timeTicks`, dropping frames decoded for earlier requests, and presents the target
   * once `onRefill` has delivered. Resolves `undefined` when a later request superseded it.
   *
   * @throws Before {@link Alpha2Player.initialize} has resolved.
   */
  async seek(timeTicks: bigint): Promise<PresentedFrame | undefined> {
    this.#assertInitialized();
    this.#generation += 1;
    this.#softwareFrames.clear();
    this.#reissued.clear();
    const request = ++this.#latestRequest;
    await this.#backend.reset();
    return this.#presentFilled(timeTicks, request, "seek");
  }

  /** Disposes the backend; a presentation in flight resolves `undefined`. */
  dispose(): void {
    this.#latestRequest += 1;
    this.#backend.dispose();
    this.#initialized = false;
  }

  #recover(): Promise<void> {
    this.#recovering ??= (async () => {
      try {
        await this.#climbDown();
        this.#generation += 1;
      } finally {
        this.#recovering = undefined;
      }
    })();
    return this.#recovering;
  }

  async #climbDown(): Promise<void> {
    const report = this.#backend.lossReport() ?? {
      hung: false,
      frameIndex: null,
      timeTicks: null,
      gpuMs: null,
      message: "the GPU device was lost",
    };
    let step = this.#ladder.onLoss(report, await this.#backend.probeSurface());
    for (;;) {
      let canvas: OffscreenCanvas | undefined;
      try {
        if (step.surface === "fresh") {
          if (this.#replaceSurface === undefined) {
            throw this.#fail(
              new Alpha2WasmError({
                code: "GPU_DEVICE_LOST",
                message: `${report.message}, and the canvas cannot take another renderer: give Alpha2Player a replaceSurface option`,
                fatal: true,
              }),
            );
          }
          canvas = await this.#replaceSurface();
        }
        const choice = await this.#backend.recover(step, canvas);
        if (choice !== undefined) this.#onRendererChange?.(choice);
        return;
      } catch (error) {
        if (error === this.#terminal) throw error;
        const next = this.#ladder.onRecoveryFailed(step, error);
        if (next === "terminal") {
          const message =
            error instanceof Error ? error.message : String(error);
          const terminal = new Alpha2WasmError({
            code: "GPU_DEVICE_LOST",
            message: `${report.message}, and no renderer could be made again (${message})`,
            fatal: true,
          });
          Object.defineProperty(terminal, "cause", {
            value: error,
            configurable: true,
          });
          throw this.#fail(terminal);
        }
        step = next;
      }
    }
  }

  #fail(error: Error): Error {
    this.#terminal = error;
    return error;
  }

  #assertInitialized(): void {
    if (!this.#initialized) {
      throw new Error("Alpha2Player must be initialized before use");
    }
  }

  // Re-presents after each refill while frames are missing. Each round must lack fewer frames
  // than the last, so a source that never arrives ends the loop.
  async #presentFilled(
    timeTicks: bigint,
    request: number,
    reason: DecoderRefillReason,
  ): Promise<PresentedFrame> {
    let result = await this.#backend.present(timeTicks);
    let lacking = Number.POSITIVE_INFINITY;
    while (
      result.missingFrames.length > 0 &&
      result.missingFrames.length < lacking &&
      request === this.#latestRequest
    ) {
      lacking = result.missingFrames.length;
      const uploads = this.#uploads;
      await this.#requestMissing(result, reason);
      if (uploads === this.#uploads || request !== this.#latestRequest) break;
      result = await this.#backend.present(timeTicks);
    }
    return result;
  }

  async #requestMissing(
    result: PresentedFrame,
    reason: DecoderRefillReason,
  ): Promise<void> {
    if (result.missingFrames.length === 0 || this.#onRefill === undefined)
      return;
    // Two clips of one source at one time need one decode, through the first clip's cursor.
    const requested = new Set<string>();
    const requests: DecoderRefillRequest[] = [];
    for (const {
      clipId,
      readerId,
      sourceId,
      mediaTimeTicks,
    } of result.missingFrames) {
      const key = frameKey(sourceId, mediaTimeTicks);
      if (requested.has(key)) continue;
      requested.add(key);
      const software =
        this.#softwareSources.has(sourceId) || this.#softwareFrames.has(key);
      if (software && this.#softwareFrames.delete(key)) this.#reissued.add(key);
      requests.push({
        sourceId,
        mediaTimeTicks,
        reason,
        generation: this.#generation,
        consumerId: decoderConsumerId(clipId, readerId),
        ...(software ? { decode: "software" as const } : {}),
      });
    }
    await this.#onRefill(requests);
    await this.#refillInSoftware(requests);
  }

  // An unreadable frame whose source has no software decoder fails with `FRAME_SUPPLY_FAILED`.
  async #refillInSoftware(
    requests: readonly DecoderRefillRequest[],
  ): Promise<void> {
    const again = new Map<SoftwareDecodeReason, DecoderRefillRequest[]>();
    for (const request of requests) {
      const key = frameKey(request.sourceId, request.mediaTimeTicks);
      const why = this.#softwareFrames.get(key);
      if (why === undefined) continue;
      this.#softwareFrames.delete(key);
      this.#reissued.add(key);
      const group = again.get(why) ?? [];
      group.push({ ...request, decode: "software" });
      again.set(why, group);
    }
    const nonPlanar = again.get("non-planar");
    if (nonPlanar !== undefined) await this.#onRefill?.(nonPlanar);
    const unreadable = again.get("unreadable");
    if (unreadable === undefined) return;
    try {
      await this.#onRefill?.(unreadable);
    } catch (error) {
      if (
        (error as { code?: unknown } | null)?.code !==
        "PLANAR_DECODE_UNAVAILABLE"
      )
        throw error;
      throw Object.assign(
        new Error(
          `the browser could not read back the frame of ${unreadable
            .map(({ sourceId }) => sourceId)
            .join(", ")}, and decoding it again in software failed`,
          { cause: (error as Error).cause ?? error },
        ),
        { code: "FRAME_SUPPLY_FAILED" },
      );
    }
  }
}

function frameKey(sourceId: string, mediaTimeTicks: bigint | string): string {
  return `${sourceId}\u0000${mediaTimeTicks}`;
}

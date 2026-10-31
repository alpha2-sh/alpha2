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
  /**
   * Clockwise stream display rotation applied by the GPU presentation pass, after `mirror`.
   */
  readonly rotationDegrees?: 0 | 90 | 180 | 270;
  /** Whether the stream mirrors the frame left to right before it turns. */
  readonly mirror?: boolean;
  /**
   * Where chroma samples sit, which WebCodecs does not report: see `chromaSitingForStream`.
   */
  readonly chromaSiting?: ChromaSiting;
  /** Seek/recovery generation; stale frames are closed without upload. */
  readonly generation: number;
  /**
   * Set when the frame was decoded in software because the request asked for it
   * ({@link DecoderRefillRequest.decode}); a `DecoderRefillCoordinator` sets it on what it
   * delivers. A render host that cannot take a software frame either refuses the source instead of
   * asking for it again.
   */
  readonly decode?: "software";
}

/** A source frame a presentation needed and did not have. */
export interface MissingFrame {
  readonly clipId: string;
  /**
   * The composition step that reads the source, when a media step does rather than the clip
   * itself; absent for the clip's own source. Each reader keeps its own decode cursor.
   */
  readonly readerId?: string;
  readonly sourceId: string;
  /**
   * The source time the clip needs, from the snapshot that was presented. A trimmed, offset, or
   * retimed clip needs its source at a time other than the timeline's, so this, not
   * `requestedTimeTicks`, is what to decode.
   */
  readonly mediaTimeTicks: bigint;
  /**
   * `"software"` when the render host sent this frame back for a software decode, or every frame
   * of its source comes back without planes: the refill decodes it in software
   * ({@link DecoderRefillRequest.decode}). Set by `exportToSink` on what it asks a refill for.
   */
  readonly decode?: "software";
}

/** Result of one real-time presentation attempt. */
export interface PresentedFrame {
  readonly requestedTimeTicks: bigint;
  /** The time whose picture is on screen; absent when nothing was presented. */
  readonly presentedTimeTicks?: bigint;
  /** Sources the presentation lacked a frame of. */
  readonly missingSources: readonly string[];
  /** Each clip's missing frame, with the source time to decode. */
  readonly missingFrames: readonly MissingFrame[];
  /** Whether the previous picture was shown again instead of a new one. */
  readonly repeated: boolean;
  /**
   * Whether the frame equals the one on screen, so nothing was rendered or presented: the canvas
   * still shows it.
   */
  readonly unchanged: boolean;
}

/** The worker-side renderer an {@link Alpha2Player} drives. */
export interface RenderBackend {
  /** Starts rendering into `canvas`. Called once, before any other method. */
  initialize(canvas: OffscreenCanvas): Promise<void>;
  /** Takes ownership of `frame.frame` and closes it once uploaded. */
  uploadVideoFrame(frame: DecodedVideoFrame): Promise<void>;
  /** Draws timeline time `timeTicks` with the frames uploaded so far. */
  present(timeTicks: bigint): Promise<PresentedFrame>;
  /** Drops uploaded frames ahead of a seek. */
  reset(): Promise<void>;
  /**
   * Whether the GPU device was lost and {@link RenderBackend.recover} must run before the next
   * presentation.
   */
  isDeviceLost(): boolean;
  /** What the lost renderer reported about its loss; `undefined` when it reported nothing. */
  lossReport(): GpuLossReport | undefined;
  /** The lost renderer's surface and whether the host still gets a WebGPU adapter. */
  probeSurface(): Promise<SurfaceProbe>;
  /**
   * Makes the renderer `step` names, on `canvas` for a fresh-surface step or on the current
   * surface, and resolves to the renderer it made (`undefined` for a backend that does not say).
   * Uploaded frames are gone afterwards. Rejects when the renderer cannot be made; the backend stays
   * lost.
   */
  recover(
    step: RecoveryStep,
    canvas?: OffscreenCanvas,
  ): Promise<RendererChoice | undefined>;
  /** Releases the backend; it is not used again. */
  dispose(): void;
}

/** Options of an {@link Alpha2Player}. */
export interface Alpha2PlayerOptions {
  /**
   * Called with the frames a presentation lacked; see {@link Alpha2Player}. It should resolve after
   * uploading what it decoded.
   */
  readonly onRefill?: (
    requests: readonly DecoderRefillRequest[],
  ) => void | Promise<void>;
  /**
   * Gives the player a new canvas, already transferred to an `OffscreenCanvas` at the same size, in
   * place of the one on screen. A recovery that needs a fresh surface (a lost WebGL2 context, the CPU
   * renderer after the GPU was lost) asks it once per step. Without it such a recovery fails with a
   * fatal `GPU_DEVICE_LOST`.
   */
  readonly replaceSurface?: () => Promise<OffscreenCanvas>;
  /** Called with the renderer each recovery made. */
  readonly onRendererChange?: (choice: RendererChoice) => void;
  /** The recovery ladder; one of its own by default. */
  readonly recovery?: GpuRecoveryLadder;
}

/**
 * Real-time browser runner that serializes presentation and recovers device loss.
 *
 * It keeps no clock: `present` shows the time it is given. For playback with audio, give it
 * `AudioPlaybackClock.timelineTicks`, which follows the audio device, so the picture stays with the
 * sound instead of drifting on the wall clock.
 *
 * A presentation that lacks source frames asks `onRefill` for them and, once the frames it uploads
 * have arrived, presents the same time again, so the time asked for is the one that shows.
 * `onRefill` should resolve after uploading what it decoded: frames uploaded later reach the screen
 * only with the next presentation.
 *
 * A frame the backend sends back for a software decode (`SOFTWARE_DECODE_REQUIRED`: no planar
 * format the engine reads, or planes the browser could not read back) is asked of `onRefill` again,
 * once, with `decode: "software"`; a source whose frames have no planar format is asked for in
 * software from then on. `onRefill` must honor `decode` (a `DecoderRefillCoordinator` does).
 *
 * A lost GPU is recovered on the next presentation by a {@link GpuRecoveryLadder}: the GPU again for
 * a first external loss, the CPU renderer once the loss was the work's own or the page has lost the
 * GPU twice. A recovery onto a fresh surface takes a canvas from `replaceSurface`. When every rung
 * fails the player is done: that presentation and every later one reject with the same fatal error,
 * without calling the backend.
 */
export class Alpha2Player {
  readonly #backend: RenderBackend;
  #initialized = false;
  #latestRequest = 0;
  #generation = 0;
  /**
   * Frames handed to the backend, so a refill that uploaded nothing is not followed by a
   * presentation that cannot differ.
   */
  #uploads = 0;
  /**
   * The device recovery in flight, which every presentation that finds the device lost waits for
   * instead of starting its own: one recovery makes the renderer again, however many presentations
   * overlap it.
   */
  #recovering?: Promise<void>;
  /** Set when recovery ran out of rungs; every later presentation rejects with it. */
  #terminal?: Error;
  readonly #replaceSurface?: () => Promise<OffscreenCanvas>;
  readonly #onRendererChange?: (choice: RendererChoice) => void;
  readonly #ladder: GpuRecoveryLadder;
  readonly #onRefill?: (
    requests: readonly DecoderRefillRequest[],
  ) => void | Promise<void>;
  /** Sources whose frames came back without planes: asked for in software from then on. */
  readonly #softwareSources = new Set<string>();
  /** Frames the backend sent back for a software decode, by {@link frameKey}, not yet asked for. */
  readonly #softwareFrames = new Map<string, SoftwareDecodeReason>();
  /** Frames already asked for again in software since the last seek: each is asked for once. */
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
   * Uploads a decoded frame, taking it over: it is closed when it is not uploaded. A frame from an
   * earlier generation (decoded before the last seek or device recovery) is closed and dropped.
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
      // The backend closed the frame; the picture is asked for again in software after the refill.
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
   * Presents `timeTicks`, recovering a lost device first and refilling missing
   * frames through `onRefill`. Resolves `undefined` when a later `present` or
   * `seek` superseded this one.
   *
   * Presentations that overlap a recovery wait for it rather than starting another, and reject
   * with its error when it fails. A recovery fails only when no renderer could be made at all (or a
   * fresh surface was needed without `replaceSurface`): it rejects with a fatal `GPU_DEVICE_LOST`,
   * as every later presentation does.
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
   * Seeks to `timeTicks`: frames decoded for earlier requests are dropped when
   * they arrive, the backend is reset, and the target is presented once
   * `onRefill` has delivered its frames. Resolves `undefined` when a later
   * request superseded it.
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

  /** Joins the recovery in flight, or starts one; the generation moves once per recovery. */
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

  /** Runs the ladder from the loss down until a renderer is made, or stores the terminal error. */
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

  /**
   * Presents `timeTicks`, and again after each refill that uploaded something while frames are
   * missing. Each round must lack fewer frames than the last, so a source that never arrives ends
   * it; a newer request ends it too, since that one will be shown instead.
   */
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
    // Two clips of one source at one time need one decode; each reads through its own cursor,
    // which the first of them names.
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

  /**
   * Asks once more, in software, for each of `requests` whose frame the backend sent back during
   * the refill. A frame sent back because the browser could not read it fails as an unrecovered
   * frame when the source has no software decoder (`FRAME_SUPPLY_FAILED`); only a frame without
   * planes makes the source one the browser cannot hand over.
   */
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

/** One source frame, by source and source time. */
function frameKey(sourceId: string, mediaTimeTicks: bigint | string): string {
  return `${sourceId}\u0000${mediaTimeTicks}`;
}

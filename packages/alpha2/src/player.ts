import type { ChromaSiting } from "./hdr-frames.js";
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
  /** Replaces a lost device; uploaded frames are gone afterwards. */
  recover(): Promise<void>;
  /** Releases the backend; it is not used again. */
  dispose(): void;
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
  readonly #onRefill?: (
    requests: readonly DecoderRefillRequest[],
  ) => void | Promise<void>;

  constructor(
    backend: RenderBackend,
    onRefill?: (
      requests: readonly DecoderRefillRequest[],
    ) => void | Promise<void>,
  ) {
    this.#backend = backend;
    this.#onRefill = onRefill;
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
    await this.#backend.uploadVideoFrame(frame);
    this.#uploads += 1;
  }

  /**
   * Presents `timeTicks`, recovering a lost device first and refilling missing
   * frames through `onRefill`. Resolves `undefined` when a later `present` or
   * `seek` superseded this one.
   *
   * Presentations that overlap a recovery wait for it rather than starting another, and reject
   * with its error when it fails; the next presentation after a failure tries again.
   *
   * @throws Before {@link Alpha2Player.initialize} has resolved.
   */
  async present(timeTicks: bigint): Promise<PresentedFrame | undefined> {
    this.#assertInitialized();
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
        await this.#backend.recover();
        this.#generation += 1;
      } finally {
        this.#recovering = undefined;
      }
    })();
    return this.#recovering;
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
      const key = `${sourceId}\u0000${mediaTimeTicks}`;
      if (requested.has(key)) continue;
      requested.add(key);
      requests.push({
        sourceId,
        mediaTimeTicks,
        reason,
        generation: this.#generation,
        consumerId: decoderConsumerId(clipId, readerId),
      });
    }
    await this.#onRefill(requests);
  }
}

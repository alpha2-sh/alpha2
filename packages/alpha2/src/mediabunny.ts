import { type ByteReader, readAudioSpeakers } from "./audio-layout.js";
import { type ChromaSiting, chromaSitingForStream } from "./chroma-siting.js";
import { decoderOrderKey, decoderPlaneOrder } from "./decoder-order.js";
import {
  ALL_FORMATS,
  AudioSample,
  AudioSampleSink,
  AudioSampleSource,
  BlobSource,
  BufferTarget,
  BufferSource,
  canEncodeAudio,
  canEncodeVideo,
  StreamTarget,
  EncodedPacketSink,
  Input,
  type InputTrack,
  type InputVideoTrack,
  Mp4OutputFormat,
  Output,
  Quality,
  VideoSample,
  VideoSampleSink,
  VideoSampleSource,
  type Source,
} from "mediabunny";

import {
  type AudioFrameGrid,
  audioFrameGrid,
  MAX_AUDIO_CHANNELS,
  microsecondsToTicks,
  placeOnAudioFrameGrid,
  presentationSlackTicks,
  readMatroskaCodecDelay,
  readVideoOrientation,
  roundingQuantum,
  ROUNDING_SCAN_BYTES,
  ROUNDING_SCAN_FRAMES,
  validateStallTimeout,
} from "./media.js";
import type {
  AudioDecoderRefillRequest,
  AudioSpeaker,
  BrowserDecodedFrame,
  BrowserAudioBlock,
  BrowserAudioDecoder,
  BrowserVideoDecoder,
  BrowserVideoDecoderOptions,
  DecoderRefillRequest,
  OptionalMediaAdapterFactory,
  VideoOrientation,
} from "./media.js";
import type { BrowserAudioBlockSink } from "./audio.js";
import type { BrowserEncodeSink, ExportedFrame } from "./exporter.js";
import type { VideoRateControl } from "./rate-control.js";
import type { PositionedWrite } from "./export-destination.js";
import { ExportFailureError } from "./export-failure.js";

/**
 * How far past the decode cursor a request may be (2 s) and still be reached by decoding forward;
 * a request further ahead seeks. Also the longest step back that continues a backward run.
 */
const MAX_SEQUENTIAL_DECODE_GAP_TICKS = 240_000n;
const DEFAULT_MAX_DECODED_FRAME_BYTES = 64 * 1024 * 1024;
/**
 * Decoded frames the forward window keeps at most: the two that answering a time takes, the frame
 * shown at it (the last at or before its deadline, the time plus the slack) and the one after.
 *
 * Each holds one of the decoder's output surfaces, and a hardware decoder has a fixed pool of them.
 * Chromium's Windows decoder allocates its DPB size plus six, and the DPB can occupy all of its own
 * share: a stream without a VUI is reordered by the whole DPB, and long-term references stay in it
 * for a whole GOP. That leaves about five surfaces for everything outside the decoder; a window of
 * eight holds them all, and the decoder waits, without an error, for one to come back.
 */
const DEFAULT_MAX_CACHED_FRAMES = 2;
/**
 * The fewest frames the forward window works with, and the fewest its byte limit evicts down to:
 * the frame at or before a time and the one after it, which proves that frame is still presented.
 */
const MIN_CACHED_FRAMES = 2;
/** How long reading one frame may take before the decoder is taken to be stuck. */
const DEFAULT_STALL_TIMEOUT_MS = 5_000;
/** How long the video encoder may hold frames without output before it is taken to be stuck. */
const DEFAULT_ENCODER_STALL_TIMEOUT_MS = 10_000;
/** Bytes of copied frames the reverse window keeps by default: about forty 1080p frames. */
const DEFAULT_MAX_REVERSE_FRAME_BYTES = 128 * 1024 * 1024;
const TICKS_PER_SECOND = 120_000;

/** Inputs supported by the optional pinned mediabunny adapter. */
export type MediabunnyInput = Blob | ArrayBuffer | ArrayBufferView;

/** Limits for {@link MediabunnyVideoDecoder.open}, on top of the shared decoder options. */
export interface MediabunnyVideoDecoderOptions extends BrowserVideoDecoderOptions {
  /**
   * Bytes of frames the reverse window keeps (default 128 MiB). They are copies, so they hold none
   * of the decoder's surfaces; a larger window decodes each keyframe interval of a backward run
   * fewer times.
   */
  readonly maxReverseFrameBytes?: number;
  /**
   * The decoder the browser is asked for (default `"no-preference"`, which lets it pick hardware).
   * Whichever it picks, a decoder that fails is replaced once by a software decoder: see {@link
   * MediabunnyVideoDecoder.refill}.
   */
  readonly hardwareAcceleration?: HardwareAcceleration;
}

/**
 * A `VideoDecoder` failure, as WebCodecs reports one: an undecodable stream or a decoder that could
 * not be configured or broke. Aborts, stalls, and the stream ending are not.
 */
function isDecoderFailure(error: unknown): boolean {
  return (
    error instanceof DOMException &&
    ["EncodingError", "NotSupportedError", "OperationError"].includes(
      error.name,
    )
  );
}

/**
 * Optional demux/decode adapter implementing alpha2's public decoder contract.
 *
 * Forward requests share one sequential decode. A request earlier than the last one, or far past
 * it, seeks: decoding restarts at the keyframe before it, and the frames it passes go through a
 * window of the last two decoder frames. A decoder that produces no frame for the stall timeout
 * fails the request instead of holding it, and every request after it, forever. When requests keep
 * stepping back (a reversed clip, a ping-pong loop's backward pass), the next miss decodes a
 * reverse window instead: the frames up to the requested time, from the keyframe before it, copied
 * out of the decoder so that dozens can be kept without stalling it. The run then plays from that
 * window, and each keyframe interval is decoded about `interval / window` times rather than once
 * per frame.
 */
export class MediabunnyVideoDecoder implements BrowserVideoDecoder {
  readonly #sourceId: string;
  readonly #input: Input;
  readonly #track: InputVideoTrack;
  #sink: VideoSampleSink;
  #hardwareAcceleration: HardwareAcceleration;
  #softwareFallback?: string;
  readonly #packets: EncodedPacketSink;
  readonly #chromaSiting: ChromaSiting;
  readonly #stallTimeoutMs: number;
  #iterator?: AsyncIterator<VideoSample>;
  #generation?: number;
  /**
   * The last time answered, from a window or by decoding: where a backward run is measured from.
   */
  #lastTargetTicks?: bigint;
  /**
   * The last time the forward decode answered: the iterator has read up to the frame after it. A
   * request at or before it, or far past it, seeks.
   */
  #cursorTicks?: bigint;
  /**
   * When the stream's first frame is presented, once a seek from the first keyframe has shown it to
   * be later than the time sought.
   */
  #leadTicks?: bigint;
  /** The cursor sought from the stream's first keyframe and has read no frame since. */
  #headSinceSeek = false;
  /** Requests in a row that were each earlier than the one before. */
  #backwardSteps = 0;
  #closed = false;
  /** How the track is shown upright, as the container says. */
  readonly #orientation: VideoOrientation;
  readonly #cache: MediabunnyFrameWindow;
  readonly #reverse: ReverseFrameWindow;
  /**
   * How late a frame may be and still be the one shown at a time, from the track's time resolution
   * ({@link presentationSlackTicks}).
   */
  readonly #slack: bigint;
  #reverseCacheHits = 0;
  #reverseWindowsBuilt = 0;

  private constructor(
    sourceId: string,
    input: Input,
    track: InputVideoTrack,
    hardwareAcceleration: HardwareAcceleration,
    packets: EncodedPacketSink,
    chromaSiting: ChromaSiting,
    orientation: VideoOrientation,
    slack: bigint,
    limits: Required<
      Pick<
        MediabunnyVideoDecoderOptions,
        | "maxCachedFrameBytes"
        | "maxCachedFrames"
        | "maxReverseFrameBytes"
        | "stallTimeoutMs"
      >
    >,
  ) {
    this.#sourceId = sourceId;
    this.#input = input;
    this.#track = track;
    this.#hardwareAcceleration = hardwareAcceleration;
    this.#sink = new VideoSampleSink(track, { hardwareAcceleration });
    this.#packets = packets;
    this.#chromaSiting = chromaSiting;
    this.#orientation = orientation;
    this.#stallTimeoutMs = limits.stallTimeoutMs;
    this.#slack = slack;
    this.#cache = new MediabunnyFrameWindow(
      limits.maxCachedFrameBytes,
      limits.maxCachedFrames,
      slack,
    );
    this.#reverse = new ReverseFrameWindow(limits.maxReverseFrameBytes, slack);
  }

  /**
   * Opens `resource`'s primary video track. Rejects when it has none, or when this browser cannot
   * decode it, so a host can fall back before the first request.
   */
  static async open(
    sourceId: string,
    resource: MediabunnyInput,
    options: MediabunnyVideoDecoderOptions = {},
  ): Promise<MediabunnyVideoDecoder> {
    const limits = {
      maxCachedFrameBytes:
        options.maxCachedFrameBytes ?? DEFAULT_MAX_DECODED_FRAME_BYTES,
      maxCachedFrames: options.maxCachedFrames ?? DEFAULT_MAX_CACHED_FRAMES,
      maxReverseFrameBytes:
        options.maxReverseFrameBytes ?? DEFAULT_MAX_REVERSE_FRAME_BYTES,
      stallTimeoutMs: options.stallTimeoutMs ?? DEFAULT_STALL_TIMEOUT_MS,
    };
    for (const [name, value] of Object.entries(limits)) {
      if (!Number.isSafeInteger(value) || value <= 0)
        throw new RangeError(`${name} must be a positive safe integer`);
    }
    if (limits.maxCachedFrames < MIN_CACHED_FRAMES)
      throw new RangeError(
        `maxCachedFrames must be at least ${MIN_CACHED_FRAMES}`,
      );
    validateStallTimeout(limits.stallTimeoutMs);
    const source: Source =
      resource instanceof Blob
        ? new BlobSource(resource)
        : new BufferSource(resource);
    const input = new Input({ source, formats: ALL_FORMATS });
    try {
      const track = await input.getPrimaryVideoTrack();
      if (track === null)
        throw new Error("mediabunny input has no video track");
      if (!(await track.canDecode()))
        throw new Error("this browser cannot decode the video track");
      // mediabunny reports a turn but not a mirror, and reads a mirroring matrix's angle otherwise
      // than native decoding does.
      const orientation = (await readVideoOrientation(resource, track.id)) ?? {
        rotationDegrees: track.rotation,
        mirror: false,
      };
      const packets = new EncodedPacketSink(track);
      return new MediabunnyVideoDecoder(
        sourceId,
        input,
        track,
        options.hardwareAcceleration ?? "no-preference",
        packets,
        await streamChromaSiting(track, packets),
        orientation,
        await trackPresentationSlack(track, packets),
        limits,
      );
    } catch (error) {
      input.dispose();
      throw error;
    }
  }

  /** Bytes both windows hold: decoder frames and reverse copies. */
  get cachedFrameBytes(): number {
    return this.#cache.bytes + this.#reverse.bytes;
  }

  /** Requests answered from a window without decoding. */
  get reverseCacheHits(): number {
    return this.#reverseCacheHits;
  }

  /** Reverse windows decoded: one per stretch of a backward run. */
  get reverseWindowsBuilt(): number {
    return this.#reverseWindowsBuilt;
  }

  /**
   * Why the decoder moved to software decoding, when the one the browser picked first failed;
   * `undefined` while that one serves.
   */
  get softwareFallback(): string | undefined {
    return this.#softwareFallback;
  }

  /**
   * The frame presented at the request's time.
   *
   * A decoder the browser picks can fail on streams it claims to decode (Chromium's VA-API decoders
   * on Linux report `EncodingError: Decoding error.`), so a decoder failure replaces it, once, with
   * a software decoder, which decodes the request again from its keyframe. A software decoder that
   * fails too rejects the request.
   */
  async refill(
    request: DecoderRefillRequest,
    signal?: AbortSignal,
  ): Promise<BrowserDecodedFrame> {
    try {
      return await this.#refill(request, signal);
    } catch (error) {
      if (
        this.#closed ||
        this.#hardwareAcceleration === "prefer-software" ||
        signal?.aborted === true ||
        !isDecoderFailure(error)
      )
        throw error;
      await this.#resetIterator();
      this.#cache.clear();
      this.#reverse.clear();
      this.#backwardSteps = 0;
      this.#hardwareAcceleration = "prefer-software";
      this.#softwareFallback = `${(error as DOMException).name}: ${(error as DOMException).message}`;
      this.#sink = new VideoSampleSink(this.#track, {
        hardwareAcceleration: "prefer-software",
      });
      return this.#refill(request, signal);
    }
  }

  async #refill(
    request: DecoderRefillRequest,
    signal?: AbortSignal,
  ): Promise<BrowserDecodedFrame> {
    if (this.#closed) throw new Error("mediabunny decoder is closed");
    if (request.sourceId !== this.#sourceId) {
      throw new Error(`decoder source mismatch: ${request.sourceId}`);
    }
    throwIfAborted(signal);
    if (
      this.#generation !== undefined &&
      this.#generation !== request.generation
    ) {
      await this.#resetIterator();
      this.#cache.clear();
      this.#reverse.clear();
      this.#backwardSteps = 0;
    }
    // Requests before the stream's first frame all present that frame.
    const target =
      this.#leadTicks !== undefined && request.mediaTimeTicks < this.#leadTicks
        ? this.#leadTicks
        : request.mediaTimeTicks;
    const last = this.#lastTargetTicks;
    const backward = last !== undefined && target < last;
    // Only a step back as short as a sequential step forward continues a run: a longer jump, such
    // as scrubbing, lands beyond any window the last one left.
    if (last !== undefined && target !== last)
      this.#backwardSteps =
        backward && last - target <= MAX_SEQUENTIAL_DECODE_GAP_TICKS
          ? this.#backwardSteps + 1
          : 0;
    const answer = (presented: PresentedFrame): BrowserDecodedFrame => ({
      sourceId: this.#sourceId,
      requestedTimeTicks: request.mediaTimeTicks,
      decodedTimeTicks: ticksFromVideoFrame(presented.frame),
      generation: request.generation,
      frame: presented.frame,
      rotationDegrees: this.#orientation.rotationDegrees,
      mirror: this.#orientation.mirror,
      chromaSiting: this.#chromaSiting,
    });
    const windowed =
      this.#reverse.presentedAt(target) ?? this.#cache.presentedAt(target);
    if (windowed !== undefined) {
      // A hit moves no cursor: the forward decode is still where it last answered, and a step past
      // this time from there may have to seek.
      this.#lastTargetTicks = target;
      this.#reverseCacheHits += 1;
      return answer(windowed);
    }
    const seek =
      request.reason === "seek" || request.reason === "device-recovery";
    // Two short steps back in a row is a backward run; one is a seek, such as a loop's wrap.
    if (!seek && this.#backwardSteps >= 2) {
      await this.#fillReverseWindow(target, signal);
      this.#generation = request.generation;
      this.#lastTargetTicks = target;
      const reversed = this.#reverse.presentedAt(target);
      if (reversed !== undefined) return answer(reversed);
    }
    const mustSeek =
      seek ||
      this.#iterator === undefined ||
      this.#generation !== request.generation ||
      this.#cursorTicks === undefined ||
      target <= this.#cursorTicks ||
      target - this.#cursorTicks > MAX_SEQUENTIAL_DECODE_GAP_TICKS;
    if (mustSeek) {
      await this.#resetIterator();
      this.#cache.clear();
      // Decode from the keyframe at or before the deadline, so each read is one frame's work and
      // the stall timeout measures the decoder rather than the GOP length.
      const start = await this.#keyframeStart(target + this.#slack);
      this.#iterator = this.#sink
        .samples(start.seconds)
        [Symbol.asyncIterator]();
      this.#generation = request.generation;
      this.#headSinceSeek = start.head;
    }
    this.#lastTargetTicks = target;
    this.#cursorTicks = target;
    // The frame shown is the last one at or before the deadline; knowing it takes the frame after,
    // which the window keeps for the next request.
    for (;;) {
      const iterator = this.#iterator;
      if (iterator === undefined) {
        throw new RangeError(
          "mediabunny source ended before the requested frame",
        );
      }
      let next: IteratorResult<VideoSample>;
      try {
        next = await this.#nextSample(iterator, signal);
      } catch (error) {
        this.#iterator = undefined;
        this.#generation = undefined;
        this.#lastTargetTicks = undefined;
        this.#cursorTicks = undefined;
        this.#cache.clear();
        throw error;
      }
      if (next.done) {
        this.#iterator = undefined;
        // Every decoded frame is in the window, so its newest is the stream's last.
        this.#cache.markEnd();
        const presented = this.#cache.presentedAt(target, true);
        if (presented !== undefined) return answer(presented);
        throw new RangeError(
          "mediabunny source ended before the requested frame",
        );
      }
      const sample = next.value;
      try {
        throwIfAborted(signal);
        const frame = sample.toVideoFrame();
        const ticks = ticksFromVideoFrame(frame);
        this.#cache.add(frame);
        frame.close();
        const head = this.#headSinceSeek;
        this.#headSinceSeek = false;
        const presented = this.#cache.presentedAt(target);
        if (presented !== undefined) return answer(presented);
        if (ticks > target + this.#slack) {
          // A late first frame from the stream's first keyframe means the stream starts there and
          // answers every earlier time; from any other keyframe it is a bad key index and answers
          // this request only.
          if (head) this.#leadTicks = ticks;
          const first = this.#cache.presentedAt(ticks);
          if (first !== undefined) return answer(first);
        }
      } finally {
        sample.close();
      }
    }
  }

  /** Drops the decode cursor and both windows; the next request seeks. */
  async flush(): Promise<void> {
    await this.#resetIterator();
    this.#cache.clear();
    this.#reverse.clear();
    this.#backwardSteps = 0;
  }

  /** Releases the decoder, both windows, and the input. A later `refill` rejects. */
  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    const closing = this.#iterator?.return?.();
    void closing?.catch(() => {});
    this.#iterator = undefined;
    this.#cache.clear();
    this.#reverse.clear();
    this.#input.dispose();
  }

  /**
   * Decodes the reverse window for `target`: from the keyframe before it, forward to the first
   * frame after its deadline (`target` plus the presentation slack), copying the frames the window
   * can hold and releasing every decoder frame as soon as it is read.
   *
   * The previous window goes first: its frames are later than the target, and a backward run has
   * passed them. The forward cursor goes too; a later forward request seeks.
   */
  async #fillReverseWindow(
    target: bigint,
    signal?: AbortSignal,
  ): Promise<void> {
    await this.#resetIterator();
    this.#cache.clear();
    this.#reverse.clear();
    const iterator = this.#sink
      .samples(await this.#keyframeSeconds(target))
      [Symbol.asyncIterator]();
    // Frames before this time are decoded but not copied: the window could not keep them.
    let floor: bigint | undefined;
    try {
      for (;;) {
        throwIfAborted(signal);
        const next = await this.#nextSample(iterator, signal);
        if (next.done) {
          this.#reverse.seal();
          break;
        }
        const sample = next.value;
        try {
          const ticks = microsecondsToTicks(sample.microsecondTimestamp);
          if (ticks > target + this.#slack) {
            this.#reverse.end(ticks);
            break;
          }
          floor ??= target - this.#reverse.span(sample);
          if (ticks >= floor) await this.#reverse.add(sample, ticks);
        } finally {
          sample.close();
        }
      }
    } finally {
      await iterator.return?.();
    }
    this.#reverseWindowsBuilt += 1;
  }

  /**
   * The iterator's next frame, or a rejection when the decoder produces none within the stall
   * timeout or `signal` aborts.
   *
   * A hardware decoder whose output surfaces are all held waits for one to come back and reports
   * nothing, so without a limit the read, and every request queued behind it, waits forever. On
   * either rejection the iterator is ended, which stops its decoder and releases what it holds; a
   * frame the read delivers after that is closed.
   */
  async #nextSample(
    iterator: AsyncIterator<VideoSample>,
    signal?: AbortSignal,
  ): Promise<IteratorResult<VideoSample>> {
    const pending = iterator.next();
    let timer: ReturnType<typeof setTimeout> | undefined;
    let abort: (() => void) | undefined;
    const interrupted = new Promise<never>((_, reject) => {
      timer = setTimeout(
        () =>
          reject(
            new Error(
              `video decoder ${this.#sourceId} stalled: no frame for ${this.#stallTimeoutMs} ms`,
            ),
          ),
        this.#stallTimeoutMs,
      );
      if (signal === undefined) return;
      abort = () =>
        reject(
          signal.reason instanceof Error
            ? signal.reason
            : new DOMException("Aborted", "AbortError"),
        );
      signal.addEventListener("abort", abort, { once: true });
    });
    try {
      return await Promise.race([pending, interrupted]);
    } catch (error) {
      void pending.then(
        (late) => {
          if (!late.done) late.value.close();
        },
        () => {},
      );
      void iterator.return?.()?.catch(() => {});
      throw error;
    } finally {
      clearTimeout(timer);
      if (abort !== undefined) signal?.removeEventListener("abort", abort);
    }
  }

  /** The time of the keyframe decoding must start from to reach `target`. */
  async #keyframeSeconds(target: bigint): Promise<number> {
    return (await this.#keyframeStart(target)).seconds;
  }

  /**
   * {@link #keyframeSeconds}, and whether that keyframe is the stream's first because none is at or
   * before `target`.
   */
  async #keyframeStart(
    target: bigint,
  ): Promise<{ readonly seconds: number; readonly head: boolean }> {
    const seconds = Number(target) / TICKS_PER_SECOND;
    const key = await this.#packets.getKeyPacket(seconds, {
      verifyKeyPackets: true,
    });
    if (key !== null)
      return { seconds: Math.min(seconds, key.timestamp), head: false };
    const first = await this.#packets.getFirstKeyPacket({
      verifyKeyPackets: true,
    });
    return {
      seconds: Math.min(seconds, first?.timestamp ?? seconds),
      head: true,
    };
  }

  async #resetIterator(): Promise<void> {
    await this.#iterator?.return?.();
    this.#iterator = undefined;
    this.#generation = undefined;
    this.#lastTargetTicks = undefined;
    this.#cursorTicks = undefined;
    this.#headSinceSeek = false;
  }
}

/** A frame a window answers with. */
interface PresentedFrame {
  readonly frame: VideoFrame;
}

interface MediabunnyCachedFrame extends PresentedFrame {
  readonly ticks: bigint;
  readonly bytes: number;
}

class MediabunnyFrameWindow {
  readonly #limit: number;
  readonly #maxFrames: number;
  readonly #slack: bigint;
  /**
   * The stream's last frame, once decoding reached the stream's end: past it no frame comes, so
   * that frame answers every later time without a decode. A property of the stream, kept when the
   * window is cleared; it applies while that frame is the newest one held.
   */
  #endTicks?: bigint;
  #entries: MediabunnyCachedFrame[] = [];
  #bytes = 0;

  constructor(limit: number, maxFrames: number, slack: bigint) {
    this.#limit = limit;
    this.#maxFrames = maxFrames;
    this.#slack = slack;
  }

  get bytes(): number {
    return this.#bytes;
  }

  add(frame: VideoFrame): void {
    const ticks = ticksFromVideoFrame(frame);
    const existing = this.#entries.findIndex((entry) => entry.ticks === ticks);
    const clone = frame.clone();
    const bytes = mediabunnyVideoFrameBytes(clone);
    if (existing >= 0) {
      const [replaced] = this.#entries.splice(existing, 1);
      if (replaced !== undefined) {
        this.#bytes -= replaced.bytes;
        replaced.frame.close();
      }
    }
    this.#entries.push({ ticks, frame: clone, bytes });
    this.#entries.sort((left, right) =>
      left.ticks < right.ticks ? -1 : left.ticks > right.ticks ? 1 : 0,
    );
    this.#bytes += bytes;
    while (
      (this.#bytes > this.#limit && this.#entries.length > MIN_CACHED_FRAMES) ||
      this.#entries.length > this.#maxFrames
    ) {
      const evicted = this.#entries.shift();
      if (evicted === undefined) break;
      this.#bytes -= evicted.bytes;
      evicted.frame.close();
    }
  }

  /** Records that the newest frame held is the stream's last: see {@link #endTicks}. */
  markEnd(): void {
    this.#endTicks = this.#entries.at(-1)?.ticks;
  }

  /**
   * A clone of the frame presented at `target`, the last one at or before its deadline (the target
   * plus the slack), or `undefined` when the window cannot tell: nothing at or before the deadline,
   * or only the newest frame before the target with no later frame to show that it is still
   * presented. `final` says no later frame is coming, which the window knows itself when its newest
   * frame is the stream's last.
   */
  presentedAt(target: bigint, final = false): PresentedFrame | undefined {
    const deadline = target + this.#slack;
    let index = -1;
    for (const [position, entry] of this.#entries.entries()) {
      if (entry.ticks > deadline) break;
      index = position;
    }
    const entry = this.#entries[index];
    if (entry === undefined) return undefined;
    // A frame from the target to its deadline is the one shown; one before it is, unless a later
    // frame still comes by the deadline.
    if (
      entry.ticks < target &&
      index === this.#entries.length - 1 &&
      !final &&
      entry.ticks !== this.#endTicks
    )
      return undefined;
    return { frame: entry.frame.clone() };
  }

  clear(): void {
    for (const entry of this.#entries.splice(0)) entry.frame.close();
    this.#bytes = 0;
  }
}

interface ReverseEntry extends PresentedFrame {
  /** When the frame is presented from. */
  readonly start: bigint;
  /**
   * When the next frame is presented from: `undefined` until that frame is read, `null` when none
   * follows.
   */
  end: bigint | null | undefined;
  readonly bytes: number;
}

/**
 * Copies of consecutive decoded frames, each answering for its presentation interval.
 *
 * Unlike the forward window, it answers from intervals it has proven rather than from neighbours,
 * and its frames belong to it rather than to the decoder. It evicts the earliest frames first: a
 * backward run reaches them last.
 */
class ReverseFrameWindow {
  readonly #limit: number;
  readonly #slack: bigint;
  #entries: ReverseEntry[] = [];
  #bytes = 0;

  constructor(limit: number, slack: bigint) {
    this.#limit = limit;
    this.#slack = slack;
  }

  get bytes(): number {
    return this.#bytes;
  }

  /**
   * How far before the requested time the window can reach from frames like `sample`: its byte
   * limit in frames, times the frame's duration. Frames earlier than that would be evicted before
   * the window is filled.
   */
  span(sample: VideoSample): bigint {
    const bytes = sampleBytes(sample);
    const frames = Math.max(1, Math.floor(this.#limit / bytes));
    // A stream that does not say how long its frames last is taken as 30 fps; a wrong guess costs
    // copies or windows, never a wrong frame.
    const duration =
      sample.microsecondDuration > 0 ? sample.microsecondDuration : 33_333;
    return microsecondsToTicks(frames * duration);
  }

  /**
   * Keeps a copy of `sample`, presented from `ticks` until a later frame is added, ended, or
   * sealed.
   */
  async add(sample: VideoSample, ticks: bigint): Promise<void> {
    const decoded = sample.toVideoFrame();
    let frame: VideoFrame;
    try {
      frame = await detachVideoFrame(decoded);
    } finally {
      decoded.close();
    }
    this.end(ticks);
    const bytes = ownedFrameBytes(frame);
    this.#entries.push({
      start: ticks,
      end: undefined,
      frame,
      bytes,
    });
    this.#bytes += bytes;
    while (this.#bytes > this.#limit && this.#entries.length > 1) {
      const evicted = this.#entries.shift();
      if (evicted === undefined) break;
      this.#bytes -= evicted.bytes;
      evicted.frame.close();
    }
  }

  /** The frame after the newest one is presented from `ticks`. */
  end(ticks: bigint): void {
    const newest = this.#entries.at(-1);
    if (newest !== undefined && newest.end === undefined) newest.end = ticks;
  }

  /** No frame follows the newest: it answers however late a time is. */
  seal(): void {
    const newest = this.#entries.at(-1);
    if (newest !== undefined && newest.end === undefined) newest.end = null;
  }

  /**
   * A clone of the frame whose presentation interval holds `target`'s deadline: the frame shown at
   * `target`.
   */
  presentedAt(target: bigint): PresentedFrame | undefined {
    const deadline = target + this.#slack;
    const entry = this.#entries.find(
      (candidate) =>
        candidate.start <= deadline &&
        candidate.end !== undefined &&
        (candidate.end === null || deadline < candidate.end),
    );
    return entry === undefined ? undefined : { frame: entry.frame.clone() };
  }

  clear(): void {
    for (const entry of this.#entries.splice(0)) entry.frame.close();
    this.#bytes = 0;
  }
}

/**
 * How late a frame of `track` may be and still be the one shown at a time ({@link
 * presentationSlackTicks}): from its time resolution and the rounding its first packets' times
 * show, as a native host reads the same file. A track whose resolution mediabunny cannot say is
 * held to {@link PRESENTATION_SLACK_TICKS}.
 */
async function trackPresentationSlack(
  track: InputVideoTrack,
  packets: EncodedPacketSink,
): Promise<bigint> {
  const first = await firstPacketTimes(track, packets);
  if (first === undefined) return presentationSlackTicks();
  return presentationSlackTicks(
    first.resolution,
    roundingQuantum(first.timestamps),
  );
}

/** The pre-skip, in 48 kHz samples, an `OpusHead` decoder configuration declares; 0 for none. */
function opusPreSkip(description: Uint8Array | null): number {
  if (
    description === null ||
    description.length < 12 ||
    new TextDecoder().decode(description.subarray(0, 8)) !== "OpusHead"
  )
    return 0;
  return description[10] | (description[11] << 8);
}

/**
 * `track`'s time resolution and its first packets' timestamps in whole units of it, up to {@link
 * ROUNDING_SCAN_FRAMES} of them or {@link ROUNDING_SCAN_BYTES}, as a native host reads the same
 * file; `undefined` when mediabunny cannot say the resolution.
 */
async function firstPacketTimes(
  track: InputTrack,
  packets: EncodedPacketSink,
): Promise<{ resolution: number; timestamps: number[] } | undefined> {
  const resolution = await track.getTimeResolution().catch(() => undefined);
  if (resolution === undefined) return undefined;
  const timestamps: number[] = [];
  let bytes = 0;
  let packet = await packets.getFirstPacket({ metadataOnly: true });
  while (
    packet !== null &&
    timestamps.length < ROUNDING_SCAN_FRAMES &&
    bytes < ROUNDING_SCAN_BYTES
  ) {
    timestamps.push(Math.round(packet.timestamp * resolution));
    bytes += packet.byteLength;
    packet = await packets.getNextPacket(packet, { metadataOnly: true });
  }
  return { resolution, timestamps };
}

/**
 * Where `track`'s chroma samples sit, from its decoder configuration's parameter sets or, when it
 * carries none, its first packet's.
 */
async function streamChromaSiting(
  track: InputVideoTrack,
  packets: EncodedPacketSink,
): Promise<ChromaSiting> {
  const config = await track.getDecoderConfig().catch(() => null);
  const description = config?.description ?? undefined;
  const first =
    description === undefined
      ? await packets.getFirstPacket().catch(() => null)
      : null;
  return chromaSitingForStream(
    config?.codec ?? track.codec,
    description,
    first?.data,
  );
}

/**
 * A copy of `frame` that holds none of the decoder's resources.
 *
 * A frame with a pixel format is copied into memory the copy owns; an opaque one (a GPU texture
 * some platforms decode to) is drawn into a bitmap instead.
 */
async function detachVideoFrame(frame: VideoFrame): Promise<VideoFrame> {
  const format = frame.format;
  if (format !== null) {
    try {
      const rect = frame.visibleRect ?? {
        x: 0,
        y: 0,
        width: frame.codedWidth,
        height: frame.codedHeight,
      };
      const buffer = new ArrayBuffer(frame.allocationSize());
      const layout = await frame.copyTo(buffer);
      // `transfer` hands the buffer over instead of copying it again where the browser supports it;
      // the DOM typings do not know it yet.
      const init: VideoFrameBufferInit & { transfer: ArrayBuffer[] } = {
        format,
        codedWidth: rect.width,
        codedHeight: rect.height,
        layout,
        timestamp: frame.timestamp,
        duration: frame.duration ?? undefined,
        displayWidth: frame.displayWidth,
        displayHeight: frame.displayHeight,
        colorSpace: frame.colorSpace.toJSON(),
        transfer: [buffer],
      };
      return new VideoFrame(buffer, init);
    } catch {
      // A layout the copy cannot express falls through to a drawn copy.
    }
  }
  const bitmap = await createImageBitmap(frame);
  try {
    return new VideoFrame(bitmap, {
      timestamp: frame.timestamp,
      duration: frame.duration ?? undefined,
    });
  } finally {
    bitmap.close();
  }
}

/** What a copy of `sample` will hold: its pixel data, or RGBA where it has no pixel format. */
function sampleBytes(sample: VideoSample): number {
  try {
    return Math.max(1, sample.allocationSize());
  } catch {
    return Math.max(1, sample.codedWidth * sample.codedHeight * 4);
  }
}

/** What a frame the reverse window owns holds. */
function ownedFrameBytes(frame: VideoFrame): number {
  try {
    return Math.max(1, frame.allocationSize());
  } catch {
    return mediabunnyVideoFrameBytes(frame);
  }
}

function ticksFromVideoFrame(frame: VideoFrame): bigint {
  return microsecondsToTicks(frame.timestamp);
}

function mediabunnyVideoFrameBytes(frame: VideoFrame): number {
  const width = Math.max(1, frame.codedWidth || frame.displayWidth || 1);
  const height = Math.max(1, frame.codedHeight || frame.displayHeight || 1);
  return width * height * 4;
}

/**
 * The part of `alpha2/wasm` the audio adapter resamples with: the engine's `AudioResampler`, the
 * converter a native host uses.
 */
export interface AudioResamplerBinding {
  readonly AudioResampler: new (
    sourceRate: number,
    targetRate: number,
  ) => AudioResampler;
}

/**
 * One `alpha2/wasm` `AudioResampler`. Positions are sample indexes: of the source at its rate, of
 * the output at the project's.
 */
export interface AudioResampler {
  /** The first source sample output `targetStart` weighs. */
  sourceStart(targetStart: number): number;
  /** The source sample after the last one output `targetEnd - 1` weighs. */
  sourceEnd(targetEnd: number): number;
  /** The first output sample at or after source sample `source`. */
  targetAtOrAfter(source: number): number;
  /**
   * `frames` outputs from `targetStart`, from `input` starting at source sample `inputStart`; the
   * source is silent outside `input`.
   */
  resample(
    input: Float32Array,
    inputStart: number,
    targetStart: number,
    frames: number,
  ): Float32Array;
  free?(): void;
}

/** Reads byte ranges of a mediabunny input, for what its demuxers do not report. */
function byteReader(resource: MediabunnyInput): ByteReader {
  if (resource instanceof Blob)
    return async (offset, length) =>
      new Uint8Array(
        await resource.slice(offset, offset + length).arrayBuffer(),
      );
  const bytes =
    resource instanceof ArrayBuffer
      ? new Uint8Array(resource)
      : new Uint8Array(
          resource.buffer,
          resource.byteOffset,
          resource.byteLength,
        );
  return (offset, length) =>
    Promise.resolve(bytes.subarray(offset, offset + length));
}

function byteLength(resource: MediabunnyInput): number {
  return resource instanceof Blob ? resource.size : resource.byteLength;
}

function bufferBytes(
  source: AllowSharedBufferSource | undefined,
): Uint8Array | null {
  if (source === undefined) return null;
  // `SharedArrayBuffer` is not defined in a page that is not cross-origin isolated, so a view is
  // told apart from a buffer, not a shared buffer from a plain one.
  if (ArrayBuffer.isView(source))
    return new Uint8Array(source.buffer, source.byteOffset, source.byteLength);
  return new Uint8Array(source);
}

/** The source rates the audio adapter reads, as a native host does. */
const SOURCE_SAMPLE_RATES = { min: 1_000, max: 768_000 } as const;

/**
 * How far before the samples a window needs decoding starts, in seconds: a native host's seek
 * pre-roll.
 *
 * A decoder that starts mid-stream has no history: AAC's first frame lacks the previous frame's
 * overlap. mediabunny starts every iteration with a new decoder at the packet holding its start
 * time, so each window starts a tenth of a second early and discards up to what it needs.
 */
const AUDIO_PREROLL_SECONDS = 0.1;
/**
 * The pre-roll for Opus (400 ms), a native host's too. Opus asks for at least 80 ms (RFC 7845), but
 * its decoder converges on a straight decode only gradually: from 100 ms early the samples a window
 * needs are still 2e-2 off, from 400 ms under 1e-6.
 */
const OPUS_PREROLL_SECONDS = 0.4;

/**
 * Optional mediabunny audio demux/decode adapter with project-rate resampling.
 *
 * Each sample is placed at its timestamp, as a native host places it: a timestamp within the
 * sample's duration or 2 ms of where the previous one ended continues it, one further ahead leaves
 * silence, one further back drops what it repeats. A window's first sample goes on the frame grid
 * the stream's first timestamps show ({@link audioFrameGrid}), so in a file that rounds them
 * (Matroska, WebM) every window holds its samples where a decode from the start puts them. A
 * window's source samples are then resampled together by the engine's `AudioResampler`, so a window
 * holds the samples a native decode of the same source holds.
 *
 * Every channel is kept, 1 to {@link MAX_AUDIO_CHANNELS}, and each block names the speaker each
 * feeds ({@link MediabunnyAudioDecoder.speakers}), read from the file as a native host reads it: a
 * WAVE channel mask, a QuickTime `chan` box, or the codec's channel configuration (AAC, Opus
 * mapping family 1, Vorbis, FLAC). The engine folds them by speaker. Those speakers are in the
 * order `FFmpeg`'s decoder delivers; a browser decoder that delivers another (Safari's AAC decoder
 * keeps the bitstream's) has its planes put back in that order, as measured once per kind of
 * stream: an AAC channel configuration or program config element, an Opus or Vorbis channel
 * count, an AC-3 or E-AC-3 layout ({@link decoderPlaneOrder}).
 */
export class MediabunnyAudioDecoder implements BrowserAudioDecoder {
  readonly #sourceId: string;
  readonly #input: Input;
  readonly #sink: AudioSampleSink;
  readonly #channelCount: number;
  /**
   * The speaker each plane of every block feeds, in plane order: the layout the source declares.
   * `null` when it declares none (a WAVE file without a channel mask, say): its blocks then carry
   * no `speakers`, and the engine assumes `FFmpeg`'s default for the channel count, as a native
   * host does. A host may report a `null` layout of more than two channels as a guess.
   */
  readonly speakers: readonly AudioSpeaker[] | null;
  /**
   * For each plane the decoder delivers, the plane of {@link speakers}' order it holds, when that
   * is not the same plane (measured, {@link decoderPlaneOrder}); `undefined` when every plane is
   * where the speakers say.
   */
  readonly #planeOrder: readonly number[] | undefined;
  readonly #sourceRate: number;
  /**
   * Where the stream's frames start, as its first timestamps show ({@link audioFrameGrid}): the
   * first sample a window decodes is placed on it, as a native host places the first frame after
   * a seek.
   */
  readonly #grid: AudioFrameGrid | undefined;
  /** How far before what a window needs its decoding starts, in seconds: the codec's pre-roll. */
  readonly #preroll: number;
  /**
   * How far the demuxer's timestamps are after the samples' presentation times, in seconds: a
   * Matroska track's `CodecDelay` in its time base ({@link readMatroskaCodecDelay}), which
   * mediabunny does not subtract; 0 in any other container.
   */
  readonly #delay: number;
  readonly #binding: AudioResamplerBinding;
  #resampler?: { targetRate: number; resampler: AudioResampler };
  #closed = false;

  private constructor(
    sourceId: string,
    input: Input,
    sink: AudioSampleSink,
    channelCount: number,
    speakers: readonly AudioSpeaker[] | null,
    planeOrder: readonly number[] | undefined,
    sourceRate: number,
    grid: AudioFrameGrid | undefined,
    preroll: number,
    delay: number,
    binding: AudioResamplerBinding,
  ) {
    this.#sourceId = sourceId;
    this.#input = input;
    this.#sink = sink;
    this.#channelCount = channelCount;
    this.speakers = speakers;
    this.#planeOrder = planeOrder;
    this.#sourceRate = sourceRate;
    this.#grid = grid;
    this.#preroll = preroll;
    this.#delay = delay;
    this.#binding = binding;
  }

  /**
   * Opens `resource`'s primary audio track, resampling with `binding`'s `AudioResampler`.
   *
   * Rejects when the input has no audio track, when its channel count is outside 1 to
   * {@link MAX_AUDIO_CHANNELS} or its sample rate outside 1 kHz to 768 kHz (`RangeError`), or when
   * its decoder configuration or channel layout cannot be read.
   */
  static async open(
    sourceId: string,
    resource: MediabunnyInput,
    binding: AudioResamplerBinding,
  ): Promise<MediabunnyAudioDecoder> {
    const source: Source =
      resource instanceof Blob
        ? new BlobSource(resource)
        : new BufferSource(resource);
    const input = new Input({ source, formats: ALL_FORMATS });
    const track = await input.getPrimaryAudioTrack();
    if (track === null) {
      input.dispose();
      throw new Error("mediabunny input has no audio track");
    }
    const channelCount = track.numberOfChannels;
    if (channelCount < 1 || channelCount > MAX_AUDIO_CHANNELS) {
      input.dispose();
      throw new RangeError(
        `audio with ${channelCount} channels is unsupported: the engine takes 1 to ${MAX_AUDIO_CHANNELS}`,
      );
    }
    let speakers: AudioSpeaker[] | null;
    let description: Uint8Array | null;
    let planeOrder: readonly number[] | null;
    try {
      const config = await track.getDecoderConfig();
      description = bufferBytes(config?.description);
      // An AC-3, E-AC-3 or DTS stream declares its layout in its frames.
      let firstPacket: Promise<Uint8Array | null> | undefined;
      const readFirstPacket = () =>
        (firstPacket ??= new EncodedPacketSink(track)
          .getFirstPacket()
          .then((packet) => packet?.data ?? null));
      speakers = await readAudioSpeakers(
        byteReader(resource),
        byteLength(resource),
        track,
        description,
        readFirstPacket,
      );
      planeOrder = await decoderPlaneOrder(
        decoderOrderKey(
          track.codec,
          description,
          channelCount,
          track.codec === "ac3" || track.codec === "eac3"
            ? await readFirstPacket()
            : null,
        ),
      );
    } catch (error) {
      input.dispose();
      throw error;
    }
    const sourceRate = track.sampleRate;
    if (
      !Number.isInteger(sourceRate) ||
      sourceRate < SOURCE_SAMPLE_RATES.min ||
      sourceRate > SOURCE_SAMPLE_RATES.max
    ) {
      input.dispose();
      throw new RangeError(
        `the source's audio is ${sourceRate} Hz, and alpha2 reads audio from ${SOURCE_SAMPLE_RATES.min} Hz to ${SOURCE_SAMPLE_RATES.max} Hz`,
      );
    }
    let grid: AudioFrameGrid | undefined;
    let delay: number;
    try {
      const first = await firstPacketTimes(track, new EncodedPacketSink(track));
      // A Matroska block is timed `CodecDelay` after its samples; native hosts subtract it as
      // FFmpeg does, so the samples line up with the same encode's in MP4.
      const codecDelay = await readMatroskaCodecDelay(resource, track.id);
      let packetGrid: AudioFrameGrid | undefined;
      if (first === undefined) delay = codecDelay / 1e9;
      else {
        const delayUnits = Math.round((codecDelay * first.resolution) / 1e9);
        delay = delayUnits / first.resolution;
        packetGrid = audioFrameGrid(
          first.timestamps.map((timestamp) => timestamp - delayUnits),
          first.resolution,
          sourceRate,
        );
      }
      // A WebCodecs Opus decoder drops the pre-skip from the first frame after each configure, so
      // each window's frames start the pre-skip before the packet grid.
      const preSkip = track.codec === "opus" ? opusPreSkip(description) : 0;
      grid =
        packetGrid === undefined
          ? undefined
          : { ...packetGrid, anchor: packetGrid.anchor - preSkip };
    } catch (error) {
      input.dispose();
      throw error;
    }
    return new MediabunnyAudioDecoder(
      sourceId,
      input,
      new AudioSampleSink(track),
      channelCount,
      speakers,
      planeOrder === null ||
        planeOrder.length !== channelCount ||
        planeOrder.every((engine, plane) => engine === plane)
        ? undefined
        : planeOrder,
      sourceRate,
      grid,
      track.codec === "opus" ? OPUS_PREROLL_SECONDS : AUDIO_PREROLL_SECONDS,
      delay,
      binding,
    );
  }

  #resamplerTo(targetRate: number): AudioResampler {
    if (this.#resampler?.targetRate !== targetRate) {
      this.#resampler?.resampler.free?.();
      this.#resampler = {
        targetRate,
        resampler: new this.#binding.AudioResampler(
          this.#sourceRate,
          targetRate,
        ),
      };
    }
    return this.#resampler.resampler;
  }

  /**
   * The request's interval at `projectSampleRate`, widened to whole project samples plus two on
   * each side (never before sample 0). Time outside the stream is silence.
   *
   * Rejects when the decoder is closed, the request names another source or an empty interval,
   * the rate is not a positive integer, `signal` aborts, or the track changes channel count or
   * sample rate mid-stream.
   */
  async refill(
    request: AudioDecoderRefillRequest,
    projectSampleRate: number,
    signal?: AbortSignal,
  ): Promise<BrowserAudioBlock> {
    if (this.#closed) throw new Error("mediabunny decoder is closed");
    if (request.sourceId !== this.#sourceId)
      throw new Error(`decoder source mismatch: ${request.sourceId}`);
    if (request.endMediaTimeTicks <= request.startMediaTimeTicks)
      throw new RangeError("audio refill range must be non-empty");
    if (!Number.isInteger(projectSampleRate) || projectSampleRate <= 0)
      throw new RangeError("invalid project sample rate");
    throwIfAborted(signal);

    // The two-sample margin lets the engine interpolate at either edge without the whole source.
    const firstSample = Math.max(
      0,
      ticksToSampleFloor(request.startMediaTimeTicks, projectSampleRate) - 2,
    );
    const lastSample =
      ticksToSampleCeil(request.endMediaTimeTicks, projectSampleRate) + 2;
    const frames = Math.max(1, lastSample - firstSample);
    const rate = this.#sourceRate;
    const channelCount = this.#channelCount;
    const resampler = this.#resamplerTo(projectSampleRate);
    const sourceStart = resampler.sourceStart(firstSample);
    const sourceEnd = resampler.sourceEnd(firstSample + frames);
    const source = Array.from(
      { length: channelCount },
      () => new Float32Array(sourceEnd - sourceStart),
    );
    const decodeFrom = sourceStart / rate - this.#preroll;
    /** The first sample decoded, when nothing comes before it in the stream. */
    let streamStart: number | undefined;
    /** Where the next sample continues the last one placed. */
    let expected: number | undefined;
    const delay = this.#delay;
    for await (const sample of this.#sink.samples(
      decodeFrom + delay,
      sourceEnd / rate + delay,
    )) {
      try {
        throwIfAborted(signal);
        // The engine folds a source's channels into the project's by speaker, so every one is kept,
        // each in the plane its speaker is listed at.
        if (sample.numberOfChannels !== channelCount)
          throw new RangeError(
            `audio track changed from ${channelCount} to ${sample.numberOfChannels} channels`,
          );
        if (sample.sampleRate !== rate)
          throw new RangeError(
            `audio track changed from ${rate} Hz to ${sample.sampleRate} Hz`,
          );
        const length = sample.numberOfFrames;
        const timestamp = sample.timestamp - delay;
        let at = Math.round(timestamp * rate);
        let skip = 0;
        if (expected === undefined) {
          // Place the window's first sample where a decode from the stream's start puts it.
          if (this.#grid !== undefined)
            at = placeOnAudioFrameGrid(this.#grid, at, length);
          // mediabunny starts at the packet holding the time asked for; a first sample after that
          // time is the stream's first.
          if (timestamp > decodeFrom) streamStart = at;
        } else {
          const ahead = at - expected;
          const tolerance = Math.max(length, Math.ceil(rate / 500));
          if (Math.abs(ahead) < tolerance) at = expected;
          else if (ahead < 0) {
            skip = Math.min(length, -ahead);
            at = expected;
          }
        }
        expected = at + length - skip;
        const from = Math.max(at, sourceStart);
        const to = Math.min(at + length - skip, sourceEnd);
        if (to <= from) continue;
        const data = new Float32Array(length);
        const order = this.#planeOrder;
        for (let plane = 0; plane < channelCount; plane += 1) {
          sample.copyTo(data, { planeIndex: plane, format: "f32-planar" });
          source[order === undefined ? plane : order[plane]].set(
            data.subarray(skip + from - at, skip + to - at),
            from - sourceStart,
          );
        }
      } finally {
        sample.close();
      }
    }
    // Outside the stream the source is silent, and so is the output: a band-limited reading of the
    // stream's first and last samples rings past them.
    const first =
      streamStart === undefined
        ? firstSample
        : Math.max(firstSample, resampler.targetAtOrAfter(streamStart));
    const end =
      expected === undefined
        ? firstSample
        : expected < sourceEnd
          ? Math.min(firstSample + frames, resampler.targetAtOrAfter(expected))
          : firstSample + frames;
    const channels = source.map((plane) => {
      const output = new Float32Array(frames);
      if (end > first)
        output.set(
          resampler.resample(plane, sourceStart, first, end - first),
          first - firstSample,
        );
      return output;
    });
    return {
      sourceId: this.#sourceId,
      startSample: BigInt(firstSample),
      generation: request.generation,
      sampleRate: projectSampleRate,
      ...(this.speakers === null ? {} : { speakers: this.speakers }),
      channels,
    };
  }

  /** Frees the resampler and the input. A later `refill` rejects. */
  close(): void {
    if (!this.#closed) {
      this.#closed = true;
      this.#resampler?.resampler.free?.();
      this.#resampler = undefined;
      this.#input.dispose();
    }
  }
}

/**
 * Options for {@link MediabunnyMp4Sink}. At least one of `videoCodec` and `audioCodec` is required.
 */
export interface MediabunnyMp4SinkOptions {
  /** Adds a video track in this codec; requires `videoRateControl`. */
  readonly videoCodec?: "avc" | "hevc" | "vp9" | "av1";
  /**
   * Exact project frame rate, for example `{ num: 30000, den: 1001 }`.
   *
   * Declaring it snaps every written timestamp and duration to the rate, which is what produces a
   * constant-frame-rate track. Without it the muxer derives durations from floating-point timestamp
   * deltas and can alternate between neighbouring values, which plays back as intermittent judder.
   */
  readonly frameRate?: { readonly num: number; readonly den: number };
  /** Adds an audio track in this codec; requires `audioBitrate`. */
  readonly audioCodec?: "aac" | "opus";
  /**
   * The video rate, already resolved. Required with `videoCodec`.
   *
   * This adapter takes numbers and never a quality level: what a level costs is the engine's
   * decision (`resolveVideoRateControl` in `alpha2`), so a caller that swaps this sink for its own
   * WebCodecs one receives the same numbers this one does. With a `quantizer`, mediabunny tries
   * constant-quantizer encoding first and falls back to `bitrate` where the browser's encoder
   * refuses it; `appliedVideoRateControl` says which ran.
   */
  readonly videoRateControl?: VideoRateControl;
  /** The video encoder's latency mode; by default the one {@link videoEncoderLatencyMode} measures. */
  readonly videoLatencyMode?: VideoEncoderLatencyMode;
  /**
   * How long the video encoder may hold frames without output before writes reject with
   * `ENCODE_STALLED` (default 10 s).
   */
  readonly stallTimeoutMs?: number;
  /** The audio bitrate in bits per second, already resolved. Required with `audioCodec`. */
  readonly audioBitrate?: number;
  /**
   * The rate the audio encoder is given, already resolved: {@link audioEncoderSampleRate}'s answer
   * for the project rate. Blocks at another rate (the project's) are converted to it with
   * `audioResampler`, the engine's converter, as a native host converts for an encoder that does
   * not take the project rate. Defaults to the blocks' own rate.
   */
  readonly audioSampleRate?: number;
  /**
   * The initialized `alpha2/wasm` module, whose `AudioResampler` converts the blocks to
   * `audioSampleRate`. Required when that differs from the blocks' rate.
   */
  readonly audioResampler?: AudioResamplerBinding;
  /**
   * Where the file goes. `stream` writes a plain MP4 (`moov` at the end, no fast start) through a
   * seekable positioned stream as it is muxed (see `WritableFileDestination` in
   * `alpha2/export-destination`), so no part of the encoded output accumulates in memory; the
   * stream is closed (committed) only once {@link MediabunnyMp4Sink.finish} has finished the file,
   * and aborted (discarded) when the export fails or is cancelled. `memory` keeps it in an
   * `ArrayBuffer`, for small outputs only: `maxBytes` is the most the output may reach, which the
   * caller sizes (and budgets for, see `memoryOutputPeakBytes`) before starting; past it the export
   * fails with `RESOURCE_LIMIT` instead of growing.
   */
  readonly destination: Mp4Destination;
}

/** Where a {@link MediabunnyMp4Sink} writes. */
export type Mp4Destination =
  | {
      readonly kind: "stream";
      readonly writable: WritableStream<PositionedWrite>;
    }
  | { readonly kind: "memory"; readonly maxBytes: number };

/**
 * The mediabunny quality a video track encodes at for the engine's rate control: what
 * {@link MediabunnyMp4Sink} configures its encoder with.
 */
export function videoEncoderQuality(rate: VideoRateControl): Quality {
  return new Quality({
    bitrate: rate.bitrate,
    bitrateMode: rate.bitrateMode,
    ...(rate.quantizer === undefined ? {} : { quantizer: rate.quantizer }),
  });
}

/**
 * Whether this browser can encode `codec` video at `width`x`height` with `rate`, asked with the
 * configurations {@link MediabunnyMp4Sink} would try. WebCodecs leaves the codecs to the browser
 * (Chrome on Linux encodes no HEVC, for one), so an export asks before it opens its encoder.
 */
export async function canEncodeVideoTrack(
  codec: "avc" | "hevc" | "vp9" | "av1",
  options: {
    readonly width: number;
    readonly height: number;
    readonly rate: VideoRateControl;
  },
): Promise<boolean> {
  return canEncodeVideo(codec, {
    width: options.width,
    height: options.height,
    quality: videoEncoderQuality(options.rate),
  }).catch(() => false);
}

/** How a browser's video encoder is opened: see {@link videoEncoderLatencyMode}. */
export type VideoEncoderLatencyMode = "quality" | "realtime";

/** The profiles mediabunny opens (H.264 High, HEVC Main), at a level for the probe's frames. */
const LATENCY_PROBE_CODECS = {
  avc: "avc1.64001e",
  hevc: "hev1.1.6.L93.B0",
  vp9: "vp09.00.10.08",
  av1: "av01.0.04M.08",
} as const;
const LATENCY_PROBE_WIDTH = 320;
const LATENCY_PROBE_HEIGHT = 240;
/** More frames than Safari's encoder takes in before it stops. */
const LATENCY_PROBE_FRAMES = 6;
/** How long a probe encoder has to put out its first chunk. */
const LATENCY_PROBE_TIMEOUT_MS = 1_500;
const latencyModes = new Map<
  keyof typeof LATENCY_PROBE_CODECS,
  Promise<VideoEncoderLatencyMode>
>();

/**
 * The latency mode {@link MediabunnyMp4Sink} opens `codec`'s encoder with: `"realtime"` only where a
 * quality-mode encoder puts out nothing (Safari 27's H.264) and a realtime one works. Measured once
 * per codec.
 */
export function videoEncoderLatencyMode(
  codec: "avc" | "hevc" | "vp9" | "av1",
): Promise<VideoEncoderLatencyMode> {
  let mode = latencyModes.get(codec);
  if (mode === undefined) {
    mode = measureLatencyMode(codec);
    latencyModes.set(codec, mode);
  }
  return mode;
}

async function measureLatencyMode(
  codec: keyof typeof LATENCY_PROBE_CODECS,
): Promise<VideoEncoderLatencyMode> {
  if (typeof VideoEncoder === "undefined") return "quality";
  // One at a time: on Chrome a realtime encoder opening at once delays the quality one.
  if ((await encoderPutsOut(codec, "quality")) !== false) return "quality";
  return (await encoderPutsOut(codec, "realtime")) === true
    ? "realtime"
    : "quality";
}

/** `true` once the encoder puts out a chunk, `false` if it puts out none, `undefined` if it failed. */
async function encoderPutsOut(
  codec: keyof typeof LATENCY_PROBE_CODECS,
  latencyMode: VideoEncoderLatencyMode,
): Promise<boolean | undefined> {
  const config: VideoEncoderConfig = {
    codec: LATENCY_PROBE_CODECS[codec],
    width: LATENCY_PROBE_WIDTH,
    height: LATENCY_PROBE_HEIGHT,
    bitrate: 500_000,
    framerate: 30,
    latencyMode,
    ...(codec === "avc" ? { avc: { format: "avc" as const } } : {}),
  };
  try {
    if (!(await VideoEncoder.isConfigSupported(config)).supported)
      return undefined;
  } catch {
    return undefined;
  }
  let outputs = 0;
  let failed = false;
  let encoder: VideoEncoder | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let putOut = () => {};
  const firstOutput = new Promise<void>((resolve) => (putOut = resolve));
  try {
    encoder = new VideoEncoder({
      output: () => {
        outputs += 1;
        putOut();
      },
      error: () => (failed = true),
    });
    encoder.configure(config);
    const pixels = new Uint8Array(
      LATENCY_PROBE_WIDTH * LATENCY_PROBE_HEIGHT * 1.5,
    );
    for (let index = 0; index < LATENCY_PROBE_FRAMES; index += 1) {
      pixels.fill(16 + index * 32, 0, LATENCY_PROBE_WIDTH);
      const frame = new VideoFrame(pixels, {
        format: "I420",
        codedWidth: LATENCY_PROBE_WIDTH,
        codedHeight: LATENCY_PROBE_HEIGHT,
        timestamp: Math.round((index * 1_000_000) / 30),
        duration: Math.round(1_000_000 / 30),
      });
      try {
        encoder.encode(frame, { keyFrame: index === 0 });
      } finally {
        frame.close();
      }
    }
    await Promise.race([
      firstOutput,
      encoder.flush().catch(() => (failed = true)),
      new Promise<void>((resolve) => {
        timer = setTimeout(resolve, LATENCY_PROBE_TIMEOUT_MS);
      }),
    ]);
    if (outputs > 0) return true;
    return failed ? undefined : false;
  } catch {
    return undefined;
  } finally {
    clearTimeout(timer);
    try {
      if (encoder?.state !== "closed") encoder?.close();
    } catch {
      // Already closed by its own failure.
    }
  }
}

/**
 * The sample rates each audio codec a {@link MediabunnyMp4Sink} writes can carry, lowest first:
 * AAC's MPEG-4 rates up to the project limit, and Opus's.
 */
const ENCODER_SAMPLE_RATES = {
  aac: [
    8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000,
    64_000, 88_200, 96_000,
  ],
  opus: [8_000, 12_000, 16_000, 24_000, 48_000],
} as const;

/**
 * The rate this browser encodes `codec` audio of a project at `sampleRate` with, by the rule a
 * native host follows: the project rate when the browser's encoder takes it, else the lowest rate
 * the codec carries above it that the encoder takes, else the highest below it; `null` when the
 * encoder takes none. Pass it to {@link MediabunnyMp4Sink} as `audioSampleRate`, so every project
 * rate exports where the browser encodes the codec at all.
 */
export async function audioEncoderSampleRate(
  codec: "aac" | "opus",
  options: {
    readonly sampleRate: number;
    readonly numberOfChannels: number;
    readonly bitrate: number;
  },
): Promise<number | null> {
  const rate = options.sampleRate;
  const carried = ENCODER_SAMPLE_RATES[codec];
  const candidates = new Set([
    rate,
    ...carried.filter((candidate) => candidate > rate),
    ...carried.filter((candidate) => candidate < rate).reverse(),
  ]);
  for (const candidate of candidates) {
    const supported = await canEncodeAudio(codec, {
      numberOfChannels: options.numberOfChannels,
      sampleRate: candidate,
      quality: new Quality({ bitrate: options.bitrate }),
    }).catch(() => false);
    if (supported) return candidate;
  }
  return null;
}

/**
 * Project-rate blocks converted to an encoder's rate with the engine's converter, as a native
 * host's AAC input converts them: outputs from the first block's start on, each made once the
 * samples it reads have arrived, the source silent outside them, and ending where the project's
 * audio does (the first output at or after its end).
 */
class EncoderRateConverter {
  readonly targetRate: number;
  readonly #projectRate: number;
  readonly #resampler: AudioResampler;
  /** The project samples from `#bufferStart` on that outputs still to be made may read. */
  #buffer: Float32Array[];
  #bufferStart: number;
  /** The project sample the next block starts at. */
  #received: number;
  /** The next output sample. */
  #produced: number;

  constructor(
    binding: AudioResamplerBinding,
    first: BrowserAudioBlock,
    targetRate: number,
  ) {
    this.targetRate = targetRate;
    this.#projectRate = first.sampleRate;
    this.#resampler = new binding.AudioResampler(first.sampleRate, targetRate);
    const start = Number(first.startSample);
    this.#buffer = first.channels.map(() => new Float32Array(0));
    this.#bufferStart = start;
    this.#received = start;
    this.#produced = this.#resampler.targetAtOrAfter(start);
  }

  /**
   * Takes the next block, which must continue the last one at the same rate and channel count,
   * and returns the outputs it completes (`startSample` at the target rate), or null for none.
   */
  push(
    block: BrowserAudioBlock,
  ): { startSample: number; channels: Float32Array[] } | null {
    if (
      block.sampleRate !== this.#projectRate ||
      block.channels.length !== this.#buffer.length
    )
      throw new RangeError("audio block format changed");
    if (Number(block.startSample) !== this.#received)
      throw new RangeError(
        "audio blocks converted to the encoder's rate must follow one another",
      );
    const frames = block.channels[0]?.length ?? 0;
    this.#buffer = this.#buffer.map((plane, index) => {
      const grown = new Float32Array(plane.length + frames);
      grown.set(plane);
      grown.set(block.channels[index], plane.length);
      return grown;
    });
    this.#received += frames;
    return this.#convert(this.#readyEnd());
  }

  /** Ends the input: the outputs still to make, up to where the project's audio ends. */
  finish(): { startSample: number; channels: Float32Array[] } | null {
    return this.#convert(this.#resampler.targetAtOrAfter(this.#received));
  }

  /** Frees the engine's converter. */
  free(): void {
    this.#resampler.free?.();
  }

  /** The end of the outputs whose samples have all arrived. */
  #readyEnd(): number {
    // The output before `end` reads up to `sourceEnd(end)`, which grows with `end`.
    let low = this.#produced;
    let high = Math.max(low, this.#resampler.targetAtOrAfter(this.#received));
    while (low < high) {
      const middle = low + Math.ceil((high - low) / 2);
      if (this.#resampler.sourceEnd(middle) <= this.#received) low = middle;
      else high = middle - 1;
    }
    return low;
  }

  #convert(
    end: number,
  ): { startSample: number; channels: Float32Array[] } | null {
    const start = this.#produced;
    if (end <= start) return null;
    const channels = this.#buffer.map((plane) =>
      this.#resampler.resample(plane, this.#bufferStart, start, end - start),
    );
    this.#produced = end;
    const keep = Math.min(
      this.#received,
      Math.max(this.#bufferStart, this.#resampler.sourceStart(end)),
    );
    if (keep > this.#bufferStart) {
      this.#buffer = this.#buffer.map((plane) =>
        plane.slice(keep - this.#bufferStart),
      );
      this.#bufferStart = keep;
    }
    return { startSample: start, channels };
  }
}

/** What a finished {@link MediabunnyMp4Sink} produced. */
export type Mp4Output =
  | { readonly kind: "stream"; readonly bytes: number }
  | { readonly kind: "memory"; readonly buffer: ArrayBuffer };

/** The rate control the browser's encoder was actually configured with. */
export interface AppliedVideoRateControl {
  /** `quantizer` when the constant-quantizer candidate was accepted. */
  readonly bitrateMode: "constant" | "variable" | "quantizer";
  /** The configured bitrate in bits per second, when the configuration set one. */
  readonly bitrate?: number;
}

/**
 * Optional mediabunny encoder/muxer implementing both public alpha2 sinks.
 *
 * The sink is the file: its `finish` finalizes every track, which suits one runner, or a host that
 * writes both tracks itself and finishes once. When two runners share the file (`exportToSink` for
 * the video and `renderOfflineAudio` for the audio, at once or one after the other), give each its
 * track instead, {@link MediabunnyMp4Sink.videoTrack} and {@link MediabunnyMp4Sink.audioTrack}: a
 * runner's `finish` then ends its own track, and the file is finalized when every configured track
 * has ended. Either track's `abort` discards the whole file, and the other runner's next write
 * fails.
 *
 * The constructor throws `RangeError` when no track is configured, a codec lacks its rate, a
 * memory destination's `maxBytes` is not a positive safe integer, or `stallTimeoutMs` is out of
 * range.
 */
export class MediabunnyMp4Sink
  implements BrowserEncodeSink, BrowserAudioBlockSink
{
  readonly #target: BufferTarget | StreamTarget;
  /**
   * A stream destination, written through the muxer and committed or discarded by this sink alone:
   * see the constructor.
   */
  readonly #file?: WritableStreamDefaultWriter<PositionedWrite>;
  #discarded = false;
  readonly #output: Output;
  #written = 0;
  #overflow: ExportFailureError | undefined;
  readonly #hasVideo: boolean;
  #video?: VideoSampleSource;
  #latencyMode?: VideoEncoderLatencyMode;
  readonly #stallTimeoutMs: number;
  #framesIn = 0;
  #framesOut = 0;
  /** When the encoder last put a frame out, or was given one while it owed none. */
  #progressAt = 0;
  readonly #audio?: AudioSampleSource;
  readonly #audioRate?: number;
  readonly #audioResampler?: AudioResamplerBinding;
  #audioConverter?: EncoderRateConverter;
  #audioFlushed = false;
  readonly #ready: Promise<void>;
  #frameDuration?: number;
  #finished = false;
  #applied?: AppliedVideoRateControl;
  /** Tracks whose runner finished, through {@link videoTrack} or {@link audioTrack}. */
  readonly #ended = new Set<"video" | "audio">();
  #videoTrack?: BrowserEncodeSink;
  #audioTrack?: BrowserAudioBlockSink;

  constructor(options: MediabunnyMp4SinkOptions) {
    const destination = options.destination;
    if (destination.kind === "memory") {
      if (
        !Number.isSafeInteger(destination.maxBytes) ||
        destination.maxBytes <= 0
      )
        throw new RangeError(
          "memory destination maxBytes must be a positive safe integer",
        );
      this.#target = new BufferTarget();
      // Mediabunny grows the buffer as it writes; stopping at the reserved size keeps the output
      // within what was budgeted for it.
      this.#target.onwrite = (_start, end) => {
        this.#written = Math.max(this.#written, end);
        if (end > destination.maxBytes)
          this.#overflow ??= new ExportFailureError({
            code: "RESOURCE_LIMIT",
            message: `The export passed the ${destination.maxBytes}-byte limit set for an output kept in memory`,
            origin: "browser",
            operation: "export",
            backend: "unknown",
            phase: "mux",
            resource: "js",
            requestedBytes: end,
          });
      };
    } else {
      // mediabunny closes its stream on finish, failure and cancel alike, and a file stream commits
      // on close, so the muxer gets a stream of its own; `finish` commits the file and `abort`
      // discards it.
      const file = destination.writable.getWriter();
      this.#file = file;
      const muxed = new WritableStream<PositionedWrite>({
        write: async (chunk) => {
          // The destination's queue bounds the bytes in flight; a refused write errors it, which
          // the next write, or the commit, reports.
          await file.ready;
          void file.write(chunk).catch(() => {});
        },
      });
      this.#target = new StreamTarget(muxed, { chunked: false });
      this.#target.onwrite = (_start, end) => {
        this.#written = Math.max(this.#written, end);
      };
    }
    this.#output = new Output({
      // Plain MP4 without fast start, so the payload is never held in memory or rewritten.
      format: new Mp4OutputFormat({ fastStart: false }),
      target: this.#target,
    });
    this.#hasVideo = options.videoCodec !== undefined;
    if (this.#hasVideo && options.videoRateControl === undefined)
      throw new RangeError("videoRateControl is required with videoCodec");
    const frameRate = options.frameRate;
    this.#frameDuration =
      this.#hasVideo &&
      frameRate !== undefined &&
      frameRate.num > 0 &&
      frameRate.den > 0
        ? frameRate.den / frameRate.num
        : undefined;
    this.#stallTimeoutMs =
      options.stallTimeoutMs ?? DEFAULT_ENCODER_STALL_TIMEOUT_MS;
    validateStallTimeout(this.#stallTimeoutMs);
    if (options.audioCodec !== undefined) {
      if (options.audioBitrate === undefined)
        throw new RangeError("audioBitrate is required with audioCodec");
      const rate = options.audioSampleRate;
      if (rate !== undefined && (!Number.isSafeInteger(rate) || rate <= 0))
        throw new RangeError("audioSampleRate must be a positive integer");
      this.#audioRate = rate;
      this.#audioResampler = options.audioResampler;
      this.#audio = new AudioSampleSource({
        codec: options.audioCodec,
        quality: new Quality({ bitrate: options.audioBitrate }),
      });
    }
    if (!this.#hasVideo && this.#audio === undefined)
      throw new RangeError("at least one output track is required");
    this.#ready = this.#open(options);
  }

  /** Adds the tracks once the video latency mode is known, and starts the file. */
  async #open(options: MediabunnyMp4SinkOptions): Promise<void> {
    const codec = options.videoCodec;
    const rate = options.videoRateControl;
    if (codec !== undefined && rate !== undefined) {
      const latencyMode =
        options.videoLatencyMode ?? (await videoEncoderLatencyMode(codec));
      if (this.#output.state === "canceled") return;
      this.#latencyMode = latencyMode;
      this.#video = new VideoSampleSource({
        codec,
        quality: videoEncoderQuality(rate),
        latencyMode,
        // mediabunny reports each candidate config before it asks the browser whether it is
        // supported and stops at the first that is, so the last one reported is the one that runs.
        onEncoderConfig: (config) => {
          const mode = config.bitrateMode as string | undefined;
          this.#applied = {
            bitrateMode:
              mode === "quantizer" || mode === "constant" ? mode : "variable",
            ...(config.bitrate === undefined
              ? {}
              : { bitrate: config.bitrate }),
          };
        },
        onEncodedPacket: () => {
          this.#framesOut += 1;
          this.#progressAt = performance.now();
        },
      });
      this.#output.addVideoTrack(
        this.#video,
        this.#frameDuration === undefined
          ? undefined
          : { frameRate: 1 / this.#frameDuration },
      );
    }
    if (this.#audio !== undefined) this.#output.addAudioTrack(this.#audio);
    await this.#output.start();
  }

  /** `work`, unless the video encoder holds frames without output for `stallTimeoutMs`. */
  #watched<T>(work: Promise<T>): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      const timer = setInterval(
        () => {
          const waited = performance.now() - this.#progressAt;
          if (
            this.#framesOut >= this.#framesIn ||
            waited < this.#stallTimeoutMs
          )
            return;
          clearInterval(timer);
          const held = this.#framesIn - this.#framesOut;
          reject(
            Object.assign(
              new Error(
                `video encoder stalled: no output for ${Math.round(waited)} ms with ${held} frames queued`,
              ),
              {
                code: "ENCODE_STALLED",
                failure: {
                  code: "GPU_TIMEOUT",
                  phase: "encode",
                  stage: "encoder-stalled",
                  resource: "unknown",
                },
              },
            ),
          );
        },
        Math.min(1_000, this.#stallTimeoutMs),
      );
      void work.finally(() => clearInterval(timer)).then(resolve, reject);
    });
  }

  /** The latency mode the video encoder was opened with; undefined until the sink has started. */
  get videoLatencyMode(): VideoEncoderLatencyMode | undefined {
    return this.#latencyMode;
  }

  /**
   * The video track as a sink of its own, for a runner that shares the file with another: its
   * `finish` ends the video track and finalizes the file once every configured track has ended;
   * its `abort` discards the file. Throws `RangeError` when no video track is configured.
   */
  get videoTrack(): BrowserEncodeSink {
    if (!this.#hasVideo) throw new RangeError("video track is not configured");
    this.#videoTrack ??= {
      writeVideo: (output) => this.writeVideo(output),
      finish: () => this.#endTrack("video"),
      abort: (reason) => this.abort(reason),
    };
    return this.#videoTrack;
  }

  /**
   * The audio track as a sink of its own, for a runner that shares the file with another: see
   * {@link videoTrack}. Throws `RangeError` when no audio track is configured.
   */
  get audioTrack(): BrowserAudioBlockSink {
    if (this.#audio === undefined)
      throw new RangeError("audio track is not configured");
    this.#audioTrack ??= {
      writeAudio: (block) => this.writeAudio(block),
      finish: () => this.#endTrack("audio"),
      abort: (reason) => this.abort(reason),
    };
    return this.#audioTrack;
  }

  /**
   * Ends one track: its encoder flushes and the muxer stops waiting for its samples. The last
   * configured track to end finishes the file; an earlier one resolves at once, so runners that
   * share the file in turn do not wait for each other.
   */
  async #endTrack(track: "video" | "audio"): Promise<void> {
    await this.#ready;
    if (!this.#ended.has(track)) {
      this.#ended.add(track);
      if (track === "audio") await this.#flushAudio();
      (track === "video" ? this.#video : this.#audio)?.close();
    }
    if (
      (!this.#hasVideo || this.#ended.has("video")) &&
      (this.#audio === undefined || this.#ended.has("audio"))
    )
      await this.finish();
  }

  /**
   * The rate control the browser's encoder was configured with, once the first frame has been
   * written: whether the quantizer candidate was accepted or the bitrate fallback ran. Undefined
   * before then.
   */
  get appliedVideoRateControl(): AppliedVideoRateControl | undefined {
    return this.#applied;
  }

  /**
   * Encodes one frame at its output time (`output.timeTicks`, counted from the output's zero).
   * Closes `output.frame` whether or not it succeeds. Rejects with `RESOURCE_LIMIT` once a memory
   * destination has passed `maxBytes`, with `ENCODE_STALLED` when the encoder stalls, or when no
   * video track is configured.
   */
  async writeVideo(output: ExportedFrame): Promise<void> {
    try {
      await this.#ready;
    } catch (error) {
      output.frame.close();
      throw error;
    }
    if (this.#overflow) {
      output.frame.close();
      throw this.#overflow;
    }
    if (this.#video === undefined) {
      output.frame.close();
      throw new Error(
        this.#hasVideo
          ? "the output was cancelled before its video track started"
          : "video track is not configured",
      );
    }
    // Declaring the duration keeps sample durations uniform instead of inferred from float deltas.
    const sample = new VideoSample(output.frame, {
      timestamp: Number(output.timeTicks) / 120_000,
      duration: this.#frameDuration,
    });
    if (this.#framesOut >= this.#framesIn) this.#progressAt = performance.now();
    this.#framesIn += 1;
    try {
      await this.#watched(this.#video.add(sample));
    } finally {
      sample.close();
      output.frame.close();
    }
  }

  /**
   * Encodes one planar block at `block.startSample`, counted from the output's zero as video
   * frames are. With an `audioSampleRate` other than the block's rate the block is converted to it
   * first; such blocks must then follow one another, each starting where the last ended, at one
   * rate and channel count. Rejects with `RESOURCE_LIMIT` once a memory destination has passed
   * `maxBytes`, when no audio track is configured, when the block's channels differ in length, or
   * (`RangeError`) when a block to convert does not continue the last or no `audioResampler` was
   * given.
   */
  async writeAudio(block: BrowserAudioBlock): Promise<void> {
    await this.#ready;
    if (this.#overflow) throw this.#overflow;
    if (this.#audio === undefined || block.channels.length === 0)
      throw new Error("audio track is not configured");
    const frames = block.channels[0]?.length ?? 0;
    if (block.channels.some((channel) => channel.length !== frames))
      throw new RangeError("audio channels have unequal lengths");
    const rate = this.#audioRate;
    if (
      this.#audioConverter === undefined &&
      (rate === undefined || rate === block.sampleRate)
    ) {
      await this.#encodeAudio(
        block.channels,
        Number(block.startSample),
        block.sampleRate,
      );
      return;
    }
    if (this.#audioConverter === undefined) {
      const binding = this.#audioResampler;
      if (binding === undefined || rate === undefined)
        throw new RangeError(
          `audioResampler is required to encode ${block.sampleRate} Hz audio at ${rate} Hz`,
        );
      this.#audioConverter = new EncoderRateConverter(binding, block, rate);
    }
    const converted = this.#audioConverter.push(block);
    if (converted !== null)
      await this.#encodeAudio(
        converted.channels,
        converted.startSample,
        this.#audioConverter.targetRate,
      );
  }

  /** Encodes the converted audio still held, once, before the audio track ends. */
  async #flushAudio(): Promise<void> {
    const converter = this.#audioConverter;
    if (converter === undefined || this.#audioFlushed) return;
    this.#audioFlushed = true;
    try {
      const rest = converter.finish();
      if (rest !== null)
        await this.#encodeAudio(
          rest.channels,
          rest.startSample,
          converter.targetRate,
        );
    } finally {
      converter.free();
    }
  }

  /** Encodes planar `channels` starting at sample `startSample` of a `sampleRate` stream. */
  async #encodeAudio(
    channels: readonly Float32Array[],
    startSample: number,
    sampleRate: number,
  ): Promise<void> {
    if (this.#audio === undefined)
      throw new Error("audio track is not configured");
    const frames = channels[0]?.length ?? 0;
    const planar = new Float32Array(frames * channels.length);
    channels.forEach((channel, index) => planar.set(channel, index * frames));
    const sample = new AudioSample({
      data: planar,
      format: "f32-planar",
      numberOfChannels: channels.length,
      sampleRate,
      timestamp: startSample / sampleRate,
    });
    try {
      await this.#audio.add(sample);
    } finally {
      sample.close();
    }
  }

  /**
   * Finalizes the file and commits a stream destination; calling it again after it resolved does
   * nothing. Rejects with `RESOURCE_LIMIT` when a memory destination passed `maxBytes`, with
   * `ENCODE_STALLED` when the encoder stalls, and with what the muxer or the destination refused; a
   * stream destination is then discarded.
   */
  async finish(): Promise<void> {
    if (this.#finished) return;
    await this.#ready;
    try {
      await this.#flushAudio();
      await this.#watched(this.#output.finalize());
      if (this.#overflow) throw this.#overflow;
      await this.#file?.close();
    } catch (error) {
      await this.#discard(error).catch(() => {});
      throw error;
    }
    this.#finished = true;
  }

  /**
   * Cancels the output and discards a stream destination, unless the file already finished.
   * Rejects when the destination refuses to be discarded.
   */
  async abort(reason: Error): Promise<void> {
    if (this.#finished) return;
    if (this.#audioConverter !== undefined && !this.#audioFlushed) {
      this.#audioFlushed = true;
      this.#audioConverter.free();
    }
    try {
      if (this.#output.state !== "canceled") await this.#output.cancel();
    } finally {
      await this.#discard(reason);
    }
  }

  /** Discards a stream destination once: writes still queued are dropped and nothing commits. */
  async #discard(reason: unknown): Promise<void> {
    if (this.#file === undefined || this.#discarded) return;
    this.#discarded = true;
    await this.#file.abort(reason);
  }

  /**
   * The finished output: its length for a stream, the bytes in memory. Null until {@link finish}
   * resolves.
   */
  get output(): Mp4Output | null {
    if (!this.#finished) return null;
    if (this.#target instanceof BufferTarget) {
      const buffer = this.#target.buffer;
      return buffer === null ? null : { kind: "memory", buffer };
    }
    return { kind: "stream", bytes: this.#written };
  }
  /** Bytes muxed so far: the file's length as written. */
  get bytesWritten(): number {
    return this.#written;
  }
}

/**
 * Factory kept in the optional subpath so mediabunny is never loaded by core APIs. Audio needs
 * `alpha2/wasm`'s resampler: see {@link MediabunnyAudioDecoder}.
 */
export class MediabunnyAdapterFactory implements OptionalMediaAdapterFactory {
  readonly #audioResampler?: AudioResamplerBinding;

  constructor(audioResampler?: AudioResamplerBinding) {
    this.#audioResampler = audioResampler;
  }

  /**
   * Opens a {@link MediabunnyVideoDecoder} with default options. Rejects with `TypeError` unless
   * `input` is a Blob, ArrayBuffer, or ArrayBuffer view, and as {@link MediabunnyVideoDecoder.open}
   * does.
   */
  async openVideo(
    sourceId: string,
    input: unknown,
  ): Promise<BrowserVideoDecoder> {
    if (
      !(input instanceof Blob) &&
      !(input instanceof ArrayBuffer) &&
      !ArrayBuffer.isView(input)
    ) {
      throw new TypeError(
        "mediabunny input must be a Blob or ArrayBuffer view",
      );
    }
    return MediabunnyVideoDecoder.open(sourceId, input);
  }

  /**
   * Opens a {@link MediabunnyAudioDecoder}. Rejects with `TypeError` for an input that is not a
   * Blob, ArrayBuffer, or ArrayBuffer view, or when the factory was built without a resampler
   * binding, and as {@link MediabunnyAudioDecoder.open} does.
   */
  async openAudio(
    sourceId: string,
    input: unknown,
  ): Promise<BrowserAudioDecoder> {
    if (
      !(input instanceof Blob) &&
      !(input instanceof ArrayBuffer) &&
      !ArrayBuffer.isView(input)
    )
      throw new TypeError(
        "mediabunny input must be a Blob or ArrayBuffer view",
      );
    if (this.#audioResampler === undefined)
      throw new TypeError(
        "mediabunny audio needs alpha2/wasm: pass it to the MediabunnyAdapterFactory",
      );
    return MediabunnyAudioDecoder.open(sourceId, input, this.#audioResampler);
  }
}

function ticksToSampleFloor(ticks: bigint, sampleRate: number): number {
  return Number((ticks * BigInt(sampleRate)) / 120_000n);
}

function ticksToSampleCeil(ticks: bigint, sampleRate: number): number {
  const rate = BigInt(sampleRate);
  return Number((ticks * rate + 119_999n) / 120_000n);
}

function throwIfAborted(signal?: AbortSignal): void {
  if (signal?.aborted === true) {
    throw signal.reason ?? new DOMException("Aborted", "AbortError");
  }
}

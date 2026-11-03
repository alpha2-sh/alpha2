import { throwIfAborted } from "./abort.js";
import {
  ActiveStopwatch,
  activeTimeout,
  withinActiveTime,
} from "./active-clock.js";
import {
  type ByteReader,
  readAudioSpeakers,
  speakersWithLfe,
} from "./audio-layout.js";
import { type ChromaSiting, chromaSitingForStream } from "./chroma-siting.js";
import { decoderOrderKey, decoderPlaneOrder } from "./decoder-order.js";
import type {
  AacEncoderProbe,
  EncoderProbeOutcome,
  VideoLatencyProbe,
} from "./export-metrics.js";

export type {
  AacEncoderProbe,
  AacProbeOutcome,
  EncoderProbeOutcome,
  VideoLatencyProbe,
} from "./export-metrics.js";
import { decodeFailureOf, withSoftwareFallback } from "./decoder-fallback.js";
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
  type InputAudioTrack,
  type InputTrack,
  type InputVideoTrack,
  MkvOutputFormat,
  MovOutputFormat,
  Mp4OutputFormat,
  Output,
  type OutputFormat,
  Quality,
  VideoSample,
  VideoSampleSink,
  VideoSampleSource,
  WebMOutputFormat,
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
import {
  DecodeUnavailableError,
  decodeRefusalMessage,
  isPlanarVideoFrame,
  PlanarDecodeUnavailableError,
  type TrackDecodeFacts,
} from "./hdr-frames.js";
import type { ColorMetadata } from "./generated/project-schema.js";
import type { AudioStreamFacts } from "./assets.js";
import type { BrowserAudioBlockSink } from "./audio.js";
import type { BrowserEncodeSink, ExportedFrame } from "./exporter.js";
import type { VideoRateControl } from "./rate-control.js";
import { type ExportContainer, resolveExportFormat } from "./containers.js";
import type { PositionedWrite } from "./export-destination.js";
import { ExportFailureError } from "./export-failure.js";

// Up to 2 s ahead decodes forward instead of seeking; also the longest step of a backward run.
const MAX_SEQUENTIAL_DECODE_GAP_TICKS = 240_000n;
const DEFAULT_MAX_DECODED_FRAME_BYTES = 64 * 1024 * 1024;
// See the same limit in media.ts: each frame holds a decoder surface from a small fixed pool.
const DEFAULT_MAX_CACHED_FRAMES = 2;
const MIN_CACHED_FRAMES = 2;
const DEFAULT_STALL_TIMEOUT_MS = 5_000;
const DEFAULT_ENCODER_STALL_TIMEOUT_MS = 10_000;
const DEFAULT_MAX_REVERSE_FRAME_BYTES = 128 * 1024 * 1024;
const TICKS_PER_SECOND = 120_000;

export type MediabunnyInput = Blob | ArrayBuffer | ArrayBufferView;

/** Limits for {@link MediabunnyVideoDecoder.open}, on top of the shared decoder options. */
export interface MediabunnyVideoDecoderOptions extends BrowserVideoDecoderOptions {
  /**
   * Bytes of copied frames for backward playback (default 128 MiB); copies hold no decoder
   * surfaces.
   */
  readonly maxReverseFrameBytes?: number;
  /** Default `"no-preference"`; a failing decoder is replaced once by a software one. */
  readonly hardwareAcceleration?: HardwareAcceleration;
}

/**
 * Optional mediabunny demux/decode adapter for alpha2's decoder contract. Forward requests share
 * one sequential decode; a backward run (reversed clip, ping-pong loop) decodes windows of frames
 * copied out of the decoder, so each keyframe interval is decoded a few times instead of per frame.
 */
export class MediabunnyVideoDecoder implements BrowserVideoDecoder {
  readonly #sourceId: string;
  readonly #input: Input;
  readonly #track: InputVideoTrack;
  #sink: VideoSampleSink;
  #hardwareAcceleration: HardwareAcceleration;
  #softwareFallback?: string;
  #decodedAny = false;
  readonly #packets: EncodedPacketSink;
  readonly #chromaSiting: ChromaSiting;
  readonly #stallTimeoutMs: number;
  #iterator?: AsyncIterator<VideoSample>;
  #generation?: number;
  #lastTargetTicks?: bigint;
  #cursorTicks?: bigint;
  #leadTicks?: bigint;
  #headSinceSeek = false;
  #backwardSteps = 0;
  #closed = false;
  readonly #orientation: VideoOrientation;
  readonly #cache: MediabunnyFrameWindow;
  readonly #reverse: ReverseFrameWindow;
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
   * Opens `resource`'s primary video track. Rejects when it has none or this browser cannot decode
   * it; with `"prefer-software"` and no software decoder, with `code:
   * "software_decode_unavailable"`.
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
      if (!(await track.canDecode())) {
        const facts = await videoTrackFacts(track);
        throw new DecodeUnavailableError(
          decodeRefusalMessage(sourceId, facts),
          facts,
        );
      }
      // `canDecode` does not ask for a software decoder, so a hardware-only codec would open and
      // then fail on its first read.
      if (
        options.hardwareAcceleration === "prefer-software" &&
        typeof VideoDecoder !== "undefined"
      ) {
        const config = await track.getDecoderConfig();
        const support =
          config === null
            ? { supported: false }
            : await VideoDecoder.isConfigSupported({
                ...config,
                hardwareAcceleration: "prefer-software",
              });
        if (support.supported !== true)
          throw Object.assign(
            new Error(
              `this browser has no software decoder for ${config?.codec ?? track.codec ?? "the video track"}`,
            ),
            { code: "software_decode_unavailable" },
          );
      }
      // mediabunny reports no mirroring and reads mirrored matrices differently from `FFmpeg`.
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

  get cachedFrameBytes(): number {
    return this.#cache.bytes + this.#reverse.bytes;
  }

  get reverseCacheHits(): number {
    return this.#reverseCacheHits;
  }

  get reverseWindowsBuilt(): number {
    return this.#reverseWindowsBuilt;
  }

  /** Why decoding moved to software after the browser's first pick failed, if it did. */
  get softwareFallback(): string | undefined {
    return this.#softwareFallback;
  }

  /**
   * The frame presented at the request's time. A decoder failure (Chromium's VA-API decoders report
   * `EncodingError`) retries once with a software decoder; if that fails, rejects with
   * {@link DecodeUnavailableError} when no frame was ever decoded, else with the failure.
   */
  async refill(
    request: DecoderRefillRequest,
    signal?: AbortSignal,
  ): Promise<BrowserDecodedFrame> {
    try {
      const frame = await withSoftwareFallback(
        () => this.#refill(request, signal),
        this.#closed ||
          this.#hardwareAcceleration === "prefer-software" ||
          signal?.aborted === true
          ? undefined
          : async (cause) => {
              await this.#resetIterator();
              this.#cache.clear();
              this.#reverse.clear();
              this.#backwardSteps = 0;
              this.#hardwareAcceleration = "prefer-software";
              this.#softwareFallback = `${cause.name}: ${cause.message}`;
              this.#sink = new VideoSampleSink(this.#track, {
                hardwareAcceleration: "prefer-software",
              });
            },
      );
      this.#decodedAny = true;
      return frame;
    } catch (error) {
      if (this.#decodedAny || this.#closed || signal?.aborted === true)
        throw error;
      throw decodeFailureOf(
        error,
        this.#sourceId,
        await videoTrackFacts(this.#track),
        false,
      );
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
    const target =
      this.#leadTicks !== undefined && request.mediaTimeTicks < this.#leadTicks
        ? this.#leadTicks
        : request.mediaTimeTicks;
    const last = this.#lastTargetTicks;
    const backward = last !== undefined && target < last;
    // A longer jump back (scrubbing) lands beyond any window the last one left.
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
      this.#lastTargetTicks = target;
      this.#reverseCacheHits += 1;
      return answer(windowed);
    }
    const seek =
      request.reason === "seek" || request.reason === "device-recovery";
    // One step back is a seek (a loop's wrap); two in a row is a backward run.
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
      // Starting at the keyframe makes the stall timeout measure the decoder, not the GOP length.
      const start = await this.#keyframeStart(target + this.#slack);
      this.#iterator = this.#sink
        .samples(start.seconds)
        [Symbol.asyncIterator]();
      this.#generation = request.generation;
      this.#headSinceSeek = start.head;
    }
    this.#lastTargetTicks = target;
    this.#cursorTicks = target;
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
          // From the first keyframe, a late first frame means the stream starts there; from any
          // other keyframe it is a bad key index and answers this request only.
          if (head) this.#leadTicks = ticks;
          const first = this.#cache.presentedAt(ticks);
          if (first !== undefined) return answer(first);
        }
      } finally {
        sample.close();
      }
    }
  }

  async flush(): Promise<void> {
    await this.#resetIterator();
    this.#cache.clear();
    this.#reverse.clear();
    this.#backwardSteps = 0;
  }

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

  /** Drops the previous window and the forward cursor, then decodes and copies a new window. */
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
   * Rejects when the decoder produces nothing within the stall timeout, since a hardware decoder
   * with all surfaces held waits silently. The iterator is then ended and a late frame closed.
   */
  async #nextSample(
    iterator: AsyncIterator<VideoSample>,
    signal?: AbortSignal,
  ): Promise<IteratorResult<VideoSample>> {
    const pending = iterator.next();
    const settled = new AbortController();
    let abort: (() => void) | undefined;
    const interrupted = new Promise<never>((_, reject) => {
      // Active time, so a frozen page does not count as a stalled decoder.
      void activeTimeout(this.#stallTimeoutMs, settled.signal).then(() =>
        reject(
          new Error(
            `video decoder ${this.#sourceId} stalled: no frame for ${this.#stallTimeoutMs} ms`,
          ),
        ),
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
      settled.abort();
      if (abort !== undefined) signal?.removeEventListener("abort", abort);
    }
  }

  async #keyframeSeconds(target: bigint): Promise<number> {
    return (await this.#keyframeStart(target)).seconds;
  }

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
  /** The stream's last frame once decoding reached the end; answers every later time. */
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

  markEnd(): void {
    this.#endTicks = this.#entries.at(-1)?.ticks;
  }

  /** A clone of the presented frame, or `undefined` when no later frame proves it yet. */
  presentedAt(target: bigint, final = false): PresentedFrame | undefined {
    const deadline = target + this.#slack;
    let index = -1;
    for (const [position, entry] of this.#entries.entries()) {
      if (entry.ticks > deadline) break;
      index = position;
    }
    const entry = this.#entries[index];
    if (entry === undefined) return undefined;
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
  readonly start: bigint;
  /** `undefined` until the next frame is read, `null` when none follows. */
  end: bigint | null | undefined;
  readonly bytes: number;
}

/** Owned copies of consecutive decoded frames; evicts the earliest first. */
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

  span(sample: VideoSample): bigint {
    const bytes = sampleBytes(sample);
    const frames = Math.max(1, Math.floor(this.#limit / bytes));
    // Unknown durations count as 30 fps; a wrong guess costs copies, never a wrong frame.
    const duration =
      sample.microsecondDuration > 0 ? sample.microsecondDuration : 33_333;
    return microsecondsToTicks(frames * duration);
  }

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

  end(ticks: bigint): void {
    const newest = this.#entries.at(-1);
    if (newest !== undefined && newest.end === undefined) newest.end = ticks;
  }

  seal(): void {
    const newest = this.#entries.at(-1);
    if (newest !== undefined && newest.end === undefined) newest.end = null;
  }

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

function opusPreSkip(description: Uint8Array | null): number {
  if (
    description === null ||
    description.length < 12 ||
    new TextDecoder().decode(description.subarray(0, 8)) !== "OpusHead"
  )
    return 0;
  return description[10] | (description[11] << 8);
}

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

/** A copy that holds no decoder resources; opaque (GPU) frames are drawn into a bitmap. */
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
      // `transfer` is missing from the DOM typings.
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

function sampleBytes(sample: VideoSample): number {
  try {
    return Math.max(1, sample.allocationSize());
  } catch {
    return Math.max(1, sample.codedWidth * sample.codedHeight * 4);
  }
}

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

/** The part of `alpha2/wasm` the audio adapter resamples with. */
export interface AudioResamplerBinding {
  readonly AudioResampler: new (
    sourceRate: number,
    targetRate: number,
  ) => AudioResampler;
}

/**
 * One `alpha2/wasm` `AudioResampler`. Source positions are at the source rate, outputs at the
 * project rate.
 */
export interface AudioResampler {
  sourceStart(targetStart: number): number;
  sourceEnd(targetEnd: number): number;
  targetAtOrAfter(source: number): number;
  /** The source is silent outside `input`. */
  resample(
    input: Float32Array,
    inputStart: number,
    targetStart: number,
    frames: number,
  ): Float32Array;
  free?(): void;
}

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
  // `SharedArrayBuffer` is undefined without cross-origin isolation.
  if (ArrayBuffer.isView(source))
    return new Uint8Array(source.buffer, source.byteOffset, source.byteLength);
  return new Uint8Array(source);
}

const SOURCE_SAMPLE_RATES = { min: 1_000, max: 768_000 } as const;

// Decoding starts this early because each mediabunny iteration starts a fresh decoder without
// history (AAC's first frame lacks the previous frame's overlap).
const AUDIO_PREROLL_SECONDS = 0.1;
// Opus converges slowly after a seek: 100 ms early is still 2e-2 off, 400 ms is under 1e-6.
const OPUS_PREROLL_SECONDS = 0.4;

const DEFAULT_PROBE_TIMEOUT_MS = 5_000;

function codecBitDepth(codec: string | null): number | undefined {
  if (codec === null) return undefined;
  const parts = codec.split(".");
  const family = parts[0]?.toLowerCase();
  if (family === "av01" || family === "vp09") {
    const depth = Number(parts[3]?.replace(/\D/g, ""));
    return Number.isInteger(depth) && depth > 0 ? depth : undefined;
  }
  if (family === "hvc1" || family === "hev1")
    return parts[1]?.replace(/\D/g, "") === "2" ? 10 : 8;
  if (family === "avc1" || family === "avc3") {
    const profile = Number.parseInt(parts[1]?.slice(0, 2) ?? "", 16);
    return profile === 110 || profile === 122 || profile === 244 ? 10 : 8;
  }
  return undefined;
}

/** The facts a refusal message names ({@link decodeRefusalMessage}), with `extra` over them. */
export async function videoTrackFacts(
  track: InputVideoTrack,
  extra: Partial<TrackDecodeFacts> = {},
): Promise<TrackDecodeFacts> {
  const codec =
    (await track.getCodecParameterString().catch(() => null)) ??
    track.codec ??
    null;
  const space = await track.getColorSpace().catch(() => undefined);
  const bitDepth = codecBitDepth(codec);
  return {
    kind: "video",
    codec,
    ...(bitDepth === undefined ? {} : { bitDepth }),
    transfer: space?.transfer ?? null,
    ...extra,
  };
}

/** Rejects after `timeoutMs` of active time; a late value from `work` goes to `onLate`. */
async function withinProbeTime<T>(
  work: Promise<T>,
  timeoutMs: number,
  what: string,
  onLate: (value: T) => void,
): Promise<T> {
  let timedOut = false;
  work.then(
    (value) => {
      if (timedOut) onLate(value);
    },
    () => {},
  );
  return withinActiveTime(work, timeoutMs, () => {
    timedOut = true;
    return new Error(`${what} did not answer within ${timeoutMs} ms`);
  });
}

async function firstFramePlanes(
  track: InputVideoTrack,
  hardwareAcceleration: "no-preference" | "prefer-software",
  timeoutMs: number,
): Promise<{ planar: boolean; format: VideoPixelFormat | null }> {
  const first = await new EncodedPacketSink(track).getFirstPacket({
    metadataOnly: true,
  });
  if (first === null) throw new Error("the video track has no packets");
  const sink = new VideoSampleSink(track, { hardwareAcceleration });
  const sample = await withinProbeTime(
    sink.getSample(first.timestamp),
    timeoutMs,
    "the video decoder",
    (late) => late?.close(),
  );
  if (sample === null) throw new Error("the video decoder gave no frame");
  try {
    const frame = sample.toVideoFrame();
    try {
      let planar = isPlanarVideoFrame(frame);
      if (planar) {
        try {
          frame.allocationSize();
        } catch {
          planar = false;
        }
      }
      return { planar, format: frame.format };
    } finally {
      frame.close();
    }
  } finally {
    sample.close();
  }
}

/**
 * Whether this browser hands `resource`'s video over as planes the engine reads, for sources that
 * need them (HDR, or a `declared` color). Resolves `"not-required"`, `"decoder"`, or `"software"`
 * when only a software decoder gives planar frames. Each decode waits at most `timeoutMs`
 * (default 5000). Rejects with {@link PlanarDecodeUnavailableError} when no readable frames
 * decode, and with {@link DecodeUnavailableError} when there is no video track.
 */
export async function probePlanarDecode(
  resource: MediabunnyInput,
  options: { declared: ColorMetadata | null; timeoutMs?: number },
): Promise<{ planes: "not-required" | "decoder" | "software" }> {
  const timeoutMs = options.timeoutMs ?? DEFAULT_PROBE_TIMEOUT_MS;
  const name = resource instanceof File ? resource.name : "the video";
  const source: Source =
    resource instanceof Blob
      ? new BlobSource(resource)
      : new BufferSource(resource);
  const input = new Input({ source, formats: ALL_FORMATS });
  try {
    const track = await input.getPrimaryVideoTrack();
    if (track === null) {
      const facts: TrackDecodeFacts = { kind: "video", codec: null };
      throw new DecodeUnavailableError(`${name} has no video track`, facts);
    }
    const facts = await videoTrackFacts(track);
    const hdr = facts.transfer === "pq" || facts.transfer === "hlg";
    if (!hdr && options.declared === null) return { planes: "not-required" };
    if (!(await track.canDecode()))
      throw new PlanarDecodeUnavailableError(
        decodeRefusalMessage(name, facts),
        facts,
      );
    let decoded: { planar: boolean; format: VideoPixelFormat | null };
    try {
      decoded = await firstFramePlanes(track, "no-preference", timeoutMs);
    } catch (cause) {
      const refused = {
        ...facts,
        browserError: cause instanceof Error ? cause.message : String(cause),
      };
      throw new PlanarDecodeUnavailableError(
        decodeRefusalMessage(name, refused),
        refused,
        { cause },
      );
    }
    if (decoded.planar) return { planes: "decoder" };
    const config = await track.getDecoderConfig();
    const software =
      typeof VideoDecoder !== "undefined" &&
      config !== null &&
      (
        await VideoDecoder.isConfigSupported({
          ...config,
          hardwareAcceleration: "prefer-software",
        })
      ).supported === true;
    if (software) {
      const again = await firstFramePlanes(
        track,
        "prefer-software",
        timeoutMs,
      ).catch(() => undefined);
      if (again?.planar === true) return { planes: "software" };
    }
    const refused: TrackDecodeFacts = {
      ...facts,
      frameFormat: decoded.format,
      softwareDecoder: software ? "opaque" : "none",
    };
    throw new PlanarDecodeUnavailableError(
      decodeRefusalMessage(name, refused),
      refused,
    );
  } finally {
    input.dispose();
  }
}

const AUDIO_TRIAL_DECODE_MS = 2_000;

/** -80 dBFS. */
const SILENT_PLANE_RMS = 1e-4;

export interface AudioTrialDecode {
  readonly channels: number;
  readonly sampleRate: number;
  readonly planeRms: readonly number[];
}

/** An audio track this browser decodes, as measured by a trial decode. */
export interface DecodableAudioTrack {
  readonly channels: number;
  readonly sampleRate: number;
  readonly speakers: AudioSpeaker[] | null;
  /** The decoder delivers one silent plane more than declared (an added LFE). */
  readonly addedLfe: boolean;
  readonly trial: AudioTrialDecode;
}

/** Resolves to null for a track with no packet; rejects on decoder error or timeout. */
async function trialAudioDecode(
  track: InputAudioTrack,
): Promise<AudioTrialDecode | null> {
  const samples = new AudioSampleSink(track).samples();
  let settled = false;
  try {
    const first = await withinActiveTime(
      samples.next(),
      AUDIO_TRIAL_DECODE_MS,
      () =>
        new DOMException(
          `the audio decoder gave nothing within ${AUDIO_TRIAL_DECODE_MS / 1000} s`,
          "TimeoutError",
        ),
    );
    settled = true;
    if (first.done === true) return null;
    const sample = first.value;
    try {
      const plane = new Float32Array(sample.numberOfFrames);
      const planeRms = Array.from(
        { length: sample.numberOfChannels },
        (_, index) => {
          sample.copyTo(plane, { planeIndex: index, format: "f32-planar" });
          let sum = 0;
          for (const value of plane) sum += value * value;
          return Math.sqrt(sum / Math.max(1, plane.length));
        },
      );
      return {
        channels: sample.numberOfChannels,
        sampleRate: sample.sampleRate,
        planeRms,
      };
    } finally {
      sample.close();
    }
  } finally {
    // A decoder that never answered may never let the iterator end; leave it to input disposal.
    if (settled) await samples.return(undefined);
    else void samples.return(undefined).catch(() => {});
  }
}

/**
 * Whether this browser decodes `track`, by a trial decode of its first packet (Firefox's
 * `canDecode()` says true for AAC layouts it then rejects), and the channel count, rate and
 * speakers it delivers. A silent extra plane is taken as an added LFE (Edge decodes E-AC-3 5.0 as
 * 5.1). Rejects with {@link DecodeUnavailableError} when the track does not decode as declared.
 */
export async function probeAudioDecode(
  name: string,
  track: InputAudioTrack,
  declared: readonly AudioSpeaker[] | null,
): Promise<DecodableAudioTrack> {
  const codec =
    (await track.getCodecParameterString().catch(() => null)) ??
    track.codec ??
    null;
  const facts: TrackDecodeFacts = {
    kind: "audio",
    codec,
    channels: track.numberOfChannels,
    ...(declared === null ? {} : { layout: [...declared] }),
  };
  const refuse = (detail: string, cause?: unknown): DecodeUnavailableError =>
    new DecodeUnavailableError(
      decodeRefusalMessage(name, { ...facts, browserError: detail }),
      { ...facts, browserError: detail },
      cause === undefined ? undefined : { cause },
    );
  if (!(await track.canDecode().catch(() => false)))
    throw refuse("the browser has no decoder for this audio");
  let trial: AudioTrialDecode | null;
  try {
    trial = await trialAudioDecode(track);
  } catch (error) {
    throw refuse(
      `the trial decode failed: ${String((error as Error)?.message ?? error)}`,
      error,
    );
  }
  if (trial === null)
    return {
      channels: track.numberOfChannels,
      sampleRate: track.sampleRate,
      speakers: declared === null ? null : [...declared],
      addedLfe: false,
      trial: {
        channels: track.numberOfChannels,
        sampleRate: track.sampleRate,
        planeRms: [],
      },
    };
  if (trial.channels === track.numberOfChannels)
    return {
      channels: trial.channels,
      sampleRate: trial.sampleRate,
      speakers: declared === null ? null : [...declared],
      addedLfe: false,
      trial,
    };
  const withLfe =
    declared !== null && trial.channels === declared.length + 1
      ? speakersWithLfe(declared)
      : null;
  if (withLfe !== null && trial.planeRms[withLfe.lfePlane] <= SILENT_PLANE_RMS)
    return {
      channels: trial.channels,
      sampleRate: trial.sampleRate,
      speakers: withLfe.speakers,
      addedLfe: true,
      trial,
    };
  throw refuse(
    `the decoder delivers ${trial.channels} channels for a source that declares ${track.numberOfChannels}`,
  );
}

/**
 * Every audio stream of `resource` in container order, as an `AssetEntry.audio_streams` lists
 * them: a clip's `audio.stream` counts over these, and `default` marks the primary track. Empty
 * without audio.
 */
export async function probeAudioStreams(
  resource: MediabunnyInput,
): Promise<AudioStreamFacts[]> {
  const input = new Input({
    source:
      resource instanceof Blob
        ? new BlobSource(resource)
        : new BufferSource(resource),
    formats: ALL_FORMATS,
  });
  try {
    const tracks = await input.getAudioTracks();
    const primary = await input.getPrimaryAudioTrack();
    return await Promise.all(
      tracks.map(async (track) => {
        const language = await track.getLanguageCode();
        const title = await track.getName();
        return {
          channels: track.numberOfChannels,
          default: track === primary,
          ...(track.codec === null ? {} : { codec: track.codec }),
          ...(language === "und" ? {} : { language }),
          ...(title ? { title } : {}),
        };
      }),
    );
  } finally {
    input.dispose();
  }
}

/** {@link probeAudioDecode} on the primary audio track; rejects with `Error` when there is none. */
export async function probeAudioSource(
  name: string,
  resource: MediabunnyInput,
): Promise<DecodableAudioTrack> {
  const input = new Input({
    source:
      resource instanceof Blob
        ? new BlobSource(resource)
        : new BufferSource(resource),
    formats: ALL_FORMATS,
  });
  try {
    const track = await input.getPrimaryAudioTrack();
    if (track === null) throw new Error("mediabunny input has no audio track");
    const config = await track.getDecoderConfig();
    const declared = await readAudioSpeakers(
      byteReader(resource),
      byteLength(resource),
      track,
      bufferBytes(config?.description),
      () =>
        new EncodedPacketSink(track)
          .getFirstPacket()
          .then((packet) => packet?.data ?? null),
    );
    return await probeAudioDecode(name, track, declared);
  } finally {
    input.dispose();
  }
}

/**
 * Optional mediabunny audio demux/decode adapter with project-rate resampling, placing samples as
 * a native host does (including the {@link audioFrameGrid}). Keeps every channel, 1 to
 * {@link MAX_AUDIO_CHANNELS}, reordered to `FFmpeg`'s plane order where a browser decoder differs
 * (Safari's AAC).
 */
export class MediabunnyAudioDecoder implements BrowserAudioDecoder {
  readonly #sourceId: string;
  readonly #input: Input;
  readonly #sink: AudioSampleSink;
  readonly #channelCount: number;
  /**
   * The speaker each plane feeds. `null` when the source declares no layout; the engine then
   * assumes `FFmpeg`'s default for the channel count.
   */
  readonly speakers: readonly AudioSpeaker[] | null;
  /**
   * Delivered plane to speaker-order index ({@link decoderPlaneOrder}); `undefined` if identity.
   */
  readonly #planeOrder: readonly number[] | undefined;
  readonly #sourceRate: number;
  readonly #grid: AudioFrameGrid | undefined;
  readonly #preroll: number;
  /** Matroska `CodecDelay` in seconds, which mediabunny does not subtract. */
  readonly #delay: number;
  readonly #binding: AudioResamplerBinding;
  readonly #facts: TrackDecodeFacts;
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
    facts: TrackDecodeFacts,
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
    this.#facts = facts;
  }

  /**
   * Opens `resource`'s primary audio track, or with `options.stream` the audio track at that
   * position among the file's audio tracks in container order (a clip's `audio.stream`). Rejects
   * when there is no such track, with `RangeError` for more than {@link MAX_AUDIO_CHANNELS}
   * channels or a rate outside 1 kHz to 768 kHz, and with {@link DecodeUnavailableError} when this
   * browser cannot decode it.
   */
  static async open(
    sourceId: string,
    resource: MediabunnyInput,
    binding: AudioResamplerBinding,
    options: { readonly stream?: number } = {},
  ): Promise<MediabunnyAudioDecoder> {
    const source: Source =
      resource instanceof Blob
        ? new BlobSource(resource)
        : new BufferSource(resource);
    const input = new Input({ source, formats: ALL_FORMATS });
    const track =
      options.stream === undefined
        ? await input.getPrimaryAudioTrack()
        : ((await input.getAudioTracks())[options.stream] ?? null);
    if (track === null) {
      input.dispose();
      throw new Error(
        options.stream === undefined
          ? "mediabunny input has no audio track"
          : `mediabunny input has no audio stream ${options.stream}`,
      );
    }
    if (
      track.numberOfChannels < 1 ||
      track.numberOfChannels > MAX_AUDIO_CHANNELS
    ) {
      input.dispose();
      throw new RangeError(
        `audio with ${track.numberOfChannels} channels is unsupported: the engine takes 1 to ${MAX_AUDIO_CHANNELS}`,
      );
    }
    let speakers: AudioSpeaker[] | null;
    let description: Uint8Array | null;
    let planeOrder: readonly number[] | null;
    let decodable: DecodableAudioTrack;
    try {
      const config = await track.getDecoderConfig();
      description = bufferBytes(config?.description);
      // An AC-3, E-AC-3 or DTS stream declares its layout in its frames.
      let firstPacket: Promise<Uint8Array | null> | undefined;
      const readFirstPacket = () =>
        (firstPacket ??= new EncodedPacketSink(track)
          .getFirstPacket()
          .then((packet) => packet?.data ?? null));
      const declared = await readAudioSpeakers(
        byteReader(resource),
        byteLength(resource),
        track,
        description,
        readFirstPacket,
      );
      decodable = await probeAudioDecode(sourceId, track, declared);
      speakers = decodable.speakers;
      // The measured order does not cover an added LFE, which the decoder puts in native order.
      planeOrder = decodable.addedLfe
        ? null
        : await decoderPlaneOrder(
            decoderOrderKey(
              track.codec,
              description,
              track.numberOfChannels,
              track.codec === "ac3" || track.codec === "eac3"
                ? await readFirstPacket()
                : null,
            ),
          );
    } catch (error) {
      input.dispose();
      throw error;
    }
    const channelCount = decodable.channels;
    if (channelCount > MAX_AUDIO_CHANNELS) {
      input.dispose();
      throw new RangeError(
        `audio decoded to ${channelCount} channels is unsupported: the engine takes 1 to ${MAX_AUDIO_CHANNELS}`,
      );
    }
    // An implicitly signalled AAC extension decodes at twice the header's rate.
    const sourceRate = decodable.sampleRate;
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
      // Native hosts subtract `CodecDelay`, so the samples line up with the same encode in MP4.
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
      // WebCodecs Opus drops the pre-skip from the first frame after each configure.
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
      {
        kind: "audio",
        codec: track.codec ?? null,
        channels: track.numberOfChannels,
        ...(speakers === null ? {} : { layout: speakers }),
      },
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
   * Decodes the interval at `projectSampleRate`, plus two samples each side; silence outside the
   * stream. Rejects when closed, for another source or empty interval, on abort, or when the track
   * changes format mid-stream; with {@link DecodeUnavailableError} when the browser's decoder
   * fails.
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
    let streamStart: number | undefined;
    let expected: number | undefined;
    const delay = this.#delay;
    try {
      for await (const sample of this.#sink.samples(
        decodeFrom + delay,
        sourceEnd / rate + delay,
      )) {
        try {
          throwIfAborted(signal);
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
            if (this.#grid !== undefined)
              at = placeOnAudioFrameGrid(this.#grid, at, length);
            // mediabunny starts at the packet holding the requested time, so a later first sample
            // is the stream's first.
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
    } catch (error) {
      if (
        typeof DOMException !== "undefined" &&
        error instanceof DOMException &&
        error.name !== "AbortError"
      ) {
        const facts = { ...this.#facts, browserError: error.message };
        throw new DecodeUnavailableError(
          decodeRefusalMessage(this.#sourceId, facts),
          facts,
          { cause: error },
        );
      }
      throw error;
    }
    // A band-limited reading of the edge samples would ring past the stream.
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

  close(): void {
    if (!this.#closed) {
      this.#closed = true;
      this.#resampler?.resampler.free?.();
      this.#resampler = undefined;
      this.#input.dispose();
    }
  }
}

/** At least one of `videoCodec` and `audioCodec` is required. */
export interface MediabunnySinkOptions {
  /** The codecs must be ones the container offers (see `alpha2/containers`). */
  readonly container: ExportContainer;
  /** Adds a video track in this codec; requires `videoRateControl`. */
  readonly videoCodec?: "avc" | "hevc" | "vp9" | "av1";
  /**
   * Exact project frame rate; without it the muxer derives durations from float deltas and judders.
   */
  readonly frameRate?: { readonly num: number; readonly den: number };
  /** Adds an audio track in this codec; requires `audioBitrate`. */
  readonly audioCodec?: "aac" | "opus";
  /**
   * Required with `videoCodec`. With a `quantizer`, constant-quantizer encoding is tried first,
   * falling back to `bitrate`; `appliedVideoRateControl` says which ran.
   */
  readonly videoRateControl?: VideoRateControl;
  /** Normally {@link canEncodeVideoTrack}'s `codecString`; defaults to mediabunny's choice. */
  readonly videoCodecString?: string;
  /** Keep alpha (VP9 in WebM or Matroska). Check {@link canEncodePixels} first. */
  readonly videoAlpha?: boolean;
  /** 10 encodes `I420P10` frames with {@link tenBitCodecString}. Default 8. */
  readonly videoBitDepth?: 8 | 10;
  /** Defaults to what {@link videoEncoderLatencyMode} measures. */
  readonly videoLatencyMode?: VideoEncoderLatencyMode;
  /** Default `"no-preference"`. Check {@link canEncodeInSoftware} before asking for software. */
  readonly videoHardwareAcceleration?: "no-preference" | "prefer-software";
  /** Writes reject with `ENCODE_STALLED` after this long without output (default 10 s). */
  readonly stallTimeoutMs?: number;
  /** Bits per second. Required with `audioCodec`. */
  readonly audioBitrate?: number;
  /** Normally {@link audioEncoderSampleRate}'s answer; defaults to the blocks' own rate. */
  readonly audioSampleRate?: number;
  /** Required when `audioSampleRate` differs from the blocks' rate. */
  readonly audioResampler?: AudioResamplerBinding;
  /**
   * `stream` writes through a seekable stream as it muxes, committed by
   * {@link MediabunnySink.finish} and discarded on failure. `memory` is for small outputs; past
   * `maxBytes` the export fails with `RESOURCE_LIMIT`.
   */
  readonly destination: ExportDestination;
}

/** Where a {@link MediabunnySink} writes. */
export type ExportDestination =
  | {
      readonly kind: "stream";
      readonly writable: WritableStream<PositionedWrite>;
    }
  | { readonly kind: "memory"; readonly maxBytes: number };

export function videoEncoderQuality(rate: VideoRateControl): Quality {
  return new Quality({
    bitrate: rate.bitrate,
    bitrateMode: rate.bitrateMode,
    ...(rate.quantizer === undefined ? {} : { quantizer: rate.quantizer }),
  });
}

export type AvcProfile = "High" | "Main" | "Constrained Baseline";

export interface VideoEncoderProfile {
  readonly profile: AvcProfile;
  readonly codecString: string;
}

/** A profile asked for and refused before the one the export opened. */
export interface VideoEncoderFallback {
  readonly encoder: string;
  readonly profile: AvcProfile;
  readonly reason: "unsupported";
  readonly detail: string;
}

/** How a video track is encoded, as {@link canEncodeVideoTrack} chose it. */
export interface VideoTrackEncoding {
  /** Undefined for codecs other than H.264, which open with mediabunny's own. */
  readonly codecString?: string;
  readonly profile?: AvcProfile;
  /** Profiles asked for first and refused; empty when High is encoded. */
  readonly fallbacks: readonly VideoEncoderFallback[];
}

const AVC_PROFILES: readonly {
  readonly profile: AvcProfile;
  readonly prefix: string;
}[] = [
  { profile: "High", prefix: "6400" },
  { profile: "Main", prefix: "4d00" },
  // constraint_set0, 1 and 2: decodable by Baseline, Main and Extended decoders.
  { profile: "Constrained Baseline", prefix: "42e0" },
];

/** H.264 levels by frame size in macroblocks and bitrate, as mediabunny picks them. */
const AVC_LEVELS: readonly {
  readonly macroblocks: number;
  readonly bitrate: number;
  readonly level: number;
}[] = [
  { macroblocks: 99, bitrate: 64_000, level: 0x0a },
  { macroblocks: 396, bitrate: 192_000, level: 0x0b },
  { macroblocks: 396, bitrate: 384_000, level: 0x0c },
  { macroblocks: 396, bitrate: 768_000, level: 0x0d },
  { macroblocks: 396, bitrate: 2_000_000, level: 0x14 },
  { macroblocks: 792, bitrate: 4_000_000, level: 0x15 },
  { macroblocks: 1_620, bitrate: 4_000_000, level: 0x16 },
  { macroblocks: 1_620, bitrate: 10_000_000, level: 0x1e },
  { macroblocks: 3_600, bitrate: 14_000_000, level: 0x1f },
  { macroblocks: 5_120, bitrate: 20_000_000, level: 0x20 },
  { macroblocks: 8_192, bitrate: 20_000_000, level: 0x28 },
  { macroblocks: 8_192, bitrate: 50_000_000, level: 0x29 },
  { macroblocks: 8_704, bitrate: 50_000_000, level: 0x2a },
  { macroblocks: 22_080, bitrate: 135_000_000, level: 0x32 },
  { macroblocks: 36_864, bitrate: 240_000_000, level: 0x33 },
  { macroblocks: 36_864, bitrate: 240_000_000, level: 0x34 },
  { macroblocks: 139_264, bitrate: 240_000_000, level: 0x3c },
  { macroblocks: 139_264, bitrate: 480_000_000, level: 0x3d },
  { macroblocks: 139_264, bitrate: 800_000_000, level: 0x3e },
];

/**
 * The H.264 profiles {@link canEncodeVideoTrack} tries, in order (High, Main, Constrained
 * Baseline), at the needed level. Empty for other codecs.
 */
export function videoEncoderProfiles(
  codec: "avc" | "hevc" | "vp9" | "av1",
  options: {
    readonly width: number;
    readonly height: number;
    readonly rate: VideoRateControl;
  },
): readonly VideoEncoderProfile[] {
  if (codec !== "avc") return [];
  const macroblocks =
    Math.ceil(options.width / 16) * Math.ceil(options.height / 16);
  const level = (
    AVC_LEVELS.find(
      (entry) =>
        macroblocks <= entry.macroblocks &&
        options.rate.bitrate <= entry.bitrate,
    ) ?? AVC_LEVELS[AVC_LEVELS.length - 1]
  ).level
    .toString(16)
    .padStart(2, "0");
  return AVC_PROFILES.map(({ profile, prefix }) => ({
    profile,
    codecString: `avc1.${prefix}${level}`,
  }));
}

/**
 * How this browser encodes `codec` video at `width`x`height` with `rate`, or `null` when it does
 * not. H.264 picks the first accepted of {@link videoEncoderProfiles}: pass its `codecString` to
 * the sink and report its `fallbacks`.
 */
export async function canEncodeVideoTrack(
  codec: "avc" | "hevc" | "vp9" | "av1",
  options: {
    readonly width: number;
    readonly height: number;
    readonly rate: VideoRateControl;
  },
): Promise<VideoTrackEncoding | null> {
  const encodes = (fullCodecString?: string) =>
    canEncodeVideo(codec, {
      width: options.width,
      height: options.height,
      quality: videoEncoderQuality(options.rate),
      ...(fullCodecString === undefined ? {} : { fullCodecString }),
    }).catch(() => false);
  const profiles = videoEncoderProfiles(codec, options);
  if (profiles.length === 0)
    return (await encodes()) ? { fallbacks: [] } : null;
  const fallbacks: VideoEncoderFallback[] = [];
  for (const { profile, codecString } of profiles) {
    if (await encodes(codecString)) return { codecString, profile, fallbacks };
    fallbacks.push({
      encoder: codecString,
      profile,
      reason: "unsupported",
      detail: `this browser does not encode H.264 ${profile} profile at ${options.width}x${options.height}`,
    });
  }
  return null;
}

/** Whether this browser has a software encoder for this configuration. */
export async function canEncodeInSoftware(
  codec: "avc" | "hevc" | "vp9" | "av1",
  options: {
    readonly width: number;
    readonly height: number;
    readonly rate: VideoRateControl;
    readonly codecString?: string;
  },
): Promise<boolean> {
  return canEncodeVideo(codec, {
    width: options.width,
    height: options.height,
    quality: videoEncoderQuality(options.rate),
    hardwareAcceleration: "prefer-software",
    ...(options.codecString === undefined
      ? {}
      : { fullCodecString: options.codecString }),
  }).catch(() => false);
}

/**
 * The 10-bit codec string for `codec`; `undefined` for H.264, which browsers do not encode at 10
 * bits.
 */
export function tenBitCodecString(
  codec: "avc" | "hevc" | "vp9" | "av1",
): string | undefined {
  switch (codec) {
    case "vp9":
      return "vp09.02.10.10";
    case "av1":
      return "av01.0.04M.10";
    case "hevc":
      return "hvc1.2.4.L123.B0";
    default:
      return undefined;
  }
}

const PIXEL_PROBE_WIDTH = 64;
const PIXEL_PROBE_HEIGHT = 64;
const PIXEL_PROBE_TIMEOUT_MS = 3_000;

/**
 * Whether this browser encodes `codec` in `pixels` (alpha, 10 bits, or both). Encodes one real
 * frame, since `isConfigSupported` can claim alpha that is then dropped.
 */
export async function canEncodePixels(
  codec: "avc" | "hevc" | "vp9" | "av1",
  pixels: { readonly bitDepth: 8 | 10; readonly alpha: boolean },
): Promise<boolean> {
  if (pixels.bitDepth === 8 && !pixels.alpha) return true;
  if (typeof VideoEncoder === "undefined" || typeof VideoFrame === "undefined")
    return false;
  const codecString =
    pixels.bitDepth === 10
      ? tenBitCodecString(codec)
      : LATENCY_PROBE_CODECS[codec];
  if (codecString === undefined) return false;
  const config: VideoEncoderConfig = {
    codec: codecString,
    width: PIXEL_PROBE_WIDTH,
    height: PIXEL_PROBE_HEIGHT,
    bitrate: 500_000,
    framerate: 30,
    ...(pixels.alpha ? { alpha: "keep" as const } : {}),
  };
  try {
    if (!(await VideoEncoder.isConfigSupported(config)).supported) return false;
  } catch {
    return false;
  }
  const format = `I420${pixels.alpha ? "A" : ""}${pixels.bitDepth === 10 ? "P10" : ""}`;
  const samples = PIXEL_PROBE_WIDTH * PIXEL_PROBE_HEIGHT;
  const bytes = pixels.bitDepth === 10 ? 2 : 1;
  const planes = (1.5 + (pixels.alpha ? 1 : 0)) * samples * bytes;
  let encoder: VideoEncoder | undefined;
  const probeDone = new AbortController();
  try {
    let kept = false;
    let failed = false;
    let settle = () => {};
    const output = new Promise<void>((resolve) => (settle = resolve));
    encoder = new VideoEncoder({
      output: (_chunk, metadata) => {
        // `alphaSideData` is newer than the DOM types this package builds with.
        const alpha = (metadata as { alphaSideData?: unknown } | undefined)
          ?.alphaSideData;
        kept = !pixels.alpha || alpha !== undefined;
        settle();
      },
      error: () => {
        failed = true;
        settle();
      },
    });
    encoder.configure(config);
    const frame = new VideoFrame(new Uint8Array(planes), {
      format: format as VideoPixelFormat,
      codedWidth: PIXEL_PROBE_WIDTH,
      codedHeight: PIXEL_PROBE_HEIGHT,
      timestamp: 0,
    });
    try {
      encoder.encode(frame, { keyFrame: true });
    } finally {
      frame.close();
    }
    await Promise.race([
      output,
      encoder.flush().catch(() => (failed = true)),
      activeTimeout(PIXEL_PROBE_TIMEOUT_MS, probeDone.signal),
    ]);
    return kept && !failed;
  } catch {
    return false;
  } finally {
    probeDone.abort();
    try {
      if (encoder?.state !== "closed") encoder?.close();
    } catch {
      // Already closed by its own failure.
    }
  }
}

export type VideoEncoderLatencyMode = "quality" | "realtime";

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
const LATENCY_PROBE_TIMEOUT_MS = 1_500;
interface LatencyMeasurement {
  readonly probe: VideoLatencyProbe;
  /** False when a probe only timed out; such an answer is not reused. */
  readonly definitive: boolean;
}

const latencyModes = new Map<
  keyof typeof LATENCY_PROBE_CODECS,
  Promise<LatencyMeasurement>
>();

/** `"realtime"` only where a quality-mode encoder outputs nothing (Safari 27's H.264). */
export async function videoEncoderLatencyMode(
  codec: "avc" | "hevc" | "vp9" | "av1",
): Promise<VideoEncoderLatencyMode> {
  return (await videoEncoderLatencyProbe(codec)).mode;
}

/** {@link videoEncoderLatencyMode} with how it was measured. */
export async function videoEncoderLatencyProbe(
  codec: "avc" | "hevc" | "vp9" | "av1",
): Promise<VideoLatencyProbe> {
  const known = latencyModes.get(codec);
  if (known !== undefined) {
    const { probe, definitive } = await known;
    if (definitive) return { ...probe, cached: true };
  }
  const measuring = measureLatencyMode(codec);
  latencyModes.set(codec, measuring);
  const { probe, definitive } = await measuring;
  if (!definitive && latencyModes.get(codec) === measuring)
    latencyModes.delete(codec);
  return probe;
}

async function measureLatencyMode(
  codec: keyof typeof LATENCY_PROBE_CODECS,
): Promise<LatencyMeasurement> {
  const started = performance.now();
  const answer = (
    mode: VideoEncoderLatencyMode,
    outcome: VideoLatencyProbe["outcome"],
    definitive: boolean,
  ): LatencyMeasurement => ({
    probe: { mode, ms: performance.now() - started, outcome, cached: false },
    definitive,
  });
  if (typeof VideoEncoder === "undefined")
    return answer("quality", "unavailable", true);
  // One at a time: on Chrome a concurrent realtime encoder delays the quality one.
  const quality = await encoderPutsOut(codec, "quality");
  if (quality === "output") return answer("quality", quality, true);
  if (quality === "failed") return answer("quality", quality, false);
  const realtime = await encoderPutsOut(codec, "realtime");
  // A silent quality encoder never outputs, so use the realtime one even if it only timed out.
  if (quality === "silent" && realtime === "timed-out")
    return answer("realtime", realtime, false);
  if (realtime !== "output") return answer("quality", realtime, false);
  if (quality === "silent") return answer("realtime", realtime, true);
  // Retry the timed-out quality probe now that the encoder is warm.
  const warm = await encoderPutsOut(codec, "quality");
  if (warm === "output") return answer("quality", warm, true);
  if (warm === "timed-out" || warm === "silent")
    return answer("realtime", realtime, true);
  return answer("quality", warm, false);
}

async function encoderPutsOut(
  codec: keyof typeof LATENCY_PROBE_CODECS,
  latencyMode: VideoEncoderLatencyMode,
): Promise<EncoderProbeOutcome> {
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
      return "failed";
  } catch {
    return "failed";
  }
  let outputs = 0;
  let failed = false;
  let flushed = false;
  let encoder: VideoEncoder | undefined;
  const probeDone = new AbortController();
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
      encoder.flush().then(
        () => (flushed = true),
        () => (failed = true),
      ),
      activeTimeout(LATENCY_PROBE_TIMEOUT_MS, probeDone.signal),
    ]);
    if (outputs > 0) return "output";
    if (failed) return "failed";
    return flushed ? "silent" : "timed-out";
  } catch {
    return "failed";
  } finally {
    probeDone.abort();
    try {
      if (encoder?.state !== "closed") encoder?.close();
    } catch {
      // Already closed by its own failure.
    }
  }
}

const ENCODER_SAMPLE_RATES = {
  aac: [
    8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000,
    64_000, 88_200, 96_000,
  ],
  opus: [8_000, 12_000, 16_000, 24_000, 48_000],
} as const;

/**
 * The rate to encode `codec` audio at for a project at `sampleRate`: the project rate if supported,
 * else the nearest higher supported rate, else the nearest lower; `null` for none. Pass it to
 * {@link MediabunnySink} as `audioSampleRate`.
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
  const carried: readonly number[] = ENCODER_SAMPLE_RATES[codec];
  const candidates = new Set([
    ...(codec === "opus" || carried.includes(rate) ? [rate] : []),
    ...carried.filter((candidate) => candidate > rate),
    ...carried.filter((candidate) => candidate < rate).reverse(),
  ]);
  for (const candidate of candidates) {
    const supported = await canEncodeAudio(codec, {
      numberOfChannels: options.numberOfChannels,
      sampleRate: candidate,
      quality: new Quality({ bitrate: options.bitrate }),
      ...(codec === "aac" ? { fullCodecString: AAC_CODEC_STRING } : {}),
    }).catch(() => false);
    if (!supported) continue;
    if (codec === "opus") return candidate;
    const { rate: putOut } = await probeAacEncoder({
      ...options,
      sampleRate: candidate,
    });
    if (putOut === undefined || putOut === candidate) return candidate;
  }
  return null;
}

/** AAC-LC, as native hosts encode; mediabunny would pick HE-AAC at 24 kHz and below. */
const AAC_CODEC_STRING = "mp4a.40.2";
const AAC_FRAME_FRAMES = 1_024;
/** ISO/IEC 14496-3 `samplingFrequencyIndex` values. */
const AAC_SAMPLING_FREQUENCIES = [
  96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000,
  12_000, 11_025, 8_000, 7_350,
];

/** Undefined for a rate without an index or a channel count without a configuration. */
function aacLcAudioSpecificConfig(
  sampleRate: number,
  numberOfChannels: number,
): Uint8Array | undefined {
  const frequency = AAC_SAMPLING_FREQUENCIES.indexOf(sampleRate);
  const channels =
    numberOfChannels === 8 ? 7 : numberOfChannels <= 6 ? numberOfChannels : 0;
  if (frequency < 0 || channels < 1) return undefined;
  return new Uint8Array([
    (2 << 3) | (frequency >> 1),
    ((frequency & 1) << 7) | (channels << 3),
  ]);
}

const AAC_DELAY_PROBE_FRAMES = 2_048;
const AAC_DELAY_MAX_FRAMES = 8_192;
const AAC_DELAY_PROBE_TIMEOUT_MS = 1_500;
const AAC_DELAY_MIN_CORRELATION = 0.5;

const aacProbes = new Map<string, Promise<AacEncoderProbe>>();

/**
 * The priming frames this browser's AAC-LC encoder puts before its input, which WebCodecs does not
 * report. Measured once per configuration by finding an encoded noise burst; 0 on failure.
 */
export async function aacEncoderDelay(options: {
  readonly sampleRate: number;
  readonly numberOfChannels: number;
  readonly bitrate: number;
}): Promise<number> {
  return (await probeAacEncoder(options)).delay;
}

/** {@link aacEncoderDelay} with its outcome. Timeouts are retried once and never cached. */
export function aacEncoderPrimingProbe(options: {
  readonly sampleRate: number;
  readonly numberOfChannels: number;
  readonly bitrate: number;
}): Promise<AacEncoderProbe> {
  return probeAacEncoder(options);
}

function probeAacEncoder(options: {
  readonly sampleRate: number;
  readonly numberOfChannels: number;
  readonly bitrate: number;
}): Promise<AacEncoderProbe> {
  const config: AudioEncoderConfig = {
    codec: AAC_CODEC_STRING,
    sampleRate: options.sampleRate,
    numberOfChannels: options.numberOfChannels,
    bitrate: options.bitrate,
  };
  const key = JSON.stringify(config);
  const known = aacProbes.get(key);
  if (known !== undefined) return known;
  const probe = (async (): Promise<AacEncoderProbe> => {
    const started = performance.now();
    let measured = await measureAacEncoder(config);
    let attempts = 1;
    if (measured.outcome === "timed-out") {
      measured = await measureAacEncoder(config);
      attempts = 2;
    }
    return { ...measured, ms: performance.now() - started, attempts };
  })();
  aacProbes.set(key, probe);
  void probe.then((measured) => {
    if (measured.outcome === "timed-out" && aacProbes.get(key) === probe)
      aacProbes.delete(key);
  });
  return probe;
}

async function measureAacEncoder(
  config: AudioEncoderConfig,
): Promise<Omit<AacEncoderProbe, "ms" | "attempts">> {
  if (
    typeof AudioEncoder === "undefined" ||
    typeof AudioDecoder === "undefined"
  )
    return { delay: 0, outcome: "unavailable" };
  let encoder: AudioEncoder | undefined;
  let decoder: AudioDecoder | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timedOut = new Promise<"timeout">((resolve) => {
    timer = setTimeout(() => resolve("timeout"), AAC_DELAY_PROBE_TIMEOUT_MS);
  });
  try {
    if (!(await AudioEncoder.isConfigSupported(config)).supported)
      return { delay: 0, outcome: "failed" };
    let failed = false;
    const chunks: EncodedAudioChunk[] = [];
    let decoderConfig: AudioDecoderConfig | undefined;
    encoder = new AudioEncoder({
      output: (chunk, metadata) => {
        chunks.push(chunk);
        decoderConfig ??= metadata?.decoderConfig;
      },
      error: () => (failed = true),
    });
    encoder.configure(config);
    // Safari's encoder drops the first input frame, so the burst follows a frame of silence.
    const probe = aacDelayProbe();
    const frames =
      AAC_FRAME_FRAMES + AAC_DELAY_PROBE_FRAMES + AAC_DELAY_MAX_FRAMES;
    const planar = new Float32Array(frames * config.numberOfChannels);
    for (let channel = 0; channel < config.numberOfChannels; channel += 1)
      planar.set(probe, channel * frames + AAC_FRAME_FRAMES);
    const input = new AudioData({
      format: "f32-planar",
      sampleRate: config.sampleRate,
      numberOfFrames: frames,
      numberOfChannels: config.numberOfChannels,
      timestamp: 0,
      data: planar,
    });
    try {
      encoder.encode(input);
    } finally {
      input.close();
    }
    if ((await Promise.race([encoder.flush(), timedOut])) === "timeout")
      return { delay: 0, outcome: "timed-out" };
    if (failed || decoderConfig === undefined)
      return { delay: 0, outcome: "failed" };
    const rate = decoderConfig.sampleRate;
    if (rate !== config.sampleRate)
      return { rate, delay: 0, outcome: "output" };
    const planes: Float32Array[] = [];
    decoder = new AudioDecoder({
      output: (audio) => {
        try {
          if (audio.sampleRate !== config.sampleRate) failed = true;
          const plane = new Float32Array(audio.numberOfFrames);
          audio.copyTo(plane, { planeIndex: 0, format: "f32-planar" });
          planes.push(plane);
        } finally {
          audio.close();
        }
      },
      error: () => (failed = true),
    });
    // Safari's own decoder refuses its encoder's full `ES_Descriptor`.
    const description = aacLcAudioSpecificConfig(
      config.sampleRate,
      config.numberOfChannels,
    );
    decoder.configure(
      description === undefined
        ? decoderConfig
        : { ...decoderConfig, codec: AAC_CODEC_STRING, description },
    );
    for (const chunk of chunks) decoder.decode(chunk);
    if ((await Promise.race([decoder.flush(), timedOut])) === "timeout")
      return { rate, delay: 0, outcome: "timed-out" };
    if (failed) return { rate, delay: 0, outcome: "failed" };
    const decoded = new Float32Array(
      planes.reduce((sum, plane) => sum + plane.length, 0),
    );
    let offset = 0;
    for (const plane of planes) {
      decoded.set(plane, offset);
      offset += plane.length;
    }
    const lag = strongestLag(decoded, probe);
    return {
      rate,
      delay: lag === null ? 0 : Math.max(0, lag - AAC_FRAME_FRAMES),
      outcome: "output",
    };
  } catch {
    return { delay: 0, outcome: "failed" };
  } finally {
    clearTimeout(timer);
    for (const coder of [encoder, decoder]) {
      try {
        if (coder?.state !== "closed") coder?.close();
      } catch {
        // Already closed by its own failure.
      }
    }
  }
}

function aacDelayProbe(): Float32Array {
  const probe = new Float32Array(AAC_DELAY_PROBE_FRAMES);
  let seed = 1;
  let smoothed = 0;
  for (let index = 0; index < probe.length; index += 1) {
    seed = (Math.imul(seed, 1_103_515_245) + 12_345) >>> 0;
    smoothed += 0.5 * ((seed / 2 ** 32) * 2 - 1 - smoothed);
    probe[index] = 0.5 * smoothed;
  }
  return probe;
}

/** The lag where `decoded` best matches `probe`, or null when none matches closely enough. */
function strongestLag(
  decoded: Float32Array,
  probe: Float32Array,
): number | null {
  let probeEnergy = 0;
  for (const value of probe) probeEnergy += value * value;
  const energy = new Float64Array(decoded.length + 1);
  for (let index = 0; index < decoded.length; index += 1)
    energy[index + 1] = energy[index] + decoded[index] * decoded[index];
  const lags = Math.min(
    AAC_FRAME_FRAMES + AAC_DELAY_MAX_FRAMES,
    decoded.length - probe.length,
  );
  let best: number | null = null;
  let bestCorrelation = AAC_DELAY_MIN_CORRELATION;
  for (let lag = 0; lag <= lags; lag += 1) {
    const windowEnergy = energy[lag + probe.length] - energy[lag];
    if (windowEnergy <= 0) continue;
    let sum = 0;
    for (let index = 0; index < probe.length; index += 1)
      sum += decoded[lag + index] * probe[index];
    const correlation = sum / Math.sqrt(probeEnergy * windowEnergy);
    if (correlation > bestCorrelation) {
      bestCorrelation = correlation;
      best = lag;
    }
  }
  return best;
}

/** Converts project-rate blocks to the encoder's rate with the engine's converter. */
class EncoderRateConverter {
  readonly targetRate: number;
  readonly #projectRate: number;
  readonly #resampler: AudioResampler;
  #buffer: Float32Array[];
  #bufferStart: number;
  #received: number;
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

  /** Blocks must be contiguous at one rate and channel count. */
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

  finish(): { startSample: number; channels: Float32Array[] } | null {
    return this.#convert(this.#resampler.targetAtOrAfter(this.#received));
  }

  free(): void {
    this.#resampler.free?.();
  }

  #readyEnd(): number {
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

// No fast start and no append-only mode, so the muxer seeks back only to patch headers.
function outputFormat(container: ExportContainer): OutputFormat {
  switch (container) {
    case "mp4":
      return new Mp4OutputFormat({ fastStart: false });
    case "mov":
      return new MovOutputFormat({ fastStart: false });
    case "mkv":
      return new MkvOutputFormat({ appendOnly: false });
    case "webm":
      return new WebMOutputFormat({ appendOnly: false });
  }
}

/** What a finished {@link MediabunnySink} produced. */
export type ExportOutput =
  | { readonly kind: "stream"; readonly bytes: number }
  | { readonly kind: "memory"; readonly buffer: ArrayBuffer };

export interface AppliedVideoRateControl {
  readonly bitrateMode: "constant" | "variable" | "quantizer";
  readonly bitrate?: number;
}

/**
 * Optional mediabunny encoder/muxer implementing both public alpha2 sinks. When two runners share
 * the file, give each its own {@link MediabunnySink.videoTrack} or
 * {@link MediabunnySink.audioTrack}; the file is finalized once every configured track has ended,
 * and either `abort` discards it. The constructor throws `RangeError` for invalid options.
 */
export class MediabunnySink
  implements BrowserEncodeSink, BrowserAudioBlockSink
{
  readonly #target: BufferTarget | StreamTarget;
  readonly #file?: WritableStreamDefaultWriter<PositionedWrite>;
  #discarded = false;
  readonly #output: Output;
  #written = 0;
  #overflow: ExportFailureError | undefined;
  readonly #hasVideo: boolean;
  #video?: VideoSampleSource;
  #latencyMode?: VideoEncoderLatencyMode;
  #latencyProbe?: VideoLatencyProbe;
  #acceleration?: "no-preference" | "prefer-software";
  readonly #addedAt = new Map<number, number>();
  #encodeLatency = { totalMs: 0, maxMs: 0, count: 0 };
  #aacProbe?: AacEncoderProbe;
  readonly #stallTimeoutMs: number;
  #framesIn = 0;
  #framesOut = 0;
  /** Active time, so a frozen page does not count as a stall. */
  #progress?: ActiveStopwatch;
  readonly #audio?: AudioSampleSource;
  readonly #audioCodec?: "aac" | "opus";
  readonly #audioBitrate?: number;
  #audioDelay?: Promise<number>;
  #audioLeadWritten = false;
  readonly #audioRate?: number;
  readonly #audioResampler?: AudioResamplerBinding;
  #audioConverter?: EncoderRateConverter;
  #audioFlushed = false;
  readonly #ready: Promise<void>;
  #frameDuration?: number;
  #finished = false;
  #applied?: AppliedVideoRateControl;
  #codecString?: string;
  readonly #ended = new Set<"video" | "audio">();
  #videoTrack?: BrowserEncodeSink;
  #audioTrack?: BrowserAudioBlockSink;

  constructor(options: MediabunnySinkOptions) {
    // Validated before anything is opened, since an unsupported pair is a caller error.
    resolveExportFormat({
      container: options.container,
      ...(options.videoCodec === undefined
        ? {}
        : { videoCodec: options.videoCodec }),
      ...(options.audioCodec === undefined
        ? {}
        : { audioCodec: options.audioCodec }),
    });
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
      // Stopping at the reserved size keeps the output within its budget.
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
      // mediabunny closes its stream on finish, failure and cancel alike, and closing commits a
      // file, so the muxer gets its own stream and this sink commits or discards.
      const file = destination.writable.getWriter();
      this.#file = file;
      const muxed = new WritableStream<PositionedWrite>({
        write: async (chunk) => {
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
      format: outputFormat(options.container),
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
      this.#audioCodec = options.audioCodec;
      this.#audioBitrate = options.audioBitrate;
      this.#audioResampler = options.audioResampler;
      this.#audio = new AudioSampleSource({
        codec: options.audioCodec,
        quality: new Quality({ bitrate: options.audioBitrate }),
        ...(options.audioCodec === "aac"
          ? { fullCodecString: AAC_CODEC_STRING }
          : {}),
      });
    }
    if (!this.#hasVideo && this.#audio === undefined)
      throw new RangeError("at least one output track is required");
    this.#ready = this.#open(options);
  }

  async #open(options: MediabunnySinkOptions): Promise<void> {
    const codec = options.videoCodec;
    const rate = options.videoRateControl;
    if (codec !== undefined && rate !== undefined) {
      let latencyMode = options.videoLatencyMode;
      if (latencyMode === undefined) {
        this.#latencyProbe = await videoEncoderLatencyProbe(codec);
        latencyMode = this.#latencyProbe.mode;
      }
      if (this.#output.state === "canceled") return;
      this.#latencyMode = latencyMode;
      const acceleration = options.videoHardwareAcceleration ?? "no-preference";
      this.#acceleration = acceleration;
      const codecString =
        options.videoCodecString ??
        (options.videoBitDepth === 10 ? tenBitCodecString(codec) : undefined);
      this.#video = new VideoSampleSource({
        codec,
        quality: videoEncoderQuality(rate),
        latencyMode,
        hardwareAcceleration: acceleration,
        ...(options.videoAlpha === true ? { alpha: "keep" as const } : {}),
        ...(codecString === undefined ? {} : { fullCodecString: codecString }),
        // mediabunny stops at the first supported candidate, so the last one reported runs.
        onEncoderConfig: (config) => {
          this.#codecString = config.codec;
          const mode = config.bitrateMode as string | undefined;
          this.#applied = {
            bitrateMode:
              mode === "quantizer" || mode === "constant" ? mode : "variable",
            ...(config.bitrate === undefined
              ? {}
              : { bitrate: config.bitrate }),
          };
        },
        onEncodedPacket: (packet) => {
          this.#framesOut += 1;
          this.#madeProgress();
          const key = Math.round(packet.timestamp * 1_000_000);
          const added = this.#addedAt.get(key);
          if (added !== undefined) {
            this.#addedAt.delete(key);
            const ms = performance.now() - added;
            this.#encodeLatency.totalMs += ms;
            this.#encodeLatency.maxMs = Math.max(this.#encodeLatency.maxMs, ms);
            this.#encodeLatency.count += 1;
          }
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

  #madeProgress(): void {
    (this.#progress ??= new ActiveStopwatch()).reset();
  }

  #watched<T>(work: Promise<T>): Promise<T> {
    const progress = (this.#progress ??= new ActiveStopwatch());
    return new Promise<T>((resolve, reject) => {
      const stop = progress.onTick(() => {
        const waited = progress.elapsed();
        if (this.#framesOut >= this.#framesIn || waited < this.#stallTimeoutMs)
          return;
        stop();
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
      });
      void work.finally(stop).then(resolve, reject);
    });
  }

  /** Undefined until the sink has started. */
  get videoLatencyMode(): VideoEncoderLatencyMode | undefined {
    return this.#latencyMode;
  }

  /** Undefined until the sink has started, and when `videoLatencyMode` was given. */
  get videoLatencyProbe(): VideoLatencyProbe | undefined {
    return this.#latencyProbe;
  }

  /** Undefined until the sink has started. */
  get videoEncoderAcceleration():
    "no-preference" | "prefer-software" | undefined {
    return this.#acceleration;
  }

  /** Hand-over to encoded packet, including queueing in the encoder. */
  get videoEncodeLatencyMs(): {
    readonly mean: number;
    readonly max: number;
    readonly count: number;
  } {
    const { totalMs, maxMs, count } = this.#encodeLatency;
    return { mean: count === 0 ? 0 : totalMs / count, max: maxMs, count };
  }

  /** Undefined before the first AAC block. */
  get aacPrimingProbe(): AacEncoderProbe | undefined {
    return this.#aacProbe;
  }

  /**
   * The video track as its own sink, for runners sharing the file. Throws `RangeError` if absent.
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

  /** The audio track as its own sink; see {@link videoTrack}. Throws `RangeError` if absent. */
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

  /** The last configured track to end finishes the file; earlier ones resolve at once. */
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

  /** Undefined until the first frame has been written. */
  get appliedVideoRateControl(): AppliedVideoRateControl | undefined {
    return this.#applied;
  }

  /** Undefined until the first frame has been written. */
  get videoCodecString(): string | undefined {
    return this.#codecString;
  }

  /**
   * Encodes one frame at `output.timeTicks` and always closes `output.frame`. Rejects with
   * `RESOURCE_LIMIT` past a memory destination's `maxBytes`, with `ENCODE_STALLED`, or when no
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
    const sample = new VideoSample(output.frame, {
      timestamp: Number(output.timeTicks) / 120_000,
      duration: this.#frameDuration,
    });
    if (this.#framesOut >= this.#framesIn) this.#madeProgress();
    this.#framesIn += 1;
    this.#addedAt.set(
      Math.round((Number(output.timeTicks) / 120_000) * 1_000_000),
      performance.now(),
    );
    try {
      await this.#watched(this.#video.add(sample));
    } finally {
      sample.close();
      output.frame.close();
    }
  }

  /**
   * Encodes one block at `block.startSample`, from the output's zero. Blocks that need rate
   * conversion must be contiguous. Rejects with `RESOURCE_LIMIT`, when no audio track is
   * configured, for ragged channels, or with `RangeError` for a gap or a missing `audioResampler`.
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

  async #encodeAudio(
    channels: readonly Float32Array[],
    startSample: number,
    sampleRate: number,
  ): Promise<void> {
    if (this.#audio === undefined)
      throw new Error("audio track is not configured");
    // The muxer trims AAC priming placed before zero (edit list, negative Matroska block times);
    // Opus states its own in its header.
    let delay = 0;
    if (this.#audioCodec === "aac" && this.#audioBitrate !== undefined) {
      delay = await (this.#audioDelay ??= aacEncoderPrimingProbe({
        sampleRate,
        numberOfChannels: channels.length,
        bitrate: this.#audioBitrate,
      }).then((probe) => {
        this.#aacProbe = probe;
        return probe.delay;
      }));
      // Safari's encoder drops its first input frame, so lead with a frame of silence.
      if (!this.#audioLeadWritten) {
        this.#audioLeadWritten = true;
        await this.#addAudio(
          channels.map(() => new Float32Array(AAC_FRAME_FRAMES)),
          startSample - AAC_FRAME_FRAMES - delay,
          sampleRate,
        );
      }
    }
    await this.#addAudio(channels, startSample - delay, sampleRate);
  }

  async #addAudio(
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
   * Finalizes the file and commits a stream destination; idempotent once resolved. Rejects with
   * `RESOURCE_LIMIT`, `ENCODE_STALLED`, or the muxer's or destination's error, discarding a stream.
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
    } finally {
      this.#progress?.dispose();
    }
    this.#finished = true;
  }

  /** Discards the output unless already finished; rejects if the destination refuses. */
  async abort(reason: Error): Promise<void> {
    if (this.#finished) return;
    this.#progress?.dispose();
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

  async #discard(reason: unknown): Promise<void> {
    if (this.#file === undefined || this.#discarded) return;
    this.#discarded = true;
    await this.#file.abort(reason);
  }

  /** Null until {@link finish} resolves. */
  get output(): ExportOutput | null {
    if (!this.#finished) return null;
    if (this.#target instanceof BufferTarget) {
      const buffer = this.#target.buffer;
      return buffer === null ? null : { kind: "memory", buffer };
    }
    return { kind: "stream", bytes: this.#written };
  }
  get bytesWritten(): number {
    return this.#written;
  }
}

/** Kept in an optional subpath so core APIs never load mediabunny. */
export class MediabunnyAdapterFactory implements OptionalMediaAdapterFactory {
  readonly #audioResampler?: AudioResamplerBinding;

  constructor(audioResampler?: AudioResamplerBinding) {
    this.#audioResampler = audioResampler;
  }

  /**
   * Rejects with `TypeError` for an unsupported input, and as {@link MediabunnyVideoDecoder.open}.
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
   * Rejects with `TypeError` for an unsupported input or a missing resampler binding, and as
   * {@link MediabunnyAudioDecoder.open}.
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

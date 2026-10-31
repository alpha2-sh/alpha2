import { raceAbort } from "./abort.js";
import { videoFrameRepackedBytes } from "./video-frames.js";
import { ExportFailureError, recordCleanupFailure } from "./export-failure.js";
import type { MissingFrame } from "./player.js";

const TICKS_PER_SECOND = 120_000n;
const DEFAULT_MAX_IN_FLIGHT_VIDEO_FRAMES = 3;
const DEFAULT_MAX_IN_FLIGHT_VIDEO_BYTES = 128 * 1024 * 1024;
// Refills one frame may take before a further `waiting` answer fails the export.
const MAX_REFILLS_PER_FRAME = 4;

/** A frame rate of `num / den` frames per second; both terms must be positive. */
export interface RationalFrameRate {
  readonly num: bigint;
  readonly den: bigint;
}

/** One rendered output frame, in output order. */
export interface ExportedFrame {
  /** Zero-based position in the output. */
  readonly index: bigint;
  /**
   * When the frame starts in the output, in 1/120000 s ticks: counted from the output's zero, the
   * start of {@link Alpha2Exporter.effectiveRange}, so the first frame is at 0. A sink writes it as
   * the sample's timestamp.
   */
  readonly timeTicks: bigint;
  /** Owned by whoever holds this value; the receiver must close it. */
  readonly frame: VideoFrame;
}

/**
 * A backend's answer to one frame request: either the source frames it still needs uploaded, or a
 * submitted render whose readback holds `retainedBytes` until it resolves.
 */
export type ExportTicket =
  | {
      readonly kind: "waiting";
      readonly missingSources: readonly string[];
      /** Each clip's missing frame, with the source time to decode, as a preview reports it. */
      readonly missingFrames: readonly MissingFrame[];
    }
  | {
      readonly kind: "rendered";
      readonly retainedBytes: number;
      readonly readback: Promise<VideoFrame>;
    };

/**
 * One step of {@link Alpha2Exporter.next}: sources to upload before the same frame is asked for
 * again, a submitted frame, or the end of the range.
 */
export type ExportAdvance =
  | {
      readonly kind: "waiting";
      /** The waiting frame's position in the output. */
      readonly index: bigint;
      /** The waiting frame's timeline time, in 1/120000 s ticks. */
      readonly timelineTicks: bigint;
      readonly missingSources: readonly string[];
      /**
       * Each clip's missing frame, with the source time to decode: a trimmed, offset, or retimed
       * clip needs its source at a time other than `timelineTicks`.
       */
      readonly missingFrames: readonly MissingFrame[];
    }
  | {
      readonly kind: "rendered";
      readonly retainedBytes: number;
      readonly output: Promise<ExportedFrame>;
    }
  | { readonly kind: "complete" };

/** An {@link ExportAdvance} that waits for sources: what {@link exportToSink} asks a refill for. */
export type ExportWaiting = Extract<ExportAdvance, { kind: "waiting" }>;

/** Options for {@link exportToSink}. */
export interface BrowserExportOptions {
  /** Cancels the export; it then rejects with an {@link ExportAbortedError} coded `cancelled`. */
  readonly signal?: AbortSignal;
  /** Frames rendering, reading back or encoding at once, a positive integer (default 3). */
  readonly maxInFlightFrames?: number;
  /**
   * Readback bytes those frames may hold at once, a positive integer (default 128 MiB). One frame
   * over it fails the export with `RESOURCE_LIMIT`.
   */
  readonly maxInFlightBytes?: number;
}

/** Browser export measurements whose names match native `performance` fields. */
export interface BrowserExportMetrics {
  readonly pipelineEnabled: true;
  /** Time spent waiting for the host to decode and upload missing sources. */
  readonly sourceConvertUploadMs: number;
  /** Worker/Wasm evaluation, render, submission, and response time. */
  readonly frameSubmitMs: number;
  /** End-to-end GPU mapping latency, including time overlapped with other work. */
  readonly readbackMapLatencyMs: number;
  /** Time for which ordered export progress was blocked on GPU mapping. */
  readonly readbackWaitMs: number;
  /** Alias retained for direct comparison with native metrics. */
  readonly readbackBlockingMs: number;
  /** Time a rendered frame waited behind earlier encoder work. */
  readonly encodeQueueWaitMs: number;
  /** Time spent inside the browser encoder/muxer video-write adapter. */
  readonly videoEncodeMs: number;
  /** Time spent finalizing the encoder and muxer. */
  readonly muxFinishMs: number;
  readonly inFlightDepth: number;
  readonly encodeChannelDepth: number;
  readonly inFlightByteBudget: number;
  readonly stagingBytesPerFrame: number;
  readonly peakReadbackBytes: number;
  /**
   * Mapped staging bytes the export read back: the raw video that crossed from the GPU to script.
   * Native reports the same field.
   */
  readonly outputReadbackBytes: number;
  /**
   * Bytes script copied to pack read-back planes tight, because this browser's `VideoFrame` ignores
   * `layout` (Safari). 0 where the frame reads the padded planes in place.
   */
  readonly outputRepackBytes: number;
  readonly peakEncodeQueueBytes: number;
  readonly maxPendingMediaEvents: number;
  readonly averagePendingMediaEvents: number;
  readonly maxInFlightReadbacks: number;
  readonly maxEncodeQueueEvents: number;
  readonly encodeBackpressureCount: number;
  readonly pipelineOverlapFrames: number;
}

/** Completion data shaped like the comparable part of a native export result. */
export interface BrowserExportResult {
  readonly renderMs: number;
  readonly frames: number;
  readonly fps: number;
  readonly performance: BrowserExportMetrics;
}

/** Backend implemented by the worker that owns the shared wasm renderer. */
export interface ExportBackend {
  /**
   * Renders the frame starting at timeline time `timeTicks`, or reports which sources it still
   * needs. The `VideoFrame` a rendered ticket reads back is stamped at `outputTimeTicks`, the
   * frame's {@link ExportedFrame.timeTicks} (its `timestamp` is that time in whole microseconds,
   * rounded down), so a sink that encodes the frame as it arrives starts its file at 0 too.
   */
  begin(timeTicks: bigint, outputTimeTicks: bigint): Promise<ExportTicket>;
  /** Whether the GPU device is lost; the exporter checks it before every frame. */
  isDeviceLost(): boolean;
  /** Abandons the export after a failure the exporter detected itself. */
  abort(reason: Error): void;
}

/**
 * Encoder/muxer adapter. Ownership of a frame transfers to `writeVideo`. Frame times count from the
 * output's zero ({@link ExportedFrame.timeTicks}).
 */
export interface BrowserEncodeSink {
  /** Encodes one frame. Calls are serialized and arrive in output order. */
  writeVideo(frame: ExportedFrame): Promise<void>;
  /** Flushes the encoder and finalizes the container after the last frame. */
  finish(): Promise<void>;
  /** Discards partial output; called once when the export fails. */
  abort(reason: Error): Promise<void> | void;
}

/**
 * An export stopped by cancellation (`cancelled`), a lost GPU device (`device-lost`) or a sink that
 * threw a non-`Error` value (`adapter`). Each may be restarted cleanly.
 */
export class ExportAbortedError extends Error {
  readonly code: "cancelled" | "device-lost" | "adapter";

  constructor(
    code: "cancelled" | "device-lost" | "adapter",
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options);
    this.name = "ExportAbortedError";
    this.code = code;
  }
}

/**
 * Exact, retryable browser export scheduler. It walks the frame grid of `rate` over a timeline
 * range; encoding and muxing stay host-owned.
 */
export class Alpha2Exporter {
  readonly #backend: ExportBackend;
  readonly #rate: RationalFrameRate;
  readonly #firstFrame: bigint;
  readonly #endFrame: bigint;
  readonly #totalFrames: bigint;
  #nextFrame: bigint;
  #nextOutput = 0n;
  #completed = 0n;
  #beginTail: Promise<void> = Promise.resolve();

  /**
   * Schedules the frames whose intervals meet `[startTicks, endTicks)`.
   *
   * @throws RangeError when a rate term is not positive, `startTicks` is negative or not before
   *   `endTicks`, or the range holds no frame start.
   */
  constructor(
    backend: ExportBackend,
    rate: RationalFrameRate,
    startTicks: bigint,
    endTicks: bigint,
  ) {
    if (
      rate.num <= 0n ||
      rate.den <= 0n ||
      startTicks < 0n ||
      startTicks >= endTicks
    ) {
      throw new RangeError("invalid export schedule");
    }
    this.#backend = backend;
    this.#rate = rate;
    this.#firstFrame = frameAtOrBefore(rate, startTicks);
    this.#nextFrame = this.#firstFrame;
    this.#endFrame = firstFrameAtOrAfter(rate, endTicks);
    if (this.#nextFrame >= this.#endFrame) {
      throw new RangeError("export range contains no frame starts");
    }
    this.#totalFrames = this.#endFrame - this.#nextFrame;
  }

  /**
   * The timeline range this export covers: the requested range widened to the frame grid,
   * `[floor(start), ceil(end))`. Its start is the output's zero, which is where a host starts the
   * export's audio.
   */
  get effectiveRange(): {
    readonly startTicks: bigint;
    readonly endTicks: bigint;
  } {
    return {
      startTicks: frameStart(this.#rate, this.#firstFrame),
      endTicks: frameStart(this.#rate, this.#endFrame),
    };
  }

  /**
   * The project-rate samples this export's audio covers at `sampleRate`: from the sample nearest
   * {@link effectiveRange}'s start to the one nearest its end, as the engine's
   * `export_audio_window` places audio on every host. `startSample` is the mix position written at
   * the output's zero.
   *
   * @throws RangeError when `sampleRate` is not a positive safe integer.
   */
  audioWindow(sampleRate: number): {
    readonly startSample: bigint;
    readonly frames: bigint;
  } {
    validatePositiveInteger(sampleRate, "sampleRate");
    const { startTicks, endTicks } = this.effectiveRange;
    const rate = BigInt(sampleRate);
    const startSample = nearestSample(startTicks, rate);
    return {
      startSample,
      frames: nearestSample(endTicks, rate) - startSample,
    };
  }

  /** Frames whose readback has resolved, out of the frames the range holds. */
  get progress(): { readonly completed: bigint; readonly total: bigint } {
    return {
      completed: this.#completed,
      total: this.#totalFrames,
    };
  }

  /**
   * Submits the next frame. Calls are serialized, so frames are submitted in order even when
   * several calls are pending. A `waiting` answer leaves the frame unsubmitted; call again after
   * uploading the missing sources.
   *
   * Rejects with an {@link ExportAbortedError} coded `device-lost` (after aborting the backend)
   * when the GPU device is lost, and with whatever the backend rejects with.
   */
  next(): Promise<ExportAdvance> {
    const next = this.#beginTail.then(() => this.#beginNext());
    this.#beginTail = next.then(
      () => undefined,
      () => undefined,
    );
    return next;
  }

  async #beginNext(): Promise<ExportAdvance> {
    if (this.#nextFrame >= this.#endFrame) return { kind: "complete" };
    if (this.#backend.isDeviceLost()) {
      const error = new ExportAbortedError(
        "device-lost",
        "GPU device lost during deterministic export",
      );
      this.#backend.abort(error);
      throw error;
    }
    const timelineTicks = frameStart(this.#rate, this.#nextFrame);
    const timeTicks = timelineTicks - frameStart(this.#rate, this.#firstFrame);
    const step = await this.#backend.begin(timelineTicks, timeTicks);
    if (step.kind === "waiting")
      return {
        kind: "waiting",
        index: this.#nextOutput,
        timelineTicks,
        missingSources: step.missingSources,
        missingFrames: step.missingFrames,
      };
    try {
      validateRetainedBytes(step.retainedBytes);
    } catch (error) {
      void step.readback.then(
        (frame) => frame.close(),
        () => {},
      );
      throw error;
    }
    const index = this.#nextOutput;
    this.#nextFrame += 1n;
    this.#nextOutput += 1n;
    const output = step.readback.then((frame) => {
      this.#completed += 1n;
      return { index, timeTicks, frame };
    });
    void output.catch(() => {});
    return { kind: "rendered", retainedBytes: step.retainedBytes, output };
  }
}

/**
 * Runs an exporter into any WebCodecs/mediabunny-compatible public sink, keeping at most
 * `maxInFlightFrames` frames and `maxInFlightBytes` readback bytes in flight. `refill` uploads the
 * source frames a frame is waiting for, which its `missingFrames` name with the source time to
 * decode, before the same frame is asked for again; a frame still waiting after four refills fails
 * the export with an `ExportFailureError` coded `RESOURCE_LIMIT` at step `upload`, stage
 * `sources-not-held`, naming the sources: the refill does not upload them. A frame whose sources
 * outgrow the binding's uploaded-frame budget fails sooner: the binding refuses the upload that
 * would overflow it (`RESOURCE_LIMIT`, stage `working-set-over-budget`), and `refill` rejects with
 * that. Neither is retried by `decideRetry`.
 *
 * `refill` must deliver through a decoder refill of the export's own, not the preview's: a
 * `DecoderRefillCoordinator` made for the export, whose `deliver` uploads to the backend the
 * export renders with and whose requests carry generation 0 (an {@link ExportWaiting} has none).
 * A coordinator resolves a request from a generation older than its own without delivering it,
 * so the preview's, past generation 0 after its first seek, would drop the export's refills
 * silently and the frame would fail at `sources-not-held`.
 *
 * Resolves after `sink.finish()` with timing and memory metrics; a `signal` that aborts before
 * then, during `finish()` included, rejects instead, at once even while a readback, a refill or a
 * write never settles (what such work brings later is closed). On any other failure it closes the
 * frames it still holds and waits for pending writes; either way it calls `sink.abort` once and
 * rejects with the failure: an {@link ExportAbortedError} for cancellation or a lost device, an
 * `ExportFailureError` coded `RESOURCE_LIMIT` when one frame exceeds `maxInFlightBytes`, a
 * `RangeError` for an invalid option, or the error the backend, `refill` or the sink raised. An
 * `abort` that throws does not replace that failure ({@link recordCleanupFailure}).
 */
export async function exportToSink(
  exporter: Alpha2Exporter,
  sink: BrowserEncodeSink,
  refill: (waiting: ExportWaiting) => Promise<void>,
  options: BrowserExportOptions = {},
): Promise<BrowserExportResult> {
  const started = performance.now();
  const maxInFlightFrames =
    options.maxInFlightFrames ?? DEFAULT_MAX_IN_FLIGHT_VIDEO_FRAMES;
  const maxInFlightBytes =
    options.maxInFlightBytes ?? DEFAULT_MAX_IN_FLIGHT_VIDEO_BYTES;
  validatePositiveInteger(maxInFlightFrames, "maxInFlightFrames");
  validatePositiveInteger(maxInFlightBytes, "maxInFlightBytes");
  let writeTail = Promise.resolve();
  const pendingReadbacks: Array<{
    readonly output: Promise<ExportedFrame>;
    readonly retainedBytes: number;
  }> = [];
  const pendingWrites: Array<{
    readonly completion: Promise<void>;
    readonly retainedBytes: number;
    settled:
      | { readonly failed: false }
      | { readonly failed: true; readonly error: unknown }
      | null;
  }> = [];
  let retainedBytes = 0;
  let readbackBytes = 0;
  let encodeQueueBytes = 0;
  let frameRetainedBytes: number | undefined;
  let exportComplete = false;
  let refills = 0;
  let sourceConvertUploadMs = 0;
  let frameSubmitMs = 0;
  let readbackMapLatencyMs = 0;
  let readbackWaitMs = 0;
  let encodeQueueWaitMs = 0;
  let videoEncodeMs = 0;
  let muxFinishMs: number;
  let peakReadbackBytes = 0;
  let outputReadbackBytes = 0;
  let outputRepackBytes = 0;
  let peakEncodeQueueBytes = 0;
  let maxPendingMediaEvents = 0;
  let pendingDepthTotal = 0;
  let pendingDepthSamples = 0;
  let maxInFlightReadbacks = 0;
  let maxEncodeQueueEvents = 0;
  let encodeBackpressureCount = 0;
  let pipelineOverlapFrames = 0;
  // Writes settle in order. A settled write frees its slot and bytes for the next frame at once;
  // a failed one fails the export.
  const retireSettledWrites = (): void => {
    for (;;) {
      const settled = pendingWrites[0]?.settled;
      if (settled === undefined || settled === null) return;
      if (settled.failed) throw settled.error;
      const write = pendingWrites.shift();
      if (write === undefined) return;
      retainedBytes -= write.retainedBytes;
      encodeQueueBytes -= write.retainedBytes;
    }
  };
  // Checked before each submission and each frame handed to the sink, and once more on each side of
  // finish(), so a cancel after the last submission still cancels: nothing then reports success.
  const cancelledError = (signal: AbortSignal): ExportAbortedError =>
    new ExportAbortedError("cancelled", "browser export cancelled", {
      cause: signal.reason,
    });
  const throwIfCancelled = (): void => {
    if (options.signal?.aborted === true) throw cancelledError(options.signal);
  };
  // A cancel ends any wait at once, even one on work that never settles (a readback whose GPU
  // stopped, a write the encoder holds); what such work brings later is released.
  const cancellable = <T>(
    work: Promise<T>,
    onLate?: (value: T) => void,
  ): Promise<T> => raceAbort(work, options.signal, onLate, cancelledError);
  try {
    while (!exportComplete || pendingReadbacks.length > 0) {
      throwIfCancelled();
      // A frame is submitted only when the budget holds it. The first frame's cost is learned from
      // the first submission, which is refused below if even it does not fit.
      for (;;) {
        retireSettledWrites();
        if (
          exportComplete ||
          pendingReadbacks.length + pendingWrites.length >= maxInFlightFrames ||
          (frameRetainedBytes !== undefined &&
            retainedBytes + frameRetainedBytes > maxInFlightBytes)
        )
          break;
        throwIfCancelled();
        const submitStarted = performance.now();
        const step = await cancellable(exporter.next(), (late) => {
          if (late.kind === "rendered")
            void late.output.then(
              ({ frame }) => frame.close(),
              () => {},
            );
        });
        frameSubmitMs += performance.now() - submitStarted;
        if (step.kind === "complete") {
          exportComplete = true;
          break;
        }
        if (step.kind === "waiting") {
          if (refills === MAX_REFILLS_PER_FRAME)
            throw sourcesNotHeld(step, refills);
          refills += 1;
          const refillStarted = performance.now();
          await cancellable(refill(step));
          sourceConvertUploadMs += performance.now() - refillStarted;
          continue;
        }
        refills = 0;
        if (
          frameRetainedBytes !== undefined &&
          frameRetainedBytes !== step.retainedBytes
        ) {
          void step.output.then(
            ({ frame }) => frame.close(),
            () => {},
          );
          throw new Error("export frame retained byte size changed");
        }
        if (step.retainedBytes > maxInFlightBytes) {
          void step.output.then(
            ({ frame }) => frame.close(),
            () => {},
          );
          throw new ExportFailureError({
            code: "RESOURCE_LIMIT",
            message: `One frame holds ${step.retainedBytes} bytes in flight, over the ${maxInFlightBytes}-byte maxInFlightBytes budget`,
            origin: "browser",
            operation: "export",
            backend: "unknown",
            phase: "render",
            frameIndex: 0,
            resource: "unknown",
            requestedBytes: step.retainedBytes,
          });
        }
        frameRetainedBytes = step.retainedBytes;
        retainedBytes += step.retainedBytes;
        readbackBytes += step.retainedBytes;
        peakReadbackBytes = Math.max(peakReadbackBytes, readbackBytes);
        const mappingStarted = performance.now();
        const measuredOutput = step.output.then(
          (output) => {
            readbackMapLatencyMs += performance.now() - mappingStarted;
            return output;
          },
          (error: unknown) => {
            readbackMapLatencyMs += performance.now() - mappingStarted;
            throw error;
          },
        );
        void measuredOutput.catch(() => {});
        pendingReadbacks.push({
          output: measuredOutput,
          retainedBytes: step.retainedBytes,
        });
        maxInFlightReadbacks = Math.max(
          maxInFlightReadbacks,
          pendingReadbacks.length,
        );
        if (pendingWrites.length > 0) pipelineOverlapFrames += 1;
      }

      const pendingDepth = pendingReadbacks.length + pendingWrites.length;
      maxPendingMediaEvents = Math.max(maxPendingMediaEvents, pendingDepth);
      pendingDepthTotal += pendingDepth;
      pendingDepthSamples += 1;

      if (pendingReadbacks.length === 0) {
        const oldestWrite = pendingWrites.shift();
        if (oldestWrite !== undefined) {
          await cancellable(oldestWrite.completion);
          retainedBytes -= oldestWrite.retainedBytes;
          encodeQueueBytes -= oldestWrite.retainedBytes;
        }
        continue;
      }
      const oldestReadback = pendingReadbacks.shift();
      if (oldestReadback === undefined) continue;
      const readbackWaitStarted = performance.now();
      const output = await cancellable(oldestReadback.output, ({ frame }) =>
        frame.close(),
      );
      readbackWaitMs += performance.now() - readbackWaitStarted;
      readbackBytes -= oldestReadback.retainedBytes;
      outputReadbackBytes += oldestReadback.retainedBytes;
      outputRepackBytes += videoFrameRepackedBytes(output.frame);
      const encodeQueued = performance.now();
      if (pendingWrites.length > 0) encodeBackpressureCount += 1;
      const write = writeTail.then(
        async () => {
          encodeQueueWaitMs += performance.now() - encodeQueued;
          const encodeStarted = performance.now();
          try {
            await sink.writeVideo(output);
          } finally {
            videoEncodeMs += performance.now() - encodeStarted;
          }
        },
        (error: unknown) => {
          output.frame.close();
          throw error;
        },
      );
      // Keep one serial consumer for sinks that require ordered calls, while the render worker
      // produces the next bounded set of frames.
      writeTail = write;
      const pendingWrite: (typeof pendingWrites)[number] = {
        completion: write,
        retainedBytes: oldestReadback.retainedBytes,
        settled: null,
      };
      write.then(
        () => {
          pendingWrite.settled = { failed: false };
        },
        (error: unknown) => {
          pendingWrite.settled = { failed: true, error };
        },
      );
      pendingWrites.push(pendingWrite);
      encodeQueueBytes += oldestReadback.retainedBytes;
      peakEncodeQueueBytes = Math.max(peakEncodeQueueBytes, encodeQueueBytes);
      maxEncodeQueueEvents = Math.max(
        maxEncodeQueueEvents,
        pendingWrites.length,
      );
      void write.catch(() => {});
    }
    await cancellable(writeTail);
    pendingWrites.length = 0;
    throwIfCancelled();
    const finishStarted = performance.now();
    await sink.finish();
    muxFinishMs = performance.now() - finishStarted;
    throwIfCancelled();
    const renderMs = performance.now() - started;
    const completed = exporter.progress.completed;
    const frames = Number(completed);
    if (!Number.isSafeInteger(frames)) {
      throw new RangeError(
        "completed frame count exceeds JavaScript's safe integer range",
      );
    }
    return {
      renderMs,
      frames,
      fps: frames / Math.max(renderMs / 1_000, Number.EPSILON),
      performance: {
        pipelineEnabled: true,
        sourceConvertUploadMs,
        frameSubmitMs,
        readbackMapLatencyMs,
        readbackWaitMs,
        readbackBlockingMs: readbackWaitMs,
        encodeQueueWaitMs,
        videoEncodeMs,
        muxFinishMs,
        inFlightDepth: maxInFlightFrames,
        encodeChannelDepth: maxInFlightFrames,
        inFlightByteBudget: maxInFlightBytes,
        stagingBytesPerFrame: frameRetainedBytes ?? 0,
        peakReadbackBytes,
        outputReadbackBytes,
        outputRepackBytes,
        peakEncodeQueueBytes,
        maxPendingMediaEvents,
        averagePendingMediaEvents:
          pendingDepthSamples === 0
            ? 0
            : pendingDepthTotal / pendingDepthSamples,
        maxInFlightReadbacks,
        maxEncodeQueueEvents,
        encodeBackpressureCount,
        pipelineOverlapFrames,
      },
    };
  } catch (error) {
    if (error instanceof ExportAbortedError && error.code === "cancelled") {
      // A cancel waits for nothing that may never settle: frames that arrive later are closed.
      for (const { output } of pendingReadbacks)
        void output.then(
          ({ frame }) => frame.close(),
          () => {},
        );
    } else {
      const abandoned = await Promise.allSettled(
        pendingReadbacks.map(({ output }) => output),
      );
      for (const result of abandoned) {
        if (result.status === "fulfilled") result.value.frame.close();
      }
      await Promise.allSettled(
        pendingWrites.map(({ completion }) => completion),
      );
    }
    const failure =
      error instanceof Error
        ? error
        : new ExportAbortedError("adapter", String(error));
    try {
      await sink.abort(failure);
    } catch (cleanup) {
      recordCleanupFailure(failure, cleanup);
    }
    throw failure;
  }
}

/**
 * The failure for a frame that refills did not fill: a refusal (`RESOURCE_LIMIT`), as the frame's
 * refill allowance ran out, and the cause is the host's refill, which another attempt runs alike,
 * so `decideRetry` does not retry it.
 */
function sourcesNotHeld(
  step: ExportWaiting,
  refills: number,
): ExportFailureError {
  return new ExportFailureError({
    code: "RESOURCE_LIMIT",
    message: `Export frame ${step.index} at timeline tick ${step.timelineTicks} still waits for ${step.missingSources.join(", ")} after ${refills} refills: the refill does not upload them`,
    origin: "browser",
    operation: "export",
    backend: "unknown",
    phase: "upload",
    stage: "sources-not-held",
    frameIndex: Number(step.index),
    timeTicks: String(step.timelineTicks),
    resource: "unknown",
    diagnostics: { missingSources: [...step.missingSources], refills },
  });
}

function validateRetainedBytes(value: number): void {
  validatePositiveInteger(value, "retainedBytes");
}

function validatePositiveInteger(value: number, name: string): void {
  if (!Number.isSafeInteger(value) || value <= 0) {
    throw new RangeError(`${name} must be a positive safe integer`);
  }
}

function frameStart(rate: RationalFrameRate, frame: bigint): bigint {
  return (frame * TICKS_PER_SECOND * rate.den) / rate.num;
}

function firstFrameAtOrAfter(rate: RationalFrameRate, ticks: bigint): bigint {
  const numerator = ticks * rate.num;
  const denominator = TICKS_PER_SECOND * rate.den;
  return (numerator + denominator - 1n) / denominator;
}

/**
 * The frame whose half-open interval `[frameStart(k), frameStart(k + 1))` holds `ticks`
 * (non-negative): the inverse of the floored frame start, as the engine's
 * `FrameRate::frame_at_or_before`. Where a frame lasts a fractional number of ticks, its floored
 * start belongs to it, not to the frame before.
 */
function frameAtOrBefore(rate: RationalFrameRate, ticks: bigint): bigint {
  return ((ticks + 1n) * rate.num - 1n) / (TICKS_PER_SECOND * rate.den);
}

/** The sample nearest `ticks` (non-negative) at `sampleRate`, half a sample rounding up. */
function nearestSample(ticks: bigint, sampleRate: bigint): bigint {
  return (ticks * sampleRate * 2n + TICKS_PER_SECOND) / (TICKS_PER_SECOND * 2n);
}

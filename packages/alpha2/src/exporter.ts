import { raceAbort } from "./abort.js";
import { videoFrameRepackedBytes } from "./video-frames.js";
import { ExportFailureError, recordCleanupFailure } from "./export-failure.js";
import type { MissingFrame } from "./player.js";
import { softwareDecodeRequest } from "./frame-intake.js";
import {
  type AacEncoderProbe,
  type BrowserExportPerformance,
  type EncodeLatency,
  ExportMetrics,
  type VideoLatencyProbe,
} from "./export-metrics.js";

const TICKS_PER_SECOND = 120_000n;
const DEFAULT_MAX_IN_FLIGHT_VIDEO_FRAMES = 3;
const DEFAULT_MAX_IN_FLIGHT_VIDEO_BYTES = 128 * 1024 * 1024;
// Refills allowed per frame before another `waiting` answer fails the export.
const MAX_REFILLS_PER_FRAME = 4;

/** `num / den` frames per second; both terms must be positive. */
export interface RationalFrameRate {
  readonly num: bigint;
  readonly den: bigint;
}

/** One rendered output frame, in output order. */
export interface ExportedFrame {
  readonly index: bigint;
  /**
   * Output start time in 1/120000 s ticks, relative to {@link Alpha2Exporter.effectiveRange}'s
   * start, so the first frame is at 0.
   */
  readonly timeTicks: bigint;
  /** The receiver must close it. */
  readonly frame: VideoFrame;
}

/**
 * A backend's answer to one frame request: the source frames still needed, or a submitted render
 * whose readback holds `retainedBytes` until it resolves.
 */
export type ExportTicket =
  | {
      readonly kind: "waiting";
      readonly missingSources: readonly string[];
      /** Each clip's missing frame, with the source time to decode. */
      readonly missingFrames: readonly MissingFrame[];
    }
  | {
      readonly kind: "rendered";
      readonly retainedBytes: number;
      readonly readback: Promise<VideoFrame>;
    };

/** Result of {@link Alpha2Exporter.next}. */
export type ExportAdvance =
  | {
      readonly kind: "waiting";
      readonly index: bigint;
      /** Timeline time in 1/120000 s ticks. */
      readonly timelineTicks: bigint;
      readonly missingSources: readonly string[];
      /** Each clip's missing frame, with the source time to decode. */
      readonly missingFrames: readonly MissingFrame[];
    }
  | {
      readonly kind: "rendered";
      readonly retainedBytes: number;
      readonly output: Promise<ExportedFrame>;
    }
  | { readonly kind: "complete" };

/** The waiting {@link ExportAdvance} that {@link exportToSink} passes to `refill`. */
export type ExportWaiting = Extract<ExportAdvance, { kind: "waiting" }>;

/** Options for {@link exportToSink}. */
export interface BrowserExportOptions {
  /** Aborting rejects with an {@link ExportAbortedError} coded `cancelled`. */
  readonly signal?: AbortSignal;
  /** Frames rendering, reading back or encoding at once (default 3). */
  readonly maxInFlightFrames?: number;
  /**
   * Readback bytes in flight at once (default 128 MiB). A single frame over it fails the export
   * with `RESOURCE_LIMIT`.
   */
  readonly maxInFlightBytes?: number;
}

/** Completion data, shaped like the comparable part of a native export result. */
export interface BrowserExportResult {
  readonly renderMs: number;
  readonly frames: number;
  readonly fps: number;
  readonly performance: BrowserExportPerformance;
  /** Present when the sink reports it. */
  readonly videoLatencyProbe?: VideoLatencyProbe;
  /** Present when the sink reports it. */
  readonly aacPrimingProbe?: AacEncoderProbe;
  /** Present when the sink reports it. */
  readonly videoEncoderAcceleration?: "no-preference" | "prefer-software";
}

/** Backend implemented by the worker that owns the shared wasm renderer. */
export interface ExportBackend {
  /**
   * Renders the frame at timeline time `timeTicks`, or reports which sources it still needs. The
   * read-back `VideoFrame`'s `timestamp` is `outputTimeTicks` in whole microseconds, rounded down.
   */
  begin(timeTicks: bigint, outputTimeTicks: bigint): Promise<ExportTicket>;
  /** Checked before every frame. */
  isDeviceLost(): boolean;
  /** Abandons the export after a failure the exporter detected. */
  abort(reason: Error): void;
}

/** Encoder and muxer adapter. Ownership of a frame transfers to `writeVideo`. */
export interface BrowserEncodeSink {
  /** Calls are serialized and arrive in output order. */
  writeVideo(frame: ExportedFrame): Promise<void>;
  /** Flushes the encoder and finalizes the container after the last frame. */
  finish(): Promise<void>;
  /** Discards partial output; called once when the export fails. */
  abort(reason: Error): Promise<void> | void;
  /** Copied into the result when present. */
  readonly videoLatencyProbe?: VideoLatencyProbe;
  readonly aacPrimingProbe?: AacEncoderProbe;
  readonly videoEncoderAcceleration?: "no-preference" | "prefer-software";
  readonly videoEncodeLatencyMs?: EncodeLatency;
}

/**
 * An export stopped by cancellation (`cancelled`), a lost GPU device (`device-lost`) or a sink that
 * threw a non-`Error` value (`adapter`). The export can be restarted cleanly.
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
 * Browser export scheduler that walks the frame grid of `rate` over a timeline range. Encoding and
 * muxing stay with the host.
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
   * The requested range widened to the frame grid, `[floor(start), ceil(end))`. Its start is
   * output time zero, where the host should also start the audio.
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
   * The audio samples this export covers at `sampleRate`, from the sample nearest
   * {@link effectiveRange}'s start to the one nearest its end, matching the engine's
   * `export_audio_window`. `startSample` is the mix position at output time zero.
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

  /** Frames whose readback has resolved, out of the total. */
  get progress(): { readonly completed: bigint; readonly total: bigint } {
    return {
      completed: this.#completed,
      total: this.#totalFrames,
    };
  }

  /**
   * Submits the next frame. Calls are serialized. A `waiting` result leaves the frame unsubmitted;
   * call again after uploading the missing sources.
   *
   * Rejects with an {@link ExportAbortedError} coded `device-lost` (after aborting the backend)
   * when the GPU device is lost, or with the backend's error.
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
 * Runs an exporter into a WebCodecs or mediabunny-compatible sink, keeping at most
 * `maxInFlightFrames` frames and `maxInFlightBytes` readback bytes in flight.
 *
 * `refill` uploads the source frames named in `missingFrames` before the frame is requested again.
 * A frame still waiting after four refills fails with `RESOURCE_LIMIT` at stage
 * `sources-not-held`; an upload over the binding's frame budget fails with `RESOURCE_LIMIT` at
 * stage `working-set-over-budget`. `decideRetry` retries neither.
 *
 * A refill rejected with `SOFTWARE_DECODE_REQUIRED` is retried once with `decode: "software"` on
 * that `missingFrames` entry, and a `non-planar` source is requested in software from then on.
 * `refill` must honor `decode`.
 *
 * `refill` must use a `DecoderRefillCoordinator` created for this export (requests at generation
 * 0), not the preview's. The preview's coordinator moves past generation 0 after its first seek
 * and would silently drop the export's requests.
 *
 * Resolves after `sink.finish()` with timing and memory metrics. Aborting `signal`, even during
 * `finish()`, rejects immediately; late results are closed. On other failures, held frames are
 * closed and pending writes awaited. In both cases `sink.abort` is called once and the promise
 * rejects with an {@link ExportAbortedError}, a `RESOURCE_LIMIT` `ExportFailureError`, a
 * `RangeError` for an invalid option, or the error from the backend, `refill` or sink. An error
 * thrown by `sink.abort` does not replace it.
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
  const metrics = new ExportMetrics({
    maxInFlightReadbacks: maxInFlightFrames,
    maxPendingVideoWrites: maxInFlightFrames,
    inFlightByteBudget: maxInFlightBytes,
  });
  // Sources needing software decode, and individual frames already retried in software.
  const softwareSources = new Set<string>();
  const softwareFrames = new Set<string>();
  const inSoftware = (missing: MissingFrame): MissingFrame =>
    softwareSources.has(missing.sourceId) ||
    softwareFrames.has(`${missing.sourceId}\u0000${missing.mediaTimeTicks}`)
      ? { ...missing, decode: "software" }
      : missing;
  // Writes settle in order; a failed write fails the export.
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
  // Also checked around finish(), so a cancel after the last submission still wins.
  const cancelledError = (signal: AbortSignal): ExportAbortedError =>
    new ExportAbortedError("cancelled", "browser export cancelled", {
      cause: signal.reason,
    });
  const throwIfCancelled = (): void => {
    if (options.signal?.aborted === true) throw cancelledError(options.signal);
  };
  // A cancel ends any wait at once, even on work that never settles; late results are released.
  const cancellable = <T>(
    work: Promise<T>,
    onLate?: (value: T) => void,
  ): Promise<T> => raceAbort(work, options.signal, onLate, cancelledError);
  try {
    while (!exportComplete || pendingReadbacks.length > 0) {
      throwIfCancelled();
      // The per-frame cost is learned from the first submission, which is refused below if it
      // alone exceeds the budget.
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
        metrics.frameSubmitMs += performance.now() - submitStarted;
        if (step.kind === "complete") {
          exportComplete = true;
          break;
        }
        if (step.kind === "waiting") {
          if (refills === MAX_REFILLS_PER_FRAME)
            throw sourcesNotHeld(step, refills);
          refills += 1;
          const refillStarted = performance.now();
          try {
            await cancellable(
              refill({
                ...step,
                missingFrames: step.missingFrames.map(inSoftware),
              }),
            );
          } catch (error) {
            const request = softwareDecodeRequest(error);
            const key =
              request && `${request.sourceId}\u0000${request.mediaTimeTicks}`;
            if (key === undefined || softwareFrames.has(key)) throw error;
            softwareFrames.add(key);
            if (request?.reason === "non-planar")
              softwareSources.add(request.sourceId);
            // A software retry does not count as another refill.
            refills -= 1;
          } finally {
            metrics.sourceSupplyMs += performance.now() - refillStarted;
          }
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
        metrics.peakReadbackBytes = Math.max(
          metrics.peakReadbackBytes,
          readbackBytes,
        );
        const mappingStarted = performance.now();
        const measuredOutput = step.output.then(
          (output) => {
            metrics.readbackMapLatencyMs += performance.now() - mappingStarted;
            return output;
          },
          (error: unknown) => {
            metrics.readbackMapLatencyMs += performance.now() - mappingStarted;
            throw error;
          },
        );
        void measuredOutput.catch(() => {});
        pendingReadbacks.push({
          output: measuredOutput,
          retainedBytes: step.retainedBytes,
        });
        metrics.peakInFlightReadbacks = Math.max(
          metrics.peakInFlightReadbacks,
          pendingReadbacks.length,
        );
        if (pendingWrites.length > 0) metrics.pipelineOverlapFrames += 1;
      }

      metrics.samplePending(pendingReadbacks.length + pendingWrites.length);

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
      metrics.readbackWaitMs += performance.now() - readbackWaitStarted;
      readbackBytes -= oldestReadback.retainedBytes;
      metrics.outputReadbackBytes += oldestReadback.retainedBytes;
      metrics.outputRepackBytes += videoFrameRepackedBytes(output.frame);
      const encodeQueued = performance.now();
      if (pendingWrites.length > 0) metrics.encodeBackpressureCount += 1;
      const write = writeTail.then(
        async () => {
          metrics.encodeQueueWaitMs += performance.now() - encodeQueued;
          const writeStarted = performance.now();
          try {
            await sink.writeVideo(output);
          } finally {
            metrics.videoWriteWaitMs += performance.now() - writeStarted;
          }
        },
        (error: unknown) => {
          output.frame.close();
          throw error;
        },
      );
      // Writes stay serialized while the render worker produces the next frames.
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
      metrics.peakEncodeQueueBytes = Math.max(
        metrics.peakEncodeQueueBytes,
        encodeQueueBytes,
      );
      metrics.maxEncodeQueueEvents = Math.max(
        metrics.maxEncodeQueueEvents,
        pendingWrites.length,
      );
      void write.catch(() => {});
    }
    await cancellable(writeTail);
    pendingWrites.length = 0;
    throwIfCancelled();
    const finishStarted = performance.now();
    await sink.finish();
    metrics.finishMs = performance.now() - finishStarted;
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
      performance: (() => {
        metrics.stagingBytesPerFrame = frameRetainedBytes ?? 0;
        return metrics.finish(sink.videoEncodeLatencyMs);
      })(),
      ...(sink.videoLatencyProbe === undefined
        ? {}
        : { videoLatencyProbe: sink.videoLatencyProbe }),
      ...(sink.aacPrimingProbe === undefined
        ? {}
        : { aacPrimingProbe: sink.aacPrimingProbe }),
      ...(sink.videoEncoderAcceleration === undefined
        ? {}
        : { videoEncoderAcceleration: sink.videoEncoderAcceleration }),
    };
  } catch (error) {
    if (error instanceof ExportAbortedError && error.code === "cancelled") {
      // Do not wait on work that may never settle; close late frames instead.
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
 * `RESOURCE_LIMIT` for a frame the refills never satisfied. A retry would run the same refill, so
 * `decideRetry` does not retry it.
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
 * The frame `k` whose `[frameStart(k), frameStart(k + 1))` holds non-negative `ticks`, matching
 * the engine's `FrameRate::frame_at_or_before`.
 */
function frameAtOrBefore(rate: RationalFrameRate, ticks: bigint): bigint {
  return ((ticks + 1n) * rate.num - 1n) / (TICKS_PER_SECOND * rate.den);
}

/** The sample nearest non-negative `ticks`, rounding half up. */
function nearestSample(ticks: bigint, sampleRate: bigint): bigint {
  return (ticks * sampleRate * 2n + TICKS_PER_SECOND) / (TICKS_PER_SECOND * 2n);
}

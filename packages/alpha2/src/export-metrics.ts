// Shared metrics for browser export loops. A field with the same name as a native host field
// measures the same quantity.

/** See {@link VideoLatencyProbe}. */
export type EncoderProbeOutcome = "output" | "silent" | "failed" | "timed-out";

/**
 * How `videoEncoderLatencyMode` (`alpha2/mediabunny`) chose `mode`. `ms` is the total probe time
 * and `outcome` the last probe's result: `"output"` (a chunk came out), `"silent"` (flushed with no
 * output), `"failed"`, `"timed-out"` (inconclusive) or `"unavailable"` (no `VideoEncoder`).
 * `cached` means a definitive answer from an earlier export was reused.
 */
export interface VideoLatencyProbe {
  readonly mode: "quality" | "realtime";
  readonly ms: number;
  readonly outcome: EncoderProbeOutcome | "unavailable";
  readonly cached: boolean;
}

/**
 * AAC-LC delay probe result: `"output"` (encoded and decoded), `"failed"` (encoder or decoder
 * error), `"timed-out"`, or `"unavailable"` (no WebCodecs audio).
 */
export type AacProbeOutcome = "output" | "failed" | "timed-out" | "unavailable";

/** Result of `aacEncoderPrimingProbe` (`alpha2/mediabunny`). */
export interface AacEncoderProbe {
  /** Sample rate the stream declares; undefined when there was no output. */
  readonly rate?: number;
  /** Priming in frames; 0 when not found or the stream is at another rate. */
  readonly delay: number;
  readonly outcome: AacProbeOutcome;
  /** Total probe time. */
  readonly ms: number;
  /** Probes run; a timeout triggers a second. */
  readonly attempts: number;
}

/** Encoder latency over the frames one export encoded. */
export interface EncodeLatency {
  readonly mean: number;
  readonly max: number;
  readonly count: number;
}

/** Timing and memory of one browser export, in milliseconds and bytes. */
export interface BrowserExportPerformance {
  readonly pipelineEnabled: true;
  /** Time waiting for source frames to be decoded, converted and uploaded. */
  readonly sourceSupplyMs: number;
  /** Worker/wasm evaluation, render, submission and response time. */
  readonly frameSubmitMs: number;
  /** End-to-end GPU mapping latency, including time overlapped with other work. */
  readonly readbackMapLatencyMs: number;
  /** Time for which ordered export progress was blocked on GPU mapping. */
  readonly readbackWaitMs: number;
  /** Same as `readbackWaitMs`, under the native host's name. */
  readonly readbackBlockingMs: number;
  /** Time a rendered frame waited behind earlier writes before its own write began. */
  readonly encodeQueueWaitMs: number;
  /**
   * Time in the sink's video write, including waiting for encoder queue space. Encoding itself is
   * measured by `videoEncodeLatencyMs`.
   */
  readonly videoWriteWaitMs: number;
  /** Time to flush the encoders and finalize the file after the last frame. */
  readonly finishMs: number;
  /** Configured limit on concurrent readbacks. */
  readonly maxInFlightReadbacks: number;
  /** Configured limit on pending video writes. */
  readonly maxPendingVideoWrites: number;
  /** Readback bytes the loop may hold at once. */
  readonly inFlightByteBudget: number;
  readonly stagingBytesPerFrame: number;
  readonly peakReadbackBytes: number;
  /** Raw video bytes read back from GPU staging buffers. */
  readonly outputReadbackBytes: number;
  /** Bytes copied to pack read-back planes tight where `VideoFrame` ignores `layout`; else 0. */
  readonly outputRepackBytes: number;
  readonly peakEncodeQueueBytes: number;
  /** Pending readbacks plus writes, peak and average over the loop's turns. */
  readonly maxPendingMediaEvents: number;
  readonly averagePendingMediaEvents: number;
  readonly peakInFlightReadbacks: number;
  /** Peak pending video writes. */
  readonly maxEncodeQueueEvents: number;
  /** Frames that waited for an earlier write. */
  readonly encodeBackpressureCount: number;
  /** Frames submitted while writes were pending. */
  readonly pipelineOverlapFrames: number;
  /** Per-frame encoder time, hand-over to packet; zero when the sink does not report it. */
  readonly videoEncodeLatencyMs: EncodeLatency;
}

/** Accumulator for {@link BrowserExportPerformance}. */
export class ExportMetrics {
  sourceSupplyMs = 0;
  frameSubmitMs = 0;
  readbackMapLatencyMs = 0;
  readbackWaitMs = 0;
  encodeQueueWaitMs = 0;
  videoWriteWaitMs = 0;
  finishMs = 0;
  stagingBytesPerFrame = 0;
  peakReadbackBytes = 0;
  outputReadbackBytes = 0;
  outputRepackBytes = 0;
  peakEncodeQueueBytes = 0;
  maxPendingMediaEvents = 0;
  peakInFlightReadbacks = 0;
  maxEncodeQueueEvents = 0;
  encodeBackpressureCount = 0;
  pipelineOverlapFrames = 0;
  #pendingTotal = 0;
  #pendingSamples = 0;
  readonly #limits: {
    readonly maxInFlightReadbacks: number;
    readonly maxPendingVideoWrites: number;
    readonly inFlightByteBudget: number;
  };

  constructor(limits: {
    readonly maxInFlightReadbacks: number;
    readonly maxPendingVideoWrites: number;
    readonly inFlightByteBudget: number;
  }) {
    this.#limits = limits;
  }

  /** Records the pending readbacks plus writes at one turn of the loop. */
  samplePending(depth: number): void {
    this.maxPendingMediaEvents = Math.max(this.maxPendingMediaEvents, depth);
    this.#pendingTotal += depth;
    this.#pendingSamples += 1;
  }

  finish(videoEncodeLatencyMs?: EncodeLatency): BrowserExportPerformance {
    return {
      pipelineEnabled: true,
      sourceSupplyMs: this.sourceSupplyMs,
      frameSubmitMs: this.frameSubmitMs,
      readbackMapLatencyMs: this.readbackMapLatencyMs,
      readbackWaitMs: this.readbackWaitMs,
      readbackBlockingMs: this.readbackWaitMs,
      encodeQueueWaitMs: this.encodeQueueWaitMs,
      videoWriteWaitMs: this.videoWriteWaitMs,
      finishMs: this.finishMs,
      maxInFlightReadbacks: this.#limits.maxInFlightReadbacks,
      maxPendingVideoWrites: this.#limits.maxPendingVideoWrites,
      inFlightByteBudget: this.#limits.inFlightByteBudget,
      stagingBytesPerFrame: this.stagingBytesPerFrame,
      peakReadbackBytes: this.peakReadbackBytes,
      outputReadbackBytes: this.outputReadbackBytes,
      outputRepackBytes: this.outputRepackBytes,
      peakEncodeQueueBytes: this.peakEncodeQueueBytes,
      maxPendingMediaEvents: this.maxPendingMediaEvents,
      averagePendingMediaEvents:
        this.#pendingSamples === 0
          ? 0
          : this.#pendingTotal / this.#pendingSamples,
      peakInFlightReadbacks: this.peakInFlightReadbacks,
      maxEncodeQueueEvents: this.maxEncodeQueueEvents,
      encodeBackpressureCount: this.encodeBackpressureCount,
      pipelineOverlapFrames: this.pipelineOverlapFrames,
      videoEncodeLatencyMs: videoEncodeLatencyMs ?? {
        mean: 0,
        max: 0,
        count: 0,
      },
    };
  }
}

/** Every field of {@link BrowserExportPerformance}, in report order. */
export const BROWSER_EXPORT_PERFORMANCE_FIELDS: readonly (keyof BrowserExportPerformance)[] =
  Object.freeze(
    Object.keys(
      new ExportMetrics({
        maxInFlightReadbacks: 0,
        maxPendingVideoWrites: 0,
        inFlightByteBudget: 0,
      }).finish(),
    ) as (keyof BrowserExportPerformance)[],
  );

// What a browser export loop measures, under names that say what each figure is. The SDK's
// `exportToSink` and the editor's export loop are separate loops; they report through this one
// accumulator so the same field means the same quantity in both. Where a native host field of the
// same name exists it measures the same thing; browser-only quantities have names of their own.

/** What a probe encoder did: see {@link VideoLatencyProbe}. */
export type EncoderProbeOutcome = "output" | "silent" | "failed" | "timed-out";

/**
 * How `videoEncoderLatencyMode` (`alpha2/mediabunny`) chose the latency mode: `mode`, after probes
 * that took `ms` in all, the last of which ended in `outcome`: `"output"` (a chunk came out),
 * `"silent"` (the encoder flushed with nothing out), `"failed"` (refused or errored), `"timed-out"`
 * (nothing within the probe's time, which proves nothing), `"unavailable"` (no `VideoEncoder`).
 * `cached` when the answer was measured for an earlier export; only definitive answers are kept.
 */
export interface VideoLatencyProbe {
  readonly mode: "quality" | "realtime";
  readonly ms: number;
  readonly outcome: EncoderProbeOutcome | "unavailable";
  readonly cached: boolean;
}

/**
 * What an AAC-LC encoder made of a delay probe: `"output"` (it encoded and its stream decoded),
 * `"failed"` (it, or the decoder, refused or errored), `"timed-out"` (no answer within the probe's
 * time), `"unavailable"` (no WebCodecs audio).
 */
export type AacProbeOutcome = "output" | "failed" | "timed-out" | "unavailable";

/** What an AAC-LC encoder made of a probe: see `aacEncoderPrimingProbe` (`alpha2/mediabunny`). */
export interface AacEncoderProbe {
  /** The rate its stream declares; undefined when it put out nothing. */
  readonly rate?: number;
  /** Its priming in frames; 0 when it was not found or the stream is at another rate. */
  readonly delay: number;
  readonly outcome: AacProbeOutcome;
  /** How long the probes took, in all. */
  readonly ms: number;
  /** Probes run: a second after one that timed out. */
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
  /**
   * Time the loop waited for its source supply: decoding the missing source frames, converting and
   * uploading them.
   */
  readonly sourceSupplyMs: number;
  /** Worker/wasm evaluation, render, submission and response time. */
  readonly frameSubmitMs: number;
  /** End-to-end GPU mapping latency, including time overlapped with other work. */
  readonly readbackMapLatencyMs: number;
  /** Time for which ordered export progress was blocked on GPU mapping. */
  readonly readbackWaitMs: number;
  /** The same figure under the native host's name. */
  readonly readbackBlockingMs: number;
  /** Time a rendered frame waited behind earlier writes before its own write began. */
  readonly encodeQueueWaitMs: number;
  /**
   * Time spent in the sink's video write: handing the frame to the encoder, including waiting for
   * room in its queue. Encoding itself overlaps it and later writes; see `videoEncodeLatencyMs`.
   */
  readonly videoWriteWaitMs: number;
  /** Time to flush the encoders and finalize the file after the last frame. */
  readonly finishMs: number;
  /** The loop's configured limit on frames reading back at once. */
  readonly maxInFlightReadbacks: number;
  /** The loop's configured limit on video writes pending at once. */
  readonly maxPendingVideoWrites: number;
  /** Readback bytes the loop may hold at once. */
  readonly inFlightByteBudget: number;
  readonly stagingBytesPerFrame: number;
  readonly peakReadbackBytes: number;
  /** Mapped staging bytes read back: the raw video that crossed from the GPU to script. */
  readonly outputReadbackBytes: number;
  /** Bytes copied to pack read-back planes tight where `VideoFrame` ignores `layout`; else 0. */
  readonly outputRepackBytes: number;
  readonly peakEncodeQueueBytes: number;
  /** Readbacks and writes pending at once, at most and on average over the loop's turns. */
  readonly maxPendingMediaEvents: number;
  readonly averagePendingMediaEvents: number;
  /** Frames reading back at once, at most. */
  readonly peakInFlightReadbacks: number;
  /** Video writes pending at once, at most. */
  readonly maxEncodeQueueEvents: number;
  /** Frames that waited for an earlier write. */
  readonly encodeBackpressureCount: number;
  /** Frames submitted while writes were pending. */
  readonly pipelineOverlapFrames: number;
  /**
   * From a frame's hand-over to the encoder to its encoded packet, per frame: what the encoder
   * took. Zero counts when the sink does not report it.
   */
  readonly videoEncodeLatencyMs: EncodeLatency;
}

/** The figures a loop adds to as it runs; see {@link BrowserExportPerformance}. */
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

  /** `limits`: what the loop is configured to hold at once. */
  constructor(limits: {
    readonly maxInFlightReadbacks: number;
    readonly maxPendingVideoWrites: number;
    readonly inFlightByteBudget: number;
  }) {
    this.#limits = limits;
  }

  /** Records the readbacks and writes pending at one turn of the loop. */
  samplePending(depth: number): void {
    this.maxPendingMediaEvents = Math.max(this.maxPendingMediaEvents, depth);
    this.#pendingTotal += depth;
    this.#pendingSamples += 1;
  }

  /** The figures as reported, with the sink's encoder latency when it has one. */
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

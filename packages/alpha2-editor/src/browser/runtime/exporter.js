// Deterministic export: the engine renders offscreen at export quality on its own exact rational
// frame schedule, so no swapchain race can duplicate a frame and a missing source retries in place.

import {
  ExportFormatError,
  offeredAudioCodecs,
  resolveExportFormat,
} from "alpha2/containers";
import {
  audioEncoderSampleRate,
  canEncodeVideoTrack,
  MediabunnySink,
} from "alpha2/mediabunny";
import {
  engineVideoCodec,
  qualityLevels,
  resolveAudioBitrate,
  resolveVideoRateControl,
} from "alpha2/rate-control";
import {
  videoFrameFromI420,
  videoFrameRepackedBytes,
} from "alpha2/video-frames";
import * as engineBinding from "alpha2/wasm";
import {
  classifyBrowserFailure,
  ExportFailureError,
  withSecondaryFailure,
} from "alpha2/export-failure";
import { audibleSources } from "./audio.js";
import { exportAudioWriter } from "./export-audio.js";
import { storageSupport } from "./output.js";
import { estimatedStagingBytes, minimumExportWorkingSet } from "./staging.js";
import { DEFAULT_IN_FLIGHT_BYTES } from "./budget.js";
import { secondsToTicks } from "alpha2/project";

// Upper bounds on frames in each stage. Within them the byte budget decides: a frame is submitted
// only when its bytes fit.
const MAX_IN_FLIGHT_READBACKS = 3;
const MAX_PENDING_VIDEO_WRITES = 3;

/** The video codecs a browser export may request, with display labels. */
export const VIDEO_CODECS = [
  { value: "avc", label: "H.264 (AVC)" },
  { value: "hevc", label: "H.265 (HEVC)" },
  { value: "vp9", label: "VP9" },
  { value: "av1", label: "AV1" },
];

/** The engine's quality level names, lowest first. */
export function qualityLevelNames() {
  return qualityLevels(engineBinding);
}

/**
 * Resolves the video rate an export at `quality` will encode at, through the engine: the same
 * function the native host calls.
 */
export function resolvedVideoRate({
  codec,
  width,
  height,
  frameRate,
  quality,
}) {
  return resolveVideoRateControl(engineBinding, {
    codec,
    width,
    height,
    frameRate,
    quality,
  });
}

/** Maps the browser codec names onto the engine's. */
export { engineVideoCodec };

/**
 * Which video codecs this browser encodes for an export at `width`x`height`, `frameRate`, and
 * `videoQuality` (default `high`), asked with the configuration that export would open its encoder
 * with: a codec answered `false` is refused when such an export starts.
 */
export async function probeEncoders(
  width,
  height,
  { frameRate = { num: 30, den: 1 }, videoQuality = "high" } = {},
) {
  const results = {};
  for (const codec of VIDEO_CODECS)
    results[codec.value] = await canEncodeVideoTrack(codec.value, {
      width,
      height,
      rate: resolvedVideoRate({
        codec: engineVideoCodec(codec.value),
        width,
        height,
        frameRate,
        quality: videoQuality,
      }),
    });
  return results;
}

/**
 * Renders `range` (seconds) of the loaded project through the engine's export schedule and writes
 * it, with its mixed audio, to the output `openOutput` opens. Resolves to the sink's output, the
 * opened output (not yet finished), and the export report. Any failure discards the output and is
 * rethrown as an `ExportFailureError` with the phase it happened in; a cancellation rethrows the
 * abort reason. `createSink(options)` makes the muxer the frames and audio are written to, a
 * `MediabunnySink` unless a test supplies another. `openOutput` receives the export's container
 * beside the size estimate, so the file is named and typed for it.
 */
export async function exportTimeline({
  supplyFrame,
  renderFrame,
  sourceTransfer,
  engine,
  project,
  doc,
  library,
  audioSupplier,
  range,
  options,
  openOutput,
  onProgress,
  signal,
  createSink = (sinkOptions) => new MediabunnySink(sinkOptions),
}) {
  const started = performance.now();
  const diagnosticsBefore = rendererDiagnostics(engine);
  const transferBefore = sourceTransfer?.();
  const metrics = browserExportMetrics();
  const settings = doc.settings;
  const rate = exportFrameRate(options.frameRate, settings);
  const fps = rate.num / rate.den;
  const start = range.start;
  const end = Math.max(start + 1 / fps, range.end);
  let frameCount = Math.max(1, Math.round((end - start) * fps));
  const audio = project ? audibleSources(project, library) : [];
  const wantsAudio =
    options.includeAudio !== false && audio.length > 0 && engine != null;
  // The container's defaults fill what the options leave out; the audio codec is settled below,
  // by what this browser encodes.
  const format = exportFormat(options);
  const container = format.container;
  const audioQuality = options.audioQuality ?? "high";
  const audioEncoding = wantsAudio
    ? await chooseAudioCodec(settings.sampleRate, audioQuality, {
        container,
        codec: options.audioCodec,
      })
    : undefined;
  const audioCodec = audioEncoding?.codec;

  const videoCodec = format.videoCodec;
  const videoQuality = options.videoQuality ?? "high";
  // Resolved by the engine, so `high` costs the same here as on the native path.
  const videoRateControl = resolvedVideoRate({
    codec: engineVideoCodec(videoCodec),
    width: settings.width,
    height: settings.height,
    frameRate: rate,
    quality: videoQuality,
  });
  await requireVideoEncoder(videoCodec, settings, videoRateControl);
  const audioBitrate =
    audioCodec === undefined
      ? undefined
      : resolveAudioBitrate(engineBinding, audioCodec, audioQuality);
  // Pipelined steps carry their own phase (`inPhase`); this is only the main loop's.
  const position = { phase: "prepare", frameIndex: undefined };
  const enter = (phase, frameIndex) => {
    position.phase = phase;
    position.frameIndex = frameIndex;
  };
  // Before anything renders: refused here when the destination cannot hold the estimated file.
  const output = await openOutput({
    container,
    durationSeconds: end - start,
    videoBitrate: videoRateControl.bitrate,
    audioBitrate: audioBitrate ?? 0,
  });
  const sink = createSink({
    container,
    videoCodec,
    audioCodec,
    videoRateControl,
    audioBitrate,
    audioSampleRate: audioEncoding?.sampleRate,
    audioResampler: engineBinding,
    // Snapping to the exact rate is what keeps the written track CFR.
    frameRate: rate,
    destination: output.destination,
  });

  // Audio is written a window ahead of the video so the file comes out interleaved.
  let writeAudioThrough = async () => {};

  // Staged readbacks and queued encoder frames share one byte budget that must fit one of each.
  const budget = options.memory?.inFlightBytes ?? DEFAULT_IN_FLIGHT_BYTES;
  let frameCost = estimatedStagingBytes(settings.width, settings.height);
  const minimumWorkingSet = minimumExportWorkingSet(
    settings.width,
    settings.height,
  );
  metrics.memory = {
    inFlightByteBudget: budget,
    minimumWorkingSetBytes: minimumWorkingSet,
    peakInFlightBytes: 0,
    budgetWaits: 0,
    idleBytesReleased: 0,
    scope:
      "rendered frames this export holds; the browser's decoder, encoder, and driver memory are outside the budget",
  };
  if (minimumWorkingSet > budget) {
    await output.abort(new Error("refused")).catch(() => {});
    throw new ExportFailureError({
      code: "RESOURCE_LIMIT",
      message: `One ${settings.width}x${settings.height} frame needs ${minimumWorkingSet} bytes in flight (its GPU readback and the copy the encoder takes), over the ${budget}-byte budget`,
      origin: "browser",
      operation: "export",
      backend: rendererBackend(engine),
      phase: "prepare",
      resource: "unknown",
      requestedBytes: minimumWorkingSet,
    });
  }

  try {
    const videoShare = FRAMES_SHARE;
    engine.beginExport(
      BigInt(secondsToTicks(start)),
      BigInt(secondsToTicks(end)),
      rate.num,
      rate.den,
      engineVideoCodec(videoCodec),
    );
    // The engine's schedule, not the request's, decides the frames and the audio start.
    frameCount = JSON.parse(engine.exportProgress()).total;
    if (wantsAudio)
      writeAudioThrough = exportAudioWriter({
        engine,
        audioSupplier,
        sink,
        sampleRate: settings.sampleRate,
        signal,
        metrics,
        enter,
      });
    const pendingReadbacks = [];
    const pendingWrites = [];
    let writeTail = Promise.resolve();
    let pendingReadbackBytes = 0;
    let pendingEncodeBytes = 0;
    let produced = 0;
    try {
      let waitingAt = null;
      let waits = 0;
      let exportComplete = false;
      // The first scheduled frame starts at or before the range start, so it is the time origin;
      // the raw range start would make the first sample's timestamp negative.
      let originTicks = null;
      // Idle renderer memory is released once per pressure episode, which ends when all drains.
      let pressured = false;
      const inFlight = () => pendingReadbackBytes + pendingEncodeBytes;
      while (!exportComplete || pendingReadbacks.length > 0) {
        while (
          !exportComplete &&
          pendingReadbacks.length < MAX_IN_FLIGHT_READBACKS
        ) {
          if (inFlight() + frameCost > budget) {
            metrics.memory.budgetWaits += 1;
            if (!pressured && typeof engine.releaseIdleMemory === "function")
              metrics.memory.idleBytesReleased += engine.releaseIdleMemory();
            pressured = true;
            break;
          }
          if (inFlight() === 0) pressured = false;
          throwIfAborted(signal);
          enter("render", produced + pendingReadbacks.length);
          const submitStarted = performance.now();
          const ticket = renderFrame();
          metrics.frameSubmitMs += performance.now() - submitStarted;
          if (ticket.kind === "complete") {
            exportComplete = true;
            break;
          }
          if (ticket.kind === "waiting") {
            // The cursor has not advanced, so uploading and retrying is exact.
            waits = ticket.timeTicks === waitingAt ? waits + 1 : 0;
            waitingAt = ticket.timeTicks;
            if (waits >= 4) {
              // Like the SDK's export runner: a source not uploaded will not upload on retry.
              throw new ExportFailureError({
                code: "RESOURCE_LIMIT",
                message: `export stalled at ${(Number(ticket.timeTicks) / 120_000).toFixed(3)}s waiting for ${ticket.missingSources.join(", ")} after ${waits} refills: the supply does not upload them`,
                origin: "browser",
                operation: "export",
                backend: rendererBackend(engine),
                phase: "upload",
                stage: "sources-not-held",
                frameIndex: produced + pendingReadbacks.length,
                timeTicks: String(ticket.timeTicks),
                resource: "unknown",
                diagnostics: {
                  missingSources: [...ticket.missingSources],
                  refills: waits,
                },
              });
            }
            const supplyStarted = performance.now();
            enter("decode", produced + pendingReadbacks.length);
            await supplyFrame(
              Number(ticket.timeTicks) / 120_000,
              ticket.missingSources,
            );
            metrics.sourceConvertUploadMs += performance.now() - supplyStarted;
            continue;
          }
          waitingAt = null;
          metrics.stagingBytesPerFrame = ticket.retainedBytes;
          frameCost = ticket.retainedBytes;
          pendingReadbackBytes += ticket.retainedBytes;
          metrics.memory.peakInFlightBytes = Math.max(
            metrics.memory.peakInFlightBytes,
            inFlight(),
          );
          metrics.peakReadbackBytes = Math.max(
            metrics.peakReadbackBytes,
            pendingReadbackBytes,
          );
          const mappingStarted = performance.now();
          const readback = ticket.readback.finally(() => {
            metrics.readbackMapLatencyMs += performance.now() - mappingStarted;
          });
          void readback.catch(() => {});
          pendingReadbacks.push({
            readback,
            retainedBytes: ticket.retainedBytes,
          });
          metrics.maxInFlightReadbacks = Math.max(
            metrics.maxInFlightReadbacks,
            pendingReadbacks.length,
          );
          if (pendingWrites.length > 0) metrics.pipelineOverlapFrames += 1;
        }

        if (pendingReadbacks.length === 0) {
          if (exportComplete) continue;
          // Only encoder frames hold the budget: wait for the oldest to be encoded.
          const oldest = pendingWrites.shift();
          if (oldest === undefined)
            throw new ExportFailureError({
              code: "RESOURCE_LIMIT",
              message: `One frame (${frameCost} bytes) does not fit the ${budget}-byte in-flight budget`,
              origin: "browser",
              operation: "export",
              backend: rendererBackend(engine),
              phase: "render",
              frameIndex: produced,
              resource: "unknown",
              requestedBytes: frameCost,
            });
          await oldest.write;
          pendingEncodeBytes -= oldest.bytes;
          continue;
        }
        const pendingDepth = pendingReadbacks.length + pendingWrites.length;
        metrics.maxPendingMediaEvents = Math.max(
          metrics.maxPendingMediaEvents,
          pendingDepth,
        );
        metrics.pendingDepthTotal += pendingDepth;
        metrics.pendingDepthSamples += 1;
        const pendingReadback = pendingReadbacks.shift();
        const readbackWaitStarted = performance.now();
        enter("readback", produced);
        const step = await pendingReadback.readback;
        metrics.readbackWaitMs += performance.now() - readbackWaitStarted;
        pendingReadbackBytes -= pendingReadback.retainedBytes;
        metrics.outputReadbackBytes += pendingReadback.retainedBytes;
        const ticks = Number(step.timeTicks);
        if (originTicks === null) originTicks = ticks;
        const packedBytes = step.pixels.byteLength;
        enter("encode", produced);
        const frame = await i420VideoFrame(
          step,
          Math.round(((ticks - originTicks) / 120_000) * 1_000_000),
          Math.round((1_000_000 * rate.den) / rate.num),
        );
        const output = {
          index: BigInt(step.index),
          timeTicks: BigInt(ticks - originTicks),
          frame,
        };
        metrics.packedBytesPerFrame = packedBytes;
        metrics.outputRepackBytes += videoFrameRepackedBytes(frame);
        const encodeQueued = performance.now();
        if (pendingWrites.length > 0) metrics.encodeBackpressureCount += 1;
        const write = writeTail.then(
          async () => {
            metrics.encodeQueueWaitMs += performance.now() - encodeQueued;
            const encodeStarted = performance.now();
            try {
              await inPhase("encode", Number(output.index), () =>
                sink.writeVideo(output),
              );
            } finally {
              metrics.videoEncodeMs += performance.now() - encodeStarted;
            }
          },
          (error) => {
            frame.close();
            throw error;
          },
        );
        writeTail = write;
        pendingEncodeBytes += packedBytes;
        metrics.memory.peakInFlightBytes = Math.max(
          metrics.memory.peakInFlightBytes,
          inFlight(),
        );
        metrics.peakEncodeQueueBytes = Math.max(
          metrics.peakEncodeQueueBytes,
          pendingEncodeBytes,
        );
        pendingWrites.push({ write, bytes: packedBytes });
        metrics.maxEncodeQueueEvents = Math.max(
          metrics.maxEncodeQueueEvents,
          pendingWrites.length,
        );
        void write.catch(() => {});
        while (
          pendingWrites.length >= MAX_PENDING_VIDEO_WRITES ||
          (pendingWrites.length > 1 && inFlight() > budget)
        ) {
          const pendingWrite = pendingWrites.shift();
          await pendingWrite.write;
          pendingEncodeBytes -= pendingWrite.bytes;
        }
        produced += 1;
        await writeAudioThrough(produced / fps + AUDIO_LEAD_SECONDS);
        onProgress?.({
          phase: "video",
          ratio: (produced / frameCount) * videoShare,
          frame: produced,
          frameCount,
        });
      }
      await writeTail;
      pendingWrites.length = 0;
    } finally {
      await Promise.allSettled(
        pendingReadbacks.map(({ readback }) => readback),
      );
      await Promise.allSettled(pendingWrites.map(({ write }) => write));
      engine.endExport();
    }

    await writeAudioThrough(Infinity);

    enter("mux", undefined);
    onProgress?.({ phase: "finalizing", ratio: FRAMES_SHARE });
    const finishStarted = performance.now();
    await sink.finish();
    metrics.muxFinishMs = performance.now() - finishStarted;
    const written = sink.output;
    if (written === null || sink.bytesWritten === 0)
      throw new Error("the encoder returned an empty result");
    const destinationStats = output.stats();
    metrics.output = {
      mode: output.mode,
      bytes: sink.bytesWritten,
      estimatedBytes: output.estimatedBytes,
      peakQueuedBytes: destinationStats?.peakQueuedBytes ?? null,
      maxQueuedBytes: destinationStats?.maxQueuedBytes ?? null,
      storage: storageSupport(),
      // A report, not a check (`openExportOutput`).
      storageEstimate: output.storageEstimate ?? null,
    };
    const renderMs = performance.now() - started;
    const transferAfter = sourceTransfer?.();
    if (transferBefore && transferAfter) {
      metrics.sourcePlanarCopyFrames =
        transferAfter.planarCopyFrames - transferBefore.planarCopyFrames;
      metrics.sourcePlanarCopyBytes =
        transferAfter.planarCopyBytes - transferBefore.planarCopyBytes;
    }
    const result = {
      renderMs,
      frames: produced,
      audio: wantsAudio,
      container,
      videoCodec,
      // Named like the native host's result for native-versus-Wasm comparison.
      videoRateControl: {
        bitrate: videoRateControl.bitrate,
        mode: videoRateControl.bitrateMode,
        quantizer: videoRateControl.quantizer ?? null,
        level: typeof videoQuality === "string" ? videoQuality : null,
        source: "request",
      },
      // `quantizer` when the encoder accepted constant-quantizer encoding, else the bitrate path.
      appliedVideoRateControl: sink.appliedVideoRateControl ?? null,
      videoLatencyMode: sink.videoLatencyMode ?? null,
      audioCodec,
      audioRateControl:
        audioBitrate === undefined
          ? null
          : {
              bitrate: audioBitrate,
              level: typeof audioQuality === "string" ? audioQuality : null,
            },
      fps: produced / Math.max(renderMs / 1_000, Number.EPSILON),
      // A retry renders from the first frame: nothing of a failed attempt is kept.
      resume: {
        status: "unavailable",
        reason:
          "browser exports keep no frames between attempts; a retry renders from the first frame",
      },
      performance: finishBrowserExportMetrics(
        metrics,
        diagnosticsBefore,
        rendererDiagnostics(engine),
      ),
    };
    // One JSON object, so DevTools users can copy it without expanding a live console object.
    console.log(JSON.stringify(result));
    throwIfAborted(signal);
    return { written, output, result };
  } catch (error) {
    // Discard first, so a muxer that closes its stream on cancel cannot commit a partial file.
    const discarded = output.abort(error).then(
      () => null,
      (abortError) => abortError,
    );
    if (signal?.aborted) {
      await discarded;
      await sink.abort(
        error instanceof Error ? error : new Error(String(error)),
      );
      throw error;
    }
    const tagged = phaseOfError(error);
    let failure = classifyBrowserFailure(error, {
      phase: tagged?.phase ?? position.phase,
      frameIndex: tagged ? tagged.frameIndex : position.frameIndex,
      backend: rendererBackend(engine),
      diagnostics: {
        request: {
          width: settings.width,
          height: settings.height,
          frameRate: rate,
          startSeconds: start,
          endSeconds: end,
          frames: frameCount,
          videoCodec,
          audioCodec: audioCodec ?? null,
          audioSampleRate: audioEncoding?.sampleRate ?? null,
          sources: (project?.sources ?? []).map((source) => source.id),
        },
        output: {
          mode: output.mode,
          estimatedBytes: output.estimatedBytes,
          storageEstimate: output.storageEstimate ?? null,
        },
        memory: {
          scope:
            "bytes this export holds in flight; browser, decoder, and encoder memory not included",
          peakReadbackBytes: metrics.peakReadbackBytes,
          peakEncodeQueueBytes: metrics.peakEncodeQueueBytes,
          stagingBytesPerFrame: metrics.stagingBytesPerFrame,
          maxInFlightReadbacks: metrics.maxInFlightReadbacks,
          jsHeap: jsHeap(),
        },
        ...deviceDiagnostics(engine),
        environment: browserEnvironment(),
      },
    });
    const discardError = await discarded;
    if (discardError !== null)
      failure = withSecondaryFailure(failure, {
        code: "UNKNOWN",
        phase: "cleanup",
        message: String(discardError?.message ?? discardError),
      });
    try {
      await sink.abort(
        error instanceof Error ? error : new Error(String(error)),
      );
    } catch (abortError) {
      failure = withSecondaryFailure(failure, {
        code: "UNKNOWN",
        phase: "cleanup",
        message: String(abortError?.message ?? abortError),
      });
    }
    throw new ExportFailureError(failure, { cause: error });
  }
}

/** How far the audio is written ahead of the video. */
const AUDIO_LEAD_SECONDS = 0.25;

/** Share of the job the frames stand for; finalizing the file is the rest. */
const FRAMES_SHARE = 0.97;
const errorPhases = new WeakMap();

/**
 * Runs `step`, marking an error it throws with the phase and frame it came from, unless an earlier
 * step already marked it.
 */
async function inPhase(phase, frameIndex, step) {
  try {
    return await step();
  } catch (error) {
    if (typeof error === "object" && error !== null && !errorPhases.has(error))
      errorPhases.set(error, { phase, frameIndex });
    throw error;
  }
}

function phaseOfError(error) {
  return typeof error === "object" && error !== null
    ? errorPhases.get(error)
    : undefined;
}

/** The device's error log and the renderer's pooled GPU bytes; not its pass timings. */
function deviceDiagnostics(engine) {
  const diagnostics = rendererDiagnostics(engine);
  if (!diagnostics) return {};
  const pool = (stats) =>
    stats && {
      activeBytes: stats.activeBytes ?? stats.active_bytes ?? null,
      highWaterBytes: stats.highWaterBytes ?? stats.high_water_bytes ?? null,
    };
  return {
    gpu: diagnostics.gpu ?? null,
    adapter: diagnostics.adapter ?? null,
    gpuMemory: {
      scope:
        "renderer-pooled allocations; browser and driver memory not included",
      textures: pool(diagnostics.textures),
      uploadTextures: pool(diagnostics.uploadTextures),
      readbackBuffers: pool(diagnostics.readbackBuffers),
    },
  };
}

function rendererBackend(engine) {
  const backend = rendererDiagnostics(engine)?.adapter?.backend;
  if (backend === "BrowserWebGpu") return "browser-webgpu";
  if (typeof backend === "string") return "browser-webgl";
  return "unknown";
}

/**
 * Chrome's JS heap figures, where it reports them; they do not include ArrayBuffers, wasm, or GPU
 * memory.
 */
function jsHeap() {
  const memory = globalThis.performance?.memory;
  return memory
    ? {
        usedBytes: memory.usedJSHeapSize,
        totalBytes: memory.totalJSHeapSize,
        limitBytes: memory.jsHeapSizeLimit,
      }
    : null;
}

function browserEnvironment() {
  const navigator = globalThis.navigator;
  return {
    userAgent: navigator?.userAgent ?? null,
    deviceMemoryGiB: navigator?.deviceMemory ?? null,
    hardwareConcurrency: navigator?.hardwareConcurrency ?? null,
    webgpu: typeof navigator?.gpu === "object" && navigator.gpu !== null,
  };
}

function browserExportMetrics() {
  return {
    pipelineEnabled: true,
    sourceConvertUploadMs: 0,
    frameSubmitMs: 0,
    readbackMapLatencyMs: 0,
    readbackWaitMs: 0,
    encodeQueueWaitMs: 0,
    videoEncodeMs: 0,
    audioDecodeMs: 0,
    audioMixMs: 0,
    audioEncodeMs: 0,
    muxFinishMs: 0,
    stagingBytesPerFrame: 0,
    packedBytesPerFrame: 0,
    peakReadbackBytes: 0,
    outputReadbackBytes: 0,
    outputRepackBytes: 0,
    sourcePlanarCopyFrames: 0,
    sourcePlanarCopyBytes: 0,
    peakEncodeQueueBytes: 0,
    maxPendingMediaEvents: 0,
    pendingDepthTotal: 0,
    pendingDepthSamples: 0,
    maxInFlightReadbacks: 0,
    maxEncodeQueueEvents: 0,
    encodeBackpressureCount: 0,
    pipelineOverlapFrames: 0,
    audioBlocksInterleaved: 0,
    output: null,
    memory: null,
  };
}

function finishBrowserExportMetrics(metrics, before, after) {
  const result = {
    pipelineEnabled: metrics.pipelineEnabled,
    sourceConvertUploadMs: metrics.sourceConvertUploadMs,
    frameSubmitMs: metrics.frameSubmitMs,
    readbackMapLatencyMs: metrics.readbackMapLatencyMs,
    readbackWaitMs: metrics.readbackWaitMs,
    readbackBlockingMs: metrics.readbackWaitMs,
    encodeQueueWaitMs: metrics.encodeQueueWaitMs,
    videoEncodeMs: metrics.videoEncodeMs,
    audioDecodeMs: metrics.audioDecodeMs,
    audioMixMs: metrics.audioMixMs,
    audioEncodeMs: metrics.audioEncodeMs,
    muxFinishMs: metrics.muxFinishMs,
    inFlightDepth: MAX_IN_FLIGHT_READBACKS,
    encodeChannelDepth: MAX_PENDING_VIDEO_WRITES,
    stagingBytesPerFrame: metrics.stagingBytesPerFrame,
    packedBytesPerFrame: metrics.packedBytesPerFrame,
    peakReadbackBytes: metrics.peakReadbackBytes,
    // Mapped output readbacks plus HDR source frames copied out planar for the tone map.
    outputReadbackBytes: metrics.outputReadbackBytes,
    // Copies made to pack planes tight where VideoFrame ignores `layout`; 0 elsewhere.
    outputRepackBytes: metrics.outputRepackBytes,
    sourcePlanarCopyFrames: metrics.sourcePlanarCopyFrames,
    sourcePlanarCopyBytes: metrics.sourcePlanarCopyBytes,
    peakEncodeQueueBytes: metrics.peakEncodeQueueBytes,
    maxPendingMediaEvents: metrics.maxPendingMediaEvents,
    averagePendingMediaEvents:
      metrics.pendingDepthSamples === 0
        ? 0
        : metrics.pendingDepthTotal / metrics.pendingDepthSamples,
    maxInFlightReadbacks: metrics.maxInFlightReadbacks,
    maxEncodeQueueEvents: metrics.maxEncodeQueueEvents,
    encodeBackpressureCount: metrics.encodeBackpressureCount,
    pipelineOverlapFrames: metrics.pipelineOverlapFrames,
    audioBlocksInterleaved: metrics.audioBlocksInterleaved,
    memory: metrics.memory,
    output: metrics.output,
  };
  if (before == null || after == null) return result;
  const counters = [
    ["graphNodes", "graphNodes"],
    ["graphPasses", "graphRenderPasses"],
    ["renderPasses", "renderPasses"],
    ["sourceSubmissions", "sourceSubmissions"],
    ["graphSubmissions", "graphSubmissions"],
    ["readbackSubmissions", "readbackSubmissions"],
    ["frameSubmissions", "frameSubmissions"],
    ["gpuCopyOperations", "gpuCopyOperations"],
    ["gpuCopyBytes", "gpuCopyBytes"],
  ];
  for (const [output, input] of counters) {
    result[output] = Math.max(
      0,
      (after.work?.[input] ?? 0) - (before.work?.[input] ?? 0),
    );
  }
  appendPoolMetrics(result, "textures", "textures", "texture", before, after);
  appendPoolMetrics(
    result,
    "uploadTextures",
    "uploadTextures",
    "uploadTexture",
    before,
    after,
  );
  appendPoolMetrics(
    result,
    "readbackBuffers",
    "readbackBuffers",
    "readbackBuffer",
    before,
    after,
  );
  appendPoolMetrics(
    result,
    "uniformBuffers",
    "uniformBuffers",
    "uniformBuffer",
    before,
    after,
  );
  result.sourceUploads = Math.max(
    0,
    (after.sourceFrames?.uploads ?? 0) - (before.sourceFrames?.uploads ?? 0),
  );
  result.physicalSourceReuses = Math.max(
    0,
    (after.sourceFrames?.physicalReuses ?? 0) -
      (before.sourceFrames?.physicalReuses ?? 0),
  );
  return result;
}

function appendPoolMetrics(
  result,
  input,
  counterPrefix,
  highWaterPrefix,
  before,
  after,
) {
  for (const counter of ["Created", "Reused", "Discarded", "Evicted"]) {
    const key = counter.toLowerCase();
    if (after[input]?.[key] === undefined) continue;
    result[`${counterPrefix}${counter}`] = Math.max(
      0,
      after[input][key] - (before[input]?.[key] ?? 0),
    );
  }
  result[`${highWaterPrefix}HighWaterBytes`] =
    after[input]?.highWaterBytes ?? 0;
}

function rendererDiagnostics(engine) {
  if (typeof engine?.diagnostics !== "function") return null;
  try {
    return JSON.parse(engine.diagnostics());
  } catch {
    return null;
  }
}

async function i420VideoFrame(step, timestamp, duration) {
  if (step.format !== "I420" || step.width % 2 || step.height % 2) {
    throw new Error("wasm export returned an invalid I420 frame");
  }
  // Rows stay padded as the GPU wrote them; where the browser ignores `layout` (Safari),
  // `videoFrameFromI420` packs them tight first.
  const widths = [step.width, step.width / 2, step.width / 2];
  const rows = [step.height, step.height / 2, step.height / 2];
  if (
    step.layout?.length !== 3 ||
    step.layout.some(
      ({ offset, stride }, plane) =>
        stride < widths[plane] ||
        offset + stride * (rows[plane] - 1) + widths[plane] >
          step.pixels.byteLength,
    )
  ) {
    throw new Error("wasm export returned an invalid I420 plane layout");
  }
  if (typeof step.colorSpace?.matrix !== "string") {
    throw new Error("wasm export returned no color space for its I420 frame");
  }
  return videoFrameFromI420(step.pixels, {
    format: "I420",
    codedWidth: step.width,
    codedHeight: step.height,
    timestamp,
    duration,
    layout: [...step.layout],
    // Without it the browser tags the stream with its own default for a buffer-backed I420 frame.
    colorSpace: step.colorSpace,
  });
}

/** The audio codecs an export may write, with their names. */
const AUDIO_CODECS = { aac: "AAC", opus: "Opus" };

/**
 * The export's container and video codec, with the container's defaults for what `options` leave
 * out: `mp4` and its `avc`. A container that does not offer a requested codec (`webm` with `avc`,
 * `mov` with `opus`) is refused before anything renders, as `INVALID_INPUT`.
 */
export function exportFormat(options) {
  try {
    return resolveExportFormat({
      container: options.container ?? "mp4",
      ...(options.videoCodec === undefined
        ? {}
        : { videoCodec: options.videoCodec }),
      ...(options.audioCodec === undefined
        ? {}
        : { audioCodec: options.audioCodec }),
    });
  } catch (error) {
    if (!(error instanceof ExportFormatError)) throw error;
    throw new ExportFailureError({
      code: "INVALID_INPUT",
      message: `${error.message}.`,
      origin: "browser",
      operation: "export",
      backend: "unknown",
      phase: "prepare",
      resource: "unknown",
    });
  }
}

/**
 * The audio codec this browser encodes the export's audio with, and the rate its encoder is given:
 * `codec` when one is asked for, else the first of the container's codecs this browser encodes, in
 * the container's order (AAC, then Opus, in MP4 and MKV; AAC only in MOV; Opus only in WebM), each
 * at the project rate where the encoder takes it and otherwise at the rate a native host would
 * convert to (`audioEncoderSampleRate` in `alpha2/mediabunny`), so every project rate exports. A
 * browser that encodes none of them at any rate (Safari before 26 has a VideoEncoder but no
 * AudioEncoder; Firefox on Linux has no AAC encoder for a MOV) is refused here, before anything
 * renders, as `INVALID_INPUT`: a retry would meet the same browser.
 *
 * @returns {Promise<{ codec: "aac" | "opus", sampleRate: number }>}
 */
export async function chooseAudioCodec(
  sampleRate,
  audioQuality,
  { container = "mp4", codec: requested } = {},
) {
  const candidates =
    requested === undefined ? offeredAudioCodecs(container) : [requested];
  for (const codec of candidates) {
    const encoderRate = await audioEncoderSampleRate(codec, {
      sampleRate,
      numberOfChannels: 2,
      bitrate: resolveAudioBitrate(engineBinding, codec, audioQuality),
    });
    if (encoderRate !== null) return { codec, sampleRate: encoderRate };
  }
  throw new ExportFailureError({
    code: "INVALID_INPUT",
    message: `This browser cannot encode audio for ${container} (${candidates.map((codec) => AUDIO_CODECS[codec]).join(" or ")}, stereo) at ${sampleRate} Hz or at any rate it could be converted to. ${requested === undefined ? "Export without audio, choose another container" : "Choose another audio codec, export without audio"}, or use the native exporter.`,
    origin: "browser",
    operation: "export",
    backend: "unknown",
    phase: "prepare",
    resource: "unknown",
  });
}

/**
 * Refuses an export whose video codec this browser does not encode at the export's size and rate,
 * before anything renders, as `INVALID_INPUT`: WebCodecs leaves the codecs to the browser, and a
 * retry would meet the same browser.
 */
export async function requireVideoEncoder(codec, { width, height }, rate) {
  if (await canEncodeVideoTrack(codec, { width, height, rate })) return;
  const label =
    VIDEO_CODECS.find((c) => c.value === codec)?.label ?? String(codec);
  throw new ExportFailureError({
    code: "INVALID_INPUT",
    message: `This browser cannot encode ${label} video at ${width}x${height}. Choose another video codec, or use the native exporter.`,
    origin: "browser",
    operation: "export",
    backend: "unknown",
    phase: "prepare",
    resource: "unknown",
  });
}

/** The exact rational export rate: the override as given, else the project's own. */
function exportFrameRate(override, settings) {
  return override
    ? { num: override.num, den: override.den }
    : { num: settings.fpsNum, den: settings.fpsDen };
}

function throwIfAborted(signal) {
  if (signal?.aborted)
    throw signal.reason ?? new DOMException("Export cancelled", "AbortError");
}

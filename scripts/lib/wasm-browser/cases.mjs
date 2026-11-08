// The cases of test-wasm-browser.mjs in run order; each name is the page's path.

import {
  extractDigest,
  extractInvalidEmit,
  extractTimeline,
  pluginDigest,
} from "./extract.mjs";
import {
  aacLayoutRefusal,
  audioContinuity,
  audioMatroskaCodecDelay,
  audioSeekState,
  audioSpeakerLayout,
  audioStoreCapacity,
  audioStreamSelection,
  audioWindowPreroll,
  fixedAudioMix,
  loudnessUnderLimiter,
} from "./audio.mjs";
import {
  audioClockTracking,
  audioWorkletHighRate,
  audioWorkletOtherRate,
  audioWorkletQuanta,
} from "./audio-worklet.mjs";
import {
  bitmapUploads,
  layers,
  layout,
  layoutGrid,
  namedInputs,
  shapes,
  smoke,
  transparentEdges,
  webgl2EffectBackendCompile,
  webgl2EffectTextureBudget,
  webgl2ProbeWithoutOffscreenWebgl,
  webgl2UploadFallback,
  webgl2WithoutFloatColorBuffers,
  webglContextLoss,
  webglWorkerRecovery,
  webgpuAdapterInfo,
  webgpuDroppedWork,
  webgpuRejectedFrame,
  webgpuUnconfiguredSurface,
} from "./backends.mjs";
import {
  bindingRefusalCodes,
  effectRegistrationOverlap,
  engineErrorCodes,
  sha256BlobYields,
  textRasterParity,
  wasmFeatureProbe,
} from "./binding.mjs";
import { crashGpuOnSignal, gpuResetLadder } from "./recovery.mjs";
import {
  cpuGrid,
  cpuRuntimeEffectRefusedWhenDrawn,
  cpuWorkerHost,
  rendererFallbackAdapter,
  rendererWorker,
} from "./cpu.mjs";
import {
  exportAacPriming,
  exportEveryContainer,
  exportPixels,
  exportEveryProjectRate,
  exportSchedule,
  exportSurvivesFrozenPage,
  exportWaitingFrames,
  exportWorkingSet,
  freezeMidExport,
} from "./export.mjs";
import { exportPerf } from "./export-perf.mjs";
import {
  extensionModuleRestart,
  extensionProviderContract,
  extensionSvgBudgets,
} from "./extensions.mjs";
import {
  frameCacheBound,
  gpuMemoryRelease,
  planBudget,
  readbackCanaryBuffers,
  resultCacheRevisit,
  stillResidency,
  wasmMemoryBound,
} from "./memory.mjs";
import { previewFullResolution, previewUnchangedFrame } from "./preview.mjs";
import {
  browserVideoConversion,
  webcodecsDecodeOrder,
  decoderSoftwareFallback,
  hdrHevcSource,
  presentationCacheColor,
  presentationCacheResize,
  sdrChromaSiting,
  videoColorContract,
  yuvAnamorphic,
} from "./video.mjs";
import { heavyEffectSplit } from "./watchdog.mjs";

// \`check\` (function source) can require that the result shows the simulated branch ran.
const underSimulation = (names, body, check) =>
  `await simulate(${JSON.stringify(names)}${check ? `, ${check}` : ""});\n${body}`;

export const cases = {
  "extract-digest": extractDigest,
  "extract-invalid-emit": extractInvalidEmit,
  "extract-timeline": extractTimeline,
  "plugin-digest": pluginDigest,
  "webgpu-smoke": () => smoke(false),
  "webgl2-smoke": () => smoke(true),
  "webgl-context-loss": webglContextLoss,
  "webgl-worker-recovery": webglWorkerRecovery,
  "webgpu-dropped-work": webgpuDroppedWork,
  "webgpu-rejected-frame": webgpuRejectedFrame,
  "heavy-effect-split": heavyEffectSplit,
  "video-color-contract": videoColorContract,
  "browser-video-conversion": browserVideoConversion,
  "decoder-software-fallback": decoderSoftwareFallback,
  "hdr-hevc-source": hdrHevcSource,
  "hdr-hevc-source-degraded": () => hdrHevcSource({ degraded: true }),
  "export-survives-frozen-page": exportSurvivesFrozenPage,
  "sdr-chroma-siting": sdrChromaSiting,
  "audio-clock-tracking": () => audioClockTracking(48_000),
  "audio-clock-tracking-other-rate": () => audioClockTracking(44_100),
  "audio-worklet-other-rate": audioWorkletOtherRate,
  "audio-worklet-quanta": audioWorkletQuanta,
  "audio-worklet-high-rate": audioWorkletHighRate,
  "wasm-memory-bound": wasmMemoryBound,
  "wasm-rolling-audio-memory": () => wasmMemoryBound(true),
  "audio-store-capacity": audioStoreCapacity,
  "export-schedule": exportSchedule,
  "frame-cache-bound": frameCacheBound,
  "still-residency": stillResidency,
  "presentation-cache-resize": presentationCacheResize,
  "presentation-cache-color": presentationCacheColor,
  "export-waiting-frames": exportWaitingFrames,
  "export-working-set": exportWorkingSet,
  "audio-seek-state": audioSeekState,
  "audio-continuity": audioContinuity,
  "audio-window-preroll": audioWindowPreroll,
  "audio-matroska-codec-delay": audioMatroskaCodecDelay,
  "audio-speaker-layout": audioSpeakerLayout,
  "audio-stream-selection": audioStreamSelection,
  "aac-layout-refusal": aacLayoutRefusal,
  "fixed-audio-mix": fixedAudioMix,
  "export-every-project-rate": exportEveryProjectRate,
  "export-every-container": exportEveryContainer,
  "export-pixels": exportPixels,
  "export-aac-priming": exportAacPriming,
  "loudness-under-limiter": loudnessUnderLimiter,
  "text-raster-parity": textRasterParity,
  "webgl2-transparent-edges": () => transparentEdges(true),
  "webgpu-transparent-edges": () => transparentEdges(false),
  "webgl2-layers": () => layers(true),
  "webgpu-layers": () => layers(false),
  "webgl2-effect-backend-compile": webgl2EffectBackendCompile,
  "webgpu-named-inputs": () => namedInputs(false),
  "webgl2-named-inputs": () => namedInputs(true),
  "webgpu-shapes": () => shapes(false),
  "webgl2-shapes": () => shapes(true),
  "webgpu-layout": () => layout(false),
  "webgl2-layout": () => layout(true),
  "webgpu-layout-grid": () => layoutGrid(false),
  "webgl2-layout-grid": () => layoutGrid(true),
  "webgl2-effect-texture-budget": webgl2EffectTextureBudget,
  "webgl2-without-float-color-buffers": webgl2WithoutFloatColorBuffers,
  "webgpu-video-frame-rejected": () => transparentEdges(false, true),
  "webgl2-bitmap-uploads": () => bitmapUploads(true),
  "webgpu-bitmap-uploads": () => bitmapUploads(false),
  "webgpu-bitmap-uploads-rejected": () => bitmapUploads(false, true),
  "extension-module-restart": extensionModuleRestart,
  "extension-svg-budgets": extensionSvgBudgets,
  "extension-provider-contract": extensionProviderContract,
  "gpu-memory-release": gpuMemoryRelease,
  "readback-canary-buffers": readbackCanaryBuffers,
  "plan-budget": planBudget,
  "engine-error-codes": engineErrorCodes,
  "binding-refusal-codes": bindingRefusalCodes,
  "result-cache-revisit": resultCacheRevisit,
  "effect-registration-overlap": effectRegistrationOverlap,
  "yuv-anamorphic": yuvAnamorphic,
  "webgl2-probe-without-offscreen-webgl": webgl2ProbeWithoutOffscreenWebgl,
  "webgl2-upload-fallback": webgl2UploadFallback,
  "wasm-feature-probe": wasmFeatureProbe,
  "sha256-blob-yields": sha256BlobYields,
  "webgpu-unconfigured-surface": webgpuUnconfiguredSurface,
  "webgpu-adapter-info": webgpuAdapterInfo,
  "preview-unchanged-frame": previewUnchangedFrame,
  "preview-full-resolution": previewFullResolution,
  "cpu-worker-host": () => cpuWorkerHost("cpu", 640, 360),
  "cpu-worker-host-1080p": () => cpuWorkerHost("cpu", 1920, 1080),
  "renderer-auto": () => cpuWorkerHost("auto", 640, 360),
  "renderer-auto-worker": () => rendererWorker("auto"),
  "renderer-auto-fallback-adapter": rendererFallbackAdapter,
  "gpu-reset-ladder": gpuResetLadder,
  "cpu-worker": () => rendererWorker("cpu"),
  "cpu-grid-1080p": cpuGrid,
  "cpu-runtime-effect-refused-when-drawn": cpuRuntimeEffectRefusedWhenDrawn,
  "cpu-fixed-audio-mix": () => fixedAudioMix(true),
  "webcodecs-decode-order": webcodecsDecodeOrder,
  "safari-sim-quality-silent": () =>
    underSimulation(
      ["quality-silent"],
      exportEveryContainer(),
      `(result) => { const modes = Object.values(result.results).filter((entry) => entry?.videoCodec === "avc").map((entry) => entry.latencyProbe?.mode); if (modes.length === 0) return { skipped: "this browser has no H.264 encoder, which the simulation is about", capability: "webcodecs-video" }; return modes.every((mode) => mode === "realtime") ? result : { ...result, ok: false, simulation: "the H.264 encoders did not open in realtime mode", modes }; }`,
    ),
  // The simulated drop removes the leading silent frame, so priming measures as own + 2112 - 1024.
  "safari-sim-aac-priming": () =>
    underSimulation(
      ["aac-first-frame-drop", "aac-priming-2112"],
      exportAacPriming(),
      `(result) => result.delay >= 2112 - 1024 ? result : { ...result, ok: false, simulation: "the measured priming does not hold the simulated 2112 frames less the dropped first frame" }`,
    ),
  "safari-sim-aac-rate": () =>
    underSimulation(["aac-rate"], exportEveryProjectRate()),
  "safari-sim-frame-layout": () =>
    underSimulation(["frame-layout-ignored"], cpuWorkerHost("cpu", 640, 360)),
  "safari-sim-decode-order": () =>
    underSimulation(
      ["decode-order"],
      webcodecsDecodeOrder(),
      `(result) => globalThis.decodeOrderOutputs > 0 ? { ...result, decodeOrderOutputs } : { ...result, ok: false, simulation: "no frame came out in decode order" }`,
    ),
};

export const pageDrivers = {
  "export-survives-frozen-page": freezeMidExport,
  "gpu-reset-ladder": crashGpuOnSignal,
};

// `domainBlocking3d` keeps Chrome's GPU-reset policy; `gpuProcessExits` is how many GPU process
// exits the case causes itself.
export const caseLaunch = {
  "gpu-reset-ladder": {
    domainBlocking3d: true,
    gpuProcessExits: 2,
    timeoutMs: 120_000,
  },
  // A 1080p export on the CPU renderer takes about a minute on a two-core machine.
  "cpu-worker-host-1080p": { timeoutMs: 180_000 },
  "cpu-grid-1080p": { timeoutMs: 180_000 },
};

// Need inputs the default run does not make.
export const perfCases = {
  "export-perf": exportPerf,
};

export const videoImportCases = new Set([
  "webgpu-transparent-edges",
  "webgpu-video-frame-rejected",
  "webgpu-bitmap-uploads",
  "webgpu-bitmap-uploads-rejected",
  "video-color-contract",
  "browser-video-conversion",
  "sdr-chroma-siting",
  "presentation-cache-resize",
]);

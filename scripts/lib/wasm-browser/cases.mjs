// The cases of test-wasm-browser.mjs, in the order they run: each name is the page's path and maps
// to the body of the page (lib/wasm-browser/page.mjs wraps it).

import {
  aacLayoutRefusal,
  audioContinuity,
  audioMatroskaCodecDelay,
  audioSeekState,
  audioSpeakerLayout,
  audioStoreCapacity,
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
  cpuRuntimeEffectRefused,
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

// A case's body run under Safari's behaviours simulated in Chrome (lib/wasm-browser/safari-sim.mjs):
// the simulations are installed before the body loads the binding, and \`check\` (the source of a
// function of the result) can require that the result shows the simulated branch ran.
const underSimulation = (names, body, check) =>
  `await simulate(${JSON.stringify(names)}${check ? `, ${check}` : ""});\n${body}`;

export const cases = {
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
  "cpu-runtime-effect-refused": cpuRuntimeEffectRefused,
  "cpu-fixed-audio-mix": () => fixedAudioMix(true),
  "webcodecs-decode-order": webcodecsDecodeOrder,
  // Safari's behaviours, simulated: the branches that handle them run in Chrome.
  "safari-sim-quality-silent": () =>
    underSimulation(
      ["quality-silent"],
      exportEveryContainer(),
      `(result) => { const modes = Object.values(result.results).filter((entry) => entry?.videoCodec === "avc").map((entry) => entry.latencyProbe?.mode); return modes.length > 0 && modes.every((mode) => mode === "realtime") ? result : { ...result, ok: false, simulation: "the H.264 encoders did not open in realtime mode", modes }; }`,
    ),
  // The probe's noise burst follows a frame of silence, which the simulated drop removes: the
  // measured priming is the encoder's own plus 2112, less 1024.
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

// What drives a case's page from outside while it runs, by DevTools: Chrome only.
export const pageDrivers = {
  "export-survives-frozen-page": freezeMidExport,
  "gpu-reset-ladder": crashGpuOnSignal,
};

// How a case's Chrome is launched, where it differs from the harness's: `domainBlocking3d` keeps
// Chrome's policy of withdrawing GPU access from a page after GPU resets (the harness turns it
// off), and `gpuProcessExits` is how many GPU process exits the case causes itself, which are not
// an environment fault (more are a failure); `timeoutMs` is how long its page has.
export const caseLaunch = {
  "gpu-reset-ladder": {
    domainBlocking3d: true,
    gpuProcessExits: 2,
    timeoutMs: 120_000,
  },
};

// Cases run only when `--case` names them: they need inputs the default run does not make.
export const perfCases = {
  "export-perf": exportPerf,
};

// Cases that import a VideoFrame into WebGPU, which some browsers cannot do.
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

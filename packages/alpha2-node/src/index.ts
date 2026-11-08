import { type ChildProcess, spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { constants, existsSync, statSync } from "node:fs";
import { access } from "node:fs/promises";
import { delimiter, dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import type {
  AssetCatalog,
  ConformReport,
  MediaPurpose,
  Selection,
} from "alpha2/assets";
import type { CapabilityReport } from "alpha2";
import {
  ExportFormatError,
  resolveEngineExportFormat,
  resolvePixels,
} from "alpha2/containers";
import type { ExportFailure, ExportPhase } from "alpha2/export-failure";
import { failureForCode } from "alpha2/export-failure";
import type { BuiltinEffectCatalog, EffectDefinition } from "alpha2/effects";
import type { FrameRate, MediaFingerprint, Project } from "alpha2/project";
import {
  MAX_PROJECT_JSON_BYTES,
  validateProject,
  validateProjectJson,
} from "alpha2/project";

import {
  type HostMessage,
  type ProgressMessage,
  type ProgressPhase,
  type ProtocolError,
  type ResponseMessage,
  createRequest,
  describeExit,
  loaderRefused,
  windowsPolicyBlocked,
} from "./protocol.js";
import { type Frame, HostWire, encodeMessage } from "./host-wire.js";
import {
  glibcHint,
  hostBlockedMessage,
  hostStartTimeout,
  hostStartTimeoutMessage,
  oomKillCount,
  spawnRefusal,
  windowsStatus,
} from "./host-start.js";
import { quarantineRefusal } from "./quarantine.js";
import {
  cachedHostBinary,
  hostCacheDirectory,
  hostExecutableName,
  packageVersion,
} from "./install.js";

export { PROTOCOL_VERSION } from "./protocol.js";
export {
  Alpha2HostInstallError,
  HOST_DOWNLOAD_URL,
  hostCacheDirectory,
  hostExecutableName,
  hostTarget,
  installHost,
  installOpenH264,
  OPENH264_DOWNLOAD_URL,
} from "./install.js";
export type {
  HostArchiveVerification,
  HostInstallErrorCode,
  InstallHostOptions,
  InstallHostResult,
  InstallOpenH264Options,
  InstallOpenH264Result,
} from "./install.js";
export type { ProgressPhase } from "./protocol.js";
export type { ExportFailure } from "alpha2/export-failure";

/** Where to find `alpha2-host`; see {@link resolveHostBinary}. */
export interface ResolveHostBinaryOptions {
  /** Exact binary path. Takes precedence over every discovery mechanism. */
  readonly hostPath?: string;
  /** Electron's `process.resourcesPath`, injectable for non-Electron callers. */
  readonly resourcesPath?: string;
  /** Base directory for relative explicit paths. Defaults to `process.cwd()`. */
  readonly cwd?: string;
  /** The cache `installHost` installs into. Defaults to {@link hostCacheDirectory}. */
  readonly cacheDirectory?: string;
}

/** Options of an {@link Alpha2NodeClient}. */
export interface Alpha2NodeOptions extends ResolveHostBinaryOptions {
  /** Most host processes the client runs at once, each running one request. Defaults to one. */
  readonly concurrency?: number;
  /**
   * Video decode path for every request. Unset uses the host default: "auto" on Linux and macOS,
   * "software" on Windows. "hardware-required" fails instead of decoding a frame on the CPU.
   */
  readonly videoDecode?: VideoDecodeMode;
  /**
   * Renderer for every request. Unset uses `ALPHA2_RENDERER`, else `auto`, which picks a hardware
   * GPU or the CPU renderer but never a software GPU adapter. `gpu` accepts any adapter and fails
   * with `GPU_INIT_FAILED` without one. `cpu` opens no graphics driver.
   */
  readonly renderer?: RendererMode;
  /**
   * Milliseconds a started host has to report that it is ready. Defaults to 30000; `Infinity`
   * waits forever. On timeout the host is killed and the request fails with `HOST_START_TIMEOUT`.
   * Values not above zero are `INVALID_INPUT`.
   */
  readonly hostStartTimeoutMs?: number;
}

/** Video decode path requested from the native host. */
export type VideoDecodeMode = "software" | "auto" | "hardware-required";

const VIDEO_DECODE_MODES: readonly VideoDecodeMode[] = [
  "software",
  "auto",
  "hardware-required",
];

/** Rejects an invalid mode before a host process is spawned. */
function checkVideoDecode(field: string, value: unknown): void {
  if (
    value !== undefined &&
    !(VIDEO_DECODE_MODES as readonly unknown[]).includes(value)
  )
    throw new Alpha2HostError(
      "INVALID_INPUT",
      `${field} must be software, auto, or hardware-required`,
    );
}

/** Renderer requested from the native host; see {@link Alpha2NodeOptions.renderer}. */
export type RendererMode = "auto" | "gpu" | "cpu";

const RENDERER_MODES: readonly RendererMode[] = ["auto", "gpu", "cpu"];

function checkRenderer(field: string, value: unknown): void {
  if (
    value !== undefined &&
    !(RENDERER_MODES as readonly unknown[]).includes(value)
  )
    throw new Alpha2HostError(
      "INVALID_INPUT",
      `${field} must be auto, gpu, or cpu`,
    );
}

/** Engine quality levels; the host resolves each to a bitrate for the codec, size and rate. */
export type QualityLevel = "very-low" | "low" | "medium" | "high" | "very-high";

/** Every {@link QualityLevel}, lowest first. */
export const QUALITY_LEVELS: readonly QualityLevel[] = [
  "very-low",
  "low",
  "medium",
  "high",
  "very-high",
];

/** A quality level, or an explicit rate in bits per second. */
export type VideoQuality =
  | QualityLevel
  | {
      readonly bitrate: number;
      /** Defaults to `variable`. */
      readonly mode?: "constant" | "variable";
      /**
       * Constant quantizer on the codec's scale (0-51 for H.264). Encoders without one use
       * `bitrate`; `videoRateControl.applied` reports which ran.
       */
      readonly quantizer?: number;
    };

/** A quality level, or an explicit stereo bitrate in bits per second. */
export type AudioQuality = QualityLevel | { readonly bitrate: number };

/** The rate an export actually encoded video at, as the host resolved it. */
export interface ResolvedVideoRateControl {
  readonly bitrate: number;
  readonly mode: "constant" | "variable";
  readonly quantizer: number | null;
  /**
   * What the encoder ran. `quality` means the quantizer was mapped to an encoder without QP (such
   * as `h264_videotoolbox`). `bitrate` with `quantizerRefused` set means the quantizer was refused.
   */
  readonly applied: "quantizer" | "quality" | "bitrate";
  readonly quantizerRefused: string | null;
  /** The level the request named, or null for an explicit rate. */
  readonly level: QualityLevel | null;
  /** `env-override` means the test-only `ALPHA2_EXPORT_VIDEO_BITRATE` overrode the request. */
  readonly source: "request" | "env-override";
}

/** Whether an export may copy its source's packets; see {@link ExportProjectRequest.strategy}. */
export type ExportStrategy = "render" | "prefer-copy" | "require-copy";

/** How an export's encoder receives frames; see {@link ExportProjectRequest.inputPolicy}. */
export type InputPolicy = "auto" | "gpu-required" | "cpu-readback";

/** One progress report of an export. */
export interface ExportProgress {
  /**
   * `frames` while encoding, `finalizing` while the output is flushed and closed. The export is
   * complete only when `exportProject` resolves.
   */
  readonly phase: ProgressPhase;
  /** Frames encoded so far; at most `total`. */
  readonly completed: number;
  /** Frames in the export. */
  readonly total: number;
}

/**
 * Receives an export's progress. A promise it returns is waited for before the export settles, and
 * its rejection fails the export.
 */
export type ExportProgressCallback = (
  progress: ExportProgress,
) => void | Promise<void>;

/** How a finished output was moved into place. */
export interface PublishReport {
  /**
   * `rename-no-replace` is a single no-clobber rename. `placeholder-rename` is used where the file
   * system lacks that (exFAT on macOS, FUSE mounts): an empty file is created exclusively, then
   * renamed over. `rename-replace` is used when the request overwrites.
   */
  readonly method:
    "rename-no-replace" | "placeholder-rename" | "rename-replace";
  /** The output folder cannot seek back, so the file was staged in the temp dir and copied. */
  readonly stagedLocally: boolean;
  /** Tries repeated while another program held a file without sharing deletion (Windows). */
  readonly retries: number;
}

/** One video encoder an export tried before the one that wrote the video. */
export interface VideoEncoderFallback {
  readonly encoder: string;
  /** Whether it was tried on the GPU; a hybrid encoder can be tried on the GPU, then the CPU. */
  readonly hardwareAccelerated: boolean;
  /**
   * `absent` (not on this machine), `busy` (all sessions held; may open later), `unsuitable` (not
   * for this size or these options), or `unknown`.
   */
  readonly kind: "absent" | "busy" | "unsuitable" | "unknown";
  /** The typed rejection. */
  readonly reason: string;
  /** The encoder's own words. */
  readonly detail: string;
}

/** Successful metadata returned by the native host after an export. */
export interface NativeExportResult {
  /** The container the file was written in. */
  readonly container: ExportContainer;
  /** The GPU adapter that rendered, or the CPU renderer's name. */
  readonly adapter: string;
  /** The FFmpeg encoder that wrote the video; `null` for a packet copy, which encodes nothing. */
  readonly videoEncoder: string | null;
  /** `hardware` or `software`: how {@link videoEncoder} runs. */
  readonly videoEncoderClass: string | null;
  /** Whether {@link videoEncoder} encoded on the GPU or a media engine. */
  readonly videoEncoderHardware: boolean | null;
  /** The encoder the request pinned, else the first the host would have taken. */
  readonly videoEncoderPreferred: string | null;
  /** The encoders tried before {@link videoEncoder}, in the order tried; `[]` when none were. */
  readonly videoEncoderFallbacks: readonly VideoEncoderFallback[];
  /** The FFmpeg encoder that wrote the audio (`aac`, `libopus`); `null` without audio. */
  readonly audioEncoder: string | null;
  /** Whether the host could decode video on the GPU when the export ran. */
  readonly hardwareDecodeReady?: boolean;
  /** Why `auto` decode set up no hardware decoder at the host's start, when it could not. */
  readonly hardwareDecodeUnavailable?: string | null;
  readonly renderMs: number;
  readonly frames: number;
  readonly audio: boolean;
  readonly fps: number;
  /** The renderer that drew the frames. */
  readonly renderer: "gpu" | "cpu";
  /** Why the host renders on the CPU; `null` on a GPU. */
  readonly rendererReason: string | null;
  /** The CPU renderer's threads; absent on a GPU host. */
  readonly rendererThreads?: number;
  /**
   * Sorted font families the project names that no supplied font matches (any name-table name,
   * after NFC and lowercasing). Those texts used fallback fonts.
   */
  readonly unmatchedFontFamilies: readonly string[];
  /**
   * How frames reached the encoder. Some `performance` counters do not apply to every pipeline, so
   * check this before comparing zeros across exports.
   */
  readonly videoPipeline: "readback" | "gpu-resident" | "packet-copy";
  /** The absolute path of the file the export wrote. */
  readonly output: string;
  /** Why the export took the readback path; `null` on the GPU-resident path. */
  readonly gpuOutputPathUnavailable: string | null;
  /** Set when a GPU-resident export failed at `frame` and restarted on the readback path. */
  readonly gpuOutputPathFallback: {
    readonly encoder: string;
    readonly frame: number;
    readonly reason: string;
  } | null;
  /** How the finished file was moved to `output`. */
  readonly publish: PublishReport;
  /**
   * External library behind the video encoder (`openh264`), with the notice its licence requires.
   * `null` for built-in FFmpeg encoders and when no video was encoded.
   */
  readonly videoEncoderLibrary?: {
    readonly provider: "cisco-openh264" | "system";
    readonly version: string;
    readonly library: string;
    readonly notice: string;
  } | null;
  /** The request's {@link ExportProjectRequest.inputPolicy}. */
  readonly inputPolicy: InputPolicy;
  /** What the export did: rendered and encoded every frame, or copied the source's packets. */
  readonly strategy: "render" | "copy";
  /** The request's {@link ExportProjectRequest.strategy}. */
  readonly requestedStrategy: ExportStrategy;
  /** Why a `prefer-copy` export rendered instead; `null` when it copied or did not ask to. */
  readonly copyIneligible: string | null;
  /**
   * A copy's audio: `copy` when it is the untouched source audio, `encode` when it was mixed and
   * encoded, `null` without audio. Absent on rendered exports.
   */
  readonly audioStrategy?: "copy" | "encode" | null;
  /** Why a copy's audio was encoded rather than copied. Absent on rendered exports. */
  readonly audioCopyIneligible?: string | null;
  /** Clips a copy joined back to back. Absent on rendered exports. */
  readonly copiedSegments?: number;
  /**
   * `yuv-repack` when every frame was an unmodified source picture already in the output format
   * (size, orientation, BT.709 limited 8-bit 4:2:0), so decoded planes went straight to the
   * encoder without rendering. `null` when it rendered, and on a copy.
   */
  readonly videoTranscode: "yuv-repack" | null;
  /** Why a rendered export did not transcode; `null` when it did, and on a copy. */
  readonly yuvTranscodeIneligible: string | null;
  /**
   * Requested range widened to the frame grid. `startTicks` is the output's time zero and can be up
   * to one frame before the requested start.
   */
  readonly effectiveRange: {
    readonly startTicks: number;
    readonly endTicks: number;
  };
  /** The color pipeline the export ran. */
  readonly color: {
    readonly workingSpace: "linear_bt709_sdr";
    /**
     * What each intermediate picture was stored in: half float wherever the adapter renders,
     * filters, and copies it, 8-bit elsewhere; the tier does not decide it.
     */
    readonly intermediateFormat: "rgba16float" | "rgba8unorm";
    /** The project's HDR source policy, and the peak HDR sources were taken to reach. */
    readonly hdrSources: "tone_map" | "reject";
    readonly hdrPeakNits: number;
    readonly output: "bt709_sdr";
  };
  /** What the export's render cache did; `null` when it asked for none. */
  readonly renderCache: RenderCacheReport | null;
  /** What extension providers could not draw as asked, once per source and kind, in order seen. */
  readonly providerDiagnostics: readonly ProviderDiagnostic[];
  /** Distinct provider reports past the 256 `providerDiagnostics` keeps. */
  readonly providerDiagnosticsOmitted: number;
  /**
   * Decoder reports in source ID order: damaged input skipped under a `tolerant` decode policy,
   * silence inserted at timestamp gaps, dropped alpha or extra layers, and a decoder session
   * shortage.
   */
  readonly decodeDiagnostics: readonly (
    DecodeDiagnostics | LayerDroppedDiagnostic | DecoderSessionsShortDiagnostic
  )[];
  /** Renderer result cache counters (repeated frames and unchanged clips reused, not redrawn). */
  readonly resultCache: {
    readonly budgetBytes: number;
    readonly hits: number;
    readonly misses: number;
    readonly ineligible: number;
    readonly inserts: number;
    readonly evictions: number;
    readonly skippedNodes: number;
  };
  /**
   * Loudness normalization: the mix as measured, the planned gain (`applied`, `null` for a mix
   * below -70 LUFS), and the result after the master limiter (`output`). `null` without `loudness`.
   */
  readonly loudness: {
    readonly targetLufs: number;
    readonly truePeakCeilingDbtp: number;
    /** Linear master limiter threshold the plan stayed under; `null` without a limiter. */
    readonly limiterThreshold: number | null;
    readonly measured: {
      readonly integratedLufs: number | null;
      readonly loudnessRangeLu: number | null;
      readonly truePeakDbtp: number | null;
      readonly samplePeakDbfs: number | null;
    };
    readonly applied: {
      readonly gainDb: number;
      readonly integratedLufs: number;
      readonly truePeakDbtp: number;
      readonly peakLimited: boolean;
      readonly limiterLimited: boolean;
    } | null;
    /** The exported mix, after the master limiter, before the audio encoder. */
    readonly output: {
      readonly integratedLufs: number | null;
      readonly loudnessRangeLu: number | null;
      readonly truePeakDbtp: number | null;
      readonly samplePeakDbfs: number | null;
    };
  } | null;
  /** Video rate control the encoder was opened with; `null` for a copy. */
  readonly videoRateControl: ResolvedVideoRateControl | null;
  /** The audio bitrate, when the export has audio. */
  readonly audioRateControl: {
    readonly bitrate: number;
    readonly level: QualityLevel | null;
  } | null;
  /** Timing and pipeline counters. Their names are diagnostic and may change between versions. */
  readonly performance: ExportPerformance;
  /** The media each source read: its original, or a proxy when the request asked for proxies. */
  readonly media: Readonly<Record<string, Selection>>;
  readonly [metadata: string]: unknown;
}

/** {@link ExportPerformance.gpuSubmissions}. */
export interface GpuSubmissions {
  /** Frames the GPU rendered. */
  readonly frames: number;
  /** Frames submitted in more than one submission. */
  readonly splitFrames: number;
  /** The most submissions one frame took. */
  readonly maxPerFrame: number;
  /** Longest predicted GPU time of one submission; `null` when no frame needed splitting. */
  readonly maxPredictedMs: number | null;
}

/**
 * Timing and pipeline counters (see the host protocol). Undeclared counters are numbers, flags or
 * names. A copy reports its own few counters and none of the declared ones.
 */
export interface ExportPerformance {
  /** Per reason a source left the hardware decode path, how many times it did. */
  readonly hardwareFallbackReasons?: Readonly<Record<string, number>>;
  /** Per reason in `hardwareFallbackReasons` that has one, the first fallback's message. */
  readonly hardwareFallbackMessages?: Readonly<Record<string, string>>;
  /** Histogram of encode channel depth, indexed by frames left queued when a command was taken. */
  readonly encodeChannelDepthSamples?: readonly number[];
  /** What the GPU-resident path reserved from the in-flight budget; `null` on the readback path. */
  readonly gpuEncodeBudget?: GpuEncodeBudget | null;
  /**
   * How frames were split into GPU submissions, each kept under an eighth of the GPU watchdog
   * (`ALPHA2_GPU_WATCHDOG_MS`, 2000 ms). `null` on the CPU renderer.
   */
  readonly gpuSubmissions?: GpuSubmissions | null;
  /** CPU hosts only. */
  readonly cpuRenderMs?: number;
  /** Graph nodes the CPU renderer drew, excluding result cache hits. */
  readonly cpuRenderNodes?: number;
  /** Decoded source frames the CPU renderer converted to pictures. */
  readonly cpuPresentedSources?: number;
  /** Peak bytes of pictures one graph held at once on the CPU renderer. */
  readonly cpuImageHighWaterBytes?: number;
  /**
   * Decoder sessions kept open: the peak concurrent videos (at least 8), lowered by
   * `memory.decoderSessionBytes`, the cap of 32 or `ALPHA2_DECODER_SESSIONS`. Absent on a copy.
   */
  readonly decoderSessions?: number | null;
  /** What decided `decoderSessions`. */
  readonly decoderSessionLimit?:
    "workload" | "memory" | "override" | "cap" | null;
  /** The most videos the export reads at once. */
  readonly decoderSessionPeak?: number | null;
  /** Decoder reopens caused by running short of sessions. */
  readonly decoderSessionReopens?: number;
  readonly [counter: string]: PerformanceValue | undefined;
}

/** A value of {@link ExportPerformance}. */
export type PerformanceValue =
  | number
  | boolean
  | string
  | null
  | readonly number[]
  | Readonly<Record<string, number>>
  | Readonly<Record<string, string>>
  | GpuEncodeBudget
  | GpuSubmissions;

/** The device memory the GPU-resident path reserved, in bytes and surfaces. */
export interface GpuEncodeBudget {
  readonly surfaceBytes: number;
  readonly planeRingBytes: number;
  readonly leaseBound: number;
  readonly retainedSurfaces: number;
  readonly surfaceCeiling: number;
  readonly reservedBytes: number;
}

/**
 * One extension provider report. `kind` is `missing_font`, `missing_image`, `unreadable_image`,
 * `unsupported_feature`, `unreadable_item`, `normalized_delay`, `non_finite_samples`,
 * `out_of_half_range` or `reduced_precision`; other fields depend on the kind.
 */
export interface ProviderDiagnostic {
  readonly source: string;
  readonly kind: string;
  readonly [field: string]: unknown;
}

/** One source's decoder report. Audio samples are at the project's sample rate. */
export interface DecodeDiagnostics {
  readonly source: string;
  /** Corruption reports; FFmpeg may report one packet more than once. */
  readonly corruptPackets: number;
  /** Runs of dropped video between healthy frames, an unrecovered tail included. */
  readonly videoGaps: number;
  /** Audio samples decoded as silence in place of damaged input. */
  readonly concealedAudioSamples: number;
  /** Audio samples decoded as silence where a clean stream's timestamps jump. */
  readonly timestampGapSamples: number;
  /** Audio samples dropped because their timestamps repeated samples already decoded. */
  readonly droppedAudioSamples: number;
}

/** Fewer decoder sessions than concurrent videos, so some decoders were reopened in turns. */
export interface DecoderSessionsShortDiagnostic {
  readonly type: "decoderSessionsShort";
  readonly peak: number;
  readonly decoderSessions: number;
  readonly limit: "memory" | "override" | "cap";
  readonly reopens: number;
  readonly message: string;
}

/**
 * Part of a source's stream was not decoded: `alpha-dropped` for alpha this host cannot read (HEVC
 * auxiliary alpha, or VP9 alpha without libvpx), `extra-layers-dropped` for non-base HEVC layers.
 */
export interface LayerDroppedDiagnostic {
  readonly type: "layerDropped";
  readonly code: "alpha-dropped" | "extra-layers-dropped";
  readonly source: string;
  readonly reason: string;
}

/** Where {@link ExportProjectRequest.renderCache} bakes frames. */
export interface RenderCacheOptions {
  readonly directory: string;
  /** The most bytes the directory holds. Default 4 GiB. */
  readonly quotaBytes?: number;
  /** When the cache writer is full, `"skip"` (default) drops the frame; `"wait"` blocks. */
  readonly backlog?: "skip" | "wait";
}

/** What an export's render cache did. */
export type RenderCacheReport =
  | {
      readonly status: "used";
      readonly hits: number;
      readonly writes: number;
      readonly bytesRead: number;
      readonly bytesWritten: number;
      /** Frames a bypassed clip kept out of the cache. */
      readonly bypassedFrames: number;
      /** Frames that failed their checksum, deleted and rendered again. */
      readonly corrupt: number;
      readonly writeErrors: number;
      readonly evictions: number;
      /** Frames not baked because the quota had no room. */
      readonly quotaSkips: number;
      /** Frames not baked because the writer was full (`backlog: "skip"`). */
      readonly busySkips: number;
      readonly quotaBytes: number;
      readonly backlog: "skip" | "wait";
      /**
       * The cache got stuck in the file system and was abandoned; remaining frames were
       * rendered and the host stops using the directory.
       */
      readonly abandoned: boolean;
    }
  | { readonly status: "unavailable"; readonly reason: string };

/** A loudness target for {@link ExportProjectRequest.loudness}. */
export interface LoudnessTarget {
  /** Integrated loudness, `-60..=0` LUFS; streaming platforms use -14 to -16. */
  readonly targetLufs: number;
  /** True-peak ceiling, `-20..=0` dBTP; EBU R128 uses -1. */
  readonly truePeakCeilingDbtp: number;
}

/** A proxy made from a source's original. */
export interface ProxyFile {
  readonly path: string;
  /** Evidence of the original it was made from, when known. */
  readonly madeFrom?: MediaFingerprint;
  /** The color pipeline baked into it, when it was tone-mapped. */
  readonly colorPipeline?: string;
}

/** What {@link Alpha2NodeClient.exportProject} renders, and where it writes it. */
export interface ExportProjectRequest {
  readonly project: Project;
  /**
   * A file path per source id. Missing, offline or mismatched media is refused with an
   * {@link Alpha2ConformError} before rendering.
   */
  readonly assets: Readonly<Record<string, string>>;
  /** Proxies per source id, in order of preference. */
  readonly proxies?: Readonly<Record<string, readonly ProxyFile[]>>;
  /** Image sequence frame files per source id, in order; `null` marks a gap. */
  readonly frames?: Readonly<Record<string, readonly (string | null)[]>>;
  /**
   * External images referenced by SVG or Lottie sources, per source id and then per locator as
   * written (`{ label: { "art/badge.png": "/media/badge.png" } }`). Unlisted locators report
   * `missing_image`. Entries for other sources are `INVALID_INPUT`.
   */
  readonly resources?: Readonly<
    Record<string, Readonly<Record<string, string>>>
  >;
  /** `original` (default) or `proxies` (fresh proxies where available); see result `media`. */
  readonly media?: "original" | "proxies";
  /** A file path per effect resource id (LUT files and the like). */
  readonly effectAssets?: Readonly<Record<string, string>>;
  /** Font files for all text, in fallback order. A project that shows text fails without them. */
  readonly fonts?: readonly string[];
  /** Definitions of custom effects the project uses beyond the built-in catalog. */
  readonly effects?: readonly EffectDefinition[];
  /** The timeline range to export, in ticks; widened to the frame grid (see `effectiveRange`). */
  readonly startTicks: number;
  readonly endTicks: number;
  /** The output frame rate. */
  readonly frameRate: FrameRate;
  /** Whether the output has an audio track. Defaults to `true`. */
  readonly includeAudio?: boolean;
  /**
   * Defaults to the one `output`'s extension names; both must agree when given. Allowed codecs per
   * container are in `capabilities.encode.containers`; others are `INVALID_INPUT`.
   */
  readonly container?: ExportContainer;
  /** Defaults to the container's: `h264`, or `vp9` for WebM. `prores` is MOV only. */
  readonly videoCodec?:
    "h264" | "hevc" | "vp9" | "av1" | "mpeg4-part2" | "prores";
  /**
   * Draw without the project background and write an alpha plane. Needs 8-bit VP9 in WebM or
   * Matroska, or ProRes 4444 in MOV.
   */
  readonly alpha?: boolean;
  /** 8 or 10; defaults to the codec's own (10 for ProRes, else 8). 10 needs HEVC, AV1, VP9 or ProRes. */
  readonly bitDepth?: 8 | 10;
  /**
   * FFmpeg encoder name from `capabilities.encode.video` or `capabilities.encode.gpuOutputPath`.
   * Pinning a GPU-input-only encoder (`h264_vulkan`) requires the GPU-resident path.
   */
  readonly videoEncoder?: string;
  /**
   * `auto` (default) uses the GPU-resident path when possible, else readback. `gpu-required` fails
   * with `GPU_OUTPUT_UNAVAILABLE` instead of falling back. `cpu-readback` always reads back.
   */
  readonly inputPolicy?: InputPolicy;
  /**
   * `render` (default) renders and encodes every frame.
   *
   * `prefer-copy` copies the sources' compressed packets when the range is untouched clips placed
   * back to back (identity transform, no composition, mask, matte, caption, transition or retime),
   * cut at IDR pictures, and each source is MP4/MOV H.264 or HEVC at the output size, BT.709
   * limited 8-bit 4:2:0, square pixels, unmirrored, on the export's frame grid.
   *
   * A copy's audio is copied when it is the untouched source audio over the whole file, otherwise
   * mixed and encoded with `audioCodec`. `videoQuality` does not apply to a copy. When the range
   * does not qualify, `prefer-copy` renders and sets `copyIneligible`; `require-copy` fails with
   * `COPY_INELIGIBLE`.
   */
  readonly strategy?: ExportStrategy;
  /** Output video quality. Defaults to `high`. */
  readonly videoQuality?: VideoQuality;
  /** Defaults to the container's: `aac`, or `opus` for WebM. */
  readonly audioCodec?: "aac" | "opus";
  /** Output audio quality. Defaults to `high`. */
  readonly audioQuality?: AudioQuality;
  /**
   * Normalizes audio to an integrated loudness (ITU-R BS.1770-4, EBU R128) under a true-peak
   * ceiling, measured in a first pass. Needs `includeAudio` and an audible source, else
   * `INVALID_INPUT`.
   */
  readonly loudness?: LoudnessTarget;
  /**
   * Directory of baked frames. A later export with the same host build, GPU, project, files and
   * effects reuses them and writes the same file. Eviction is least recently used, pinned clips
   * last. Frames are baked on the readback path only.
   */
  readonly renderCache?: RenderCacheOptions;
  /**
   * Byte budgets that override the host's environment settings and defaults field by field. An
   * export whose minimum working set does not fit fails with `RESOURCE_LIMIT` before rendering.
   * On low-memory machines, budgets left at their defaults shrink to half of available memory;
   * budgets set here never do. The result's `memoryPolicy` reports what applied.
   */
  readonly memory?: ExportMemoryBudget;
  /** Overrides the client's decode mode for this export. */
  readonly videoDecode?: VideoDecodeMode;
  /** Overrides the client's renderer for this export. */
  readonly renderer?: RendererMode;
  /** File to write; extension `.mp4`, `.m4v`, `.mov`, `.mkv` or `.webm`, matching `container`. */
  readonly output: string;
  /**
   * Replace an existing regular file at `output` once the export completes. Defaults to `false`.
   * An `output` that is also one of the request's inputs is always refused.
   */
  readonly overwrite?: boolean;
  /** Cancels the export, removes its partial file, and rejects with `Alpha2AbortError`. */
  readonly signal?: AbortSignal;
  /**
   * A throw or rejection fails the export with `PROGRESS_CALLBACK_FAILED`. The export settles only
   * after every returned promise has.
   */
  readonly onProgress?: ExportProgressCallback;
  /**
   * Receives the result after a successful export. The export resolves after its promise does; a
   * throw or rejection rejects the export.
   */
  readonly onResult?: (result: NativeExportResult) => void | Promise<void>;
}

/** The byte budgets an export runs within. See `memoryPolicy` in the host protocol. */
export interface ExportMemoryBudget {
  /** Rendered frames between the GPU and the encoder. Default 256 MiB. */
  readonly inFlightBytes?: number;
  /** Decoded source frames waiting to be rendered. Default 256 MiB. */
  readonly decodeQueueBytes?: number;
  /** Frames a reversed clip keeps decoded. Default 256 MiB. */
  readonly reverseCacheBytes?: number;
  /** Decoded audio kept around the mix window. Default 256 MiB. */
  readonly audioWindowBytes?: number;
  /**
   * Open extension-provider sources (SVG, animated images, Lottie, OpenEXR, image sequences).
   * Past it, sources not shown in the last two frames are closed and reopened when needed.
   * Default 512 MiB.
   */
  readonly providerSourceBytes?: number;
  /**
   * Open video decoder sessions, estimated from the largest video. Bounds how many sessions stay
   * open; beyond that, decoders are reopened in turns (slower, same output). Default 4 GiB.
   */
  readonly decoderSessionBytes?: number;
  /** Fraction in (0, 1] applied to every resolved budget, for retrying with less in flight. */
  readonly scale?: number;
}

/** One encoder a native host can feed. */
export interface EncodeCapability {
  /** The codec id, such as `h264` or `aac`. */
  readonly id: string;
  /** The FFmpeg encoder short name, as `videoEncoder` takes it. */
  readonly encoder: string;
  /**
   * Where the encoder runs. `hybrid` is a MediaFoundation encoder, a software transform unless the
   * host is configured to request hardware.
   */
  readonly encoderClass?: "software" | "hardware" | "hybrid";
  readonly hardwareAccelerated?: boolean;
  readonly preferred?: boolean;
  /** Smallest output the encoder accepts, in pixels; `null` when it reports none. */
  readonly minimumWidth?: number | null;
  readonly minimumHeight?: number | null;
  /**
   * For a user-installed encoder (`openh264`): `cisco-openh264` for Cisco's verified binary or
   * `system` for a distro library, plus version, path and required licence notice. `null` for
   * built-in FFmpeg encoders.
   */
  readonly provider?: "cisco-openh264" | "system" | null;
  readonly version?: string | null;
  readonly library?: string | null;
  readonly notice?: string | null;
}

/**
 * Whether a native host can export H.264. `gpu-only` means only the GPU-resident path
 * (`gpuEncoder`) encodes it, so exports that path declines fail. `reason` and `remedy` are `null`
 * when `available`.
 */
export interface H264EncodeStatus {
  readonly status: "available" | "gpu-only" | "unavailable";
  /** The encoders that take CPU frames, most preferred first. */
  readonly encoders: readonly string[];
  readonly gpuEncoder: string | null;
  readonly reason: string | null;
  readonly remedy: string | null;
}

/** OpenH264, installed by the user (`npx @alpha2-sh/node install --openh264`). */
export interface OpenH264Capability {
  readonly status: "available" | "not-installed" | "disabled" | "unusable";
  /** The name `videoEncoder` takes: `openh264`. */
  readonly encoder: string;
  readonly provider: "cisco-openh264" | "system" | null;
  readonly version: string | null;
  readonly library: string | null;
  /** The OpenH264 release the install command is pinned to. */
  readonly pinnedVersion: string;
  /** "OpenH264 Video Codec provided by Cisco Systems, Inc.", which Cisco's licence requires. */
  readonly notice: string;
  readonly reason: string | null;
  readonly remedy: string | null;
}

/**
 * The GPU-resident output path. Its encoder may be absent from `capabilities.encode.video` (on
 * Linux, `h264_vulkan` takes hardware frames only). An export uses this path when the codec
 * matches, no other encoder is pinned, and the size is even and within the extents. Otherwise
 * `auto` falls back to readback and `gpu-required` fails with `GPU_OUTPUT_UNAVAILABLE`.
 */
export interface GpuOutputPathCapability {
  readonly available: boolean;
  /** Why the path is unavailable, or null when it is available. */
  readonly reason: string | null;
  /** The native API the encoder runs on: `vulkan` or `videotoolbox`. */
  readonly backend: string | null;
  readonly encoder: string | null;
  readonly videoCodec: string | null;
  /** The surface layout the encoder takes: `nv12`. */
  readonly inputLayout: string | null;
  /**
   * `gpu-copy` renders into separate planes and copies them on the GPU; `direct-target` renders
   * into the encoder's surface.
   */
  readonly inputHandoff: string | null;
  /** Whether the encoder takes GPU frames only, so that pinning it asks for this path. */
  readonly gpuInputOnly: boolean | null;
  /** Whether the encoder runs on the renderer's own device. */
  readonly sharesRendererDevice: boolean | null;
  /** Smallest output the device's encoder accepts, in pixels. */
  readonly minimumWidth: number | null;
  readonly minimumHeight: number | null;
  readonly maximumWidth: number | null;
  readonly maximumHeight: number | null;
  /** The output is aligned up to this before the extents are checked. */
  readonly widthAlignment: number | null;
  readonly heightAlignment: number | null;
  /**
   * H.264 encode reference limits. B pictures need a non-zero `maxBPictureL0References` or
   * `maxL1References`. Absent when unavailable; null when not reported (VideoToolbox).
   */
  readonly maxPPictureL0References?: number | null;
  readonly maxBPictureL0References?: number | null;
  readonly maxL1References?: number | null;
  /** Rate-control modes the device reports: `cbr`, `vbr`, `disabled` (constant QP). */
  readonly rateControlModes?: readonly ("cbr" | "vbr" | "disabled")[];
  /** Extra plane-texture pairs besides the encoder's surfaces; 0 for `direct-target`. */
  readonly planeRingSlots: number | null;
  /** Surfaces the encoder may still hold after release; counted in the in-flight budget. */
  readonly retainedSurfaces: number | null;
  readonly requiresEvenDimensions: boolean;
  /**
   * Every codec the path takes, with its limits; the fields above describe the first. Empty when
   * the path is unavailable.
   */
  readonly codecs?: readonly GpuOutputCodec[];
}

/** What the GPU-resident path's encoder takes for one codec. */
export interface GpuOutputCodec {
  readonly videoCodec: string;
  readonly encoder: string;
  readonly minimumWidth: number;
  readonly minimumHeight: number;
  readonly maximumWidth: number;
  readonly maximumHeight: number;
  readonly widthAlignment: number;
  readonly heightAlignment: number;
  readonly maxPPictureL0References: number | null;
  readonly maxBPictureL0References: number | null;
  readonly maxL1References: number | null;
  readonly rateControlModes: readonly string[];
}

/** One codec's hardware decode registration in the linked FFmpeg build. */
export interface DecodeCodecCapability {
  readonly id: string;
  readonly decoder: string | null;
  /**
   * Whether the decoder advertises this platform's hardware configuration. Actual hardware use is
   * still decided per stream.
   */
  readonly hardwareConfig: boolean;
  /**
   * Software decoder for the codec, or `null` when only hardware decodes it (AV1 without
   * `libdav1d`). Such sources are `unsupported` without hardware decode.
   */
  readonly softwareDecoder: string | null;
}

/** A container an export can write. */
export type ExportContainer = "mp4" | "mov" | "mkv" | "webm";

/**
 * Codecs a container allows, the same on every host. Whether this host can encode them is in
 * {@link HostEncodeCapabilities} `video` and `audio`.
 */
export interface ContainerCapability {
  readonly id: ExportContainer;
  /** Lowercase file extensions without the dot; the first is the usual one. */
  readonly extensions: readonly string[];
  readonly video: readonly string[];
  readonly audio: readonly string[];
  readonly defaultVideoCodec: string;
  readonly defaultAudioCodec: string;
}

/** What a native host encodes. */
export interface HostEncodeCapabilities {
  readonly containers: readonly ContainerCapability[];
  /** Encoders the host's software sinks can feed. */
  readonly video: readonly EncodeCapability[];
  readonly audio: readonly EncodeCapability[];
  readonly gpuOutputPath: GpuOutputPathCapability;
  /** Whether H.264, the default codec of MP4, MOV and MKV, can be exported, and how to fix it. */
  readonly h264: H264EncodeStatus;
  readonly openh264: OpenH264Capability;
}

/** How a native host decodes video. */
export interface HostDecodeCapabilities {
  readonly mode: VideoDecodeMode;
  readonly backend: "vulkan" | "videotoolbox" | "d3d12va" | "none";
  /** Whether a shared decode context was created for this host process. */
  readonly deviceReady: boolean;
  /** Why `auto` fell back to software decode at startup; `null` otherwise. */
  readonly hardwareUnavailable: string | null;
  /** Whether the device can present 10-bit decoded planes. */
  readonly tenBit: boolean;
  /**
   * Codecs the decode device handles in hardware; `null` without a device or where the backend
   * decides per stream (D3D12VA).
   */
  readonly deviceCodecs: readonly string[] | null;
  readonly codecs: readonly DecodeCodecCapability[];
  /**
   * Codecs whose alpha the host decodes: `prores` (4444), and `vp9` with `libvpx-vp9`. HEVC
   * auxiliary alpha decodes opaque.
   */
  readonly alpha: readonly string[];
}

/** What {@link Alpha2NodeClient.getCapabilities} reports about the host. */
export interface HostCapabilities {
  readonly status: "ready";
  /**
   * The renderer the host started with. `capabilities.features` also reports whether runtime WGSL
   * effects run (`effects.runtime`, not on the CPU).
   */
  readonly renderer: "gpu" | "cpu";
  /** As {@link NativeExportResult.rendererReason}. */
  readonly rendererReason: string | null;
  /** The CPU renderer's threads; absent on a GPU host. */
  readonly rendererThreads?: number;
  /**
   * Product version and build on macOS, distribution and kernel on Linux,
   * `major.minor.build.revision` on Windows.
   */
  readonly osVersion: string;
  /** What the host can do, in the report every alpha2 runtime gives. */
  readonly capabilities: CapabilityReport<
    HostDecodeCapabilities,
    HostEncodeCapabilities
  >;
  readonly effectCatalog: BuiltinEffectCatalog;
  readonly [diagnostic: string]: unknown;
}

/** What a host error carries besides its code and message. */
export interface Alpha2HostErrorOptions extends ErrorOptions {
  /** The host's failure report; without one, the code classifies it (`alpha2/export-failure`). */
  readonly failure?: ExportFailure;
  /** Export step for a failure classified here. Default `prepare`. */
  readonly phase?: ExportPhase;
  /** The last part of the host's standard error, at most {@link STDERR_TAIL_BYTES}. */
  readonly stderr?: string;
  /** How the host process ended, when it ended before answering. */
  readonly exitCode?: number | null;
  readonly signal?: string | null;
  /** As {@link Alpha2HostError.keptPath}. */
  readonly keptPath?: string;
  /** As {@link Alpha2HostError.path}. */
  readonly path?: string;
  /** As {@link Alpha2HostError.problem}. */
  readonly problem?: string;
}

/** Bytes of the host's standard error an error keeps. */
export const STDERR_TAIL_BYTES = 16_384;

/**
 * Base of every error this client rejects with. `failure` classifies it for a retry policy
 * (`alpha2/export-retry`): the host's report when sent, otherwise derived from `code` the same way
 * the host would.
 */
export class Alpha2HostError extends Error {
  /** A machine-readable code, such as `HOST_NOT_FOUND`, `INVALID_INPUT` or `DECODE_STALLED`. */
  readonly code: string;
  readonly failure: ExportFailure;
  readonly stderr?: string;
  readonly exitCode?: number | null;
  readonly signal?: string | null;
  /**
   * For `OUTPUT_PUBLISH_FAILED`: where the finished render was kept beside the output, if it was.
   * The caller owns the file.
   */
  readonly keptPath?: string;
  /** Where a compose refusal lies in the spec, template or data, such as `data.title`. */
  readonly path?: string;
  /** What is wrong where a compose refusal lies. */
  readonly problem?: string;

  constructor(code: string, message: string, options?: Alpha2HostErrorOptions) {
    super(message, options);
    this.name = "Alpha2HostError";
    this.code = code;
    this.failure =
      options?.failure ??
      failureForCode(code, {
        message,
        origin: "node",
        backend: "native",
        phase: options?.phase ?? "prepare",
        ...(options?.stderr === undefined ? {} : { stderr: options.stderr }),
        ...(options?.exitCode === undefined
          ? {}
          : { exitCode: options.exitCode }),
        ...(options?.signal === undefined ? {} : { signal: options.signal }),
      });
    if (options?.stderr !== undefined) this.stderr = options.stderr;
    if (options?.exitCode !== undefined) this.exitCode = options.exitCode;
    if (options?.signal !== undefined) this.signal = options.signal;
    if (options?.keptPath !== undefined) this.keptPath = options.keptPath;
    if (options?.path !== undefined) this.path = options.path;
    if (options?.problem !== undefined) this.problem = options.problem;
  }
}

/** An export refused before it rendered: the project's media is not ready. */
export class Alpha2ConformError extends Alpha2HostError {
  /** What each source would read and what blocks it. */
  readonly report: ConformReport;

  constructor(
    message: string,
    report: ConformReport,
    options?: Alpha2HostErrorOptions,
  ) {
    super("CONFORM_FAILED", message, options);
    this.name = "Alpha2ConformError";
    this.report = report;
  }
}

/** What {@link Alpha2NodeClient.getCapabilities} takes. */
export interface CapabilitiesOptions {
  /** Withdraws this call; see {@link Alpha2NodeClient.getCapabilities}. */
  readonly signal?: AbortSignal;
  /** The decode mode to report for, in place of the client's. */
  readonly videoDecode?: VideoDecodeMode;
  /** The renderer to report for, in place of the client's. */
  readonly renderer?: RendererMode;
}

interface CapabilityProbe {
  readonly result: Promise<HostCapabilities>;
  readonly controller: AbortController;
  waiting: number;
  settled: boolean;
}

/** What {@link Alpha2NodeClient.conform} asks. */
export interface ConformRequest {
  readonly project: Project;
  /** As {@link ExportProjectRequest.assets}, {@link ExportProjectRequest.proxies} and `frames`. */
  readonly assets?: Readonly<Record<string, string>>;
  readonly proxies?: Readonly<Record<string, readonly ProxyFile[]>>;
  readonly frames?: Readonly<Record<string, readonly (string | null)[]>>;
  /** Defaults to `export_original`. */
  readonly purpose?: MediaPurpose;
  readonly signal?: AbortSignal;
}

/** What the host found, and what the purpose would read. */
export interface ConformResult {
  readonly catalog: AssetCatalog;
  readonly report: ConformReport;
}

/** The host could not create its GPU device (`GPU_INIT_FAILED`); only with `renderer: "gpu"`. */
export class Alpha2GpuInitializationError extends Alpha2HostError {
  constructor(message: string, options?: Alpha2HostErrorOptions) {
    super("GPU_INIT_FAILED", message, options);
    this.name = "Alpha2GpuInitializationError";
  }
}

/** The project failed validation before a host started (`INVALID_INPUT`); `problems` lists why. */
export class Alpha2ProjectValidationError extends Alpha2HostError {
  readonly problems: readonly string[];

  constructor(problems: readonly string[]) {
    super("INVALID_INPUT", `invalid alpha2 project:\n${problems.join("\n")}`);
    this.name = "Alpha2ProjectValidationError";
    this.problems = problems;
  }
}

/** The request's `signal` aborted it (`ABORTED`). */
export class Alpha2AbortError extends Alpha2HostError {
  constructor() {
    super("ABORTED", "alpha2 export was aborted");
    this.name = "Alpha2AbortError";
  }
}

const HOST_RELEASES_URL = "https://github.com/alpha2-sh/alpha2/releases";

/**
 * Locates `alpha2-host` without loading Electron. The binary ships in the platform's native
 * archive (`bin/alpha2-host` beside its `lib/`), which `installHost` downloads into the cache.
 * Electron apps must keep `bin/` and `lib/` together and unpacked from ASAR.
 *
 * Order: `hostPath`, `ALPHA2_HOST_PATH` (empty means unset), Electron resources (`bin/`, then the
 * directory itself), this package's `bin/`, the cache for this package's version, then `PATH`.
 *
 * Throws {@link Alpha2HostError}: `INVALID_INPUT` for an empty `hostPath`, `HOST_NOT_FOUND` when
 * an explicit path is not a file or nothing is found.
 */
export function resolveHostBinary(
  options: ResolveHostBinaryOptions = {},
): string {
  const cwd = options.cwd ?? process.cwd();
  const executable = hostExecutableName();
  if (options.hostPath !== undefined && options.hostPath.trim() === "")
    throw new Alpha2HostError("INVALID_INPUT", "hostPath must not be empty");
  const fromEnvironment = process.env.ALPHA2_HOST_PATH;
  const explicit =
    options.hostPath ?? (fromEnvironment?.trim() ? fromEnvironment : undefined);
  if (explicit !== undefined) {
    const candidate = asarUnpacked(resolve(cwd, explicit));
    if (isFile(candidate)) return candidate;
    throw new Alpha2HostError(
      "HOST_NOT_FOUND",
      `alpha2-host does not exist: ${candidate}`,
    );
  }

  const processResources = (
    process as NodeJS.Process & { resourcesPath?: string }
  ).resourcesPath;
  const resourcesPath = options.resourcesPath ?? processResources;
  const moduleDirectory = dirname(fileURLToPath(import.meta.url));
  const candidates = [
    resourcesPath && join(resourcesPath, "bin", executable),
    resourcesPath && join(resourcesPath, executable),
    join(moduleDirectory, "..", "bin", executable),
  ];
  for (const raw of candidates) {
    if (!raw) continue;
    const candidate = asarUnpacked(raw);
    if (isFile(candidate)) return candidate;
  }
  const cached = cachedHostBinary(
    options.cacheDirectory === undefined
      ? hostCacheDirectory(cwd)
      : resolve(cwd, options.cacheDirectory),
  );
  if (cached !== undefined) return cached;
  for (const directory of (process.env.PATH ?? "").split(delimiter)) {
    if (!directory) continue;
    const candidate = join(directory, executable);
    if (isFile(candidate)) return candidate;
  }
  const version = packageVersion();
  throw new Alpha2HostError(
    "HOST_NOT_FOUND",
    `could not locate ${executable}; run \`npx @alpha2-sh/node install\` to download ` +
      `alpha2-host${version ? ` ${version}` : ""} for this platform into the cache, or download the ` +
      `native archive from ${HOST_RELEASES_URL} and set hostPath or ALPHA2_HOST_PATH to its ` +
      `bin/${executable}, or put that bin/ on PATH`,
  );
}

/** Frames of a project to render to PNG, each as an export renders it. */
export interface StillsRequest extends RenderInputs {
  /**
   * 1 to 256 frames with distinct outputs. Each time snaps to the frame-grid frame containing it.
   * Frames render in time order regardless of the order given.
   */
  readonly stills: readonly StillEntry[];
  /** Canvas size to render at instead of the project's, 1 to 16384 pixels each way. */
  readonly size?: { readonly width: number; readonly height: number };
  /**
   * Replace existing regular files at the outputs. Defaults to `false`. An output that is also an
   * input is always refused.
   */
  readonly overwrite?: boolean;
  /** Draw without the project background, over transparent pixels. Defaults to `false`. */
  readonly alpha?: boolean;
  /** Overrides the client's decode mode for this request. */
  readonly videoDecode?: VideoDecodeMode;
  /** Overrides the client's renderer for this request. */
  readonly renderer?: RendererMode;
  /** Cancels the request and removes its partial file. */
  readonly signal?: AbortSignal;
}

/** One frame of a {@link StillsRequest}. */
export interface StillEntry {
  /** A timeline time in ticks (120,000 a second). */
  readonly time: number;
  /** The PNG to write. */
  readonly output: string;
}

/** One frame of a {@link StillsRequest} at its time, to its PNG. */
export type StillRequest = Omit<StillsRequest, "stills"> & StillEntry;

/** One PNG a still request wrote. */
export interface StillFrame {
  /** The absolute path of the file written. */
  readonly output: string;
  /** Time of the grid frame drawn, in ticks. */
  readonly frameTime: number;
  readonly width: number;
  readonly height: number;
  /** How the PNG was moved to `output`. */
  readonly publish: PublishReport;
}

/** What a still request did. */
export interface StillResult {
  /** In the order they were asked for. */
  readonly stills: readonly StillFrame[];
  readonly renderer: "gpu" | "cpu";
  readonly renderMs: number;
  readonly timings: {
    readonly decodeMs: number;
    readonly renderMs: number;
    readonly writeMs: number;
  };
  readonly media: Readonly<Record<string, Selection>>;
  readonly decodeDiagnostics: readonly unknown[];
  /** As {@link NativeExportResult.unmatchedFontFamilies}. */
  readonly unmatchedFontFamilies: readonly string[];
  readonly [metadata: string]: unknown;
}

const MAX_STILLS = 256;
const MAX_STILL_DIMENSION = 16_384;

/**
 * Runs requests on `alpha2-host` sidecar processes that it keeps between requests, at most
 * `concurrency` of them, each running one request at a time. A host serves only requests that
 * would start a host with the same environment: decode mode, renderer and the rest of
 * `process.env` as it is when the request is made. A host that drains or exits is replaced by a
 * new one. Idle hosts do not keep this process alive. Every rejection is an {@link Alpha2HostError}.
 */
export class Alpha2NodeClient {
  readonly #pool: HostPool;
  readonly #videoDecode: VideoDecodeMode | undefined;
  readonly #renderer: RendererMode | undefined;
  /** Capability probes, running or answered, keyed by decode mode and renderer. */
  readonly #probes = new Map<string, CapabilityProbe>();

  /**
   * Resolves the host binary now. Throws `RangeError` for a bad `concurrency`, and
   * {@link Alpha2HostError} for invalid options (`INVALID_INPUT`) or a missing binary.
   */
  constructor(options: Alpha2NodeOptions = {}) {
    const concurrency = options.concurrency ?? 1;
    if (!Number.isSafeInteger(concurrency) || concurrency < 1) {
      throw new RangeError("concurrency must be a positive integer");
    }
    checkVideoDecode("videoDecode", options.videoDecode);
    checkRenderer("renderer", options.renderer);
    this.#pool = new HostPool(
      resolveHostBinary(options),
      concurrency,
      checkHostStartTimeout(options.hostStartTimeoutMs),
    );
    this.#videoDecode = options.videoDecode;
    this.#renderer = options.renderer;
  }

  /**
   * What the host can decode, render and encode for the given (or the client's) modes. The first
   * call per host environment (the modes over `process.env`) asks a host; concurrent calls share
   * the probe and the report is cached for later calls. A failed probe is not cached.
   *
   * `options.signal` rejects this call with `Alpha2AbortError`; the probe is cancelled only when no
   * other call is waiting for it.
   */
  async getCapabilities(
    options: CapabilitiesOptions = {},
  ): Promise<HostCapabilities> {
    checkVideoDecode("videoDecode", options.videoDecode);
    checkRenderer("renderer", options.renderer);
    const { signal } = options;
    if (signal?.aborted) throw new Alpha2AbortError();
    const videoDecode = options.videoDecode ?? this.#videoDecode;
    const renderer = options.renderer ?? this.#renderer;
    const key = environmentKey(hostEnvironment({ videoDecode, renderer }));
    let probe = this.#probes.get(key);
    if (probe === undefined) {
      const controller = new AbortController();
      const result = this.#request(
        "capabilities",
        {},
        { videoDecode, renderer },
        controller.signal,
      ) as Promise<HostCapabilities>;
      const started: CapabilityProbe = {
        result,
        controller,
        waiting: 0,
        settled: false,
      };
      probe = started;
      this.#probes.set(key, started);
      result.then(
        () => {
          started.settled = true;
        },
        () => {
          started.settled = true;
          if (this.#probes.get(key) === started) this.#probes.delete(key);
        },
      );
    }
    if (probe.settled) return probe.result;
    // Calls without a signal count too: they keep the probe alive.
    const waited = probe;
    waited.waiting += 1;
    return new Promise((resolveCall, rejectCall) => {
      const leave = (): void => {
        signal?.removeEventListener("abort", withdraw);
        waited.waiting -= 1;
      };
      const withdraw = (): void => {
        leave();
        if (waited.waiting === 0 && !waited.settled) {
          if (this.#probes.get(key) === waited) this.#probes.delete(key);
          waited.controller.abort();
        }
        rejectCall(new Alpha2AbortError());
      };
      signal?.addEventListener("abort", withdraw, { once: true });
      waited.result.then(
        (report) => {
          leave();
          resolveCall(report);
        },
        (error: Error) => {
          leave();
          rejectCall(error);
        },
      );
    });
  }

  /**
   * Which media a purpose would read for each source, and what blocks it, without rendering.
   * Originals are hashed only when the project recorded a hash. Rejects with
   * {@link Alpha2ProjectValidationError} for an invalid project.
   */
  async conform(request: ConformRequest): Promise<ConformResult> {
    const problems = projectProblems(request.project);
    if (problems.length > 0) throw new Alpha2ProjectValidationError(problems);
    validateMediaPaths(request, false);
    return (await this.#request(
      "conform",
      {
        project: request.project,
        assets: request.assets ?? {},
        proxies: proxiesPayload(request.proxies),
        frames: request.frames ?? {},
        purpose: (request.purpose ?? "export_original").replace(/_/g, "-"),
      },
      { videoDecode: this.#videoDecode, renderer: this.#renderer },
      request.signal,
    )) as ConformResult;
  }

  /**
   * Compiles a compose spec or binds, lints or describes a template without a GPU, with the
   * functions `alpha2/compose` and Python's `alpha2.compose` use. `compile` takes
   * `{spec, facts?, fonts?, templates?}`, `bind` `{template, data?, theme?, templates?}`,
   * `dataSchema` `{template}`, `lint` `{template, data?, templates?}`, `requirements` `{spec}`
   * and `templates` nothing. Rejects with {@link Alpha2HostError} `INVALID_INPUT`, whose `path`
   * and `problem` name where a spec, template or data fails.
   */
  async compose(
    action: ComposeAction,
    fields: Readonly<Record<string, unknown>> = {},
    signal?: AbortSignal,
  ): Promise<Record<string, unknown>> {
    return (await this.#request(
      "compose",
      { ...fields, action },
      { videoDecode: this.#videoDecode, renderer: this.#renderer },
      signal,
    )) as Record<string, unknown>;
  }

  /**
   * Renders and encodes `request.project` to `request.output` and resolves with that path.
   *
   * Rejects with {@link Alpha2ProjectValidationError} or `INVALID_INPUT` for an invalid request,
   * {@link Alpha2ConformError} when media is not ready, {@link Alpha2AbortError} on abort, and
   * {@link Alpha2HostError} otherwise. Under `auto` decode, a hardware decoder stall
   * (`DECODE_STALLED`) retries once in software with progress restarting; if that fails too, the
   * stall is its `cause` and `failure.diagnostics.decodeStall`.
   */
  async exportProject(request: ExportProjectRequest): Promise<string> {
    const problems = projectProblems(request.project);
    if (problems.length > 0) throw new Alpha2ProjectValidationError(problems);
    validateExportRequest(request);
    const videoDecode = request.videoDecode ?? this.#videoDecode;
    const renderer = request.renderer ?? this.#renderer;
    const run = (mode: VideoDecodeMode | undefined) =>
      this.#request(
        "export",
        {
          ...renderInputsPayload(request),
          startTicks: request.startTicks,
          endTicks: request.endTicks,
          frameRate: request.frameRate,
          includeAudio: request.includeAudio ?? true,
          // Omitted fields are resolved by the host from the container.
          ...(request.container === undefined
            ? {}
            : { container: request.container }),
          ...(request.videoCodec === undefined
            ? {}
            : { videoCodec: request.videoCodec }),
          ...(request.alpha === true ? { alpha: true } : {}),
          ...(request.bitDepth === undefined
            ? {}
            : { bitDepth: request.bitDepth }),
          videoEncoder: request.videoEncoder,
          inputPolicy: request.inputPolicy ?? "auto",
          strategy: request.strategy ?? "render",
          videoQuality: request.videoQuality,
          ...(request.audioCodec === undefined
            ? {}
            : { audioCodec: request.audioCodec }),
          audioQuality: request.audioQuality,
          loudness: request.loudness,
          renderCache: request.renderCache,
          ...(request.memory === undefined ? {} : { memory: request.memory }),
          output: request.output,
          overwrite: request.overwrite ?? false,
        },
        { videoDecode: mode, renderer },
        request.signal,
        request.onProgress,
      );
    let result: NativeExportResult;
    try {
      result = (await run(videoDecode)) as NativeExportResult;
    } catch (error) {
      // A decoder stuck in the driver cannot be recovered in-process, so rerun on a software
      // decode host. Software stalls would recur and `hardware-required` forbids the fallback.
      if (
        !(error instanceof Alpha2HostError) ||
        error.code !== "DECODE_STALLED" ||
        error.failure?.resource !== "gpu" ||
        (videoDecode ?? process.env.ALPHA2_VIDEO_DECODE) === "hardware-required"
      )
        throw error;
      try {
        result = (await run("software")) as NativeExportResult;
      } catch (retryError) {
        throw recordDecodeStall(retryError, error);
      }
    }
    await request.onResult?.(result);
    return request.output;
  }

  /**
   * Renders frames to 8-bit sRGB RGBA PNG files, identical to what an export draws before
   * encoding. One host serves all frames.
   *
   * Rejects as {@link exportProject} does, without the decode retry.
   */
  async stills(request: StillsRequest): Promise<StillResult> {
    const problems = projectProblems(request.project);
    if (problems.length > 0) throw new Alpha2ProjectValidationError(problems);
    validateStillsRequest(request);
    return (await this.#request(
      "still",
      {
        ...renderInputsPayload(request),
        stills: request.stills.map(({ time, output }) => ({ time, output })),
        overwrite: request.overwrite ?? false,
        ...(request.alpha === true ? { alpha: true } : {}),
        ...(request.size === undefined
          ? {}
          : {
              size: {
                width: request.size.width,
                height: request.size.height,
              },
            }),
      },
      {
        videoDecode: request.videoDecode ?? this.#videoDecode,
        renderer: request.renderer ?? this.#renderer,
      },
      request.signal,
    )) as StillResult;
  }

  /** Renders one frame; {@link stills} with a single entry. */
  async still(request: StillRequest): Promise<StillResult> {
    const { time, output, ...rest } = request;
    return this.stills({ ...rest, stills: [{ time, output }] });
  }

  /**
   * Ends every host this client has started: each finishes the request it runs, then its standard
   * input closes. Resolves once they have exited. Later requests start new hosts.
   */
  async close(): Promise<void> {
    await this.#pool.close();
  }

  async #request(
    operation: "capabilities" | "export" | "conform" | "still" | "compose",
    payload: Readonly<Record<string, unknown>>,
    modes: HostModes,
    signal?: AbortSignal,
    onProgress?: ExportProgressCallback,
  ): Promise<unknown> {
    if (signal?.aborted) throw new Alpha2AbortError();
    const requestId = `node-${process.pid}-${Date.now()}-${nextRequestSequence++}`;
    // Serialize before a host is taken so unserializable values (BigInt, cycles) start nothing.
    let frame: Frame;
    try {
      frame = encodeMessage(createRequest(operation, requestId, payload));
    } catch (cause) {
      throw new Alpha2HostError(
        "INVALID_INPUT",
        `the ${operation} request cannot be written as JSON: ${cause instanceof Error ? cause.message : String(cause)}`,
        { cause },
      );
    }
    // The host would create its GPU device before refusing an oversized request.
    if (frame.bodyBytes > MAX_REQUEST_LINE_BYTES)
      throw new Alpha2HostError(
        "INVALID_INPUT",
        `the ${operation} request is ${frame.bodyBytes} bytes of JSON; the host reads at most ${MAX_REQUEST_LINE_BYTES}`,
      );
    return this.#pool.request(frame, requestId, modes, signal, onProgress);
  }
}

/**
 * Attaches the hardware stall to the software retry's error, as `cause` and as
 * `failure.diagnostics.decodeStall` (which survives serialization). The retry's failure class is
 * kept so retry policies judge the last run.
 */
function recordDecodeStall(
  retryError: unknown,
  stall: Alpha2HostError,
): unknown {
  if (!(retryError instanceof Alpha2HostError)) return retryError;
  if (retryError.cause === undefined)
    Object.defineProperty(retryError, "cause", {
      value: stall,
      writable: true,
      configurable: true,
    });
  (retryError as { failure: ExportFailure }).failure = {
    ...retryError.failure,
    diagnostics: {
      ...retryError.failure.diagnostics,
      decodeStall: {
        hostCode: stall.code,
        code: stall.failure.code,
        phase: stall.failure.phase,
        message: stall.message,
        ...(stall.stderr === undefined ? {} : { stderr: stall.stderr }),
      },
    },
  };
  return retryError;
}

/** Shape and serialized size (`MAX_PROJECT_JSON_BYTES`) problems the engine would refuse. */
function projectProblems(project: Project): string[] {
  // `undefined` does not serialize; let the object validator report it.
  const json = JSON.stringify(project) as string | undefined;
  return json === undefined
    ? validateProject(project)
    : validateProjectJson(json);
}

const convenienceClients = new Map<string, Alpha2NodeClient>();

/**
 * {@link Alpha2NodeClient.getCapabilities} on a client shared by module-level calls with the same
 * `concurrency` and resolved binary.
 */
export async function getCapabilities(
  options: Alpha2NodeOptions & CapabilitiesOptions = {},
): Promise<HostCapabilities> {
  return sharedClient(options).getCapabilities({
    ...(options.signal === undefined ? {} : { signal: options.signal }),
    ...(options.videoDecode === undefined
      ? {}
      : { videoDecode: options.videoDecode }),
    ...(options.renderer === undefined ? {} : { renderer: options.renderer }),
  });
}

/**
 * {@link Alpha2NodeClient.exportProject} on a client shared by module-level calls with the same
 * `concurrency` and resolved binary, so `concurrency` holds across calls. `options.videoDecode`
 * and `options.renderer` apply where the request names none.
 */
export async function exportProject(
  request: ExportProjectRequest,
  options: Alpha2NodeOptions = {},
): Promise<string> {
  return sharedClient(options).exportProject({
    ...request,
    ...(request.videoDecode === undefined && options.videoDecode !== undefined
      ? { videoDecode: options.videoDecode }
      : {}),
    ...(request.renderer === undefined && options.renderer !== undefined
      ? { renderer: options.renderer }
      : {}),
  });
}

/** {@link Alpha2NodeClient.stills} on the shared client. */
export async function stills(
  request: StillsRequest,
  options: Alpha2NodeOptions = {},
): Promise<StillResult> {
  return sharedClient(options).stills({
    ...request,
    ...(request.videoDecode === undefined && options.videoDecode !== undefined
      ? { videoDecode: options.videoDecode }
      : {}),
    ...(request.renderer === undefined && options.renderer !== undefined
      ? { renderer: options.renderer }
      : {}),
  });
}

/** {@link Alpha2NodeClient.still} on the shared client. */
export async function still(
  request: StillRequest,
  options: Alpha2NodeOptions = {},
): Promise<StillResult> {
  const { time, output, ...rest } = request;
  return stills({ ...rest, stills: [{ time, output }] }, options);
}

/** Keyed without decode mode and renderer, which travel per request, so calls share a bound. */
function sharedClient(options: Alpha2NodeOptions): Alpha2NodeClient {
  const concurrency = options.concurrency ?? 1;
  if (!Number.isSafeInteger(concurrency) || concurrency < 1) {
    throw new RangeError("concurrency must be a positive integer");
  }
  checkVideoDecode("videoDecode", options.videoDecode);
  checkRenderer("renderer", options.renderer);
  const hostStartTimeoutMs = checkHostStartTimeout(options.hostStartTimeoutMs);
  const binary = resolveHostBinary(options);
  const key = `${concurrency}\0${hostStartTimeoutMs}\0${binary}`;
  let client = convenienceClients.get(key);
  if (client === undefined) {
    client = new Alpha2NodeClient({
      hostPath: binary,
      concurrency,
      hostStartTimeoutMs,
    });
    convenienceClients.set(key, client);
  }
  return client;
}

function checkHostStartTimeout(value: unknown): number {
  const timeout = hostStartTimeout(value);
  if ("invalid" in timeout)
    throw new Alpha2HostError("INVALID_INPUT", timeout.invalid);
  return timeout.ms;
}

type RenderInputs = Pick<
  ExportProjectRequest,
  | "project"
  | "assets"
  | "proxies"
  | "frames"
  | "resources"
  | "media"
  | "effectAssets"
  | "fonts"
  | "effects"
>;

/** Shape checks only; media usability is left to the host's conform check. */
function validateRenderInputs(request: RenderInputs): void {
  validateMediaPaths(request, true);
  checkPathRecord("effectAssets", request.effectAssets, "effect asset path");
  if (request.resources !== undefined) {
    if (!isRecord(request.resources))
      invalidShape("resources must map source ids to their files");
    for (const [id, files] of Object.entries(request.resources))
      checkPathRecord(`resources.${id}`, files, "resource path");
  }
  if (request.effects !== undefined && !Array.isArray(request.effects))
    invalidShape("effects must be a list of effect definitions");
  if (
    request.media !== undefined &&
    !["original", "proxies"].includes(request.media)
  ) {
    throw new Alpha2HostError(
      "INVALID_INPUT",
      'media must be "original" or "proxies"',
    );
  }
  if (
    request.fonts !== undefined &&
    (!Array.isArray(request.fonts) || !request.fonts.every(nonEmptyString))
  )
    invalidShape("fonts must be a list of font file paths");
  for (const resource of request.project.effect_resources ?? []) {
    const path = request.effectAssets?.[resource.id];
    if (typeof path !== "string" || path.length === 0) {
      throw new Alpha2HostError(
        "MISSING_INPUT",
        `missing effect asset path for resource ${resource.id}`,
      );
    }
  }
}

function renderInputsPayload(request: RenderInputs): Record<string, unknown> {
  return {
    project: request.project,
    assets: request.assets,
    proxies: proxiesPayload(request.proxies),
    frames: request.frames ?? {},
    resources: request.resources ?? {},
    media: request.media ?? "original",
    effectAssets: request.effectAssets ?? {},
    fonts: request.fonts ?? [],
    effects: request.effects ?? [],
  };
}

function validateStillsRequest(request: StillsRequest): void {
  validateRenderInputs(request);
  if (
    !Array.isArray(request.stills) ||
    request.stills.length === 0 ||
    request.stills.length > MAX_STILLS
  )
    throw new Alpha2HostError(
      "INVALID_INPUT",
      `stills must list 1 to ${MAX_STILLS} frames`,
    );
  const outputs = new Map<string, string>();
  for (const still of request.stills) {
    if (
      !isRecord(still) ||
      !Number.isSafeInteger(still.time) ||
      (still.time as number) < 0
    )
      throw new Alpha2HostError(
        "INVALID_INPUT",
        "a still's time is a non-negative integer tick count",
      );
    if (!nonEmptyString(still.output))
      throw new Alpha2HostError(
        "INVALID_INPUT",
        "a still's output must not be empty",
      );
    // Case- and normalization-insensitive file systems (Windows, macOS) treat these as one file.
    const key = resolve(still.output).normalize("NFC").toLowerCase();
    const earlier = outputs.get(key);
    if (earlier !== undefined)
      throw new Alpha2HostError(
        "INVALID_INPUT",
        `two stills write one file: ${earlier} and ${still.output}`,
      );
    outputs.set(key, still.output);
  }
  const fits = (side: unknown): boolean =>
    Number.isSafeInteger(side) &&
    (side as number) >= 1 &&
    (side as number) <= MAX_STILL_DIMENSION;
  if (
    request.size !== undefined &&
    (!isRecord(request.size) ||
      !fits(request.size.width) ||
      !fits(request.size.height))
  )
    throw new Alpha2HostError(
      "INVALID_INPUT",
      `size must be { width, height }, each 1 to ${MAX_STILL_DIMENSION} pixels`,
    );
  checkOverwrite(request.overwrite);
  checkVideoDecode("videoDecode", request.videoDecode);
  checkRenderer("renderer", request.renderer);
}

function checkOverwrite(overwrite: unknown): void {
  if (overwrite !== undefined && typeof overwrite !== "boolean")
    throw new Alpha2HostError(
      "INVALID_INPUT",
      "overwrite must be true or false",
    );
}

function validateExportRequest(request: ExportProjectRequest): void {
  const integer = (value: number): boolean => Number.isSafeInteger(value);
  if (
    !integer(request.startTicks) ||
    !integer(request.endTicks) ||
    request.endTicks <= request.startTicks
  ) {
    throw new Alpha2HostError(
      "INVALID_INPUT",
      "export range must be a non-empty integer tick range",
    );
  }
  if (
    !Number.isSafeInteger(request.frameRate.num) ||
    request.frameRate.num <= 0 ||
    !Number.isSafeInteger(request.frameRate.den) ||
    request.frameRate.den <= 0
  ) {
    throw new Alpha2HostError(
      "INVALID_INPUT",
      "frameRate must be a positive rational",
    );
  }
  for (const [field, value] of Object.entries(request.memory ?? {})) {
    if (field === "scale") {
      if (
        value !== undefined &&
        (typeof value !== "number" || !(value > 0 && value <= 1))
      )
        throw new Alpha2HostError(
          "INVALID_INPUT",
          "memory.scale must be above 0 and at most 1",
        );
      continue;
    }
    if (
      ![
        "inFlightBytes",
        "decodeQueueBytes",
        "reverseCacheBytes",
        "audioWindowBytes",
        "providerSourceBytes",
        "decoderSessionBytes",
      ].includes(field)
    )
      throw new Alpha2HostError(
        "INVALID_INPUT",
        `memory has an unknown field "${field}"`,
      );
    if (value !== undefined && (!Number.isSafeInteger(value) || value <= 0))
      throw new Alpha2HostError(
        "INVALID_INPUT",
        `memory.${field} must be a positive byte count`,
      );
  }
  validateRenderInputs(request);
  if (typeof request.output !== "string" || request.output.length === 0) {
    throw new Alpha2HostError("INVALID_INPUT", "output must not be empty");
  }
  checkOverwrite(request.overwrite);
  checkVideoDecode("videoDecode", request.videoDecode);
  checkRenderer("renderer", request.renderer);
  // Apply the engine's container policy here so a bad format never starts a host.
  for (const field of ["container", "videoCodec", "audioCodec"] as const) {
    const value = request[field];
    if (
      value !== undefined &&
      (typeof value !== "string" || value.length === 0)
    )
      throw new Alpha2HostError(
        "INVALID_INPUT",
        `${field} must be a non-empty string`,
      );
  }
  if (request.alpha !== undefined && typeof request.alpha !== "boolean")
    throw new Alpha2HostError("INVALID_INPUT", "alpha must be true or false");
  try {
    const format = resolveEngineExportFormat({
      extension: extname(request.output).slice(1),
      ...(request.container === undefined
        ? {}
        : { container: request.container }),
      ...(request.videoCodec === undefined
        ? {}
        : { videoCodec: request.videoCodec }),
      ...(request.audioCodec === undefined
        ? {}
        : { audioCodec: request.audioCodec }),
    });
    resolvePixels(format.videoCodec, format.container, {
      ...(request.bitDepth === undefined ? {} : { bitDepth: request.bitDepth }),
      alpha: request.alpha === true,
    });
  } catch (error) {
    if (error instanceof ExportFormatError)
      throw new Alpha2HostError("INVALID_INPUT", error.message);
    throw error;
  }
  if (
    request.videoEncoder !== undefined &&
    (typeof request.videoEncoder !== "string" ||
      request.videoEncoder.length === 0)
  ) {
    throw new Alpha2HostError(
      "INVALID_INPUT",
      "videoEncoder must be a non-empty FFmpeg encoder name",
    );
  }
  if (
    request.strategy !== undefined &&
    !["render", "prefer-copy", "require-copy"].includes(request.strategy)
  ) {
    throw new Alpha2HostError(
      "INVALID_INPUT",
      "strategy must be render, prefer-copy, or require-copy",
    );
  }
  if (
    request.inputPolicy !== undefined &&
    !["auto", "gpu-required", "cpu-readback"].includes(request.inputPolicy)
  ) {
    throw new Alpha2HostError(
      "INVALID_INPUT",
      "inputPolicy must be auto, gpu-required, or cpu-readback",
    );
  }
  validateQuality("videoQuality", request.videoQuality, [
    "bitrate",
    "mode",
    "quantizer",
  ]);
  validateQuality("audioQuality", request.audioQuality, ["bitrate"]);
  validateLoudness(request);
  const cache = request.renderCache;
  if (
    cache !== undefined &&
    (typeof cache.directory !== "string" ||
      cache.directory.length === 0 ||
      (cache.quotaBytes !== undefined &&
        (!Number.isSafeInteger(cache.quotaBytes) || cache.quotaBytes <= 0)) ||
      (cache.backlog !== undefined &&
        cache.backlog !== "skip" &&
        cache.backlog !== "wait"))
  )
    throw new Alpha2HostError(
      "INVALID_INPUT",
      'renderCache takes a non-empty directory, an optional positive integer quotaBytes, and an optional backlog "skip" or "wait"',
    );
}

function validateMediaPaths(
  request: Pick<ConformRequest, "assets" | "proxies" | "frames">,
  assetsRequired: boolean,
): void {
  if (assetsRequired && request.assets === undefined)
    invalidShape("assets must map source ids to paths");
  checkPathRecord("assets", request.assets, "asset path");
  if (request.proxies !== undefined) {
    if (!isRecord(request.proxies))
      invalidShape("proxies must map source ids to lists of proxy files");
    for (const [id, files] of Object.entries(request.proxies))
      if (
        !Array.isArray(files) ||
        !files.every((file) => isRecord(file) && nonEmptyString(file.path))
      )
        invalidShape(
          `proxies for source ${id} must be a list of { path } proxy files`,
        );
  }
  if (request.frames !== undefined) {
    if (!isRecord(request.frames))
      invalidShape("frames must map source ids to lists of frame files");
    for (const [id, files] of Object.entries(request.frames))
      if (
        !Array.isArray(files) ||
        !files.every((file) => file === null || nonEmptyString(file))
      )
        invalidShape(
          `frames for source ${id} must be a list of frame file paths, null for a gap`,
        );
  }
}

function checkPathRecord(field: string, value: unknown, what: string): void {
  if (value === undefined) return;
  if (!isRecord(value)) invalidShape(`${field} must map ids to paths`);
  for (const [id, path] of Object.entries(value))
    if (!nonEmptyString(path))
      invalidShape(`${what} for ${id} must be a non-empty string`);
}

function invalidShape(message: string): never {
  throw new Alpha2HostError("INVALID_INPUT", message);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function nonEmptyString(value: unknown): value is string {
  return typeof value === "string" && value.length > 0;
}

/** Mirrors the host's `loudness` checks and messages. */
function validateLoudness(request: ExportProjectRequest): void {
  const loudness = request.loudness;
  if (loudness === undefined) return;
  const invalid = (message: string): never => {
    throw new Alpha2HostError("INVALID_INPUT", message);
  };
  if (typeof loudness !== "object" || loudness === null)
    invalid("loudness must be an object { targetLufs, truePeakCeilingDbtp }");
  const unknown = Object.keys(loudness).find(
    (key) => key !== "targetLufs" && key !== "truePeakCeilingDbtp",
  );
  if (unknown !== undefined)
    invalid(
      `loudness has an unknown key "${unknown}"; it takes targetLufs and truePeakCeilingDbtp`,
    );
  const within = (
    key: "targetLufs" | "truePeakCeilingDbtp",
    low: number,
  ): void => {
    const value = loudness[key];
    if (typeof value !== "number" || !(value >= low && value <= 0))
      invalid(`loudness.${key} must be within ${low}..=0, received ${value}`);
  };
  within("targetLufs", -60);
  within("truePeakCeilingDbtp", -20);
  if (request.includeAudio === false)
    invalid(
      "loudness normalizes the export's audio and needs includeAudio: true",
    );
}

/** Mirrors the host's checks so the error comes before a process starts. */
function validateQuality(
  field: "videoQuality" | "audioQuality",
  quality: unknown,
  keys: readonly string[],
): void {
  if (quality === undefined) return;
  const invalid = (message: string): never => {
    throw new Alpha2HostError("INVALID_INPUT", `${field}${message}`);
  };
  if (typeof quality === "string") {
    if (!(QUALITY_LEVELS as readonly string[]).includes(quality))
      invalid(
        ` must be one of ${QUALITY_LEVELS.join(", ")}, received "${quality}"`,
      );
    return;
  }
  if (typeof quality !== "object" || quality === null || Array.isArray(quality))
    invalid(" must be a quality level name or an explicit rate object");
  const rate = quality as Record<string, unknown>;
  const unknown = Object.keys(rate).find((key) => !keys.includes(key));
  if (unknown !== undefined)
    invalid(
      ` has an unknown key "${unknown}"; an explicit rate takes ${keys.join(" and ")}`,
    );
  if (
    typeof rate.bitrate !== "number" ||
    !Number.isSafeInteger(rate.bitrate) ||
    rate.bitrate <= 0
  )
    invalid(".bitrate must be a positive integer number of bits per second");
  if (
    rate.mode !== undefined &&
    rate.mode !== "constant" &&
    rate.mode !== "variable"
  )
    invalid('.mode must be "constant" or "variable"');
  if (
    rate.quantizer !== undefined &&
    (typeof rate.quantizer !== "number" ||
      !Number.isInteger(rate.quantizer) ||
      rate.quantizer < 0 ||
      rate.quantizer > 255)
  )
    invalid(".quantizer must be an integer from 0 to 255");
}

/** What a {@link Alpha2NodeClient.compose} request does. */
export type ComposeAction =
  "compile" | "bind" | "dataSchema" | "lint" | "requirements" | "templates";

/** Modes a request sets over this process's environment. */
interface HostModes {
  readonly videoDecode?: VideoDecodeMode | undefined;
  readonly renderer?: RendererMode | undefined;
}

/**
 * The environment a host would start with now. The host reads its modes and every `ALPHA2_*`
 * setting at startup, so the modes go in the environment.
 */
function hostEnvironment(modes: HostModes): NodeJS.ProcessEnv {
  return {
    ...process.env,
    ...(modes.videoDecode === undefined
      ? {}
      : { ALPHA2_VIDEO_DECODE: modes.videoDecode }),
    ...(modes.renderer === undefined
      ? {}
      : { ALPHA2_RENDERER: modes.renderer }),
  };
}

/** Hosts started with one environment are interchangeable, so the whole of it is their key. */
function environmentKey(environment: NodeJS.ProcessEnv): string {
  const entries = Object.entries(environment)
    .filter(([, value]) => value !== undefined)
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  return createHash("sha256").update(JSON.stringify(entries)).digest("hex");
}

/** Times a request answered `HOST_RETIRING` is sent to another host before it fails. */
const MAX_RETIRING_RESENDS = 3;

/** How long a failed answer waits for stderr, which arrives on its own pipe. */
const STDERR_SETTLE_MS = 50;

interface HostWaiter {
  readonly key: string;
  readonly environment: NodeJS.ProcessEnv;
  /** Runs the request on the host at once, so nothing the host writes comes in between. */
  readonly grant: (host: Host) => void;
  readonly refuse: (error: Error) => void;
  /** The idle host of other modes ended to make room for this request. */
  evicted?: Host;
}

type RunOutcome =
  | { readonly kind: "result"; readonly result: unknown }
  | { readonly kind: "unsent" }
  | { readonly kind: "retiring"; readonly error: Alpha2HostError };

/** Builds a request's error once the host's stderr for it is complete. */
type FailureBuilder = (stderr: string, phase: ExportPhase) => Error;

interface ActiveRun {
  readonly progress: (message: ProgressMessage) => void;
  readonly answer: (message: ResponseMessage) => void;
  /** The host is ending; the request settles with this error when it exits. */
  readonly fail: (error: FailureBuilder) => void;
  readonly exited: (code: number | null, signal: string | null) => void;
  readonly phase: () => ExportPhase;
}

/**
 * The hosts of one client, kept between requests and sent one request at a time, as the host
 * protocol's "Keeping hosts" describes.
 */
class HostPool {
  readonly binary: string;
  readonly startTimeoutMs: number;
  readonly #limit: number;
  /** Every host until its process exits, which is what the limit counts. */
  readonly #hosts = new Set<Host>();
  readonly #waiting: HostWaiter[] = [];

  constructor(binary: string, limit: number, startTimeoutMs: number) {
    this.binary = binary;
    this.#limit = limit;
    this.startTimeoutMs = startTimeoutMs;
  }

  async request(
    frame: Frame,
    requestId: string,
    modes: HostModes,
    signal?: AbortSignal,
    onProgress?: ExportProgressCallback,
  ): Promise<unknown> {
    for (let resends = 0; ;) {
      const outcome = await this.#acquire(modes, signal, (host) =>
        host.run(frame, requestId, signal, onProgress),
      );
      if (outcome.kind === "result") return outcome.result;
      if (outcome.kind === "retiring") {
        if (resends === MAX_RETIRING_RESENDS) throw outcome.error;
        resends += 1;
      }
    }
  }

  async close(): Promise<void> {
    const hosts = [...this.#hosts];
    for (const host of hosts) host.retire();
    await Promise.all(hosts.map((host) => host.exited));
  }

  /** Gives each waiting request an idle host of its modes, a new host, or room for one. */
  dispatch(): void {
    for (const waiter of [...this.#waiting]) {
      const host = [...this.#hosts].find(
        (candidate) => candidate.idle && candidate.key === waiter.key,
      );
      if (host === undefined || !this.#dequeue(waiter)) continue;
      host.lease();
      waiter.grant(host);
    }
    for (const waiter of [...this.#waiting]) {
      if (!this.#waiting.includes(waiter)) continue;
      if (this.#hosts.size < this.#limit) {
        this.#dequeue(waiter);
        const host = new Host(this, waiter);
        this.#hosts.add(host);
        void host.start();
        continue;
      }
      if (waiter.evicted !== undefined && this.#hosts.has(waiter.evicted))
        continue;
      // Idle hosts left after the first pass serve no waiting request.
      const idle = [...this.#hosts].find((candidate) => candidate.idle);
      if (idle === undefined) return;
      waiter.evicted = idle;
      idle.retire();
    }
  }

  removed(host: Host): void {
    this.#hosts.delete(host);
    this.dispatch();
  }

  /** Puts back a request whose host started draining before it was ready. */
  requeue(waiter: HostWaiter): void {
    this.#waiting.unshift(waiter);
    this.dispatch();
  }

  #acquire(
    modes: HostModes,
    signal: AbortSignal | undefined,
    run: (host: Host) => Promise<RunOutcome>,
  ): Promise<RunOutcome> {
    if (signal?.aborted) return Promise.reject(new Alpha2AbortError());
    return new Promise((resolveRun, rejectRun) => {
      let settled = false;
      const leave = (): boolean => {
        if (settled) return false;
        settled = true;
        signal?.removeEventListener("abort", abort);
        return true;
      };
      const environment = hostEnvironment(modes);
      const waiter: HostWaiter = {
        key: environmentKey(environment),
        environment,
        grant: (host) => {
          if (leave()) resolveRun(run(host));
          else host.release();
        },
        refuse: (error) => {
          if (leave()) rejectRun(error);
        },
      };
      const abort = (): void => {
        this.#dequeue(waiter);
        for (const host of this.#hosts) host.withdraw(waiter);
        waiter.refuse(new Alpha2AbortError());
      };
      signal?.addEventListener("abort", abort, { once: true });
      this.#waiting.push(waiter);
      this.dispatch();
    });
  }

  #dequeue(waiter: HostWaiter): boolean {
    const index = this.#waiting.indexOf(waiter);
    if (index < 0) return false;
    this.#waiting.splice(index, 1);
    return true;
  }
}

/** One `alpha2-host serve` process. */
class Host {
  readonly key: string;
  /** Resolves once the process has exited, or failed to start. */
  readonly exited: Promise<void>;
  readonly #pool: HostPool;
  readonly #environment: NodeJS.ProcessEnv;
  /** The request that started this host, until it is ready. */
  #waiter: HostWaiter | undefined;
  #markExited: () => void = () => {};
  #child: ChildProcess | undefined;
  #wire: HostWire | undefined;
  #ready = false;
  /** Sent no more requests: draining, failed, ending or exited. */
  #retired = false;
  #leased = false;
  #inputClosed = false;
  #gone = false;
  #held = true;
  #run: ActiveRun | undefined;
  #startError: FailureBuilder | undefined;
  #startTimedOut = false;
  #startTimer: NodeJS.Timeout | undefined;
  #terminateTimer: NodeJS.Timeout | undefined;
  #killTimer: NodeJS.Timeout | undefined;
  #oomKillsAtStart: number | undefined;
  /** The last bytes of stderr, and how many were written in all. */
  #stderr = Buffer.alloc(0);
  #stderrTotal = 0;

  constructor(pool: HostPool, waiter: HostWaiter) {
    this.#pool = pool;
    this.#waiter = waiter;
    this.key = waiter.key;
    this.#environment = waiter.environment;
    this.exited = new Promise((resolveExit) => {
      this.#markExited = resolveExit;
    });
  }

  get idle(): boolean {
    return this.#ready && !this.#leased && !this.#retired;
  }

  lease(): void {
    this.#leased = true;
    this.#updateRef();
  }

  withdraw(waiter: HostWaiter): void {
    if (this.#waiter === waiter) this.#waiter = undefined;
  }

  async start(): Promise<void> {
    const binary = this.#pool.binary;
    try {
      // Node on Windows runs batch files only through a shell, so the spawn would fail with EINVAL.
      if (process.platform === "win32" && /\.(bat|cmd)$/i.test(binary))
        throw new Alpha2HostError(
          "HOST_NOT_EXECUTABLE",
          `alpha2-host must be the executable, not a batch file: ${binary}`,
        );
      await access(
        binary,
        process.platform === "win32" ? constants.F_OK : constants.X_OK,
      ).catch((cause: unknown) => {
        throw new Alpha2HostError(
          "HOST_NOT_EXECUTABLE",
          `cannot execute alpha2-host: ${binary}`,
          { cause },
        );
      });
      // macOS would hold a quarantined host on an approval prompt nobody can answer.
      if (process.platform === "darwin") {
        const refusal = await quarantineCheck(binary);
        if (refusal !== undefined)
          throw new Alpha2HostError("HOST_QUARANTINED", refusal);
      }
    } catch (error) {
      this.#startError = () => error as Error;
      this.#exit(null, null);
      return;
    }
    if (this.#retired) this.#exit(null, null);
    else this.#spawn();
  }

  /** Stops sending requests here, and closes stdin once no request runs. */
  retire(): void {
    if (this.#retired) return;
    this.#retired = true;
    const waiter = this.#waiter;
    if (!this.#ready && waiter !== undefined) {
      this.#waiter = undefined;
      this.#pool.requeue(waiter);
    }
    if (!this.#leased) this.#closeInput();
    this.#updateRef();
  }

  /** Writes the request and settles with its answer; the host must be leased. */
  run(
    frame: Frame,
    requestId: string,
    signal: AbortSignal | undefined,
    onProgress: ExportProgressCallback | undefined,
  ): Promise<RunOutcome> {
    const wire = this.#wire;
    if (this.#retired || wire === undefined) {
      this.release();
      return Promise.resolve({ kind: "unsent" });
    }
    if (signal?.aborted) {
      this.release();
      return Promise.reject(new Alpha2AbortError());
    }
    return new Promise((resolveRun, rejectRun) => {
      const stderrMark = this.#stderrTotal;
      // Baseline for detecting an OOM kill while the request runs.
      const oomKillsBefore = oomKillCount();
      let lastPhase: ProgressPhase | undefined;
      let answer: ResponseMessage | undefined;
      let failure: FailureBuilder | undefined;
      let written = false;
      let settled = false;
      let cancelTimer: NodeJS.Timeout | undefined;
      let writeFailedTimer: NodeJS.Timeout | undefined;
      let settleTimer: NodeJS.Timeout | undefined;
      // Unsettled onProgress promises; the request settles only after them.
      const pendingCallbacks = new Set<Promise<void>>();
      let callbackFailure: Error | undefined;
      const phase = (): ExportPhase =>
        lastPhase === "finalizing"
          ? "mux"
          : lastPhase === "frames"
            ? "render"
            : "prepare";
      const stderrTail = (): string => this.#stderrSince(stderrMark);
      const settle = (decide: () => RunOutcome | Error): void => {
        if (settled) return;
        settled = true;
        signal?.removeEventListener("abort", abort);
        for (const timer of [cancelTimer, writeFailedTimer, settleTimer])
          clearTimeout(timer);
        this.#run = undefined;
        const decided = decide();
        if (!this.#gone) this.release();
        void Promise.allSettled([...pendingCallbacks]).then(() => {
          if (decided instanceof Error) rejectRun(decided);
          else if (callbackFailure !== undefined) rejectRun(callbackFailure);
          else resolveRun(decided);
        });
      };
      const fromAnswer = (message: ResponseMessage): RunOutcome | Error => {
        if (failure !== undefined) return failure(stderrTail(), phase());
        if (message.ok) return { kind: "result", result: message.result };
        const error = errorFromHost(message.error, stderrTail(), phase());
        return message.error.code === "HOST_RETIRING"
          ? { kind: "retiring", error }
          : error;
      };
      // A running request gets a cancel line so the host removes its partial file; the host is
      // ended if it does not answer in time, or at once when the request is not fully written.
      const cancel = (error: Error): void => {
        if (settled || answer !== undefined || failure !== undefined) return;
        failure = () => error;
        if (!written || this.#inputClosed || !this.#alive()) {
          this.#kill();
          return;
        }
        wire.write(
          encodeMessage(
            createRequest("cancel", `${requestId}:cancel`, {
              target: requestId,
            }),
          ),
        );
        cancelTimer = setTimeout(() => this.#kill(), HOST_CANCEL_TIMEOUT_MS);
      };
      const abort = (): void => cancel(new Alpha2AbortError());
      const callbackFailed = (error: Error): void => {
        callbackFailure ??= error;
        cancel(error);
      };
      const track = (returned: unknown): void => {
        if (!isThenable(returned)) return;
        const pending = Promise.resolve(returned).then(
          () => undefined,
          (cause: unknown) =>
            callbackFailed(
              new Alpha2HostError(
                "PROGRESS_CALLBACK_FAILED",
                "onProgress callback rejected",
                { cause, phase: phase() },
              ),
            ),
        );
        pendingCallbacks.add(pending);
        void pending.then(() => pendingCallbacks.delete(pending));
      };
      this.#run = {
        phase,
        progress: (message) => {
          if (message.requestId !== requestId) return;
          lastPhase = message.phase ?? "frames";
          notifyProgress(message, onProgress, callbackFailed, track, phase());
        },
        answer: (message) => {
          // One request at a time, so an answer to an unreadable line is this request's answer.
          if (
            answer !== undefined ||
            (message.requestId !== requestId &&
              message.requestId !== UNREAD_REQUEST_ID)
          )
            return;
          answer = message;
          if (message.hostState === "draining") this.retire();
          if (message.ok) settle(() => fromAnswer(message));
          else
            settleTimer = setTimeout(
              () => settle(() => fromAnswer(message)),
              STDERR_SETTLE_MS,
            );
        },
        fail: (error) => {
          failure ??= error;
        },
        exited: (code, exitSignal) =>
          settle(() => {
            if (answer !== undefined) return fromAnswer(answer);
            const tail = stderrTail();
            if (failure !== undefined) return failure(tail, phase());
            return exitError(code, exitSignal, tail, oomKillsBefore, phase());
          }),
      };
      signal?.addEventListener("abort", abort, { once: true });
      wire.write(frame, (error) => {
        if (!error) {
          written = true;
          return;
        }
        // Give the host's fatal message or exit time to name the cause first.
        writeFailedTimer = setTimeout(() => {
          failure ??= () =>
            new Alpha2HostError("HOST_WRITE_FAILED", error.message, {
              cause: error,
            });
          this.#kill();
        }, HOST_SHUTDOWN_TIMEOUT_MS);
      });
    });
  }

  #spawn(): void {
    const binary = this.#pool.binary;
    const startFailed = (cause: unknown): Alpha2HostError => {
      const refusal = spawnRefusal(binary, cause);
      return refusal === undefined
        ? new Alpha2HostError(
            "HOST_START_FAILED",
            `could not start alpha2-host: ${String(cause)}`,
            { cause },
          )
        : new Alpha2HostError(refusal.code, refusal.message, { cause });
    };
    this.#oomKillsAtStart = oomKillCount();
    let child: ChildProcess;
    try {
      child = spawn(binary, ["serve"], {
        stdio: ["pipe", "pipe", "pipe"],
        windowsHide: true,
        env: this.#environment,
      });
    } catch (cause) {
      this.#startError = () => startFailed(cause);
      this.#exit(null, null);
      return;
    }
    this.#child = child;
    const { stdin, stdout, stderr } = child;
    const streamsMissing = !stdin || !stdout || !stderr;
    // A spawn without standard streams (EMFILE, ENFILE) may never emit close, so end it now.
    const failWithoutHost = (error: Alpha2HostError): void => {
      this.#startError ??= () => error;
      if (child.pid !== undefined) child.kill("SIGKILL");
      this.#exit(null, null);
    };
    // Register first: an unhandled spawn `error` event would crash this process.
    child.on("error", (cause) => {
      if (streamsMissing) failWithoutHost(startFailed(cause));
      else this.#fail(() => startFailed(cause));
    });
    child.on("close", (code, exitSignal) => this.#exit(code, exitSignal));
    if (streamsMissing) {
      // Fallback in case Node does not report the cause on the next tick.
      setImmediate(() =>
        failWithoutHost(
          startFailed("its standard streams could not be created"),
        ),
      );
      return;
    }
    // Pipe errors when the host exits would crash this process if unhandled; the exit decides.
    const ignore = (): void => {};
    stdin.on("error", ignore);
    stdout.on("error", ignore);
    stderr.on("error", ignore);
    stderr.on("data", (chunk: Buffer) => this.#appendStderr(chunk));
    this.#wire = new HostWire(stdin, stdout, {
      message: (message) => this.#receive(message),
      invalid: (cause) =>
        this.#fail(
          (_stderr, phase) =>
            new Alpha2HostError(
              "INVALID_RESPONSE",
              "alpha2-host returned invalid JSON",
              { cause, phase },
            ),
        ),
      diagnostic: (text) => this.#appendStderr(Buffer.from(text)),
    });
    // A host held before running (approval prompt, scan, stopped process) writes nothing.
    const timeoutMs = this.#pool.startTimeoutMs;
    if (timeoutMs <= MAX_TIMER_MS)
      this.#startTimer = setTimeout(() => {
        this.#startTimedOut = true;
        this.#startError ??= () =>
          new Alpha2HostError(
            "HOST_START_TIMEOUT",
            hostStartTimeoutMessage(binary, timeoutMs),
          );
        this.#retired = true;
        child.kill("SIGKILL");
      }, timeoutMs);
  }

  #receive(message: HostMessage): void {
    if ("type" in message && message.type === "state") {
      if (message.state === "draining") this.retire();
      else if (!this.#ready && !this.#retired) this.#becomeReady();
      return;
    }
    if ("type" in message && message.type === "fatal") {
      const error = message.error;
      const fatal: FailureBuilder = (stderr, phase) =>
        errorFromHost(error, stderr, phase);
      if (!this.#ready) this.#startError ??= fatal;
      else this.#run?.fail(fatal);
      // The host exits after a fatal message; nothing is left to cancel.
      this.#retired = true;
      this.#closeInput();
      this.#updateRef();
      return;
    }
    if (!this.#ready) {
      this.#fail(
        () =>
          new Alpha2HostError(
            "INVALID_RESPONSE",
            "alpha2-host wrote a message before its ready state",
          ),
      );
      return;
    }
    if ("type" in message) this.#run?.progress(message);
    else this.#run?.answer(message);
  }

  #becomeReady(): void {
    this.#ready = true;
    clearTimeout(this.#startTimer);
    const waiter = this.#waiter;
    this.#waiter = undefined;
    if (waiter === undefined) {
      this.#updateRef();
      this.#pool.dispatch();
      return;
    }
    this.lease();
    waiter.grant(this);
  }

  /** A failure that ends the host; the request it starts or runs fails with it. */
  #fail(error: FailureBuilder): void {
    if (!this.#ready) this.#startError ??= error;
    else this.#run?.fail(error);
    this.#kill();
  }

  release(): void {
    this.#leased = false;
    this.#updateRef();
    if (this.#retired) this.#closeInput();
    else this.#pool.dispatch();
  }

  #closeInput(): void {
    if (this.#inputClosed || this.#gone) return;
    this.#inputClosed = true;
    this.#wire?.end();
    // A host that stays up once its input ends is terminated, then killed.
    this.#terminateTimer = setTimeout(
      () => this.#kill(),
      HOST_SHUTDOWN_TIMEOUT_MS,
    );
  }

  /** Closes stdin, then SIGTERM, then SIGKILL after the shutdown timeout. */
  #kill(): void {
    if (this.#gone) return;
    this.#retired = true;
    this.#closeInput();
    clearTimeout(this.#terminateTimer);
    if (this.#alive()) this.#child?.kill("SIGTERM");
    this.#killTimer ??= setTimeout(() => {
      if (this.#alive()) this.#child?.kill("SIGKILL");
    }, HOST_SHUTDOWN_TIMEOUT_MS);
    this.#updateRef();
  }

  #exit(code: number | null, exitSignal: string | null): void {
    if (this.#gone) return;
    this.#gone = true;
    this.#retired = true;
    for (const timer of [
      this.#startTimer,
      this.#terminateTimer,
      this.#killTimer,
    ])
      clearTimeout(timer);
    this.#wire?.close();
    const waiter = this.#waiter;
    this.#waiter = undefined;
    const run = this.#run;
    this.#pool.removed(this);
    this.#markExited();
    if (run !== undefined) run.exited(code, exitSignal);
    else if (!this.#ready && waiter !== undefined)
      void this.#startFailure(code, exitSignal).then(waiter.refuse);
  }

  async #startFailure(
    code: number | null,
    exitSignal: string | null,
  ): Promise<Error> {
    // A macOS host killed before it was ready (start timeout or dismissed approval prompt) is
    // checked for quarantine again.
    if (
      process.platform === "darwin" &&
      (this.#startTimedOut ||
        (this.#startError === undefined && exitSignal === "SIGKILL"))
    ) {
      const refusal = await quarantineCheck(this.#pool.binary, true);
      if (refusal !== undefined)
        return new Alpha2HostError("HOST_QUARANTINED", refusal);
    }
    const tail = this.#stderrSince(0);
    return (
      this.#startError?.(tail, "prepare") ??
      exitError(code, exitSignal, tail, this.#oomKillsAtStart, "prepare")
    );
  }

  #alive(): boolean {
    const child = this.#child;
    return (
      child !== undefined &&
      child.exitCode === null &&
      child.signalCode === null
    );
  }

  /** Only an idle host lets this process exit; the host exits when its stdin closes with it. */
  #updateRef(): void {
    const child = this.#child;
    const hold = !this.idle;
    if (child === undefined || hold === this.#held) return;
    this.#held = hold;
    for (const handle of [child, child.stdin, child.stdout, child.stderr]) {
      const counted = handle as { ref?: () => void; unref?: () => void } | null;
      if (hold) counted?.ref?.();
      else counted?.unref?.();
    }
  }

  #appendStderr(chunk: Buffer): void {
    this.#stderrTotal += chunk.length;
    this.#stderr = Buffer.concat([this.#stderr, chunk]);
    if (this.#stderr.length > STDERR_TAIL_BYTES)
      this.#stderr = this.#stderr.subarray(
        this.#stderr.length - STDERR_TAIL_BYTES,
      );
  }

  /** What the host wrote to stderr since `mark` bytes, at most {@link STDERR_TAIL_BYTES}. */
  #stderrSince(mark: number): string {
    const written = this.#stderrTotal - mark;
    return utf8Tail(
      this.#stderr.subarray(Math.max(0, this.#stderr.length - written)),
      STDERR_TAIL_BYTES,
    );
  }
}

/** Classifies a host that exited before answering. */
function exitError(
  code: number | null,
  exitSignal: string | null,
  tail: string,
  oomKillsBefore: number | undefined,
  phase: ExportPhase,
): Alpha2HostError {
  const ended = describeExit(code, exitSignal);
  const detail = tail.trim() || ended;
  const exit = { stderr: tail, exitCode: code, signal: exitSignal };
  if (code === 70 || detail.includes("GPU_INIT_FAILED"))
    return new Alpha2GpuInitializationError(detail, exit);
  // A policy refused the executable or a library; retrying would fail the same way.
  if (code !== null && windowsPolicyBlocked(code))
    return new Alpha2HostError(
      "HOST_BLOCKED",
      hostBlockedMessage(windowsStatus(code)),
      exit,
    );
  // The loader refused the executable (missing or mismatched FFmpeg); not retryable.
  if (loaderRefused(code, tail))
    return new Alpha2HostError(
      "HOST_LOAD_FAILED",
      `alpha2-host could not be loaded: ${detail}${glibcHint(tail)}`,
      exit,
    );
  if (
    exitSignal === "SIGKILL" &&
    oomKillsBefore !== undefined &&
    (oomKillCount() ?? oomKillsBefore) > oomKillsBefore
  ) {
    // An OOM kill in this cgroup while the host ran; a smaller `memory.scale` may help.
    const failure: ExportFailure = {
      code: "OUT_OF_MEMORY",
      message: `the kernel's out-of-memory killer ended alpha2-host (${ended})`,
      origin: "node",
      operation: "export",
      backend: "native",
      phase,
      resource: "cpu",
      ...exit,
    };
    return new Alpha2HostError(
      "HOST_OOM_KILLED",
      `the kernel's out-of-memory killer ended alpha2-host: ${detail}`,
      { failure, ...exit },
    );
  }
  // No known cause; a SIGKILL without an OOM count rise may be an external kill.
  const failure: ExportFailure = {
    code: "HOST_EXITED",
    message: `alpha2-host exited before responding (${ended})`,
    origin: "node",
    operation: "export",
    backend: "native",
    phase,
    resource: "unknown",
    ...exit,
  };
  return new Alpha2HostError(
    "HOST_EXITED",
    `alpha2-host exited before responding: ${detail}`,
    { failure, ...exit },
  );
}

function isThenable(value: unknown): value is PromiseLike<unknown> {
  return (
    (typeof value === "object" || typeof value === "function") &&
    value !== null &&
    typeof (value as { then?: unknown }).then === "function"
  );
}

function notifyProgress(
  message: ProgressMessage,
  callback: ExportProgressCallback | undefined,
  fail: (error: Error) => void,
  track: (returned: unknown) => void,
  phase: ExportPhase,
): void {
  try {
    track(
      callback?.({
        phase: message.phase ?? "frames",
        completed: message.completed,
        total: message.total,
      }),
    );
  } catch (cause) {
    fail(
      new Alpha2HostError(
        "PROGRESS_CALLBACK_FAILED",
        "onProgress callback threw",
        { cause, phase },
      ),
    );
  }
}

function errorFromHost(
  error: ProtocolError,
  stderr: string,
  phase: ExportPhase,
): Alpha2HostError {
  const failure =
    error.failure === undefined
      ? undefined
      : ({
          ...error.failure,
          message: error.message,
          stderr,
        } as unknown as ExportFailure);
  if (error.code === "GPU_INIT_FAILED")
    return new Alpha2GpuInitializationError(error.message, {
      failure,
      stderr,
      phase,
    });
  if (error.code === "CONFORM_FAILED" && error.details !== undefined)
    return new Alpha2ConformError(
      error.message,
      error.details as ConformReport,
      { failure, stderr, phase },
    );
  const details = error.details as
    { keptPath?: unknown; path?: unknown; problem?: unknown } | undefined;
  const keptPath = details?.keptPath;
  return new Alpha2HostError(error.code, error.message, {
    failure,
    stderr,
    phase,
    ...(error.code === "OUTPUT_PUBLISH_FAILED" && typeof keptPath === "string"
      ? { keptPath }
      : {}),
    ...(typeof details?.path === "string" && typeof details.problem === "string"
      ? { path: details.path, problem: details.problem }
      : {}),
  });
}

function proxiesPayload(
  proxies: Readonly<Record<string, readonly ProxyFile[]>> | undefined,
): Record<string, unknown[]> {
  return Object.fromEntries(
    Object.entries(proxies ?? {}).map(([id, files]) => [
      id,
      files.map((file) => ({
        path: file.path,
        ...(file.madeFrom === undefined ? {} : { madeFrom: file.madeFrom }),
        ...(file.colorPipeline === undefined
          ? {}
          : { colorPipeline: file.colorPipeline }),
      })),
    ]),
  );
}

function asarUnpacked(path: string): string {
  return path.replace(/([\\/])app\.asar([\\/])/, "$1app.asar.unpacked$2");
}

function isFile(path: string): boolean {
  try {
    return existsSync(path) && statSync(path).isFile();
  } catch {
    return false;
  }
}

/** The last at most `limit` bytes as text, starting at a whole character. */
function utf8Tail(bytes: Buffer, limit: number): string {
  let start = Math.max(0, bytes.length - limit);
  // Skip continuation bytes (10xxxxxx).
  while (start < bytes.length && ((bytes[start] ?? 0) & 0xc0) === 0x80)
    start += 1;
  const text = bytes.subarray(start).toString("utf8");
  // Invalid bytes decode as 3-byte U+FFFD, so trim from the front to fit again.
  let excess = Buffer.byteLength(text) - limit;
  let index = 0;
  while (excess > 0 && index < text.length) {
    const point = text.codePointAt(index) ?? 0;
    excess -= Buffer.byteLength(String.fromCodePoint(point));
    index += point > 0xffff ? 2 : 1;
  }
  return text.slice(index);
}

const HOST_SHUTDOWN_TIMEOUT_MS = 5_000;

const HOST_CANCEL_TIMEOUT_MS = 5_000;

/** Longest `setTimeout` delay; a longer start timeout means none. */
const MAX_TIMER_MS = 2 ** 31 - 1;

const quarantineChecks = new Map<string, Promise<string | undefined>>();

function quarantineCheck(
  binary: string,
  again = false,
): Promise<string | undefined> {
  if (again) quarantineChecks.delete(binary);
  let check = quarantineChecks.get(binary);
  if (check === undefined) {
    check = quarantineRefusal(binary).catch(() => undefined);
    quarantineChecks.set(binary, check);
  }
  return check;
}

/** Must match the host's limit, newline excluded. */
const MAX_REQUEST_LINE_BYTES = 2 * MAX_PROJECT_JSON_BYTES;

/** The `requestId` the host uses when it could not read the request's own. */
const UNREAD_REQUEST_ID = "unknown";

let nextRequestSequence = 0;

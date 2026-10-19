import { type ChildProcess, spawn } from "node:child_process";
import { constants, existsSync, statSync } from "node:fs";
import { access } from "node:fs/promises";
import { delimiter, dirname, join, resolve } from "node:path";
import { createInterface, type Interface } from "node:readline";
import type { Writable } from "node:stream";
import { fileURLToPath } from "node:url";

import type {
  AssetCatalog,
  ConformReport,
  MediaPurpose,
  Selection,
} from "alpha2/assets";
import type { CapabilityReport } from "alpha2";
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
  createRequest,
  loaderRefused,
  parseHostMessage,
} from "./protocol.js";

export { PROTOCOL_VERSION } from "./protocol.js";
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
}

/** Options of an {@link Alpha2NodeClient}. */
export interface Alpha2NodeOptions extends ResolveHostBinaryOptions {
  /** Maximum simultaneous sidecars. Defaults to one. */
  readonly concurrency?: number;
  /**
   * Video decode path for every request this client makes. When it is unset the host uses its
   * platform default, which is "auto" on Linux and "software" on macOS and Windows. "auto" uses the
   * GPU decoder where the device and stream support it, and "hardware-required" fails instead of
   * decoding a video frame on the CPU.
   */
  readonly videoDecode?: VideoDecodeMode;
}

/** Video decode path requested from the native host. */
export type VideoDecodeMode = "software" | "auto" | "hardware-required";

const VIDEO_DECODE_MODES: readonly VideoDecodeMode[] = [
  "software",
  "auto",
  "hardware-required",
];

/**
 * Refuses a `field` that is set and not a {@link VideoDecodeMode}: the host would refuse it at its
 * start, after the process was made.
 */
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

/**
 * The engine's five quality levels. The engine defines what each one costs (a bitrate that depends
 * on codec, output size and frame rate) and the host resolves it before the encoder opens; an app
 * maps its own presets onto these rather than inventing rates.
 */
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
       * A constant quantizer on the codec's own scale (0-51 for H.264). The encoder runs it where
       * it can and falls back to `bitrate` where it cannot; `videoRateControl.applied` reports
       * which ran.
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
   * What the encoder ran: `quantizer`, or `bitrate` because none was asked for or because the
   * encoder refused it (`quantizerRefused` says why).
   */
  readonly applied: "quantizer" | "bitrate";
  readonly quantizerRefused: string | null;
  /** The level the request named, or null for an explicit rate. */
  readonly level: QualityLevel | null;
  /**
   * `env-override` means `ALPHA2_EXPORT_VIDEO_BITRATE` was set in the host's environment and won
   * over the request. It is test instrumentation, never a production setting.
   */
  readonly source: "request" | "env-override";
}

/** Whether an export may copy its source's packets; see {@link ExportProjectRequest.strategy}. */
export type ExportStrategy = "render" | "prefer-copy" | "require-copy";

/** How an export's encoder receives frames; see {@link ExportProjectRequest.inputPolicy}. */
export type InputPolicy = "auto" | "gpu-required" | "cpu-readback";

/** One progress report of an export. */
export interface ExportProgress {
  /**
   * `frames` while frames are encoded; `finalizing` once every frame is and the output is being
   * flushed and closed. The export is complete only when `exportProject` resolves.
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

/** Successful metadata returned by the native host after an export. */
export interface NativeExportResult {
  readonly renderMs: number;
  readonly frames: number;
  readonly audio: boolean;
  readonly fps: number;
  /**
   * Which output path produced this export. Several `performance` counters are structurally absent
   * on one of them rather than merely zero, so a reader comparing two exports needs this to tell
   * the two kinds of zero apart.
   */
  readonly videoOutputPath: "readback" | "gpu-resident" | "packet-copy";
  /**
   * Why the export took the readback path: the host has no GPU encode device, the codec or a pinned
   * encoder is not the GPU path's, `inputPolicy` is `cpu-readback`, an old validation layer,
   * `ALPHA2_GPU_ENCODE=0`, an output the encoder refuses, or an in-flight budget too small for it.
   * `null` on the GPU-resident path.
   */
  readonly gpuOutputPathUnavailable: string | null;
  /** The request's {@link ExportProjectRequest.inputPolicy}. */
  readonly inputPolicy: InputPolicy;
  /** What the export did: rendered and encoded every frame, or copied the source's packets. */
  readonly strategy: "render" | "copy";
  /** The request's {@link ExportProjectRequest.strategy}. */
  readonly requestedStrategy: ExportStrategy;
  /** Why a `prefer-copy` export rendered instead; `null` when it copied or did not ask to. */
  readonly copyIneligible: string | null;
  /**
   * A copy's audio: `copy` when the mix is the source's own audio, untouched, over the whole file,
   * so its packets are copied; `encode` when it is mixed as a render mixes it and encoded to AAC
   * beside the copied video; `null` without audio. Absent on rendered exports.
   */
  readonly audioStrategy?: "copy" | "encode" | null;
  /** Why a copy's audio was encoded rather than copied. Absent on rendered exports. */
  readonly audioCopyIneligible?: string | null;
  /** Clips a copy joined back to back. Absent on rendered exports. */
  readonly copiedSegments?: number;
  /**
   * How a GPU-resident export fed its encoder without rendering: `yuv-repack` when every frame was
   * a source picture shown as it is (the same clips a copy needs, any codec or frame rate, each
   * source already at the output size, upright with square pixels, tagged BT.709 limited 8-bit
   * 4:2:0), so each decoded picture was copied plane for plane into the encoder's input with no
   * graph and no color conversion (`performance.yuvRepacks`, with `graphPasses` and
   * `sourceConversions` zero). `null` when it rendered, and on a copy.
   */
  readonly videoTranscode: "yuv-repack" | null;
  /** Why a rendered export did not transcode; `null` when it did, and on a copy. */
  readonly yuvTranscodeIneligible: string | null;
  /**
   * The timeline range the export covered: the requested range widened to the frame grid.
   * `startTicks` is the output's zero for video and audio, and can be up to one frame before the
   * requested start.
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
  /**
   * What extension providers reported they could not draw as asked (a missing font or image, a
   * feature left out, changed samples, a normalized delay), each once per source in the order first
   * seen; `[]` when none did.
   */
  readonly providerDiagnostics: readonly ProviderDiagnostic[];
  /** Distinct provider reports past the 256 `providerDiagnostics` keeps. */
  readonly providerDiagnosticsOmitted: number;
  /**
   * What the export's decoders reported about damaged input, per source in source ID order: what a
   * `tolerant` decode policy (`ALPHA2_DECODE_POLICY`) skipped, and silence inserted where a clean
   * stream's timestamps jump; `[]` when they reported nothing.
   */
  readonly decodeDiagnostics: readonly DecodeDiagnostics[];
  /**
   * What the renderer's result cache did: repeated frames and unchanged clip pictures are found
   * instead of drawn, with the same output.
   */
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
   * The loudness normalization an export asked for: what the mix measured before it, the gain
   * applied (`applied` is the plan, `null` for a mix silent below -70 LUFS), and what the export
   * put out (`output`, measured after the master limiter). `null` when the request had no
   * `loudness`.
   */
  readonly loudness: {
    readonly targetLufs: number;
    readonly truePeakCeilingDbtp: number;
    /** The master limiter's linear threshold the plan kept the mix under, `null` without one. */
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
  /**
   * The video rate control the encoder was opened with; `null` for a copy, which encodes nothing.
   */
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

/**
 * An export's timing and pipeline counters (see the host protocol). Most are numbers, flags or
 * names; the ones that are not are declared. A copy reports a few counters of its own and none of
 * the declared ones.
 */
export interface ExportPerformance {
  /** Per reason a source left the hardware decode path, how many times it did. */
  readonly hardwareFallbackReasons?: Readonly<Record<string, number>>;
  /**
   * How many times the encode worker took a command and left each number of frames behind in its
   * channel, indexed by that number.
   */
  readonly encodeChannelDepthSamples?: readonly number[];
  /** What the GPU-resident path reserved from the in-flight budget; `null` on the readback path. */
  readonly gpuEncodeBudget?: GpuEncodeBudget | null;
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
  | GpuEncodeBudget;

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
 * One extension provider report: the source it is about, its `kind` (`missing_font`,
 * `missing_image`, `unreadable_image`, `unsupported_feature`, `unreadable_item`,
 * `normalized_delay`, `non_finite_samples`, `out_of_half_range`, `reduced_precision`), and that
 * kind's fields.
 */
export interface ProviderDiagnostic {
  readonly source: string;
  readonly kind: string;
  readonly [field: string]: unknown;
}

/**
 * What one source's decoders reported during an export. Audio samples are at the project's sample
 * rate.
 */
export interface DecodeDiagnostics {
  readonly source: string;
  /**
   * Corruption observations: FFmpeg may report one damaged packet at more than one stage, so this
   * is not a count of distinct packets.
   */
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

/** Where {@link ExportProjectRequest.renderCache} bakes frames. */
export interface RenderCacheOptions {
  readonly directory: string;
  /** The most bytes the directory holds. Default 4 GiB. */
  readonly quotaBytes?: number;
  /**
   * A frame rendered while the host's cache writer is full: `"skip"` (default) leaves it unbaked so
   * the cache never slows the export; `"wait"` holds the export until it is baked.
   */
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
       * The cache's writer or reader was left stuck in the directory's
       * filesystem; the export rendered every frame it had not taken by
       * then, the writer's counts are what it had done, and the host process
       * uses the directory no more.
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
   * A file path per source id, for the sources a render reads. The host checks them before it
   * renders and refuses with an {@link Alpha2ConformError} listing what is missing, offline, or not
   * the media the project recorded.
   */
  readonly assets: Readonly<Record<string, string>>;
  /** Proxies per source id, in order of preference. */
  readonly proxies?: Readonly<Record<string, readonly ProxyFile[]>>;
  /**
   * Each image sequence's frame files in the project's order, per source id; `null` where the
   * sequence has a gap.
   */
  readonly frames?: Readonly<Record<string, readonly (string | null)[]>>;
  /**
   * Files an SVG source's `<image href>` or a Lottie source's image assets name outside the source,
   * per source id and then per locator as the source writes it
   * (`{ label: { "art/badge.png": "/media/badge.png" } }`): what a browser page registers with
   * `registerResource`. A locator not given draws as missing and is reported as `missing_image`.
   * Resources for a source that is not an SVG or Lottie source of the project are `INVALID_INPUT`.
   */
  readonly resources?: Readonly<
    Record<string, Readonly<Record<string, string>>>
  >;
  /**
   * `original` (the default) reads originals only; `proxies` reads fresh proxies where there are
   * some. The result's `media` says what each source read.
   */
  readonly media?: "original" | "proxies";
  /** A file path per effect resource id (LUT files and the like). */
  readonly effectAssets?: Readonly<Record<string, string>>;
  /**
   * Font files caption tracks, text generators, and SVG text draw with, in fallback order. A
   * project that shows text fails without them.
   */
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
  /** Defaults to `h264`. */
  readonly videoCodec?: "h264" | "hevc" | "vp9" | "av1" | "mpeg4-part2";
  /**
   * Concrete FFmpeg encoder short name: one of `capabilities.encode.video`, or the GPU path's
   * `capabilities.encode.gpuOutputPath.encoder`. Pinning the GPU path's encoder asks for the
   * GPU-resident path when it takes GPU frames only (`gpuOutputPath.gpuInputOnly`, as `h264_vulkan`
   * does); `h264_videotoolbox` is in both and runs on whichever path `inputPolicy` selects.
   */
  readonly videoEncoder?: string;
  /**
   * How the encoder receives frames: `auto` (default) takes the GPU-resident path where the host,
   * codec, size, and budget allow it and the readback path otherwise; `gpu-required` fails with
   * `GPU_OUTPUT_UNAVAILABLE` instead of falling back; `cpu-readback` always reads back. The
   * result's `videoOutputPath` says which ran.
   */
  readonly inputPolicy?: InputPolicy;
  /**
   * `render` (default) renders and encodes every frame.
   *
   * `prefer-copy` copies the sources' compressed packets instead when both hold:
   *
   * - the range is its sources shown untouched: clips that tile it one after another, each with an
   *   identity transform and no composition, mask, matte, caption, transition, or retime, none
   *   another clip's matte, cut at IDR pictures and encoded alike;
   * - its packets under the range decode on their own into exactly those frames: an MP4/MOV H.264
   *   or HEVC source already at the output size, tagged BT.709 limited 8-bit 4:2:0, neither
   *   mirrored nor shown with non-square pixels (the container's declared pixel aspect ratio, else
   *   the codec's) nor declaring non-square pixels in the codec, on the export's frame grid,
   *   starting at an IDR picture.
   *
   * A copy's audio, if any, is copied when it is the only sound the render hears and the source's
   * own, untouched, over the whole file; otherwise it is mixed as a render mixes it (silence
   * included) and encoded to AAC beside the copied video. `videoQuality` does not apply to a copy,
   * which keeps the source's encoding. When the range does not qualify, `prefer-copy` renders and
   * says why in `copyIneligible`; `require-copy` fails with `COPY_INELIGIBLE` instead.
   */
  readonly strategy?: ExportStrategy;
  /** Output video quality. Defaults to `high`. */
  readonly videoQuality?: VideoQuality;
  /** Defaults to `aac`. */
  readonly audioCodec?: "aac";
  /** Output audio quality. Defaults to `high`. */
  readonly audioQuality?: AudioQuality;
  /**
   * Normalizes the export's audio to an integrated loudness (ITU-R BS.1770-4, EBU R128), keeping
   * its true peak at or below a ceiling. Needs `includeAudio` and a project that uses an audible
   * source; the host refuses it otherwise with `INVALID_INPUT`. The host measures the mix in a
   * first pass.
   */
  readonly loudness?: LoudnessTarget;
  /**
   * A directory to bake the export's frames into and find them in: a later export of the same
   * snapshot (the same host build, GPU, project, files, and effects) takes each baked frame instead
   * of rendering it, and writes the same file. `quotaBytes` (default 4 GiB) bounds the directory;
   * the least recently used frames go first, pinned clips' frames last. Frames are baked on the
   * readback path only, on a writer thread beside the export; `backlog` says whether a frame the
   * writer has no room for is skipped or waited for. The result's `renderCache` says what happened.
   */
  readonly renderCache?: RenderCacheOptions;
  /**
   * Byte budgets for this export, replacing the host's environment settings
   * (`ALPHA2_EXPORT_IN_FLIGHT_BYTES` and the rest) and defaults field by field. Each bounds memory
   * the host reserves before it allocates; an export whose least working set does not fit fails
   * with `RESOURCE_LIMIT` before it renders. The result's `memoryPolicy` reports what applied and
   * where each value came from.
   */
  readonly memory?: ExportMemoryBudget;
  /** Overrides the client's decode mode for this export. */
  readonly videoDecode?: VideoDecodeMode;
  /** The MP4 file to write. */
  readonly output: string;
  /**
   * Cancels the export: the host removes its partial file and exits, and the promise rejects with
   * `Alpha2AbortError` once it has.
   */
  readonly signal?: AbortSignal;
  /**
   * Receives each progress report. A throw, or a rejection of the promise it returns, fails the
   * export with `PROGRESS_CALLBACK_FAILED` and ends its host; the export settles only after every
   * promise it returned has.
   */
  readonly onProgress?: ExportProgressCallback;
  /**
   * Receives native timing and pipeline metrics after a successful export. The export resolves
   * after the promise it returns does; a throw or a rejection rejects the export with that error.
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
   * What the open extension-provider sources (SVG, animated images, Lottie, OpenEXR, image
   * sequences) hold together; past it the host closes the ones the last two frames did not show and
   * opens them again when shown. Default 512 MiB.
   */
  readonly providerSourceBytes?: number;
  /**
   * A fraction in (0, 1] every budget is scaled by once resolved: how a retry asks for less in
   * flight.
   */
  readonly scale?: number;
}

/** One encoder a native host can feed. */
export interface EncodeCapability {
  /** The codec id, such as `h264` or `aac`. */
  readonly id: string;
  /** The FFmpeg encoder short name, as `videoEncoder` takes it. */
  readonly encoder: string;
  /**
   * How the linked runtime declares this encoder executes, corrected for the encoders the pinned
   * runtime misdeclares (`h264_videotoolbox` is `hardware`). `hybrid` is a MediaFoundation
   * encoder, which is a software transform unless the host is configured to ask for a hardware one.
   */
  readonly encoderClass?: "software" | "hardware" | "hybrid";
  readonly hardwareAccelerated?: boolean;
  readonly preferred?: boolean;
  /** Smallest output the encoder accepts, in pixels; `null` when it reports none. */
  readonly minimumWidth?: number | null;
  readonly minimumHeight?: number | null;
}

/**
 * The GPU-resident output path, which the encoder list cannot describe.
 *
 * `capabilities.encode.video` lists the encoders the host's software sinks can feed. On Linux the
 * GPU path's encoder (`h264_vulkan`) takes hardware frames and nothing else, so it is not in that
 * list; on macOS it is `h264_videotoolbox`, which is. This says whether the path exists on this
 * host and what it accepts, so a caller can tell before exporting whether its output will be
 * GPU-resident: the codec must be the one named here, no other encoder may be pinned (pinning a
 * `gpuInputOnly` encoder asks for this path), and the output size must be even and within the
 * reported extents. Under `inputPolicy: "auto"` an export outside them still succeeds on the
 * readback path; under `gpu-required` it fails with `GPU_OUTPUT_UNAVAILABLE`. Every export reports
 * the path it took as `videoOutputPath`.
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
   * How a packed frame reaches the encoder: `gpu-copy` renders into the backend's planes and copies
   * them on the GPU; `direct-target` renders into the encoder's own surface.
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
  /**
   * Granularity the output is aligned up to before the extents apply. A 158x62 output is accepted
   * where 64x48 is not, because the first aligns up to the minimum coded extent and the second does
   * not.
   */
  readonly widthAlignment: number | null;
  readonly heightAlignment: number | null;
  /**
   * The device's H.264 encode reference limits. B pictures need a non-zero
   * `maxBPictureL0References` or `maxL1References`; absent when the path is unavailable, and null
   * when the encoder does not report them (VideoToolbox).
   */
  readonly maxPPictureL0References?: number | null;
  readonly maxBPictureL0References?: number | null;
  readonly maxL1References?: number | null;
  /** Rate-control modes the device reports: `cbr`, `vbr`, `disabled` (constant QP). */
  readonly rateControlModes?: readonly ("cbr" | "vbr" | "disabled")[];
  /**
   * Plane-texture pairs the renderer packs into besides the encoder's surfaces; 0 for
   * `direct-target`.
   */
  readonly planeRingSlots: number | null;
  /**
   * Surfaces the handoff and the encoder still hold after the host lets go; the in-flight byte
   * budget pays for them too.
   */
  readonly retainedSurfaces: number | null;
  readonly requiresEvenDimensions: boolean;
  /**
   * Every codec the path takes on this device, each with its own limits; the fields above describe
   * the first. One entry on Linux and macOS; on Windows each of H.264, HEVC and AV1 the device
   * encodes. Empty when the path is unavailable.
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
   * Whether the decoder advertises this platform's hardware configuration. Registration only: a
   * device, profile, and surface format are still decided per stream, and every export reports the
   * path its frames took.
   */
  readonly hardwareConfig: boolean;
  /**
   * The decoder a software session opens for the codec, or `null` when only a hardware session
   * decodes it (AV1 on a runtime built without libdav1d; the pinned FFmpeg SDK decodes it with
   * `libdav1d`). A source with such a stream is `unsupported` on a host that cannot decode it in
   * hardware.
   */
  readonly softwareDecoder: string | null;
}

/** What a native host encodes. */
export interface HostEncodeCapabilities {
  readonly container: "mp4";
  /** Encoders the host's software sinks can feed. */
  readonly video: readonly EncodeCapability[];
  readonly audio: readonly EncodeCapability[];
  readonly gpuOutputPath: GpuOutputPathCapability;
}

/** How a native host decodes video. */
export interface HostDecodeCapabilities {
  readonly mode: VideoDecodeMode;
  readonly backend: "vulkan" | "videotoolbox" | "d3d12va" | "none";
  /** Whether a shared decode context was created for this host process. */
  readonly deviceReady: boolean;
  /**
   * Why `auto` could not set up hardware decode at the host's start and decodes in software;
   * `null` otherwise.
   */
  readonly hardwareUnavailable: string | null;
  /** Whether the device can present 10-bit decoded planes. */
  readonly tenBit: boolean;
  /** Codecs the decode device itself enabled, where the backend reports them. */
  readonly deviceCodecs: readonly string[] | null;
  readonly codecs: readonly DecodeCodecCapability[];
}

/** What {@link Alpha2NodeClient.getCapabilities} reports about the host. */
export interface HostCapabilities {
  readonly status: "ready";
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
  /**
   * The host's report of a failed export. Without one, the error's code classifies it. See
   * `alpha2/export-failure`.
   */
  readonly failure?: ExportFailure;
  /** The export step the error came from, for a failure classified here. Default `prepare`. */
  readonly phase?: ExportPhase;
  /** The last part of the host's standard error, at most {@link STDERR_TAIL_BYTES}. */
  readonly stderr?: string;
  /** How the host process ended, when it ended before answering. */
  readonly exitCode?: number | null;
  readonly signal?: string | null;
}

/** Bytes of the host's standard error an error keeps. */
export const STDERR_TAIL_BYTES = 16_384;

/**
 * Every error this client rejects with. Its `failure` classifies it for a retry policy
 * (`alpha2/export-retry`): the host's own report when the host sent one, and otherwise the class
 * its code is evidence of (`failureForCode`), as the host would have classified the same code, so a
 * refusal made here or before an export started is as final as the host's.
 */
export class Alpha2HostError extends Error {
  /** A machine-readable code, such as `HOST_NOT_FOUND`, `INVALID_INPUT` or `DECODE_STALLED`. */
  readonly code: string;
  readonly failure: ExportFailure;
  readonly stderr?: string;
  readonly exitCode?: number | null;
  readonly signal?: string | null;

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
}

/** One capability probe and the calls waiting for it with a signal. */
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

/** The host could not create its GPU device (`GPU_INIT_FAILED`). */
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

/**
 * The file name `alpha2-host` has on `platform`: Windows executables carry `.exe`, and nothing
 * there adds it to a path that lacks it.
 */
export function hostExecutableName(
  platform: NodeJS.Platform = process.platform,
): string {
  return platform === "win32" ? "alpha2-host.exe" : "alpha2-host";
}

/** Where the native archives that carry `alpha2-host` are published. */
const HOST_RELEASES_URL = "https://github.com/alpha2-sh/alpha2/releases";

/**
 * Locates `alpha2-host` without loading Electron. The package does not ship the binary: it comes
 * in the platform's native archive on https://github.com/alpha2-sh/alpha2/releases, as
 * `bin/alpha2-host` beside the `lib/` it loads. Native archives and Electron applications should
 * keep `bin/` and `lib/` together and unpack both from ASAR. Resolution order is explicit path,
 * `ALPHA2_HOST_PATH`, Electron resources (`bin/`, then the directory itself), this package's own
 * `bin/`, then `PATH`.
 *
 * Throws an {@link Alpha2HostError}: `INVALID_INPUT` for an empty `hostPath`, `HOST_NOT_FOUND` when
 * an explicit path is not a file or no candidate exists; the latter message names the native
 * archive. An empty `ALPHA2_HOST_PATH` is unset.
 */
export function resolveHostBinary(
  options: ResolveHostBinaryOptions = {},
): string {
  const cwd = options.cwd ?? process.cwd();
  const executable = hostExecutableName();
  if (options.hostPath !== undefined && options.hostPath.trim() === "")
    throw new Alpha2HostError("INVALID_INPUT", "hostPath must not be empty");
  // An empty variable is an unset one, as a shell or Compose leaves a variable it was given blank.
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
  for (const directory of (process.env.PATH ?? "").split(delimiter)) {
    if (!directory) continue;
    const candidate = join(directory, executable);
    if (isFile(candidate)) return candidate;
  }
  throw new Alpha2HostError(
    "HOST_NOT_FOUND",
    `could not locate ${executable}; download the native archive for this platform from ` +
      `${HOST_RELEASES_URL} and set hostPath or ALPHA2_HOST_PATH to its bin/${executable}, ` +
      "or put that bin/ on PATH",
  );
}

/**
 * Runs requests on `alpha2-host` sidecar processes, one process per request, at most
 * `concurrency` at a time; further requests wait for a slot. Every rejection is an
 * {@link Alpha2HostError}.
 */
export class Alpha2NodeClient {
  readonly #binary: string;
  readonly #slots: Semaphore;
  readonly #videoDecode: VideoDecodeMode | undefined;
  /** Capability probes by decode mode: running, or answered. */
  readonly #probes = new Map<string, CapabilityProbe>();

  /**
   * Resolves the host binary now. Throws a `RangeError` for a `concurrency` that is not a positive
   * integer, and an {@link Alpha2HostError} when `videoDecode` is not a mode (`INVALID_INPUT`) or
   * the binary cannot be found.
   */
  constructor(options: Alpha2NodeOptions = {}) {
    const concurrency = options.concurrency ?? 1;
    if (!Number.isSafeInteger(concurrency) || concurrency < 1) {
      throw new RangeError("concurrency must be a positive integer");
    }
    checkVideoDecode("videoDecode", options.videoDecode);
    this.#binary = resolveHostBinary(options);
    this.#slots = new Semaphore(concurrency);
    this.#videoDecode = options.videoDecode;
  }

  /**
   * What the host can decode, render and encode, for the client's decode mode or
   * `options.videoDecode`. The first call for a mode starts a host, in a slot like any request;
   * calls made while it runs share it, and its report answers every later call for that mode on
   * this client, since the same binary and mode report the same. A probe that fails is not kept:
   * the next call starts another.
   *
   * `options.signal` withdraws this call (`Alpha2AbortError`); the probe goes on while another call
   * still waits for it, and is withdrawn, from its slot or its host, when none does.
   */
  async getCapabilities(
    options: CapabilitiesOptions = {},
  ): Promise<HostCapabilities> {
    checkVideoDecode("videoDecode", options.videoDecode);
    const { signal } = options;
    if (signal?.aborted) throw new Alpha2AbortError();
    const videoDecode = options.videoDecode ?? this.#videoDecode;
    // Unset, the mode is the host's default or the one this process's environment names.
    const key =
      videoDecode ?? `environment:${process.env.ALPHA2_VIDEO_DECODE ?? ""}`;
    let probe = this.#probes.get(key);
    if (probe === undefined) {
      const controller = new AbortController();
      const result = (async () => {
        const release = await this.#slots.acquire(controller.signal);
        try {
          return (await requestHost(
            this.#binary,
            "capabilities",
            {},
            { videoDecode },
            controller.signal,
          )) as HostCapabilities;
        } finally {
          release();
        }
      })();
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
    // Every call waiting counts, a call without a signal too: it holds the probe until it settles.
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
   * Which media a purpose would read for each source, and what blocks it, without rendering. The
   * host hashes an original only when the project recorded a hash to compare. Rejects with an
   * {@link Alpha2ProjectValidationError} for an invalid project.
   */
  async conform(request: ConformRequest): Promise<ConformResult> {
    const problems = projectProblems(request.project);
    if (problems.length > 0) throw new Alpha2ProjectValidationError(problems);
    validateMediaPaths(request, false);
    const release = await this.#slots.acquire(request.signal);
    try {
      return (await requestHost(
        this.#binary,
        "conform",
        {
          project: request.project,
          assets: request.assets ?? {},
          proxies: proxiesPayload(request.proxies),
          frames: request.frames ?? {},
          purpose: (request.purpose ?? "export_original").replace(/_/g, "-"),
        },
        { videoDecode: this.#videoDecode },
        request.signal,
      )) as ConformResult;
    } finally {
      release();
    }
  }

  /**
   * Renders and encodes `request.project` to `request.output` and resolves with that path.
   *
   * Rejects with an {@link Alpha2ProjectValidationError} or an `INVALID_INPUT` error before a host
   * starts when the project or request is invalid, an {@link Alpha2ConformError} when the media is
   * not ready, an {@link Alpha2AbortError} when `signal` aborts, and an {@link Alpha2HostError}
   * otherwise. Under the `auto` decode mode, an export whose hardware decoder stalls
   * (`DECODE_STALLED`) runs once more decoding in software; its progress starts over. When that
   * run fails too, its error is the export's, with the stall as its `cause` and in
   * `failure.diagnostics.decodeStall`.
   */
  async exportProject(request: ExportProjectRequest): Promise<string> {
    const problems = projectProblems(request.project);
    if (problems.length > 0) throw new Alpha2ProjectValidationError(problems);
    validateExportRequest(request);
    const release = await this.#slots.acquire(request.signal);
    const videoDecode = request.videoDecode ?? this.#videoDecode;
    try {
      const run = (mode: VideoDecodeMode | undefined) =>
        requestHost(
          this.#binary,
          "export",
          {
            project: request.project,
            assets: request.assets,
            proxies: proxiesPayload(request.proxies),
            frames: request.frames ?? {},
            resources: request.resources ?? {},
            media: request.media ?? "original",
            effectAssets: request.effectAssets ?? {},
            fonts: request.fonts ?? [],
            effects: request.effects ?? [],
            startTicks: request.startTicks,
            endTicks: request.endTicks,
            frameRate: request.frameRate,
            includeAudio: request.includeAudio ?? true,
            videoCodec: request.videoCodec ?? "h264",
            videoEncoder: request.videoEncoder,
            inputPolicy: request.inputPolicy ?? "auto",
            strategy: request.strategy ?? "render",
            videoQuality: request.videoQuality,
            audioCodec: request.audioCodec ?? "aac",
            audioQuality: request.audioQuality,
            loudness: request.loudness,
            renderCache: request.renderCache,
            ...(request.memory === undefined ? {} : { memory: request.memory }),
            output: request.output,
          },
          { videoDecode: mode },
          request.signal,
          request.onProgress,
        );
      let result: NativeExportResult;
      try {
        result = (await run(videoDecode)) as NativeExportResult;
      } catch (error) {
        // A hardware decoder stuck inside the driver cannot be recovered by its host, so the export
        // reruns in a new host decoding on the CPU. Only a stall with a hardware session (resource
        // "gpu") is retried: a software stall would recur, and `hardware-required` rules it out.
        if (
          !(error instanceof Alpha2HostError) ||
          error.code !== "DECODE_STALLED" ||
          error.failure?.resource !== "gpu" ||
          (videoDecode ?? process.env.ALPHA2_VIDEO_DECODE) ===
            "hardware-required"
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
    } finally {
      release();
    }
  }
}

/**
 * `retryError`, the failure of the software run a hardware decode stall led to, with the stall
 * kept: as its `cause` unless it has one, and in its failure's diagnostics as `decodeStall`, which
 * crosses process boundaries with the failure. The retry's failure stays the export's: a retry
 * policy judges what the last run did.
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

/**
 * What the host's engine would refuse in `project`, found before a host starts: the shape the SDK
 * validator checks, and the size the engine checks in the text it is sent
 * (`MAX_PROJECT_JSON_BYTES`), which an object cannot show.
 */
function projectProblems(project: Project): string[] {
  // `undefined` and the like have no text; the object validator says what they lack.
  const json = JSON.stringify(project) as string | undefined;
  return json === undefined
    ? validateProject(project)
    : validateProjectJson(json);
}

const convenienceClients = new Map<string, Alpha2NodeClient>();

/**
 * {@link Alpha2NodeClient.getCapabilities} on a client shared by every module-level call with the
 * same `concurrency` and resolved binary, for `options.videoDecode`.
 */
export async function getCapabilities(
  options: Alpha2NodeOptions & CapabilitiesOptions = {},
): Promise<HostCapabilities> {
  return sharedClient(options).getCapabilities({
    ...(options.signal === undefined ? {} : { signal: options.signal }),
    ...(options.videoDecode === undefined
      ? {}
      : { videoDecode: options.videoDecode }),
  });
}

/**
 * {@link Alpha2NodeClient.exportProject} on a client shared by every module-level call with the
 * same `concurrency` and resolved binary, so `concurrency` holds across calls whatever decode mode
 * each asks for. `options.videoDecode` applies where the request names none.
 */
export async function exportProject(
  request: ExportProjectRequest,
  options: Alpha2NodeOptions = {},
): Promise<string> {
  return sharedClient(options).exportProject(
    request.videoDecode === undefined && options.videoDecode !== undefined
      ? { ...request, videoDecode: options.videoDecode }
      : request,
  );
}

/**
 * The pool is the host binary and `concurrency`: the decode mode travels with each request, so
 * calls that differ only in it still share the bound.
 */
function sharedClient(options: Alpha2NodeOptions): Alpha2NodeClient {
  const concurrency = options.concurrency ?? 1;
  if (!Number.isSafeInteger(concurrency) || concurrency < 1) {
    throw new RangeError("concurrency must be a positive integer");
  }
  checkVideoDecode("videoDecode", options.videoDecode);
  const binary = resolveHostBinary(options);
  const key = `${concurrency}\0${binary}`;
  let client = convenienceClients.get(key);
  if (client === undefined) {
    client = new Alpha2NodeClient({ hostPath: binary, concurrency });
    convenienceClients.set(key, client);
  }
  return client;
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
  // Media usability is the host's conform check, which answers with a structured report.
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
  if (typeof request.output !== "string" || request.output.length === 0) {
    throw new Alpha2HostError("INVALID_INPUT", "output must not be empty");
  }
  checkVideoDecode("videoDecode", request.videoDecode);
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

/**
 * Refuses `assets`, `proxies` and `frames` not shaped as the host reads them (paths per source id)
 * before a host starts; the host would refuse them as `INVALID_REQUEST`.
 */
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

/** Refuses a `field` that is not a record of non-empty path strings. */
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

/** The host's `loudness` checks, with its messages. */
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

/**
 * Rejects what the host would reject, with the same field names, before a process is started. The
 * host checks again; this only moves the error earlier.
 */
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

async function requestHost(
  binary: string,
  operation: "capabilities" | "export" | "conform",
  payload: Readonly<Record<string, unknown>>,
  settings: { readonly videoDecode?: VideoDecodeMode },
  signal?: AbortSignal,
  onProgress?: ExportProgressCallback,
): Promise<unknown> {
  if (signal?.aborted) throw new Alpha2AbortError();
  // Windows Node starts a batch file only through a shell, which this never asks for: the spawn
  // would fail with EINVAL, a start that no retry mends.
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
      {
        cause,
      },
    );
  });
  const requestId = `node-${process.pid}-${Date.now()}-${nextRequestSequence++}`;
  // The request becomes its line before a host starts: a value JSON cannot carry (a BigInt, a
  // cycle, a throwing `toJSON`) refuses the request and starts nothing.
  let requestLine: string;
  try {
    requestLine = `${JSON.stringify(createRequest(operation, requestId, payload))}\n`;
  } catch (cause) {
    throw new Alpha2HostError(
      "INVALID_INPUT",
      `the ${operation} request cannot be written as JSON: ${cause instanceof Error ? cause.message : String(cause)}`,
      { cause },
    );
  }
  // The host would make its GPU device before refusing a line this long, unread.
  const lineBytes = Buffer.byteLength(requestLine) - 1;
  if (lineBytes > MAX_REQUEST_LINE_BYTES)
    throw new Alpha2HostError(
      "INVALID_INPUT",
      `the ${operation} request is ${lineBytes} bytes of JSON; the host reads at most ${MAX_REQUEST_LINE_BYTES}`,
    );
  return new Promise((resolvePromise, rejectPromise) => {
    const startFailed = (cause: unknown): Alpha2HostError =>
      new Alpha2HostError(
        "HOST_START_FAILED",
        `could not start alpha2-host: ${String(cause)}`,
        { cause },
      );
    let child: ChildProcess;
    try {
      child = spawn(binary, ["serve"], {
        stdio: ["pipe", "pipe", "pipe"],
        windowsHide: true,
        // The host chooses its decode path while it creates the renderer device, before any request
        // arrives, so the mode travels in the environment rather than in the request.
        env:
          settings.videoDecode === undefined
            ? process.env
            : { ...process.env, ALPHA2_VIDEO_DECODE: settings.videoDecode },
      });
    } catch (cause) {
      rejectPromise(startFailed(cause));
      return;
    }
    let settled = false;
    let outcome: HostOutcome | undefined;
    let forceKillTimer: NodeJS.Timeout | undefined;
    let writeFailedTimer: NodeJS.Timeout | undefined;
    // The last bytes of the host's standard error, cut to a character when read.
    let stderr = Buffer.alloc(0);
    let lastPhase: ProgressPhase | undefined;
    let reader: Interface | undefined;
    let requestPipe: Writable | undefined;
    // What an async onProgress returned and has not settled: the request settles after them, so a
    // callback never runs past its promise.
    const pendingCallbacks = new Set<Promise<void>>();
    let callbackFailure: Error | undefined;
    const stderrTail = (): string => utf8Tail(stderr, STDERR_TAIL_BYTES);
    const phaseNow = (): ExportPhase =>
      lastPhase === "finalizing"
        ? "mux"
        : lastPhase === "frames"
          ? "render"
          : "prepare";
    const finish = (nextOutcome: HostOutcome): void => {
      if (settled || outcome !== undefined) return;
      outcome = nextOutcome;
      signal?.removeEventListener("abort", abort);
      reader?.close();
      requestPipe?.end();
      if (child.exitCode === null && child.signalCode === null)
        child.kill("SIGTERM");
      forceKillTimer = setTimeout(() => {
        if (child.exitCode === null && child.signalCode === null)
          child.kill("SIGKILL");
      }, HOST_SHUTDOWN_TIMEOUT_MS);
    };
    const fail = (error: Error): void => finish({ ok: false, error });
    // A host error becomes the call's error once the host has ended: stderr written before the
    // answer may be read after it (often on macOS).
    const failFromHost = (error: ProtocolError): void => {
      const phase = phaseNow();
      finish({
        ok: false,
        error: () => errorFromHost(error, stderrTail(), phase),
      });
    };
    const abort = (): void => fail(new Alpha2AbortError());
    const callbackFailed = (error: Error): void => {
      callbackFailure ??= error;
      fail(error);
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
              { cause, phase: phaseNow() },
            ),
          ),
      );
      pendingCallbacks.add(pending);
      void pending.then(() => pendingCallbacks.delete(pending));
    };
    const conclude = (code: number | null, exitSignal: string | null): void => {
      if (outcome?.ok === true && callbackFailure !== undefined) {
        rejectPromise(callbackFailure);
      } else if (outcome?.ok === true) {
        resolvePromise(outcome.result);
      } else if (outcome?.ok === false) {
        rejectPromise(
          typeof outcome.error === "function" ? outcome.error() : outcome.error,
        );
      } else {
        const tail = stderrTail();
        const ended = `exit ${exitSignal ?? code ?? "unknown"}`;
        const detail = tail.trim() || ended;
        if (code === 70 || detail.includes("GPU_INIT_FAILED")) {
          rejectPromise(
            new Alpha2GpuInitializationError(detail, {
              stderr: tail,
              exitCode: code,
              signal: exitSignal,
            }),
          );
        } else if (loaderRefused(code, tail)) {
          // The loader refused the executable before it ran (a missing or mismatched FFmpeg):
          // the same host on the same machine fails the same way, so no retry is asked for.
          rejectPromise(
            new Alpha2HostError(
              "HOST_LOAD_FAILED",
              `alpha2-host could not be loaded: ${detail}`,
              { stderr: tail, exitCode: code, signal: exitSignal },
            ),
          );
        } else {
          // Nothing the host said names a cause, so none is claimed: a SIGKILL is as consistent
          // with an external kill as with the OOM killer.
          const failure: ExportFailure = {
            code: "HOST_EXITED",
            message: `alpha2-host exited before responding (${ended})`,
            origin: "node",
            operation: "export",
            backend: "native",
            phase: phaseNow(),
            resource: "unknown",
            stderr: tail,
            exitCode: code,
            signal: exitSignal,
          };
          rejectPromise(
            new Alpha2HostError(
              "HOST_EXITED",
              `alpha2-host exited before responding: ${detail}`,
              {
                failure,
                stderr: tail,
                exitCode: code,
                signal: exitSignal,
              },
            ),
          );
        }
      }
    };
    // A spawn without its standard streams (EMFILE, ENFILE) has no host to end and may never close:
    // it settles as it fails.
    const failWithoutHost = (error: Error): void => {
      if (settled) return;
      settled = true;
      if (child.pid !== undefined) child.kill("SIGKILL");
      rejectPromise(error);
    };
    const { stdin, stdout, stderr: errorPipe } = child;
    const streamsMissing = !stdin || !stdout || !errorPipe;
    // Before anything else: a spawn that fails reports it as this event, which would otherwise end
    // this process unheard.
    child.on("error", (cause) => {
      if (streamsMissing) failWithoutHost(startFailed(cause));
      else fail(startFailed(cause));
    });
    child.on("close", (code, exitSignal) => {
      if (settled) return;
      settled = true;
      signal?.removeEventListener("abort", abort);
      reader?.close();
      if (forceKillTimer !== undefined) clearTimeout(forceKillTimer);
      if (writeFailedTimer !== undefined) clearTimeout(writeFailedTimer);
      if (pendingCallbacks.size === 0) conclude(code, exitSignal);
      else
        void Promise.allSettled(pendingCallbacks).then(() =>
          conclude(code, exitSignal),
        );
    });
    if (streamsMissing) {
      // Node reports the cause on the next tick; this names the failure if it does not.
      setImmediate(() =>
        failWithoutHost(
          startFailed("its standard streams could not be created"),
        ),
      );
      return;
    }
    try {
      requestPipe = stdin;
      // A pipe fails when the host goes away; its stream `error` event would end this process
      // unheard, and the host's own report or exit is the outcome, so it is only noted.
      const ignore = (): void => {};
      stdin.on("error", ignore);
      stdout.on("error", ignore);
      errorPipe.on("error", ignore);
      errorPipe.on("data", (chunk: Buffer) => {
        stderr = Buffer.concat([stderr, chunk]);
        if (stderr.length > STDERR_TAIL_BYTES)
          stderr = stderr.subarray(stderr.length - STDERR_TAIL_BYTES);
      });
      reader = createInterface({ input: stdout });
      reader.on("line", (line) => {
        // Every message is a JSON object. A line that does not start as one is a library's the host
        // loaded (a Vulkan validation layer prints to stdout): diagnostic output, kept with stderr.
        if (!line.startsWith("{")) {
          stderr = Buffer.concat([stderr, Buffer.from(`${line}\n`)]);
          if (stderr.length > STDERR_TAIL_BYTES)
            stderr = stderr.subarray(stderr.length - STDERR_TAIL_BYTES);
          return;
        }
        let message: HostMessage;
        try {
          message = parseHostMessage(line);
        } catch (cause) {
          fail(
            new Alpha2HostError(
              "INVALID_RESPONSE",
              "alpha2-host returned invalid JSON",
              { cause, phase: phaseNow() },
            ),
          );
          return;
        }
        if ("type" in message) {
          if (message.type === "fatal") {
            failFromHost(message.error);
            return;
          }
          if (message.requestId === requestId) {
            lastPhase = message.phase ?? "frames";
            notifyProgress(
              message,
              onProgress,
              callbackFailed,
              track,
              phaseNow(),
            );
          }
          return;
        }
        // A process serves this one request, so an answer to a line the host could not read an id
        // from is its answer.
        if (
          message.requestId !== requestId &&
          message.requestId !== UNREAD_REQUEST_ID
        )
          return;
        if (message.ok) finish({ ok: true, result: message.result });
        else failFromHost(message.error);
      });
      if (signal?.aborted) {
        abort();
        return;
      }
      signal?.addEventListener("abort", abort, { once: true });
      stdin.write(requestLine, (error) => {
        if (!error) {
          stdin.end();
          return;
        }
        // The host's own report (a fatal line) and its exit arrive after this: they name the cause.
        // Only a host that stays up without taking its request fails as unwritten.
        writeFailedTimer = setTimeout(() => {
          fail(
            new Alpha2HostError("HOST_WRITE_FAILED", error.message, {
              cause: error,
            }),
          );
        }, HOST_SHUTDOWN_TIMEOUT_MS);
      });
    } catch (cause) {
      fail(startFailed(cause));
    }
  });
}

function isThenable(value: unknown): value is PromiseLike<unknown> {
  return (
    (typeof value === "object" || typeof value === "function") &&
    value !== null &&
    typeof (value as { then?: unknown }).then === "function"
  );
}

/**
 * Calls `callback` with a progress report. A throw fails the request; a promise it returns is
 * `track`ed, whose rejection fails it too.
 */
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

/**
 * The error the host reported, with its failure report, or, when it sent none (a request it could
 * not read, a start that failed), the failure its code is evidence of at `phase`.
 */
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
  return new Alpha2HostError(error.code, error.message, {
    failure,
    stderr,
    phase,
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

type Release = () => void;
/** How a request ended: its result, or its error, or what makes the error once the host has. */
type HostOutcome =
  | { readonly ok: true; readonly result: unknown }
  | { readonly ok: false; readonly error: Error | (() => Error) };

/**
 * The last at most `limit` bytes of `bytes` as text, from the first whole character: a byte bound,
 * as {@link STDERR_TAIL_BYTES} says, for text that is not ASCII too.
 */
function utf8Tail(bytes: Buffer, limit: number): string {
  let start = Math.max(0, bytes.length - limit);
  // A continuation byte (10xxxxxx) is the middle of a character.
  while (start < bytes.length && ((bytes[start] ?? 0) & 0xc0) === 0x80)
    start += 1;
  const text = bytes.subarray(start).toString("utf8");
  // A byte that is not UTF-8 becomes U+FFFD, three bytes: drop characters from the front to fit.
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

/** The longest request line the host reads (`MAX_REQUEST_LINE_BYTES`), newline excluded. */
const MAX_REQUEST_LINE_BYTES = 2 * MAX_PROJECT_JSON_BYTES;

/** The `requestId` the host answers a line with when it could not read the line's own. */
const UNREAD_REQUEST_ID = "unknown";

interface Waiter {
  readonly resolve: (release: Release) => void;
  readonly reject: (error: unknown) => void;
  readonly signal?: AbortSignal;
  abort?: () => void;
}

class Semaphore {
  #available: number;
  readonly #waiting: Waiter[] = [];

  constructor(limit: number) {
    this.#available = limit;
  }

  acquire(signal?: AbortSignal): Promise<Release> {
    if (signal?.aborted) return Promise.reject(new Alpha2AbortError());
    if (this.#available > 0) {
      this.#available -= 1;
      return Promise.resolve(this.#release());
    }
    return new Promise((resolveWaiter, reject) => {
      const waiter: Waiter = {
        resolve: resolveWaiter,
        reject,
        signal,
        abort: undefined,
      };
      const abort = (): void => {
        const index = this.#waiting.indexOf(waiter);
        if (index >= 0) this.#waiting.splice(index, 1);
        reject(new Alpha2AbortError());
      };
      waiter.abort = abort;
      this.#waiting.push(waiter);
      signal?.addEventListener("abort", abort, { once: true });
      if (signal?.aborted) abort();
    });
  }

  #release(): Release {
    let released = false;
    return () => {
      if (released) return;
      released = true;
      const waiter = this.#waiting.shift();
      if (waiter) {
        if (waiter.abort)
          waiter.signal?.removeEventListener("abort", waiter.abort);
        waiter.resolve(this.#release());
      } else {
        this.#available += 1;
      }
    };
  }
}

let nextRequestSequence = 0;

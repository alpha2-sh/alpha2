// Frame keys come only from the engine's project snapshot; hosts must not derive them (for example
// by rounding `timelineSeconds - clip.start`).

import type { FrameUploadReceipt } from "./effects.js";
import type { ColorMetadata } from "./generated/project-schema.js";
import {
  type ChromaSiting,
  isHdrVideoFrame,
  isPlanarVideoFrame,
  decodeRefusalMessage,
  NonPlanarVideoFrameError,
  PlanarCopyPool,
  PlanarDecodeUnavailableError,
  type PlanarFrameIdentity,
  type PlanarYuvEngine,
  type PlanarYuvFrame,
  uploadPlanarYuvFrame,
  type VideoConversion,
} from "./hdr-frames.js";
import { decoderConsumerId } from "./media.js";
import {
  type PresentedFrameEngine,
  sendsBitmaps,
  uploadPresentedFrame,
} from "./presented-frames.js";
import {
  PhysicalFrameAliases,
  type PhysicalFrameIdentity,
} from "./physical-frames.js";
import {
  intakeVideoFrame,
  readbackCounters,
  type ReadbackRecoveryStats,
  readPlanesWithRetries,
} from "./frame-intake.js";

export type { ReadbackRecoveryStats } from "./frame-intake.js";

const TICKS_PER_SECOND = 120_000;

const DEFAULT_MAX_RETIRED_FRAME_BYTES = 256 * 1024 * 1024;
const DEFAULT_MAX_CACHED_IMAGE_BYTES = 256 * 1024 * 1024;
const DEFAULT_MAX_DECODE_CONCURRENCY = 2;

/** One decode request the engine reports for a single timeline timestamp (`frameNeeds`). */
export interface FrameNeed {
  /** Overlapping clips may report the same source. */
  readonly clipId: string;
  /**
   * Composition step reading the source; `null` or absent when the clip reads it directly. Each
   * reader has its own decoder cursor.
   */
  readonly readerId?: string | null;
  readonly sourceId: string;
  /** Decimal tick string, never a rounded number. */
  readonly mediaTimeTicks: string;
}

/** A timeline source as the host knows it. */
export interface FrameSupplyLayer {
  readonly id: string;
  readonly kind: "video" | "image" | "audio";
}

/** A decoded frame the engine copies without a CPU round trip. */
export interface FrameSupplyVideoFrame {
  readonly kind: "video-frame";
  readonly frame: VideoFrame;
  /** The decoded frame's own timestamp, for wrappers that reset `frame.timestamp`. */
  readonly physicalTimeMicroseconds?: number;
  /** Clockwise container display rotation, applied after {@link mirror}. */
  readonly rotation?: 0 | 90 | 180 | 270;
  /** Mirror left to right before rotating. */
  readonly mirror?: boolean;
  /** WebCodecs does not report it; see `chromaSitingForStream`. */
  readonly chromaSiting?: ChromaSiting;
}

/**
 * A video frame read back to RGBA, for engines that cannot take a `VideoFrame`. Its identity lets
 * requests for the same decoded frame share one upload.
 */
export interface FrameSupplyRgbaFrame {
  readonly kind: "rgba-frame";
  /** RGBA8 at the output size, rows packed. */
  readonly pixels: Uint8Array;
  /** Safe integer. */
  readonly physicalTimeMicroseconds: number;
  /** Safe integer. */
  readonly durationMicroseconds?: number;
  /** Display size before scaling to the output. */
  readonly sourceWidth: number;
  readonly sourceHeight: number;
  readonly rotation?: 0 | 90 | 180 | 270;
}

/**
 * An extension provider frame at its own size (SVG, animated image, Lottie, OpenEXR, image
 * sequence), which the engine scales into the output.
 */
export type FrameSupplyRasterFrame = {
  readonly kind: "raster-frame";
  readonly width: number;
  readonly height: number;
  /** First source time it shows at, in microseconds; serves as its identity. */
  readonly physicalTimeMicroseconds: number;
  readonly durationMicroseconds?: number;
} & (
  | {
      readonly precision?: "rgba8";
      /** sRGB RGBA8, rows packed. */
      readonly pixels: Uint8Array;
      /** Default `"premultiplied"`; decoded images are usually `"straight"`. */
      readonly alpha?: "premultiplied" | "straight";
    }
  | {
      readonly precision: "linear_f16";
      /**
       * Premultiplied linear-light RGBA half-float bits in BT.709 primaries, rows packed. Values
       * outside 0 to 1 are kept.
       */
      readonly pixels: Uint16Array;
    }
);

/**
 * What `pixelsFor` resolves with. Bare RGBA8 pixels at the output size are only for still images.
 */
export type FrameSupplySource =
  | Uint8Array
  | FrameSupplyVideoFrame
  | FrameSupplyRgbaFrame
  | FrameSupplyRasterFrame;

/** Decode hints for one `pixelsFor` call. */
export interface FrameSupplyDecodeOptions {
  /** Whether the engine can consume a `VideoFrame` without a readback. */
  readonly preferVideoFrame: boolean;
  /** Decoder cursor owner; see {@link decoderCursorKey}. */
  readonly consumerId: string;
  /**
   * The source the engine asked for, which may differ from the layer's (a proxy). Key per-source
   * host state by this, as `liveSourceIds` does.
   */
  readonly sourceId: string;
  /**
   * `"software"` requests a CPU decode on a separate cursor, resolving with a `VideoFrame`. Used
   * after a hardware frame could not be read back, or when a frame the engine must convert has no
   * planar format (then for every later frame of the source). Reject if software decode is
   * unavailable.
   */
  readonly decode?: "software";
}

/** Host callbacks and frame geometry for one `supply` call. */
export interface FrameSupplyOptions {
  /** Output size; RGBA pixels must match it. */
  readonly width: number;
  readonly height: number;
  /**
   * A value that changes (by `Object.is`) exactly when a source's pixels do, such as new media,
   * provider settings or a proxy swap. A change drops that source's uploads here and in the engine.
   * Without it, a source's pixels are assumed fixed while it stays in the project.
   */
  readonly contentOf?: (sourceId: string) => unknown;
  /** Sources still in the project; others have their uploads retired. */
  readonly liveSourceIds: ReadonlySet<string>;
  // `supply` destructures these, so bind object methods before passing them.
  /** A missing layer makes `supply` reject. */
  readonly getLayer: (sourceId: string) => FrameSupplyLayer | null | undefined;
  /**
   * Decodes `layer` at `seconds` of source time, or resolves `null` when there is no frame. The
   * supply takes ownership of a returned `VideoFrame`; a rejection rejects `supply`.
   */
  readonly pixelsFor: (
    layer: FrameSupplyLayer,
    seconds: number,
    options: FrameSupplyDecodeOptions,
  ) => Promise<FrameSupplySource | null>;
  /** Opens or retains one decoder cursor per consumer before decoding starts. */
  readonly retainVideoConsumers?: (
    consumerIds: ReadonlySet<string>,
  ) => Promise<void> | void;
  /** Returns false once an edit, seek or pause invalidates the request; checked per decode. */
  readonly isCurrent: () => boolean;
}

/** Memory envelope for source textures whose render submission has completed. */
export interface FrameRetentionOptions {
  /**
   * Rendered texture bytes kept for reuse. Defaults to 256 MiB; the newest generation is kept even
   * if it alone exceeds this.
   */
  readonly maxRetiredBytes?: number;
}

/** Resource and concurrency limits owned by one frame supplier. */
export interface FrameSupplyConfig extends FrameRetentionOptions {
  /** Cached still bytes, evicted least recently used first. Defaults to 256 MiB. */
  readonly maxCachedImageBytes?: number;
  /** Maximum concurrent decoder cursors. Defaults to 2. */
  readonly maxDecodeConcurrency?: number;
  /** See {@link VideoConversion}. Defaults to `"engine"`. */
  readonly videoConversion?: VideoConversion;
  /**
   * Fallback for a frame whose planes still cannot be read back after retries. `"redecode"`
   * (default) decodes it again in software; `"present"` hands it to the browser's conversion
   * unless the engine must convert it, trading accuracy for preview latency. Required frames with
   * no planar format always redecode.
   */
  readonly unreadableFrames?: "redecode" | "present";
  /**
   * Handling of a source the engine must convert (HDR or declared color) when no decoder gives
   * planes. `"refuse"` (default) rejects with `PlanarDecodeUnavailableError`. `"present-degraded"`
   * shows the browser's conversion via `uploadDegradedVideoFrame` and lists the source in
   * `degradedSources`; the engine then refuses to export such frames. Exports should keep
   * `"refuse"`.
   */
  readonly opaqueEngineFrames?: "refuse" | "present-degraded";
}

interface DecodedNeed {
  readonly need: FrameNeed;
  readonly layer: FrameSupplyLayer;
  /** `null` once handed to the engine. */
  readonly source: FrameSupplySource | null;
  /**
   * Set for a `VideoFrame` the engine converts from planes, with the declared color as JSON. The
   * planes are copied only if no earlier upload serves the same picture.
   */
  readonly planar?: {
    readonly declared: string | undefined;
    /** HDR or project-declared color. */
    readonly required: boolean;
  };
  /** Presented through the browser's conversion (degraded source). */
  readonly degraded?: true;
}

/**
 * Keeps rendered upload generations within a byte budget, releasing the oldest through `release`.
 *
 * The constructor throws `RangeError` when `maxRetiredBytes` is not a positive safe integer.
 */
export class FrameGenerationRetainer {
  readonly #release: (generation: bigint) => void;
  readonly #maxRetiredBytes: number;
  #retired: FrameUploadReceipt[] = [];
  #retiredBytes = 0;

  constructor(
    release: (generation: bigint) => void,
    options: FrameRetentionOptions = {},
  ) {
    const maxRetiredBytes =
      options.maxRetiredBytes ?? DEFAULT_MAX_RETIRED_FRAME_BYTES;
    validatePositiveInteger(maxRetiredBytes, "maxRetiredBytes");
    this.#release = release;
    this.#maxRetiredBytes = maxRetiredBytes;
  }

  get retainedBytes(): number {
    return this.#retiredBytes;
  }

  /**
   * Takes over rendered generations, releasing older ones past the budget.
   *
   * @throws RangeError for a receipt with a non-positive generation or byte count.
   */
  retire(uploads: readonly FrameUploadReceipt[]): void {
    for (const upload of uploads) {
      validateUploadReceipt(upload);
      this.#retired.push(upload);
      this.#retiredBytes += upload.retainedBytes;
      // Keep the newest generation even over budget so a large frame cannot block progress.
      while (
        this.#retired.length > 1 &&
        this.#retiredBytes > this.#maxRetiredBytes
      ) {
        const retired = this.#retired.shift();
        if (retired !== undefined) {
          this.#retiredBytes -= retired.retainedBytes;
          this.#release(retired.generation);
        }
      }
    }
  }

  /** Forgets held generations without releasing them, after the engine dropped its frame store. */
  discardAll(): void {
    this.#retired = [];
    this.#retiredBytes = 0;
  }
}

/** One upload reported through {@link FrameSupply.onUpload}. */
export interface FrameUploadTrace {
  readonly sourceId: string;
  readonly mediaTimeTicks: string;
  /** Sparse 32-bit checksum of the uploaded RGBA pixels. */
  readonly checksum: number;
}

/** Uploads accepted by the engine for one timestamp. */
export interface SuppliedFrame {
  /** Every need, including those served by an existing upload. */
  readonly needs: readonly FrameNeed[];
  /** Pass to `retire` once rendered. */
  readonly uploaded: readonly FrameUploadReceipt[];
}

/** Outcome of one real-time presentation attempt. */
export interface PlaybackResult {
  readonly requestedTimeTicks: number;
  /** `null` until the engine has presented a frame, and after a reset. */
  readonly presentedTimeTicks: number | null;
  readonly missingSources: readonly string[];
  /**
   * Sources shown through the browser's conversion (`opaqueEngineFrames: "present-degraded"`); the
   * engine refuses to export them.
   */
  readonly degradedSources?: readonly string[];
  readonly repeated: boolean;
  /** The frame matched the last one, so nothing was rendered and the canvas is unchanged. */
  readonly unchanged: boolean;
}

/**
 * Engine surface this module drives, satisfied by the wasm renderer or a forwarding proxy. Optional
 * methods are probed per call; a source that needs a missing one makes `supply` reject.
 */
export interface FrameSupplyEngine {
  readonly deviceLost: boolean;
  readonly supportsExternalVideoFrame: boolean;
  frameNeeds(timeTicks: bigint): string;
  uploadRgbaImage(
    sourceId: string,
    width: number,
    height: number,
    pixels: Uint8Array,
  ): FrameUploadReceipt;
  uploadRgbaFrame(
    sourceId: string,
    mediaTimeTicks: bigint,
    width: number,
    height: number,
    pixels: Uint8Array,
  ): FrameUploadReceipt;
  uploadPresentedVideoFrame?(
    sourceId: string,
    mediaTimeTicks: bigint,
    frame: VideoFrame,
    width: number,
    height: number,
    rotation: number | undefined,
    mirror: boolean,
  ): FrameUploadReceipt;
  readonly videoFrameCopy?: string;
  uploadPresentedImageBitmap?: PresentedFrameEngine["uploadPresentedImageBitmap"];
  /** Presents a frame through the browser's conversion and marks its source degraded. */
  uploadDegradedVideoFrame?(
    sourceId: string,
    mediaTimeTicks: bigint,
    frame: VideoFrame,
    width: number,
    height: number,
    rotation: number,
    mirror: boolean,
  ): FrameUploadReceipt;
  /** Converts planar YUV with the project's color pipeline; used for all HDR frames. */
  uploadYuvFrame?: PlanarYuvEngine["uploadYuvFrame"];
  /** Presents a premultiplied or straight RGBA8 frame at its own size into the output. */
  uploadPresentedRgbaFrame?: (
    sourceId: string,
    mediaTimeTicks: bigint,
    width: number,
    height: number,
    pixels: Uint8Array,
    premultiplied: boolean,
    outputWidth: number,
    outputHeight: number,
  ) => FrameUploadReceipt;
  /** Like `uploadPresentedRgbaFrame`, for a still shown at every time. */
  uploadPresentedRgbaImage?: (
    sourceId: string,
    width: number,
    height: number,
    pixels: Uint8Array,
    premultiplied: boolean,
    outputWidth: number,
    outputHeight: number,
  ) => FrameUploadReceipt;
  /** Presents a premultiplied linear-light RGBA half-float frame at its own size. */
  uploadPresentedLinearFrame?: (
    sourceId: string,
    mediaTimeTicks: bigint,
    width: number,
    height: number,
    pixels: Uint16Array,
    outputWidth: number,
    outputHeight: number,
  ) => FrameUploadReceipt;
  /** Like `uploadPresentedLinearFrame`, for a still shown at every time. */
  uploadPresentedLinearImage?: (
    sourceId: string,
    width: number,
    height: number,
    pixels: Uint16Array,
    outputWidth: number,
    outputHeight: number,
  ) => FrameUploadReceipt;
  /** Whether the loaded project declares `sourceId` HDR. */
  hdrSource?(sourceId: string): boolean;
  /** Project-declared color for `sourceId`, as JSON. */
  sourceColorMetadata?(sourceId: string): string | undefined;
  aliasVideoFrame(
    sourceId: string,
    existingMediaTimeTicks: bigint,
    requestedMediaTimeTicks: bigint,
  ): boolean;
  /** Whether the still for `sourceId` is still upload `generation`; the engine may evict stills. */
  holdsImage(sourceId: string, generation: bigint): boolean;
  releaseFrameGeneration(generation: bigint): void;
  /** Drops a source's frames and still after its media changed. */
  releaseSourceFrames(sourceId: string): void;
  renderAt(timeTicks: bigint): string;
}

/** The content epoch of a source as uploaded by this supply. */
interface SourceContent {
  readonly content: unknown;
  readonly epoch: number;
  /**
   * Set once a required frame had no planar format. The browser does this for every frame of a
   * source, so later frames go straight to software.
   */
  softwareDecode?: true;
  /** Set once no decoder gave planes; later frames fail immediately with this error. */
  refused?: PlanarDecodeUnavailableError;
  /** Set instead of `refused` under `opaqueEngineFrames: "present-degraded"`. */
  degraded?: true;
}

/** RGBA8 pixels at the output size, or a provider frame at its own size. */
type Still = Uint8Array | FrameSupplyRasterFrame;

interface CachedImage {
  readonly upload: FrameUploadReceipt;
  /**
   * RGBA8 pixels compared by array, or a provider raster compared by identity (providers return a
   * new array for the same picture).
   */
  readonly still: Still;
  readonly width: number;
  readonly height: number;
  lastUsed: bigint;
}

/**
 * Keys the decoder cursor of one clip reading one asset. Clips sharing a source need separate
 * cursors, or one decoder would seek back and forth every frame.
 */
export function decoderCursorKey(assetId: string, consumerId: string): string {
  return `${assetId}\u0000${consumerId}`;
}

/** Cumulative raw source video a {@link FrameSupply} copied through script memory. */
export interface SourceTransferStats {
  readonly planarCopyFrames: number;
  readonly planarCopyBytes: number;
}

/**
 * Reference host-side consumer of the engine's `frameNeeds` contract. It decodes each needed frame
 * through a clip-scoped cursor, uploads it, and bounds retention. Needs resolving to the same
 * decoded frame share one upload.
 *
 * The constructor throws `RangeError` for a limit that is not a positive safe integer.
 */
export class FrameSupply {
  readonly #engine: FrameSupplyEngine;
  readonly #retainer: FrameGenerationRetainer;
  readonly #maxCachedImageBytes: number;
  readonly #maxDecodeConcurrency: number;
  readonly #images = new Map<string, CachedImage>();
  readonly #physicalFrames = new PhysicalFrameAliases();
  readonly #decodeTails = new Map<string, Promise<void>>();
  #activeDecodes = 0;
  #decodeWaiters: Array<() => void> = [];
  #cachedImageBytes = 0;
  readonly #videoConversion: VideoConversion;
  readonly #unreadableFrames: "redecode" | "present";
  readonly #presentsDegraded: boolean;
  #planarCopyFrames = 0;
  #planarCopyBytes = 0;
  readonly #readback = readbackCounters();
  readonly #planarCopies = new PlanarCopyPool();
  #imageUse = 0n;
  readonly #contents = new Map<string, SourceContent>();
  #nextEpoch = 0;
  /**
   * Replaced or evicted still uploads, retired at the next {@link FrameSupply.retire}. Retiring
   * immediately could release a generation that an alias just served before the engine renders it.
   */
  #replacedStills: FrameUploadReceipt[] = [];

  /**
   * Optional RGBA upload trace for diagnosing a clip that shows another clip's picture. Two sources
   * with the same checksum point to a host decoding bug; distinct checksums point to the engine.
   */
  onUpload: ((entry: FrameUploadTrace) => void) | null = null;

  constructor(engine: FrameSupplyEngine, options: FrameSupplyConfig = {}) {
    const maxCachedImageBytes =
      options.maxCachedImageBytes ?? DEFAULT_MAX_CACHED_IMAGE_BYTES;
    const maxDecodeConcurrency =
      options.maxDecodeConcurrency ?? DEFAULT_MAX_DECODE_CONCURRENCY;
    validatePositiveInteger(maxCachedImageBytes, "maxCachedImageBytes");
    validatePositiveInteger(maxDecodeConcurrency, "maxDecodeConcurrency");
    this.#engine = engine;
    this.#maxCachedImageBytes = maxCachedImageBytes;
    this.#maxDecodeConcurrency = maxDecodeConcurrency;
    const videoConversion = options.videoConversion ?? "engine";
    if (videoConversion !== "engine" && videoConversion !== "browser")
      throw new RangeError('videoConversion must be "engine" or "browser"');
    this.#videoConversion = videoConversion;
    const unreadableFrames = options.unreadableFrames ?? "redecode";
    if (unreadableFrames !== "redecode" && unreadableFrames !== "present")
      throw new RangeError('unreadableFrames must be "redecode" or "present"');
    this.#unreadableFrames = unreadableFrames;
    const opaqueEngineFrames = options.opaqueEngineFrames ?? "refuse";
    if (
      opaqueEngineFrames !== "refuse" &&
      opaqueEngineFrames !== "present-degraded"
    )
      throw new RangeError(
        'opaqueEngineFrames must be "refuse" or "present-degraded"',
      );
    this.#presentsDegraded =
      opaqueEngineFrames === "present-degraded" &&
      typeof engine.uploadDegradedVideoFrame === "function";
    this.#retainer = new FrameGenerationRetainer(
      (generation) => this.#engine.releaseFrameGeneration(generation),
      options,
    );
  }

  /** See {@link FrameRetentionOptions}. */
  get retiredBytes(): number {
    return this.#retainer.retainedBytes;
  }

  get cachedImageBytes(): number {
    return this.#cachedImageBytes;
  }

  /** Planar copies made for engine conversion (see {@link VideoConversion}). */
  get sourceTransfer(): SourceTransferStats {
    return {
      planarCopyFrames: this.#planarCopyFrames,
      planarCopyBytes: this.#planarCopyBytes,
    };
  }

  /** Cumulative readback failures and how they were recovered. */
  get readbackRecovery(): ReadbackRecoveryStats {
    return { ...this.#readback };
  }

  #trace(sourceId: string, mediaTimeTicks: string, pixels: Uint8Array): void {
    if (!this.onUpload) return;
    // Sample sparsely: a full hash per frame would dominate export time.
    let checksum = 0;
    const stride = Math.max(4, (pixels.length >> 10) & ~3);
    for (let index = 0; index < pixels.length; index += stride) {
      checksum = (checksum * 31 + (pixels[index] ?? 0)) >>> 0;
    }
    this.onUpload({ sourceId, mediaTimeTicks, checksum });
  }

  /**
   * Uploads every source the engine needs at one timestamp.
   *
   * Resolves with upload receipts for the caller to pass to {@link FrameSupply.retire} after
   * rendering, or `null` when `isCurrent` turned false mid-decode (decoded frames are closed).
   *
   * Rejects with a fatal `GPU_DEVICE_LOST` error when the device is lost, and with an `Error` when
   * a source is unavailable, decodes to nothing or the wrong kind of frame, or an upload fails.
   * Earlier uploads are then retired and remaining frames closed.
   */
  async supply(
    timeTicks: bigint,
    {
      width,
      height,
      contentOf,
      liveSourceIds,
      getLayer,
      pixelsFor,
      retainVideoConsumers,
      isCurrent,
    }: FrameSupplyOptions,
  ): Promise<SuppliedFrame | null> {
    if (!isCurrent()) return null;
    if (this.#engine.deviceLost) {
      // Same code the binding uses, so the owner recreates the renderer.
      const error = new Error(
        "GPU device was lost; make the renderer again",
      ) as Error & { code: string; fatal: boolean };
      error.name = "Alpha2Error";
      error.code = "GPU_DEVICE_LOST";
      error.fatal = true;
      throw error;
    }
    const needs = JSON.parse(this.#engine.frameNeeds(timeTicks)) as FrameNeed[];
    await retainVideoConsumers?.(new Set(needs.map(consumerOf)));
    if (!isCurrent()) return null;
    for (const [id, entry] of this.#images) {
      if (!liveSourceIds.has(id)) {
        this.#removeImage(id, entry);
      }
    }
    for (const id of this.#contents.keys()) {
      if (!liveSourceIds.has(id)) this.#contents.delete(id);
    }
    const epochs = new Map<string, number>();
    for (const { sourceId } of needs) {
      if (!epochs.has(sourceId))
        epochs.set(sourceId, this.#contentEpoch(sourceId, contentOf));
    }
    const decoded = await this.#decodeNeeds(
      needs,
      getLayer,
      pixelsFor,
      isCurrent,
    );
    if (!isCurrent()) {
      closeDecodedNeeds(decoded);
      return null;
    }
    const uploaded: FrameUploadReceipt[] = [];
    const stills = new Map<string, Still>();
    let decodedIndex = 0;
    try {
      for (; decodedIndex < decoded.length; decodedIndex += 1) {
        const entry = decoded[decodedIndex];
        if (entry === undefined) continue;
        const { need, layer, source, planar } = entry;
        let degraded = entry.degraded === true;
        const sourceTicks = BigInt(need.mediaTimeTicks);
        const content = epochs.get(need.sourceId) ?? 0;
        let videoFrame: FrameSupplyVideoFrame | null = null;
        let rgbaFrame: FrameSupplyRgbaFrame | null = null;
        let rasterFrame: FrameSupplyRasterFrame | null = null;
        let pixels: Uint8Array | null = null;
        if (source !== null) {
          if ("kind" in source) {
            if (source.kind === "video-frame") videoFrame = source;
            else if (source.kind === "raster-frame") rasterFrame = source;
            else rgbaFrame = source;
          } else pixels = source;
        }
        if (rgbaFrame) pixels = rgbaFrame.pixels;
        if (pixels) this.#trace(need.sourceId, need.mediaTimeTicks, pixels);
        if (layer.kind === "image") {
          const still = rasterFrame ?? pixels;
          if (!still)
            throw new Error(`image source decoded no pixels: ${need.sourceId}`);
          stills.set(need.sourceId, still);
          this.#supplyImage(need.sourceId, still, width, height);
        } else if (rasterFrame) {
          const identity = rasterIdentity(
            need.sourceId,
            content,
            rasterFrame,
            width,
            height,
          );
          const exact = {
            sourceId: need.sourceId,
            mediaTimeTicks: sourceTicks,
          };
          if (
            this.#physicalFrames.alias(identity, exact, (existing, requested) =>
              this.#engine.aliasVideoFrame(
                requested.sourceId,
                existing.mediaTimeTicks,
                requested.mediaTimeTicks,
              ),
            )
          ) {
            continue;
          }
          const upload = this.#presenter().frame(
            need.sourceId,
            sourceTicks,
            rasterFrame,
            width,
            height,
          );
          validateUploadReceipt(upload);
          uploaded.push(upload);
          this.#physicalFrames.record(identity, exact);
        } else if (videoFrame) {
          const identity = videoIdentity(
            need.sourceId,
            content,
            videoFrame,
            width,
            height,
          );
          const exact = {
            sourceId: need.sourceId,
            mediaTimeTicks: sourceTicks,
          };
          if (
            this.#physicalFrames.alias(identity, exact, (existing, requested) =>
              this.#engine.aliasVideoFrame(
                requested.sourceId,
                existing.mediaTimeTicks,
                requested.mediaTimeTicks,
              ),
            )
          ) {
            videoFrame.frame.close();
            continue;
          }
          let upload: FrameUploadReceipt | undefined;
          if (planar !== undefined && !degraded) {
            // The copy owns and closes the frame unless it hands it back for a browser upload.
            decoded[decodedIndex] = { need, layer, source: null };
            const copied = await this.#copyPlanar(
              need,
              layer,
              videoFrame,
              planar,
              pixelsFor,
            );
            if (copied === undefined || copied === DEGRADED) {
              decoded[decodedIndex] = { need, layer, source: videoFrame };
              degraded = copied === DEGRADED;
            } else {
              try {
                upload = uploadPlanarYuvFrame(
                  this.#planarEngine(),
                  need.sourceId,
                  sourceTicks,
                  copied,
                  width,
                  height,
                );
              } finally {
                // The upload has already copied the planes into the engine.
                this.#planarCopies.release(copied.data);
              }
            }
          }
          if (upload === undefined && degraded) {
            const engine = this.#engine;
            if (engine.uploadDegradedVideoFrame === undefined)
              throw new Error("engine cannot present degraded frames");
            // The engine owns and closes the frame whether or not the upload succeeds.
            decoded[decodedIndex] = { need, layer, source: null };
            upload = engine.uploadDegradedVideoFrame(
              need.sourceId,
              sourceTicks,
              videoFrame.frame,
              width,
              height,
              videoFrame.rotation ?? 0,
              videoFrame.mirror ?? false,
            );
          }
          if (upload === undefined) {
            const engine = this.#presentedEngine();
            const presented = [
              need.sourceId,
              sourceTicks,
              videoFrame.frame,
              width,
              height,
              videoFrame.rotation ?? 0,
              videoFrame.mirror ?? false,
            ] as const;
            upload = sendsBitmaps(engine)
              ? await uploadPresentedFrame(engine, ...presented)
              : engine.uploadPresentedVideoFrame(...presented);
          }
          validateUploadReceipt(upload);
          uploaded.push(upload);
          this.#physicalFrames.record(identity, exact);
        } else {
          if (!pixels)
            throw new Error(`video source decoded no pixels: ${need.sourceId}`);
          if (!rgbaFrame) {
            throw new Error(
              `video source must preserve physical frame identity: ${need.sourceId}`,
            );
          }
          const identity = rgbaIdentity(
            need.sourceId,
            content,
            rgbaFrame,
            width,
            height,
          );
          const exact = {
            sourceId: need.sourceId,
            mediaTimeTicks: sourceTicks,
          };
          if (
            this.#physicalFrames.alias(identity, exact, (existing, requested) =>
              this.#engine.aliasVideoFrame(
                requested.sourceId,
                existing.mediaTimeTicks,
                requested.mediaTimeTicks,
              ),
            )
          ) {
            continue;
          }
          const upload = this.#engine.uploadRgbaFrame(
            need.sourceId,
            sourceTicks,
            width,
            height,
            pixels,
          );
          validateUploadReceipt(upload);
          uploaded.push(upload);
          this.#physicalFrames.record(identity, exact);
        }
      }
      this.#keepStillsHeld(stills, width, height);
    } catch (error) {
      closeDecodedNeeds(decoded.slice(decodedIndex));
      this.retire(uploaded);
      throw error;
    }
    this.#evictImages(stills);
    return { needs, uploaded };
  }

  async #decodeNeeds(
    needs: readonly FrameNeed[],
    getLayer: FrameSupplyOptions["getLayer"],
    pixelsFor: FrameSupplyOptions["pixelsFor"],
    isCurrent: FrameSupplyOptions["isCurrent"],
  ): Promise<DecodedNeed[]> {
    const groups = new Map<
      string,
      Array<{ index: number; need: FrameNeed; layer: FrameSupplyLayer }>
    >();
    const results = new Array<DecodedNeed | undefined>(needs.length);
    for (const [index, need] of needs.entries()) {
      const layer = getLayer(need.sourceId);
      if (!layer)
        throw new Error(`frame source is unavailable: ${need.sourceId}`);
      if (layer.kind === "audio") continue;
      const key = keyForNeed(need, layer);
      const group = groups.get(key) ?? [];
      group.push({ index, need, layer });
      groups.set(key, group);
    }
    const pending = [...groups.values()];
    let next = 0;
    let failure: unknown;
    let failed = false;
    const directVideo =
      this.#engine.supportsExternalVideoFrame &&
      typeof this.#engine.uploadPresentedVideoFrame === "function";
    const planar = typeof this.#engine.uploadYuvFrame === "function";
    const worker = async (): Promise<void> => {
      for (;;) {
        const group = pending[next++];
        if (group === undefined) return;
        for (const { index, need, layer } of group) {
          // A replaced request starts no further decodes.
          if (failed || !isCurrent()) return;
          try {
            const refused = this.#contents.get(need.sourceId)?.refused;
            if (refused !== undefined) throw refused;
            const sourceTicks = BigInt(need.mediaTimeTicks);
            const declared =
              planar && layer.kind === "video"
                ? this.#engine.sourceColorMetadata?.(need.sourceId)
                : undefined;
            const enginesColor =
              declared !== undefined ||
              (planar &&
                layer.kind === "video" &&
                this.#engine.hdrSource?.(need.sourceId) === true);
            const degraded =
              layer.kind === "video" &&
              this.#contents.get(need.sourceId)?.degraded === true;
            const software =
              !degraded &&
              layer.kind === "video" &&
              this.#contents.get(need.sourceId)?.softwareDecode === true;
            const decoded = await this.#decodeCursor(
              software
                ? `${keyForNeed(need, layer)}\u0000software`
                : keyForNeed(need, layer),
              () =>
                pixelsFor(layer, Number(sourceTicks) / TICKS_PER_SECOND, {
                  preferVideoFrame: directVideo || enginesColor,
                  consumerId: consumerOf(need),
                  sourceId: need.sourceId,
                  ...(software ? { decode: "software" as const } : {}),
                }),
            );
            results[index] =
              degraded &&
              decoded !== null &&
              "kind" in decoded &&
              decoded.kind === "video-frame"
                ? { need, layer, source: decoded, degraded: true }
                : this.#planarRoute(
                    need,
                    layer,
                    decoded,
                    enginesColor,
                    declared,
                    planar,
                  );
          } catch (error) {
            failure = error;
            failed = true;
            return;
          }
        }
      }
    };
    await Promise.all(
      Array.from(
        { length: Math.min(this.#maxDecodeConcurrency, pending.length) },
        worker,
      ),
    );
    const decoded = results.filter(
      (entry): entry is DecodedNeed => entry !== undefined,
    );
    if (failed) {
      closeDecodedNeeds(decoded);
      throw failure;
    }
    return decoded;
  }

  /**
   * Routes a decoded `VideoFrame` to the engine's planar conversion: HDR frames, sources with
   * declared color (`enginesColor`), and other planar frames unless SDR is left to the browser.
   * Such sources must decode to a `VideoFrame`; an RGBA readback is browser-converted.
   * Planes are copied only at upload, in case an earlier upload serves the picture.
   */
  #planarRoute(
    need: FrameNeed,
    layer: FrameSupplyLayer,
    source: FrameSupplySource | null,
    enginesColor: boolean,
    declared: string | undefined,
    planar: boolean,
  ): DecodedNeed {
    const isVideoFrame =
      source !== null && "kind" in source && source.kind === "video-frame";
    if (!isVideoFrame) {
      if (enginesColor && source !== null) {
        throw new Error(
          `source ${need.sourceId} must decode to a VideoFrame, not RGBA: the engine converts its color, which the project declares or which is HDR`,
        );
      }
      return { need, layer, source };
    }
    if (
      !planar ||
      !(
        enginesColor ||
        isHdrVideoFrame(source.frame) ||
        (this.#videoConversion === "engine" && isPlanarVideoFrame(source.frame))
      )
    )
      return { need, layer, source };
    return {
      need,
      layer,
      source,
      planar: {
        declared,
        required: enginesColor || isHdrVideoFrame(source.frame),
      },
    };
  }

  /**
   * Copies a frame out planar for the engine and closes it.
   *
   * Unreadable frames are retried, then decoded again in software (decoding is exact, so the planes
   * match). With `unreadableFrames: "present"`, a frame the engine need not convert resolves
   * `undefined` and stays open for the browser. Non-planar frames go straight to software, as do
   * later frames of the source. Rejects, closing the frame, when the copy or software decode fails.
   */
  async #copyPlanar(
    need: FrameNeed,
    layer: FrameSupplyLayer,
    source: FrameSupplyVideoFrame,
    planar: NonNullable<DecodedNeed["planar"]>,
    pixelsFor: FrameSupplyOptions["pixelsFor"],
  ): Promise<PlanarYuvFrame | undefined | typeof DEGRADED> {
    let intake: Awaited<ReturnType<typeof intakeVideoFrame>>;
    try {
      intake = await intakeVideoFrame(source.frame, {
        required: planar.required,
        declared: parseDeclared(planar.declared),
        identity: planarIdentity(source),
        pool: this.#planarCopies,
        counters: this.#readback,
        unreadable: this.#unreadableFrames,
        takesPresented: takesPresentedFrames(this.#engine),
      });
    } catch (error) {
      source.frame.close();
      throw error;
    }
    if (intake.kind === "present") return undefined;
    let copied: PlanarYuvFrame;
    if (intake.kind === "planar") copied = intake.planar;
    else {
      const timestamp =
        source.physicalTimeMicroseconds ?? source.frame.timestamp;
      const transfer: string | null = source.frame.colorSpace?.transfer ?? null;
      const nonPlanar = intake.cause instanceof NonPlanarVideoFrameError;
      // Keep the frame open in case it is presented degraded.
      const keep = nonPlanar && this.#presentsDegraded;
      if (!keep) source.frame.close();
      const content = this.#contents.get(need.sourceId);
      if (nonPlanar && content !== undefined) content.softwareDecode = true;
      try {
        copied = await this.#redecodePlanar(
          need,
          layer,
          planar,
          { timestamp, transfer },
          pixelsFor,
          intake.cause,
        );
      } catch (error) {
        if (keep && error instanceof PlanarDecodeUnavailableError) {
          if (content !== undefined) {
            content.refused = undefined;
            content.softwareDecode = undefined;
            content.degraded = true;
          }
          return DEGRADED;
        }
        if (keep) source.frame.close();
        throw error;
      }
    }
    source.frame.close();
    this.#planarCopyFrames += 1;
    this.#planarCopyBytes += copied.data.byteLength;
    return copied;
  }

  /**
   * Decodes `need` again in software and copies it out planar.
   *
   * When `cause` is a {@link NonPlanarVideoFrameError} and software decode is unavailable or also
   * non-planar, rejects with {@link PlanarDecodeUnavailableError} and fails later frames of the
   * source at once. Other failures reject with `FRAME_SUPPLY_FAILED`.
   */
  async #redecodePlanar(
    need: FrameNeed,
    layer: FrameSupplyLayer,
    planar: NonNullable<DecodedNeed["planar"]>,
    { timestamp, transfer }: { timestamp: number; transfer: string | null },
    pixelsFor: FrameSupplyOptions["pixelsFor"],
    cause: unknown,
  ): Promise<PlanarYuvFrame> {
    const what =
      cause instanceof NonPlanarVideoFrameError
        ? `the browser decoded the frame of ${need.sourceId} at ${need.mediaTimeTicks} to ${
            cause.format === null
              ? "an opaque frame"
              : `a ${cause.format} frame`
          } the engine must convert from planes (HDR, or color the project declares)`
        : `the browser could not read back the frame of ${need.sourceId} at ${need.mediaTimeTicks}`;
    const unrecovered = (reason: string, error: unknown = cause) =>
      Object.assign(new Error(`${what}, and ${reason}`, { cause: error }), {
        code: "FRAME_SUPPLY_FAILED",
      });
    const refuse = (
      softwareDecoder: "none" | "opaque",
      error: unknown,
    ): PlanarDecodeUnavailableError => {
      const facts = {
        kind: "video" as const,
        codec: null,
        transfer,
        frameFormat:
          cause instanceof NonPlanarVideoFrameError ? cause.format : null,
        softwareDecoder,
        browserError: error instanceof Error ? error.message : String(error),
      };
      const refusal = new PlanarDecodeUnavailableError(
        decodeRefusalMessage(need.sourceId, facts),
        facts,
        { cause: error },
      );
      const content = this.#contents.get(need.sourceId);
      if (content !== undefined) content.refused = refusal;
      return refusal;
    };
    const nonPlanar = cause instanceof NonPlanarVideoFrameError;
    let decoded: FrameSupplySource | null;
    try {
      decoded = await this.#decodeCursor(
        `${keyForNeed(need, layer)}\u0000software`,
        () =>
          pixelsFor(
            layer,
            Number(BigInt(need.mediaTimeTicks)) / TICKS_PER_SECOND,
            {
              preferVideoFrame: true,
              consumerId: consumerOf(need),
              sourceId: need.sourceId,
              decode: "software",
            },
          ),
      );
    } catch (error) {
      if (
        nonPlanar &&
        (error as { code?: unknown } | null)?.code ===
          "software_decode_unavailable"
      )
        throw refuse("none", error);
      throw unrecovered("decoding it again in software failed", error);
    }
    if (
      decoded === null ||
      !("kind" in decoded) ||
      decoded.kind !== "video-frame"
    )
      throw unrecovered("decoding it again in software gave no VideoFrame");
    const again = decoded;
    try {
      const at = again.physicalTimeMicroseconds ?? again.frame.timestamp;
      if (at !== timestamp)
        throw unrecovered(
          `decoding it again in software gave the frame at ${at} us`,
        );
      let copied: PlanarYuvFrame;
      try {
        copied = await readPlanesWithRetries(
          again.frame,
          parseDeclared(planar.declared),
          planarIdentity(again),
          this.#planarCopies,
          this.#readback,
        );
      } catch (error) {
        if (nonPlanar && error instanceof NonPlanarVideoFrameError)
          throw refuse("opaque", error);
        throw error;
      }
      this.#readback.redecodedFrames += 1;
      return copied;
    } catch (error) {
      throw error instanceof PlanarDecodeUnavailableError ||
        (error instanceof Error && error.cause === cause)
        ? error
        : unrecovered(
            "the software decode's frame could not be copied either",
            error,
          );
    } finally {
      again.frame.close();
    }
  }

  /** The engine's presented RGBA upload methods, bound to it. */
  #presenter() {
    const engine = this.#engine;
    const required = <T>(upload: T | undefined): T => {
      if (upload === undefined)
        throw new Error("engine cannot present RGBA frames");
      return upload;
    };
    return {
      frame: (
        sourceId: string,
        ticks: bigint,
        raster: FrameSupplyRasterFrame,
        width: number,
        height: number,
      ) =>
        raster.precision === "linear_f16"
          ? required(engine.uploadPresentedLinearFrame).call(
              engine,
              sourceId,
              ticks,
              raster.width,
              raster.height,
              raster.pixels,
              width,
              height,
            )
          : required(engine.uploadPresentedRgbaFrame).call(
              engine,
              sourceId,
              ticks,
              raster.width,
              raster.height,
              raster.pixels,
              raster.alpha !== "straight",
              width,
              height,
            ),
      image: (
        sourceId: string,
        raster: FrameSupplyRasterFrame,
        width: number,
        height: number,
      ) =>
        raster.precision === "linear_f16"
          ? required(engine.uploadPresentedLinearImage).call(
              engine,
              sourceId,
              raster.width,
              raster.height,
              raster.pixels,
              width,
              height,
            )
          : required(engine.uploadPresentedRgbaImage).call(
              engine,
              sourceId,
              raster.width,
              raster.height,
              raster.pixels,
              raster.alpha !== "straight",
              width,
              height,
            ),
    };
  }

  /** The engine, asserted to take presented `VideoFrame` uploads. */
  #presentedEngine(): PresentedFrameEngine {
    const engine = this.#engine;
    if (!takesPresentedFrames(engine))
      throw new Error("engine cannot consume a VideoFrame directly");
    return engine;
  }

  #planarEngine(): PlanarYuvEngine {
    const engine = this.#engine;
    const upload = engine.uploadYuvFrame;
    if (upload === undefined)
      throw new Error("engine cannot convert planar YUV");
    return { uploadYuvFrame: (...args) => upload.apply(engine, args) };
  }

  async #decodeCursor<T>(key: string, decode: () => Promise<T>): Promise<T> {
    const previous = this.#decodeTails.get(key) ?? Promise.resolve();
    const result = previous
      .catch(() => {})
      .then(async () => {
        await this.#acquireDecodeSlot();
        try {
          return await decode();
        } finally {
          this.#releaseDecodeSlot();
        }
      });
    const tail = result.then(
      () => {},
      () => {},
    );
    this.#decodeTails.set(key, tail);
    try {
      return await result;
    } finally {
      if (this.#decodeTails.get(key) === tail) this.#decodeTails.delete(key);
    }
  }

  async #acquireDecodeSlot(): Promise<void> {
    if (this.#activeDecodes < this.#maxDecodeConcurrency) {
      this.#activeDecodes += 1;
      return;
    }
    await new Promise<void>((resolve) => this.#decodeWaiters.push(resolve));
    this.#activeDecodes += 1;
  }

  #releaseDecodeSlot(): void {
    this.#activeDecodes -= 1;
    this.#decodeWaiters.shift()?.();
  }

  /**
   * Retires receipts from {@link FrameSupply.supply} after rendering, along with replaced or
   * evicted stills. The oldest are released past `maxRetiredBytes`.
   */
  retire(uploads: readonly FrameUploadReceipt[]): void {
    const replaced = this.#replacedStills;
    this.#replacedStills = [];
    this.#retainer.retire(replaced);
    this.#retainer.retire(uploads);
  }

  #nextImageUse(): bigint {
    this.#imageUse += 1n;
    return this.#imageUse;
  }

  #removeImage(id: string, image: CachedImage): void {
    if (!this.#forgetImage(id, image)) return;
    this.#replacedStills.push(image.upload);
  }

  /** Drops a cache entry whose upload the engine already evicted. */
  #forgetImage(id: string, image: CachedImage): boolean {
    if (!this.#images.delete(id)) return false;
    this.#cachedImageBytes -= image.upload.retainedBytes;
    return true;
  }

  /** Uploads a still unless the cache holds this picture at this output size. */
  #supplyImage(id: string, still: Still, width: number, height: number): void {
    const cached = this.#images.get(id);
    if (
      cached !== undefined &&
      sameStill(cached.still, still) &&
      cached.width === width &&
      cached.height === height
    ) {
      cached.lastUsed = this.#nextImageUse();
      return;
    }
    const upload =
      "kind" in still
        ? this.#presenter().image(id, still, width, height)
        : this.#engine.uploadRgbaImage(id, width, height, still);
    validateUploadReceipt(upload);
    if (cached) this.#removeImage(id, cached);
    this.#images.set(id, {
      upload,
      still,
      width,
      height,
      lastUsed: this.#nextImageUse(),
    });
    this.#cachedImageBytes += upload.retainedBytes;
  }

  /**
   * Re-uploads stills this timestamp shows that the engine silently evicted (oldest first, by its
   * own budget). One upload can evict another, so this repeats while it makes progress.
   */
  #keepStillsHeld(
    stills: ReadonlyMap<string, Still>,
    width: number,
    height: number,
  ): void {
    for (let pass = 0; pass <= stills.size; pass += 1) {
      let dropped = false;
      for (const [id, still] of stills) {
        const cached = this.#images.get(id);
        if (
          cached !== undefined &&
          this.#engine.holdsImage(id, cached.upload.generation)
        )
          continue;
        if (cached !== undefined) this.#forgetImage(id, cached);
        dropped = true;
        this.#supplyImage(id, still, width, height);
      }
      if (!dropped) return;
    }
  }

  /**
   * Content epoch of `id` used in physical-frame identities. When `contentOf` reports a change, the
   * source's uploads are dropped here and in the engine.
   */
  #contentEpoch(
    id: string,
    contentOf: FrameSupplyOptions["contentOf"],
  ): number {
    const content = contentOf?.(id);
    const known = this.#contents.get(id);
    if (known !== undefined && Object.is(known.content, content))
      return known.epoch;
    if (known !== undefined) {
      this.#engine.releaseSourceFrames(id);
      const image = this.#images.get(id);
      if (image !== undefined) this.#forgetImage(id, image);
    }
    this.#nextEpoch += 1;
    this.#contents.set(id, { content, epoch: this.#nextEpoch });
    return this.#nextEpoch;
  }

  #evictImages(protectedImages: ReadonlyMap<string, unknown>): void {
    while (this.#cachedImageBytes > this.#maxCachedImageBytes) {
      let oldest: [string, CachedImage] | undefined;
      for (const entry of this.#images) {
        if (protectedImages.has(entry[0])) continue;
        if (oldest === undefined || entry[1].lastUsed < oldest[1].lastUsed) {
          oldest = entry;
        }
      }
      if (oldest === undefined) return;
      this.#removeImage(oldest[0], oldest[1]);
    }
  }

  /**
   * Supplies one timestamp, presents it, and retires the uploads.
   *
   * Resolves with `null` when `isCurrent` turned false before presenting. Rejects as
   * {@link FrameSupply.supply} does, or when the engine still reports missing sources.
   */
  async render(
    timeTicks: bigint,
    options: FrameSupplyOptions,
  ): Promise<PlaybackResult | null> {
    const supplied = await this.supply(timeTicks, options);
    if (!supplied) return null;
    const { needs, uploaded } = supplied;
    try {
      if (!options.isCurrent()) return null;
      const playback = JSON.parse(
        this.#engine.renderAt(timeTicks),
      ) as PlaybackResult;
      // Do not await GPU completion: it adds latency without fixing flicker.
      if (playback.missingSources.length) {
        const missing = needs.filter((need) =>
          playback.missingSources.includes(need.sourceId),
        );
        throw new Error(
          `missing frames at timeline ${timeTicks}: ${missing
            .map((need) => `${need.sourceId}@${need.mediaTimeTicks}`)
            .join(", ")}`,
        );
      }
      return playback;
    } finally {
      this.retire(uploaded);
    }
  }
}

function videoIdentity(
  sourceId: string,
  contentGeneration: number,
  source: FrameSupplyVideoFrame,
  outputWidth: number,
  outputHeight: number,
): PhysicalFrameIdentity {
  const frame = source.frame;
  return {
    sourceId,
    contentGeneration,
    timestamp: String(
      validateFrameTime(
        source.physicalTimeMicroseconds ?? frame.timestamp,
        "VideoFrame timestamp",
      ),
    ),
    duration:
      frame.duration === null
        ? null
        : validateFrameTime(frame.duration, "VideoFrame duration"),
    sourceWidth: frame.displayWidth,
    sourceHeight: frame.displayHeight,
    outputWidth,
    outputHeight,
    rotation: source.rotation ?? 0,
    mirror: source.mirror ?? false,
  };
}

function rasterIdentity(
  sourceId: string,
  contentGeneration: number,
  source: FrameSupplyRasterFrame,
  outputWidth: number,
  outputHeight: number,
): PhysicalFrameIdentity {
  return {
    sourceId,
    contentGeneration,
    timestamp: String(
      validateFrameTime(
        source.physicalTimeMicroseconds,
        "raster frame timestamp",
      ),
    ),
    duration:
      source.durationMicroseconds === undefined
        ? null
        : validateFrameTime(
            source.durationMicroseconds,
            "raster frame duration",
          ),
    sourceWidth: source.width,
    sourceHeight: source.height,
    outputWidth,
    outputHeight,
    rotation: 0,
    mirror: false,
  };
}

function rgbaIdentity(
  sourceId: string,
  contentGeneration: number,
  source: FrameSupplyRgbaFrame,
  outputWidth: number,
  outputHeight: number,
): PhysicalFrameIdentity {
  return {
    sourceId,
    contentGeneration,
    timestamp: String(
      validateFrameTime(
        source.physicalTimeMicroseconds,
        "RGBA frame timestamp",
      ),
    ),
    duration:
      source.durationMicroseconds === undefined
        ? null
        : validateFrameTime(source.durationMicroseconds, "RGBA frame duration"),
    sourceWidth: source.sourceWidth,
    sourceHeight: source.sourceHeight,
    outputWidth,
    outputHeight,
    rotation: source.rotation ?? 0,
    mirror: false,
  };
}

/**
 * Whether two stills are the same picture: the same RGBA8 array, or provider rasters with equal
 * time, duration, size, precision and alpha.
 */
function sameStill(cached: Still, still: Still): boolean {
  // Check shape, not class, since host pixels may come from another realm.
  if (!("kind" in cached) || !("kind" in still)) return cached === still;
  return (
    cached.width === still.width &&
    cached.height === still.height &&
    cached.physicalTimeMicroseconds === still.physicalTimeMicroseconds &&
    cached.durationMicroseconds === still.durationMicroseconds &&
    rasterFormat(cached) === rasterFormat(still)
  );
}

/** A raster's precision and, for RGBA8, its alpha. */
function rasterFormat(raster: FrameSupplyRasterFrame): string {
  return raster.precision === "linear_f16"
    ? "linear_f16"
    : `rgba8/${raster.alpha ?? "premultiplied"}`;
}

function validateFrameTime(value: number, name: string): number {
  if (!Number.isSafeInteger(value)) {
    throw new RangeError(`${name} must be a safe integer`);
  }
  return value;
}

function validateUploadReceipt(upload: FrameUploadReceipt): void {
  if (upload.generation <= 0n) {
    throw new RangeError("frame generation must be positive");
  }
  validatePositiveInteger(upload.retainedBytes, "frame retainedBytes");
}

function validatePositiveInteger(value: number, name: string): void {
  if (!Number.isSafeInteger(value) || value <= 0) {
    throw new RangeError(`${name} must be a positive safe integer`);
  }
}

/** `#copyPlanar` result for a frame to present degraded. */
const DEGRADED = Symbol("degraded");

function parseDeclared(declared: string | undefined): ColorMetadata | null {
  return declared === undefined
    ? null
    : (JSON.parse(declared) as ColorMetadata);
}

function planarIdentity(source: FrameSupplyVideoFrame): PlanarFrameIdentity {
  return {
    ...(source.physicalTimeMicroseconds === undefined
      ? {}
      : { physicalTimeMicroseconds: source.physicalTimeMicroseconds }),
    ...(source.rotation === undefined ? {} : { rotation: source.rotation }),
    ...(source.mirror === undefined ? {} : { mirror: source.mirror }),
    ...(source.chromaSiting === undefined
      ? {}
      : { chromaSiting: source.chromaSiting }),
  };
}

/** Whether `engine` has `uploadPresentedVideoFrame`. */
function takesPresentedFrames(
  engine: FrameSupplyEngine,
): engine is FrameSupplyEngine & PresentedFrameEngine {
  return typeof engine.uploadPresentedVideoFrame === "function";
}

function closeDecodedNeeds(decoded: readonly DecodedNeed[]): void {
  for (const { source } of decoded) {
    if (source && "kind" in source && source.kind === "video-frame") {
      source.frame.close();
    }
  }
}

function keyForNeed(need: FrameNeed, layer: FrameSupplyLayer): string {
  return layer.kind === "image"
    ? need.sourceId
    : decoderCursorKey(need.sourceId, consumerOf(need));
}

/**
 * The sequential reader of a source: the clip or a composition step. Readers at different times
 * need separate cursors to avoid constant seeking.
 */
function consumerOf(need: FrameNeed): string {
  return decoderConsumerId(need.clipId ?? need.sourceId, need.readerId);
}

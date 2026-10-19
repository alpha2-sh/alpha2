// The engine's immutable project snapshot is the sole authority for frame keys. The host must not
// derive them itself, for example by rounding (timelineSeconds - clip.start).

import type { FrameUploadReceipt } from "./effects.js";
import type { ColorMetadata } from "./generated/project-schema.js";
import {
  type ChromaSiting,
  copyPlanarYuvFrame,
  isHdrVideoFrame,
  isPlanarVideoFrame,
  PlanarCopyPool,
  type PlanarYuvEngine,
  uploadPlanarYuvFrame,
  type VideoConversion,
} from "./hdr-frames.js";
import { decoderConsumerId } from "./media.js";
import {
  PhysicalFrameAliases,
  type PhysicalFrameIdentity,
} from "./physical-frames.js";

/** Tick rate shared by every engine timestamp. */
const TICKS_PER_SECOND = 120_000;

const DEFAULT_MAX_RETIRED_FRAME_BYTES = 256 * 1024 * 1024;
const DEFAULT_MAX_CACHED_IMAGE_BYTES = 256 * 1024 * 1024;
const DEFAULT_MAX_DECODE_CONCURRENCY = 2;

/** One decode request the engine reports for a single timeline timestamp (`frameNeeds`). */
export interface FrameNeed {
  /** Clip that needs the frame. Overlapping clips report the same source. */
  readonly clipId: string;
  /**
   * The composition step that reads the source, when a media step does rather than the clip
   * itself; `null` or absent for the clip's own source. Each reader keeps its own decoder cursor.
   */
  readonly readerId?: string | null;
  readonly sourceId: string;
  /** Source timestamp as a decimal tick string, never a rounded number. */
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
  /**
   * The decoded frame's own timestamp, in microseconds, for sources whose wrapper resets
   * `frame.timestamp`. Defaults to `frame.timestamp`.
   */
  readonly physicalTimeMicroseconds?: number;
  /**
   * Clockwise stream display rotation, when the container declares one, applied after
   * {@link mirror}.
   */
  readonly rotation?: 0 | 90 | 180 | 270;
  /** Whether the stream mirrors the frame left to right before it turns. */
  readonly mirror?: boolean;
  /**
   * Where chroma samples sit, which WebCodecs does not report: see `chromaSitingForStream`.
   */
  readonly chromaSiting?: ChromaSiting;
}

/**
 * A video frame read back to RGBA, for an engine that cannot take a `VideoFrame`. It carries the
 * decoded frame's identity so that requests landing on one decoded frame share one upload.
 */
export interface FrameSupplyRgbaFrame {
  readonly kind: "rgba-frame";
  /** RGBA8 at the output size, rows packed. */
  readonly pixels: Uint8Array;
  /** The decoded frame's timestamp, in microseconds; a safe integer. */
  readonly physicalTimeMicroseconds: number;
  /** The decoded frame's duration, in microseconds; a safe integer. */
  readonly durationMicroseconds?: number;
  /** The decoded frame's display size, before scaling to the output. */
  readonly sourceWidth: number;
  readonly sourceHeight: number;
  readonly rotation?: 0 | 90 | 180 | 270;
}

/**
 * A frame at its own size that the engine presents into the output, as it presents every source:
 * an extension provider's (SVG, animated image, Lottie, OpenEXR, image sequence). The page never
 * scales it.
 */
export type FrameSupplyRasterFrame = {
  readonly kind: "raster-frame";
  readonly width: number;
  readonly height: number;
  /** The first source time it shows at, in microseconds: its identity. */
  readonly physicalTimeMicroseconds: number;
  readonly durationMicroseconds?: number;
} & (
  | {
      readonly precision?: "rgba8";
      /** sRGB RGBA8, rows packed: premultiplied unless `alpha` says otherwise. */
      readonly pixels: Uint8Array;
      /**
       * Whether the pixels' color is premultiplied by their alpha (the default) or straight, as a
       * decoded image's is.
       */
      readonly alpha?: "premultiplied" | "straight";
    }
  | {
      readonly precision: "linear_f16";
      /**
       * Premultiplied linear-light RGBA half floats (their bits) in BT.709 primaries, rows packed;
       * values above 1 and below 0 are kept.
       */
      readonly pixels: Uint16Array;
    }
);

/**
 * What `pixelsFor` resolves with: bare RGBA8 pixels at the output size (a still image only), or a
 * frame carrying its decoded identity. A video source must not resolve with bare pixels.
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
  /**
   * Decoder cursor owner. Overlapping clips sharing one asset must decode
   * through independent cursors; see {@link decoderCursorKey}.
   */
  readonly consumerId: string;
  /**
   * The source the engine asked for. The layer may stand in for it (a proxy under an ID of its
   * own), so a host keys what it keeps per source by this, as `liveSourceIds` names sources.
   */
  readonly sourceId: string;
}

/** Host callbacks and frame geometry for one `supply` call. */
export interface FrameSupplyOptions {
  /** The project's output size, which RGBA pixels must match. */
  readonly width: number;
  readonly height: number;
  /**
   * What a source's pixels are, as a value that changes exactly when they do: other media, other
   * provider settings, a proxy instead of the original. Compared with `Object.is`. A source whose
   * value changes loses what was uploaded for it, in this supply and in the engine; every other
   * source keeps its uploads across project loads. Without it a source's pixels never change while
   * it stays in the project, which is the engine's own rule: media that changes under an unchanged
   * locator needs a new source ID.
   */
  readonly contentOf?: (sourceId: string) => unknown;
  /** Sources still present in the project. Others have their uploads retired. */
  readonly liveSourceIds: ReadonlySet<string>;
  // These are standalone functions, not methods: `supply` destructures them, so a host that
  // passes an object method must bind it first.
  /** The source's layer; a missing one makes `supply` reject. */
  readonly getLayer: (sourceId: string) => FrameSupplyLayer | null | undefined;
  /**
   * Decodes `layer` at `seconds` of source time. Resolves with the frame, or `null` when the
   * source has no frame there; a rejection rejects `supply`. The supply takes ownership of a
   * returned `VideoFrame`.
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
  /**
   * Returns false once an edit, seek, or pause invalidates this request. Asked before each decode,
   * so a replaced request decodes nothing more.
   */
  readonly isCurrent: () => boolean;
}

/** Memory envelope for source textures whose render submission has completed. */
export interface FrameRetentionOptions {
  /**
   * Texture bytes kept resident after their frames were rendered, so repeated requests reuse them.
   * Defaults to 256 MiB. The newest generation is kept even when it alone exceeds this.
   */
  readonly maxRetiredBytes?: number;
}

/** Resource and concurrency limits owned by one frame supplier. */
export interface FrameSupplyConfig extends FrameRetentionOptions {
  /**
   * Bytes of uploaded stills kept cached, least recently used evicted first. Defaults to 256 MiB.
   */
  readonly maxCachedImageBytes?: number;
  /** Maximum number of independent decoder cursors running at once. Defaults to 2. */
  readonly maxDecodeConcurrency?: number;
  /**
   * Who converts a decoded SDR `VideoFrame`: the engine, from its planes (`"engine"`, the
   * default), or the browser (`"browser"`); see {@link VideoConversion}.
   */
  readonly videoConversion?: VideoConversion;
}

interface DecodedNeed {
  readonly need: FrameNeed;
  readonly layer: FrameSupplyLayer;
  /** What the host decoded; `null` once the supply has handed it on. */
  readonly source: FrameSupplySource | null;
  /**
   * Set for a `VideoFrame` the engine converts from planes (every planar frame, unless the supply
   * leaves SDR to the browser): the declared color as JSON, if any. It is copied out planar only
   * when no earlier upload of the same picture serves it.
   */
  readonly planar?: { readonly declared: string | undefined };
}

/**
 * Keeps completed source upload generations resident within a byte budget, releasing the oldest
 * through `release` when the budget is exceeded.
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

  /** Bytes of the generations currently held. */
  get retainedBytes(): number {
    return this.#retiredBytes;
  }

  /**
   * Takes over generations whose rendering has completed, releasing older ones past the budget.
   *
   * @throws RangeError for a receipt with a non-positive generation or byte count.
   */
  retire(uploads: readonly FrameUploadReceipt[]): void {
    for (const upload of uploads) {
      validateUploadReceipt(upload);
      this.#retired.push(upload);
      this.#retiredBytes += upload.retainedBytes;
      // Keep the newest generation even when one texture exceeds the budget, so a large but valid
      // frame cannot block forward progress.
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

  /**
   * Forgets every held generation without releasing it, for when the owning engine has dropped its
   * whole frame store.
   */
  discardAll(): void {
    this.#retired = [];
    this.#retiredBytes = 0;
  }
}

/** One upload reported through {@link FrameSupply.onUpload}. */
export interface FrameUploadTrace {
  readonly sourceId: string;
  readonly mediaTimeTicks: string;
  /** A sparse 32-bit checksum of the uploaded RGBA pixels, for telling pictures apart. */
  readonly checksum: number;
}

/** Uploads accepted by the engine for one timestamp. */
export interface SuppliedFrame {
  /** Everything the engine asked for, including needs served by an existing upload. */
  readonly needs: readonly FrameNeed[];
  /** Receipts for the textures this call uploaded; pass them to `retire` once rendered. */
  readonly uploaded: readonly FrameUploadReceipt[];
}

/** Outcome of one real-time presentation attempt. */
export interface PlaybackResult {
  readonly requestedTimeTicks: number;
  /** `null` until the engine has presented a frame, and after a reset. */
  readonly presentedTimeTicks: number | null;
  readonly missingSources: readonly string[];
  readonly repeated: boolean;
}

/**
 * Engine surface this module drives.
 *
 * The wasm `WebEffectRegistry` satisfies it; a worker-side proxy that forwards the same calls does
 * too. Optional methods are probed per call, and a source that needs a missing one makes `supply`
 * reject.
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
  /**
   * Converts planar YUV with the project's color pipeline. An engine that has it takes every HDR
   * frame this way, never through the browser's conversion, and every other planar frame unless
   * the supply leaves SDR to the browser.
   */
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
  /**
   * Presents a premultiplied linear-light RGBA half-float frame at its own size into the output.
   */
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
  /** The color the loaded project declares for `sourceId`, as JSON. */
  sourceColorMetadata?(sourceId: string): string | undefined;
  aliasVideoFrame(
    sourceId: string,
    existingMediaTimeTicks: bigint,
    requestedMediaTimeTicks: bigint,
  ): boolean;
  /**
   * Whether the engine still holds the still `sourceId` as the upload `generation` names; the
   * engine drops stills on its own.
   */
  holdsImage(sourceId: string, generation: bigint): boolean;
  releaseFrameGeneration(generation: bigint): void;
  /** Drops the frames and still of one source whose media changed. */
  releaseSourceFrames(sourceId: string): void;
  renderAt(timeTicks: bigint): string;
}

/**
 * Which content of a source this supply uploaded, and the number that names it in physical-frame
 * identities.
 */
interface SourceContent {
  readonly content: unknown;
  readonly epoch: number;
}

/**
 * What a still layer decoded to: RGBA8 pixels at the output size, or a provider frame at its own
 * size.
 */
type Still = Uint8Array | FrameSupplyRasterFrame;

interface CachedImage {
  readonly upload: FrameUploadReceipt;
  /**
   * What was uploaded: RGBA8 pixels, known by the array, or a provider raster, known by its
   * identity, since a provider answers each call with a new array of the same picture.
   */
  readonly still: Still;
  readonly width: number;
  readonly height: number;
  lastUsed: bigint;
}

/**
 * Keys the decoder cursor of one clip reading one asset.
 *
 * Two clips sharing a source must not share a cursor: a single sequential decoder would seek
 * backwards and forwards on every output frame.
 */
export function decoderCursorKey(assetId: string, consumerId: string): string {
  return `${assetId}\u0000${consumerId}`;
}

/** Raw source video a {@link FrameSupply} copied through script memory, cumulative. */
export interface SourceTransferStats {
  readonly planarCopyFrames: number;
  readonly planarCopyBytes: number;
}

/**
 * Reference host-side consumer of the engine's `frameNeeds` contract.
 *
 * It asks the engine which source frames one timestamp requires, decodes each through a
 * clip-scoped cursor, uploads them, and bounds upload retention. Requests whose frames resolve to
 * one decoded frame share one upload.
 *
 * The constructor throws `RangeError` for a limit in `options` that is not a positive safe integer.
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
  #planarCopyFrames = 0;
  #planarCopyBytes = 0;
  /** Buffers the planar copies reuse; each is given back once its upload has taken it. */
  readonly #planarCopies = new PlanarCopyPool();
  #imageUse = 0n;
  readonly #contents = new Map<string, SourceContent>();
  #nextEpoch = 0;
  /**
   * Still uploads a supply replaced or evicted, for the next {@link FrameSupply.retire}. Retiring
   * them at once could release, past the retired budget, a generation that a request of the same
   * frame was just served from by alias, before the engine renders that frame.
   */
  #replacedStills: FrameUploadReceipt[] = [];

  /**
   * Optional upload trace, for diagnosing a clip that renders another clip's picture. Every RGBA
   * upload then reports which source received which pixels. Two sources sharing a checksum means
   * the host decoded the wrong asset, which is a host bug; distinct checksums move the search into
   * the engine.
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
    this.#retainer = new FrameGenerationRetainer(
      (generation) => this.#engine.releaseFrameGeneration(generation),
      options,
    );
  }

  /** Bytes of rendered source textures kept resident; see {@link FrameRetentionOptions}. */
  get retiredBytes(): number {
    return this.#retainer.retainedBytes;
  }

  /** Bytes of uploaded stills currently cached. */
  get cachedImageBytes(): number {
    return this.#cachedImageBytes;
  }

  /**
   * Raw source video this supply copied through script memory, cumulative. Frames are copied out
   * planar so the engine converts them, as natively (see {@link VideoConversion}); that copy is a
   * source-side cost, apart from any output readback.
   */
  get sourceTransfer(): SourceTransferStats {
    return {
      planarCopyFrames: this.#planarCopyFrames,
      planarCopyBytes: this.#planarCopyBytes,
    };
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
   * Uploads every source the engine asks for at one timestamp. Browser video stays as a
   * `VideoFrame` until the WebGPU external-image copy consumes it.
   *
   * Resolves with receipts for the textures this call uploaded, or `null` when `isCurrent` turned
   * false mid-decode (decoded frames are then closed). The caller retires the receipts with
   * {@link FrameSupply.retire} once the engine has consumed them, so this call does not; the still
   * uploads it replaced or evicted are retired then too.
   *
   * Rejects with a fatal `GPU_DEVICE_LOST` error when the engine's device is lost, and with an
   * `Error` when a source is unavailable, decodes to nothing or to the wrong kind of frame, or an
   * upload fails. Uploads made before the failure are retired and remaining frames closed.
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
      // Coded as the binding codes a loss, so an owner can tell it from a failed frame and make the
      // renderer again.
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
          let upload: FrameUploadReceipt;
          if (planar !== undefined) {
            // The copy takes the frame over and closes it, failed or not.
            decoded[decodedIndex] = { need, layer, source: null };
            const copied = await this.#copyPlanar(videoFrame, planar.declared);
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
              // The upload copied the planes into the engine before it returned.
              this.#planarCopies.release(copied.data);
            }
          } else {
            // Called on the engine: these methods lose their receiver when detached.
            if (this.#engine.uploadPresentedVideoFrame === undefined)
              throw new Error("engine cannot consume a VideoFrame directly");
            upload = this.#engine.uploadPresentedVideoFrame(
              need.sourceId,
              sourceTicks,
              videoFrame.frame,
              width,
              height,
              videoFrame.rotation,
              videoFrame.mirror ?? false,
            );
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
          // A request a seek or an edit replaced starts no further decodes: the next request waits
          // only for those already running.
          if (failed || !isCurrent()) return;
          try {
            const sourceTicks = BigInt(need.mediaTimeTicks);
            // Declared or HDR color is converted by the engine from planes.
            const declared =
              planar && layer.kind === "video"
                ? this.#engine.sourceColorMetadata?.(need.sourceId)
                : undefined;
            const enginesColor =
              declared !== undefined ||
              (planar &&
                layer.kind === "video" &&
                this.#engine.hdrSource?.(need.sourceId) === true);
            const decoded = await this.#decodeCursor(
              keyForNeed(need, layer),
              () =>
                pixelsFor(layer, Number(sourceTicks) / TICKS_PER_SECOND, {
                  preferVideoFrame: directVideo || enginesColor,
                  consumerId: consumerOf(need),
                  sourceId: need.sourceId,
                }),
            );
            results[index] = this.#planarRoute(
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
   * Routes a decoded `VideoFrame` to the engine's own color conversion, from planes: an HDR frame,
   * every frame of a source whose project declares its color (`enginesColor`), which wins over
   * the frame's own as it does natively, and, unless the supply leaves SDR to the browser, every
   * other frame with a planar format. A source the engine must convert must decode to a
   * `VideoFrame`: an RGBA readback has been through the browser's conversion already. The planes
   * are copied out only at upload, after an earlier upload of the same picture had the chance to
   * serve it.
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
    return { need, layer, source, planar: { declared } };
  }

  /** Copies a frame out planar for the engine to convert; the copy closes the frame. */
  async #copyPlanar(
    source: FrameSupplyVideoFrame,
    declared: string | undefined,
  ) {
    const copied = await copyPlanarYuvFrame(
      source.frame,
      declared === undefined ? null : (JSON.parse(declared) as ColorMetadata),
      {
        ...(source.physicalTimeMicroseconds === undefined
          ? {}
          : { physicalTimeMicroseconds: source.physicalTimeMicroseconds }),
        ...(source.rotation === undefined ? {} : { rotation: source.rotation }),
        ...(source.mirror === undefined ? {} : { mirror: source.mirror }),
        ...(source.chromaSiting === undefined
          ? {}
          : { chromaSiting: source.chromaSiting }),
      },
      this.#planarCopies,
    );
    this.#planarCopyFrames += 1;
    this.#planarCopyBytes += copied.data.byteLength;
    return copied;
  }

  /** The engine's presented RGBA uploads, called on the engine. */
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
   * Hands over receipts from {@link FrameSupply.supply} once the engine has rendered with them,
   * after the still uploads the supplies since the last call replaced or evicted. The oldest are
   * released past `maxRetiredBytes`.
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

  /** Drops a cache entry whose upload the engine no longer holds, so there is nothing to retire. */
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
   * Uploads again each still this timestamp shows that the engine no longer holds. The engine drops
   * stills by its own byte budget and count limit, oldest upload first, without telling the page,
   * so a still cached here since long ago is the first to go, before this supply or during it. One
   * upload can drop another still this timestamp shows, so this repeats while it makes progress.
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
   * The number naming the content of `id` this supply uploads, in physical-frame identities. When
   * `contentOf` says the source changed, what was uploaded for it goes, here and in the engine;
   * other sources keep theirs.
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
   * Supplies one timestamp and presents it, retiring the uploads afterwards.
   *
   * Resolves with `null` when `isCurrent` turned false before presenting. Rejects as
   * {@link FrameSupply.supply} does, and with an `Error` naming the missing frames when the engine
   * still reports missing sources after the supply.
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
      // Do not await GPU completion here: it adds latency and does not fix flicker. Export reads
      // pixels back offscreen instead.
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
 * Whether two stills are one picture: the same RGBA8 array, or provider rasters of the same
 * identity (its first source time and duration, size, precision, and alpha). A source whose content
 * changes loses its cached still through `contentOf` first.
 */
function sameStill(cached: Still, still: Still): boolean {
  // Told apart by shape, not by class: a host's pixels can be a typed array of another realm.
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
 * Who reads a source sequentially: the clip, or one of its composition's steps. Two steps reading
 * one source at different times must not share a cursor, or each read makes the decoder seek.
 */
function consumerOf(need: FrameNeed): string {
  return decoderConsumerId(need.clipId ?? need.sourceId, need.readerId);
}

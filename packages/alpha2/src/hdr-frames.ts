// Decoded frames reach the engine as planar YUV and are converted by the engine's shader, as
// natively: a canvas or external texture would tone-map or clip HDR and misplace subsampled chroma.
// SDR frames take this path too unless the host sets `videoConversion: "browser"`.

import type { ChromaSiting } from "./chroma-siting.js";
import type { FrameUploadReceipt } from "./effects.js";
import type { ColorMetadata } from "./generated/project-schema.js";

/** WebCodecs pixel formats the engine converts from planar YUV. */
const PLANAR_FORMATS = new Set([
  "I420",
  "I420P10",
  "I420P12",
  "I422",
  "I422P10",
  "I422P12",
  "I444",
  "I444P10",
  "I444P12",
  "I420A",
  "I420AP10",
  "I420AP12",
  "I422A",
  "I422AP10",
  "I422AP12",
  "I444A",
  "I444AP10",
  "I444AP12",
  "NV12",
]);

export {
  type ChromaSiting,
  chromaSitingForCodec,
  chromaSitingForStream,
  type SampleAspect,
  sampleAspectForStream,
} from "./chroma-siting.js";

/** What a planar copy carries of its frame's identity beyond the frame itself. */
export interface PlanarFrameIdentity {
  readonly physicalTimeMicroseconds?: number;
  readonly rotation?: 0 | 90 | 180 | 270;
  readonly mirror?: boolean;
  readonly chromaSiting?: ChromaSiting;
}

/** A decoded frame copied out as planar YUV, with the color it carries. */
export interface PlanarYuvFrame {
  readonly kind: "yuv-frame";
  /** WebCodecs pixel format, one of the planar formats the engine reads. */
  readonly format: string;
  /** Size of the copied planes: the frame's visible picture. */
  readonly codedWidth: number;
  readonly codedHeight: number;
  readonly data: Uint8Array;
  /** `[offset, stride]` per plane, in bytes. */
  readonly layout: readonly number[];
  /** `[x, y, width, height]` of the picture inside the coded frame. */
  readonly visibleRect: readonly [number, number, number, number];
  readonly color: ColorMetadata;
  readonly chromaSiting: ChromaSiting;
  /**
   * Size the picture is shown at before it turns: its pixel aspect ratio, which the engine applies.
   */
  readonly displayWidth: number;
  readonly displayHeight: number;
  readonly physicalTimeMicroseconds: number;
  readonly durationMicroseconds?: number;
  /** Clockwise turn that shows the frame upright, after {@link mirror}. */
  readonly rotation?: 0 | 90 | 180 | 270;
  /** Whether the frame is mirrored left to right before it turns. */
  readonly mirror?: boolean;
}

/** Engine surface that converts planar YUV with the project's color pipeline. */
export interface PlanarYuvEngine {
  /**
   * Uploads one planar frame for `sourceId` at `mediaTimeTicks`, to be
   * converted with the project's color pipeline and shown upright at
   * `outputWidth` x `outputHeight`.
   */
  uploadYuvFrame(
    sourceId: string,
    mediaTimeTicks: bigint,
    format: string,
    codedWidth: number,
    codedHeight: number,
    data: Uint8Array,
    layout: Uint32Array,
    visibleRect: Uint32Array,
    /**
     * Size the visible picture is shown at before it turns (a `VideoFrame`'s display size): its
     * pixel aspect ratio.
     */
    displayWidth: number,
    displayHeight: number,
    colorJson: string,
    chromaSiting: ChromaSiting,
    outputWidth: number,
    outputHeight: number,
    rotationDegrees: number,
    mirror: boolean,
  ): FrameUploadReceipt;
}

/**
 * The engine's color description of a WebCodecs color space.
 *
 * Unspecified parts take the SDR BT.709 limited-range defaults; a PQ or HLG
 * transfer makes the signal HDR.
 */
export function colorMetadataFromVideoColorSpace(
  space: VideoColorSpaceInit | null | undefined,
): ColorMetadata {
  // TypeScript's DOM library predates WebCodecs' HDR values ("pq", "hlg", "bt2020", "bt2020-ncl"),
  // so the fields are read as strings.
  const named = space as
    | {
        transfer?: string | null;
        primaries?: string | null;
        matrix?: string | null;
      }
    | null
    | undefined;
  const transfer = (() => {
    switch (named?.transfer) {
      case "pq":
        return "pq" as const;
      case "hlg":
        return "hlg" as const;
      case "iec61966-2-1":
        return "srgb" as const;
      default:
        return "bt709" as const;
    }
  })();
  const primaries = (() => {
    switch (named?.primaries) {
      case "bt2020":
        return "bt2020" as const;
      case "bt470bg":
      case "smpte170m":
        return "bt601" as const;
      default:
        return "bt709" as const;
    }
  })();
  const matrix = (() => {
    switch (named?.matrix) {
      case "rgb":
        return "rgb" as const;
      case "bt2020-ncl":
        return "bt2020_ncl" as const;
      case "bt470bg":
      case "smpte170m":
        return "bt601" as const;
      default:
        return "bt709" as const;
    }
  })();
  return {
    dynamic_range: transfer === "pq" || transfer === "hlg" ? "hdr" : "sdr",
    matrix,
    primaries,
    range: space?.fullRange === true ? "full" : "limited",
    transfer,
  };
}

/**
 * How a host turns decoded SDR `VideoFrame`s into the engine's pixels.
 *
 * `"engine"` (the default) copies each frame that has a planar format the engine reads out with
 * `copyTo` and converts it with the engine's shader, as a native host converts the same stream:
 * the frame's own color space, and the chroma siting its decoder reports (`chromaSiting`). A
 * frame without such a format (an opaque hardware frame) still goes through the browser.
 * `"browser"` imports SDR frames as textures instead, which saves the copy (in Chrome on a desktop
 * GPU about 1 ms a 1080p frame, 4 to 5 ms a 4K one) but leaves the conversion to the browser, which
 * does not read the stream's siting: Chrome converts H.264 and HEVC chroma, sited left, as if
 * centered, so every sharp chroma edge lands apart from a native export's. HDR frames, and frames
 * of a source whose color the project declares, are the engine's either way.
 */
export type VideoConversion = "engine" | "browser";

/**
 * A frame with no planar YUV format the engine reads: an opaque frame (a hardware frame the
 * browser does not map to the CPU, `format` null) or an RGB one. Reading it again cannot help;
 * decoding it again in software can.
 */
export class NonPlanarVideoFrameError extends Error {
  readonly format: VideoPixelFormat | null;
  constructor(format: VideoPixelFormat | null) {
    super(
      `frame has no planar YUV format the engine reads (${format ?? "opaque"}); decode HDR sources, and sources whose color the project declares, in software`,
    );
    this.name = "NonPlanarVideoFrameError";
    this.format = format;
  }
}

/** Whether `frame` has a planar format the engine converts (`copyPlanarYuvFrame` can copy it). */
export function isPlanarVideoFrame(frame: VideoFrame): boolean {
  return frame.format !== null && PLANAR_FORMATS.has(frame.format);
}

/** Whether a decoded frame's own color space says it is HDR. */
export function isHdrVideoFrame(frame: VideoFrame): boolean {
  const transfer: string | null | undefined = frame.colorSpace?.transfer;
  return transfer === "pq" || transfer === "hlg";
}

/**
 * Buffers planar copies reuse: a 4K 10-bit frame is 25 MB, which a fresh allocation zero-fills
 * and the garbage collector reclaims at the frame rate. A copy takes a buffer with
 * {@link PlanarCopyPool.acquire} and gives it back with {@link PlanarCopyPool.release} once its
 * upload has taken the bytes (an engine upload copies them into wasm memory before it returns).
 * Copies that overlap each take their own buffer.
 */
export class PlanarCopyPool {
  readonly #maxFree: number;
  #free: ArrayBuffer[] = [];

  /** Keeps at most `maxFree` (default 2) buffers between copies. */
  constructor(maxFree = 2) {
    if (!Number.isSafeInteger(maxFree) || maxFree < 0)
      throw new RangeError("maxFree must be a non-negative safe integer");
    this.#maxFree = maxFree;
  }

  /** `bytes` bytes to copy into: a kept buffer large enough, or a new one. */
  acquire(bytes: number): Uint8Array {
    // Kept smallest first: the first one large enough is the smallest that fits.
    const index = this.#free.findIndex((buffer) => buffer.byteLength >= bytes);
    const kept = index < 0 ? undefined : this.#free.splice(index, 1)[0];
    return new Uint8Array(kept ?? new ArrayBuffer(bytes), 0, bytes);
  }

  /** Gives back the buffer under `data`; nothing may read `data` afterwards. */
  release(data: Uint8Array): void {
    const buffer = data.buffer;
    if (!(buffer instanceof ArrayBuffer) || this.#free.includes(buffer)) return;
    this.#free.push(buffer);
    this.#free.sort((left, right) => left.byteLength - right.byteLength);
    // The largest serve every size, so past the limit the smallest go.
    this.#free.splice(0, Math.max(0, this.#free.length - this.#maxFree));
  }

  /** Drops every kept buffer. */
  clear(): void {
    this.#free = [];
  }

  /** Bytes the kept buffers hold. */
  get keptBytes(): number {
    return this.#free.reduce((sum, buffer) => sum + buffer.byteLength, 0);
  }
}

/**
 * Copies `frame` out as planar YUV and closes it.
 *
 * `declared` is the color the project declares for the source, which wins
 * over the frame's own color space, as it does natively. A frame without a
 * CPU-readable planar format (a hardware frame the browser will not map) is
 * refused rather than converted by the browser.
 *
 * `frame` is closed whether the copy succeeds or not. Rejects with
 * {@link NonPlanarVideoFrameError} when the frame has no planar format the
 * engine reads, and otherwise when it has no visible rectangle or `copyTo`
 * fails.
 *
 * With `pool`, the copy's `data` is a buffer from it, which the caller gives back with
 * {@link PlanarCopyPool.release} after uploading; a failed copy gives it back itself.
 */
export async function copyPlanarYuvFrame(
  frame: VideoFrame,
  declared: ColorMetadata | null,
  identity: PlanarFrameIdentity = {},
  pool?: PlanarCopyPool,
): Promise<PlanarYuvFrame> {
  try {
    return await readPlanarYuvFrame(frame, declared, identity, pool);
  } finally {
    frame.close();
  }
}

/**
 * {@link copyPlanarYuvFrame} without closing `frame`: the caller owns it whether the copy succeeds
 * or not, so a frame whose planes the browser could not read back can be read again. Rejects as
 * {@link copyPlanarYuvFrame} does, with {@link NonPlanarVideoFrameError} for a frame no read can
 * copy; with `pool`, a failed copy gives its buffer back.
 *
 * @internal For `FrameSupply`.
 */
export async function readPlanarYuvFrame(
  frame: VideoFrame,
  declared: ColorMetadata | null,
  identity: PlanarFrameIdentity = {},
  pool?: PlanarCopyPool,
): Promise<PlanarYuvFrame> {
  const format = frame.format;
  if (format === null || !isPlanarVideoFrame(frame))
    throw new NonPlanarVideoFrameError(format);
  const visible = frame.visibleRect;
  if (!visible) throw new Error("frame has no visible rectangle");
  // copyTo copies the visible rectangle by default, so the copy is exactly the picture.
  const size = frame.allocationSize();
  const data = pool?.acquire(size) ?? new Uint8Array(size);
  let planes: PlaneLayout[];
  try {
    planes = await frame.copyTo(data);
  } catch (error) {
    pool?.release(data);
    throw error;
  }
  return {
    kind: "yuv-frame",
    format,
    codedWidth: visible.width,
    codedHeight: visible.height,
    data,
    layout: planes.flatMap((plane) => [plane.offset, plane.stride]),
    visibleRect: [0, 0, visible.width, visible.height],
    color: declared ?? colorMetadataFromVideoColorSpace(frame.colorSpace),
    chromaSiting: identity.chromaSiting ?? "unspecified",
    displayWidth: frame.displayWidth,
    displayHeight: frame.displayHeight,
    physicalTimeMicroseconds:
      identity.physicalTimeMicroseconds ?? frame.timestamp,
    ...(frame.duration === null
      ? {}
      : { durationMicroseconds: frame.duration }),
    ...(identity.rotation === undefined ? {} : { rotation: identity.rotation }),
    ...(identity.mirror === undefined ? {} : { mirror: identity.mirror }),
  };
}

/**
 * Uploads a planar frame for the engine to convert and present at
 * `outputWidth` x `outputHeight`, the size it is shown upright at. Throws what
 * the engine's `uploadYuvFrame` throws.
 */
export function uploadPlanarYuvFrame(
  engine: PlanarYuvEngine,
  sourceId: string,
  mediaTimeTicks: bigint,
  frame: PlanarYuvFrame,
  outputWidth: number,
  outputHeight: number,
): FrameUploadReceipt {
  return engine.uploadYuvFrame(
    sourceId,
    mediaTimeTicks,
    frame.format,
    frame.codedWidth,
    frame.codedHeight,
    frame.data,
    Uint32Array.from(frame.layout),
    Uint32Array.from(frame.visibleRect),
    frame.displayWidth,
    frame.displayHeight,
    JSON.stringify(frame.color),
    frame.chromaSiting,
    outputWidth,
    outputHeight,
    frame.rotation ?? 0,
    frame.mirror ?? false,
  );
}

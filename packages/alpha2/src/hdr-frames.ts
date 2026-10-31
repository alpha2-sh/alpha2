// Decoded frames reach the engine as planar YUV for its own shader to convert, because a canvas or
// external texture would tone-map or clip HDR and misplace subsampled chroma.

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

/** Frame metadata a planar copy carries beyond the `VideoFrame` itself. */
export interface PlanarFrameIdentity {
  readonly physicalTimeMicroseconds?: number;
  readonly rotation?: 0 | 90 | 180 | 270;
  readonly mirror?: boolean;
  readonly chromaSiting?: ChromaSiting;
}

/** A decoded frame copied out as planar YUV. */
export interface PlanarYuvFrame {
  readonly kind: "yuv-frame";
  /** WebCodecs pixel format. */
  readonly format: string;
  /** Size of the copied planes, which hold only the visible picture. */
  readonly codedWidth: number;
  readonly codedHeight: number;
  readonly data: Uint8Array;
  /** `[offset, stride]` per plane, in bytes. */
  readonly layout: readonly number[];
  /** `[x, y, width, height]` of the picture inside the coded frame. */
  readonly visibleRect: readonly [number, number, number, number];
  readonly color: ColorMetadata;
  readonly chromaSiting: ChromaSiting;
  /** Display size before rotation; carries the pixel aspect ratio. */
  readonly displayWidth: number;
  readonly displayHeight: number;
  readonly physicalTimeMicroseconds: number;
  readonly durationMicroseconds?: number;
  /** Clockwise turn that shows the frame upright, after {@link mirror}. */
  readonly rotation?: 0 | 90 | 180 | 270;
  /** Mirror left to right before rotating. */
  readonly mirror?: boolean;
}

/** Engine surface that converts planar YUV with the project's color pipeline. */
export interface PlanarYuvEngine {
  /** Uploads one planar frame, shown upright at `outputWidth` by `outputHeight`. */
  uploadYuvFrame(
    sourceId: string,
    mediaTimeTicks: bigint,
    format: string,
    codedWidth: number,
    codedHeight: number,
    data: Uint8Array,
    layout: Uint32Array,
    visibleRect: Uint32Array,
    /** Display size before rotation; carries the pixel aspect ratio. */
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
 * Converts a WebCodecs color space to engine color metadata. Unspecified parts default to SDR
 * BT.709 limited range; a PQ or HLG transfer marks the signal HDR.
 */
export function colorMetadataFromVideoColorSpace(
  space: VideoColorSpaceInit | null | undefined,
): ColorMetadata {
  // TypeScript's DOM library lacks the HDR enum values, so read the fields as strings.
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
 * How a host converts decoded SDR `VideoFrame`s.
 *
 * `"engine"` (default) copies planar frames with `copyTo` and converts them in the engine's shader,
 * matching a native export; opaque hardware frames still go through the browser. `"browser"`
 * imports SDR frames as textures, saving the copy (roughly 1 ms per 1080p frame, 4 to 5 ms per 4K
 * frame on a desktop GPU) but ignoring chroma siting, so chroma edges differ from a native export.
 * HDR frames and sources with project-declared color always use the engine.
 */
export type VideoConversion = "engine" | "browser";

/**
 * The frame has no planar YUV format the engine reads: it is opaque (`format` null) or RGB.
 * Retrying the read cannot help.
 */
export class NonPlanarVideoFrameError extends Error {
  readonly code = "NON_PLANAR_FRAME";
  readonly format: VideoPixelFormat | null;
  constructor(format: VideoPixelFormat | null) {
    super(
      `frame has no planar YUV format the engine reads (${format ?? "opaque"})`,
    );
    this.name = "NonPlanarVideoFrameError";
    this.format = format;
  }
}

/** Facts about a track the browser could not decode, used to build the refusal message. */
export interface TrackDecodeFacts {
  readonly kind: "video" | "audio";
  /** WebCodecs string (`hvc1.2.4.L93.B0`) or short name; null when unknown. */
  readonly codec: string | null;
  readonly bitDepth?: number;
  readonly chroma?: "420" | "422" | "444";
  /** Transfer (`pq`, `hlg`, `bt709`); null when undeclared. */
  readonly transfer?: string | null;
  readonly channels?: number;
  /** Speaker names (`FL FR FC`). */
  readonly layout?: readonly string[];
  /** Decoded frame format; null for an opaque frame. */
  readonly frameFormat?: VideoPixelFormat | null;
  readonly softwareDecoder?: "none" | "opaque" | "planar";
  /** The browser's error, for diagnostics only. */
  readonly browserError?: string;
}

/** A refused track is invalid input, since the same file always fails the same way. */
export interface DecodeRefusalFailure {
  readonly code: "INVALID_INPUT";
  readonly phase: "decode";
  readonly stage: "decode-unavailable" | "planar-decode-unavailable";
}

/** This browser cannot decode the track at all. */
export class DecodeUnavailableError extends Error {
  readonly code = "DECODE_UNAVAILABLE";
  readonly facts: TrackDecodeFacts;
  readonly failure: DecodeRefusalFailure = {
    code: "INVALID_INPUT",
    phase: "decode",
    stage: "decode-unavailable",
  };
  constructor(
    message: string,
    facts: TrackDecodeFacts,
    options?: ErrorOptions,
  ) {
    super(message, options);
    this.name = "DecodeUnavailableError";
    this.facts = facts;
  }
}

/**
 * The browser decodes the track only to opaque or RGB frames and has no planar software decoder,
 * but the source needs planes (HDR or project-declared color).
 */
export class PlanarDecodeUnavailableError extends Error {
  readonly code = "PLANAR_DECODE_UNAVAILABLE";
  readonly facts: TrackDecodeFacts;
  readonly failure: DecodeRefusalFailure = {
    code: "INVALID_INPUT",
    phase: "decode",
    stage: "planar-decode-unavailable",
  };
  constructor(
    message: string,
    facts: TrackDecodeFacts,
    options?: ErrorOptions,
  ) {
    super(message, options);
    this.name = "PlanarDecodeUnavailableError";
    this.facts = facts;
  }
}

const CODEC_NAMES: readonly (readonly [RegExp, string])[] = [
  [/^(hvc1|hev1|hevc)/i, "HEVC"],
  [/^(avc1|avc3|avc|h264)/i, "H.264"],
  [/^(av01|av1)/i, "AV1"],
  [/^(vp09|vp9)/i, "VP9"],
  [/^vp8/i, "VP8"],
  [/^(mp4a|aac)/i, "AAC"],
  [/^opus/i, "Opus"],
  [/^vorbis/i, "Vorbis"],
  [/^flac/i, "FLAC"],
];

/** Human-readable codec name: `hvc1.2.4.L93.B0` is `HEVC`. */
export function codecName(codec: string | null): string {
  if (codec === null || codec === "") return "unknown";
  return CODEC_NAMES.find(([pattern]) => pattern.test(codec))?.[1] ?? codec;
}

/** Human-readable bit depth and chroma, such as `10-bit` and `4:2:2`. */
export function pictureDetails(facts: TrackDecodeFacts): string[] {
  return [
    ...(facts.bitDepth === undefined ? [] : [`${facts.bitDepth}-bit`]),
    ...(facts.chroma === undefined || facts.chroma === "420"
      ? []
      : [`4:${facts.chroma.slice(1, 2)}:${facts.chroma.slice(2)}`]),
  ];
}

/** User-facing refusal message built from `facts` alone, with a suggested workaround. */
export function decodeRefusalMessage(
  name: string,
  facts: TrackDecodeFacts,
): string {
  const codec = codecName(facts.codec);
  if (facts.kind === "audio") {
    const channels =
      facts.channels === undefined
        ? ""
        : ` with ${facts.channels} channel${facts.channels === 1 ? "" : "s"}${
            facts.layout && facts.layout.length > 0
              ? ` (${facts.layout.join(" ")})`
              : ""
          }`;
    const stereo =
      facts.channels !== undefined && facts.channels > 2
        ? " or convert it to stereo"
        : " or convert it to AAC";
    return `${name}: this browser cannot decode ${codec} audio${channels}. Use another browser${stereo}.`;
  }
  const hdr = facts.transfer === "pq" || facts.transfer === "hlg";
  const details = [
    codec,
    ...pictureDetails(facts),
    ...(hdr ? [facts.transfer === "pq" ? "PQ" : "HLG"] : []),
  ].join(", ");
  const what = `${name} is ${hdr ? "HDR " : ""}video (${details})`;
  if (facts.softwareDecoder !== undefined || facts.frameFormat !== undefined)
    return `${what}: this browser decodes it only to frames the engine cannot read and has no software decoder that gives planes. Convert it to H.264 High 10 or AV1 10-bit, or export with the native host.`;
  return `${what}: this browser cannot decode it. Convert it to H.264 or AV1, use another browser, or export with the native host.`;
}

/** Whether `copyPlanarYuvFrame` can copy `frame`. */
export function isPlanarVideoFrame(frame: VideoFrame): boolean {
  return frame.format !== null && PLANAR_FORMATS.has(frame.format);
}

/** Whether the frame's own color space is HDR. */
export function isHdrVideoFrame(frame: VideoFrame): boolean {
  const transfer: string | null | undefined = frame.colorSpace?.transfer;
  return transfer === "pq" || transfer === "hlg";
}

/**
 * Reusable buffers for planar copies, avoiding a fresh zero-filled allocation per frame (25 MB for
 * a 4K 10-bit frame). Release a buffer once its upload returns, since the engine copies the bytes
 * into wasm memory.
 */
export class PlanarCopyPool {
  readonly #maxFree: number;
  #free: ArrayBuffer[] = [];

  /** Keeps at most `maxFree` idle buffers. */
  constructor(maxFree = 2) {
    if (!Number.isSafeInteger(maxFree) || maxFree < 0)
      throw new RangeError("maxFree must be a non-negative safe integer");
    this.#maxFree = maxFree;
  }

  /** Returns a view of `bytes` bytes over a kept buffer, or a new one. */
  acquire(bytes: number): Uint8Array {
    // Sorted smallest first, so this finds the best fit.
    const index = this.#free.findIndex((buffer) => buffer.byteLength >= bytes);
    const kept = index < 0 ? undefined : this.#free.splice(index, 1)[0];
    return new Uint8Array(kept ?? new ArrayBuffer(bytes), 0, bytes);
  }

  /** Returns the buffer under `data` to the pool; `data` must not be used afterwards. */
  release(data: Uint8Array): void {
    const buffer = data.buffer;
    if (!(buffer instanceof ArrayBuffer) || this.#free.includes(buffer)) return;
    this.#free.push(buffer);
    this.#free.sort((left, right) => left.byteLength - right.byteLength);
    // Evict the smallest, since larger buffers serve every size.
    this.#free.splice(0, Math.max(0, this.#free.length - this.#maxFree));
  }

  clear(): void {
    this.#free = [];
  }

  /** Total bytes of idle buffers. */
  get keptBytes(): number {
    return this.#free.reduce((sum, buffer) => sum + buffer.byteLength, 0);
  }
}

/**
 * Copies `frame` out as planar YUV and always closes it. `declared` color overrides the frame's own
 * color space.
 *
 * With `pool`, `data` comes from the pool and the caller releases it after uploading.
 *
 * Rejects with {@link NonPlanarVideoFrameError} for a frame without a planar format, and otherwise
 * when it has no visible rectangle or `copyTo` fails.
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
 * {@link copyPlanarYuvFrame} without closing `frame`, so a failed readback can be retried.
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
  // `copyTo` copies only the visible rectangle by default.
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

/** Uploads a planar frame, shown upright at `outputWidth` by `outputHeight`. */
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

/**
 * Export containers and the codecs each offers.
 *
 * Mirrors the engine's `alpha2_media::Container` policy so requests can be validated before an
 * encoder or host starts. A container offers only what mainstream players decode in it, which is
 * narrower than what mediabunny can write (QuickTime does not play VP9, AV1 or Opus in MOV).
 */

import {
  engineVideoCodec,
  type BrowserVideoCodec,
  type EngineVideoCodec,
} from "./rate-control.js";

export type ExportContainer = "mp4" | "mov" | "mkv" | "webm";

export type ExportAudioCodec = "aac" | "opus";

/** One container's policy. */
export interface ContainerPolicy {
  readonly id: ExportContainer;
  /** Lowercase extensions without the dot; the first is used for exported files. */
  readonly extensions: readonly string[];
  readonly mimeType: string;
  /** Video codecs in the engine's spelling, default first. */
  readonly video: readonly EngineVideoCodec[];
  /** Audio codecs, default first; also the browser's fallback order. */
  readonly audio: readonly ExportAudioCodec[];
}

export const EXPORT_CONTAINERS: readonly ContainerPolicy[] = [
  {
    id: "mp4",
    extensions: ["mp4", "m4v"],
    mimeType: "video/mp4",
    video: ["h264", "hevc", "vp9", "av1", "mpeg4-part2"],
    audio: ["aac", "opus"],
  },
  {
    id: "mov",
    extensions: ["mov"],
    mimeType: "video/quicktime",
    video: ["h264", "hevc", "mpeg4-part2", "prores"],
    audio: ["aac"],
  },
  {
    id: "mkv",
    extensions: ["mkv"],
    mimeType: "video/x-matroska",
    video: ["h264", "hevc", "vp9", "av1", "mpeg4-part2"],
    audio: ["aac", "opus"],
  },
  {
    id: "webm",
    extensions: ["webm"],
    mimeType: "video/webm",
    video: ["vp9", "av1"],
    audio: ["opus"],
  },
];

/** Why an export format request cannot be resolved. */
export type ExportFormatErrorReason =
  | "unknown"
  | "unknown-extension"
  | "extension-mismatch"
  | "video-codec-not-offered"
  | "audio-codec-not-offered"
  | "alpha-unsupported"
  | "alpha-unsupported-in-container"
  | "bit-depth-unsupported";

/** Thrown when a container or codec request is not allowed. */
export class ExportFormatError extends RangeError {
  readonly reason: ExportFormatErrorReason;

  constructor(reason: ExportFormatErrorReason, message: string) {
    super(message);
    this.name = "ExportFormatError";
    this.reason = reason;
  }
}

/** @throws ExportFormatError When `container` is not one of {@link EXPORT_CONTAINERS}. */
export function containerPolicy(container: string): ContainerPolicy {
  const policy = EXPORT_CONTAINERS.find((entry) => entry.id === container);
  if (policy === undefined)
    throw new ExportFormatError(
      "unknown",
      `unknown container "${container}"; expected one of ${EXPORT_CONTAINERS.map((entry) => entry.id).join(", ")}`,
    );
  return policy;
}

/** Looks up a container by extension (without the dot), case-insensitively. */
export function containerFromExtension(
  extension: string,
): ExportContainer | undefined {
  const lower = extension.toLowerCase();
  return EXPORT_CONTAINERS.find((entry) => entry.extensions.includes(lower))
    ?.id;
}

/** Default browser video codec for `container`. */
export function defaultVideoCodec(
  container: ExportContainer,
): BrowserVideoCodec {
  return browserVideoCodec(containerPolicy(container).video[0]);
}

/** Audio codecs `container` offers, in browser fallback order. */
export function offeredAudioCodecs(
  container: ExportContainer,
): readonly ExportAudioCodec[] {
  return containerPolicy(container).audio;
}

/** A resolved export format, in the engine's codec spelling. */
export interface EngineExportFormat {
  readonly container: ExportContainer;
  readonly videoCodec: EngineVideoCodec;
  readonly audioCodec: ExportAudioCodec;
}

/**
 * Resolves an export's container and codecs in the engine's codec spelling (`h264`), matching the
 * engine's `resolve_export_format`.
 *
 * - The container comes from `container`, else from `extension`, else MP4.
 * - A given container and extension must agree.
 * - A missing codec takes the container's default; a given one must be offered.
 *
 * @throws ExportFormatError When the request breaks one of these rules.
 */
export function resolveEngineExportFormat(request: {
  readonly container?: string;
  /** The output's extension without the dot, empty when it has none. */
  readonly extension?: string;
  readonly videoCodec?: string;
  readonly audioCodec?: string;
}): EngineExportFormat {
  const policy = resolveContainer(request.container, request.extension);
  const video = request.videoCodec ?? policy.video[0];
  if (!(policy.video as readonly string[]).includes(video))
    throw new ExportFormatError(
      "video-codec-not-offered",
      `${policy.id} does not offer video codec ${video}; it offers ${policy.video.join(", ")}`,
    );
  const audio = request.audioCodec ?? policy.audio[0];
  if (!(policy.audio as readonly string[]).includes(audio))
    throw new ExportFormatError(
      "audio-codec-not-offered",
      `${policy.id} does not offer audio codec ${audio}; it offers ${policy.audio.join(", ")}`,
    );
  return {
    container: policy.id,
    videoCodec: video as EngineVideoCodec,
    audioCodec: audio as ExportAudioCodec,
  };
}

/** A resolved export format, in the browser's codec spelling. */
export interface ExportFormat {
  readonly container: ExportContainer;
  readonly videoCodec: BrowserVideoCodec;
  readonly audioCodec: ExportAudioCodec;
}

/**
 * {@link resolveEngineExportFormat} in the browser's codec spelling (`avc`).
 *
 * @throws ExportFormatError When the request breaks one of its rules.
 */
export function resolveExportFormat(request: {
  readonly container?: string;
  /** The output's extension without the dot, empty when it has none. */
  readonly extension?: string;
  readonly videoCodec?: BrowserVideoCodec;
  readonly audioCodec?: ExportAudioCodec;
}): ExportFormat {
  const { videoCodec, ...rest } = request;
  const format = resolveEngineExportFormat({
    ...rest,
    ...(videoCodec === undefined
      ? {}
      : { videoCodec: engineVideoCodec(videoCodec) }),
  });
  return {
    container: format.container,
    videoCodec: browserVideoCodec(format.videoCodec),
    audioCodec: format.audioCodec,
  };
}

function resolveContainer(
  container: string | undefined,
  extension: string | undefined,
): ContainerPolicy {
  if (container !== undefined) {
    const policy = containerPolicy(container);
    if (
      extension !== undefined &&
      !policy.extensions.includes(extension.toLowerCase())
    ) {
      const found = extension === "" ? "no extension" : `".${extension}"`;
      throw new ExportFormatError(
        "extension-mismatch",
        `a ${policy.id} output must end in ${dotted(policy.extensions)}, not ${found}`,
      );
    }
    return policy;
  }
  if (extension === undefined) return containerPolicy("mp4");
  const named = containerFromExtension(extension);
  if (named !== undefined) return containerPolicy(named);
  const every = dotted(EXPORT_CONTAINERS.flatMap((entry) => entry.extensions));
  throw new ExportFormatError(
    "unknown-extension",
    extension === ""
      ? `the output has no extension to name its container; use one of ${every}`
      : `the output extension ".${extension}" names no container; use one of ${every}`,
  );
}

function dotted(extensions: readonly string[]): string {
  return extensions.map((extension) => `.${extension}`).join(", ");
}

function browserVideoCodec(codec: EngineVideoCodec): BrowserVideoCodec {
  if (codec === "h264") return "avc";
  if (codec === "mpeg4-part2" || codec === "prores")
    throw new ExportFormatError(
      "video-codec-not-offered",
      `the browser has no ${codec} encoder`,
    );
  return codec;
}

/** Output pixel layout, matching the engine's `PixelLayout`. */
export interface PixelLayout {
  readonly chroma: "420" | "422" | "444";
  readonly bitDepth: 8 | 10;
  readonly alpha: boolean;
  /** `FFmpeg` pixel format name, such as `yuv420p` or `yuva444p10le`. */
  readonly format: string;
}

/**
 * Resolves the pixel layout for `codec` in `container`, matching the engine's `resolve_pixels`.
 * `bitDepth` defaults to 10 for ProRes and 8 otherwise.
 *
 * H.264 and MPEG-4 Part 2 write 8-bit 4:2:0. HEVC and AV1 also allow 10-bit. VP9 allows 10-bit, or
 * 8-bit alpha outside MP4. ProRes is 10-bit 4:2:2 (422 HQ), or 4:4:4 with alpha (4444). The
 * container must offer the codec ({@link resolveEngineExportFormat} checks that).
 *
 * @throws ExportFormatError When the codec cannot be written with this depth and alpha.
 */
export function resolvePixels(
  codec: EngineVideoCodec,
  container: ExportContainer,
  request: { readonly bitDepth?: number; readonly alpha?: boolean } = {},
): PixelLayout {
  const bitDepth = request.bitDepth ?? (codec === "prores" ? 10 : 8);
  if (bitDepth !== 8 && bitDepth !== 10)
    throw new ExportFormatError(
      "bit-depth-unsupported",
      `bitDepth: ${bitDepth} is not 8 or 10`,
    );
  const alpha = request.alpha === true;
  const refuseDepth = () =>
    new ExportFormatError(
      "bit-depth-unsupported",
      `video codec ${codec} cannot be written at ${bitDepth} bits${alpha ? " with alpha" : ""}`,
    );
  let chroma: PixelLayout["chroma"] = "420";
  if (codec === "prores") {
    if (bitDepth === 8) throw refuseDepth();
    chroma = alpha ? "444" : "422";
  } else if (codec === "vp9") {
    if (alpha && container === "mp4")
      throw new ExportFormatError(
        "alpha-unsupported-in-container",
        `${container} cannot carry vp9 alpha; use webm or mkv`,
      );
    if (alpha && bitDepth === 10) throw refuseDepth();
  } else {
    if (alpha)
      throw new ExportFormatError(
        "alpha-unsupported",
        `video codec ${codec} cannot keep alpha; use vp9 (webm, mkv) or prores (mov)`,
      );
    if (bitDepth === 10 && codec !== "hevc" && codec !== "av1")
      throw refuseDepth();
  }
  return {
    chroma,
    bitDepth,
    alpha,
    format: `yuv${alpha ? "a" : ""}${chroma}p${bitDepth === 10 ? "10le" : ""}`,
  };
}

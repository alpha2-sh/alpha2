/**
 * Export containers: the file formats an export is written in, and which codecs each one offers.
 *
 * This is the engine's container policy (`alpha2_media::Container`), mirrored here as a constant so
 * a browser sink, or a client of a native host, can validate a request before it opens an encoder
 * or starts a host. Both copies are pinned by the shared corpus
 * `tests/cases/export-container-corpus.json`. A container offers only the codecs its mainstream
 * players decode in it, which is narrower than what mediabunny writes: QuickTime does not play
 * VP9, AV1 or Opus in a MOV. ProRes is a QuickTime codec, offered in MOV only.
 */

import {
  engineVideoCodec,
  type BrowserVideoCodec,
  type EngineVideoCodec,
} from "./rate-control.js";

/** The containers an export can be written in. */
export type ExportContainer = "mp4" | "mov" | "mkv" | "webm";

/** The audio codecs an export can carry. */
export type ExportAudioCodec = "aac" | "opus";

/** One container's policy. */
export interface ContainerPolicy {
  readonly id: ExportContainer;
  /** Lowercase file extensions without the dot; the first names an exported file. */
  readonly extensions: readonly string[];
  readonly mimeType: string;
  /** Offered video codecs in the engine's spelling, the default first. */
  readonly video: readonly EngineVideoCodec[];
  /** Offered audio codecs, the default first; also the order a browser falls back in. */
  readonly audio: readonly ExportAudioCodec[];
}

/** Every container's policy, in the engine's order. */
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

/** A container or codec request the policy refuses. */
export class ExportFormatError extends RangeError {
  readonly reason: ExportFormatErrorReason;

  constructor(reason: ExportFormatErrorReason, message: string) {
    super(message);
    this.name = "ExportFormatError";
    this.reason = reason;
  }
}

/**
 * The policy of a container.
 *
 * @throws ExportFormatError When `container` is not one of {@link EXPORT_CONTAINERS}.
 */
export function containerPolicy(container: string): ContainerPolicy {
  const policy = EXPORT_CONTAINERS.find((entry) => entry.id === container);
  if (policy === undefined)
    throw new ExportFormatError(
      "unknown",
      `unknown container "${container}"; expected one of ${EXPORT_CONTAINERS.map((entry) => entry.id).join(", ")}`,
    );
  return policy;
}

/** The container a file extension (without the dot) names, case-insensitively. */
export function containerFromExtension(
  extension: string,
): ExportContainer | undefined {
  const lower = extension.toLowerCase();
  return EXPORT_CONTAINERS.find((entry) => entry.extensions.includes(lower))
    ?.id;
}

/** The video codec a browser export in `container` gets when it names none. */
export function defaultVideoCodec(
  container: ExportContainer,
): BrowserVideoCodec {
  return browserVideoCodec(containerPolicy(container).video[0]);
}

/** The audio codecs `container` offers, in the order a browser tries them. */
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
 * Resolves an export's container and codecs, in the engine's codec spelling (`h264`): the same rule
 * as the engine's `resolve_export_format`, which a native host applies to the same request.
 *
 * - `container` names the container. When it is absent, `extension` names it, and with neither
 *   (a browser export, which writes no path) it is MP4.
 * - A named container and an extension must agree.
 * - An absent codec is the container's default, and a named one must be one it offers.
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
 * {@link resolveEngineExportFormat} in the browser's codec spelling (`avc`), for a browser sink.
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

/** The pictures an export writes: the engine's `PixelLayout`. */
export interface PixelLayout {
  readonly chroma: "420" | "422" | "444";
  readonly bitDepth: 8 | 10;
  readonly alpha: boolean;
  /** The FFmpeg pixel format name, such as `yuv420p` or `yuva444p10le`. */
  readonly format: string;
}

/**
 * The pictures `codec` writes in `container` for a request of `bitDepth` (absent: the codec's own,
 * 10 for ProRes, else 8) and `alpha`: the engine's `resolve_pixels`, pinned to it by the shared
 * corpus. H.264 and MPEG-4 Part 2 write 8-bit 4:2:0; HEVC and AV1 also 10-bit; VP9 also 10-bit,
 * and 8-bit alpha outside MP4; ProRes is 10-bit 4:2:2 (422 HQ), or 4:4:4 with alpha (4444).
 * The container must offer the codec ({@link resolveEngineExportFormat} checks that first).
 *
 * @throws ExportFormatError When the codec cannot be written so.
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

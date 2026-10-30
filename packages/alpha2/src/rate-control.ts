/**
 * Output rate control for browser exports, resolved by the engine.
 *
 * The engine owns what a quality level means: `resolveRateControl` and `resolveAudioBitrate` in
 * `alpha2/wasm` are the same functions the native host calls, so a browser export and a native
 * export at one level and one output receive the same numbers. An app resolves once, at the top of
 * an export, and hands the numbers to whichever sink it built; a sink never sees a level, and
 * neither does anything in this package; the level names come from the engine too, through
 * `qualityLevels`.
 */

/** The rate-control exports of the `alpha2/wasm` binding. */
export interface RateControlBinding {
  resolveRateControl(
    codec: string,
    width: number,
    height: number,
    frameRateNum: number,
    frameRateDen: number,
    level: string,
  ): string;
  resolveAudioBitrate(codec: string, level: string): number;
  qualityLevels(): string;
}

/** The engine's spellings of the video codecs it resolves a rate for. */
export type EngineVideoCodec =
  "h264" | "hevc" | "vp9" | "av1" | "mpeg4-part2" | "prores";

/** The WebCodecs/mediabunny spellings of the same codecs. */
export type BrowserVideoCodec = "avc" | "hevc" | "vp9" | "av1";

/** A resolved video rate control, in the shape a browser sink receives. */
export interface VideoRateControl {
  /** Target, or with a quantizer the fallback, bits per second. */
  readonly bitrate: number;
  readonly bitrateMode: "constant" | "variable";
  /** Constant quantizer on the codec's scale, where the engine chose one. */
  readonly quantizer?: number;
}

/** A level name, or an explicit rate. */
export type VideoQualityRequest =
  | string
  | {
      readonly bitrate: number;
      readonly mode?: "constant" | "variable";
      /** A constant quantizer, with `bitrate` as its fallback. */
      readonly quantizer?: number;
    };

/** A level name, or an explicit stereo bitrate. */
export type AudioQualityRequest = string | { readonly bitrate: number };

/**
 * Maps a browser codec name onto the engine's.
 *
 * This is the one place `avc` becomes `h264`: the engine's mapping is keyed on one codec
 * vocabulary, and the browser's is translated at this edge.
 */
export function engineVideoCodec(codec: BrowserVideoCodec): EngineVideoCodec {
  return codec === "avc" ? "h264" : codec;
}

/** The engine's quality level names, lowest first. */
export function qualityLevels(binding: RateControlBinding): readonly string[] {
  return JSON.parse(binding.qualityLevels()) as string[];
}

/**
 * Resolves a video quality request: a level name through the engine, for this
 * codec, size, and rate; an explicit rate as given, `variable` by default.
 *
 * @throws RangeError When an explicit `bitrate` is not a positive integer.
 * @throws When the engine does not know the level or codec.
 */
export function resolveVideoRateControl(
  binding: RateControlBinding,
  request: {
    readonly codec: EngineVideoCodec;
    readonly width: number;
    readonly height: number;
    readonly frameRate: { readonly num: number; readonly den: number };
    readonly quality: VideoQualityRequest;
  },
): VideoRateControl {
  const { quality } = request;
  if (typeof quality !== "string") {
    positiveBitrate("videoQuality", quality.bitrate);
    return {
      bitrate: quality.bitrate,
      bitrateMode: quality.mode ?? "variable",
      ...(quality.quantizer === undefined
        ? {}
        : { quantizer: quality.quantizer }),
    };
  }
  const resolved = JSON.parse(
    binding.resolveRateControl(
      request.codec,
      request.width,
      request.height,
      request.frameRate.num,
      request.frameRate.den,
      quality,
    ),
  ) as {
    bitrate: number;
    mode: "constant" | "variable";
    quantizer: number | null;
  };
  return {
    bitrate: resolved.bitrate,
    bitrateMode: resolved.mode,
    ...(resolved.quantizer === null ? {} : { quantizer: resolved.quantizer }),
  };
}

/**
 * Resolves an audio quality request to bits per second: a level name through
 * the engine, an explicit bitrate as given.
 *
 * @throws RangeError When an explicit `bitrate` is not a positive integer.
 * @throws When the engine does not know the level.
 */
export function resolveAudioBitrate(
  binding: RateControlBinding,
  codec: "aac" | "opus",
  quality: AudioQualityRequest,
): number {
  if (typeof quality !== "string") {
    positiveBitrate("audioQuality", quality.bitrate);
    return quality.bitrate;
  }
  return binding.resolveAudioBitrate(codec, quality);
}

function positiveBitrate(field: string, bitrate: number): void {
  if (!Number.isSafeInteger(bitrate) || bitrate <= 0)
    throw new RangeError(
      `${field}.bitrate must be a positive integer number of bits per second`,
    );
}

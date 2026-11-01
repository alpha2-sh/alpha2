/**
 * Output rate control for browser exports. Quality levels resolve through the same engine functions
 * the native host calls, so both get the same numbers. Resolve once per export and pass the numbers
 * to the sink.
 */

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

export type EngineVideoCodec =
  "h264" | "hevc" | "vp9" | "av1" | "mpeg4-part2" | "prores";

/** WebCodecs/mediabunny codec names. */
export type BrowserVideoCodec = "avc" | "hevc" | "vp9" | "av1";

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

/** Maps a browser codec name onto the engine's (`avc` becomes `h264`). */
export function engineVideoCodec(codec: BrowserVideoCodec): EngineVideoCodec {
  return codec === "avc" ? "h264" : codec;
}

/** The engine's quality level names, lowest first. */
export function qualityLevels(binding: RateControlBinding): readonly string[] {
  return JSON.parse(binding.qualityLevels()) as string[];
}

/**
 * Resolves a level name through the engine for this codec, size and rate. An explicit rate is
 * returned as given, `variable` by default.
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
 * Resolves an audio quality request to bits per second.
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

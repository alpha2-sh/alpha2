// The decoder policy both browser video decoders follow: a decoder the browser picked that fails is
// replaced, once, by a software decoder, which decodes the request again; and a track no decoder
// gave a single frame of is one this browser cannot decode, refused as such rather than as an
// unknown failure that a retry would meet again.

import {
  DecodeUnavailableError,
  decodeRefusalMessage,
  type TrackDecodeFacts,
} from "./hdr-frames.js";

/**
 * A `VideoDecoder` failure, as WebCodecs reports one: an undecodable stream or a decoder that could
 * not be configured or broke. Aborts, stalls, and the stream ending are not.
 */
export function isDecoderFailure(error: unknown): boolean {
  return (
    error instanceof DOMException &&
    ["EncodingError", "NotSupportedError", "OperationError"].includes(
      error.name,
    )
  );
}

/**
 * Runs `decodeOnce`; when it fails with a decoder failure ({@link isDecoderFailure}) and
 * `reopenSoftware` is given (the decoder is not software already), reopens in software and runs
 * `decodeOnce` once more. Rejects with the failure otherwise, and with the software attempt's.
 */
export async function withSoftwareFallback<T>(
  decodeOnce: () => Promise<T>,
  reopenSoftware: ((cause: DOMException) => Promise<void>) | undefined,
): Promise<T> {
  try {
    return await decodeOnce();
  } catch (error) {
    if (reopenSoftware === undefined || !isDecoderFailure(error)) throw error;
    await reopenSoftware(error as DOMException);
    return decodeOnce();
  }
}

/**
 * What a decoder failure becomes when no decoder ever gave a frame of the track, the software one
 * included: {@link DecodeUnavailableError} (a browser that says it decodes a codec and then refuses
 * its configuration at the first frame). A failure after frames were decoded is left as it is: the
 * stream, or the decoder, broke mid-way.
 */
export function decodeFailureOf(
  error: unknown,
  name: string,
  facts: TrackDecodeFacts,
  decodedAny: boolean,
): unknown {
  if (decodedAny || !isDecoderFailure(error)) return error;
  const refused = {
    ...facts,
    browserError: `${(error as DOMException).name}: ${(error as DOMException).message}`,
  };
  return new DecodeUnavailableError(
    decodeRefusalMessage(name, refused),
    refused,
    {
      cause: error,
    },
  );
}

// Shared decoder failure policy: retry once in software, and report a track that never produced a
// frame as undecodable rather than as a retryable failure.

import {
  DecodeUnavailableError,
  decodeRefusalMessage,
  type TrackDecodeFacts,
} from "./hdr-frames.js";

/** Whether `error` is a WebCodecs decoder failure (not an abort, stall or end of stream). */
export function isDecoderFailure(error: unknown): boolean {
  return (
    error instanceof DOMException &&
    ["EncodingError", "NotSupportedError", "OperationError"].includes(
      error.name,
    )
  );
}

/**
 * Runs `decodeOnce`; on a decoder failure, if `reopenSoftware` is given, reopens in software and
 * runs it once more.
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
 * Turns a decoder failure into {@link DecodeUnavailableError} when no frame of the track was ever
 * decoded. Other errors are returned unchanged.
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

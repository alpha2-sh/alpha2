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
 * runs it once more. When that attempt fails in its decoder too, `restore` runs before the
 * rejection, so the next request starts from the original choice.
 */
export async function withSoftwareFallback<T>(
  decodeOnce: () => Promise<T>,
  reopenSoftware: ((cause: DOMException) => Promise<void>) | undefined,
  restore?: () => Promise<void>,
): Promise<T> {
  try {
    return await decodeOnce();
  } catch (error) {
    if (reopenSoftware === undefined || !isDecoderFailure(error)) throw error;
    await reopenSoftware(error as DOMException);
    try {
      return await decodeOnce();
    } catch (retry) {
      if (restore !== undefined && isDecoderFailure(retry)) await restore();
      throw retry;
    }
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

import { recordCleanupFailure } from "./export-failure.js";
import type { BrowserAudioBlock } from "./media.js";

/**
 * Where {@link renderOfflineAudio} delivers rendered audio, owned by the host
 * (an encoder, a file writer, a test collector).
 */
export interface BrowserAudioBlockSink {
  /**
   * Takes the next block, in timeline order. The next block is not rendered
   * until the returned promise settles; a rejection aborts the render.
   */
  writeAudio(block: BrowserAudioBlock): Promise<void>;
  /** Called once after the last block was written. */
  finish(): Promise<void>;
  /**
   * Called once instead of `finish` when rendering, writing, or finishing
   * fails or is aborted; `reason` is the error the render rejects with.
   */
  abort(reason: Error): Promise<void> | void;
}

/**
 * Renders `frameCount` frames of the project's audio starting at project-rate
 * sample `startSample`. Every channel of the returned block must hold exactly
 * `frameCount` samples.
 */
export type OfflineAudioRenderer = (
  startSample: bigint,
  frameCount: number,
) => Promise<BrowserAudioBlock>;

/**
 * Renders `totalFrames` frames through `render` into `sink`, in blocks whose
 * sizes cycle through `blockSizes`, so a schedule of irregular block sizes is
 * reproduced exactly on every run.
 *
 * Blocks are rendered and written one at a time. On success the sink's
 * `finish` is awaited; on any failure, including `signal` aborting before the
 * promise resolves (during the last block or `finish` included), the sink's
 * `abort` is awaited with the error and the promise rejects with it
 * (`signal.reason`, or an `AbortError`, for an abort). An `abort` that throws
 * does not replace that error ({@link recordCleanupFailure}).
 *
 * @throws RangeError When `totalFrames` is negative, `blockSizes` is empty or
 *   holds a size that is not a positive safe integer (before the sink is
 *   touched), or `render` returns a block of the wrong length.
 */
export async function renderOfflineAudio(
  totalFrames: bigint,
  blockSizes: readonly number[],
  render: OfflineAudioRenderer,
  sink: BrowserAudioBlockSink,
  signal?: AbortSignal,
): Promise<void> {
  if (
    totalFrames < 0n ||
    blockSizes.length === 0 ||
    blockSizes.some((size) => !Number.isSafeInteger(size) || size <= 0)
  ) {
    throw new RangeError("invalid offline audio schedule");
  }
  let position = 0n;
  let block = 0;
  // Checked around each block and on each side of `finish`, so a late cancel never reports success.
  const throwIfAborted = (): void => {
    if (signal?.aborted === true)
      throw signal.reason ?? new DOMException("Aborted", "AbortError");
  };
  try {
    while (position < totalFrames) {
      throwIfAborted();
      const remaining = totalFrames - position;
      const requested = blockSizes[block % blockSizes.length];
      const frames = Number(
        remaining < BigInt(requested) ? remaining : BigInt(requested),
      );
      const output = await render(position, frames);
      if (output.channels.some((channel) => channel.length !== frames))
        throw new RangeError("renderer returned an invalid audio block");
      throwIfAborted();
      await sink.writeAudio(output);
      position += BigInt(frames);
      block += 1;
    }
    throwIfAborted();
    await sink.finish();
    throwIfAborted();
  } catch (error) {
    const failure = error instanceof Error ? error : new Error(String(error));
    try {
      await sink.abort(failure);
    } catch (cleanup) {
      recordCleanupFailure(failure, cleanup);
    }
    throw failure;
  }
}

// Stall limits shared by the video and audio decoder adapters.
import { activeTimeout } from "./active-clock.js";

/** A decoder stall: `DECODE_STALLED`, classed as a GPU timeout in the decode phase. */
export function decoderStalled(message: string, stage: string): Error {
  return Object.assign(new Error(message), {
    code: "DECODE_STALLED",
    failure: {
      code: "GPU_TIMEOUT",
      phase: "decode",
      stage,
      resource: "unknown",
    },
  });
}

/**
 * The iterator's next sample, raced against `stallMs` of active time (a frozen page does not
 * count) and `signal`. On a stall (`DECODE_STALLED`) or an abort (the signal's reason) the
 * iterator is ended and a sample that arrives late is closed.
 */
export async function nextSampleOrStall<T extends { close(): void }>(
  iterator: AsyncIterator<T>,
  {
    stallMs,
    signal,
    sourceId,
    kind,
  }: {
    readonly stallMs: number;
    readonly signal?: AbortSignal | undefined;
    readonly sourceId: string;
    readonly kind: "video" | "audio";
  },
): Promise<IteratorResult<T>> {
  const pending = iterator.next();
  const settled = new AbortController();
  let abort: (() => void) | undefined;
  const interrupted = new Promise<never>((_, reject) => {
    void activeTimeout(stallMs, settled.signal).then(() =>
      reject(
        decoderStalled(
          `${kind} decoder ${sourceId} stalled: no ${kind === "video" ? "frame" : "samples"} for ${stallMs} ms`,
          `${kind}-decoder-stalled`,
        ),
      ),
    );
    if (signal === undefined) return;
    abort = () =>
      reject(
        signal.reason instanceof Error
          ? signal.reason
          : new DOMException("Aborted", "AbortError"),
      );
    if (signal.aborted) abort();
    else signal.addEventListener("abort", abort, { once: true });
  });
  try {
    return await Promise.race([pending, interrupted]);
  } catch (error) {
    void pending.then(
      (late) => {
        if (!late.done) late.value.close();
      },
      () => {},
    );
    void iterator.return?.()?.catch(() => {});
    throw error;
  } finally {
    settled.abort();
    if (abort !== undefined) signal?.removeEventListener("abort", abort);
  }
}

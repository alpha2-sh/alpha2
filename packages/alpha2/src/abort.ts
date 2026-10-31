/**
 * Cancellation shared by the package's loops: the error an aborted signal carries, and a race that
 * lets a cancel end a wait on work that may never settle (a GPU readback, a decoder, a write).
 */

/**
 * The reason a signal carries, as an `Error` a caller can reject with. An `Error` passes through
 * untouched, so an `AbortError` keeps its name; a string becomes an `AbortError` with that text,
 * and anything else a plain `AbortError`.
 */
export function abortError(signal: AbortSignal): Error {
  const reason: unknown = signal.reason;
  if (reason instanceof Error) return reason;
  if (typeof reason === "string") return new DOMException(reason, "AbortError");
  return new DOMException("Aborted", "AbortError");
}

/** Throws {@link abortError} when `signal` has aborted. */
export function throwIfAborted(signal?: AbortSignal): void {
  if (signal?.aborted === true) throw abortError(signal);
}

/**
 * `work`, unless `signal` aborts first: then rejects at once with `cancelled()` (default
 * {@link abortError}). Work that settles after the cancel is not waited for; a value it brings is
 * given to `onLate`, which releases what it holds (a `VideoFrame`, a readback).
 */
export async function raceAbort<T>(
  work: Promise<T>,
  signal: AbortSignal | undefined,
  onLate?: (value: T) => void,
  cancelled: (signal: AbortSignal) => Error = abortError,
): Promise<T> {
  if (signal === undefined) return work;
  let lost = false;
  void work.then(
    (value) => {
      if (lost) onLate?.(value);
    },
    () => {},
  );
  let abort: (() => void) | undefined;
  const aborted = new Promise<never>((_, reject) => {
    abort = (): void => {
      lost = true;
      reject(cancelled(signal));
    };
    if (signal.aborted) abort();
    else signal.addEventListener("abort", abort, { once: true });
  });
  try {
    return await Promise.race([work, aborted]);
  } finally {
    if (abort !== undefined) signal.removeEventListener("abort", abort);
  }
}

/**
 * The signal's reason as an `Error`. An `Error` reason passes through; anything else becomes a
 * `DOMException` named `AbortError`.
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
 * Resolves with `work`, or rejects with `cancelled(signal)` as soon as `signal` aborts. A value that
 * arrives after the abort is passed to `onLate` so its resources can be released.
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

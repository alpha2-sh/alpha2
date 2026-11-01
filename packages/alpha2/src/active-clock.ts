/**
 * Timeouts that count active time, not wall-clock time.
 *
 * A frozen or sleeping page would otherwise fire every pending timeout on resume and report a
 * paused decoder or encoder as stalled. Each tick credits at most {@link MAX_CREDIT_MS}, so a long
 * freeze counts as one tick while a throttled one-second timer still counts in full.
 */

/** Most active time one tick credits, in milliseconds. */
export const MAX_CREDIT_MS = 1_100;

/** Stopwatch tick interval, in milliseconds. */
export const ACTIVE_TICK_MS = 250;

/** Clock source for a stopwatch; replaceable in tests. */
export interface ActiveClock {
  now(): number;
  every(ms: number, tick: () => void): () => void;
}

const browserClock: ActiveClock = {
  now: () => performance.now(),
  every: (ms, tick) => {
    const timer = setInterval(tick, ms);
    return () => clearInterval(timer);
  },
};

/** Active time since it started or was last reset. */
export class ActiveStopwatch {
  readonly #clock: ActiveClock;
  #elapsed = 0;
  #last: number;
  #stop: (() => void) | undefined;
  readonly #listeners = new Set<() => void>();

  constructor(clock: ActiveClock = browserClock) {
    this.#clock = clock;
    this.#last = clock.now();
    this.#stop = clock.every(ACTIVE_TICK_MS, () => this.#tick());
  }

  #credit(): number {
    const now = this.#clock.now();
    const credit = Math.min(Math.max(0, now - this.#last), MAX_CREDIT_MS);
    this.#last = now;
    return credit;
  }

  #tick(): void {
    this.#elapsed += this.#credit();
    for (const listener of [...this.#listeners]) listener();
  }

  /** Active milliseconds so far. */
  elapsed(): number {
    this.#elapsed += this.#credit();
    return this.#elapsed;
  }

  reset(): void {
    this.#elapsed = 0;
    this.#last = this.#clock.now();
  }

  /** Calls `listener` after every tick until the returned function is called. */
  onTick(listener: () => void): () => void {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }

  /** Stops the ticks; `elapsed()` still answers. */
  dispose(): void {
    this.#stop?.();
    this.#stop = undefined;
    this.#listeners.clear();
  }
}

/**
 * Resolves once `ms` of active time has passed. Aborting `signal` stops the count and leaves the
 * promise pending forever.
 */
export function activeTimeout(
  ms: number,
  signal?: AbortSignal,
  clock: ActiveClock = browserClock,
): Promise<void> {
  return new Promise<void>((resolve) => {
    if (signal?.aborted === true) return;
    const watch = new ActiveStopwatch(clock);
    const stop = (): void => {
      watch.dispose();
      signal?.removeEventListener("abort", stop);
    };
    watch.onTick(() => {
      if (watch.elapsed() < ms) return;
      stop();
      resolve();
    });
    signal?.addEventListener("abort", stop, { once: true });
  });
}

/** Resolves with `work`, or rejects with `timeout()` if `ms` of active time passes first. */
export async function withinActiveTime<T>(
  work: Promise<T>,
  ms: number,
  timeout: () => unknown,
  clock: ActiveClock = browserClock,
): Promise<T> {
  const done = new AbortController();
  try {
    return await Promise.race([
      work,
      activeTimeout(ms, done.signal, clock).then(() => {
        throw timeout();
      }),
    ]);
  } finally {
    done.abort();
  }
}

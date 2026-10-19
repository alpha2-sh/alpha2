// Web Locks for tests of origin-private storage (proxies, temporary exports).

/**
 * Web Locks, as a page of the origin sees them: shared holders together, an exclusive one alone.
 */
export class FakeLocks {
  #held = new Map();
  #waiting = [];
  async request(name, options, callback) {
    const mode = options.mode ?? "exclusive";
    const free = () => {
      const held = this.#held.get(name);
      return !held || (mode === "shared" && held.mode === "shared");
    };
    options.signal?.throwIfAborted();
    if (!free()) {
      if (options.ifAvailable) return callback(null);
      await new Promise((wake, fail) => {
        const waiter = { free, wake };
        this.#waiting.push(waiter);
        // As browsers do: a request whose signal aborts while it waits is withdrawn, rejecting
        // with an AbortError.
        options.signal?.addEventListener(
          "abort",
          () => {
            this.#waiting = this.#waiting.filter((other) => other !== waiter);
            fail(new DOMException("The request was aborted.", "AbortError"));
          },
          { once: true },
        );
      });
    }
    const held = this.#held.get(name) ?? { mode, count: 0 };
    held.count++;
    this.#held.set(name, held);
    try {
      return await callback({ name, mode });
    } finally {
      if (--held.count === 0) this.#held.delete(name);
      const waiting = this.#waiting;
      this.#waiting = [];
      for (const waiter of waiting)
        if (waiter.free()) waiter.wake();
        else this.#waiting.push(waiter);
    }
  }
}

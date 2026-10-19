// Web Locks marking OPFS entries in use: users hold the lock shared, removers take it exclusively
// only if unheld. Closed pages release their locks; without Web Locks nothing is held.

const locks = () => globalThis.navigator?.locks ?? null;

/**
 * Holds the shared lock `name` until `release()`. Resolves once held; where the browser has no Web
 * Locks, or refuses the lock, at once and holding nothing. `release()` returns a promise that
 * settles when the lock is let go.
 * @param {string} name
 * @returns {Promise<{ release(): Promise<void> }>}
 */
export async function holdShared(name) {
  const manager = locks();
  if (!manager) return { release: async () => {} };
  let letGo;
  const done = new Promise((resolve) => (letGo = resolve));
  let held;
  const acquired = new Promise((resolve) => (held = resolve));
  const request = manager
    .request(name, { mode: "shared" }, () => {
      held();
      return done;
    })
    .catch(() => held());
  await acquired;
  let released = null;
  return {
    release() {
      if (!released) {
        letGo();
        released = request;
      }
      return released;
    },
  };
}

/**
 * Runs `remove` holding the lock `name` exclusively, only if no page holds it, and resolves to
 * whether it ran. Without Web Locks it runs.
 * @param {string} name
 * @param {() => Promise<void>} remove
 * @returns {Promise<boolean>}
 */
export async function whenUnheld(name, remove) {
  const manager = locks();
  if (!manager) {
    await remove();
    return true;
  }
  return manager.request(
    name,
    { mode: "exclusive", ifAvailable: true },
    async (lock) => {
      if (!lock) return false;
      await remove();
      return true;
    },
  );
}

/**
 * Runs `body` holding the lock `name` exclusively, waiting for any page that holds it, and resolves
 * to what `body` resolves to; `body` is told whether it holds the lock. Where the browser has no
 * Web Locks, or refuses the lock, `body` runs at once, holding nothing. Rejects with
 * `signal.reason` once `signal` aborts while it waits.
 * @template T
 * @param {string} name
 * @param {{ signal?: AbortSignal }} options
 * @param {(held: boolean) => Promise<T>} body
 * @returns {Promise<T>}
 */
export async function withExclusive(name, { signal } = {}, body) {
  signal?.throwIfAborted();
  const manager = locks();
  if (!manager) return body(false);
  let started = false;
  try {
    return await manager.request(
      name,
      signal ? { mode: "exclusive", signal } : { mode: "exclusive" },
      () => {
        started = true;
        return body(true);
      },
    );
  } catch (error) {
    if (started) throw error;
    signal?.throwIfAborted();
    return body(false);
  }
}

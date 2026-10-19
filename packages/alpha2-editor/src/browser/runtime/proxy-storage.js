// Where kept proxies live: OPFS {@link PROXY_DIRECTORY}, `<hash>.mp4` plus `<hash>.json` per
// original SHA-256. A proxy is shared by every project of the origin, so it lives while something
// refers to it. Readers hold a shared Web Lock on the hash and removal takes it exclusively; makers
// also take turns on a second lock ({@link whileMakingProxy}) and write `<hash>.<id>.part` until
// finished. No media libraries here, so a storage-only page can load this module alone.

import { holdShared, whenUnheld, withExclusive } from "./locks.js";

/** The origin-private directory proxies are kept in. */
export const PROXY_DIRECTORY = "alpha2-proxies";
const NAME = /^([0-9a-f]{64})\.(mp4|json)$/;
const lockName = (hash) => `alpha2-proxy:${hash}`;
const makeLockName = (hash) => `alpha2-proxy-make:${hash}`;
/** The name an unfinished proxy video of `hash` is written under. */
export const unfinishedProxyName = (hash, id) => `${hash}.${id}.part`;
const UNFINISHED = /^([0-9a-f]{64})\.[0-9A-Za-z-]+\.part$/;

/**
 * The proxy directory, or `null` where the browser has no origin-private file system (or, with
 * `create: false`, where no proxy was ever kept).
 * @returns {Promise<FileSystemDirectoryHandle | null>}
 */
export async function proxyDirectory({ create = true } = {}) {
  try {
    const root = await globalThis.navigator.storage.getDirectory();
    return await root.getDirectoryHandle(PROXY_DIRECTORY, { create });
  } catch {
    return null;
  }
}

/**
 * Holds the proxy of `hash` in use, for this page, until `release()`: a removal from any page of
 * the origin leaves it alone meanwhile. Resolves once held; where the browser has no Web Locks, at
 * once and holding nothing.
 *
 * `release()` returns a promise that settles when the lock is let go.
 * @param {string} hash
 * @returns {Promise<{ release(): Promise<void> }>}
 */
export async function holdProxy(hash) {
  return holdShared(lockName(hash));
}

/**
 * Runs `make` as the only page of the origin making the proxy of `hash`: it waits for a page that
 * is making it to finish, so it can find that page's proxy instead of making it again. `make` is
 * told whether it holds the turn; where the browser has no Web Locks it runs at once, holding
 * nothing. Rejects with `signal.reason` once `signal` aborts while it waits.
 * @template T
 * @param {string} hash
 * @param {{ signal?: AbortSignal }} options
 * @param {(held: boolean) => Promise<T>} make
 * @returns {Promise<T>}
 */
export function whileMakingProxy(hash, options, make) {
  return withExclusive(makeLockName(hash), options, make);
}

/**
 * Removes what a page that stopped while making the proxy of `hash` left: the unfinished videos.
 * Only for the page holding the turn to make it ({@link whileMakingProxy}), when no other page can
 * be writing one.
 * @param {FileSystemDirectoryHandle} directory
 * @param {string} hash
 */
export async function removeUnfinishedProxies(directory, hash) {
  const names = [];
  for await (const [name] of directory.entries())
    if (UNFINISHED.exec(name)?.[1] === hash) names.push(name);
  for (const name of names) await directory.removeEntry(name).catch(() => {});
}

/**
 * The proxies kept in this origin's storage: each original's hash and the bytes its proxy takes.
 * @returns {Promise<import("../types.js").KeptProxy[]>}
 */
export async function keptProxies() {
  const directory = await proxyDirectory({ create: false });
  if (!directory) return [];
  /** @type {Map<string, number>} */
  const bytes = new Map();
  for await (const [name, handle] of directory.entries()) {
    const match = NAME.exec(name);
    if (!match || handle.kind !== "file") continue;
    const size = await handle
      .getFile()
      .then((file) => file.size)
      .catch(() => 0);
    bytes.set(match[1], (bytes.get(match[1]) ?? 0) + size);
  }
  return [...bytes].map(([hash, size]) => ({ hash, bytes: size }));
}

/**
 * Removes the kept proxies of `hashes`, except those a page of this origin holds in use ({@link
 * holdProxy}). Resolves to the hashes removed and those left because they are in use; a hash with
 * nothing kept counts as removed. A string that is not a lowercase hex SHA-256 is ignored and
 * listed in neither.
 * @param {Iterable<string>} hashes
 * @returns {Promise<import("../types.js").ProxyRemoval>}
 */
export async function removeKeptProxies(hashes) {
  const removed = [];
  const inUse = [];
  const directory = await proxyDirectory({ create: false });
  for (const hash of new Set(hashes)) {
    if (!/^[0-9a-f]{64}$/.test(hash)) continue;
    const erase = async () => {
      // The record first: without it the proxy is not found, whatever happens to the video.
      for (const name of [`${hash}.json`, `${hash}.mp4`])
        await directory?.removeEntry(name).catch((error) => {
          if (error?.name !== "NotFoundError") throw error;
        });
    };
    const done = await whenUnheld(lockName(hash), erase);
    (done ? removed : inUse).push(hash);
  }
  return { removed, inUse };
}

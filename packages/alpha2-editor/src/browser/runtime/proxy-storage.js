// Kept proxies live in OPFS {@link PROXY_DIRECTORY} as `<hash>.mp4` plus `<hash>.json` per original
// SHA-256, shared by every project of the origin. Readers hold a shared Web Lock on the hash and
// removal takes it exclusively; makers also take turns on a second lock ({@link whileMakingProxy})
// and write `<hash>.<id>.part` until finished. No media libraries here, so storage-only pages can
// load this module alone.

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
 * The proxy directory, or `null` without an origin-private file system (or, with `create: false`,
 * when no proxy was ever kept).
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
 * Holds the proxy of `hash` in use for this page until `release()`, so removals from any page of
 * the origin skip it. Resolves once held; without Web Locks, at once and holding nothing.
 * `release()` settles when the lock is let go.
 * @param {string} hash
 * @returns {Promise<{ release(): Promise<void> }>}
 */
export async function holdProxy(hash) {
  return holdShared(lockName(hash));
}

/**
 * Runs `make` as the only page of the origin making the proxy of `hash`, waiting for a page already
 * making it so the caller can find that proxy instead. `make` is told whether it holds the turn;
 * without Web Locks it runs at once, holding nothing. Rejects with `signal.reason` if `signal`
 * aborts while waiting.
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
 * Removes unfinished videos a stopped page left while making the proxy of `hash`. Only for the page
 * holding the turn ({@link whileMakingProxy}), when no other page can be writing one.
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
 * Each proxy kept in this origin's storage: the original's hash and the proxy's size in bytes.
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
 * Removes the kept proxies of `hashes` except those a page holds in use ({@link holdProxy}).
 * Resolves to the removed hashes and those left in use; a hash with nothing kept counts as removed.
 * Strings that are not lowercase hex SHA-256 are ignored and listed in neither.
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

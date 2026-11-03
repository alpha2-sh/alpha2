// Proxies: lighter copies of a source's video, kept in OPFS under the original's content hash so a
// reopened project previews before its files are relinked. The engine decides whether a proxy may
// stand in for its source. Proxies stream to their file while encoding; only a browser without
// writable OPFS keeps them in memory, for this page and within a size limit.
import {
  ALL_FORMATS,
  BlobSource,
  BufferTarget,
  Conversion,
  Input,
  Mp4OutputFormat,
  Output,
  StreamTarget,
  canEncodeAudio,
} from "mediabunny";
import {
  MEMORY_OUTPUT_COPIES,
  WritableFileDestination,
} from "alpha2/export-destination";
import { EditorError } from "../../core/document.js";
import { displaySize, probeEvidence } from "./fingerprint.js";
import { DEFAULT_MEMORY_OUTPUT_LIMIT } from "./output.js";
import {
  holdProxy,
  keptProxies,
  proxyDirectory,
  removeKeptProxies,
  removeUnfinishedProxies,
  unfinishedProxyName,
  whileMakingProxy,
} from "./proxy-storage.js";

/** A proxy's longer displayed edge, in pixels. */
export const PROXY_LONG_EDGE = 960;

/**
 * How long a proxy conversion may go without progress before it fails as `proxy_stalled`. Some
 * browser encoders stop emitting chunks without an error.
 */
export const PROXY_STALL_MS = 30_000;

/**
 * @typedef {{
 *   made_from: import("alpha2/project").MediaFingerprint,
 *   width: number,
 *   height: number,
 *   frame_rate?: { num: number, den: number },
 *   duration_ticks?: number,
 *   audio_sample_rate?: number,
 * }} ProxyRecord
 */

/**
 * Keeps proxies by their original's SHA-256, in OPFS where writable, else in this page's memory
 * within {@link DEFAULT_MEMORY_OUTPUT_LIMIT} by default.
 */
export class ProxyStore {
  /** @type {Map<string, { record: ProxyRecord, file: File }>} */
  #memory = new Map();
  #memoryLimit;
  #stallMs;

  constructor({
    memoryLimit = DEFAULT_MEMORY_OUTPUT_LIMIT,
    stallMs = PROXY_STALL_MS,
  } = {}) {
    this.#memoryLimit = memoryLimit;
    this.#stallMs = stallMs;
  }

  /** Holds the proxy of `hash` in use until `release()` so no page removes it (see `holdProxy`). */
  hold(hash) {
    return holdProxy(hash);
  }

  /** The proxy made from the media with `hash`, if one is kept. */
  async find(hash) {
    const kept = this.#memory.get(hash);
    if (kept) return kept;
    const directory = await proxyDirectory();
    if (!directory) return null;
    try {
      const record = JSON.parse(
        await (
          await (await directory.getFileHandle(`${hash}.json`)).getFile()
        ).text(),
      );
      const file = await (
        await directory.getFileHandle(`${hash}.mp4`)
      ).getFile();
      // Read each time, since another page may have removed it.
      return { record, file };
    } catch {
      return null;
    }
  }

  /** Each kept proxy's original hash and size in bytes, on disk or in this page's memory. */
  async list() {
    const kept = new Map(
      (await keptProxies()).map(({ hash, bytes }) => [hash, bytes]),
    );
    for (const [hash, { file }] of this.#memory)
      kept.set(hash, (kept.get(hash) ?? 0) + file.size);
    return [...kept].map(([hash, bytes]) => ({ hash, bytes }));
  }

  /**
   * Removes the proxies of `hashes` except those held in use; resolves to removed and in-use
   * hashes.
   */
  async remove(hashes) {
    const wanted = [...new Set(hashes)];
    for (const hash of wanted) this.#memory.delete(hash);
    return removeKeptProxies(wanted);
  }

  /**
   * Makes and keeps a proxy of `file` (with `fingerprint`): video scaled to fit
   * {@link PROXY_LONG_EDGE} with a key frame every second, original timestamps and audio sample
   * rate. Returns an existing proxy for the same bytes instead. Pages of the origin take turns per
   * hash (`whileMakingProxy`).
   *
   * Refuses before encoding when the conversion would drop a track the fingerprint records. Rejects
   * with `signal.reason` if `signal` aborts before the proxy is kept, and with `proxy_stalled` when
   * the conversion makes no progress for {@link PROXY_STALL_MS}; neither keeps a file.
   */
  async generate(file, fingerprint, { signal, onProgress } = {}) {
    signal?.throwIfAborted();
    const hash = fingerprint.content_sha256;
    if (!hash)
      throw new EditorError(
        "unsupported_media",
        "A proxy needs the original's hash",
        "proxy",
      );
    const kept = await this.find(hash);
    if (kept) return kept;
    return whileMakingProxy(hash, { signal }, async (held) => {
      const made = await this.find(hash);
      if (made) return made;
      const directory = held ? await proxyDirectory() : null;
      if (directory) await removeUnfinishedProxies(directory, hash);
      // Held while made so no page removes it before it is returned.
      const hold = await holdProxy(hash);
      try {
        return await this.#make(file, fingerprint, hash, {
          signal,
          onProgress,
        });
      } finally {
        void hold.release();
      }
    });
  }

  /** Makes the proxy of `file` while holding the turn (see {@link generate}). */
  async #make(file, fingerprint, hash, { signal, onProgress }) {
    signal?.throwIfAborted();
    const input = new Input({
      source: new BlobSource(file),
      formats: ALL_FORMATS,
    });
    // Installed before setup awaits, so an earlier abort cancels the conversion once it exists.
    let cancel = () => {};
    const onAbort = () => cancel();
    signal?.addEventListener("abort", onAbort, { once: true });
    try {
      const video = await input.getPrimaryVideoTrack();
      if (!video)
        throw new EditorError(
          "unsupported_media",
          `${file.name} has no video to proxy`,
          "proxy",
        );
      const space = await video.getColorSpace().catch(() => null);
      // The browser's scaler tone-maps HDR its own way, so the proxy would not match the original.
      if (space?.transfer === "pq" || space?.transfer === "hlg")
        throw new EditorError(
          "unsupported_media",
          `${file.name} is HDR; the browser cannot make a proxy that keeps its color`,
          "proxy",
        );
      // Proxies have square pixels, so use the original's displayed size.
      const shown = await displaySize(video);
      const scale = Math.min(
        1,
        PROXY_LONG_EDGE / Math.max(shown.width, shown.height),
      );
      const even = (value) => Math.max(2, 2 * Math.round((value * scale) / 2));
      const audio = await input.getPrimaryAudioTrack();
      // AAC keeps any sample rate, else PCM; Opus would resample and the engine would refuse the
      // proxy.
      const audioCodec =
        audio &&
        (await canEncodeAudio("aac", {
          numberOfChannels: audio.numberOfChannels,
          sampleRate: audio.sampleRate,
        }).catch(() => false))
          ? "aac"
          : "pcm-s16";
      signal?.throwIfAborted();
      const sink = await this.#open(hash);
      try {
        const output = new Output({
          // No fast start: samples stream out as muxed and nothing rewrites the payload.
          format: new Mp4OutputFormat({ fastStart: false }),
          target: sink.target,
        });
        const conversion = await Conversion.init({
          input,
          output,
          tracks: "primary",
          // Without it a conversion starts at the first frame, shifting an original with a late
          // one.
          trim: { start: 0 },
          video: {
            width: even(shown.width),
            height: even(shown.height),
            fit: "fill",
            keyFrameInterval: 1,
          },
          audio: { codec: audioCodec },
          showWarnings: false,
        });
        if (!conversion.isValid)
          throw new EditorError(
            "unsupported_media",
            `Cannot make a proxy of ${file.name}: ${conversion.discardedTracks
              .map((track) => track.reason)
              .join(", ")}`,
            "proxy",
          );
        // The engine reads a proxy only with every stream its original's fingerprint records.
        const left = conversion.discardedTracks.find(
          ({ track }) =>
            track === video || (track === audio && fingerprint.streams?.audio),
        );
        if (left)
          throw new EditorError(
            "unsupported_media",
            `Cannot make a proxy of ${file.name}: this browser cannot convert its ${left.track === video ? "video" : "audio"} (${left.reason})`,
            "proxy",
          );
        // Discard before the output closes it so a cancelled proxy is never committed.
        cancel = () => {
          void sink.discard();
          void conversion.cancel();
        };
        sink.onOverflow = cancel;
        signal?.throwIfAborted();
        // Raced against the conversion, since a stalled encoder may not settle even when cancelled.
        let stalled = null;
        let stallTimer = 0;
        let failStalled = () => {};
        const stall = new Promise((_, reject) => {
          failStalled = reject;
        });
        const armStall = () => {
          clearTimeout(stallTimer);
          stallTimer = setTimeout(() => {
            stalled = new EditorError(
              "proxy_stalled",
              `The proxy of ${file.name} made no progress for ${Math.round(this.#stallMs / 1000)} s; the browser's video decoder or encoder stopped`,
              "proxy",
            );
            failStalled(stalled);
            cancel();
          }, this.#stallMs);
        };
        conversion.onProgress = (fraction) => {
          armStall();
          onProgress?.(fraction);
        };
        armStall();
        const executed = conversion.execute();
        executed.catch(() => {});
        try {
          await Promise.race([executed, stall]);
        } catch (error) {
          signal?.throwIfAborted();
          throw stalled ?? sink.failure ?? error;
        } finally {
          clearTimeout(stallTimer);
        }
        signal?.throwIfAborted();
        if (stalled) throw stalled;
        if (sink.failure) throw sink.failure;
        // From here a failure discards the file; an abort after keeping leaves it kept.
        cancel = () => {};
        const proxy = await sink.file();
        const evidence = await probeEvidence(proxy);
        const shape = evidence.streams?.video;
        /** @type {ProxyRecord} */
        const record = {
          made_from: fingerprint,
          width: shape?.width ?? 0,
          height: shape?.height ?? 0,
          ...(shape?.frame_rate ? { frame_rate: shape.frame_rate } : {}),
          ...(evidence.duration_ticks === undefined
            ? {}
            : { duration_ticks: evidence.duration_ticks }),
          ...(evidence.streams?.audio
            ? { audio_sample_rate: evidence.streams.audio.sample_rate }
            : {}),
        };
        if (
          !record.width ||
          !record.height ||
          (fingerprint.streams?.audio && record.audio_sample_rate === undefined)
        )
          throw new EditorError(
            "unsupported_media",
            `Cannot make a proxy of ${file.name}: the converted file lacks its ${record.width && record.height ? "audio" : "video"}`,
            "proxy",
          );
        return { record, file: await sink.keep(record) };
      } catch (error) {
        await sink.discard().catch(() => {});
        throw error;
      }
    } finally {
      signal?.removeEventListener("abort", onAbort);
      input.dispose();
    }
  }

  /**
   * Where the proxy of `hash` is written: `target` takes the muxer's writes, `keep(record)` keeps
   * it, `discard()` leaves nothing. The file is written under its own name and moved to
   * `<hash>.mp4` by `keep`, so `discard` removes only what this sink wrote.
   */
  async #open(hash) {
    const directory = await proxyDirectory();
    const name = unfinishedProxyName(hash, uniqueId());
    const kept = `${hash}.mp4`;
    /** @type {FileSystemFileHandle | null} */
    let handle = null;
    let writable = null;
    if (directory)
      try {
        handle = await directory.getFileHandle(name, { create: true });
        if (typeof handle.createWritable === "function")
          writable = await handle.createWritable();
      } catch {
        writable = null;
      }
    if (writable && handle && directory) {
      const destination = new WritableFileDestination(writable);
      /** @type {Promise<void> | null} */
      let discarding = null;
      let moved = false;
      let recorded = false;
      return {
        target: new StreamTarget(destination.writable, { chunked: false }),
        get failure() {
          const failure = destination.failure;
          return failure
            ? new EditorError(
                failure.code === "STORAGE_QUOTA_EXCEEDED"
                  ? "storage_quota_exceeded"
                  : "proxy_write_failed",
                `The proxy could not be kept: ${failure.message}`,
                "proxy",
              )
            : null;
        },
        onOverflow: () => {},
        file: () => handle.getFile(),
        keep: async (record) => {
          // A video without a record is a stopped page's leftover, and this page holds the turn.
          await directory.removeEntry(kept).catch(() => {});
          moved = true;
          await moveFile(directory, handle, name, kept);
          const json = await (
            await directory.getFileHandle(`${hash}.json`, { create: true })
          ).createWritable();
          recorded = true;
          await json.write(JSON.stringify(record));
          await json.close();
          return (await directory.getFileHandle(kept)).getFile();
        },
        discard: () =>
          (discarding ??= (async () => {
            await destination.abort();
            // An unrecorded file is this sink's own; no other page's proxy is in place while this
            // page holds the turn.
            if (recorded)
              await directory.removeEntry(`${hash}.json`).catch(() => {});
            if (moved) await directory.removeEntry(kept).catch(() => {});
            await directory.removeEntry(name).catch(() => {});
          })()),
      };
    }
    // An empty file a browser without `createWritable` could not fill.
    if (handle) await directory?.removeEntry(name).catch(() => {});
    const maxBytes = Math.floor(this.#memoryLimit / MEMORY_OUTPUT_COPIES);
    const target = new BufferTarget();
    /** @type {File | null} */
    let finished = null;
    const sink = {
      target,
      failure: null,
      onOverflow: () => {},
      file: async () =>
        (finished ??= new File([target.buffer], kept, { type: "video/mp4" })),
      keep: async (record) => {
        const file = await sink.file();
        this.#memory.set(hash, { record, file });
        return file;
      },
      discard: async () => {},
    };
    target.onwrite = (_start, end) => {
      if (end <= maxBytes || sink.failure) return;
      sink.failure = new EditorError(
        "resource_limit",
        `This browser cannot keep proxies on disk, and a proxy kept in memory may take at most ${Math.round(maxBytes / 1e6)} MB. Preview from the original instead.`,
        "proxy",
      );
      sink.onOverflow();
    };
    return sink;
  }
}

function uniqueId() {
  return (
    globalThis.crypto?.randomUUID?.() ??
    `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`
  );
}

/**
 * Moves finished file `handle` (named `from`) to `to`, with `move` where available, else by copy
 * and remove.
 * @param {FileSystemDirectoryHandle} directory
 * @param {FileSystemFileHandle} handle
 * @param {string} from
 * @param {string} to
 */
async function moveFile(directory, handle, from, to) {
  const move =
    /** @type {{ move?: (into: FileSystemDirectoryHandle, name: string) => Promise<void> }} */ (
      handle
    ).move;
  if (typeof move === "function")
    try {
      await move.call(handle, directory, to);
      return;
    } catch {
      // This browser's move refuses here, so copy instead.
    }
  const writable = await (
    await directory.getFileHandle(to, { create: true })
  ).createWritable();
  try {
    const reader = (await handle.getFile()).stream().getReader();
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      await writable.write(value);
    }
    await writable.close();
  } catch (error) {
    await writable.abort().catch(() => {});
    throw error;
  }
  await directory.removeEntry(from).catch(() => {});
}

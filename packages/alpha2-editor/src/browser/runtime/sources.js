// Answers per-frame source requests with decoded VideoFrames; Canvas2D premultiplied RGBA remains
// for text rasters and for an engine that cannot take a VideoFrame.
//
// Still and text rasters share one LRU cache bounded by bytes, but what the frame being drawn reads
// is never evicted, even past the budget. Stills are decoded per read and closed once read.
//
// Decoder cursors are scoped by clip consumer so overlapping uses of one source cannot seek each
// other. WebCodecs is the primary path; an `HTMLVideoElement` seek is the fallback for codecs it
// cannot open or frames that keep failing (`decodeFallbacks`).

import { decoderCursorKey } from "alpha2/frame-supply";
import { MediabunnyVideoDecoder } from "alpha2/mediabunny";
import { containRect, seekVideoFrame } from "./preview-utils.js";
import { decodeStill, stillRaster } from "./still.js";

const TICKS_PER_SECOND = 120_000;
/** Failed frames in a row, each retried with a reopened decoder, before element seeks take over. */
const MAX_DECODE_FAILURES = 3;
/** Default byte budget of still and text rasters, matching the frame supply's image cache. */
export const DEFAULT_RASTER_BUDGET_BYTES = 256 * 1024 * 1024;
/**
 * The default byte budget of what open extension sources hold (their decoded frames in the provider
 * module and the frame each showed last): the native host's `providerSourceBytes` default.
 */
export const DEFAULT_PROVIDER_SOURCE_BYTES = 512 * 1024 * 1024;

/** The host side of the engine's per-frame source requests for one renderer (preview or export). */
export class SourcePixels {
  #width = 1920;
  #height = 1080;
  #decoders = new Map();
  #seekVideos = new Map();
  /**
   * Rasters by asset ID in LRU order: `{ key, raster, bytes, frame }`, where `key` is what the
   * raster was made from and `frame` the frame that last read it.
   */
  #rasters = new Map();
  #rasterBytes = 0;
  /** Counts the frames drawn: the engine releases once before each frame reads its sources. */
  #frame = 0;
  #staging = null;

  /** What fell back from WebCodecs decoding, by source (see `decodeFallbacks`). */
  #fallbacks = new Map();

  /** Extension sources opened, by `providerKey`, in least-recently-shown order. */
  #providers = new Map();
  /** Sources the provider budget closed and a later frame opened again. */
  #providerReopens = 0;
  /** Asset IDs by `providerKey` of sources the provider budget closed, until they reopen. */
  #providersClosed = new Map();
  /**
   * Per animated-image asset, `{ asset, provider, opened }`: a reader opened once whose file and
   * kept frames every reader made from it shares (see `#openReader`).
   */
  #readerMakers = new Map();
  /** The fonts an SVG's text draws with: the engine's caption fonts (see `setFonts`). */
  #fonts = [];
  /** What each SVG's pixels are with the fonts it was drawn with, for `contentOf`. */
  #fontContents = new WeakMap();

  constructor(
    library,
    {
      decoderLimit = 32,
      rasterBudgetBytes = DEFAULT_RASTER_BUDGET_BYTES,
      providerSourceBytes = DEFAULT_PROVIDER_SOURCE_BYTES,
      extensionsWasmUrl,
      videoDecode = "auto",
    } = {},
  ) {
    if (videoDecode !== "auto" && videoDecode !== "software")
      throw new RangeError('videoDecode must be "auto" or "software"');
    if (!Number.isSafeInteger(rasterBudgetBytes) || rasterBudgetBytes <= 0)
      throw new RangeError("rasterBudgetBytes must be a positive integer");
    if (!Number.isSafeInteger(providerSourceBytes) || providerSourceBytes <= 0)
      throw new RangeError("providerSourceBytes must be a positive integer");
    this.decoderLimit = decoderLimit;
    this.rasterBudgetBytes = rasterBudgetBytes;
    this.providerSourceBytes = providerSourceBytes;
    this.library = library;
    this.extensionsWasmUrl = extensionsWasmUrl;
    /**
     * How video decodes: `"auto"` lets the browser pick, a hardware decoder where it has one;
     * `"software"` decodes on the CPU, so no frame needs a GPU readback, except a source the
     * browser has no software decoder for, which decodes as `"auto"` and is reported in
     * `decodeFallbacks`.
     */
    this.videoDecode = videoDecode;
  }

  get diagnostics() {
    const decoders = [...this.#decoders.values()]
      .map((decoder) => decoder.video)
      .filter(Boolean);
    return {
      liveDecoders: this.#decoders.size,
      retainedFrameBytes: decoders.reduce(
        (bytes, video) => bytes + video.cachedFrameBytes,
        0,
      ),
      windowedFrames: decoders.reduce(
        (hits, video) => hits + video.reverseCacheHits,
        0,
      ),
      reverseWindows: decoders.reduce(
        (windows, video) => windows + video.reverseWindowsBuilt,
        0,
      ),
      cachedRasters: this.#rasters.size,
      cachedRasterBytes: this.#rasterBytes,
      fallbackVideos: this.#seekVideos.size,
      decodeFallbackFrames: [...this.#fallbacks.values()].reduce(
        (frames, entry) => frames + entry.frames,
        0,
      ),
      openProviderSources: this.#providers.size,
      providerSourceBytes: this.#providerBytes(),
      providerReopens: this.#providerReopens,
    };
  }

  setOutputSize(width, height) {
    if (width === this.#width && height === this.#height) return;
    this.#width = width;
    this.#height = height;
    this.#staging = null;
    for (const [id, entry] of this.#rasters)
      if (entry.key.width !== undefined) this.#dropRaster(id, entry);
  }

  /**
   * The fonts an SVG's text draws with, in fallback order: the stack the engine draws captions with
   * (`{ bytes }` each), as the native host draws an SVG's text with its caption fonts. An SVG
   * opened with other fonts opens again on its next frame; other sources are not touched.
   */
  setFonts(fonts) {
    if (sameFonts(this.#fonts, fonts)) return;
    this.#fonts = [...fonts];
  }

  /**
   * What a source's pixels are, for `FrameSupply`'s `contentOf`: its runtime asset, and for an SVG
   * also the fonts its text draws with, so a change of fonts draws it again. The same object while
   * neither changes.
   */
  contentOf(asset) {
    if (!asset || !drawsText(asset.provider)) return asset;
    const known = this.#fontContents.get(asset);
    if (known?.fonts === this.#fonts) return known;
    const content = { asset, fonts: this.#fonts };
    this.#fontContents.set(asset, content);
    return content;
  }

  /** Drops the cached raster of `assetId`, so its next read makes it again. */
  invalidate(assetId) {
    const entry = this.#rasters.get(assetId);
    if (entry) this.#dropRaster(assetId, entry);
  }

  /**
   * Bound method handed to `FrameSupply` as its `pixelsFor` callback. `sourceId` is the source the
   * engine asked for, which the layer may stand in for (a proxy has an ID of its own): decoders are
   * kept per source, as `release` names them. `decode: "software"` asks for the frame decoded on
   * the CPU (see `#softwareFrame`).
   */
  pixelsFor = async (
    layer,
    seconds,
    { preferVideoFrame = false, consumerId = layer.id, sourceId, decode } = {},
  ) => {
    const asset = layer.asset ?? layer;
    if (decode === "software")
      return this.#softwareFrame(
        asset,
        seconds,
        consumerId,
        sourceId ?? asset.id,
      );
    if (asset.provider)
      return this.#providerPixels(
        asset,
        providerKey(asset, consumerId),
        seconds,
      );
    if (asset.kind === "text")
      return this.#cachedRaster(
        asset.id,
        { raster: asset.raster, width: this.#width, height: this.#height },
        () => this.#textPixels(asset),
      );
    if (asset.kind === "image")
      return this.#cachedRaster(asset.id, { file: asset.file }, () =>
        this.#imagePixels(asset),
      );
    return this.#videoPixels(
      asset,
      seconds,
      preferVideoFrame,
      consumerId,
      sourceId ?? asset.id,
    );
  };

  /** A frame an extension provider draws at its own size; `key` is its open source. */
  async #providerPixels(asset, key, seconds, reopened = false) {
    let opened = this.#providers.get(key);
    // Reopened when the asset, its provider settings, or an SVG's fonts change; providers are
    // compared by identity first, so a shown frame does not serialize them.
    const { provider } = asset;
    const fonts = drawsText(provider) ? this.#fonts : null;
    if (
      opened?.asset !== asset ||
      !sameProviderSettings(opened.provider, provider) ||
      opened.fonts !== fonts
    ) {
      opened?.source.then(({ source }) => source.release()).catch(() => {});
      const opening = {
        asset,
        assetId: asset.id,
        provider,
        fonts,
        source:
          provider.format === "animated_image"
            ? this.#openReader(asset)
            : openProvider(asset, this.extensionsWasmUrl, fonts),
      };
      // Keep what a provider reports, but not a load or read failure: retry next frame.
      opening.source.catch((error) => {
        if (retryableOpen.has(error) && this.#providers.get(key) === opening)
          this.#providers.delete(key);
      });
      opened = opening;
      this.#providers.set(key, opened);
      if (this.#providersClosed.delete(key)) this.#providerReopens += 1;
    }
    // Kept by the budget for this frame and the next.
    opened.frame = this.#frame;
    this.#providers.delete(key);
    this.#providers.set(key, opened);
    const { source, extensions, read } = await opened.source;
    opened.open = source;
    const ticks = BigInt(Math.round(seconds * TICKS_PER_SECOND));
    const fit = { width: this.#width, height: this.#height };
    // The frame shown last answers every time it covers, as the same object, so a still is not
    // uploaded again each frame.
    const shown = opened.shown;
    if (
      shown &&
      shown.width === fit.width &&
      shown.height === fit.height &&
      ticks >= shown.start &&
      (shown.end === undefined || ticks < shown.end)
    )
      return shown.result;
    let frame;
    try {
      // An image sequence's frame enters the provider module for this render only.
      frame = await extensions.withResources(
        read,
        () => source.needs(ticks),
        () => source.render(ticks, fit),
      );
    } catch (error) {
      // The provider module restarted after another source stopped it: this one opens again.
      if (error?.name !== "StaleExtensionSourceError" || reopened) throw error;
      if (this.#providers.get(key) === opened) this.#providers.delete(key);
      return this.#providerPixels(asset, key, seconds, true);
    }
    // Microseconds identify the physical frame.
    const micros = (value) =>
      Math.round((value * 1_000_000) / TICKS_PER_SECOND);
    const result = {
      kind: "raster-frame",
      precision: frame.precision,
      pixels: frame.pixels,
      width: frame.width,
      height: frame.height,
      physicalTimeMicroseconds: micros(frame.startTicks),
      ...(frame.endTicks === undefined
        ? {}
        : { durationMicroseconds: micros(frame.endTicks - frame.startTicks) }),
    };
    // The first frame also answers every earlier time.
    opened.shown = {
      ...fit,
      start: frame.index === 0 ? -(2n ** 63n) : BigInt(frame.startTicks),
      end: frame.endTicks === undefined ? undefined : BigInt(frame.endTicks),
      result,
    };
    this.#boundProviders();
    return result;
  }

  /**
   * A reader of an animated image, of its own (see `providerKey`), made from what the asset's
   * readers share so the file is decoded once; as the native host does.
   */
  async #openReader(asset) {
    const { provider } = asset;
    const made = this.#readerMakers.get(asset.id);
    if (
      made?.asset === asset &&
      sameProviderSettings(made.provider, provider)
    ) {
      try {
        const { maker, extensions, read } = await made.opened;
        const source = maker?.reader();
        if (source) return { source, extensions, read };
      } catch {
        // What made the readers did not open, or a module restart left it behind: open again.
      }
      if (this.#readerMakers.get(asset.id) === made)
        this.#dropReaderMaker(asset.id);
    } else if (made) this.#dropReaderMaker(asset.id);
    const opening = openProvider(asset, this.extensionsWasmUrl, null);
    const entry = {
      asset,
      provider,
      opened: opening.then(({ source, extensions, read }) => ({
        maker: source.reader(),
        extensions,
        read,
      })),
    };
    entry.opened.catch(() => {
      if (this.#readerMakers.get(asset.id) === entry)
        this.#readerMakers.delete(asset.id);
    });
    this.#readerMakers.set(asset.id, entry);
    return opening;
  }

  /** Lets go of what makes an animated image's readers; the readers it made stay open. */
  #dropReaderMaker(assetId) {
    const made = this.#readerMakers.get(assetId);
    if (!made) return;
    this.#readerMakers.delete(assetId);
    made.opened.then(({ maker }) => maker?.release()).catch(() => {});
  }

  /** Lets go of the reader makers of animated images no open source reads any more. */
  #dropUnreadMakers() {
    if (this.#readerMakers.size === 0) return;
    const read = new Set();
    for (const opened of this.#providers.values()) read.add(opened.assetId);
    for (const assetId of [...this.#readerMakers.keys()])
      if (!read.has(assetId)) this.#dropReaderMaker(assetId);
  }

  /** What open extension sources hold; an animated image's shared file counts once per asset. */
  #providerBytes() {
    let bytes = 0;
    const shared = new Map();
    for (const opened of this.#providers.values()) {
      const resident = opened.open?.residentBytes ?? 0;
      const common = Math.min(opened.open?.sharedBytes ?? 0, resident);
      if (common > 0)
        shared.set(
          opened.assetId,
          Math.max(shared.get(opened.assetId) ?? 0, common),
        );
      bytes +=
        resident - common + (opened.shown?.result.pixels.byteLength ?? 0);
    }
    for (const common of shared.values()) bytes += common;
    return bytes;
  }

  /**
   * Keeps open extension sources within `providerSourceBytes` by closing the least recently shown
   * ones; sources this frame or the previous one showed stay open even past the budget.
   */
  #boundProviders() {
    let bytes = this.#providerBytes();
    let closed = false;
    for (const [key, opened] of this.#providers) {
      if (bytes <= this.providerSourceBytes) break;
      if (opened.frame >= this.#frame - 1) continue;
      this.#providers.delete(key);
      this.#providersClosed.set(key, opened.assetId);
      opened.source.then(({ source }) => source.release()).catch(() => {});
      closed = true;
      bytes = this.#providerBytes();
    }
    if (closed) this.#dropUnreadMakers();
  }

  #context() {
    if (!this.#staging)
      this.#staging = new OffscreenCanvas(this.#width, this.#height);
    if (
      this.#staging.width !== this.#width ||
      this.#staging.height !== this.#height
    ) {
      this.#staging = new OffscreenCanvas(this.#width, this.#height);
    }
    return this.#staging.getContext("2d", { willReadFrequently: true });
  }

  /**
   * The raster `make` makes for `id` from `key`, cached while `key` is unchanged. A raster read by
   * the frame being drawn is not evicted until a later frame; failures are not kept.
   */
  #cachedRaster(id, key, make) {
    let entry = this.#rasters.get(id);
    if (entry && !sameKey(entry.key, key)) {
      this.#dropRaster(id, entry);
      entry = undefined;
    }
    if (entry) {
      this.#rasters.delete(id);
      this.#rasters.set(id, entry);
      entry.frame = this.#frame;
      return entry.raster;
    }
    entry = { key, raster: null, bytes: 0, frame: this.#frame };
    entry.raster = new Promise((resolve) => resolve(make())).then(
      (raster) => {
        if (this.#rasters.get(id) === entry) {
          entry.bytes = rasterBytes(raster);
          this.#rasterBytes += entry.bytes;
          this.#evictRasters();
        }
        return raster;
      },
      (error) => {
        if (this.#rasters.get(id) === entry) this.#rasters.delete(id);
        throw error;
      },
    );
    this.#rasters.set(id, entry);
    return entry.raster;
  }

  #dropRaster(id, entry) {
    this.#rasterBytes -= entry.bytes;
    this.#rasters.delete(id);
  }

  /**
   * Evicts the least recently used rasters the frame being drawn has not read, until the cache is
   * within its budget or only that frame's rasters are left.
   */
  #evictRasters() {
    for (const [id, entry] of this.#rasters) {
      if (this.#rasterBytes <= this.rasterBudgetBytes) return;
      if (entry.frame === this.#frame || entry.bytes === 0) continue;
      this.#dropRaster(id, entry);
    }
  }

  /** A still at its own size in stored code values (see `still.js`), decoded from its file. */
  async #imagePixels(asset) {
    if (!asset.file) throw new Error(`${asset.id} has no file to decode`);
    const bitmap = await decodeStill(asset.file);
    try {
      return stillRaster(bitmap);
    } finally {
      bitmap.close();
    }
  }

  /** Text rasters arrive premultiplied, so they are blitted at native size, not resampled. */
  #textPixels(asset) {
    const pixels = new Uint8ClampedArray(this.#width * this.#height * 4);
    const raster = asset.raster;
    if (raster) {
      const sourceWidth = raster.texture.width;
      const sourceHeight = raster.texture.height;
      const offsetX = Math.round((this.#width - sourceWidth) / 2);
      const offsetY = Math.round((this.#height - sourceHeight) / 2);
      for (let row = 0; row < sourceHeight; row += 1) {
        const y = offsetY + row;
        if (y < 0 || y >= this.#height) continue;
        for (let column = 0; column < sourceWidth; column += 1) {
          const x = offsetX + column;
          if (x < 0 || x >= this.#width) continue;
          const from = (row * sourceWidth + column) * 4;
          const to = (y * this.#width + x) * 4;
          pixels[to] = raster.rgba8[from];
          pixels[to + 1] = raster.rgba8[from + 1];
          pixels[to + 2] = raster.rgba8[from + 2];
          pixels[to + 3] = raster.rgba8[from + 3];
        }
      }
    }
    return pixels;
  }

  async #videoPixels(asset, seconds, preferVideoFrame, consumerId, sourceId) {
    const decoded = await this.#decodeFrame(
      asset,
      seconds,
      consumerId,
      sourceId,
    );
    const fallback = decoded
      ? null
      : await this.#seekFallback(asset, seconds, consumerId, sourceId);
    const imported = decoded ?? {
      frame: new VideoFrame(fallback),
      rotation: 0,
    };
    const physicalTimeMicroseconds = decoded
      ? imported.frame.timestamp
      : Math.round(fallback.currentTime * 1_000_000);
    const durationMicroseconds = imported.frame.duration ?? undefined;
    const sourceWidth = imported.frame.displayWidth;
    const sourceHeight = imported.frame.displayHeight;
    if (preferVideoFrame) {
      return {
        kind: "video-frame",
        frame: imported.frame,
        rotation: imported.rotation,
        mirror: imported.mirror ?? false,
        chromaSiting: imported.chromaSiting,
        physicalTimeMicroseconds,
      };
    }
    try {
      const image = imported.frame;
      const width = image.displayWidth ?? image.videoWidth ?? image.width;
      const height = image.displayHeight ?? image.videoHeight ?? image.height;
      return {
        kind: "rgba-frame",
        pixels: this.#drawToBuffer(image, width, height),
        physicalTimeMicroseconds,
        durationMicroseconds,
        sourceWidth,
        sourceHeight,
        rotation: imported.rotation,
      };
    } finally {
      imported.frame.close();
    }
  }

  #drawToBuffer(image, sourceWidth, sourceHeight) {
    const context = this.#context();
    context.clearRect(0, 0, this.#width, this.#height);
    context.imageSmoothingEnabled = true;
    context.imageSmoothingQuality = "high";
    context.drawImage(
      image,
      ...containRect(sourceWidth, sourceHeight, this.#width, this.#height),
    );
    const pixels = context.getImageData(0, 0, this.#width, this.#height).data;
    premultiplyInPlace(pixels);
    return pixels;
  }

  async #decodeFrame(asset, seconds, consumerId, sourceId) {
    if (!asset.file) return null;
    const cursor = decoderCursorKey(sourceId, consumerId);
    let decoder = this.#decoders.get(cursor);
    // The source now reads other media (its proxy, or the original again): the cursor starts over.
    if (decoder && decoder.asset !== asset) {
      await closeDecoder(decoder);
      this.#decoders.delete(cursor);
      decoder = undefined;
    }
    if (!decoder) {
      if (this.#cursorCount() >= this.decoderLimit)
        throw Object.assign(new Error("Video decoder budget reached"), {
          code: "resource_budget",
        });
      decoder = {
        sourceId,
        asset,
        consumerId,
        opening: null,
        video: null,
        failed: false,
        failures: 0,
      };
      this.#decoders.set(cursor, decoder);
    }
    if (decoder.failed) {
      this.#fallbacks.get(sourceId).frames += 1;
      return null;
    }
    // A decoder that cannot open (a codec the browser cannot decode) is the fallback's for good.
    try {
      decoder.video ??= await (decoder.opening ??= this.#openVideo(decoder));
    } catch (error) {
      decoder.opening = null;
      return this.#fallBack(decoder, error, true);
    }
    const request = {
      sourceId: asset.id,
      mediaTimeTicks: BigInt(Math.round(seconds * TICKS_PER_SECOND)),
      reason: "prefetch",
      generation: 0,
    };
    let decoded;
    try {
      decoded = await decoder.video.refill(request);
    } catch {
      // A failed read (stall, decoder error) is retried once with a new decoder, which a stalled
      // decoder's surfaces do not hold up.
      closeOpened(decoder);
      try {
        decoder.opening = this.#openVideo(decoder);
        decoder.video = await decoder.opening;
        decoded = await decoder.video.refill(request);
      } catch (error) {
        closeOpened(decoder);
        decoder.failures += 1;
        return this.#fallBack(
          decoder,
          error,
          decoder.failures >= MAX_DECODE_FAILURES,
        );
      }
    }
    decoder.failures = 0;
    if (decoder.video.softwareFallback && !decoder.reportedSoftware) {
      decoder.reportedSoftware = true;
      console.info(
        `preview decodes ${asset.id} in software: ${decoder.video.softwareFallback}`,
      );
    }
    return {
      frame: decoded.frame,
      rotation: decoded.rotationDegrees ?? 0,
      mirror: decoded.mirror ?? false,
      chromaSiting: decoded.chromaSiting,
    };
  }

  /**
   * Decoder cursors the budget counts, one per consumer: a software cursor beside its consumer's
   * own cursor is not counted again.
   */
  #cursorCount() {
    const consumers = new Set();
    for (const decoder of this.#decoders.values())
      consumers.add(decoderCursorKey(decoder.sourceId, decoder.consumerId));
    return consumers.size;
  }

  /**
   * Opens `decoder`'s source as `videoDecode` says. A source the browser has no software decoder
   * for opens as `"auto"`, for good, and is reported once in `decodeFallbacks`.
   */
  async #openVideo(decoder) {
    const { asset } = decoder;
    if (this.videoDecode !== "software" || decoder.softwareUnavailable)
      return MediabunnyVideoDecoder.open(asset.id, asset.file);
    try {
      return await MediabunnyVideoDecoder.open(asset.id, asset.file, {
        hardwareAcceleration: "prefer-software",
      });
    } catch (error) {
      if (error?.code !== "software_decode_unavailable") throw error;
      decoder.softwareUnavailable = true;
      const key = `software-unavailable\u0000${decoder.sourceId}`;
      if (!this.#fallbacks.has(key))
        this.#fallbacks.set(key, {
          kind: "software-unavailable",
          sourceId: decoder.sourceId,
          reason: String(error.message),
          frames: 0,
          permanent: false,
        });
      return MediabunnyVideoDecoder.open(asset.id, asset.file);
    }
  }

  /**
   * The frame at `seconds` decoded on the CPU, on a cursor of its own beside the consumer's: what
   * `FrameSupply` asks for when the browser could not read a hardware frame back. Rejects rather
   * than falling back to an element seek, whose frame would be the browser's again.
   *
   * The cursor shares the consumer's source and consumer IDs, so `retainConsumers` and `release`
   * close it with the consumer's; it stays open between frames, as readback failures come in runs
   * and the next one then decodes on from it. A consumer whose frames all decode here (a source
   * the browser decodes to opaque frames) has no other cursor, so this one opens within the budget.
   */
  async #softwareFrame(asset, seconds, consumerId, sourceId) {
    if (!asset.file)
      throw new Error(`${asset.id} has no file to decode in software`);
    const primary = decoderCursorKey(sourceId, consumerId);
    const cursor = `${primary}\u0000software`;
    let decoder = this.#decoders.get(cursor);
    if (decoder && decoder.asset !== asset) {
      await closeDecoder(decoder);
      this.#decoders.delete(cursor);
      decoder = undefined;
    }
    if (!decoder) {
      if (
        !this.#decoders.has(primary) &&
        this.#cursorCount() >= this.decoderLimit
      )
        throw Object.assign(new Error("Video decoder budget reached"), {
          code: "resource_budget",
        });
      decoder = {
        sourceId,
        asset,
        consumerId,
        software: true,
        opening: null,
        video: null,
      };
      this.#decoders.set(cursor, decoder);
    }
    try {
      decoder.video ??= await (decoder.opening ??= MediabunnyVideoDecoder.open(
        asset.id,
        asset.file,
        { hardwareAcceleration: "prefer-software" },
      ));
      const decoded = await decoder.video.refill({
        sourceId: asset.id,
        mediaTimeTicks: BigInt(Math.round(seconds * TICKS_PER_SECOND)),
        reason: "prefetch",
        generation: 0,
      });
      return {
        kind: "video-frame",
        frame: decoded.frame,
        rotation: decoded.rotationDegrees ?? 0,
        mirror: decoded.mirror ?? false,
        chromaSiting: decoded.chromaSiting,
        physicalTimeMicroseconds: decoded.frame.timestamp,
      };
    } catch (error) {
      // The next request opens it again.
      closeOpened(decoder);
      throw error;
    }
  }

  /**
   * Records that `decoder`'s frame falls back to an element seek because of `error`; `permanent`
   * applies it to every later frame of the cursor.
   */
  #fallBack(decoder, error, permanent) {
    if (permanent) decoder.failed = true;
    const reason = String(error?.message ?? error);
    const known = this.#fallbacks.get(decoder.sourceId);
    this.#fallbacks.set(decoder.sourceId, {
      kind: "element-seek",
      sourceId: decoder.sourceId,
      reason,
      frames: (known?.frames ?? 0) + 1,
      permanent: permanent || (known?.permanent ?? false),
    });
    console.warn(
      permanent
        ? `preview falls back to HTMLVideoElement seeking for ${decoder.sourceId}: ${reason}`
        : `a frame of ${decoder.sourceId} falls back to HTMLVideoElement seeking: ${reason}`,
    );
    return null;
  }

  /**
   * What fell back, per source and kind: `"element-seek"`, frames read by element seeks rather
   * than decoded, with the last reason, how many frames, and whether its decoding stopped for good
   * (it could not open, or failed {@link MAX_DECODE_FAILURES} frames running);
   * `"software-unavailable"`, a source `videoDecode: "software"` decoded as `"auto"` because the
   * browser has no software decoder for it (`frames` 0). A browser export reports it.
   */
  get decodeFallbacks() {
    return [...this.#fallbacks.values()].map((entry) => ({ ...entry }));
  }

  async #seekFallback(asset, seconds, consumerId, sourceId) {
    if (!asset.video)
      throw Object.assign(
        new Error(
          asset.video === null
            ? `${asset.id} has no media element to seek (the browser's media pipeline could not open it)`
            : `No worker decoder for ${asset.id}; use the main-thread media fallback`,
        ),
        { code: "unsupported_codec" },
      );
    const cursor = decoderCursorKey(sourceId, consumerId);
    let fallback = this.#seekVideos.get(cursor);
    if (fallback && fallback.asset !== asset) {
      unloadVideo(fallback.video);
      this.#seekVideos.delete(cursor);
      fallback = undefined;
    }
    if (!fallback) {
      // Playback audio must never move the cursor used by fallback frame seeks.
      const video = asset.video.cloneNode(true);
      Object.assign(video, {
        autoplay: false,
        muted: true,
        loop: false,
        preload: "auto",
      });
      fallback = { sourceId, asset, consumerId, video };
      this.#seekVideos.set(cursor, fallback);
      await waitForLoad(video);
    }
    const { video } = fallback;
    const duration = Number.isFinite(video.duration)
      ? video.duration
      : asset.duration || 1;
    await seekVideoFrame(
      video,
      Math.min(Math.max(0, seconds), Math.max(0, duration - 1e-3)),
    );
    return video;
  }

  /** Keeps decoder sessions bounded to consumers needed by the current frame. */
  retainConsumers = async (liveConsumerIds) => {
    for (const [cursor, decoder] of this.#decoders) {
      if (liveConsumerIds.has(decoder.consumerId)) continue;
      await closeDecoder(decoder);
      this.#decoders.delete(cursor);
    }
    for (const [cursor, fallback] of this.#seekVideos) {
      if (liveConsumerIds.has(fallback.consumerId)) continue;
      unloadVideo(fallback.video);
      this.#seekVideos.delete(cursor);
    }
  };

  /**
   * Drops decoders, extension sources, and rasters for source IDs not in `liveIds`. The engine
   * calls it before each frame reads its sources, so the previous frame's reads may be evicted.
   */
  async release(liveIds) {
    this.#frame += 1;
    for (const [key, opened] of this.#providers) {
      if (liveIds.has(opened.assetId)) continue;
      this.#providers.delete(key);
      opened.source.then(({ source }) => source.release()).catch(() => {});
    }
    for (const [key, assetId] of this.#providersClosed)
      if (!liveIds.has(assetId)) this.#providersClosed.delete(key);
    this.#dropUnreadMakers();
    for (const [cursor, decoder] of this.#decoders) {
      if (liveIds.has(decoder.sourceId)) continue;
      await closeDecoder(decoder);
      this.#decoders.delete(cursor);
    }
    for (const [cursor, fallback] of this.#seekVideos) {
      if (liveIds.has(fallback.sourceId)) continue;
      unloadVideo(fallback.video);
      this.#seekVideos.delete(cursor);
    }
    for (const [id, entry] of this.#rasters)
      if (!liveIds.has(id)) this.#dropRaster(id, entry);
    this.#evictRasters();
  }

  async dispose() {
    await this.release(new Set());
  }
}

/** Whether two raster keys name the same raster: the same objects and sizes, field by field. */
function sameKey(a, b) {
  const fields = Object.keys(a);
  return (
    fields.length === Object.keys(b).length &&
    fields.every((field) => Object.is(a[field], b[field]))
  );
}

/** The bytes a raster's pixels hold: a text raster is its pixels, a still a raster frame. */
function rasterBytes(raster) {
  return (raster.pixels ?? raster).byteLength;
}

/** Closes a cursor's decoder, including one still opening. */
async function closeDecoder(decoder) {
  const video =
    decoder.video ?? (await decoder.opening?.catch(() => null)) ?? null;
  video?.close();
  decoder.video = null;
  decoder.opening = null;
}

/** Closes a cursor's opened decoder, so its next read opens another. */
function closeOpened(decoder) {
  decoder.video?.close();
  decoder.video = null;
  decoder.opening = null;
}

/** Lets a fallback seek element go of its media. */
function unloadVideo(video) {
  video.removeAttribute("src");
  video.load();
}

function premultiplyInPlace(pixels) {
  for (let index = 0; index < pixels.length; index += 4) {
    const alpha = pixels[index + 3];
    if (alpha === 255) continue;
    pixels[index] = Math.round((pixels[index] * alpha) / 255);
    pixels[index + 1] = Math.round((pixels[index + 1] * alpha) / 255);
    pixels[index + 2] = Math.round((pixels[index + 2] * alpha) / 255);
  }
}

function waitForLoad(video) {
  return new Promise((resolve, reject) => {
    const cleanup = () => {
      clearTimeout(timer);
      video.removeEventListener("loadeddata", ready);
      video.removeEventListener("error", failed);
    };
    const ready = () => {
      cleanup();
      resolve();
    };
    const failed = () => {
      cleanup();
      reject(video.error ?? new Error("video load failed"));
    };
    const timer = setTimeout(() => {
      cleanup();
      reject(new Error("video load timed out"));
    }, 10_000);
    video.addEventListener("loadeddata", ready, { once: true });
    video.addEventListener("error", failed, { once: true });
    video.load();
  });
}

/**
 * Which open extension source answers `consumerId`'s read of `asset`. An animated image decodes in
 * order and keeps only a window of frames, so each reader gets its own (sharing one would rewind
 * it); every other provider serves all readers from one source, as on the native host.
 */
function providerKey(asset, consumerId) {
  return asset.provider.format === "animated_image"
    ? decoderCursorKey(asset.id, consumerId)
    : asset.id;
}

/** Serialized settings by provider object; settings are never mutated, so each serializes once. */
const providerSettingsKeys = new WeakMap();

/**
 * `provider`'s settings as one comparable string, serialized once per provider object: an image
 * sequence's holds every frame's locator.
 */
export function providerSettings(provider) {
  if (provider === null || typeof provider !== "object")
    return JSON.stringify(provider ?? null);
  let key = providerSettingsKeys.get(provider);
  if (key === undefined) {
    key = JSON.stringify(provider);
    providerSettingsKeys.set(provider, key);
  }
  return key;
}

/** Results of {@link sameProviderSettings}, by the first provider, then the second. */
const providerSettingsEqual = new WeakMap();

/**
 * Whether two providers have the same settings: the same object, or equal ones. Two objects are
 * compared once, so a pair asked about for every frame costs no serialization or string compare.
 */
export function sameProviderSettings(a, b) {
  if (a === b) return true;
  const objects =
    a !== null && typeof a === "object" && b !== null && typeof b === "object";
  if (!objects) return providerSettings(a) === providerSettings(b);
  let known = providerSettingsEqual.get(a);
  let same = known?.get(b);
  if (same === undefined) {
    same = providerSettings(a) === providerSettings(b);
    if (!known) providerSettingsEqual.set(a, (known = new WeakMap()));
    known.set(b, same);
  }
  return same;
}

/** Whether a provider source's pixels depend on the fonts: an SVG's text. */
function drawsText(provider) {
  return provider?.format === "svg";
}

/** Whether two font stacks are the same fonts in the same order. */
function sameFonts(a, b) {
  return (
    a.length === b.length &&
    a.every((font, index) => font.bytes === b[index].bytes)
  );
}

/** Errors of {@link openProvider} (module load or file read) that a later open may not meet. */
const retryableOpen = new WeakSet();

/**
 * Opens an asset's extension source, an SVG with `fonts`. An image sequence's frames enter the
 * provider module only for the call that reads them.
 */
async function openProvider(asset, wasmUrl, fonts) {
  const retryable = (error) => {
    const failure = error instanceof Error ? error : new Error(String(error));
    retryableOpen.add(failure);
    return failure;
  };
  const read = async (locator) => {
    const file = asset.frameFiles?.get(locator);
    if (!file) throw new Error(`${asset.id} has no file for frame ${locator}`);
    try {
      return new Uint8Array(await file.arrayBuffer());
    } catch (error) {
      throw retryable(error);
    }
  };
  let extensions;
  let own;
  try {
    const { loadExtensions } = await import("alpha2/extensions");
    extensions = await loadExtensions(wasmUrl);
    own = asset.file
      ? new Uint8Array(await asset.file.arrayBuffer())
      : undefined;
  } catch (error) {
    throw retryable(error);
  }
  const source = await extensions.withResources(
    read,
    () => extensions.openNeeds(asset.provider),
    () => {
      // One module serves preview and export sources whose fonts may differ, so set them in the
      // same task as the open.
      if (fonts) extensions.setFonts(fonts.map((font) => font.bytes));
      return extensions.open(asset.provider, own);
    },
  );
  return { source, extensions, read };
}

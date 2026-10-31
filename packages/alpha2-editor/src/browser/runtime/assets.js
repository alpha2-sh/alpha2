// Media library: import, probe, and keep browser-side resources for each source. The engine only
// learns that a source exists (`browser://<id>`); this module answers its per-frame requests.

import {
  codecName,
  colorMetadataFromVideoColorSpace,
  DecodeUnavailableError,
  decodeRefusalMessage,
  pictureDetails,
  PlanarDecodeUnavailableError,
} from "alpha2/hdr-frames";
import {
  MediabunnyVideoDecoder,
  probePlanarDecode,
  videoTrackFacts,
} from "alpha2/mediabunny";
import { TICKS_PER_SECOND } from "alpha2/project";
import { OffscreenTextRasterizer } from "alpha2/text";
import { ALL_FORMATS, AudioSampleSink, BlobSource, Input } from "mediabunny";
const uid = (prefix) => `${prefix}-${crypto.randomUUID()}`;
import { drawGenerator, generatorLabel } from "./generators.js";
import { seekVideoFrame } from "./preview-utils.js";
import { decodeStill } from "./still.js";
import { readUserText } from "./text.js";
import { EditorError } from "../../core/document.js";
import { TEXT_DEFAULTS } from "../../core/text.js";
import { displaySize } from "./fingerprint.js";

const THUMBNAIL_WIDTH = 168;

export const IMAGE_TYPES = [
  "image/png",
  "image/jpeg",
  "image/webp",
  "image/gif",
  "image/avif",
];
export const VIDEO_TYPES = [
  "video/mp4",
  "video/webm",
  "video/quicktime",
  "video/x-matroska",
];
export const AUDIO_TYPES = [
  "audio/mpeg",
  "audio/mp4",
  "audio/wav",
  "audio/x-wav",
  "audio/ogg",
  "audio/flac",
  "audio/aac",
];
export const ACCEPTED_TYPES = [
  ...VIDEO_TYPES,
  ...IMAGE_TYPES,
  ...AUDIO_TYPES,
].join(",");

/** A title's style where its authoring leaves a field out (`TEXT_DEFAULTS`, exported from core). */
export const DEFAULT_TEXT = TEXT_DEFAULTS;

export class AssetLibrary {
  #assets = new Map();
  #urls = [];
  #rasterizer = new OffscreenTextRasterizer();
  #textVersion = 1n;

  get map() {
    return this.#assets;
  }

  get all() {
    return [...this.#assets.values()];
  }

  get(id) {
    return this.#assets.get(id) ?? null;
  }

  /** Metadata persisted in an editor project. Media bytes stay in local files. */
  projectManifest() {
    return this.all.map((asset) => ({
      id: asset.id,
      kind: asset.kind,
      name: asset.name,
      locator: asset.locator ?? localLocator(asset.file, asset.name),
      type: asset.file?.type ?? asset.type ?? "",
      size: asset.file?.size ?? asset.size ?? null,
      lastModified: asset.file?.lastModified ?? asset.lastModified ?? null,
      width: asset.width ?? null,
      height: asset.height ?? null,
      frameRate: asset.frameRate ?? null,
      duration: asset.duration ?? 0,
      hasAudio: Boolean(asset.hasAudio ?? asset.audioBuffer),
      fit: asset.fit ?? null,
      colorMetadata: asset.colorMetadata ?? null,
      text: asset.kind === "text" ? structuredClone(asset.text) : null,
      generator:
        asset.kind === "generated" ? structuredClone(asset.generator) : null,
    }));
  }

  /** Replaces the library with embedded text assets and missing local-file references. */
  restoreProjectManifest(manifest) {
    this.clear();
    for (const reference of manifest) {
      if (reference.kind === "text" && reference.text) {
        this.createText(reference.text, { id: reference.id });
        continue;
      }
      if (reference.kind === "generated" && reference.generator) {
        this.createGenerated(reference.generator, {
          id: reference.id,
          name: reference.name,
        });
        continue;
      }
      this.#assets.set(reference.id, {
        ...reference,
        missing: true,
        thumbnail: null,
        peaks: null,
        audioBuffer: null,
      });
    }
    return this.all.filter((asset) => asset.missing);
  }

  get missing() {
    return this.all.filter((asset) => asset.missing);
  }

  /** Imports a non-media resource consumed by an engine effect. */
  async importEffectResource(file, kind) {
    if (kind !== "lut3d" || !file.name.toLowerCase().endsWith(".cube")) {
      throw new Error("this effect slot requires a .cube 3D LUT file");
    }
    const reference = this.#matchingReference(file, kind);
    const asset = {
      id: reference?.id ?? uid("fxr"),
      kind,
      name: reference?.name ?? file.name,
      locator: reference?.locator ?? localLocator(file, file.name),
      file,
      text: await readUserText(file),
    };
    this.#assets.set(asset.id, asset);
    return asset;
  }

  usedBy(doc) {
    return new Set(doc.clips.map((clip) => clip.assetId));
  }

  remove(id) {
    const asset = this.#assets.get(id);
    if (!asset) return;
    asset.video?.removeAttribute("src");
    this.#assets.delete(id);
  }

  /**
   * Imports browser files; each one that does not import is reported back with its name and why
   * (see {@link importFile}).
   */
  async importFiles(files, onProgress) {
    const imported = [];
    const rejected = [];
    const relinked = [];
    const list = [...files];
    for (const [index, file] of list.entries()) {
      onProgress?.(index, list.length, file.name);
      try {
        const { asset, relinked: reference } = await this.#import(file);
        imported.push(asset);
        if (reference) relinked.push(asset.id);
      } catch (error) {
        rejected.push({ name: file.name, error });
      }
    }
    return { imported, rejected, relinked };
  }

  /**
   * Imports one browser file as an image, video or audio asset. Rejects with an `EditorError`:
   * `unsupported_media` for a file that is none of those, `unreadable_media` for one that is but
   * that this browser could not read (a codec it does not decode, a damaged file), with the
   * decoder's or demuxer's reason in the message and as the `cause`. Rejects with `signal`'s
   * reason once it aborts a decode.
   */
  async importFile(file, { signal } = {}) {
    return (await this.#import(file, signal)).asset;
  }

  async #import(file, signal) {
    const kind = mediaKindForFile(file);
    if (!kind)
      throw new EditorError(
        "unsupported_media",
        `Cannot import ${file.name}: not an image, video or audio file`,
        "import",
      );
    let as = kind;
    let probe = null;
    try {
      // Probe the container first: browsers type by extension, so an audio-only .webm is `video/*`.
      probe = kind === "video" ? await probeVideo(file) : null;
      if (probe && !probe.video && probe.hasAudio) as = "audio";
      const reference = this.#matchingReference(file, as);
      const asset =
        as === "image"
          ? await this.#importImage(file, reference)
          : as === "video"
            ? await this.#importVideo(file, reference, probe, signal)
            : await this.#importAudio(file, reference);
      return { asset, relinked: !!reference };
    } catch (error) {
      if (signal?.aborted) throw signal.reason;
      if (error instanceof EditorError) throw error;
      throw new EditorError(
        "unreadable_media",
        `Cannot read ${file.name} as ${as}: ${importFailureReason(error, as, probe?.video?.facts)}`,
        "import",
        undefined,
        true,
        { cause: error },
      );
    }
  }

  /** A still keeps its file, size, and thumbnail; `SourcePixels` decodes it again per frame. */
  async #importImage(file, reference = null) {
    const bitmap = await decodeStill(file);
    let width, height, thumbnail;
    try {
      ({ width, height } = bitmap);
      thumbnail = thumbnailFrom(bitmap, width, height);
    } finally {
      bitmap.close();
    }
    const asset = {
      id: reference?.id ?? uid("src"),
      kind: "image",
      name: reference?.name ?? file.name,
      locator: reference?.locator ?? localLocator(file, file.name),
      file,
      width,
      height,
      duration: 0,
      fit: "contain",
      thumbnail,
    };
    this.#assets.set(asset.id, asset);
    return asset;
  }

  /**
   * `probe` is null when the container was unreadable; an element with no picture is refused. A
   * file the media element cannot open still imports, with no element (`video: null`), when the
   * decoder preview and export read it with decodes its picture.
   */
  async #importVideo(file, reference = null, probe = null, signal) {
    if (probe && !probe.video)
      throw new Error("the file has neither a picture nor sound");
    // An HDR source must reach the engine as planes, which some browsers never hand over (an
    // opaque hardware frame and no software decoder): refused here, by name, rather than at its
    // first frame.
    if (probe?.video?.colorMetadata) {
      try {
        await probePlanarDecode(file, { declared: probe.video.colorMetadata });
      } catch (error) {
        if (
          error instanceof PlanarDecodeUnavailableError ||
          error instanceof DecodeUnavailableError
        )
          throw new EditorError(
            "unsupported_media",
            decodeRefusalMessage(file.name, error.facts),
            "import",
            undefined,
            true,
            { cause: error },
          );
        throw error;
      }
    }
    const url = URL.createObjectURL(file);
    this.#urls.push(url);
    const video = document.createElement("video");
    Object.assign(video, {
      src: url,
      muted: true,
      playsInline: true,
      preload: "auto",
      loop: false,
    });
    const failed = await new Promise((resolve) => {
      video.onloadeddata = () => resolve(null);
      video.onerror = () =>
        resolve(video.error ?? new Error("video decode failed"));
    });
    // The element is a fallback reader, not the gate: Chrome's media pipeline refuses some files
    // WebCodecs decodes (a stream copy cut inside an open GOP), which preview and export read.
    if (failed) {
      if (!probe?.video?.decodable) throw failed;
      video.removeAttribute("src");
    }
    const element = failed ? null : video;
    if (!probe && !video.videoWidth)
      throw new Error("the browser found no picture in it");
    const read = probe?.video ?? {
      width: video.videoWidth,
      height: video.videoHeight,
      frameRate: 30,
      colorMetadata: null,
    };
    // A chunked WebM (MediaRecorder timeslice) has no Duration and the element reports +Infinity.
    const duration =
      element && Number.isFinite(element.duration)
        ? element.duration
        : ((await containerDuration(file)) ?? 0);
    const asset = {
      id: reference?.id ?? uid("src"),
      kind: "video",
      name: reference?.name ?? file.name,
      locator: reference?.locator ?? localLocator(file, file.name),
      file,
      url,
      video: element,
      width: read.width,
      height: read.height,
      frameRate: read.frameRate,
      duration,
      // Having audio is the document's fact; whether this browser decodes it is the session's.
      hasAudio: probe?.hasAudio ?? false,
      audioDecodable: probe?.audioDecodable ?? false,
      colorMetadata: read.colorMetadata,
      fit: "contain",
      thumbnail: null,
      peaks: null,
    };
    if (element)
      asset.thumbnail = await videoThumbnail(element, read.width, read.height);
    else {
      try {
        asset.thumbnail = await decodedThumbnail(asset, signal);
      } catch (error) {
        throw new Error(
          "the media element could not open it, and decoding it with WebCodecs failed too",
          { cause: error },
        );
      }
    }
    this.#assets.set(asset.id, asset);
    return asset;
  }

  async #importAudio(file, reference = null) {
    const { duration, decodable } = await probeAudio(file);
    const asset = {
      id: reference?.id ?? uid("src"),
      kind: "audio",
      name: reference?.name ?? file.name,
      locator: reference?.locator ?? localLocator(file, file.name),
      file,
      duration,
      hasAudio: true,
      audioDecodable: decodable,
      peaks: null,
      thumbnail: null,
    };
    this.#assets.set(asset.id, asset);
    return asset;
  }

  /** Registers a project-rate audio buffer synthesized in the page rather than read from a file. */
  addGeneratedAudio(name, audioBuffer) {
    const asset = {
      id: uid("src"),
      kind: "audio",
      name,
      generated: true,
      duration: audioBuffer.duration,
      hasAudio: true,
      audioBuffer,
      peaks: computePeaks(audioBuffer),
      thumbnail: null,
    };
    this.#assets.set(asset.id, asset);
    return asset;
  }

  /**
   * Creates a text source rasterized by the packaged `OffscreenTextRasterizer`, the browser host
   * for the engine's host-rasterized text contract.
   */
  createText(style = {}, { id = uid("src") } = {}) {
    const text = { ...DEFAULT_TEXT, ...style };
    const asset = {
      id,
      kind: "text",
      name: text.content.split("\n")[0].slice(0, 28) || "Text",
      text,
      duration: 0,
      fit: "native",
      raster: null,
      thumbnail: null,
    };
    this.#assets.set(asset.id, asset);
    this.rasterizeText(asset, text);
    return asset;
  }

  /**
   * Creates a source the engine draws itself from `generator` (see `generators.js`); it needs no
   * file and has no duration.
   */
  createGenerated(generator, { id = uid("gen"), name = null } = {}) {
    const asset = {
      id,
      kind: "generated",
      name: name ?? generatorLabel(generator.kind),
      generator: structuredClone(generator),
      duration: 0,
      thumbnail: null,
    };
    asset.thumbnail = generatedThumbnail(asset.generator);
    this.#assets.set(asset.id, asset);
    return asset;
  }

  updateGenerated(asset, generator) {
    const renamed = asset.name === generatorLabel(asset.generator.kind);
    asset.generator = structuredClone(generator);
    if (renamed) asset.name = generatorLabel(generator.kind);
    asset.thumbnail = generatedThumbnail(asset.generator);
  }

  rasterizeText(asset, text) {
    asset.text = { ...asset.text, ...text };
    asset.name = asset.text.content.split("\n")[0].slice(0, 28) || "Text";
    this.#textVersion += 1n;
    const raster = this.#rasterizer.rasterize({
      text: asset.text.content || " ",
      contentVersion: this.#textVersion,
      fontFamilies: asset.text.fontFamily
        .split(",")
        .map((family) => family.trim()),
      fontSize: Math.max(4, asset.text.fontSize),
      maxWidth: asset.text.maxWidth > 0 ? asset.text.maxWidth : undefined,
      lineHeight: asset.text.lineHeight,
      align: asset.text.align,
      color: asset.text.color,
    });
    asset.raster = raster;
    asset.width = raster.texture.width;
    asset.height = raster.texture.height;
    asset.thumbnail = textThumbnail(raster);
    return asset;
  }

  dispose() {
    this.clear();
    this.#rasterizer.close();
  }

  clear() {
    for (const url of this.#urls) URL.revokeObjectURL(url);
    this.#urls = [];
    this.#assets.clear();
  }

  #matchingReference(file, kind) {
    const candidates = this.missing.filter((asset) => asset.kind === kind);
    const path = file.webkitRelativePath || file.name;
    return (
      candidates.find((asset) => asset.locator === path) ??
      candidates.find(
        (asset) => asset.name === file.name && matchesFileIdentity(asset, file),
      ) ??
      candidates.find((asset) => basename(asset.locator) === file.name)
    );
  }
}

function localLocator(file, fallback) {
  return file?.webkitRelativePath || file?.name || fallback || "";
}

function basename(locator) {
  return String(locator ?? "")
    .replaceAll("\\", "/")
    .split("/")
    .at(-1);
}

function matchesFileIdentity(reference, file) {
  const sizeMatches =
    reference.size === null ||
    reference.size === undefined ||
    reference.size === file.size;
  const modifiedMatches =
    reference.lastModified === null ||
    reference.lastModified === undefined ||
    reference.lastModified === file.lastModified;
  return sizeMatches && modifiedMatches;
}

function mediaKindForFile(file) {
  if (file.type.startsWith("image/")) return "image";
  if (file.type.startsWith("video/")) return "video";
  if (file.type.startsWith("audio/")) return "audio";
  const extension = file.name.toLowerCase().split(".").at(-1);
  if (["png", "jpg", "jpeg", "webp", "gif", "avif"].includes(extension))
    return "image";
  if (["mp4", "webm", "mov", "mkv"].includes(extension)) return "video";
  if (["mp3", "m4a", "wav", "ogg", "flac", "aac"].includes(extension))
    return "audio";
  return null;
}

/**
 * What a video container holds, read by the demuxer: its picture (`video`: size, frame rate, an HDR
 * declaration, and whether WebCodecs decodes it), or null when it has no video track, and whether
 * it has an audio stream. Null when the demuxer cannot read the container.
 */
export async function probeVideo(file) {
  const input = new Input({
    source: new BlobSource(file),
    formats: ALL_FORMATS,
  });
  try {
    const track = await input.getPrimaryVideoTrack();
    const audio = await input.getPrimaryAudioTrack();
    let video = null;
    if (track) {
      const stats = await track.computePacketStats(120).catch(() => null);
      const space = await track.getColorSpace().catch(() => null);
      const shown = await displaySize(track);
      video = {
        // Only an HDR declaration is recorded, so the project knows the source needs the tone map.
        colorMetadata:
          space?.transfer === "pq" || space?.transfer === "hlg"
            ? colorMetadataFromVideoColorSpace(space)
            : null,
        width: shown.width,
        height: shown.height,
        frameRate: stats?.averagePacketRate ?? 30,
        decodable: await track.canDecode().catch(() => false),
        facts: await videoTrackFacts(track),
      };
    }
    return {
      video,
      hasAudio: audio !== null,
      audioDecodable: Boolean(
        audio && (await audio.canDecode().catch(() => false)),
      ),
    };
  } catch (error) {
    console.warn("probe fell back to HTMLVideoElement metadata", error);
    return null;
  } finally {
    input.dispose();
  }
}

/** `MediaError` codes: the source is in a format the element does not read, or did not decode. */
const MEDIA_ERR_DECODE = 3;
const MEDIA_ERR_SRC_NOT_SUPPORTED = 4;

/**
 * Why an import failed, in this editor's words: a browser's own error (a media element's
 * `MediaError`, a decoder's `DOMException`) is never shown, since its text is the engine's
 * internals (Gecko names its C++ classes); it stays the error's cause. `facts` describe the
 * video track when the probe read one.
 */
export function importFailureReason(error, kind, facts) {
  const browserError =
    typeof DOMException !== "undefined" && error instanceof DOMException;
  const mediaError =
    !(error instanceof Error) && typeof error?.code === "number";
  if (!mediaError && !browserError && error instanceof Error)
    return error.message;
  const undecodable =
    browserError ||
    error?.code === MEDIA_ERR_DECODE ||
    error?.code === MEDIA_ERR_SRC_NOT_SUPPORTED;
  if (!undecodable) return `this browser could not read the ${kind}`;
  if (kind !== "video" || !facts)
    return `this browser cannot decode this ${kind}`;
  const details = pictureDetails(facts);
  return `this browser cannot decode ${codecName(facts.codec)} video${
    details.length > 0 ? ` (${details.join(", ")})` : ""
  }`;
}

/** A container's length in seconds, as its last packet ends; null when it cannot be read. */
async function containerDuration(file) {
  const input = new Input({
    source: new BlobSource(file),
    formats: ALL_FORMATS,
  });
  try {
    const seconds = await input.computeDuration();
    return Number.isFinite(seconds) && seconds > 0 ? seconds : null;
  } catch {
    return null;
  } finally {
    input.dispose();
  }
}

/**
 * An audio file's duration, and whether this browser can decode it. A file this browser cannot
 * decode (Safari before 26 has no `AudioDecoder`) still imports: the document records the audio,
 * which a native host and another browser hear.
 */
async function probeAudio(file) {
  const input = new Input({
    source: new BlobSource(file),
    formats: ALL_FORMATS,
  });
  try {
    const track = await input.getPrimaryAudioTrack();
    if (!track) throw new Error("media has no audio track");
    const decodable = await track.canDecode().catch(() => false);
    const duration =
      (await track.getDurationFromMetadata().catch(() => null)) ??
      (decodable ? await track.computeDuration() : 0);
    return { duration, decodable };
  } finally {
    input.dispose();
  }
}

/**
 * Streams a file's primary audio track into per-bucket peak magnitudes (`bucketsPerSecond`, capped
 * at `maxBuckets`) with bounded memory. A bucket's peak is its loudest sample in any channel,
 * since decoders order channels differently. `null` without audio.
 */
export async function streamPeaks(
  file,
  { signal, bucketsPerSecond = 100, maxBuckets = 360_000 } = {},
) {
  const input = new Input({
    source: new BlobSource(file),
    formats: ALL_FORMATS,
  });
  try {
    const track = await input.getPrimaryAudioTrack();
    if (!track || !(await track.canDecode())) return null;
    const duration =
      (await track.getDurationFromMetadata().catch(() => null)) ??
      (await track.computeDuration());
    if (!duration) return null;
    const buckets = Math.max(
      1,
      Math.min(maxBuckets, Math.ceil(duration * bucketsPerSecond)),
    );
    const peaks = new Float32Array(buckets);
    const scale = buckets / duration;
    let plane = new Float32Array(0);
    for await (const sample of new AudioSampleSink(track).samples()) {
      try {
        signal?.throwIfAborted();
        const frames = sample.numberOfFrames;
        if (plane.length < frames) plane = new Float32Array(frames);
        for (let c = 0; c < sample.numberOfChannels; c++) {
          sample.copyTo(plane, { planeIndex: c, format: "f32-planar" });
          const start = sample.timestamp;
          const step = 1 / sample.sampleRate;
          for (let i = 0; i < frames; i++) {
            const b = Math.floor((start + i * step) * scale);
            if (b < 0 || b >= buckets) continue;
            const v = Math.abs(plane[i]);
            if (v > peaks[b]) peaks[b] = v;
          }
        }
      } finally {
        sample.close();
      }
    }
    return peaks;
  } finally {
    input.dispose();
  }
}

/** Per-bucket peak magnitudes used to draw clip waveforms. */
export function computePeaks(buffer, buckets = 1_200) {
  const channel = buffer.getChannelData(0);
  const second =
    buffer.numberOfChannels > 1 ? buffer.getChannelData(1) : channel;
  const peaks = new Float32Array(buckets);
  const span = Math.max(1, Math.floor(channel.length / buckets));
  for (let bucket = 0; bucket < buckets; bucket += 1) {
    const start = bucket * span;
    let peak = 0;
    for (
      let index = start;
      index < Math.min(start + span, channel.length);
      index += 1
    ) {
      peak = Math.max(peak, Math.abs(channel[index]), Math.abs(second[index]));
    }
    peaks[bucket] = peak;
  }
  return peaks;
}

/**
 * A thumbnail of `image` shown at `width` x `height`. With `orientation`, `image` is a decoded
 * frame that shows upright mirrored left to right when `mirror` is set, then turned `rotation`
 * degrees clockwise.
 */
function thumbnailFrom(image, width, height, orientation = null) {
  // Rendered once at import; the timeline re-reads `dataUrl` on every redraw.
  const scale = THUMBNAIL_WIDTH / Math.max(1, width);
  const canvas = document.createElement("canvas");
  canvas.width = THUMBNAIL_WIDTH;
  canvas.height = Math.max(1, Math.round(height * scale));
  const context = canvas.getContext("2d");
  if (orientation) {
    const turned = orientation.rotation % 180 !== 0;
    const drawn = turned
      ? { width: canvas.height, height: canvas.width }
      : { width: canvas.width, height: canvas.height };
    context.translate(canvas.width / 2, canvas.height / 2);
    context.rotate((orientation.rotation * Math.PI) / 180);
    if (orientation.mirror) context.scale(-1, 1);
    context.drawImage(
      image,
      -drawn.width / 2,
      -drawn.height / 2,
      drawn.width,
      drawn.height,
    );
  } else context.drawImage(image, 0, 0, canvas.width, canvas.height);
  canvas.dataUrl = canvas.toDataURL("image/jpeg", 0.72);
  return canvas;
}

async function videoThumbnail(video, width, height) {
  const clone = video.cloneNode(true);
  clone.muted = true;
  try {
    await new Promise((resolve, reject) => {
      clone.onloadeddata = resolve;
      clone.onerror = () => reject(clone.error);
      clone.load();
    });
    await seekVideoFrame(clone, Math.min(0.25 * (clone.duration || 1), 1));
    return thumbnailFrom(clone, width, height);
  } catch {
    return null;
  } finally {
    clone.removeAttribute("src");
  }
}

/**
 * A thumbnail of `asset`, a file the media element cannot open, decoded by the decoder preview and
 * export read it with: its stall limit bounds a decoder that never answers. Rejects when it decodes
 * no picture, or with `signal`'s reason once it aborts.
 */
async function decodedThumbnail(asset, signal) {
  const decoder = await MediabunnyVideoDecoder.open(asset.id, asset.file);
  try {
    // Requests before the stream's first frame present that frame, as preview shows it.
    const seconds = Math.min(0.25 * (asset.duration || 1), 1);
    const { frame, rotationDegrees, mirror } = await decoder.refill(
      {
        sourceId: asset.id,
        mediaTimeTicks: BigInt(Math.round(seconds * TICKS_PER_SECOND)),
        reason: "seek",
        generation: 0,
      },
      signal,
    );
    try {
      return thumbnailFrom(frame, asset.width, asset.height, {
        rotation: rotationDegrees ?? 0,
        mirror: mirror ?? false,
      });
    } finally {
      frame.close();
    }
  } finally {
    decoder.close();
  }
}

function generatedThumbnail(generator) {
  const canvas = document.createElement("canvas");
  canvas.width = THUMBNAIL_WIDTH;
  canvas.height = Math.round((THUMBNAIL_WIDTH * 9) / 16);
  drawGenerator(
    canvas.getContext("2d"),
    generator,
    canvas.width,
    canvas.height,
  );
  canvas.dataUrl = canvas.toDataURL("image/png");
  return canvas;
}

function textThumbnail(raster) {
  const canvas = document.createElement("canvas");
  canvas.width = THUMBNAIL_WIDTH;
  canvas.height = Math.max(
    1,
    Math.round(
      (raster.texture.height / raster.texture.width) * THUMBNAIL_WIDTH,
    ),
  );
  const context = canvas.getContext("2d");
  const source = new ImageData(
    new Uint8ClampedArray(unpremultiply(raster.rgba8)),
    raster.texture.width,
    raster.texture.height,
  );
  const staging = new OffscreenCanvas(
    raster.texture.width,
    raster.texture.height,
  );
  staging.getContext("2d").putImageData(source, 0, 0);
  context.drawImage(staging, 0, 0, canvas.width, canvas.height);
  canvas.dataUrl = canvas.toDataURL("image/png");
  return canvas;
}

/** `putImageData` expects straight alpha; the rasterizer emits premultiplied. */
function unpremultiply(pixels) {
  const output = new Uint8ClampedArray(pixels.length);
  for (let index = 0; index < pixels.length; index += 4) {
    const alpha = pixels[index + 3];
    const scale = alpha === 0 ? 0 : 255 / alpha;
    output[index] = pixels[index] * scale;
    output[index + 1] = pixels[index + 1] * scale;
    output[index + 2] = pixels[index + 2] * scale;
    output[index + 3] = alpha;
  }
  return output;
}

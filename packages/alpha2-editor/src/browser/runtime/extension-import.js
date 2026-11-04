// Importing media an extension provider reads. The provider is chosen by probing bytes, never by
// file name; the extensions wasm module loads only when the bytes could match one of its formats.
import {
  MAX_LOTTIE_DOCUMENT_BYTES,
  MAX_PDF_DOCUMENT_BYTES,
  MAX_PROVIDER_DOCUMENT_BYTES,
  MAX_SEQUENCE_FRAMES,
  MAX_SVG_DOCUMENT_BYTES,
} from "alpha2/project";
import { EditorError } from "../../core/document.js";

const TICKS_PER_SECOND = 120_000;

/** Whether a file's start could be SVG, GIF, WebP, APNG, Lottie, dotLottie, OpenEXR or PDF. */
export function mayBeExtension(head) {
  const ascii = (at, text) =>
    [...text].every((char, index) => head[at + index] === char.charCodeAt(0));
  if (ascii(0, "GIF8") || isExr(head) || isPdf(head)) return true;
  if (ascii(0, "RIFF") && ascii(8, "WEBP"))
    return ascii(12, "VP8X") && (head[20] & 0x02) !== 0;
  if (head[0] === 0x89 && ascii(1, "PNG")) return pngDeclaresAnimation(head);
  if (ascii(0, "PK\u0003\u0004")) return true;
  const text = new TextDecoder().decode(head);
  if (/^\s*\{/.test(text)) return true;
  // An SVG root may follow a prolog longer than the head, so XML starting with one is probed.
  return /<svg[\s>/]/.test(text) || /^\s*<[?!]/.test(text);
}

/**
 * The byte limit for a file that could be a document format a provider parses whole, by its start:
 * dotLottie (ZIP, `MAX_PROVIDER_DOCUMENT_BYTES`), Lottie (JSON, `MAX_LOTTIE_DOCUMENT_BYTES`), SVG
 * (XML, `MAX_SVG_DOCUMENT_BYTES`) or PDF (`MAX_PDF_DOCUMENT_BYTES`), with the format's name;
 * `null` for other files. Larger files are refused unread.
 */
export function providerDocumentLimit(head) {
  if (isPdf(head))
    return { limit: MAX_PDF_DOCUMENT_BYTES, what: "PDF document" };
  if (
    head[0] === 0x50 &&
    head[1] === 0x4b &&
    head[2] === 0x03 &&
    head[3] === 0x04
  )
    return { limit: MAX_PROVIDER_DOCUMENT_BYTES, what: "dotLottie package" };
  const text = new TextDecoder().decode(head);
  if (/^\s*\{/.test(text))
    return { limit: MAX_LOTTIE_DOCUMENT_BYTES, what: "Lottie file" };
  if (/<svg[\s>/]/.test(text) || /^\s*<[?!]/.test(text))
    return { limit: MAX_SVG_DOCUMENT_BYTES, what: "SVG document" };
  return null;
}

/** Whether `%PDF-` starts within the first 1024 bytes, as PDF readers accept. */
function isPdf(head) {
  const signature = [0x25, 0x50, 0x44, 0x46, 0x2d];
  const end = Math.min(head.length, 1024) - signature.length;
  for (let at = 0; at <= end; at++) {
    if (signature.every((byte, index) => head[at + index] === byte))
      return true;
  }
  return false;
}

function isExr(head) {
  return (
    head[0] === 0x76 && head[1] === 0x2f && head[2] === 0x31 && head[3] === 0x01
  );
}

/** Whether a PNG's chunks before its image data include an animation control. */
function pngDeclaresAnimation(head) {
  const view = new DataView(head.buffer, head.byteOffset, head.byteLength);
  for (let at = 8; at + 8 <= head.length;) {
    const kind = String.fromCharCode(...head.subarray(at + 4, at + 8));
    if (kind === "acTL") return true;
    if (kind === "IDAT") return false;
    at += 12 + view.getUint32(at);
  }
  return false;
}

/** The duration of a provider's timing in seconds, or 0 for a still. */
export function timingSeconds(timing) {
  if (timing.kind === "frames")
    return (
      timing.durations.reduce((sum, ticks) => sum + ticks, 0) / TICKS_PER_SECOND
    );
  if (timing.kind === "rate")
    return (timing.frames * timing.rate.den) / timing.rate.num;
  return 0;
}

function halfValue(bits) {
  const exponent = (bits >> 10) & 0x1f;
  const fraction = bits & 0x3ff;
  const sign = bits & 0x8000 ? -1 : 1;
  if (exponent === 0) return sign * fraction * 2 ** -24;
  if (exponent === 31) return fraction ? Number.NaN : sign * Infinity;
  return sign * (1 + fraction / 1024) * 2 ** (exponent - 15);
}

/** The sRGB code (0 to 255) of a linear value, clipped to SDR. */
function srgbCode(linear) {
  const value = Math.min(1, Math.max(0, linear));
  const encoded =
    value <= 0.0031308 ? 12.92 * value : 1.055 * value ** (1 / 2.4) - 0.055;
  return Math.round(encoded * 255);
}

/** A thumbnail's width, as an image sequence's PNG thumbnail has it. */
const THUMBNAIL_WIDTH = 160;

/**
 * A small preview of a rendered frame (premultiplied sRGB RGBA8 or linear half floats) as an image
 * URL at most {@link THUMBNAIL_WIDTH} wide. Only sampled pixels are converted.
 */
export async function thumbnailOf(frame) {
  const width = Math.min(THUMBNAIL_WIDTH, frame.width);
  const height = Math.max(
    1,
    Math.min(frame.height, Math.round((width * frame.height) / frame.width)),
  );
  const straight = new Uint8ClampedArray(width * height * 4);
  for (let y = 0; y < height; y += 1) {
    const sourceY = Math.floor(((y + 0.5) * frame.height) / height);
    for (let x = 0; x < width; x += 1) {
      const sourceX = Math.floor(((x + 0.5) * frame.width) / width);
      const from = (sourceY * frame.width + sourceX) * 4;
      const at = (y * width + x) * 4;
      if (frame.precision === "linear_f16") {
        const alpha = Math.min(
          1,
          Math.max(0, halfValue(frame.pixels[from + 3])),
        );
        for (let channel = 0; channel < 3; channel += 1)
          straight[at + channel] = alpha
            ? srgbCode(halfValue(frame.pixels[from + channel]) / alpha)
            : 0;
        straight[at + 3] = Math.round(alpha * 255);
        continue;
      }
      const alpha = frame.pixels[from + 3];
      for (let channel = 0; channel < 3; channel += 1)
        straight[at + channel] = alpha
          ? Math.min(
              255,
              Math.round((frame.pixels[from + channel] * 255) / alpha),
            )
          : 0;
      straight[at + 3] = alpha;
    }
  }
  const canvas = new OffscreenCanvas(width, height);
  canvas
    .getContext("2d")
    .putImageData(new ImageData(straight, width, height), 0, 0);
  const blob = await canvas.convertToBlob({ type: "image/png" });
  const dataUrl = await new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result);
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(blob);
  });
  return { dataUrl, width, height };
}

/**
 * The provider a relinked file opens with: the defaults {@link stageProviderFile} staged, plus the
 * choices `recorded` for the source where the new file still offers them (a package's animation,
 * an OpenEXR still's primaries).
 */
export function keepProviderChoices(recorded, staged, report) {
  if (!recorded || !staged || recorded.format !== staged.format) return staged;
  if (
    staged.format === "lottie" &&
    recorded.animation !== undefined &&
    report?.items?.includes(recorded.animation)
  )
    return { ...staged, animation: recorded.animation };
  if (
    staged.format === "exr" &&
    recorded.primaries !== undefined &&
    report?.color
  )
    return { ...staged, primaries: recorded.primaries };
  if (
    staged.format === "pdf" &&
    recorded.page !== undefined &&
    report?.items?.includes(String(recorded.page))
  )
    return { ...staged, page: recorded.page };
  return staged;
}

/** The provider source import records for a probed file. */
function providerOf(report) {
  if (report.format === "svg" || report.format === "exr")
    return { format: report.format };
  // The first page, until the user picks another.
  if (report.format === "pdf") return { format: "pdf", page: 1 };
  if (report.format === "lottie" || report.format === "dot_lottie")
    return {
      format: "lottie",
      // A package's first animation that opens, until the user picks another.
      ...(report.items.length
        ? { animation: report.item ?? report.items[0] }
        : {}),
    };
  return { format: "animated_image" };
}

/**
 * What import reads of `bytes` with provider module `extensions`: the claiming provider, the probe
 * report and a first-frame thumbnail; `null` when no provider claims them. The source is opened as
 * import records it, so a file that probes but does not open is refused here.
 *
 * @throws When a provider claims the bytes and cannot read them.
 */
export async function readProviderFile(extensions, bytes) {
  const found = await extensions.probe(bytes);
  if (!found) return null;
  const read = await readProviderSource(
    extensions,
    providerOf(found.report),
    bytes,
  );
  return { ...read, report: found.report };
}

/**
 * What a provider source shows: its report and a first-frame thumbnail. `bytes` are its file,
 * absent for a provider that reads none.
 *
 * @throws When the source does not open.
 */
export async function readProviderSource(extensions, provider, bytes) {
  await extensions.prepare(provider);
  const source = extensions.open(provider, bytes);
  try {
    return {
      report: source.report,
      provider,
      thumbnail: await thumbnailOf(
        source.render(0n, { width: 160, height: 90 }),
      ),
    };
  } finally {
    source.release();
  }
}

/**
 * An SVG's diagnostics when opened with font stack `fonts`, listing text no font draws
 * (`missing_font`).
 */
export function svgDiagnostics(extensions, provider, bytes, fonts) {
  extensions.setFonts(fonts);
  const source = extensions.open(provider, bytes);
  try {
    return source.report.diagnostics;
  } finally {
    source.release();
  }
}

/**
 * Stages a provider-claimed file from what {@link readProviderFile} read: a still SVG is an image
 * source, an animation a video source of one cycle (repetition is the clip's).
 */
export function stageProviderFile(
  file,
  { report, provider, thumbnail },
  fingerprint,
) {
  const kind = report.timing.kind === "static" ? "image" : "video";
  const duration = timingSeconds(report.timing);
  const id = crypto.randomUUID();
  const runtime = {
    id,
    kind,
    name: file.name,
    file,
    provider,
    report,
    width: report.width,
    height: report.height,
    duration,
    hasAudio: false,
    fit: "contain",
    thumbnail,
    ...(report ? { report } : {}),
  };
  return {
    runtime,
    description: {
      id,
      kind,
      name: file.name,
      type: file.type,
      size: file.size,
      lastModified: file.lastModified,
      width: report.width,
      height: report.height,
      duration,
      provider,
    },
    ...(fingerprint
      ? {
          fingerprint: {
            ...fingerprint,
            streams: { video: { width: report.width, height: report.height } },
            ...(duration > 0
              ? { duration_ticks: Math.round(duration * TICKS_PER_SECOND) }
              : {}),
          },
        }
      : {}),
    dispose() {},
  };
}

/** The frame number a file's name ends with, before its extension. */
function frameNumber(name) {
  const match = /(\d+)\.[^.]+$/.exec(name);
  return match ? Number(match[1]) : null;
}

/** Missing numbers a description of a sequence lists. */
const LISTED_GAPS = 5;

/**
 * How numbered files form one image sequence from their names alone: the frame number before each
 * name's extension fixes the order. Answers the first and last numbers, the frame count (every
 * number between them), how many have no file, and the first few gaps. Otherwise answers why not:
 * a name without a number, a duplicate number, a number past 2^53 - 1 (a document cannot hold it
 * exactly), or too many frames. Never walks the numbers in between.
 *
 * @param {readonly string[]} names
 * @returns {{ ok: true, first: number, last: number, frames: number, missing: number, gaps: number[] } | { ok: false, code: string, message: string }}
 */
export function sequenceNumbering(names) {
  const numbers = [];
  for (const name of names) {
    const number = frameNumber(name);
    if (number === null)
      return {
        ok: false,
        code: "sequence_numbering",
        message: `${name} has no frame number`,
      };
    if (!Number.isSafeInteger(number))
      return {
        ok: false,
        code: "sequence_numbering",
        message: `${name}'s frame number is larger than ${Number.MAX_SAFE_INTEGER}`,
      };
    numbers.push(number);
  }
  if (numbers.length === 0)
    return {
      ok: false,
      code: "sequence_empty",
      message: "Choose the sequence's frames",
    };
  numbers.sort((a, b) => a - b);
  for (let index = 1; index < numbers.length; index += 1)
    if (numbers[index] === numbers[index - 1])
      return {
        ok: false,
        code: "sequence_numbering",
        message: `Frame ${numbers[index]} is listed twice`,
      };
  const first = numbers[0];
  const last = numbers.at(-1);
  const frames = last - first + 1;
  if (frames > MAX_SEQUENCE_FRAMES)
    return {
      ok: false,
      code: "sequence_range",
      message: `Frames ${first} to ${last} are ${frames} frames; a sequence has at most ${MAX_SEQUENCE_FRAMES}`,
    };
  const gaps = [];
  for (
    let index = 1;
    index < numbers.length && gaps.length < LISTED_GAPS;
    index += 1
  )
    for (
      let number = numbers[index - 1] + 1;
      number < numbers[index] && gaps.length < LISTED_GAPS;
      number += 1
    )
      gaps.push(number);
  return {
    ok: true,
    first,
    last,
    frames,
    missing: frames - numbers.length,
    gaps,
  };
}

/** A PNG's size from its header, or `null` when it is not a PNG. */
async function pngSize(file) {
  const head = new Uint8Array(await file.slice(0, 24).arrayBuffer());
  if (head[0] !== 0x89 || head[1] !== 0x50) return null;
  const view = new DataView(head.buffer);
  return { width: view.getUint32(16), height: view.getUint32(20) };
}

/** Header bytes read for an OpenEXR file's display window. */
const EXR_HEADER_BYTES = 64 * 1024;
/**
 * Attributes read before the display window; writers use a few dozen, so more means not an image.
 */
const EXR_MAX_ATTRIBUTES = 1024;

/**
 * An OpenEXR image's display window size from its header, or `null` when not OpenEXR. The walk
 * stops at an attribute that does not end past its start, so a crafted size cannot loop.
 */
async function exrSize(file) {
  const head = new Uint8Array(
    await file.slice(0, EXR_HEADER_BYTES).arrayBuffer(),
  );
  if (!isExr(head)) return null;
  const view = new DataView(head.buffer);
  const text = (at) => {
    const end = head.indexOf(0, at);
    if (end < 0) return null;
    return {
      value: String.fromCharCode(...head.subarray(at, end)),
      next: end + 1,
    };
  };
  for (
    let at = 8, attributes = 0;
    at < head.length && attributes < EXR_MAX_ATTRIBUTES;
    attributes += 1
  ) {
    const name = text(at);
    if (!name || name.value === "") return null;
    const type = text(name.next);
    if (!type || type.next + 4 > head.length) return null;
    const size = view.getInt32(type.next, true);
    const value = type.next + 4;
    if (name.value === "displayWindow" && type.value === "box2i") {
      if (value + 16 > head.length) return null;
      const [xMin, yMin, xMax, yMax] = [0, 4, 8, 12].map((offset) =>
        view.getInt32(value + offset, true),
      );
      return { width: xMax - xMin + 1, height: yMax - yMin + 1 };
    }
    if (size < 0) return null;
    at = value + size;
  }
  return null;
}

/** A still frame's format and size from its header: a PNG or an OpenEXR image. */
async function frameShape(file) {
  const png = await pngSize(file);
  if (png) return { format: "png", ...png };
  const exr = await exrSize(file);
  if (exr) return { format: "exr", ...exr };
  return null;
}

/**
 * Stages numbered PNGs or OpenEXR images as one image sequence at `frameRate`. The numbers fix the
 * order and first frame; a gap is refused unless `missing` holds or blanks it, and every frame must
 * match the first one's format and size. `openExr(file)` returns the provider module's thumbnail
 * and report for an OpenEXR first frame (see {@link exrFirstFrame}). `id` names the source (a
 * relink uses the asset's own), new by default.
 */
export async function stageSequence(
  files,
  { frameRate, missing = "reject", openExr, id = crypto.randomUUID() },
) {
  const numbering = sequenceNumbering(files.map((file) => file.name));
  if (!numbering.ok)
    throw new EditorError(numbering.code, numbering.message, "import");
  const numbered = files
    .map((file) => ({ file, number: frameNumber(file.name) }))
    .sort((a, b) => a.number - b.number);
  const { first, last } = numbering;
  const size = await frameShape(numbered[0].file);
  if (!size)
    throw new EditorError(
      "unsupported_media",
      `${numbered[0].file.name} is not a PNG or an OpenEXR image`,
      "import",
    );
  const kind = size.format === "exr" ? "OpenEXR image" : "PNG";
  for (const { file } of numbered.slice(1)) {
    const other = await frameShape(file);
    if (
      !other ||
      other.format !== size.format ||
      other.width !== size.width ||
      other.height !== size.height
    )
      throw new EditorError(
        "sequence_size",
        `${file.name} is not a ${size.width}x${size.height} ${kind} like the first frame`,
        "import",
      );
  }
  if (numbering.missing > 0 && missing === "reject")
    throw new EditorError(
      "sequence_gap",
      `Frames ${numbering.gaps.join(", ")}${numbering.missing > numbering.gaps.length ? "..." : ""} are missing; hold or blank them to import anyway`,
      "import",
    );
  const locator = (number) => `browser://${id}/frame/${number}`;
  // Every number from first to last is a frame; a gap is null.
  const frames = new Array(numbering.frames).fill(null);
  const frameFiles = new Map();
  for (const { file, number } of numbered) {
    frames[number - first] = locator(number);
    frameFiles.set(locator(number), file);
  }
  const provider = {
    format: "image_sequence",
    frames,
    frame_rate: frameRate,
    start_number: first,
    missing,
  };
  const duration = (frames.length * frameRate.den) / frameRate.num;
  const stem = numbered[0].file.name.replace(/\d+\.[^.]+$/, "");
  const name = `${stem}[${first}-${last}].${size.format}`;
  const opened =
    size.format === "exr"
      ? await openExr(numbered[0].file)
      : { thumbnail: await pngThumbnail(numbered[0].file, size), report: null };
  const { thumbnail } = opened;
  // An OpenEXR sequence's color and report are its first frame's.
  const report = opened.report && {
    ...opened.report,
    format: "exr_sequence",
    timing: { kind: "rate", rate: frameRate, frames: frames.length },
  };
  const runtime = {
    id,
    kind: "video",
    name,
    provider,
    frameFiles,
    width: size.width,
    height: size.height,
    frameRate: frameRate.num / frameRate.den,
    duration,
    hasAudio: false,
    fit: "contain",
    thumbnail,
    ...(report ? { report } : {}),
  };
  return {
    runtime,
    name,
    description: {
      id,
      kind: "video",
      name,
      width: size.width,
      height: size.height,
      duration,
      provider,
    },
    // A sequence records the frames' shape, rate and duration, not one file's hash or length.
    fingerprint: {
      streams: {
        video: {
          width: size.width,
          height: size.height,
          frame_rate: frameRate,
        },
      },
      duration_ticks: Math.round(duration * TICKS_PER_SECOND),
    },
    dispose() {},
  };
}

/** A PNG frame's thumbnail, {@link THUMBNAIL_WIDTH} pixels wide. */
async function pngThumbnail(file, size) {
  const bitmap = await createImageBitmap(file);
  const canvas = new OffscreenCanvas(
    THUMBNAIL_WIDTH,
    Math.max(1, Math.round((THUMBNAIL_WIDTH * size.height) / size.width)),
  );
  canvas.getContext("2d").drawImage(bitmap, 0, 0, canvas.width, canvas.height);
  bitmap.close();
  const blob = await canvas.convertToBlob({ type: "image/png" });
  const dataUrl = await new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result);
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(blob);
  });
  return { dataUrl, width: canvas.width, height: canvas.height };
}

/**
 * An OpenEXR frame's thumbnail, sampled from the frame provider module `extensions` renders from
 * `bytes` at its own size, with the provider's report.
 */
export async function exrFirstFrame(extensions, bytes) {
  const source = extensions.open({ format: "exr" }, bytes);
  try {
    return {
      thumbnail: await thumbnailOf(
        source.render(0n, { width: 160, height: 90 }),
      ),
      report: source.report,
    };
  } finally {
    source.release();
  }
}

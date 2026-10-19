/** Generic texture metadata shared with image and video host inputs. */
export interface HostTextureDescriptor {
  /** Positive version identifying these pixels; a new version means new content. */
  readonly contentVersion: bigint;
  readonly width: number;
  readonly height: number;
  readonly premultiplication: "premultiplied" | "straight" | "opaque";
  readonly colorSpace: "srgb" | "linear-srgb";
}

/** Layout facts retained beside a generic host texture. */
export interface BrowserTextTexture {
  readonly texture: HostTextureDescriptor;
  /**
   * Where the layout box's left edge lies in the texture: the margin, the same on both sides, that
   * holds ink and decorations past the box.
   */
  readonly layoutX: number;
  /** Where the layout box's top edge lies in the texture, with the same margin below it. */
  readonly layoutY: number;
  readonly layoutWidth: number;
  readonly layoutHeight: number;
  /** Distance from the layout box's top to the first line's baseline. */
  readonly firstBaseline: number;
  readonly ascent: number;
  readonly descent: number;
  readonly lineCount: number;
  /** Tightly packed, premultiplied RGBA8 rows for renderer upload. */
  readonly rgba8: Uint8ClampedArray;
}

/** Browser-owned text layout and raster style. */
export interface BrowserTextRequest {
  /** The text to draw, at most {@link MAX_TEXT_BYTES} as UTF-8; `\n` starts a new paragraph. */
  readonly text: string;
  /** Positive cache key: a request with a cached version returns the cached texture unchanged. */
  readonly contentVersion: bigint;
  /**
   * Font families in fallback order, at least one. CSS generic family names are passed as keywords,
   * every other name as a quoted family name.
   */
  readonly fontFamilies: readonly string[];
  /** Font size in pixels, above zero and at most {@link MAX_TEXT_FONT_SIZE}. */
  readonly fontSize: number;
  /** Wrap width in pixels, at most {@link MAX_TEXT_DIMENSION}; zero or absent disables wrapping. */
  readonly maxWidth?: number;
  /**
   * Baseline-to-baseline distance in font-size multiples (default 1.2), above zero and at most
   * {@link MAX_TEXT_LINE_HEIGHT}.
   */
  readonly lineHeight?: number;
  /**
   * Where lines sit in the layout box, as canvas `textAlign` names it; text runs left to right, so
   * `start` is `left`.
   */
  readonly align?: CanvasTextAlign;
  /** A CSS color (default opaque white). */
  readonly color?: string;
  /**
   * How emoji are drawn (default `native-color`): in the platform's color font, as text
   * presentation where there is one (`monochrome`), or as one U+FFFD per grapheme shown as an
   * emoji (`replace`), as the portable host replaces them. Symbols shown as text by default, such
   * as (c), stay.
   */
  readonly emojiPolicy?: "native-color" | "monochrome" | "replace";
}

/**
 * The CSS generic families: keywords, which a quoted name would turn into a family of that literal
 * name.
 */
const GENERIC_FAMILIES = new Set([
  "serif",
  "sans-serif",
  "monospace",
  "cursive",
  "fantasy",
  "system-ui",
  "ui-serif",
  "ui-sans-serif",
  "ui-monospace",
  "ui-rounded",
  "math",
  "emoji",
  "fangsong",
]);

/**
 * `families` as a CSS font-family list: generic families as keywords, every other name a quoted CSS
 * string.
 */
function cssFontFamilies(families: readonly string[]): string {
  return families
    .map((name) => {
      const keyword = name.toLowerCase();
      if (GENERIC_FAMILIES.has(keyword)) return keyword;
      return `"${name.replace(/[\\"]/g, "\\$&").replace(/\n/g, "\\a ")}"`;
    })
    .join(", ");
}

const TEXT_ALIGNS = new Set(["start", "end", "left", "right", "center"]);

/** The `WasmTextRasterizer` class that `alpha2/wasm` exports. */
export interface WasmTextRasterizerHandle {
  addFont(family: string, bytes: Uint8Array, faceIndex: number): void;
  rasterize(
    requestJson: string,
    contentVersion: bigint,
  ): {
    readonly descriptor: string;
    takePixels(): Uint8Array;
    free?(): void;
  };
  invalidate(contentVersion: bigint): void;
  clear(): void;
  readonly cacheSize: number;
  free?(): void;
}

/** The part of the `alpha2/wasm` module `WasmTextRasterizer` needs. */
export interface WasmTextBinding {
  readonly WasmTextRasterizer: new () => WasmTextRasterizerHandle;
}

/** A text layout and raster style for {@link WasmTextRasterizer}. */
export interface WasmTextRequest {
  readonly text: string;
  readonly contentVersion: bigint;
  /** Logical font size in pixels, up to 2048. */
  readonly fontSize: number;
  /** Layout width; zero or absent disables wrapping. */
  readonly maxWidth?: number;
  /** Baseline-to-baseline distance in font-size multiples (default 1.2). */
  readonly lineHeight?: number;
  readonly align?: "start" | "center" | "end";
  /** Straight (not premultiplied), sRGB-encoded RGBA, each in `0..=255`. */
  readonly color: readonly [number, number, number, number];
  /**
   * What becomes of color-only emoji (default `monochrome`); `replace` draws each emoji grapheme as
   * one U+FFFD.
   */
  readonly emojiPolicy?: "monochrome" | "replace";
}

/**
 * The engine's portable text host (`alpha2-text`: Unicode bidirectional
 * layout and line breaking, rustybuzz shaping, `ab_glyph_rasterizer`
 * outlines) running in wasm.
 *
 * It is the same code a native host rasterizes with, so with the same font bytes a browser preview
 * and a native export draw identical text; the platform's own rasterizer, {@link
 * OffscreenTextRasterizer}, does not. Fonts are caller-supplied bytes in fallback order; system
 * fonts are not reachable. Color emoji are drawn as outlines or replaced (`emojiPolicy`).
 */
export class WasmTextRasterizer {
  readonly #handle: WasmTextRasterizerHandle;

  /** Creates a rasterizer with no fonts from an initialized `alpha2/wasm`. */
  constructor(binding: WasmTextBinding) {
    this.#handle = new binding.WasmTextRasterizer();
  }

  /**
   * Appends an OpenType font to the fallback stack. Every cached result is dropped, since fonts
   * change what a request draws.
   */
  addFont(
    family: string,
    bytes: ArrayBuffer | Uint8Array,
    faceIndex = 0,
  ): void {
    const view = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
    this.#handle.addFont(family, view, faceIndex);
  }

  /**
   * Shapes and rasterizes `request`, cached by content version: the rasterizer keeps up to 64 MiB
   * of results, dropping the least recently used past it.
   *
   * @throws RangeError when `contentVersion` is not positive; the engine throws for an invalid
   *   style, such as a font size over {@link MAX_TEXT_FONT_SIZE}.
   */
  rasterize(request: WasmTextRequest): BrowserTextTexture {
    const { contentVersion, ...style } = request;
    if (contentVersion <= 0n) {
      throw new RangeError("text content version must be positive");
    }
    const raster = this.#handle.rasterize(
      JSON.stringify(style),
      contentVersion,
    );
    let descriptor: WasmTextDescriptor;
    let pixels: Uint8Array;
    try {
      descriptor = JSON.parse(raster.descriptor) as WasmTextDescriptor;
      pixels = raster.takePixels();
    } finally {
      raster.free?.();
    }
    return {
      texture: {
        contentVersion,
        width: descriptor.width,
        height: descriptor.height,
        premultiplication: "premultiplied",
        colorSpace: "srgb",
      },
      layoutX: descriptor.layoutX,
      layoutY: descriptor.layoutY,
      layoutWidth: descriptor.layoutWidth,
      layoutHeight: descriptor.layoutHeight,
      firstBaseline: descriptor.firstBaseline,
      ascent: descriptor.ascent,
      descent: descriptor.descent,
      lineCount: descriptor.lineCount,
      rgba8: new Uint8ClampedArray(
        pixels.buffer,
        pixels.byteOffset,
        pixels.byteLength,
      ),
    };
  }

  /** Drops the cached texture for `contentVersion`, if any. */
  invalidate(contentVersion: bigint): void {
    this.#handle.invalidate(contentVersion);
  }

  /** Drops every cached texture; fonts stay loaded. */
  clear(): void {
    this.#handle.clear();
  }

  /** Releases the wasm-side rasterizer and its fonts. */
  close(): void {
    this.#handle.free?.();
  }

  /** Number of cached textures. */
  get cacheSize(): number {
    return this.#handle.cacheSize;
  }
}

interface WasmTextDescriptor {
  readonly width: number;
  readonly height: number;
  readonly layoutX: number;
  readonly layoutY: number;
  readonly layoutWidth: number;
  readonly layoutHeight: number;
  readonly firstBaseline: number;
  readonly ascent: number;
  readonly descent: number;
  readonly lineCount: number;
}

/**
 * OffscreenCanvas/FontFace text host with explicit version invalidation.
 *
 * It draws with the browser's own shaper and rasterizer, which reach system
 * fonts and color emoji; its glyphs differ from a native host's. Use
 * {@link WasmTextRasterizer} where preview and export must match.
 */
export class OffscreenTextRasterizer {
  readonly #cache = new Map<bigint, BrowserTextTexture>();
  readonly #fonts = new Set<FontFace>();

  /**
   * Loads a caller-supplied font and adds it to the global's `FontFaceSet` (the worker's `fonts`
   * or `document.fonts`) until {@link close}. No stylesheet is touched. Rejects when the font data
   * cannot be loaded.
   *
   * Cached textures are not invalidated; bump `contentVersion` or call {@link clear} for text that
   * should pick up the new font.
   */
  async loadFont(
    family: string,
    source: ArrayBuffer,
    descriptors: FontFaceDescriptors = {},
  ): Promise<FontFace> {
    const face = await new FontFace(family, source, descriptors).load();
    // WorkerGlobalScope exposes fonts directly; Window uses document.fonts.
    const owner = globalThis as typeof globalThis & {
      fonts?: FontFaceSet;
      document?: Document;
    };
    const set = owner.fonts ?? owner.document?.fonts;
    set?.add(face);
    this.#fonts.add(face);
    return face;
  }

  /**
   * Shapes and rasterizes through the browser platform, cached by content version.
   *
   * @throws RangeError when the request is invalid (see {@link BrowserTextRequest}) or the texture
   *   would exceed {@link MAX_TEXT_DIMENSION}.
   * @throws Error when no OffscreenCanvas 2D context is available.
   */
  rasterize(request: BrowserTextRequest): BrowserTextTexture {
    validateRequest(request);
    const cached = this.#cache.get(request.contentVersion);
    if (cached !== undefined) return cached;

    const fontSize = request.fontSize;
    const lineAdvance = fontSize * (request.lineHeight ?? 1.2);
    const family = cssFontFamilies(request.fontFamilies);
    const probe = new OffscreenCanvas(1, 1).getContext("2d", {
      willReadFrequently: true,
    });
    if (probe === null)
      throw new Error("OffscreenCanvas 2D context is unavailable");
    probe.font = `${fontSize}px ${family}`;
    probe.textBaseline = "alphabetic";
    const normalizedText = normalizeEmoji(
      request.text,
      request.emojiPolicy ?? "native-color",
    );
    const lines = wrapLines(probe, normalizedText, request.maxWidth ?? 0);
    probe.textAlign = request.align ?? "start";
    const measured = lines.map((line) => probe.measureText(line));
    const textWidth = measured.reduce(
      (maximum, metrics) => Math.max(maximum, metrics.width),
      0,
    );
    const layoutWidth = Math.max(
      1,
      request.maxWidth && request.maxWidth > 0 ? request.maxWidth : textWidth,
    );
    // The box holds every line's nominal em at fixed baselines; ink past it goes in an equal margin
    // on both sides so the box stays centered.
    const nominalAscent = fontSize * 0.8;
    const layoutHeight = Math.max(
      1,
      (lines.length - 1) * lineAdvance + Math.max(lineAdvance, fontSize),
    );
    const boxWidth = checkedDimension(layoutWidth);
    const boxHeight = checkedDimension(layoutHeight);
    const x =
      request.align === "center"
        ? layoutWidth / 2
        : request.align === "end" || request.align === "right"
          ? layoutWidth
          : 0;
    let overflowX = 0;
    let overflowY = 0;
    let ascent = nominalAscent;
    let descent = fontSize * 0.2;
    measured.forEach((metrics, index) => {
      const baseline = nominalAscent + index * lineAdvance;
      overflowX = Math.max(
        overflowX,
        metrics.actualBoundingBoxLeft - x,
        x + metrics.actualBoundingBoxRight - boxWidth,
      );
      overflowY = Math.max(
        overflowY,
        metrics.actualBoundingBoxAscent - baseline,
        baseline + metrics.actualBoundingBoxDescent - boxHeight,
      );
      ascent = Math.max(ascent, metrics.actualBoundingBoxAscent);
      descent = Math.max(descent, metrics.actualBoundingBoxDescent);
    });
    const marginX = Math.ceil(overflowX);
    const marginY = Math.ceil(overflowY);
    const width = checkedDimension(boxWidth + 2 * marginX);
    const height = checkedDimension(boxHeight + 2 * marginY);
    const canvas = new OffscreenCanvas(width, height);
    const context = canvas.getContext("2d", { willReadFrequently: true });
    if (context === null)
      throw new Error("OffscreenCanvas 2D context is unavailable");
    context.font = `${fontSize}px ${family}`;
    context.textBaseline = "alphabetic";
    context.textAlign = request.align ?? "start";
    context.fillStyle = request.color ?? "#ffffffff";
    lines.forEach((line, index) =>
      context.fillText(
        line,
        marginX + x,
        marginY + nominalAscent + index * lineAdvance,
      ),
    );
    const image = context.getImageData(0, 0, width, height);
    premultiplyInPlace(image.data);
    const result: BrowserTextTexture = {
      texture: {
        contentVersion: request.contentVersion,
        width,
        height,
        premultiplication: "premultiplied",
        colorSpace: "srgb",
      },
      layoutX: marginX,
      layoutY: marginY,
      layoutWidth,
      layoutHeight,
      firstBaseline: nominalAscent,
      ascent,
      descent,
      lineCount: lines.length,
      rgba8: image.data,
    };
    this.#cache.set(request.contentVersion, result);
    return result;
  }

  /** Drops the cached texture for `contentVersion`, if any. */
  invalidate(contentVersion: bigint): void {
    this.#cache.delete(contentVersion);
  }

  /** Drops every cached texture; loaded fonts stay registered. */
  clear(): void {
    this.#cache.clear();
  }

  /** Removes the fonts this rasterizer loaded from the global `FontFaceSet` and drops the cache. */
  close(): void {
    const owner = globalThis as typeof globalThis & {
      fonts?: FontFaceSet;
      document?: Document;
    };
    const set = owner.fonts ?? owner.document?.fonts;
    for (const face of this.#fonts) set?.delete(face);
    this.#fonts.clear();
    this.#cache.clear();
  }

  /** Number of cached textures. */
  get cacheSize(): number {
    return this.#cache.size;
  }
}

/** The largest font size, in pixels, either text host draws. */
export const MAX_TEXT_FONT_SIZE = 2_048;
/** The widest and tallest text texture, in pixels, and so the widest wrap width. */
export const MAX_TEXT_DIMENSION = 16_384;
/** The longest text either text host draws, in UTF-8 bytes. */
export const MAX_TEXT_BYTES = 64 * 1024;
/** The largest line height either text host draws, in font-size multiples. */
export const MAX_TEXT_LINE_HEIGHT = 10;

/**
 * `text`'s UTF-8 length is at most {@link MAX_TEXT_BYTES}; never shorter than its UTF-16 length.
 */
function textWithinLimit(text: string): boolean {
  return (
    text.length <= MAX_TEXT_BYTES &&
    new TextEncoder().encode(text).length <= MAX_TEXT_BYTES
  );
}

/** Refuses what the portable host (`alpha2-text`) refuses, before any text is measured. */
function validateRequest(request: BrowserTextRequest): void {
  if (
    request.contentVersion <= 0n ||
    typeof request.text !== "string" ||
    !textWithinLimit(request.text) ||
    request.fontFamilies.length === 0 ||
    !Number.isFinite(request.fontSize) ||
    request.fontSize <= 0 ||
    request.fontSize > MAX_TEXT_FONT_SIZE ||
    (request.maxWidth !== undefined &&
      (!Number.isFinite(request.maxWidth) ||
        request.maxWidth > MAX_TEXT_DIMENSION)) ||
    (request.lineHeight !== undefined &&
      (!Number.isFinite(request.lineHeight) ||
        request.lineHeight <= 0 ||
        request.lineHeight > MAX_TEXT_LINE_HEIGHT)) ||
    (request.align !== undefined && !TEXT_ALIGNS.has(request.align))
  ) {
    throw new RangeError("invalid browser text raster request");
  }
}

/**
 * `text` in lines no wider than `maxWidth` (none when it is not positive): a line breaks before the
 * word that would overflow it, and a word wider than the whole width breaks between graphemes, as
 * the portable host breaks it between glyphs; spaces hang past the end.
 *
 * Each word and each grapheme of an overflowing word is measured once and a line's width is their
 * sum, as the portable host sums cluster advances, so the cost is linear in the text. The ink of
 * each drawn line is measured whole afterwards, so a kerning difference is never cut off.
 */
function wrapLines(
  context: OffscreenCanvasRenderingContext2D,
  text: string,
  maxWidth: number,
): string[] {
  const output: string[] = [];
  const width = (piece: string) =>
    piece === "" ? 0 : context.measureText(piece).width;
  for (const paragraph of text.split("\n")) {
    if (!(maxWidth > 0)) {
      output.push(paragraph);
      continue;
    }
    // The line so far, its width, and its width without the spaces it ends in.
    let line = "";
    let lineWidth = 0;
    let inkWidth = 0;
    for (const token of paragraph.match(/\S+\s*|\s+/gu) ?? [""]) {
      const word = token.trimEnd();
      const wordWidth = width(word);
      const tokenWidth = word === token ? wordWidth : width(token);
      // Only a paragraph's first token can be all spaces, and then the line is still empty.
      if (line !== "" && lineWidth + wordWidth > maxWidth) {
        output.push(line.trimEnd());
        line = token;
        lineWidth = tokenWidth;
        inkWidth = wordWidth;
      } else {
        if (word !== "") inkWidth = lineWidth + wordWidth;
        line += token;
        lineWidth += tokenWidth;
      }
      if (inkWidth > maxWidth) {
        let piece = "";
        let pieceWidth = 0;
        let pieceInk = 0;
        for (const grapheme of graphemes(line)) {
          const graphemeWidth = width(grapheme);
          const ink = /\S/u.test(grapheme);
          if (piece !== "" && ink && pieceWidth + graphemeWidth > maxWidth) {
            output.push(piece.trimEnd());
            piece = "";
            pieceWidth = 0;
            pieceInk = 0;
          }
          piece += grapheme;
          pieceWidth += graphemeWidth;
          if (ink) pieceInk = pieceWidth;
        }
        line = piece;
        lineWidth = pieceWidth;
        inkWidth = pieceInk;
      }
    }
    output.push(line.trimEnd());
  }
  return output.length === 0 ? [""] : output;
}

/** `text`'s user-perceived characters, or its code points where `Intl.Segmenter` is missing. */
function graphemes(text: string): string[] {
  const Segmenter = (
    Intl as typeof Intl & {
      Segmenter?: new (
        locale?: string,
        options?: { granularity: "grapheme" },
      ) => { segment(input: string): Iterable<{ segment: string }> };
    }
  ).Segmenter;
  if (Segmenter === undefined) return Array.from(text);
  return Array.from(
    new Segmenter(undefined, { granularity: "grapheme" }).segment(text),
    (part) => part.segment,
  );
}

function checkedDimension(value: number): number {
  const result = Math.ceil(value);
  if (
    !Number.isSafeInteger(result) ||
    result <= 0 ||
    result > MAX_TEXT_DIMENSION
  ) {
    throw new RangeError("text texture dimension is unsupported");
  }
  return result;
}

function premultiplyInPlace(pixels: Uint8ClampedArray): void {
  for (let index = 0; index < pixels.length; index += 4) {
    const alpha = pixels[index + 3] ?? 0;
    pixels[index] = Math.round(((pixels[index] ?? 0) * alpha) / 255);
    pixels[index + 1] = Math.round(((pixels[index + 1] ?? 0) * alpha) / 255);
    pixels[index + 2] = Math.round(((pixels[index + 2] ?? 0) * alpha) / 255);
  }
}

/**
 * Whether a grapheme is shown as an emoji (UTS #51), as the portable host (`alpha2-text`) reads one
 * too: unless U+FE0E asks for text, one holding an `Emoji_Presentation` character (regional
 * indicators and skin tones included), or U+FE0F with a pictograph or a keycap. A symbol shown as
 * text by default, such as (c) or a sun, is not one: a text font draws it.
 */
function isEmoji(grapheme: string): boolean {
  if (grapheme.includes("\ufe0e")) return false;
  return (
    /\p{Emoji_Presentation}/u.test(grapheme) ||
    (grapheme.includes("\ufe0f") &&
      /\p{Extended_Pictographic}|\u20e3/u.test(grapheme))
  );
}

/** A lone pictograph, with or without a presentation selector: what U+FE0E can make text. */
const LONE_PICTOGRAPH = /^(\p{Extended_Pictographic})\ufe0f?$/u;

function normalizeEmoji(
  text: string,
  policy: NonNullable<BrowserTextRequest["emojiPolicy"]>,
): string {
  if (policy === "native-color") return text;
  let normalized = "";
  for (const segment of graphemes(text)) {
    if (policy === "replace") {
      // Each emoji grapheme, sequence or flag, becomes one replacement character.
      normalized += isEmoji(segment) ? "\ufffd" : segment;
      continue;
    }
    // Text presentation exists only for a lone pictograph; a skin tone, a ZWJ sequence, a flag, or
    // a keycap has none, and a selector inside one would split it.
    const lone = LONE_PICTOGRAPH.exec(segment);
    normalized += lone ? `${lone[1]}\ufe0e` : segment;
  }
  return normalized;
}

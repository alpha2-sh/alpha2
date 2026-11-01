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
  /** The layout box's left edge in the texture; equal side margins hold overhanging ink. */
  readonly layoutX: number;
  /** The layout box's top edge in the texture, with the same margin below it. */
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
  /** The background boxes alone when `splitBackground` was set; `rgba8` then excludes them. */
  readonly background?: Uint8ClampedArray;
}

/** Browser-owned text layout and raster style. */
export interface BrowserTextRequest {
  /** The text to draw, at most {@link MAX_TEXT_BYTES} as UTF-8; `\n` starts a new paragraph. */
  readonly text: string;
  /** Positive cache key; a cached version returns the cached texture unchanged. */
  readonly contentVersion: bigint;
  /** Font families in fallback order, at least one. CSS generic family names stay keywords. */
  readonly fontFamilies: readonly string[];
  /** Font size in pixels, above zero and at most {@link MAX_TEXT_FONT_SIZE}. */
  readonly fontSize: number;
  /** Wrap width in pixels, at most {@link MAX_TEXT_DIMENSION}; zero or absent disables wrapping. */
  readonly maxWidth?: number;
  /** Line height in font-size multiples (default 1.2), up to {@link MAX_TEXT_LINE_HEIGHT}. */
  readonly lineHeight?: number;
  /** Canvas `textAlign`; text runs left to right, so `start` is `left`. */
  readonly align?: CanvasTextAlign;
  /** A CSS color (default opaque white). */
  readonly color?: string;
  /**
   * Emoji drawing (default `color`). `monochrome` uses text presentation where one exists;
   * `replace` draws one U+FFFD per emoji grapheme, as the portable host does.
   */
  readonly emojiPolicy?: "color" | "monochrome" | "replace";
}

// Quoting these would turn them into literal family names.
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

export interface WasmTextRasterizerHandle {
  addFont(family: string, bytes: Uint8Array, faceIndex: number): void;
  addFontFile(family: string, bytes: Uint8Array): void;
  rasterize(
    requestJson: string,
    contentVersion: bigint,
  ): {
    readonly descriptor: string;
    takeBackground?(): Uint8Array;
    takePixels(): Uint8Array;
    free?(): void;
  };
  invalidate(contentVersion: bigint): void;
  clear(): void;
  readonly cacheSize: number;
  free?(): void;
}

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
  /** Line height in font-size multiples (default 1.2). */
  readonly lineHeight?: number;
  /**
   * CSS-style letter spacing in pixels (default 0), within -0.5 to 1 font sizes. None is added
   * after a line's last character or within cursive scripts.
   */
  readonly letterSpacing?: number;
  /**
   * Weight for face matching, 1 to 1000 (default 400), as CSS `font-weight`. Variable faces follow
   * their `wght` axis; no face is synthetically emboldened.
   */
  readonly fontWeight?: number;
  readonly align?: "start" | "center" | "end";
  /** A solid color or a gradient over the layout box; also fills color glyphs' text parts. */
  readonly fill: WasmTextColor | WasmTextGradient;
  /**
   * Emoji drawing (default `color`). `color` uses color glyphs (COLR, CBDT, sbix), `monochrome`
   * draws outlines in the fill, `replace` draws one U+FFFD per emoji grapheme.
   */
  readonly emojiPolicy?: "color" | "monochrome" | "replace";
  /** Outline bands, innermost first: at most 4, with widths summing to at most 64 pixels. */
  readonly strokes?: readonly {
    readonly color: WasmTextColor;
    readonly width: number;
  }[];
  /** A box behind each line, grown by `padding` pixels (at most 256). */
  readonly background?: {
    readonly color: WasmTextColor;
    readonly padding: number;
  };
  /**
   * Returns the boxes as their own plane ({@link BrowserTextTexture.background}) so the page can
   * draw a shadow between them and the glyphs.
   */
  readonly splitBackground?: boolean;
  /** One animation state applied to every cluster. Tracking never re-wraps lines. */
  readonly clusterState?: WasmClusterState;
  /** One animation state per cluster, in logical order. */
  readonly clusterStates?: readonly WasmClusterState[];
}

/**
 * Per-cluster animation; omitted fields are identity. Offset and tracking are pixels, rotation is
 * clockwise radians, opacity is `0..=1`. A color mix computes `color * keep + add` in premultiplied
 * linear light.
 */
export interface WasmClusterState {
  readonly offset?: readonly [number, number];
  readonly scale?: readonly [number, number];
  readonly rotation?: number;
  readonly opacity?: number;
  readonly fill?: WasmColorMix;
  readonly stroke?: WasmColorMix;
  readonly tracking?: number;
}

/** See {@link WasmClusterState}. */
export interface WasmColorMix {
  readonly keep: number;
  readonly add: readonly [number, number, number, number];
}

/** Straight (not premultiplied), sRGB-encoded RGBA, each in `0..=255`. */
export type WasmTextColor = readonly [number, number, number, number];

/**
 * A gradient over the layout box. Points are box fractions (`[0, 0]` top-left), the radius is a
 * fraction of the shorter side, the conic angle is radians clockwise from up. 2 to 16 stops with
 * straight sRGB colors in `0..=1`, interpolated in linear light.
 */
export type WasmTextGradient = (
  | {
      readonly type: "linear";
      readonly start: readonly [number, number];
      readonly end: readonly [number, number];
      readonly spread?: "pad" | "repeat" | "reflect";
    }
  | {
      readonly type: "radial";
      readonly center: readonly [number, number];
      readonly radius: number;
      readonly spread?: "pad" | "repeat" | "reflect";
    }
  | {
      readonly type: "conic";
      readonly center: readonly [number, number];
      readonly angle?: number;
    }
) & {
  readonly stops: readonly {
    readonly offset: number;
    readonly color: readonly [number, number, number, number];
  }[];
};

/**
 * The engine's portable text host (`alpha2-text`) in wasm. Given the same font bytes it draws text
 * identical to a native export, unlike {@link OffscreenTextRasterizer}. Fonts are caller-supplied
 * bytes in fallback order; system fonts are not reachable.
 */
export class WasmTextRasterizer {
  readonly #handle: WasmTextRasterizerHandle;

  constructor(binding: WasmTextBinding) {
    this.#handle = new binding.WasmTextRasterizer();
  }

  /** Appends an OpenType font to the fallback stack and drops every cached result. */
  addFont(
    family: string,
    bytes: ArrayBuffer | Uint8Array,
    faceIndex = 0,
  ): void {
    const view = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
    this.#handle.addFont(family, view, faceIndex);
  }

  /**
   * Appends every face of a font file (each weight of a `.ttc`) to the fallback stack, as the
   * export host does, and drops every cached result.
   */
  addFontFile(family: string, bytes: ArrayBuffer | Uint8Array): void {
    const view = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
    this.#handle.addFontFile(family, view);
  }

  /**
   * Shapes and rasterizes `request`, cached by content version (LRU, up to 64 MiB).
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
    let background: Uint8Array | undefined;
    try {
      descriptor = JSON.parse(raster.descriptor) as WasmTextDescriptor;
      background = raster.takeBackground?.();
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
      ...(background !== undefined && background.byteLength > 0
        ? {
            background: new Uint8ClampedArray(
              background.buffer,
              background.byteOffset,
              background.byteLength,
            ),
          }
        : {}),
    };
  }

  invalidate(contentVersion: bigint): void {
    this.#handle.invalidate(contentVersion);
  }

  /** Drops every cached texture; fonts stay loaded. */
  clear(): void {
    this.#handle.clear();
  }

  close(): void {
    this.#handle.free?.();
  }

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
 * OffscreenCanvas/FontFace text host. It uses the browser's shaper and system fonts, so its glyphs
 * differ from a native host's; use {@link WasmTextRasterizer} where preview and export must match.
 */
export class OffscreenTextRasterizer {
  readonly #cache = new Map<bigint, BrowserTextTexture>();
  readonly #fonts = new Set<FontFace>();

  /**
   * Loads a font into the global `FontFaceSet` until {@link close}. Rejects when the font data
   * cannot be loaded. Cached textures are kept; bump `contentVersion` or call {@link clear}.
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
      request.emojiPolicy ?? "color",
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
    // The box holds each line's nominal em, centered in its line; overhanging ink goes in an equal
    // margin on both sides so the box stays centered.
    const halfLeading = Math.max(0, lineAdvance - fontSize) / 2;
    const nominalAscent = fontSize * 0.8;
    const firstBaseline = halfLeading + nominalAscent;
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
      const baseline = firstBaseline + index * lineAdvance;
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
        marginY + firstBaseline + index * lineAdvance,
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
      firstBaseline,
      ascent,
      descent,
      lineCount: lines.length,
      rgba8: image.data,
    };
    this.#cache.set(request.contentVersion, result);
    return result;
  }

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

  get cacheSize(): number {
    return this.#cache.size;
  }
}

/** The largest font size, in pixels, either text host draws. */
export const MAX_TEXT_FONT_SIZE = 2_048;
/** The largest text texture side and wrap width, in pixels. */
export const MAX_TEXT_DIMENSION = 16_384;
/** The longest text either text host draws, in UTF-8 bytes. */
export const MAX_TEXT_BYTES = 64 * 1024;
/** The largest line height either text host draws, in font-size multiples. */
export const MAX_TEXT_LINE_HEIGHT = 10;

// UTF-8 is never shorter than UTF-16 code units, so the cheap check runs first.
function textWithinLimit(text: string): boolean {
  return (
    text.length <= MAX_TEXT_BYTES &&
    new TextEncoder().encode(text).length <= MAX_TEXT_BYTES
  );
}

// Refuses what the portable host refuses, before any text is measured.
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
 * Breaks before an overflowing word, and inside a word wider than the line between graphemes, as
 * the portable host does; trailing spaces hang. Line widths sum per-word measurements, as the
 * portable host sums cluster advances, which keeps the cost linear.
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
    // `inkWidth` excludes trailing spaces.
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

// Falls back to code points where `Intl.Segmenter` is missing.
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

// UTS #51 emoji presentation, matching the portable host. Text-default symbols such as (c) are not
// emoji.
function isEmoji(grapheme: string): boolean {
  if (grapheme.includes("\ufe0e")) return false;
  return (
    /\p{Emoji_Presentation}/u.test(grapheme) ||
    (grapheme.includes("\ufe0f") &&
      /\p{Extended_Pictographic}|\u20e3/u.test(grapheme))
  );
}

// Only a lone pictograph can take text presentation (U+FE0E).
const LONE_PICTOGRAPH = /^(\p{Extended_Pictographic})\ufe0f?$/u;

function normalizeEmoji(
  text: string,
  policy: NonNullable<BrowserTextRequest["emojiPolicy"]>,
): string {
  if (policy === "color") return text;
  let normalized = "";
  for (const segment of graphemes(text)) {
    if (policy === "replace") {
      normalized += isEmoji(segment) ? "\ufffd" : segment;
      continue;
    }
    // A selector inside a sequence (skin tone, ZWJ, flag, keycap) would split it.
    const lone = LONE_PICTOGRAPH.exec(segment);
    normalized += lone ? `${lone[1]}\ufe0e` : segment;
  }
  return normalized;
}

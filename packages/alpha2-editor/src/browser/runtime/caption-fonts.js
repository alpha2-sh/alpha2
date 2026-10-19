// Fonts caption tracks draw with, in fallback order. The engine rasterizes captions from these
// files, so preview and every export draw the same pixels; fonts are not part of the document.

export class CaptionFonts {
  constructor(defaultFontUrl) {
    this.defaultFontUrl = defaultFontUrl;
  }
  /** `{ id, name, bytes, faceIndex, builtIn }`, primary first. */
  #fonts = [];
  #nextId = 1;

  get list() {
    return this.#fonts.map(({ id, name, builtIn }) => ({ id, name, builtIn }));
  }

  /** The fonts in fallback order, for an engine to register. */
  get stack() {
    return this.#fonts;
  }

  async loadDefault() {
    if (!this.defaultFontUrl) return;
    const response = await fetch(this.defaultFontUrl);
    if (!response.ok)
      throw new Error(
        `the default caption font is missing (${response.status})`,
      );
    this.#fonts.push({
      id: "font-default",
      name: "DejaVu Sans",
      bytes: new Uint8Array(await response.arrayBuffer()),
      faceIndex: 0,
      builtIn: true,
    });
  }

  /**
   * Adds a font file ahead of the others, so it draws every glyph it has and the fonts after it
   * fill in the rest (a Korean font before DejaVu, say).
   */
  async add(file) {
    const bytes = new Uint8Array(await file.arrayBuffer());
    const font = {
      id: `font-${this.#nextId++}`,
      name: file.name.replace(/\.(ttf|otf|ttc|otc)$/i, ""),
      bytes,
      faceIndex: 0,
      builtIn: false,
    };
    this.#fonts.unshift(font);
    return font;
  }

  remove(id) {
    this.#fonts = this.#fonts.filter((font) => font.builtIn || font.id !== id);
  }
}

/**
 * Replaces an engine registry's caption fonts with `fonts`, in order. A font the engine cannot
 * parse costs only itself: it is left out and every other font is still registered, the built-in
 * fallbacks among them. Returns what was left out, as `{ font, error }`, for the host to report.
 */
export function registerCaptionFonts(registry, fonts) {
  registry.clearFonts();
  const refused = [];
  for (const font of fonts) {
    try {
      // wasm-bindgen takes the bytes by copy.
      registry.addFont(font.name, font.bytes, font.faceIndex);
    } catch (error) {
      refused.push({ font, error });
    }
  }
  return refused;
}

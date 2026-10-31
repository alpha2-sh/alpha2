// Fonts caption tracks draw with, in fallback order. The engine rasterizes captions from these
// files, so preview and every export draw the same pixels. Fonts are not part of the document.

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

  /** Adds a font ahead of the others, so the fonts after it fill in glyphs it lacks. */
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
 * parse is left out and the rest, built-in fallbacks included, still register. Returns what was
 * left out as `{ font, error }`.
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

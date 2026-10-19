// Files that take an extension provider a while to read, used by `test:editor:package` to check
// that the editor reads them off the page's thread.

/**
 * A GIF89a of `frames` frames, `width`x`height`, every pixel of every frame drawn, each shown for
 * 40 ms, LZW-compressed as an encoder writes it.
 * @returns {Uint8Array}
 */
export function animatedGif({ width, height, frames }) {
  const out = [];
  const u16 = (value) => out.push(value & 0xff, value >> 8);
  out.push(..."GIF89a".split("").map((char) => char.charCodeAt(0)));
  u16(width);
  u16(height);
  // A global table of 256 colors.
  out.push(0xf7, 0, 0);
  for (let index = 0; index < 256; index += 1)
    out.push(index, (index * 3) & 0xff, 255 - index);
  // Loop forever.
  out.push(
    0x21,
    0xff,
    0x0b,
    ..."NETSCAPE2.0".split("").map((c) => c.charCodeAt(0)),
  );
  out.push(0x03, 0x01, 0, 0, 0);
  const pixels = new Uint8Array(width * height);
  for (let frame = 0; frame < frames; frame += 1) {
    for (let y = 0; y < height; y += 1)
      for (let x = 0; x < width; x += 1)
        pixels[y * width + x] = ((x >> 4) + (y >> 4) * 5 + frame * 7) & 0xff;
    // A graphic control extension: 4 hundredths of a second, no disposal, no transparency.
    out.push(0x21, 0xf9, 0x04, 0x00, 4, 0, 0, 0);
    out.push(0x2c);
    u16(0);
    u16(0);
    u16(width);
    u16(height);
    out.push(0);
    out.push(8);
    const data = lzw(pixels);
    for (let at = 0; at < data.length; at += 255) {
      const block = data.subarray(at, at + 255);
      out.push(block.length);
      for (const byte of block) out.push(byte);
    }
    out.push(0);
  }
  out.push(0x3b);
  return Uint8Array.from(out);
}

/** GIF's variable-width LZW of 8-bit indices. */
function lzw(indices) {
  const clear = 256;
  const end = 257;
  const bytes = [];
  let buffer = 0;
  let bits = 0;
  let width = 9;
  const emit = (code) => {
    buffer |= code << bits;
    bits += width;
    while (bits >= 8) {
      bytes.push(buffer & 0xff);
      buffer >>>= 8;
      bits -= 8;
    }
  };
  let table = new Map();
  let next = 258;
  emit(clear);
  let prefix = indices[0];
  for (let at = 1; at < indices.length; at += 1) {
    const value = indices[at];
    const key = prefix * 256 + value;
    const known = table.get(key);
    if (known !== undefined) {
      prefix = known;
      continue;
    }
    emit(prefix);
    if (next < 4096) {
      table.set(key, next);
      next += 1;
      if (next > 1 << width && width < 12) width += 1;
    } else {
      emit(clear);
      table = new Map();
      next = 258;
      width = 9;
    }
    prefix = value;
  }
  emit(prefix);
  emit(end);
  if (bits > 0) bytes.push(buffer & 0xff);
  return Uint8Array.from(bytes);
}

/**
 * A Lottie animation of `layers` shape layers, each a small square moving across a 1920x1080
 * canvas over 120 frames: a large document, well within what the provider opens.
 * @returns {Uint8Array}
 */
export function largeLottie({ layers }) {
  const value = (k) => ({ a: 0, k });
  const shapeLayers = [];
  for (let index = 0; index < layers; index += 1) {
    const x = (index * 37) % 1920;
    const y = (index * 53) % 1080;
    shapeLayers.push({
      ddd: 0,
      ind: index + 1,
      ty: 4,
      nm: `square ${index}`,
      sr: 1,
      ks: {
        o: value(100),
        r: value(0),
        p: {
          a: 1,
          k: [
            {
              t: 0,
              s: [x, y, 0],
              i: { x: [0.5], y: [0.5] },
              o: { x: [0.5], y: [0.5] },
            },
            { t: 119, s: [(x + 240) % 1920, y, 0] },
          ],
        },
        a: value([0, 0, 0]),
        s: value([100, 100, 100]),
      },
      ao: 0,
      shapes: [
        { ty: "rc", d: 1, s: value([2, 2]), p: value([0, 0]), r: value(0) },
        {
          ty: "fl",
          c: value([(index % 7) / 7, 0.4, 0.8, 1]),
          o: value(100),
          r: 1,
        },
      ],
      ip: 0,
      op: 120,
      st: 0,
      bm: 0,
    });
  }
  const document = {
    v: "5.7.4",
    fr: 30,
    ip: 0,
    op: 120,
    w: 1920,
    h: 1080,
    nm: "squares",
    ddd: 0,
    assets: [],
    layers: shapeLayers,
  };
  return new TextEncoder().encode(JSON.stringify(document));
}

/**
 * Small files whose drawing the provider's budgets refuse at open: an SVG whose morphology filter
 * would take minutes at its size, and a Lottie whose repeated gradient paints its canvas hundreds
 * of times a frame.
 */
export const refusedProviderFiles = {
  "morphology.svg": `<svg xmlns="http://www.w3.org/2000/svg" width="1920" height="1080"><filter id="f"><feMorphology operator="dilate" radius="400"/></filter><rect width="1920" height="1080" fill="red" filter="url(#f)"/></svg>`,
  "painted.json": JSON.stringify(paintedLottie(30, 10)),
};

/** A 1920x1080 Lottie whose full-canvas gradient rectangle is repeated `inner` x `outer` times. */
function paintedLottie(inner, outer) {
  const value = (k) => ({ a: 0, k });
  const repeater = (copies) => ({
    ty: "rp",
    c: value(copies),
    o: value(0),
    m: 1,
    tr: {
      ty: "tr",
      p: value([0, 0]),
      a: value([0, 0]),
      s: value([100, 100]),
      r: value(0),
      so: value(100),
      eo: value(100),
    },
  });
  const transform = {
    ty: "tr",
    p: value([0, 0]),
    a: value([0, 0]),
    s: value([100, 100]),
    r: value(0),
    o: value(100),
  };
  return {
    v: "5.7.4",
    fr: 30,
    ip: 0,
    op: 30,
    w: 1920,
    h: 1080,
    nm: "x",
    ddd: 0,
    assets: [],
    layers: [
      {
        ddd: 0,
        ind: 1,
        ty: 4,
        nm: "l",
        sr: 1,
        ks: {
          o: value(100),
          r: value(0),
          p: value([0, 0, 0]),
          a: value([0, 0, 0]),
          s: value([100, 100, 100]),
        },
        ao: 0,
        shapes: [
          {
            ty: "gr",
            it: [
              {
                ty: "rc",
                d: 1,
                s: value([1920, 1080]),
                p: value([960, 540]),
                r: value(0),
              },
              {
                ty: "gf",
                o: value(100),
                r: 1,
                t: 1,
                s: value([0, 0]),
                e: value([1920, 1080]),
                g: { p: 2, k: value([0, 1, 0, 0, 1, 0, 0, 1]) },
              },
              repeater(inner),
              transform,
            ],
          },
          repeater(outer),
        ],
        ip: 0,
        op: 30,
        st: 0,
        bm: 0,
      },
    ],
  };
}

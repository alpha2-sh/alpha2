//! SVG, Lottie, LaTeX, and PDF rasters held within a code or so of references drawn on linux-x64.
//!
//! The rasterizers are not bit-exact across CPU families and wasm (tiny-skia's fast reciprocal in
//! color-dodge and color-burn, platform math in transforms, arcs, strokes and filters), so each
//! case exercises one of those operations within a tolerance.
//!
//! `ALPHA2_UPDATE_RASTER_PINS=1 cargo test -p alpha2-extension-raster-pins` rewrites the
//! references (linux-x64 only).
#![cfg(feature = "rasters")]

use std::sync::Arc;

use alpha2_extensions::{ExtensionError, Registry, ResourceResolver};
use alpha2_model::ProviderSource;
use alpha2_time::MediaTime;

/// Width and height of every case.
pub const SIDE: u32 = 256;

const FONT: &[u8] = include_bytes!("../../fonts/Alpha2Fixture.ttf");

/// How a case's document is opened.
#[derive(Clone, Copy)]
enum Kind {
    Svg,
    Lottie,
    /// A display-style formula at 64 pixels per em, its document the formula.
    Latex,
    /// The first page of a PDF on white paper.
    Pdf,
}

/// One document and the reference it must stay close to.
pub struct Case {
    /// The reference's file stem in `tests/corpus/extensions/raster-pins`.
    pub name: &'static str,
    /// How many codes a sample may be from the reference.
    pub tolerance: u8,
    kind: Kind,
    document: &'static str,
    reference: &'static [u8],
}

macro_rules! case {
    ($name:literal, $tolerance:literal, $kind:ident, $document:expr) => {
        Case {
            name: $name,
            tolerance: $tolerance,
            kind: Kind::$kind,
            document: $document,
            reference: include_bytes!(concat!(
                "../../corpus/extensions/raster-pins/",
                $name,
                ".png"
            )),
        }
    };
}

/// The cases. Color-dodge and color-burn divide by the distance from white, so those groups
/// allow four codes (3 measured on wasm, `aarch64` macOS and `x86_64` Windows); others allow one.
pub static CASES: [Case; 15] = [
    case!(
        "svg-dodge-burn",
        4,
        Svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256">
  <defs>
    <linearGradient id="l" x1="0" y1="0" x2="1" y2="0.3">
      <stop offset="0" stop-color="#1a3c8f"/><stop offset="0.5" stop-color="#e0b040"/>
      <stop offset="1" stop-color="#f0f0e8"/>
    </linearGradient>
    <radialGradient id="r" cx="0.4" cy="0.6" r="0.6">
      <stop offset="0" stop-color="#ff8040" stop-opacity="0.9"/>
      <stop offset="1" stop-color="#2040a0" stop-opacity="0.4"/>
    </radialGradient>
  </defs>
  <rect width="256" height="256" fill="url(#l)"/>
  <g style="mix-blend-mode: color-dodge"><rect x="20" y="20" width="150" height="200" fill="url(#r)"/></g>
  <g style="mix-blend-mode: color-burn"><circle cx="170" cy="150" r="80" fill="url(#r)"/></g>
</svg>"##
    ),
    case!(
        "svg-feblend-dodge",
        1,
        Svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256">
  <defs>
    <linearGradient id="a" x2="1"><stop offset="0" stop-color="#103050"/><stop offset="1" stop-color="#d0e0a0"/></linearGradient>
    <linearGradient id="b" y2="1"><stop offset="0" stop-color="#a02060"/><stop offset="1" stop-color="#40c0f0" stop-opacity="0.6"/></linearGradient>
    <filter id="f" x="0" y="0" width="1" height="1">
      <feFlood flood-color="#7090c0" flood-opacity="0.7" result="flood"/>
      <feBlend in="SourceGraphic" in2="flood" mode="color-dodge"/>
    </filter>
  </defs>
  <rect width="256" height="256" fill="url(#a)"/>
  <rect x="16" y="16" width="224" height="224" fill="url(#b)" filter="url(#f)"/>
</svg>"##
    ),
    case!(
        "svg-rotate-skew",
        1,
        Svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256">
  <rect width="256" height="256" fill="#202428"/>
  <g transform="translate(128 128) rotate(17) skewX(13)">
    <rect x="-70" y="-50" width="140" height="100" fill="#e06030"/>
    <rect x="-40" y="-20" width="80" height="40" fill="#30a0e0" fill-opacity="0.6"/>
  </g>
</svg>"##
    ),
    case!(
        "svg-arcs",
        1,
        Svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256">
  <ellipse cx="96" cy="110" rx="70" ry="40" transform="rotate(-23 96 110)" fill="#80c040"/>
  <path d="M 40 220 A 60 35 30 1 1 220 180 A 25 25 0 0 0 150 140" fill="none" stroke="#f0d060" stroke-width="9"/>
</svg>"##
    ),
    case!(
        "svg-cusp-stroke",
        1,
        Svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256">
  <path d="M 30 160 C 226 60 30 60 226 160" fill="none" stroke="#e8e8f0" stroke-width="14" stroke-linejoin="round"/>
  <path d="M 30 220 C 180 170 76 170 226 220" fill="none" stroke="#f05050" stroke-width="6" stroke-linecap="round"/>
</svg>"##
    ),
    case!(
        "svg-marker",
        1,
        Svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256">
  <defs>
    <marker id="m" markerWidth="12" markerHeight="12" refX="6" refY="6" orient="auto">
      <path d="M 0 0 L 12 6 L 0 12 z" fill="#f0a020"/>
    </marker>
  </defs>
  <polyline points="24,200 80,60 140,150 230,40" fill="none" stroke="#a0c0f0" stroke-width="4"
    marker-start="url(#m)" marker-mid="url(#m)" marker-end="url(#m)"/>
</svg>"##
    ),
    case!(
        "svg-hue-rotate",
        1,
        Svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256">
  <defs>
    <linearGradient id="g" x2="1" y2="1"><stop offset="0" stop-color="#ff2020"/><stop offset="0.5" stop-color="#20ff60"/><stop offset="1" stop-color="#2040ff"/></linearGradient>
    <filter id="h"><feColorMatrix type="hueRotate" values="73"/></filter>
  </defs>
  <rect width="256" height="256" fill="url(#g)" filter="url(#h)"/>
</svg>"##
    ),
    case!(
        "svg-gamma",
        1,
        Svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256">
  <defs>
    <linearGradient id="g" x2="1"><stop offset="0" stop-color="#000000"/><stop offset="1" stop-color="#ffffff"/></linearGradient>
    <filter id="c">
      <feComponentTransfer>
        <feFuncR type="gamma" amplitude="1" exponent="0.45" offset="0"/>
        <feFuncG type="gamma" amplitude="0.9" exponent="2.2" offset="0.05"/>
        <feFuncB type="gamma" amplitude="1.1" exponent="1.3" offset="-0.02"/>
      </feComponentTransfer>
    </filter>
  </defs>
  <rect width="256" height="256" fill="url(#g)" filter="url(#c)"/>
</svg>"##
    ),
    case!(
        "svg-text-path",
        1,
        Svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256">
  <defs><path id="p" d="M 20 180 Q 128 20 236 180"/></defs>
  <text font-family="DejaVu Sans" font-size="26" fill="#f0f0f0">
    <textPath href="#p">Agile wave of text</textPath>
  </text>
</svg>"##
    ),
    case!("lottie-motion", 1, Lottie, LOTTIE_MOTION),
    case!(
        "latex-fraction-root",
        1,
        Latex,
        r"x = \frac{-b \pm \sqrt{b^2 - 4ac}}{2a}"
    ),
    case!(
        "latex-matrix-cases",
        1,
        Latex,
        r"\begin{pmatrix} a & b \\ c & d \end{pmatrix} f(x) = \begin{cases} x^2 & x \ge 0 \\ -x & x < 0 \end{cases}"
    ),
    case!(
        "latex-operators-accents",
        1,
        Latex,
        r"\sum_{n=1}^{\infty} \frac{1}{n^2} = \int_0^1 \widehat{f}\,dx + \color{blue}{\tilde\alpha}"
    ),
    case!(
        "pdf-shading-blend",
        1,
        Pdf,
        include_str!("../../corpus/extensions/shading.pdf")
    ),
    case!(
        "pdf-image",
        1,
        Pdf,
        include_str!("../../corpus/extensions/image.pdf")
    ),
];

/// No blend modes: velato ignores a layer's blend mode.
const LOTTIE_MOTION: &str = r#"{"v":"5.7.4","fr":30,"ip":0,"op":30,"w":256,"h":256,"nm":"pins","ddd":0,"assets":[],
"layers":[
{"ddd":0,"ind":1,"ty":4,"nm":"star","sr":1,"ao":0,"ip":0,"op":30,"st":0,"bm":0,
 "ks":{"o":{"a":0,"k":100},"r":{"a":1,"k":[{"t":0,"s":[0],"i":{"x":[0.42],"y":[0]},"o":{"x":[0.58],"y":[1]}},{"t":30,"s":[200]}]},
  "p":{"a":0,"k":[96,104,0]},"a":{"a":0,"k":[0,0,0]},"s":{"a":0,"k":[100,100,100]}},
 "shapes":[{"ty":"gr","nm":"g","it":[
  {"ty":"sr","nm":"star","sy":1,"d":1,"pt":{"a":0,"k":5},"p":{"a":0,"k":[0,0]},"r":{"a":0,"k":0},
   "ir":{"a":0,"k":30},"is":{"a":0,"k":20},"or":{"a":0,"k":70},"os":{"a":0,"k":10}},
  {"ty":"gf","nm":"grad","o":{"a":0,"k":100},"r":1,"t":2,"s":{"a":0,"k":[0,0]},"e":{"a":0,"k":[60,0]},
   "g":{"p":3,"k":{"a":0,"k":[0,1,0.3,0.1,0.5,0.9,0.8,0.2,1,0.2,0.4,1]}}},
  {"ty":"tr","p":{"a":0,"k":[0,0]},"a":{"a":0,"k":[0,0]},"s":{"a":0,"k":[100,100]},"r":{"a":0,"k":0},"o":{"a":0,"k":100}}]}]},
{"ddd":0,"ind":2,"ty":4,"nm":"ring","sr":1,"ao":0,"ip":0,"op":30,"st":0,"bm":0,
 "ks":{"o":{"a":0,"k":100},"r":{"a":0,"k":0},"p":{"a":0,"k":[176,80,0]},"a":{"a":0,"k":[0,0,0]},"s":{"a":0,"k":[100,100,100]}},
 "shapes":[{"ty":"gr","nm":"g","it":[
  {"ty":"el","nm":"e","d":1,"s":{"a":0,"k":[90,60]},"p":{"a":0,"k":[0,0]}},
  {"ty":"tm","nm":"trim","s":{"a":0,"k":12},"e":{"a":0,"k":81},"o":{"a":0,"k":37},"m":1},
  {"ty":"st","nm":"stroke","c":{"a":0,"k":[0.95,0.85,0.3,1]},"o":{"a":0,"k":100},"w":{"a":0,"k":7},"lc":2,"lj":2},
  {"ty":"tr","p":{"a":0,"k":[0,0]},"a":{"a":0,"k":[0,0]},"s":{"a":0,"k":[100,100]},"r":{"a":0,"k":0},"o":{"a":0,"k":100}}]}]},
{"ddd":0,"ind":3,"ty":4,"nm":"copies","sr":1,"ao":0,"ip":0,"op":30,"st":0,"bm":0,
 "ks":{"o":{"a":0,"k":100},"r":{"a":0,"k":0},"p":{"a":0,"k":[150,190,0]},"a":{"a":0,"k":[0,0,0]},"s":{"a":0,"k":[100,100,100]}},
 "shapes":[{"ty":"gr","nm":"g","it":[
  {"ty":"rc","nm":"r","d":1,"s":{"a":0,"k":[40,24]},"p":{"a":0,"k":[0,0]},"r":{"a":0,"k":6}},
  {"ty":"fl","nm":"f","c":{"a":0,"k":[0.3,0.7,0.95,1]},"o":{"a":0,"k":80},"r":1},
  {"ty":"rp","nm":"rep","c":{"a":0,"k":4},"o":{"a":0,"k":0},"m":1,
   "tr":{"ty":"tr","p":{"a":0,"k":[22,-9]},"a":{"a":0,"k":[0,0]},"s":{"a":0,"k":[83,83]},"r":{"a":0,"k":11},"so":{"a":0,"k":100},"eo":{"a":0,"k":45}}},
  {"ty":"tr","p":{"a":0,"k":[0,0]},"a":{"a":0,"k":[0,0]},"s":{"a":0,"k":[100,100]},"r":{"a":0,"k":0},"o":{"a":0,"k":100}}]}]}
]}"#;

struct FixtureFont;

impl ResourceResolver for FixtureFont {
    fn bytes(&self, locator: &str) -> Result<Arc<[u8]>, ExtensionError> {
        Err(ExtensionError::MissingResource(locator.to_owned()))
    }

    fn fonts(&self) -> Vec<Arc<[u8]>> {
        vec![Arc::from(FONT)]
    }
}

/// `case` drawn at [`SIDE`] by [`SIDE`], as premultiplied RGBA8.
///
/// # Panics
/// Panics if the document does not open or draw.
#[must_use]
pub fn rasterize(case: &Case) -> Vec<u8> {
    let (source, bytes) = match case.kind {
        Kind::Svg => (ProviderSource::Svg, Some(case.document)),
        Kind::Lottie => (
            ProviderSource::Lottie { animation: None },
            Some(case.document),
        ),
        Kind::Pdf => (
            ProviderSource::Pdf {
                page: 1,
                background: alpha2_model::Rgba::new(1.0, 1.0, 1.0, 1.0),
            },
            Some(case.document),
        ),
        Kind::Latex => (
            ProviderSource::Latex {
                latex: case.document.to_owned(),
                font_size: 64.0,
                color: alpha2_model::Rgba::new(0.9, 0.8, 0.2, 1.0),
                display: true,
            },
            None,
        ),
    };
    let mut opened = Registry::with_defaults()
        .open(
            &source,
            bytes.map(|document| Arc::from(document.as_bytes())),
            Arc::new(FixtureFont),
        )
        .unwrap_or_else(|error| panic!("{}: {error:?}", case.name));
    let frame = opened
        .render(MediaTime::from_ticks(13 * 4_000), (SIDE, SIDE))
        .unwrap_or_else(|error| panic!("{}: {error:?}", case.name));
    let pixels = frame.pixels.rgba8().expect("an 8-bit frame");
    if (frame.width, frame.height) == (SIDE, SIDE) {
        return pixels.to_vec();
    }
    assert!(
        matches!(case.kind, Kind::Latex | Kind::Pdf),
        "{} is {}x{}",
        case.name,
        frame.width,
        frame.height
    );
    // A formula or page keeps its aspect, so it is placed top left on a transparent square.
    let mut square = vec![0; (SIDE * SIDE * 4) as usize];
    let row = frame.width as usize * 4;
    for (y, line) in pixels.chunks(row).enumerate() {
        let at = y * SIDE as usize * 4;
        square[at..at + row].copy_from_slice(line);
    }
    square
}

/// `case`'s reference pixels.
///
/// # Panics
/// Panics if the reference is not a [`SIDE`]-square RGBA8 PNG.
#[must_use]
pub fn reference(case: &Case) -> Vec<u8> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(case.reference))
        .read_info()
        .unwrap_or_else(|error| panic!("{} reference: {error}", case.name));
    let mut pixels = vec![0; reader.output_buffer_size().expect("reference size")];
    let info = reader.next_frame(&mut pixels).expect("reference pixels");
    assert_eq!(
        (info.width, info.height, info.color_type, info.bit_depth),
        (SIDE, SIDE, png::ColorType::Rgba, png::BitDepth::Eight),
        "{} reference",
        case.name
    );
    pixels.truncate(info.buffer_size());
    pixels
}

/// How far one case's raster is from its reference.
pub struct Drift {
    /// The case.
    pub name: &'static str,
    /// How far it may be, in codes.
    pub tolerance: u8,
    /// The largest difference of a sample, in codes.
    pub max: u8,
    /// How many samples differ at all.
    pub differing: usize,
}

/// Every case's [`Drift`] on this target.
#[must_use]
pub fn drifts() -> Vec<Drift> {
    CASES
        .iter()
        .map(|case| {
            let (max, differing) = rasterize(case)
                .iter()
                .zip(reference(case))
                .map(|(sample, expected)| sample.abs_diff(expected))
                .filter(|difference| *difference > 0)
                .fold((0, 0), |(max, count), difference| {
                    (max.max(difference), count + 1)
                });
            Drift {
                name: case.name,
                tolerance: case.tolerance,
                max,
                differing,
            }
        })
        .collect()
}

/// [`drifts`] as JSON, `[{"name", "tolerance", "max", "differing"}]`, for the wasm build.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(js_name = extensionRasterDrifts)]
#[must_use]
pub fn extension_raster_drifts() -> String {
    serde_json::Value::Array(
        drifts()
            .iter()
            .map(|drift| {
                serde_json::json!({
                    "name": drift.name,
                    "tolerance": drift.tolerance,
                    "max": drift.max,
                    "differing": drift.differing,
                })
            })
            .collect(),
    )
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::{CASES, SIDE, drifts, rasterize};

    #[test]
    fn extension_rasters_stay_within_their_tolerance_of_the_reference() {
        if std::env::var_os("ALPHA2_UPDATE_RASTER_PINS").is_some() {
            let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../corpus/extensions/raster-pins");
            std::fs::create_dir_all(&directory).expect("reference directory");
            for case in &CASES {
                let file = std::fs::File::create(directory.join(format!("{}.png", case.name)))
                    .expect("reference file");
                let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), SIDE, SIDE);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                encoder
                    .write_header()
                    .and_then(|mut writer| writer.write_image_data(&rasterize(case)))
                    .expect("reference written");
            }
            return;
        }
        let mut failures = Vec::new();
        for drift in drifts() {
            // The count shows drift before a sample moves by more than a code.
            eprintln!(
                "{}: {} samples differ, by at most {}",
                drift.name, drift.differing, drift.max
            );
            if drift.max > drift.tolerance {
                failures.push(drift.name);
            }
        }
        assert!(
            failures.is_empty(),
            "further from the reference than allowed: {failures:?}"
        );
    }
}

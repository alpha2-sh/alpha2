//! Hostile documents for the provider tests and the fuzz seeds. Self-contained, because
//! `examples/hostile_seeds.rs` includes this file by path.

/// An SVG whose body repeats one entity of `per_entity` empty groups `references` times.
#[cfg(feature = "svg")]
pub(crate) fn svg_entity_elements(per_entity: usize, references: usize) -> String {
    format!(
        concat!(
            "<?xml version=\"1.0\"?>\n<!DOCTYPE svg [\n<!ENTITY b \"{}\">\n]>\n",
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\">{}</svg>\n"
        ),
        "<g/>".repeat(per_entity),
        "&b;".repeat(references)
    )
}

/// An SVG whose body repeats `&a;` `top` times, where `a` is `mid` references to an entity of
/// `per_entity` empty groups. The parser takes at most 255 references per top-level reference.
#[cfg(feature = "svg")]
pub(crate) fn svg_nested_entity_elements(per_entity: usize, mid: usize, top: usize) -> String {
    format!(
        concat!(
            "<?xml version=\"1.0\"?>\n<!DOCTYPE svg [\n<!ENTITY b \"{}\">\n<!ENTITY a \"{}\">\n]>\n",
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\">{}</svg>\n"
        ),
        "<g/>".repeat(per_entity),
        "&b;".repeat(mid),
        "&a;".repeat(top)
    )
}

/// An SVG with one attribute of `top` references to an entity of `mid` references to an entity
/// of `text` bytes.
#[cfg(feature = "svg")]
pub(crate) fn svg_entity_attribute(text: usize, mid: usize, top: usize) -> String {
    format!(
        concat!(
            "<?xml version=\"1.0\"?>\n<!DOCTYPE svg [\n<!ENTITY b \"{}\">\n<!ENTITY a \"{}\">\n]>\n",
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\">",
            "<desc id=\"{}\"/></svg>\n"
        ),
        "x".repeat(text),
        "&b;".repeat(mid),
        "&a;".repeat(top)
    )
}

/// An SVG that draws `inner` as `count` separate `data:` SVG images.
#[cfg(feature = "svg")]
pub(crate) fn svg_nested_images(inner: &str, count: usize) -> String {
    use base64::Engine as _;
    let image = format!(
        r#"<image width="8" height="8" href="data:image/svg+xml;base64,{}"/>"#,
        base64::engine::general_purpose::STANDARD.encode(inner)
    );
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8">{}</svg>"#,
        image.repeat(count)
    )
}

/// How one link of an SVG reference chain names the next.
#[cfg(feature = "svg")]
#[derive(Clone, Copy, Debug)]
pub(crate) enum SvgLink {
    /// A pattern whose content fills with the next.
    Fill,
    /// A pattern whose content strokes with the next.
    Stroke,
    /// A pattern whose content a style sheet rule fills with the next.
    StyleSheet,
    /// A pattern whose content a `style` attribute fills with the next.
    StyleAttribute,
    Mask,
    ClipPath,
    /// A filter whose `feImage` draws an element filtered by the next.
    FeImage,
}

#[cfg(feature = "svg")]
impl SvgLink {
    pub(crate) const ALL: [Self; 7] = [
        Self::Fill,
        Self::Stroke,
        Self::StyleSheet,
        Self::StyleAttribute,
        Self::Mask,
        Self::ClipPath,
        Self::FeImage,
    ];
}

/// An SVG whose drawn rectangle starts a chain of `links` references of one kind. With `ring`, the
/// last link names the first; otherwise the last one's content names nothing.
#[cfg(feature = "svg")]
pub(crate) fn svg_reference_chain(link: SvgLink, links: usize, ring: bool) -> String {
    use std::fmt::Write as _;
    let next = |at: usize| {
        if at + 1 < links {
            Some(at + 1)
        } else {
            ring.then_some(0)
        }
    };
    let property = match link {
        SvgLink::Fill | SvgLink::StyleSheet | SvgLink::StyleAttribute => "fill",
        SvgLink::Stroke => "stroke",
        SvgLink::Mask => "mask",
        SvgLink::ClipPath => "clip-path",
        SvgLink::FeImage => "filter",
    };
    let mut body = String::new();
    let mut sheet = String::new();
    for at in 0..links {
        let named = next(at).map_or_else(String::new, |next| match link {
            SvgLink::StyleSheet => {
                let _ = write!(sheet, "#e{at}{{fill:url(#r{next})}}");
                String::new()
            }
            SvgLink::StyleAttribute => format!(r#" style="fill:url(#r{next})""#),
            SvgLink::FeImage => String::new(),
            _ => format!(r#" {property}="url(#r{next})""#),
        });
        let content = format!(r#"<rect id="e{at}" width="1" height="1"{named}/>"#);
        let _ = match link {
            SvgLink::Fill | SvgLink::Stroke | SvgLink::StyleSheet | SvgLink::StyleAttribute => {
                write!(
                    body,
                    r#"<pattern id="r{at}" width="1" height="1">{content}</pattern>"#
                )
            }
            SvgLink::Mask => write!(body, r#"<mask id="r{at}">{content}</mask>"#),
            SvgLink::ClipPath => write!(body, r#"<clipPath id="r{at}">{content}</clipPath>"#),
            SvgLink::FeImage => {
                let image = next(at).map_or_else(String::new, |next| {
                    format!(r##"<feImage href="#f{next}"/>"##)
                });
                write!(
                    body,
                    r#"<filter id="r{at}">{image}<feFlood/></filter><rect id="f{at}" width="1" height="1" filter="url(#r{at})"/>"#
                )
            }
        };
    }
    if !sheet.is_empty() {
        body.insert_str(0, &format!("<style>{sheet}</style>"));
    }
    let first = match link {
        // The first filtered element is drawn where it stands.
        SvgLink::FeImage => String::new(),
        _ => format!(r#"<rect width="5" height="5" {property}="url(#r0)"/>"#),
    };
    format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">{body}{first}</svg>"#)
}

/// An SVG drawing a rectangle masked by `levels` masks of two rectangles, each masked by the next.
#[cfg(feature = "svg")]
pub(crate) fn svg_doubling_masks(levels: usize) -> String {
    use std::fmt::Write as _;
    let mut body =
        String::from(r##"<mask id="m0"><rect width="1" height="1" fill="#fff"/></mask>"##);
    for level in 1..=levels {
        let previous = level - 1;
        let _ = write!(
            body,
            r#"<mask id="m{level}"><rect width="1" height="1" mask="url(#m{previous})"/><rect x="1" width="1" height="1" mask="url(#m{previous})"/></mask>"#
        );
    }
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">{body}<rect width="10" height="10" mask="url(#m{levels})"/></svg>"#
    )
}

/// What the chunks of an [`ExrPart`] hold.
#[cfg(feature = "exr")]
#[derive(Clone, Copy)]
pub(crate) enum ExrChunks {
    /// A 9-byte zlib stream each, so the file is little more than its header.
    Empty,
    /// A valid zlib stream of a whole block of zeros each.
    Zeros,
    /// Empty, but the first chunk claims this many bytes of data.
    Claim(i32),
}

/// One ZIP-compressed part of an OpenEXR file: B, G and R half channels and `extra` float
/// channels, in scan lines or a single tile.
#[cfg(feature = "exr")]
#[derive(Clone, Copy)]
pub(crate) struct ExrPart {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) extra: usize,
    pub(crate) tiled: bool,
    pub(crate) chunks: ExrChunks,
}

/// An OpenEXR file of `parts`; more than one makes a multi-part file.
#[cfg(feature = "exr")]
pub(crate) fn exr_file(parts: &[ExrPart]) -> Vec<u8> {
    let multipart = parts.len() > 1;
    let single_tiled = !multipart && parts[0].tiled;
    let mut out = 20_000_630_u32.to_le_bytes().to_vec();
    let flags = 2 | if single_tiled { 0x200 } else { 0 } | if multipart { 0x1000 } else { 0 };
    out.extend_from_slice(&u32::to_le_bytes(flags));
    let mut blocks = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        exr_header(&mut out, part, multipart.then_some(index));
        // ZIP holds 16 scan lines per block.
        let rows = if part.tiled {
            part.height
        } else {
            part.height.min(16)
        };
        let mut y = 0;
        while y < part.height {
            blocks.push((index, y, rows.min(part.height - y)));
            y += rows;
        }
    }
    if multipart {
        out.push(0);
    }
    let table = out.len();
    out.resize(table + 8 * blocks.len(), 0);
    for (number, (index, y, rows)) in blocks.into_iter().enumerate() {
        let part = &parts[index];
        let offset = u64::try_from(out.len()).expect("offset");
        out[table + 8 * number..table + 8 * number + 8].copy_from_slice(&offset.to_le_bytes());
        let bytes_per_pixel = 6 + 4 * part.extra;
        let data = match part.chunks {
            ExrChunks::Zeros => zlib_zeros(rows as usize * part.width as usize * bytes_per_pixel),
            ExrChunks::Empty | ExrChunks::Claim(_) => vec![0x78, 0x9c, 0x63, 0, 0, 0, 1, 0, 1],
        };
        let declared = match part.chunks {
            ExrChunks::Claim(claim) if y == 0 => claim,
            _ => i32::try_from(data.len()).expect("chunk size"),
        };
        if multipart {
            out.extend_from_slice(&i32::try_from(index).expect("part").to_le_bytes());
        }
        if part.tiled {
            out.extend_from_slice(&[0; 16]);
        } else {
            out.extend_from_slice(&i32::try_from(y).expect("row").to_le_bytes());
        }
        out.extend_from_slice(&declared.to_le_bytes());
        out.extend_from_slice(&data);
    }
    out
}

/// Writes the header of `part`, with the name and type a multi-part file's part `index` needs.
#[cfg(feature = "exr")]
fn exr_header(out: &mut Vec<u8>, part: &ExrPart, index: Option<usize>) {
    let mut attribute = |name: &str, kind: &str, value: &[u8]| {
        out.extend_from_slice(&[name.as_bytes(), b"\0", kind.as_bytes(), b"\0"].concat());
        out.extend_from_slice(&u32::try_from(value.len()).expect("size").to_le_bytes());
        out.extend_from_slice(value);
    };
    let channel = |name: &str, pixel_type: u32| {
        let mut bytes = [name.as_bytes(), b"\0"].concat();
        for word in [pixel_type, 0, 1, 1] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes
    };
    let mut list = ["B", "G", "R"].map(|name| channel(name, 1)).concat();
    for at in 0..part.extra {
        list.extend(channel(&format!("c{at:06}"), 2));
    }
    list.push(0);
    let side = |value: u32| i32::try_from(value).expect("side") - 1;
    let window = [0, 0, side(part.width), side(part.height)]
        .map(i32::to_le_bytes)
        .concat();
    attribute("channels", "chlist", &list);
    attribute("compression", "compression", &[3]);
    attribute("dataWindow", "box2i", &window);
    attribute("displayWindow", "box2i", &window);
    attribute("lineOrder", "lineOrder", &[0]);
    if let Some(index) = index {
        attribute("name", "string", format!("part{index}").as_bytes());
    }
    attribute("pixelAspectRatio", "float", &1.0_f32.to_le_bytes());
    attribute("screenWindowCenter", "v2f", &[0; 8]);
    attribute("screenWindowWidth", "float", &1.0_f32.to_le_bytes());
    if part.tiled {
        let mut tiles = [part.width, part.height].map(u32::to_le_bytes).concat();
        tiles.push(0);
        attribute("tiles", "tiledesc", &tiles);
    }
    if index.is_some() {
        let kind = if part.tiled {
            "tiledimage"
        } else {
            "scanlineimage"
        };
        attribute("type", "string", kind.as_bytes());
    }
    out.push(0);
}

/// A zlib stream of `length` zero bytes: one literal, then fixed-Huffman copies of 258 bytes.
#[cfg(feature = "exr")]
fn zlib_zeros(length: usize) -> Vec<u8> {
    struct Bits {
        bytes: Vec<u8>,
        used: u32,
    }
    impl Bits {
        fn push(&mut self, bit: u32) {
            if self.used.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if bit != 0 {
                *self.bytes.last_mut().expect("byte") |= 1 << (self.used % 8);
            }
            self.used += 1;
        }
        // Huffman codes go most significant bit first.
        fn code(&mut self, code: u32, length: u32) {
            for at in (0..length).rev() {
                self.push((code >> at) & 1);
            }
        }
    }
    let mut bits = Bits {
        bytes: vec![0x78, 0x01],
        used: 16,
    };
    // Final block, fixed Huffman codes.
    bits.push(1);
    bits.push(1);
    bits.push(0);
    let literal = |bits: &mut Bits| bits.code(0x30, 8);
    let mut left = length;
    if left > 0 {
        literal(&mut bits);
        left -= 1;
    }
    while left >= 258 {
        // Length 258 is symbol 285; distance 1 is code 0.
        bits.code(0xc5, 8);
        bits.code(0, 5);
        left -= 258;
    }
    for _ in 0..left {
        literal(&mut bits);
    }
    bits.code(0, 7);
    let sum = u32::try_from(length % 65_521).expect("adler");
    bits.bytes
        .extend_from_slice(&((sum << 16) | 1).to_be_bytes());
    bits.bytes
}

/// A one-page document of `content` with `resources` and `extra` objects numbered from 5.
#[cfg(feature = "pdf")]
pub(crate) fn pdf_one_page(resources: &str, content: &[u8], extra: &[Vec<u8>]) -> Vec<u8> {
    use std::io::Write as _;
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 60 30] /Resources {resources} /Contents 4 0 R >>")
            .into_bytes(),
        pdf_stream("", content),
    ];
    objects.extend_from_slice(extra);
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        writeln!(out, "{} 0 obj", index + 1).expect("write");
        out.extend_from_slice(object);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    write!(out, "xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).expect("write");
    for offset in offsets {
        writeln!(out, "{offset:010} 00000 n ").expect("write");
    }
    write!(
        out,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    )
    .expect("write");
    out
}

/// A stream object of `data` with the dictionary `entries`.
#[cfg(feature = "pdf")]
pub(crate) fn pdf_stream(entries: &str, data: &[u8]) -> Vec<u8> {
    let mut object = format!("<< {entries} /Length {} >>\nstream\n", data.len()).into_bytes();
    object.extend_from_slice(data);
    object.extend_from_slice(b"\nendstream");
    object
}

#[cfg(feature = "pdf")]
fn pdf_flate(data: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(data).expect("compress");
    encoder.finish().expect("compressed")
}

/// A page that invokes a form `fan` times, each form the next one `fan` times, `levels` forms
/// deep; the last fills one rectangle.
#[cfg(feature = "pdf")]
pub(crate) fn pdf_forms(fan: usize, levels: usize) -> Vec<u8> {
    let invoke = |form: usize| format!("/F{form} Do\n").repeat(fan).into_bytes();
    let forms: Vec<Vec<u8>> = (1..=levels)
        .map(|level| {
            let (resources, content) = if level < levels {
                (
                    format!("<< /XObject << /F{} {} 0 R >> >>", level + 1, level + 5),
                    invoke(level + 1),
                )
            } else {
                ("<< >>".to_owned(), b"0 0 1 1 re f".to_vec())
            };
            pdf_stream(
                &format!(
                    "/Type /XObject /Subtype /Form /BBox [0 0 60 30] /Resources {resources} /Filter /FlateDecode"
                ),
                &pdf_flate(&content),
            )
        })
        .collect();
    pdf_one_page("<< /XObject << /F1 5 0 R >> >>", &invoke(1), &forms)
}

/// A page whose form invokes itself.
#[cfg(feature = "pdf")]
pub(crate) fn pdf_self_invoking_form() -> Vec<u8> {
    let form = pdf_stream(
        "/Type /XObject /Subtype /Form /BBox [0 0 60 30] /Resources << /XObject << /F1 5 0 R >> >>",
        b"0 0 1 1 re f /F1 Do /F1 Do",
    );
    pdf_one_page("<< /XObject << /F1 5 0 R >> >>", b"/F1 Do", &[form])
}

/// A page that draws one `side` by `side` image `draws` times, gray or RGB, of noise or flat.
#[cfg(feature = "pdf")]
pub(crate) fn pdf_image_draws(side: usize, rgb: bool, noise: bool, draws: usize) -> Vec<u8> {
    let channels = if rgb { 3 } else { 1 };
    let mut state = 0x2545_f491_u32;
    let samples: Vec<u8> = (0..side * side * channels)
        .map(|_| {
            if !noise {
                return 0x80;
            }
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state.to_le_bytes()[0]
        })
        .collect();
    let space = if rgb { "/DeviceRGB" } else { "/DeviceGray" };
    let image = pdf_stream(
        &format!(
            "/Type /XObject /Subtype /Image /Width {side} /Height {side} /ColorSpace {space} /BitsPerComponent 8 /Filter /FlateDecode"
        ),
        &pdf_flate(&samples),
    );
    pdf_one_page(
        "<< /XObject << /Im 5 0 R >> >>",
        &b"q 60 0 0 30 0 0 cm /Im Do Q\n".repeat(draws),
        &[image],
    )
}

/// A page that shows `shown` glyphs of a Type 3 font whose glyph fills `fills` rectangles.
#[cfg(feature = "pdf")]
pub(crate) fn pdf_type3_glyphs(fills: usize, shown: usize) -> Vec<u8> {
    let font = b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1 1] /FontMatrix [1 0 0 1 0 0] /CharProcs << /a 6 0 R >> /Encoding << /Type /Encoding /Differences [97 /a] >> /FirstChar 97 /LastChar 97 /Widths [1] >>".to_vec();
    let glyph = pdf_stream(
        "",
        &[b"1 0 d0 ".as_slice(), &b"0 0 1 1 re f ".repeat(fills)].concat(),
    );
    let text = [b"BT /T 1 Tf (".as_slice(), &b"a".repeat(shown), b") Tj ET"].concat();
    pdf_one_page("<< /Font << /T 5 0 R >> >>", &text, &[font, glyph])
}

/// A page whose form inflates to `bytes` of stroked lines.
#[cfg(feature = "pdf")]
pub(crate) fn pdf_content_bomb(bytes: usize) -> Vec<u8> {
    let line = b"0 0 m 1 1 l S\n";
    let content = line.repeat(bytes / line.len());
    let form = pdf_stream(
        "/Type /XObject /Subtype /Form /BBox [0 0 60 30] /Filter /FlateDecode",
        &pdf_flate(&content),
    );
    pdf_one_page("<< /XObject << /F1 5 0 R >> >>", b"/F1 Do", &[form])
}

/// A lossless 16 x 16 gray JPEG 2000 codestream whose SIZ marker declares `side` x `side` in one
/// tile, so its data is too short for the size it declares.
#[cfg(feature = "pdf")]
pub(crate) fn j2k_codestream(side: u32) -> Vec<u8> {
    let mut codestream = vec![
        0xff, 0x4f, 0xff, 0x51, 0x00, 0x29, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00,
        0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00,
        0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x07, 0x01, 0x01,
        0xff, 0x52, 0x00, 0x0c, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x04, 0x04, 0x00, 0x01, 0xff,
        0x5c, 0x00, 0x04, 0x40, 0x40, 0xff, 0x90, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0f,
        0x00, 0x01, 0xff, 0x93, 0x80, 0xff, 0xd9,
    ];
    // SOC, the SIZ marker and length, capabilities, then image and tile sizes.
    for at in [8, 24] {
        codestream[at..at + 4].copy_from_slice(&side.to_be_bytes());
        codestream[at + 4..at + 8].copy_from_slice(&side.to_be_bytes());
    }
    codestream
}

/// An embedded JBIG2 stream of one page information segment of `side` x `side`.
#[cfg(feature = "pdf")]
pub(crate) fn jbig2_page(side: u32) -> Vec<u8> {
    let mut info = Vec::new();
    for word in [side, side, 0, 0] {
        info.extend_from_slice(&word.to_be_bytes());
    }
    info.extend_from_slice(&[0, 0, 0]);
    // Segment number, type 48 (page information), no referred segments, page 1, data length.
    let mut segment = 0_u32.to_be_bytes().to_vec();
    segment.extend_from_slice(&[48, 0, 1]);
    segment.extend_from_slice(&u32::try_from(info.len()).expect("small").to_be_bytes());
    segment.extend_from_slice(&info);
    segment
}

/// A page that draws a one-pixel image of `data` `draws` times; `entries` name its filter.
#[cfg(feature = "pdf")]
pub(crate) fn pdf_coded_image(entries: &str, data: &[u8], draws: usize) -> Vec<u8> {
    let image = pdf_stream(
        &format!("/Type /XObject /Subtype /Image /Width 1 {entries} /ColorSpace /DeviceGray"),
        data,
    );
    pdf_one_page(
        "<< /XObject << /Im 5 0 R >> >>",
        &b"q 60 0 0 30 0 0 cm /Im Do Q\n".repeat(draws),
        &[image],
    )
}

/// A one-pixel-wide OpenEXR file of B44 blocks whose deltas wrap below zero, found by fuzzing.
#[cfg(feature = "exr")]
pub(crate) fn exr_wrapping_b44() -> Vec<u8> {
    vec![
        0x76, 0x2f, 0x31, 0x01, 0x02, 0x00, 0x00, 0x00, 0x63, 0x68, 0x61, 0x6e, 0x6e, 0x65, 0x6c,
        0x73, 0x00, 0x63, 0x68, 0x6c, 0x69, 0x73, 0x74, 0x00, 0x37, 0x00, 0x00, 0x00, 0x42, 0x00,
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
        0x00, 0x47, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x01, 0x00, 0x00, 0x00, 0x52, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x63, 0x6f, 0x6d, 0x70, 0x72, 0x65, 0x73,
        0x73, 0x69, 0x6f, 0x6e, 0x00, 0x63, 0x6f, 0x6d, 0x70, 0x72, 0x65, 0x73, 0x73, 0x69, 0x6f,
        0x6e, 0x00, 0x01, 0x00, 0x00, 0x00, 0x07, 0x64, 0x61, 0x74, 0x61, 0x57, 0x69, 0x6e, 0x64,
        0x6f, 0x77, 0x00, 0x62, 0x6f, 0x78, 0x32, 0x69, 0x00, 0x10, 0x00, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x2e, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x64,
        0x69, 0x73, 0x70, 0x6c, 0x61, 0x79, 0x57, 0x69, 0x6e, 0x64, 0x6f, 0x77, 0x00, 0x62, 0x6f,
        0x78, 0x32, 0x69, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x03, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x6c, 0x69, 0x6e, 0x65, 0x4f, 0x72,
        0x64, 0x65, 0x72, 0x00, 0x6c, 0x69, 0x6e, 0x65, 0x4f, 0x72, 0x64, 0x65, 0x72, 0x00, 0x01,
        0x00, 0x00, 0x00, 0x00, 0x70, 0x69, 0x78, 0x65, 0x6c, 0x41, 0x73, 0x70, 0x65, 0x63, 0x74,
        0x52, 0x61, 0x74, 0x69, 0x6f, 0x00, 0x66, 0x6c, 0x6f, 0x61, 0x74, 0x00, 0x04, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x80, 0x3f, 0x73, 0x63, 0x72, 0x65, 0x65, 0x6e, 0x57, 0x69, 0x6e, 0x64,
        0x6f, 0x77, 0x43, 0x65, 0x6e, 0x74, 0x65, 0x72, 0x00, 0x76, 0x34, 0x66, 0x00, 0x08, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x73, 0x63, 0x72, 0x65, 0x65,
        0x6e, 0x57, 0x69, 0x6e, 0x64, 0x6f, 0x77, 0x57, 0x69, 0x64, 0x74, 0x68, 0x00, 0x66, 0x6c,
        0x6f, 0x61, 0x74, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x3f, 0x00, 0x49, 0x01,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x5d, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
        0x00, 0x00, 0x00, 0x0e, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x34, 0x00, 0x38,
        0x00, 0x3c, 0x00, 0x38, 0x02, 0x00, 0x00, 0x38, 0x00, 0x38, 0x00, 0x3c, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x3c,
    ]
}

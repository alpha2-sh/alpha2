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

//! Bounds what drawing one page decodes before hayro, which decodes every stream it reads in full.

use std::collections::HashSet;
use std::io::Read as _;

use hayro_svg::hayro_syntax::Filter;
use hayro_svg::hayro_syntax::content::TypedIter;
use hayro_svg::hayro_syntax::content::ops::TypedInstruction;
use hayro_svg::hayro_syntax::object::{Dict, MaybeRef, Name, Object, ObjectIdentifier, Stream};
use hayro_svg::hayro_syntax::page::Page;
use hayro_svg::hayro_syntax::xref::XRef;

use crate::api::ExtensionError;
use crate::budget::MAX_SOURCE_IMAGE_PIXELS;

/// Most bytes the streams one page reaches may decode to together.
pub(super) const MAX_DECODED_BYTES: u64 = 256 * 1024 * 1024;

/// Most objects, direct or not, the walk from one page visits.
const MAX_VISITS: usize = 1 << 21;

/// Keys that lead to the page tree, other pages, actions, structure, or metadata, which drawing
/// the page does not read.
const SKIPPED: [&[u8]; 12] = [
    b"Parent",
    b"P",
    b"Dest",
    b"Dests",
    b"A",
    b"AA",
    b"B",
    b"Popup",
    b"IRT",
    b"StructParent",
    b"Metadata",
    b"Thumb",
];

/// The fonts every PDF reader has, which hayro draws with its bundled substitutes.
const STANDARD_FONTS: [&str; 14] = [
    "Helvetica",
    "Helvetica-Bold",
    "Helvetica-Oblique",
    "Helvetica-BoldOblique",
    "Courier",
    "Courier-Bold",
    "Courier-Oblique",
    "Courier-BoldOblique",
    "Times-Roman",
    "Times-Bold",
    "Times-Italic",
    "Times-BoldItalic",
    "Symbol",
    "ZapfDingbats",
];

/// Keys of a stream's own dictionary that describe its bytes rather than lead elsewhere.
const STREAM_KEYS: [&[u8]; 4] = [b"Length", b"Filter", b"DecodeParms", b"DL"];

/// What a stream is drawn as.
#[derive(Clone, Copy, PartialEq)]
enum Role {
    /// Read but not drawn as content, such as a font program or a function.
    Data,
    /// Operators, which may hold inline images.
    Content,
}

struct Walk<'a> {
    xref: &'a XRef,
    seen: HashSet<ObjectIdentifier>,
    visits: usize,
    decoded: u64,
    pixels: u64,
    substituted: Vec<String>,
}

/// Refuses a page whose streams decode past [`MAX_DECODED_BYTES`] or whose images decode past
/// [`MAX_SOURCE_IMAGE_PIXELS`], before hayro reads them. Answers the simple fonts it uses that
/// are neither embedded nor one of the standard 14, which hayro substitutes.
///
/// # Errors
/// [`ExtensionError::Budget`].
pub(super) fn check_page(page: &Page<'_>) -> Result<Vec<String>, ExtensionError> {
    let mut walk = Walk {
        xref: page.xref(),
        seen: HashSet::new(),
        visits: 0,
        decoded: 0,
        pixels: 0,
        substituted: Vec::new(),
    };
    let mut stack = Vec::new();
    for (key, value) in page.raw().entries() {
        let role = if key.as_ref() == b"Contents" {
            Role::Content
        } else {
            Role::Data
        };
        if !skipped(&key) {
            stack.push((value, role));
        }
    }
    while let Some((value, role)) = stack.pop() {
        walk.visits += 1;
        if walk.visits > MAX_VISITS {
            return Err(ExtensionError::Budget(format!(
                "the page reaches more than {MAX_VISITS} objects"
            )));
        }
        let object = match value {
            MaybeRef::Ref(reference) => {
                let id = ObjectIdentifier::from(reference);
                if !walk.seen.insert(id) {
                    continue;
                }
                match walk.xref.get::<Object<'_>>(id) {
                    Some(object) => object,
                    None => continue,
                }
            }
            MaybeRef::NotRef(object) => object,
        };
        match object {
            Object::Dict(dict) => {
                if is_named(&dict, b"Type", b"Page") {
                    continue;
                }
                walk.font(&dict);
                Walk::children(&dict, role, &mut stack, &[]);
            }
            Object::Array(array) => {
                for item in array.raw_iter() {
                    stack.push((item, role));
                }
            }
            Object::Stream(stream) => {
                let dict = stream.dict().clone();
                let role = if role == Role::Content
                    || is_named(&dict, b"Subtype", b"Form")
                    || dict.get::<i32>(b"PatternType") == Some(1)
                {
                    Role::Content
                } else {
                    Role::Data
                };
                walk.stream(&stream, role)?;
                Walk::children(&dict, Role::Data, &mut stack, &STREAM_KEYS);
            }
            _ => {}
        }
    }
    Ok(walk.substituted)
}

fn skipped(key: &Name<'_>) -> bool {
    SKIPPED.contains(&key.as_ref())
}

fn is_named(dict: &Dict<'_>, key: &[u8], name: &[u8]) -> bool {
    dict.get::<Name<'_>>(key)
        .is_some_and(|value| value.as_ref() == name)
}

impl Walk<'_> {
    /// Queues `dict`'s values. Glyph procedures and appearance streams are content.
    fn children<'o>(
        dict: &Dict<'o>,
        role: Role,
        stack: &mut Vec<(MaybeRef<Object<'o>>, Role)>,
        own: &[&[u8]],
    ) {
        for (key, value) in dict.entries() {
            if skipped(&key) || own.contains(&key.as_ref()) {
                continue;
            }
            let role = if matches!(key.as_ref(), b"CharProcs" | b"AP") {
                Role::Content
            } else {
                role
            };
            stack.push((value, role));
        }
    }

    /// Records a simple font that is neither embedded nor standard. Composite fonts are
    /// recorded when hayro asks for their fallback.
    fn font(&mut self, dict: &Dict<'_>) {
        let simple = [b"Type1".as_slice(), b"MMType1", b"TrueType"]
            .iter()
            .any(|subtype| is_named(dict, b"Subtype", subtype));
        if !is_named(dict, b"Type", b"Font") || !simple {
            return;
        }
        let embedded = dict
            .get::<Dict<'_>>(b"FontDescriptor")
            .is_some_and(|descriptor| {
                [b"FontFile".as_slice(), b"FontFile2", b"FontFile3"]
                    .iter()
                    .any(|key| descriptor.contains_key(key))
            });
        let Some(name) = dict.get::<Name<'_>>(b"BaseFont") else {
            return;
        };
        // A subset tag is six capitals and a plus sign.
        let name = name.as_str();
        let name = match name.split_once('+') {
            Some((tag, rest)) if tag.len() == 6 && tag.bytes().all(|b| b.is_ascii_uppercase()) => {
                rest
            }
            _ => name,
        };
        if !embedded
            && !STANDARD_FONTS.contains(&name)
            && !self.substituted.iter().any(|seen| seen == name)
        {
            self.substituted.push(name.to_owned());
        }
    }

    fn stream(&mut self, stream: &Stream<'_>, role: Role) -> Result<(), ExtensionError> {
        let dict = stream.dict();
        let image = is_named(dict, b"Subtype", b"Image");
        if image {
            self.image(dict, stream)?;
        }
        let keep = role == Role::Content && !image;
        let left = MAX_DECODED_BYTES - self.decoded;
        // Counted first, so a stream past the limit is refused without being held.
        if keep {
            decode(stream, left, false)?;
        }
        match decode(stream, left, keep)? {
            Decoded::Bytes(bytes) => {
                self.decoded += bytes.len() as u64;
                self.inline_images(&bytes)?;
            }
            Decoded::Length(length) => self.decoded += length,
            Decoded::Image => {}
        }
        Ok(())
    }

    /// Counts an image's pixels by its declared size and, for a JPEG, by its coded size, which
    /// the decoder uses.
    fn image(&mut self, dict: &Dict<'_>, stream: &Stream<'_>) -> Result<(), ExtensionError> {
        let side = |short: &[u8], long: &[u8]| {
            dict.get::<u32>(short)
                .or_else(|| dict.get::<u32>(long))
                .unwrap_or(0)
        };
        let mut pixels = u64::from(side(b"W", b"Width")) * u64::from(side(b"H", b"Height"));
        if stream.filters().contains(&Filter::DctDecode)
            && let Ok(coded) = imagesize::blob_size(&stream.raw_data())
        {
            let coded = u64::try_from(coded.width.saturating_mul(coded.height)).unwrap_or(u64::MAX);
            pixels = pixels.max(coded);
        }
        self.pixels = self.pixels.saturating_add(pixels);
        if self.pixels > MAX_SOURCE_IMAGE_PIXELS {
            return Err(ExtensionError::Budget(format!(
                "the page's images decode to more than {MAX_SOURCE_IMAGE_PIXELS} pixels"
            )));
        }
        // Decoded samples take at most 8 bytes a pixel (four 16-bit components).
        self.add_decoded(pixels.saturating_mul(8))
    }

    fn inline_images(&mut self, content: &[u8]) -> Result<(), ExtensionError> {
        let mut operations = TypedIter::new(content);
        while let Some(operation) = operations.next() {
            if let TypedInstruction::InlineImage(inline) = operation {
                let stream = inline.0.clone();
                self.image(stream.dict(), &stream)?;
                let left = MAX_DECODED_BYTES - self.decoded;
                if let Decoded::Length(length) = decode(&stream, left, false)? {
                    self.add_decoded(length)?;
                }
            }
        }
        Ok(())
    }

    fn add_decoded(&mut self, bytes: u64) -> Result<(), ExtensionError> {
        self.decoded = self.decoded.saturating_add(bytes);
        if self.decoded > MAX_DECODED_BYTES {
            return Err(too_much_decoded());
        }
        Ok(())
    }
}

fn too_much_decoded() -> ExtensionError {
    ExtensionError::Budget(format!(
        "the page's streams decode to more than {MAX_DECODED_BYTES} bytes"
    ))
}

enum Decoded {
    /// The decoded bytes, kept for content streams.
    Bytes(Vec<u8>),
    /// How many bytes it decodes to.
    Length(u64),
    /// Ends in an image codec, counted by its pixels.
    Image,
}

/// Decodes `stream` through its filters, never past `left` bytes at any step. Predictors only
/// shrink data, so they are not applied. A filter hayro does not read decodes to nothing.
fn decode(stream: &Stream<'_>, left: u64, keep: bool) -> Result<Decoded, ExtensionError> {
    let filters = stream.filters();
    let mut data = stream.raw_data().into_owned();
    let mut filters = filters.as_slice();
    if filters.first() == Some(&Filter::Crypt) {
        filters = &filters[1..];
    }
    let limit = usize::try_from(left).unwrap_or(usize::MAX);
    for (index, filter) in filters.iter().enumerate() {
        let last = index + 1 == filters.len();
        data = match filter {
            Filter::FlateDecode if last && !keep => return inflated_length(&data, left),
            Filter::FlateDecode => inflate(&data, limit)?,
            Filter::AsciiHexDecode => ascii_hex(&data),
            Filter::Ascii85Decode => ascii_85(&data, limit)?,
            Filter::RunLengthDecode => run_length(&data, limit)?,
            Filter::LzwDecode => {
                let early = stream
                    .dict()
                    .get::<Dict<'_>>(b"DecodeParms")
                    .and_then(|params| params.get::<i32>(b"EarlyChange"))
                    .unwrap_or(1);
                lzw(&data, early != 0, limit)?
            }
            Filter::DctDecode
            | Filter::JpxDecode
            | Filter::Jbig2Decode
            | Filter::CcittFaxDecode => return Ok(Decoded::Image),
            Filter::Crypt => Vec::new(),
        };
    }
    if data.len() > limit {
        return Err(too_much_decoded());
    }
    Ok(if keep {
        Decoded::Bytes(data)
    } else {
        Decoded::Length(data.len() as u64)
    })
}

/// Inflates zlib data, or raw deflate when zlib fails as hayro does, and answers the length and,
/// when `keep`, the bytes. Output before a corrupt end counts, since hayro keeps it too.
fn inflated(data: &[u8], left: u64, keep: bool) -> Result<(u64, Vec<u8>), ExtensionError> {
    let mut best = (0, Vec::new());
    for zlib in [true, false] {
        let mut reader: Box<dyn std::io::Read> = if zlib {
            Box::new(flate2::read::ZlibDecoder::new(data))
        } else {
            Box::new(flate2::read::DeflateDecoder::new(data))
        };
        let mut buffer = vec![0; 64 * 1024];
        let mut total = 0_u64;
        let mut kept = Vec::new();
        let finished = loop {
            match reader.read(&mut buffer) {
                Ok(0) => break true,
                Ok(read) => {
                    total += read as u64;
                    if total > left {
                        return Err(too_much_decoded());
                    }
                    if keep {
                        kept.extend_from_slice(&buffer[..read]);
                    }
                }
                Err(_) => break false,
            }
        };
        if total >= best.0 {
            best = (total, kept);
        }
        if finished {
            break;
        }
    }
    Ok(best)
}

fn inflated_length(data: &[u8], left: u64) -> Result<Decoded, ExtensionError> {
    inflated(data, left, false).map(|(length, _)| Decoded::Length(length))
}

fn inflate(data: &[u8], limit: usize) -> Result<Vec<u8>, ExtensionError> {
    inflated(data, limit as u64, true).map(|(_, bytes)| bytes)
}

fn ascii_hex(data: &[u8]) -> Vec<u8> {
    let digits = data
        .iter()
        .take_while(|&&byte| byte != b'>')
        .filter_map(|&byte| char::from(byte).to_digit(16))
        .collect::<Vec<_>>();
    digits
        .chunks(2)
        .map(|pair| u8::try_from(pair[0] * 16 + pair.get(1).copied().unwrap_or(0)).unwrap_or(0))
        .collect()
}

fn ascii_85(data: &[u8], limit: usize) -> Result<Vec<u8>, ExtensionError> {
    let mut output = Vec::new();
    let mut group = Vec::with_capacity(5);
    let flush = |group: &[u32], output: &mut Vec<u8>| {
        let value = (0..5).fold(0_u32, |value, index| {
            value
                .wrapping_mul(85)
                .wrapping_add(group.get(index).copied().unwrap_or(84))
        });
        output.extend_from_slice(&value.to_be_bytes()[..group.len().saturating_sub(1)]);
    };
    for &byte in data {
        match byte {
            b'~' => break,
            b'z' if group.is_empty() => output.extend_from_slice(&[0; 4]),
            b'!'..=b'u' => {
                group.push(u32::from(byte - b'!'));
                if group.len() == 5 {
                    flush(&group, &mut output);
                    group.clear();
                }
            }
            _ => {}
        }
        if output.len() > limit {
            return Err(too_much_decoded());
        }
    }
    if group.len() > 1 {
        flush(&group, &mut output);
    }
    Ok(output)
}

fn run_length(data: &[u8], limit: usize) -> Result<Vec<u8>, ExtensionError> {
    let mut output = Vec::new();
    let mut at = 0;
    while let Some(&length) = data.get(at) {
        match length {
            0..=127 => {
                let end = (at + 2 + usize::from(length)).min(data.len());
                output.extend_from_slice(&data[at + 1..end]);
                at = end;
            }
            128 => break,
            _ => {
                let Some(&byte) = data.get(at + 1) else { break };
                output.resize(output.len() + 257 - usize::from(length), byte);
                at += 2;
            }
        }
        if output.len() > limit {
            return Err(too_much_decoded());
        }
    }
    Ok(output)
}

/// A code past 257 is the previous code's string plus one byte, so codes are kept as ranges of
/// the output.
fn lzw(data: &[u8], early_change: bool, limit: usize) -> Result<Vec<u8>, ExtensionError> {
    const CLEAR: usize = 256;
    const END: usize = 257;
    let mut output = Vec::<u8>::new();
    let mut table = Vec::<(usize, usize)>::new();
    let mut previous: Option<(usize, usize)> = None;
    let mut width = 9;
    let (mut bits, mut held) = (0_u32, 0_u32);
    for &byte in data {
        bits = (bits << 8) | u32::from(byte);
        held += 8;
        while held >= width {
            held -= width;
            let code = ((bits >> held) & ((1 << width) - 1)) as usize;
            bits &= (1 << held) - 1;
            if code == CLEAR {
                table.clear();
                previous = None;
                width = 9;
                continue;
            }
            if code == END {
                return Ok(output);
            }
            let start = output.len();
            let entry = if code < CLEAR {
                output.push(u8::try_from(code).unwrap_or(0));
                (start, 1)
            } else if let Some(&(at, length)) = table.get(code - 258) {
                output.extend_from_within(at..at + length);
                (start, length)
            } else if let (Some((at, length)), true) = (previous, code - 258 == table.len()) {
                output.extend_from_within(at..at + length);
                output.push(output[at]);
                (start, length + 1)
            } else {
                return Ok(output);
            };
            if let Some((at, length)) = previous
                && table.len() + 258 < 4096
            {
                table.push((at, length + 1));
            }
            previous = Some(entry);
            let next = table.len() + 258 + usize::from(early_change);
            width = match next {
                ..512 => 9,
                512..1024 => 10,
                1024..2048 => 11,
                _ => 12,
            };
            if output.len() > limit {
                return Err(too_much_decoded());
            }
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::{ascii_85, ascii_hex, lzw, run_length};

    #[test]
    fn the_text_and_run_length_filters_decode_as_the_spec_shows() {
        assert_eq!(ascii_hex(b"48 65 6c6C6\n>99"), b"Hell\x60");
        assert_eq!(
            ascii_85(b"87cURD]i,\"Ebo80~>", 64).expect("decoded"),
            b"Hello World!"
        );
        assert_eq!(ascii_85(b"z!!~>", 64).expect("decoded"), [0, 0, 0, 0, 0]);
        assert_eq!(
            run_length(&[2, b'a', b'b', b'c', 253, b'x', 128], 64).expect("decoded"),
            b"abcxxxx"
        );
        assert!(run_length(&[129, 0, 129, 0], 200).is_err());
    }

    #[test]
    fn lzw_decodes_the_spec_example_within_its_limit() {
        // PDF 32000-1:2008 7.4.4.2: "-----A---B" with EarlyChange 1.
        let coded = [0x80, 0x0b, 0x60, 0x50, 0x22, 0x0c, 0x0c, 0x85, 0x01];
        assert_eq!(lzw(&coded, true, 64).expect("decoded"), b"-----A---B");
        assert!(lzw(&coded, true, 4).is_err());
    }
}

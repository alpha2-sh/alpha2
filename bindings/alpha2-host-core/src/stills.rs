//! Still picture decoding (PNG, JPEG, WebP) to straight RGBA, with the Exif orientation.
//!
//! Embedded ICC profiles are ignored: samples are read as sRGB code values, as the desktop host
//! reads them.

use std::io::Cursor;

use alpha2_media::{
    ChromaLocation, DecodeError, DecodedVideoFrame, Orientation, PixelFormat, Plane, Rect,
    SampleAspectRatio, frame_size_supported,
};
use alpha2_time::MediaTime;

/// A still picture container, told apart by its signature.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum StillFormat {
    /// PNG; an animated PNG reads as its default image.
    Png,
    /// JPEG (JFIF or Exif), baseline or progressive.
    Jpeg,
    /// WebP, lossy or lossless, with or without alpha; an animated WebP is refused.
    WebP,
}

impl StillFormat {
    /// A stable name for messages.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::WebP => "WebP",
        }
    }
}

/// The format whose signature starts `head`, which needs its first 12 bytes for WebP.
#[must_use]
pub fn still_format(head: &[u8]) -> Option<StillFormat> {
    if head.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(StillFormat::Jpeg)
    } else if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(StillFormat::Png)
    } else if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        Some(StillFormat::WebP)
    } else {
        None
    }
}

/// What a still's headers say, read without decoding its samples.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct StillInfo {
    /// The container.
    pub format: StillFormat,
    /// Stored width in pixels, before orientation.
    pub width: u32,
    /// Stored height in pixels, before orientation.
    pub height: u32,
    /// How the picture is shown upright, from its Exif `Orientation`.
    pub orientation: Orientation,
}

/// A decoded still picture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedStill {
    /// What the headers say.
    pub info: StillInfo,
    /// Straight (not premultiplied) 8-bit RGBA rows, `width * 4` bytes each, as stored.
    pub rgba: Vec<u8>,
}

impl DecodedStill {
    /// The picture as one straight RGBA frame at time zero. It carries no color tags; the
    /// renderer reads RGBA samples as sRGB code values.
    #[must_use]
    pub fn into_video_frame(self) -> DecodedVideoFrame {
        let StillInfo {
            width,
            height,
            orientation,
            ..
        } = self.info;
        DecodedVideoFrame {
            pts: MediaTime::ZERO,
            dts: None,
            duration: None,
            visible_rect: Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            sample_aspect_ratio: SampleAspectRatio::default(),
            orientation,
            pixel_format: PixelFormat::Rgba8,
            chroma_location: ChromaLocation::Unspecified,
            color: None,
            planes: vec![Plane {
                bytes: self.rgba.into(),
                stride: width * 4,
                width,
                height,
            }],
        }
    }
}

/// Reads a still's format, stored size and orientation from its headers.
///
/// # Errors
/// [`DecodeError::Unsupported`] for bytes that are no PNG, JPEG or WebP and for pictures past
/// the engine's frame limits; [`DecodeError::Adapter`] for malformed headers.
pub fn probe_still(bytes: &[u8]) -> Result<StillInfo, DecodeError> {
    let format = still_format(bytes).ok_or_else(not_a_still)?;
    let (width, height) = match format {
        StillFormat::Png => {
            let mut decoder = png_decoder(bytes);
            let info = decoder
                .read_header_info()
                .map_err(|error| png_error(&error))?;
            info.size()
        }
        StillFormat::Jpeg => {
            let mut decoder = jpeg_decoder(bytes);
            decoder.decode_headers().map_err(jpeg_error)?;
            jpeg_size(&decoder)?
        }
        StillFormat::WebP => webp_decoder(bytes)?.dimensions(),
    };
    check_size(format, width, height)?;
    Ok(StillInfo {
        format,
        width,
        height,
        orientation: orientation(format, bytes),
    })
}

/// Decodes a PNG, JPEG or WebP still to straight RGBA.
///
/// Palette, grayscale, 16-bit and interlaced PNGs, progressive, grayscale and CMYK JPEGs, and
/// lossy or lossless WebP with alpha all expand to RGBA. An animated PNG decodes its default
/// image. Sizes are checked against the engine's frame limits before allocating.
///
/// # Errors
/// As [`probe_still`], [`DecodeError::Unsupported`] for an animated WebP, and
/// [`DecodeError::Adapter`] for undecodable samples.
pub fn decode_still(bytes: &[u8]) -> Result<DecodedStill, DecodeError> {
    let format = still_format(bytes).ok_or_else(not_a_still)?;
    let (width, height, rgba) = match format {
        StillFormat::Png => decode_png(bytes)?,
        StillFormat::Jpeg => decode_jpeg(bytes)?,
        StillFormat::WebP => decode_webp(bytes)?,
    };
    Ok(DecodedStill {
        info: StillInfo {
            format,
            width,
            height,
            orientation: orientation(format, bytes),
        },
        rgba,
    })
}

fn not_a_still() -> DecodeError {
    DecodeError::Unsupported("not a PNG, JPEG or WebP picture".into())
}

fn check_size(format: StillFormat, width: u32, height: u32) -> Result<(), DecodeError> {
    if width == 0 || height == 0 {
        return Err(DecodeError::Adapter(format!(
            "{}: a {width}x{height} picture is empty",
            format.as_str()
        )));
    }
    if frame_size_supported(width, height) {
        Ok(())
    } else {
        Err(DecodeError::Unsupported(format!(
            "{}: a {width}x{height} picture is larger than {} pixels or {} on a side",
            format.as_str(),
            alpha2_media::MAX_FRAME_PIXELS,
            alpha2_media::MAX_FRAME_DIMENSION
        )))
    }
}

/// A zeroed RGBA buffer for a size [`check_size`] accepted.
fn rgba_buffer(width: u32, height: u32) -> Vec<u8> {
    // Within the frame limits, so at most 2^28 bytes.
    vec![0; width as usize * height as usize * 4]
}

/// Rewrites `pixels` packed pixels of `source_size` bytes at the front of `buffer` as RGBA.
///
/// Walks forward when pixels shrink and backward when they grow, so no pixel is overwritten
/// before it is read.
fn expand_in_place(
    buffer: &mut [u8],
    pixels: usize,
    source_size: usize,
    to_rgba: impl Fn(&[u8]) -> [u8; 4],
) {
    let mut convert = |index: usize| {
        let mut source = [0; 8];
        let start = index * source_size;
        source[..source_size].copy_from_slice(&buffer[start..start + source_size]);
        buffer[index * 4..index * 4 + 4].copy_from_slice(&to_rgba(&source[..source_size]));
    };
    if source_size >= 4 {
        (0..pixels).for_each(&mut convert);
    } else {
        (0..pixels).rev().for_each(&mut convert);
    }
}

fn png_decoder(bytes: &[u8]) -> png::Decoder<Cursor<&[u8]>> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    decoder
}

fn png_error(error: &png::DecodingError) -> DecodeError {
    DecodeError::Adapter(format!("PNG: {error}"))
}

fn decode_png(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), DecodeError> {
    let mut decoder = png_decoder(bytes);
    let (width, height) = decoder
        .read_header_info()
        .map_err(|error| png_error(&error))?
        .size();
    check_size(StillFormat::Png, width, height)?;
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().map_err(|error| png_error(&error))?;
    let (color, depth) = reader.output_color_type();
    let channels = color.samples();
    let sample_size = if depth == png::BitDepth::Sixteen {
        2
    } else {
        1
    };
    let pixels = width as usize * height as usize;
    let source_size = channels * sample_size;
    let mut buffer = vec![0; pixels * source_size.max(4)];
    let written = reader
        .output_buffer_size()
        .and_then(|written| buffer.get_mut(..written))
        .ok_or_else(|| DecodeError::Adapter("PNG: the decoded size disagrees".into()))?;
    let frame = reader
        .next_frame(written)
        .map_err(|error| png_error(&error))?;
    // The first frame is the `IDAT` image; an `fcTL` before it must give the canvas size.
    if (frame.width, frame.height) != (width, height) {
        return Err(DecodeError::Adapter(format!(
            "PNG: a {}x{} picture in a {width}x{height} canvas",
            frame.width, frame.height
        )));
    }
    let wide = |sample: &[u8]| round_16_to_8(u16::from_be_bytes([sample[0], sample[1]]));
    match (channels, sample_size) {
        (4, 1) => {}
        (3, 1) => expand_in_place(&mut buffer, pixels, 3, |p| [p[0], p[1], p[2], 255]),
        (2, 1) => expand_in_place(&mut buffer, pixels, 2, |p| [p[0], p[0], p[0], p[1]]),
        (1, 1) => expand_in_place(&mut buffer, pixels, 1, |p| [p[0], p[0], p[0], 255]),
        (4, 2) => expand_in_place(&mut buffer, pixels, 8, |p| {
            [wide(&p[0..]), wide(&p[2..]), wide(&p[4..]), wide(&p[6..])]
        }),
        (3, 2) => expand_in_place(&mut buffer, pixels, 6, |p| {
            [wide(&p[0..]), wide(&p[2..]), wide(&p[4..]), 255]
        }),
        (2, 2) => expand_in_place(&mut buffer, pixels, 4, |p| {
            let gray = wide(p);
            [gray, gray, gray, wide(&p[2..])]
        }),
        (1, 2) => expand_in_place(&mut buffer, pixels, 2, |p| {
            let gray = wide(p);
            [gray, gray, gray, 255]
        }),
        _ => {
            return Err(DecodeError::Unsupported(format!(
                "PNG: {color:?} at {} bits after expansion",
                depth as u8
            )));
        }
    }
    buffer.truncate(pixels * 4);
    if source_size > 4 {
        buffer.shrink_to_fit();
    }
    Ok((width, height, buffer))
}

/// A 16-bit sample on the 8-bit scale: `value / 256` rounded half up, at most 255.
fn round_16_to_8(value: u16) -> u8 {
    u8::try_from((u32::from(value) + 128) >> 8).unwrap_or(u8::MAX)
}

fn jpeg_decoder(bytes: &[u8]) -> zune_jpeg::JpegDecoder<zune_core::bytestream::ZCursor<&[u8]>> {
    // Any 16-bit side parses, so `check_size` gives the refusal.
    let options = zune_core::options::DecoderOptions::default()
        .set_max_width(usize::from(u16::MAX))
        .set_max_height(usize::from(u16::MAX))
        // RGB, not RGBA: zune-jpeg 0.5 misaligns four-channel output from CMYK and YCCK.
        .jpeg_set_out_colorspace(zune_core::colorspace::ColorSpace::RGB);
    zune_jpeg::JpegDecoder::new_with_options(zune_core::bytestream::ZCursor::new(bytes), options)
}

fn jpeg_error(error: zune_jpeg::errors::DecodeErrors) -> DecodeError {
    match error {
        zune_jpeg::errors::DecodeErrors::Unsupported(scheme) => {
            DecodeError::Unsupported(format!("JPEG: {scheme:?}"))
        }
        error => DecodeError::Adapter(format!("JPEG: {error}")),
    }
}

fn jpeg_size<T: zune_core::bytestream::ZByteReaderTrait>(
    decoder: &zune_jpeg::JpegDecoder<T>,
) -> Result<(u32, u32), DecodeError> {
    let malformed = || DecodeError::Adapter("JPEG: no frame header".into());
    let (width, height) = decoder.dimensions().ok_or_else(malformed)?;
    let side = |value: usize| u32::try_from(value).map_err(|_| malformed());
    Ok((side(width)?, side(height)?))
}

fn decode_jpeg(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), DecodeError> {
    let mut decoder = jpeg_decoder(bytes);
    decoder.decode_headers().map_err(jpeg_error)?;
    let (width, height) = jpeg_size(&decoder)?;
    check_size(StillFormat::Jpeg, width, height)?;
    let mut rgba = rgba_buffer(width, height);
    let pixels = width as usize * height as usize;
    decoder
        .decode_into(&mut rgba[..pixels * 3])
        .map_err(jpeg_error)?;
    expand_in_place(&mut rgba, pixels, 3, |p| [p[0], p[1], p[2], 255]);
    Ok((width, height, rgba))
}

fn webp_error(error: image_webp::DecodingError) -> DecodeError {
    match error {
        image_webp::DecodingError::UnsupportedFeature(feature) => {
            DecodeError::Unsupported(format!("WebP: {feature}"))
        }
        error => DecodeError::Adapter(format!("WebP: {error}")),
    }
}

/// A WebP decoder past the headers of a picture that is not animated.
fn webp_decoder(bytes: &[u8]) -> Result<image_webp::WebPDecoder<Cursor<&[u8]>>, DecodeError> {
    let decoder = image_webp::WebPDecoder::new(Cursor::new(bytes)).map_err(webp_error)?;
    if decoder.is_animated() {
        return Err(DecodeError::Unsupported(
            "WebP: an animated picture is not a still".into(),
        ));
    }
    Ok(decoder)
}

fn decode_webp(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), DecodeError> {
    let mut decoder = webp_decoder(bytes)?;
    let (width, height) = decoder.dimensions();
    check_size(StillFormat::WebP, width, height)?;
    let mut rgba = rgba_buffer(width, height);
    let pixels = width as usize * height as usize;
    if decoder.has_alpha() {
        decoder.read_image(&mut rgba).map_err(webp_error)?;
    } else {
        decoder
            .read_image(&mut rgba[..pixels * 3])
            .map_err(webp_error)?;
        expand_in_place(&mut rgba, pixels, 3, |p| [p[0], p[1], p[2], 255]);
    }
    Ok((width, height, rgba))
}

/// The orientation the still's Exif `Orientation` tag names, upright when there is none.
fn orientation(format: StillFormat, bytes: &[u8]) -> Orientation {
    let exif = match format {
        StillFormat::Jpeg => jpeg_exif(bytes),
        StillFormat::Png => png_exif(bytes),
        StillFormat::WebP => webp_exif(bytes),
    };
    exif.and_then(exif_orientation)
        .and_then(Orientation::from_exif)
        .unwrap_or(Orientation::UPRIGHT)
}

/// The TIFF structure of the first `APP1` Exif segment before the first scan.
fn jpeg_exif(bytes: &[u8]) -> Option<&[u8]> {
    let mut at = 2;
    loop {
        // Markers may be padded with any number of 0xFF fill bytes.
        while *bytes.get(at)? == 0xff && bytes.get(at + 1) == Some(&0xff) {
            at += 1;
        }
        if *bytes.get(at)? != 0xff {
            return None;
        }
        let marker = *bytes.get(at + 1)?;
        at += 2;
        match marker {
            // Start of scan, end of image.
            0xda | 0xd9 => return None,
            // Markers without a length: TEM and RSTn.
            0x01 | 0xd0..=0xd7 => continue,
            _ => {}
        }
        let length = usize::from(u16::from_be_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]));
        let segment = bytes.get(at + 2..at.checked_add(length)?)?;
        if marker == 0xe1
            && let Some(tiff) = segment.strip_prefix(b"Exif\0\0")
        {
            return Some(tiff);
        }
        at += length;
    }
}

/// The payload of the first `eXIf` chunk.
fn png_exif(bytes: &[u8]) -> Option<&[u8]> {
    let mut at = 8;
    loop {
        let length =
            usize::try_from(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?)).ok()?;
        let kind = bytes.get(at + 4..at + 8)?;
        let data = bytes.get(at + 8..(at + 8).checked_add(length)?)?;
        match kind {
            b"eXIf" => return Some(data),
            b"IEND" => return None,
            _ => at += 12 + length,
        }
    }
}

/// The payload of the first `EXIF` chunk.
fn webp_exif(bytes: &[u8]) -> Option<&[u8]> {
    let mut at = 12;
    loop {
        let kind = bytes.get(at..at + 4)?;
        let length = usize::try_from(u32::from_le_bytes(
            bytes.get(at + 4..at + 8)?.try_into().ok()?,
        ))
        .ok()?;
        let data = bytes.get(at + 8..(at + 8).checked_add(length)?)?;
        if kind == b"EXIF" {
            return Some(data);
        }
        // Chunks are padded to an even size.
        at += 8 + length + (length & 1);
    }
}

/// The `Orientation` (0x0112) value in IFD0 of an Exif TIFF structure.
///
/// PNG `eXIf` and WebP `EXIF` chunks hold the TIFF structure itself; one that starts with the
/// JPEG `Exif\0\0` prefix is malformed and names no orientation.
fn exif_orientation(tiff: &[u8]) -> Option<u16> {
    let big_endian = match tiff.get(..4)? {
        b"II*\0" => false,
        b"MM\0*" => true,
        _ => return None,
    };
    let u16_at = |at: usize| {
        let bytes = tiff.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if big_endian {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        })
    };
    let u32_at = |at: usize| {
        let bytes = tiff.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if big_endian {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        })
    };
    let ifd = usize::try_from(u32_at(4)?).ok()?;
    let entries = usize::from(u16_at(ifd)?);
    (0..entries).find_map(|index| {
        let entry = ifd.checked_add(2 + index * 12)?;
        if u16_at(entry)? != 0x0112 || u32_at(entry + 4)? != 1 {
            return None;
        }
        match u16_at(entry + 2)? {
            // SHORT, the type the standard gives the tag.
            3 => u16_at(entry + 8),
            // LONG, which some writers use.
            4 => u16::try_from(u32_at(entry + 8)?).ok(),
            _ => None,
        }
    })
}

#[cfg(test)]
#[path = "stills_tests.rs"]
mod tests;

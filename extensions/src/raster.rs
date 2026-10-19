//! PNG decoding shared by the animated-image, image-sequence, and Lottie providers.

use crate::api::ExtensionError;

/// The size a PNG's header declares, without decoding it; `None` when the
/// bytes do not start with a PNG signature and header.
#[cfg(feature = "lottie")]
pub(crate) fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let field =
        |at: usize| u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    Some((field(16), field(20)))
}

/// Decodes a still PNG to straight RGBA8, with its size; the size its header
/// declares is checked before anything that size is allocated.
///
/// # Errors
/// Why [`check_canvas`](crate::budget::check_canvas) refuses its size, or why
/// it cannot be decoded.
#[cfg(any(feature = "image-sequence", feature = "lottie"))]
pub(crate) fn decode_png(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), ExtensionError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::STRIP_16 | png::Transformations::ALPHA,
    );
    let mut reader = decoder
        .read_info()
        .map_err(|error| ExtensionError::Malformed(error.to_string()))?;
    let info = reader.info();
    crate::budget::check_canvas(info.width, info.height)?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| ExtensionError::Budget("the PNG is too large".into()))?;
    let mut buffer = vec![0; size];
    let output = reader
        .next_frame(&mut buffer)
        .map_err(|error| ExtensionError::Malformed(error.to_string()))?;
    let pixels = to_rgba8(
        &buffer[..output.buffer_size()],
        output.color_type,
        output.width,
        output.height,
        output.line_size,
    )?;
    Ok((output.width, output.height, pixels))
}

/// Converts a decoded PNG frame (8 bits, any color type after expansion) to
/// packed straight RGBA8.
pub(crate) fn to_rgba8(
    buffer: &[u8],
    color: png::ColorType,
    width: u32,
    height: u32,
    line_size: usize,
) -> Result<Vec<u8>, ExtensionError> {
    let channels = match color {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => {
            return Err(ExtensionError::Malformed(
                "an indexed PNG was not expanded".into(),
            ));
        }
    };
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
    for row in buffer.chunks(line_size).take(height as usize) {
        for pixel in row.chunks_exact(channels).take(width as usize) {
            pixels.extend_from_slice(&match pixel {
                [gray] => [*gray, *gray, *gray, 255],
                [gray, alpha] => [*gray, *gray, *gray, *alpha],
                [red, green, blue] => [*red, *green, *blue, 255],
                [red, green, blue, alpha] => [*red, *green, *blue, *alpha],
                _ => [0, 0, 0, 0],
            });
        }
    }
    Ok(pixels)
}

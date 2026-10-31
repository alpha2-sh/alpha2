//! One decoding rule for user text files (project and effects JSON, `.cube` LUTs).

use std::path::Path;

use crate::protocol::HostError;

/// UTF-8 with an optional BOM, or UTF-16 with a little- or big-endian BOM (what Windows
/// `PowerShell` 5.1 writes). Legacy code pages are refused rather than guessed at.
///
/// # Errors
/// `IO_ERROR` when the file cannot be read, `INVALID_INPUT` naming the first offending byte.
pub fn read_user_text(path: &Path, what: &str) -> Result<String, HostError> {
    let bytes = std::fs::read(path).map_err(|error| {
        HostError::new(
            "IO_ERROR",
            format!("cannot read the {what} {}: {error}", path.display()),
        )
    })?;
    decode_user_text(&bytes)
        .map_err(|offset| HostError::invalid_input(not_text(what, path, offset)))
}

pub fn not_text(what: &str, path: &Path, offset: usize) -> String {
    format!(
        "{what} {} is not UTF-8 text (invalid byte at offset {offset}); save it as UTF-8",
        path.display()
    )
}

/// Returns the offset of the first byte that breaks [`read_user_text`]'s rule on failure.
pub fn decode_user_text(bytes: &[u8]) -> Result<String, usize> {
    if let Some(rest) = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        return std::str::from_utf8(rest)
            .map(str::to_owned)
            .map_err(|error| 3 + error.valid_up_to());
    }
    if let Some(rest) = bytes.strip_prefix(&[0xff, 0xfe]) {
        return decode_utf16(rest, u16::from_le_bytes).map_err(|offset| 2 + offset);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xfe, 0xff]) {
        return decode_utf16(rest, u16::from_be_bytes).map_err(|offset| 2 + offset);
    }
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|error| error.valid_up_to())
}

fn decode_utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> Result<String, usize> {
    let (pairs, odd) = bytes.as_chunks::<2>();
    if !odd.is_empty() {
        return Err(bytes.len() - 1);
    }
    let mut text = String::with_capacity(pairs.len());
    let mut units = 0;
    for decoded in char::decode_utf16(pairs.iter().map(|pair| unit(*pair))) {
        let decoded = decoded.map_err(|_| units * 2)?;
        units += decoded.len_utf16();
        text.push(decoded);
    }
    Ok(text)
}

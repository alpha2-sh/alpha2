//! Spellings of paths: when two names a request gives are one output, and
//! which file a missing name was probably meant to be.

use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

/// The key two still outputs are compared by: the absolute path, composed
/// (NFC) and lower-cased. Windows and macOS file systems usually ignore case,
/// and macOS also normalization, so two spellings that differ only there are
/// one file; the clients compare by the same rule, with the same Unicode
/// default case mapping.
pub fn output_key(path: &Path) -> String {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    absolute
        .to_string_lossy()
        .nfc()
        .collect::<String>()
        .to_lowercase()
}

/// A file beside the missing `path` whose name is the same text in another
/// Unicode normalization (a name stored decomposed, as older macOS tools
/// wrote it, asked for composed, or the reverse), read from the parent
/// directory once. `None` when there is none, or the directory cannot be
/// read.
pub fn normalization_sibling(path: &Path) -> Option<PathBuf> {
    let wanted = path.file_name()?.to_str()?.nfc().collect::<String>();
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    std::fs::read_dir(parent)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .find(|name| {
            name.to_str().is_some_and(|name| {
                Some(name) != path.file_name().and_then(|own| own.to_str())
                    && name.nfc().collect::<String>() == wanted
            })
        })
        .map(|name| parent.join(name))
}

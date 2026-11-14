use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

/// A file beside the missing `path` whose name is the same text in another Unicode normalization.
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

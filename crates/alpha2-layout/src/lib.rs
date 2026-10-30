//! Where alpha2 text and layout boxes lie, without a GPU.
//!
//! A text style or a layout generator's box tree becomes the requests the
//! text host (`alpha2-text`) draws and the rectangles they fill: text by its
//! style's position and anchor, a layout by CSS flexbox and grid. The engine
//! draws from these same requests and rectangles, so a measurement and a
//! frame place text alike, and a host without a GPU (a wasm page, the Python
//! package) measures what a render would draw. [`json`] writes the
//! measurements in the one JSON form every binding returns.

pub mod json;
pub mod layout;
mod text;
pub mod text_animation;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alpha2_text::{TextFont, TextRasterError};

pub use layout::{
    LAYOUT_WRAP_SLACK, LayoutBounds, NodeBounds, NodeRects, ResolvedLayout, layout_texts,
    measure_layout, resolve, shape_texts,
};
pub use text::{
    MAX_FONT_PIXELS, MAX_PADDING_PIXELS, MAX_STROKE_PIXELS, ShadowPixels, TextAnimation,
    TextBounds, TextDraw, animated_text_key, font_pixels, layout_text_key, measure_text, rgba8,
    shadow_pixels, shaped_key, stroke_pixels, text_draw, text_fill, text_key, text_request,
};

/// The size, in pixels, of a picture text and layouts are placed on: never
/// empty, and within the largest frame the engine draws
/// ([`alpha2_media::MAX_FRAME_DIMENSION`], [`alpha2_media::MAX_FRAME_PIXELS`]).
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub struct PictureSize {
    width: u32,
    height: u32,
}

/// Why a size is not a [`PictureSize`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PictureSizeError {
    /// A side is zero.
    Empty,
    /// A side, or the area, is larger than any frame the engine draws.
    TooLarge,
}

impl fmt::Display for PictureSizeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "picture dimensions must be non-zero",
            Self::TooLarge => "picture dimensions exceed the largest frame",
        })
    }
}

impl std::error::Error for PictureSizeError {}

impl PictureSize {
    /// A picture of `width` by `height` pixels.
    ///
    /// # Errors
    /// Returns [`PictureSizeError`] for an empty side or a size larger than
    /// any frame.
    pub const fn new(width: u32, height: u32) -> Result<Self, PictureSizeError> {
        if width == 0 || height == 0 {
            Err(PictureSizeError::Empty)
        } else if width > alpha2_media::MAX_FRAME_DIMENSION
            || height > alpha2_media::MAX_FRAME_DIMENSION
            || (width as u64) * (height as u64) > alpha2_media::MAX_FRAME_PIXELS
        {
            Err(PictureSizeError::TooLarge)
        } else {
            Ok(Self { width, height })
        }
    }

    /// Width in pixels.
    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    /// Height in pixels.
    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }

    /// The shorter side in pixels: what lengths, a font size among them, are
    /// fractions of.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn unit(self) -> f32 {
        self.width.min(self.height) as f32
    }
}

/// A rectangle as fractions of the canvas, y down.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct CanvasRect {
    /// Left edge, a fraction of the canvas width.
    pub left: f32,
    /// Top edge, a fraction of the canvas height.
    pub top: f32,
    /// Right edge, a fraction of the canvas width.
    pub right: f32,
    /// Bottom edge, a fraction of the canvas height.
    pub bottom: f32,
}

/// Where a text's first and last lines' alphabetic baselines lie, y as a
/// fraction of the canvas (or picture) height, as a [`CanvasRect`]'s top is:
/// a text's baseline lines up with another's, or with a point, by
/// arithmetic.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Baselines {
    /// The first line's baseline.
    pub first: f32,
    /// The last line's baseline: the first's for one line.
    pub last: f32,
}

/// Why a font file could not be registered ([`fonts_from_files`]).
#[derive(Debug)]
#[non_exhaustive]
pub enum FontFileError {
    /// The file could not be read.
    Read {
        /// The file.
        path: PathBuf,
        /// Why.
        error: std::io::Error,
    },
    /// The file holds no face the text host draws at `face_index`.
    Invalid {
        /// The file.
        path: PathBuf,
        /// The face, counted from 0 within a collection.
        face_index: u32,
        /// The text host's reason.
        error: TextRasterError,
    },
}

impl fmt::Display for FontFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, error } => write!(formatter, "font {}: {error}", path.display()),
            Self::Invalid {
                path,
                face_index,
                error,
            } => write!(
                formatter,
                "font {} face {face_index}: {error}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for FontFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { error, .. } => Some(error),
            Self::Invalid { error, .. } => Some(error),
        }
    }
}

/// The faces a host registers from a list of font files, and each file's
/// bytes ([`fonts_from_files`]).
#[derive(Clone, Debug)]
pub struct FontFiles {
    /// Every face, in fallback order, sharing its file's bytes.
    pub faces: Vec<TextFont>,
    /// Each file's bytes, read once, in the order given.
    pub bytes: Vec<Arc<[u8]>>,
}

/// The fonts of the files at `paths`, in fallback order, as a host registers
/// them, and each file's bytes, read once: each file's family is its file
/// stem, and every face of a collection is registered, in its order, sharing
/// the file's bytes, so its weights can be matched. A measurement and a
/// render given the same paths draw with the same faces.
///
/// # Errors
/// Returns [`FontFileError`] naming the first file that cannot be read or
/// holds no face the text host draws.
pub fn fonts_from_files(paths: &[impl AsRef<Path>]) -> Result<FontFiles, FontFileError> {
    let mut fonts = Vec::with_capacity(paths.len());
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let path = path.as_ref();
        let bytes: Arc<[u8]> = std::fs::read(path)
            .map_err(|error| FontFileError::Read {
                path: path.to_owned(),
                error,
            })?
            .into();
        let family = path
            .file_stem()
            .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
        for face_index in 0..alpha2_text::face_count(&bytes) {
            alpha2_text::check_font(&bytes, face_index).map_err(|error| {
                FontFileError::Invalid {
                    path: path.to_owned(),
                    face_index,
                    error,
                }
            })?;
            fonts.push(TextFont {
                family: family.clone(),
                bytes: Arc::clone(&bytes),
                face_index,
            });
        }
        files.push(bytes);
    }
    Ok(FontFiles {
        faces: fonts,
        bytes: files,
    })
}

#[cfg(test)]
mod tests;

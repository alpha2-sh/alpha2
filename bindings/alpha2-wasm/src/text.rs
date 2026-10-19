//! `WasmTextRasterizer`: the portable text host (`alpha2-text`) for the web.
//!
//! The same shaping and rasterization code runs natively, so a browser preview
//! and a native export of one project draw identical text when both use it
//! with the same fonts.

use alpha2_media::{EmojiPolicy, TextAlign};
use alpha2_text::{TextFont, TextRasterRequest, TextRasterResult};
use serde::Deserialize;

/// The JSON request `WasmTextRasterizer.rasterize` takes (camelCase, as the
/// TypeScript wrapper writes it). The content version travels beside it as a
/// `bigint`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TextRequestWire {
    text: String,
    font_size: f32,
    #[serde(default)]
    max_width: f32,
    #[serde(default = "default_line_height")]
    line_height: f32,
    #[serde(default)]
    align: AlignWire,
    color: [u8; 4],
    #[serde(default)]
    emoji_policy: EmojiWire,
    #[serde(default)]
    outline: Option<OutlineWire>,
    #[serde(default)]
    background: Option<BackgroundWire>,
    #[serde(default)]
    font_family: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutlineWire {
    color: [u8; 4],
    width: f32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BackgroundWire {
    color: [u8; 4],
    padding: f32,
}

const fn default_line_height() -> f32 {
    1.2
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum AlignWire {
    #[default]
    Start,
    Center,
    End,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum EmojiWire {
    #[default]
    Monochrome,
    Replace,
}

impl TextRequestWire {
    pub(crate) fn into_request(self, content_version: u64) -> TextRasterRequest {
        TextRasterRequest {
            text: self.text,
            font_size: self.font_size,
            max_width: self.max_width,
            line_height: self.line_height,
            align: match self.align {
                AlignWire::Start => TextAlign::Start,
                AlignWire::Center => TextAlign::Center,
                AlignWire::End => TextAlign::End,
            },
            color: self.color,
            emoji_policy: match self.emoji_policy {
                EmojiWire::Monochrome => EmojiPolicy::Monochrome,
                EmojiWire::Replace => EmojiPolicy::Replace,
            },
            content_version,
            outline: self.outline.map(|outline| alpha2_text::TextOutline {
                color: outline.color,
                width: outline.width,
            }),
            background: self
                .background
                .map(|background| alpha2_text::TextBackground {
                    color: background.color,
                    padding: background.padding,
                }),
            font_family: self.font_family,
        }
    }
}

/// The layout facts of a result as the JSON the TypeScript wrapper reads.
pub(crate) fn descriptor_json(result: &TextRasterResult) -> String {
    let descriptor = &result.descriptor;
    serde_json::json!({
        "width": descriptor.texture.width,
        "height": descriptor.texture.height,
        "layoutX": descriptor.layout_x,
        "layoutY": descriptor.layout_y,
        "layoutWidth": descriptor.layout_width,
        "layoutHeight": descriptor.layout_height,
        "firstBaseline": descriptor.first_baseline,
        "ascent": descriptor.ascent,
        "descent": descriptor.descent,
        "lineCount": descriptor.line_count,
    })
    .to_string()
}

pub(crate) fn font(family: String, bytes: Vec<u8>, face_index: u32) -> TextFont {
    TextFont {
        family,
        bytes: bytes.into(),
        face_index,
    }
}

#[cfg(target_arch = "wasm32")]
mod web {
    use std::sync::Arc;

    use super::{TextRasterResult, TextRequestWire, descriptor_json, font};
    use alpha2_text::TextRasterizer;
    use wasm_bindgen::prelude::*;
    use web_sys::js_sys::Uint8Array;

    use crate::web::{invalid_input, typed_error};

    /// Shapes and rasterizes text from caller-supplied fonts, in wasm.
    #[wasm_bindgen]
    pub struct WasmTextRasterizer {
        inner: TextRasterizer,
    }

    /// One rasterized text texture.
    #[wasm_bindgen]
    pub struct RasterizedText {
        descriptor: String,
        /// Shared with the rasterizer's cache; copied once, into JavaScript.
        raster: Option<Arc<TextRasterResult>>,
    }

    #[wasm_bindgen]
    impl RasterizedText {
        /// Layout facts as JSON.
        #[wasm_bindgen(getter)]
        pub fn descriptor(&self) -> String {
            self.descriptor.clone()
        }

        /// Copies out the pixels, tightly packed premultiplied RGBA8 rows,
        /// and lets go of them; a second call returns an empty array.
        #[wasm_bindgen(js_name = takePixels)]
        pub fn take_pixels(&mut self) -> Uint8Array {
            self.raster.take().map_or_else(
                || Uint8Array::new_with_length(0),
                |raster| Uint8Array::from(raster.pixels.rgba8.as_slice()),
            )
        }
    }

    impl From<Arc<TextRasterResult>> for RasterizedText {
        fn from(result: Arc<TextRasterResult>) -> Self {
            Self {
                descriptor: descriptor_json(&result),
                raster: Some(result),
            }
        }
    }

    #[wasm_bindgen]
    impl WasmTextRasterizer {
        /// A rasterizer with no fonts; add them in fallback order.
        #[wasm_bindgen(constructor)]
        #[must_use]
        pub fn new() -> Self {
            Self {
                inner: TextRasterizer::empty(),
            }
        }

        /// Appends an OpenType font to the fallback stack.
        ///
        /// # Errors
        /// Rejects malformed font bytes.
        #[wasm_bindgen(js_name = addFont)]
        pub fn add_font(
            &mut self,
            family: String,
            bytes: Vec<u8>,
            face_index: u32,
        ) -> Result<(), JsValue> {
            self.inner
                .add_font(font(family, bytes, face_index))
                .map_err(|error| typed_error(&error))
        }

        /// Shapes, lays out, and rasterizes one request, or returns the cached
        /// result of its content version. The cache keeps the default budget
        /// of results (`alpha2_text::DEFAULT_TEXT_CACHE_BYTES`), dropping the
        /// least recently used past it.
        ///
        /// # Errors
        /// Rejects a malformed or out-of-range request, or a rasterizer
        /// without fonts.
        pub fn rasterize(
            &mut self,
            request_json: &str,
            content_version: u64,
        ) -> Result<RasterizedText, JsValue> {
            let wire: TextRequestWire = serde_json::from_str(request_json)
                .map_err(|error| invalid_input(format!("invalid text request: {error}")))?;
            self.inner
                .rasterize(&wire.into_request(content_version))
                .map(RasterizedText::from)
                .map_err(|error| typed_error(&error))
        }

        /// Drops one cached content version.
        pub fn invalidate(&mut self, content_version: u64) {
            self.inner.invalidate(content_version);
        }

        /// Drops every cached result, keeping the fonts.
        pub fn clear(&mut self) {
            self.inner.clear();
        }

        /// Number of cached results.
        #[wasm_bindgen(getter, js_name = cacheSize)]
        #[must_use]
        pub fn cache_size(&self) -> usize {
            self.inner.cache_len()
        }
    }

    impl Default for WasmTextRasterizer {
        fn default() -> Self {
            Self::new()
        }
    }
}

#[cfg(test)]
#[path = "text_tests.rs"]
mod tests;

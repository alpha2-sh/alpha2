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
    #[serde(default)]
    letter_spacing: f32,
    #[serde(default = "default_font_weight")]
    font_weight: u16,
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

const fn default_font_weight() -> u16 {
    400
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
    Color,
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
                EmojiWire::Color => EmojiPolicy::Color,
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
            letter_spacing: self.letter_spacing,
            font_weight: self.font_weight,
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

/// Every face of the font file `bytes`, sharing its bytes, each checked
/// before any is registered: what `addFontFile` adds, as a native host
/// registers a file ([`alpha2_layout::fonts_from_files`]).
pub(crate) fn file_faces(
    family: &str,
    bytes: Vec<u8>,
) -> Result<Vec<TextFont>, alpha2_text::TextRasterError> {
    let bytes: std::sync::Arc<[u8]> = bytes.into();
    (0..alpha2_text::face_count(&bytes))
        .map(|face_index| {
            alpha2_text::check_font(&bytes, face_index)?;
            Ok(TextFont {
                family: family.to_owned(),
                bytes: std::sync::Arc::clone(&bytes),
                face_index,
            })
        })
        .collect()
}

#[cfg(target_arch = "wasm32")]
mod web {
    use std::sync::Arc;

    use super::{TextRasterResult, TextRequestWire, descriptor_json, file_faces, font};
    use alpha2_layout::json::{MeasureError, measure_layout_json, measure_text_json};
    use alpha2_text::TextRasterizer;
    use wasm_bindgen::prelude::*;
    use web_sys::js_sys::Uint8Array;

    use crate::web::{invalid_input, typed_error};

    /// How many faces the font file `bytes` holds: a font collection's
    /// count, else 1. `addFontFile` registers each.
    #[wasm_bindgen(js_name = fontFaceCount)]
    #[must_use]
    pub fn font_face_count(bytes: &[u8]) -> u32 {
        alpha2_text::face_count(bytes)
    }

    /// A measurement's refusal as the engine answers it: the text host's as
    /// [`alpha2_engine::TextError`].
    fn measure_error(error: MeasureError) -> JsValue {
        match error {
            MeasureError::Text(error) => typed_error(&alpha2_engine::TextError::Raster(error)),
            error => invalid_input(error.to_string()),
        }
    }

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

        /// Appends every face of an OpenType font file to the fallback stack,
        /// in the file's order: each weight of a collection (`.ttc`), all
        /// checked before any is added.
        ///
        /// # Errors
        /// Rejects malformed font bytes, naming no face added.
        #[wasm_bindgen(js_name = addFontFile)]
        pub fn add_font_file(&mut self, family: &str, bytes: Vec<u8>) -> Result<(), JsValue> {
            for face in file_faces(family, bytes).map_err(|error| typed_error(&error))? {
                self.inner
                    .add_font(face)
                    .map_err(|error| typed_error(&error))?;
            }
            Ok(())
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

    /// Measures text generators with caller-supplied fonts, without a GPU:
    /// where the engine draws each one's text on the canvas. Given the fonts
    /// a renderer draws with, in the same order, it answers for that
    /// renderer's frames.
    #[wasm_bindgen]
    pub struct WasmTextMeasurer {
        inner: TextRasterizer,
    }

    #[wasm_bindgen]
    impl WasmTextMeasurer {
        /// A measurer with no fonts; add them in fallback order.
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

        /// Appends every face of an OpenType font file to the fallback stack,
        /// in the file's order: each weight of a collection (`.ttc`), all
        /// checked before any is added.
        ///
        /// # Errors
        /// Rejects malformed font bytes, naming no face added.
        #[wasm_bindgen(js_name = addFontFile)]
        pub fn add_font_file(&mut self, family: &str, bytes: Vec<u8>) -> Result<(), JsValue> {
            for face in file_faces(family, bytes).map_err(|error| typed_error(&error))? {
                self.inner
                    .add_font(face)
                    .map_err(|error| typed_error(&error))?;
            }
            Ok(())
        }

        /// Drops every font.
        #[wasm_bindgen(js_name = clearFonts)]
        pub fn clear_fonts(&mut self) {
            self.inner = TextRasterizer::empty();
        }

        /// Where the text generator `generator_json` (`{ "type": "text",
        /// "text", "style" }`, as a project holds it) lies on a `width` by
        /// `height` canvas, as JSON `{ layout, content, ink, lineCount,
        /// baselines }`: each a `{ left, top, right, bottom }` rectangle in
        /// fractions of the canvas, as the style's position is, on the picture
        /// the generator draws before its clip's transform; `ink` is `null`
        /// when the text draws nothing. `baselines` is `{ first, last }`, the
        /// first and last lines' alphabetic baselines in fractions of the
        /// canvas height. Measure at the project's canvas size: text laid out
        /// at another size can break its lines elsewhere.
        ///
        /// # Errors
        /// Rejects JSON that is no valid text generator, an empty canvas, and
        /// a measurer without fonts.
        pub fn measure(
            &self,
            generator_json: &str,
            width: u32,
            height: u32,
        ) -> Result<String, JsValue> {
            measure_text_json(&self.inner, generator_json, width, height).map_err(measure_error)
        }

        /// Where each node of the layout generator `generator_json`
        /// (`{ "type": "layout", "root" }`, as a project holds it) lies on a
        /// `width` by `height` picture at picture time `time` (ticks), laid
        /// out with these fonts as a frame lays it out, as JSON `{ nodes }`:
        /// each node in preorder as `{ at, border, content, overflow,
        /// lineCount, baselines }`, its boxes `{ left, top, right, bottom }` in
        /// fractions of the picture, before its clip's transform and node
        /// visuals. `lineCount` is `null` for a node without text, and
        /// `baselines` (`{ first, last }`, in fractions of the picture height)
        /// for a node without text or with empty text. Measure at the size
        /// the picture is drawn at. The layout's `"<n>px"` lengths are pixels
        /// of the project canvas, `canvasWidth` by `canvasHeight`: the
        /// picture's size when they are omitted.
        ///
        /// # Errors
        /// Rejects JSON that is no valid layout generator (on that canvas),
        /// an empty picture or canvas, a time that is not whole ticks, and a
        /// measurer without fonts for a layout with text.
        #[wasm_bindgen(js_name = measureLayout)]
        pub fn measure_layout(
            &self,
            generator_json: &str,
            width: u32,
            height: u32,
            time: f64,
            canvas_width: Option<u32>,
            canvas_height: Option<u32>,
        ) -> Result<String, JsValue> {
            let canvas = match (canvas_width, canvas_height) {
                (Some(canvas_width), Some(canvas_height)) => Some((canvas_width, canvas_height)),
                (None, None) => None,
                _ => {
                    return Err(invalid_input(
                        "give both canvasWidth and canvasHeight, or neither".to_owned(),
                    ));
                }
            };
            let time = crate::geometry::time(time).map_err(invalid_input)?;
            measure_layout_json(&self.inner, generator_json, width, height, time, canvas)
                .map_err(measure_error)
        }
    }

    impl Default for WasmTextMeasurer {
        fn default() -> Self {
            Self::new()
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

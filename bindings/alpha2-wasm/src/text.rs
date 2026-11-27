//! `WasmTextRasterizer`: the portable text host (`alpha2-text`) for the web. It runs the same
//! code as native, so preview and export draw identical text with the same fonts.

use alpha2_media::{EmojiPolicy, TextAlign};
use alpha2_text::{
    TextFill, TextFont, TextGradient, TextGradientKind, TextGradientStop, TextRasterRequest,
    TextRasterResult, TextSpread,
};
use serde::Deserialize;

/// The camelCase JSON request `WasmTextRasterizer.rasterize` takes. The content version travels
/// beside it as a `bigint`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TextRequestWire {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    runs: Option<Vec<RunWire>>,
    #[serde(default)]
    clamp: Option<ClampWire>,
    font_size: f32,
    #[serde(default)]
    max_width: f32,
    #[serde(default = "default_line_height")]
    line_height: f32,
    #[serde(default)]
    align: AlignWire,
    fill: FillWire,
    #[serde(default)]
    emoji_policy: EmojiWire,
    #[serde(default)]
    strokes: Vec<StrokeWire>,
    #[serde(default)]
    background: Option<BackgroundWire>,
    #[serde(default)]
    split_background: bool,
    /// One state every cluster takes, for an animated raster.
    #[serde(default)]
    cluster_state: Option<StateWire>,
    /// A state for each cluster, in logical order, for an animated raster.
    #[serde(default)]
    cluster_states: Option<Vec<StateWire>>,
    #[serde(default)]
    font_family: Option<String>,
    #[serde(default)]
    letter_spacing: f32,
    #[serde(default = "default_font_weight")]
    font_weight: u16,
}

/// A cluster's state; omitted fields take their identity value. Offset and tracking are in
/// pixels, rotation in clockwise radians, color mixes as `keep` and premultiplied linear-light
/// `add`.
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StateWire {
    #[serde(default)]
    offset: [f32; 2],
    #[serde(default = "unit_scale")]
    scale: [f32; 2],
    #[serde(default)]
    rotation: f32,
    #[serde(default = "full")]
    opacity: f32,
    #[serde(default)]
    fill: Option<MixWire>,
    #[serde(default)]
    stroke: Option<MixWire>,
    #[serde(default)]
    tracking: f32,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct MixWire {
    keep: f32,
    add: [f32; 4],
}

const fn unit_scale() -> [f32; 2] {
    [1.0, 1.0]
}

const fn full() -> f32 {
    1.0
}

impl StateWire {
    fn state(self) -> alpha2_text::ClusterState {
        let mix = |mix: Option<MixWire>| {
            mix.map(|mix| alpha2_text::ColorMix {
                keep: mix.keep,
                add: mix.add,
            })
        };
        alpha2_text::ClusterState {
            offset: self.offset,
            scale: self.scale,
            rotation: self.rotation,
            opacity: self.opacity,
            fill: mix(self.fill),
            stroke: mix(self.stroke),
            tracking: self.tracking,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RunWire {
    text: String,
    #[serde(default)]
    font_family: Option<String>,
    #[serde(default)]
    font_weight: Option<u16>,
    #[serde(default)]
    font_style: Option<FontStyleWire>,
    #[serde(default)]
    font_size: Option<f32>,
    #[serde(default)]
    letter_spacing: Option<f32>,
    #[serde(default)]
    fill: Option<FillWire>,
    #[serde(default)]
    strokes: Option<Vec<StrokeWire>>,
    #[serde(default)]
    background_color: Option<[u8; 4]>,
    #[serde(default)]
    underline: Option<bool>,
    #[serde(default)]
    line_through: Option<bool>,
}

impl RunWire {
    const fn is_plain(&self) -> bool {
        self.font_family.is_none()
            && self.font_weight.is_none()
            && self.font_style.is_none()
            && self.font_size.is_none()
            && self.letter_spacing.is_none()
            && self.fill.is_none()
            && self.strokes.is_none()
            && self.background_color.is_none()
            && self.underline.is_none()
            && self.line_through.is_none()
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FontStyleWire {
    Normal,
    Italic,
    Oblique,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClampWire {
    #[serde(default)]
    max_lines: Option<u32>,
    #[serde(default)]
    max_height: f32,
    #[serde(default = "default_ellipsis")]
    ellipsis: Option<String>,
}

#[allow(clippy::unnecessary_wraps)]
fn default_ellipsis() -> Option<String> {
    Some("\u{2026}".into())
}

fn strokes(list: Vec<StrokeWire>) -> Vec<alpha2_text::TextStroke> {
    list.into_iter()
        .map(|stroke| alpha2_text::TextStroke {
            color: stroke.color,
            width: stroke.width,
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrokeWire {
    color: [u8; 4],
    width: f32,
}

/// A fill: straight sRGB bytes, or a gradient over the layout box (points and radius as
/// fractions of it, conic angle in radians clockwise from up, stop colors straight sRGB in
/// `0..=1`).
#[derive(Deserialize)]
#[serde(untagged)]
enum FillWire {
    Solid([u8; 4]),
    Gradient(GradientWire),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum GradientWire {
    Linear {
        start: [f32; 2],
        end: [f32; 2],
        stops: Vec<StopWire>,
        #[serde(default)]
        spread: SpreadWire,
    },
    Radial {
        center: [f32; 2],
        radius: f32,
        stops: Vec<StopWire>,
        #[serde(default)]
        spread: SpreadWire,
    },
    Conic {
        center: [f32; 2],
        #[serde(default)]
        angle: f32,
        stops: Vec<StopWire>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StopWire {
    offset: f32,
    color: [f32; 4],
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SpreadWire {
    #[default]
    Pad,
    Repeat,
    Reflect,
}

impl FillWire {
    fn into_fill(self) -> TextFill {
        let stops = |stops: Vec<StopWire>| {
            stops
                .into_iter()
                .map(|stop| TextGradientStop {
                    offset: stop.offset,
                    color: stop.color,
                })
                .collect()
        };
        let spread = |spread: SpreadWire| match spread {
            SpreadWire::Pad => TextSpread::Pad,
            SpreadWire::Repeat => TextSpread::Repeat,
            SpreadWire::Reflect => TextSpread::Reflect,
        };
        match self {
            Self::Solid(color) => TextFill::Solid(color),
            Self::Gradient(GradientWire::Linear {
                start,
                end,
                stops: list,
                spread: mode,
            }) => TextFill::Gradient(TextGradient {
                kind: TextGradientKind::Linear { start, end },
                stops: stops(list),
                spread: spread(mode),
            }),
            Self::Gradient(GradientWire::Radial {
                center,
                radius,
                stops: list,
                spread: mode,
            }) => TextFill::Gradient(TextGradient {
                kind: TextGradientKind::Radial { center, radius },
                stops: stops(list),
                spread: spread(mode),
            }),
            Self::Gradient(GradientWire::Conic {
                center,
                angle,
                stops: list,
            }) => TextFill::Gradient(TextGradient {
                kind: TextGradientKind::Conic { center, angle },
                stops: stops(list),
                spread: TextSpread::Pad,
            }),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BackgroundWire {
    color: [u8; 4],
    padding: PaddingWire,
    #[serde(default)]
    radius: f32,
    #[serde(default)]
    border: Option<BorderWire>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PaddingWire {
    All(f32),
    Sides([f32; 4]),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BorderWire {
    width: f32,
    color: [u8; 4],
    #[serde(default)]
    dash: Option<DashWire>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DashWire {
    pattern: Vec<f32>,
    #[serde(default)]
    offset: f32,
}

impl BackgroundWire {
    fn into_background(self) -> alpha2_text::TextBackground {
        alpha2_text::TextBackground {
            color: self.color,
            padding: match self.padding {
                PaddingWire::All(padding) => [padding; 4],
                PaddingWire::Sides(sides) => sides,
            },
            radius: self.radius,
            border: self.border.map(|border| alpha2_text::TextBoxBorder {
                width: border.width,
                color: border.color,
                dash: border.dash.map(|dash| alpha2_text::TextBoxDash {
                    pattern: dash.pattern,
                    offset: dash.offset,
                }),
            }),
        }
    }
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
    pub(crate) const fn is_animated(&self) -> bool {
        self.cluster_state.is_some() || self.cluster_states.is_some()
    }

    pub(crate) fn into_request(
        self,
        content_version: u64,
    ) -> Result<TextRasterRequest, alpha2_text::TextRasterError> {
        let (text, runs) = match (self.text, self.runs) {
            (Some(text), None) => (text, Vec::new()),
            (None, Some(runs)) => (runs.iter().map(|run| run.text.as_str()).collect(), runs),
            _ => return Err(alpha2_text::TextRasterError::InvalidRequest),
        };
        let mut request = TextRasterRequest {
            text,
            font_size: self.font_size,
            max_width: self.max_width,
            line_height: self.line_height,
            align: match self.align {
                AlignWire::Start => TextAlign::Start,
                AlignWire::Center => TextAlign::Center,
                AlignWire::End => TextAlign::End,
            },
            fill: self.fill.into_fill(),
            emoji_policy: match self.emoji_policy {
                EmojiWire::Color => EmojiPolicy::Color,
                EmojiWire::Monochrome => EmojiPolicy::Monochrome,
                EmojiWire::Replace => EmojiPolicy::Replace,
            },
            content_version,
            strokes: strokes(self.strokes),
            split_background: self.split_background,
            background: self.background.map(BackgroundWire::into_background),
            font_family: self.font_family,
            letter_spacing: self.letter_spacing,
            font_weight: self.font_weight,
            clamp: self.clamp.map(|clamp| alpha2_text::TextClamp {
                max_lines: clamp.max_lines,
                max_height: clamp.max_height,
                ellipsis: clamp.ellipsis,
            }),
            spans: Vec::new(),
        };
        let mut start = 0;
        for run in runs {
            let bytes = start..start + run.text.len();
            start = bytes.end;
            if run.is_plain() {
                continue;
            }
            request.spans.push(alpha2_text::TextSpan {
                bytes,
                font_family: run.font_family.or_else(|| request.font_family.clone()),
                font_weight: run.font_weight.unwrap_or(request.font_weight),
                font_style: match run.font_style {
                    None | Some(FontStyleWire::Normal) => alpha2_text::FontStyle::Normal,
                    Some(FontStyleWire::Italic) => alpha2_text::FontStyle::Italic,
                    Some(FontStyleWire::Oblique) => alpha2_text::FontStyle::Oblique,
                },
                font_size: run.font_size.unwrap_or(request.font_size),
                letter_spacing: run.letter_spacing.unwrap_or(request.letter_spacing),
                fill: run
                    .fill
                    .map_or_else(|| request.fill.clone(), FillWire::into_fill),
                strokes: run.strokes.map_or_else(|| request.strokes.clone(), strokes),
                background_color: run.background_color,
                underline: run.underline.unwrap_or(false),
                line_through: run.line_through.unwrap_or(false),
            });
        }
        Ok(request)
    }
}

/// An animated request and its states: the wire's list, or its one state repeated for every
/// cluster.
pub(crate) fn animated_request(
    wire: TextRequestWire,
    content_version: u64,
    rasterizer: &alpha2_text::TextRasterizer,
) -> Result<(TextRasterRequest, Vec<alpha2_text::ClusterState>), alpha2_text::TextRasterError> {
    let (one, list) = (wire.cluster_state, wire.cluster_states.clone());
    let request = TextRequestWire {
        cluster_state: None,
        cluster_states: None,
        ..wire
    }
    .into_request(content_version)?;
    let states = if let Some(list) = list {
        list.into_iter().map(StateWire::state).collect()
    } else {
        let clusters = rasterizer.layout(&request)?.clusters.len();
        vec![one.map_or(alpha2_text::ClusterState::IDENTITY, StateWire::state); clusters]
    };
    Ok((request, states))
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

/// Every face of the font file `bytes`, sharing its bytes and all checked before any is
/// registered, as a native host registers a file ([`alpha2_layout::fonts_from_files`]).
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

    /// How many faces the font file `bytes` holds: a collection's count, else 1.
    #[wasm_bindgen(js_name = fontFaceCount)]
    #[must_use]
    pub fn font_face_count(bytes: &[u8]) -> u32 {
        alpha2_text::face_count(bytes)
    }

    /// Maps a text host measurement refusal to [`alpha2_engine::TextError`].
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
        /// Shared with the rasterizer's cache; copied once into JavaScript.
        raster: Option<Arc<TextRasterResult>>,
    }

    #[wasm_bindgen]
    impl RasterizedText {
        /// Layout facts as JSON.
        #[wasm_bindgen(getter)]
        pub fn descriptor(&self) -> String {
            self.descriptor.clone()
        }

        /// Copies out the background boxes split off by `splitBackground`, packed like the
        /// pixels. Empty when not split, or after `takePixels`.
        #[wasm_bindgen(js_name = takeBackground)]
        pub fn take_background(&mut self) -> Uint8Array {
            self.raster
                .as_ref()
                .and_then(|raster| raster.background.as_deref())
                .map_or_else(|| Uint8Array::new_with_length(0), Uint8Array::from)
        }

        /// Copies out the pixels as tightly packed premultiplied RGBA8 rows and releases them and
        /// the background; a second call returns an empty array.
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

        /// Appends every face of an OpenType font file (each weight of a `.ttc`) to the fallback
        /// stack in file order, all checked before any is added.
        ///
        /// # Errors
        /// Rejects malformed font bytes; no face is added.
        #[wasm_bindgen(js_name = addFontFile)]
        pub fn add_font_file(&mut self, family: &str, bytes: Vec<u8>) -> Result<(), JsValue> {
            for face in file_faces(family, bytes).map_err(|error| typed_error(&error))? {
                self.inner
                    .add_font(face)
                    .map_err(|error| typed_error(&error))?;
            }
            Ok(())
        }

        /// Shapes, lays out, and rasterizes one request, or returns the cached result for its
        /// content version. The cache keeps `alpha2_text::DEFAULT_TEXT_CACHE_BYTES` of results,
        /// least recently used first out.
        ///
        /// # Errors
        /// Rejects a malformed or out-of-range request, or a rasterizer without fonts.
        pub fn rasterize(
            &mut self,
            request_json: &str,
            content_version: u64,
        ) -> Result<RasterizedText, JsValue> {
            let wire: TextRequestWire = serde_json::from_str(request_json)
                .map_err(|error| invalid_input(format!("invalid text request: {error}")))?;
            if !wire.is_animated() {
                return wire
                    .into_request(content_version)
                    .and_then(|request| self.inner.rasterize(&request))
                    .map(RasterizedText::from)
                    .map_err(|error| typed_error(&error));
            }
            let (request, states) = super::animated_request(wire, content_version, &self.inner)
                .map_err(|error| typed_error(&error))?;
            self.inner
                .rasterize_animated(&request, &states)
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

    /// Measures where text generators draw on the canvas, without a GPU. Given the same fonts in
    /// the same order as a renderer, it answers for that renderer's frames.
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

        /// Appends every face of an OpenType font file (each weight of a `.ttc`) to the fallback
        /// stack in file order, all checked before any is added.
        ///
        /// # Errors
        /// Rejects malformed font bytes; no face is added.
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

        /// Where the text generator `generator_json` (`{ "type": "text", "text", "style" }`)
        /// lies on a `width` by `height` canvas, as JSON `{ layout, content, ink, lineCount,
        /// baselines, fontSize, truncated, overflow }`.
        ///
        /// Rectangles are `{ left, top, right, bottom }` in canvas fractions, before the clip's
        /// transform; `ink` is `null` when nothing draws. `baselines` is `{ first, last }`, the
        /// first and last alphabetic baselines as fractions of canvas height. `fontSize` is the
        /// size drawn after `font_size_fit`. `truncated` says `line_clamp` cut lines, and
        /// `overflow` that the drawn text splits a word or is taller than `max_height`. Measure
        /// at the project canvas size, since another size can break lines elsewhere.
        ///
        /// # Errors
        /// Rejects an invalid text generator, an empty canvas, and a measurer without fonts.
        pub fn measure(
            &self,
            generator_json: &str,
            width: u32,
            height: u32,
        ) -> Result<String, JsValue> {
            measure_text_json(&self.inner, generator_json, width, height).map_err(measure_error)
        }

        /// Where each line, word and cluster of the text generator `generator_json` lies on a
        /// `width` by `height` canvas, as JSON `{ fontSize, layout, lines, words, clusters }`:
        /// the static layout (no animator states), at the fitted size and after the clamp.
        ///
        /// Lines are `{ clusters: [first, end], box, em, baseline }`, words `{ clusters, boxes,
        /// ink }` (a box per line), clusters `{ range: [start, end], run, word, line, space,
        /// ellipsis, fontSize, baseline, box, ink }` with `range` in UTF-16 code units of the
        /// text. Rectangles are `{ left, top, right, bottom }` in canvas fractions before the
        /// clip's transform; `run`, `word` and `ink` may be `null`.
        ///
        /// # Errors
        /// Rejects an invalid text generator, an empty canvas, and a measurer without fonts.
        #[wasm_bindgen(js_name = layoutText)]
        pub fn layout_text(
            &self,
            generator_json: &str,
            width: u32,
            height: u32,
        ) -> Result<String, JsValue> {
            alpha2_layout::json::text_layout_json(
                &self.inner,
                generator_json,
                width,
                height,
                alpha2_layout::json::StringUnit::Utf16,
            )
            .map_err(measure_error)
        }

        /// Where each node of the layout generator `generator_json` (`{ "type": "layout",
        /// "root" }`) lies on a `width` by `height` picture at `time` (ticks), as JSON
        /// `{ nodes }`.
        ///
        /// Nodes are in preorder as `{ at, border, content, overflow, lineCount, baselines,
        /// fontSize, truncated }`, boxes `{ left, top, right, bottom }` in picture fractions
        /// before the clip's transform and node visuals. `lineCount` and `fontSize` are `null`
        /// for a node without text, and `baselines` (`{ first, last }`, fractions of picture
        /// height) also for empty text. `truncated` says `line_clamp` cut its lines. `"<n>px"`
        /// lengths are project canvas pixels (`canvasWidth` by `canvasHeight`, defaulting to the
        /// picture size).
        ///
        /// # Errors
        /// Rejects an invalid layout generator, an empty picture or canvas, a time that is not
        /// whole ticks, and a measurer without fonts for a layout with text.
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

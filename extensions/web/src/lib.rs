//! The extension providers as a separate wasm module, loaded only by pages that need them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use alpha2_extensions::{
    Diagnostic, ExtensionError, FramePixels, Registry, ResourceResolver, SharedResolver,
    VisualSource,
};
use alpha2_model::ProviderSource;
use alpha2_time::MediaTime;
use wasm_bindgen::prelude::*;

fn js_error(error: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&error.to_string())
}

/// Resources registered by the page: files by locator, and fonts.
#[derive(Default)]
struct Files {
    files: HashMap<String, Arc<[u8]>>,
    fonts: Vec<Arc<[u8]>>,
}

/// The page's resources, shared with sources that read them while rendering.
#[derive(Clone, Default)]
struct Resources(Arc<Mutex<Files>>);

impl Resources {
    fn lock(&self) -> MutexGuard<'_, Files> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ResourceResolver for Resources {
    fn bytes(&self, locator: &str) -> Result<Arc<[u8]>, ExtensionError> {
        self.lock()
            .files
            .get(locator)
            .cloned()
            .ok_or_else(|| ExtensionError::MissingResource(locator.to_owned()))
    }

    fn fonts(&self) -> Vec<Arc<[u8]>> {
        self.lock().fonts.clone()
    }
}

/// The providers this module was built with, and the sources a page opened.
///
/// Failures are thrown as plain string messages, not `Error` objects.
#[wasm_bindgen]
pub struct ExtensionRegistry {
    registry: Registry,
    resources: Resources,
    sources: HashMap<u32, Box<dyn VisualSource>>,
    next: u32,
}

/// One frame a source rendered.
#[wasm_bindgen]
pub struct RenderedFrame {
    width: u32,
    height: u32,
    pixels: FramePixels,
    index: usize,
    start: i64,
    end: Option<i64>,
    diagnostics: Vec<Diagnostic>,
}

#[wasm_bindgen]
impl RenderedFrame {
    /// Width in pixels.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// `rgba8` for [`Self::pixels`], `linear_f16` for [`Self::half_pixels`].
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn precision(&self) -> String {
        match self.pixels {
            FramePixels::LinearF16(_) => "linear_f16",
            _ => "rgba8",
        }
        .to_owned()
    }

    /// Premultiplied, sRGB-encoded, packed RGBA8 rows; empty for another precision.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn pixels(&self) -> js_sys::Uint8Array {
        js_sys::Uint8Array::from(self.pixels.rgba8().unwrap_or_default())
    }

    /// Premultiplied linear-light RGBA half-float bits, BT.709 primaries, rows packed; empty for
    /// another precision.
    #[wasm_bindgen(getter, js_name = halfPixels)]
    #[must_use]
    pub fn half_pixels(&self) -> js_sys::Uint16Array {
        js_sys::Uint16Array::from(self.pixels.linear_f16().unwrap_or_default())
    }

    /// Diagnostics, as a JSON array.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn diagnostics(&self) -> String {
        serde_json::to_string(&self.diagnostics).unwrap_or_default()
    }

    /// Which of the source's frames this is.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn index(&self) -> usize {
        self.index
    }

    /// The first source tick it shows at; 0 for the first frame.
    #[wasm_bindgen(getter, js_name = startTicks)]
    #[must_use]
    pub fn start_ticks(&self) -> i64 {
        self.start.max(0)
    }

    /// The first source tick it no longer shows at, or `undefined` for the last frame.
    #[wasm_bindgen(getter, js_name = endTicks)]
    #[must_use]
    pub fn end_ticks(&self) -> Option<i64> {
        self.end
    }
}

impl Default for ExtensionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[wasm_bindgen]
impl ExtensionRegistry {
    /// Every provider this module was built with.
    #[wasm_bindgen(constructor)]
    #[must_use]
    pub fn new() -> Self {
        Self {
            registry: Registry::with_defaults(),
            resources: Resources::default(),
            sources: HashMap::new(),
            next: 1,
        }
    }

    /// The providers' names, in priority order, as a JSON array.
    #[must_use]
    pub fn providers(&self) -> String {
        serde_json::to_string(&self.registry.names()).unwrap_or_default()
    }

    /// Whether this module opens a project's provider source (JSON).
    #[must_use]
    pub fn supports(&self, provider: &str) -> bool {
        serde_json::from_str::<ProviderSource>(provider)
            .is_ok_and(|source| self.registry.supports(&source))
    }

    /// The first claiming provider's result as JSON `{ provider, report }`, or `undefined`.
    ///
    /// # Errors
    /// If a claiming provider cannot read the bytes.
    pub fn probe(&self, bytes: &[u8]) -> Result<Option<String>, JsValue> {
        match self.registry.probe(bytes) {
            None => Ok(None),
            Some(Err(error)) => Err(js_error(error)),
            Some(Ok((provider, report))) => serde_json::to_string(&serde_json::json!({
                "provider": provider,
                "report": report,
            }))
            .map(Some)
            .map_err(js_error),
        }
    }

    /// Registers bytes under a locator. Sources read them only during open or render, so a page
    /// may register them just for that call (see [`Self::needs`]).
    #[wasm_bindgen(js_name = registerResource)]
    pub fn register_resource(&mut self, locator: String, bytes: Vec<u8>) {
        self.resources.lock().files.insert(locator, bytes.into());
    }

    /// Forgets the bytes registered under a locator.
    #[wasm_bindgen(js_name = releaseResource)]
    pub fn release_resource(&mut self, locator: &str) {
        self.resources.lock().files.remove(locator);
    }

    /// Total bytes registered under locators.
    #[wasm_bindgen(js_name = resourceBytes)]
    #[must_use]
    pub fn resource_bytes(&self) -> usize {
        self.resources
            .lock()
            .files
            .values()
            .map(|file| file.len())
            .sum()
    }

    /// Registers a font for SVG text. Sources read fonts when they open.
    #[wasm_bindgen(js_name = registerFont)]
    pub fn register_font(&mut self, bytes: Vec<u8>) {
        self.resources.lock().fonts.push(bytes.into());
    }

    /// Forgets every registered font; open sources keep theirs.
    #[wasm_bindgen(js_name = clearFonts)]
    pub fn clear_fonts(&mut self) {
        self.resources.lock().fonts.clear();
    }

    /// Locators opening a provider source (JSON) reads, as a JSON array.
    ///
    /// # Errors
    /// When the provider source is not valid JSON.
    #[wasm_bindgen(js_name = openNeeds)]
    pub fn open_needs(&self, provider: &str) -> Result<String, JsValue> {
        let provider: ProviderSource = serde_json::from_str(provider).map_err(js_error)?;
        serde_json::to_string(&self.registry.open_needs(&provider)).map_err(js_error)
    }

    /// Locators rendering at a source tick reads that the source does not hold, as a JSON array.
    ///
    /// # Errors
    /// When the handle is not open.
    pub fn needs(&self, handle: u32, time_ticks: i64) -> Result<String, JsValue> {
        let source = self
            .sources
            .get(&handle)
            .ok_or_else(|| js_error("the source is not open"))?;
        serde_json::to_string(&source.needs(MediaTime::from_ticks(time_ticks))).map_err(js_error)
    }

    /// Opens a provider source (JSON) with its bytes (none for an image sequence or LaTeX) and
    /// returns a handle.
    ///
    /// # Errors
    /// If the source cannot be opened.
    pub fn open(&mut self, provider: &str, bytes: Option<Vec<u8>>) -> Result<u32, JsValue> {
        let provider: ProviderSource = serde_json::from_str(provider).map_err(js_error)?;
        let source = self
            .registry
            .open(
                &provider,
                bytes.map(Arc::from),
                Arc::new(self.resources.clone()) as SharedResolver,
            )
            .map_err(js_error)?;
        let handle = self.next;
        self.next = self.next.wrapping_add(1).max(1);
        self.sources.insert(handle, source);
        Ok(handle)
    }

    /// What an opened source is, as JSON.
    ///
    /// # Errors
    /// When the handle is not open.
    pub fn report(&self, handle: u32) -> Result<String, JsValue> {
        let source = self
            .sources
            .get(&handle)
            .ok_or_else(|| js_error("the source is not open"))?;
        serde_json::to_string(source.report()).map_err(js_error)
    }

    /// The frame at a source tick. Vector sources rasterize to fit `fit_width` by `fit_height`
    /// (0 by 0 for their own size).
    ///
    /// # Errors
    /// When the handle is not open or the frame cannot be produced.
    pub fn render(
        &mut self,
        handle: u32,
        time_ticks: i64,
        fit_width: u32,
        fit_height: u32,
    ) -> Result<RenderedFrame, JsValue> {
        let source = self
            .sources
            .get_mut(&handle)
            .ok_or_else(|| js_error("the source is not open"))?;
        let frame = source
            .render(MediaTime::from_ticks(time_ticks), (fit_width, fit_height))
            .map_err(js_error)?;
        Ok(RenderedFrame {
            width: frame.width,
            height: frame.height,
            index: frame.index,
            start: frame.start.ticks(),
            end: frame.end.map(MediaTime::ticks),
            pixels: frame.pixels,
            diagnostics: frame.diagnostics,
        })
    }

    /// Bytes an open source holds between renders (cached frames, images, and file bytes).
    ///
    /// # Errors
    /// When the handle is not open.
    #[wasm_bindgen(js_name = residentBytes)]
    pub fn resident_bytes(&self, handle: u32) -> Result<usize, JsValue> {
        self.sources
            .get(&handle)
            .map(|source| source.resident_bytes())
            .ok_or_else(|| js_error("the source is not open"))
    }

    /// A handle to another independently positioned reader that shares the source's decoded
    /// data. It stays valid after the original is closed. `undefined` when any time is equally
    /// cheap to draw, so the page should share the source.
    ///
    /// # Errors
    /// When the handle is not open.
    pub fn reader(&mut self, handle: u32) -> Result<Option<u32>, JsValue> {
        let source = self
            .sources
            .get(&handle)
            .ok_or_else(|| js_error("the source is not open"))?;
        let Some(reader) = source.reader() else {
            return Ok(None);
        };
        let handle = self.next;
        self.next = self.next.wrapping_add(1).max(1);
        self.sources.insert(handle, reader);
        Ok(Some(handle))
    }

    /// The part of [`Self::resident_bytes`] shared with [`Self::reader`] readers, counted once.
    ///
    /// # Errors
    /// When the handle is not open.
    #[wasm_bindgen(js_name = sharedBytes)]
    pub fn shared_bytes(&self, handle: u32) -> Result<usize, JsValue> {
        self.sources
            .get(&handle)
            .map(|source| source.shared_bytes())
            .ok_or_else(|| js_error("the source is not open"))
    }

    /// Closes a source.
    pub fn release(&mut self, handle: u32) {
        self.sources.remove(&handle);
    }
}

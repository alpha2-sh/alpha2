//! Static SVG, rasterized by resvg at the size it is shown.
//!
//! Only host-registered resources are read: fonts from the resolver (never system fonts) and
//! images from `data:` URLs or registered locators. Missing fonts and images are reported.
//!
//! usvg and resvg do not bound their own work, so costs are counted first and an over-budget
//! document is refused: nesting depth ([`markup`], [`MAX_NESTING`]), conversion steps
//! ([`document`], [`MAX_READING`]), total SVG text including nested images
//! ([`MAX_SVG_DOCUMENT_BYTES`]), and drawing cost at each render size ([`render_cost`]).

mod document;
mod markup;
mod path_data;
mod render_cost;

use std::io::Read as _;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use alpha2_time::MediaTime;
use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg::{
    self, FontResolver, ImageHrefResolver, ImageKind, Node, Options, Tree, roxmltree,
};

use alpha2_model::{MAX_SVG_DOCUMENT_BYTES, ProviderSource};

use crate::api::{
    Diagnostic, ExtensionError, Format, FramePixels, ProbeReport, ResourceResolver, Timing,
    VisualFrame, VisualProvider, VisualSource,
};
use crate::budget::{check_canvas, check_document_size, check_image_pixels};

/// Reads static SVG documents.
pub struct SvgProvider;

impl VisualProvider for SvgProvider {
    fn name(&self) -> &'static str {
        "svg"
    }

    fn probe(&self, bytes: &[u8]) -> Option<Result<ProbeReport, ExtensionError>> {
        if !looks_like_svg(bytes) {
            return None;
        }
        Some(
            check_document_size(&ProviderSource::Svg, bytes, bytes.len() as u64)
                .and_then(|()| parse(bytes, &crate::api::NoResources))
                .map(|(_, report)| report),
        )
    }

    fn open(
        &self,
        bytes: Arc<[u8]>,
        _source: &ProviderSource,
        resolver: &dyn ResourceResolver,
    ) -> Result<Box<dyn VisualSource>, ExtensionError> {
        if !looks_like_svg(&bytes) {
            return Err(ExtensionError::Mismatch("not an SVG document".into()));
        }
        check_document_size(&ProviderSource::Svg, &bytes, bytes.len() as u64)?;
        let (tree, report) = parse(&bytes, resolver)?;
        Ok(Box::new(SvgSource {
            tree,
            report,
            last: None,
        }))
    }
}

/// Whether the bytes are XML with an `svg` root, skipping any BOM, prolog, comments, and doctype
/// (internal subset included, as Adobe Illustrator writes).
fn looks_like_svg(bytes: &[u8]) -> bool {
    let mut rest = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    loop {
        rest = rest.trim_ascii_start();
        let skipped = if let Some(after) = rest.strip_prefix(b"<?") {
            after_closing(after, b"?>")
        } else if let Some(after) = rest.strip_prefix(b"<!--") {
            after_closing(after, b"-->")
        } else if let Some(after) = rest.strip_prefix(b"<!DOCTYPE") {
            after_doctype(after)
        } else {
            return rest.strip_prefix(b"<svg").is_some_and(|after| {
                after
                    .first()
                    .is_some_and(|next| next.is_ascii_whitespace() || matches!(next, b'>' | b'/'))
            });
        };
        let Some(after) = skipped else {
            return false;
        };
        rest = after;
    }
}

fn after_closing<'a>(bytes: &'a [u8], closing: &[u8]) -> Option<&'a [u8]> {
    let at = bytes
        .windows(closing.len())
        .position(|window| window == closing)?;
    Some(&bytes[at + closing.len()..])
}

/// What follows a doctype whose `<!DOCTYPE` precedes `bytes`. Quoted literals, comments, and the
/// internal subset may contain a `>` that does not end it.
fn after_doctype(mut bytes: &[u8]) -> Option<&[u8]> {
    let mut subset = false;
    loop {
        if let Some(after) = bytes.strip_prefix(b"<!--") {
            bytes = after_closing(after, b"-->")?;
        } else if let Some(after) = bytes.strip_prefix(b"<?") {
            bytes = after_closing(after, b"?>")?;
        } else {
            let (&first, after) = bytes.split_first()?;
            bytes = match first {
                b'"' | b'\'' => after_closing(after, &[first])?,
                b'[' if !subset => {
                    subset = true;
                    after
                }
                b']' if subset => {
                    subset = false;
                    after
                }
                b'>' if !subset => return Some(after),
                _ => after,
            };
        }
    }
}

/// Deepest element nesting, including nested SVG images and `<use>` copies. At this depth the
/// recursive parser, usvg, and resvg use well under a quarter of a 1 MiB stack; a native release
/// build overflows 1 MiB between 600 and 1,000 levels. Real documents nest a few tens deep.
pub(crate) const MAX_NESTING: usize = 256;

/// Most conversion steps for one SVG and its nested images (style matching, `<use>` checks,
/// ancestry walks at about 18 ns each). About a second natively.
pub(crate) const MAX_READING: u64 = 1 << 27;

/// Remaining budget for one document and its nested SVG images, shared with the image resolver.
struct Reading {
    /// Unpacked SVG text bytes left.
    text: AtomicUsize,
    steps: AtomicU64,
    /// Levels used by the enclosing documents being converted.
    nesting: AtomicUsize,
    /// A nested document's budget error, which refuses the outer document.
    refused: Mutex<Option<ExtensionError>>,
}

impl Reading {
    fn new() -> Self {
        Self {
            text: AtomicUsize::new(MAX_SVG_DOCUMENT_BYTES),
            steps: AtomicU64::new(MAX_READING),
            nesting: AtomicUsize::new(0),
            refused: Mutex::new(None),
        }
    }

    fn refuse(&self, error: ExtensionError) {
        self.refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(error);
    }

    fn refusal(&self) -> Option<ExtensionError> {
        self.refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    fn take_text(&self, length: usize) -> Result<(), ExtensionError> {
        let left = self.text.load(Ordering::Relaxed);
        let left = left.checked_sub(length).ok_or_else(too_much_text)?;
        self.text.store(left, Ordering::Relaxed);
        Ok(())
    }
}

fn too_much_text() -> ExtensionError {
    ExtensionError::Budget(format!(
        "the SVG and the SVG images it draws are more than {MAX_SVG_DOCUMENT_BYTES} bytes of text together"
    ))
}

/// Parses SVG `text` within the remaining `reading` budget, reserving this document's nesting
/// for the SVG images it draws.
fn read_document(
    text: &[u8],
    options: &Options,
    reading: &Reading,
) -> Result<Tree, ExtensionError> {
    let malformed = |error: usvg::Error| ExtensionError::Malformed(error.to_string());
    reading.take_text(text.len())?;
    let text = std::str::from_utf8(text).map_err(|_| malformed(usvg::Error::NotAnUtf8Str))?;
    let base = reading.nesting.load(Ordering::Relaxed);
    let limit = MAX_NESTING.saturating_sub(base);
    markup::nesting(text, limit)?;
    let xml = roxmltree::Document::parse_with_options(
        text,
        roxmltree::ParsingOptions {
            allow_dtd: true,
            ..roxmltree::ParsingOptions::default()
        },
    )
    .map_err(|error| malformed(usvg::Error::ParsingFailed(error)))?;
    let steps = reading.steps.load(Ordering::Relaxed);
    let conversion = document::conversion(&xml, steps, limit)?;
    reading
        .steps
        .store(steps - conversion.steps, Ordering::Relaxed);
    reading
        .nesting
        .store(base + conversion.depth, Ordering::Relaxed);
    let tree = Tree::from_xmltree(&xml, options);
    reading.nesting.store(base, Ordering::Relaxed);
    tree.map_err(malformed)
}

/// Parses a document with only `resolver`'s resources, checks its budgets at its own size, and
/// reports what it could not draw.
fn parse(
    bytes: &[u8],
    resolver: &dyn ResourceResolver,
) -> Result<(Tree, ProbeReport), ExtensionError> {
    let registered_fonts = resolver.fonts();
    let missing_fonts = Mutex::new(Vec::<String>::new());
    let image_reports = Mutex::new(Vec::<Diagnostic>::new());
    let reading = Reading::new();
    let options = Options {
        // Filled on the first font request, so documents without text load no fonts.
        fontdb: Arc::new(usvg::fontdb::Database::new()),
        font_resolver: font_resolver(&registered_fonts, &missing_fonts),
        image_href_resolver: image_resolver(resolver, &image_reports, &reading),
        ..Options::default()
    };
    let tree = read_document(bytes, &options, &reading);
    drop(options);
    if let Some(refusal) = reading.refusal() {
        return Err(refusal);
    }
    let tree = tree?;
    let size = tree.size().to_int_size();
    check_canvas(size.width(), size.height())?;
    check_image_pixels(image_pixels(tree.root()))?;
    render_cost::check(&tree, size.width(), size.height())?;
    let mut diagnostics = Vec::new();
    let missing_fonts = missing_fonts
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    if !missing_fonts.is_empty() {
        diagnostics.push(Diagnostic::MissingFont {
            families: missing_fonts.join(", "),
        });
    }
    for diagnostic in image_reports
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner)
    {
        if !diagnostics.contains(&diagnostic) {
            diagnostics.push(diagnostic);
        }
    }
    let report = ProbeReport {
        format: Format::Svg,
        width: size.width(),
        height: size.height(),
        timing: Timing::Static,
        loop_count: None,
        items: Vec::new(),
        item: None,
        color: None,
        diagnostics,
    };
    Ok((tree, report))
}

/// Selects fonts from `registered`, loading them on the first request, and records unmatched
/// families in `missing`. usvg asks only for text it actually draws.
fn font_resolver<'a>(
    registered: &'a [Arc<[u8]>],
    missing: &'a Mutex<Vec<String>>,
) -> FontResolver<'a> {
    let select_font = FontResolver::default_font_selector();
    FontResolver {
        select_font: Box::new(move |font, fontdb| {
            if fontdb.is_empty() {
                let fontdb = Arc::make_mut(fontdb);
                for font in registered {
                    // Shared, not copied; the source keeps its fonts as long as its tree.
                    fontdb
                        .load_font_source(usvg::fontdb::Source::Binary(Arc::new(Arc::clone(font))));
                }
            }
            let found = select_font(font, fontdb);
            if found.is_none() {
                let mut missing = missing.lock().unwrap_or_else(PoisonError::into_inner);
                for family in font.families() {
                    let family = match family {
                        usvg::FontFamily::Named(name) => name.clone(),
                        generic => generic.to_string(),
                    };
                    if !missing.contains(&family) {
                        missing.push(family);
                    }
                }
            }
            found
        }),
        select_fallback: FontResolver::default_fallback_selector(),
    }
}

/// Resolves `data:` images and registered hrefs, recording what is not drawn in `reports`.
/// Nested SVG images share the outer document's `reading` budget.
fn image_resolver<'a>(
    resolver: &'a dyn ResourceResolver,
    reports: &'a Mutex<Vec<Diagnostic>>,
    reading: &'a Reading,
) -> ImageHrefResolver<'a> {
    let default_data = ImageHrefResolver::default_data_resolver();
    let report = move |diagnostic| {
        reports
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(diagnostic);
    };
    ImageHrefResolver {
        // Nested SVG documents resolve their `data:` images through this too.
        resolve_data: Box::new(move |mime, data, options| {
            // Matches usvg's own resolver: SVG type, or plain text that is not a raster image.
            let svg = mime == "image/svg+xml"
                || (mime == "text/plain" && imagesize::image_type(&data).is_err());
            if svg {
                return nested_svg(&data, options, reading);
            }
            let kind = default_data(mime, data, options)?;
            within_canvas(kind, || format!("data:{mime}"), reports)
        }),
        resolve_string: Box::new(move |href, options| {
            let unreadable = |reason: &str| {
                report(Diagnostic::UnreadableImage {
                    href: href.to_owned(),
                    reason: reason.to_owned(),
                });
                None
            };
            let Ok(bytes) = resolver.bytes(href) else {
                report(Diagnostic::MissingImage {
                    href: href.to_owned(),
                });
                return None;
            };
            let data = || Arc::new(bytes.to_vec());
            let raster = match registered_image(&bytes) {
                Some(Registered::Png) => ImageKind::PNG(data()),
                Some(Registered::Jpeg) => ImageKind::JPEG(data()),
                Some(Registered::Gif) => ImageKind::GIF(data()),
                Some(Registered::Webp) => ImageKind::WEBP(data()),
                // Like a `data:` SVG, its external images are not loaded (SVG-as-image rules).
                Some(Registered::Svg) => {
                    return nested_svg(&bytes, options, reading)
                        .or_else(|| unreadable("the SVG document does not parse"));
                }
                None => return unreadable("not a PNG, JPEG, GIF, WebP, or SVG image"),
            };
            within_canvas(raster, || href.to_owned(), reports)
        }),
    }
}

/// Reads a plain or gzipped SVG image within `reading`. Over budget refuses the outer document;
/// a parse failure just leaves the image out.
fn nested_svg(data: &[u8], options: &Options, reading: &Reading) -> Option<ImageKind> {
    let unpacked;
    let text = if data.starts_with(&[0x1f, 0x8b]) {
        // Unpacked only as far as there is text left to read.
        let left = reading.text.load(Ordering::Relaxed);
        let mut text = Vec::new();
        let read = flate2::read::GzDecoder::new(data)
            .take(u64::try_from(left).unwrap_or(u64::MAX).saturating_add(1))
            .read_to_end(&mut text);
        if read.is_err() {
            return None;
        }
        if text.len() > left {
            reading.refuse(too_much_text());
            return None;
        }
        unpacked = text;
        &unpacked[..]
    } else {
        data
    };
    let nested = Options {
        resources_dir: None,
        dpi: options.dpi,
        font_size: options.font_size,
        languages: options.languages.clone(),
        shape_rendering: options.shape_rendering,
        text_rendering: options.text_rendering,
        image_rendering: options.image_rendering,
        default_size: options.default_size,
        image_href_resolver: ImageHrefResolver {
            resolve_data: Box::new(|mime, data, nested| {
                (options.image_href_resolver.resolve_data)(mime, data, nested)
            }),
            resolve_string: Box::new(|_, _| None),
        },
        fontdb: options.fontdb.clone(),
        font_resolver: FontResolver {
            select_font: Box::new(|font, fontdb| (options.font_resolver.select_font)(font, fontdb)),
            select_fallback: Box::new(|c, used, fontdb| {
                (options.font_resolver.select_fallback)(c, used, fontdb)
            }),
        },
        ..Options::default()
    };
    match read_document(text, &nested, reading) {
        Ok(tree) => Some(ImageKind::SVG(tree)),
        Err(error @ ExtensionError::Budget(_)) => {
            reading.refuse(error);
            None
        }
        Err(_) => None,
    }
}

/// Drops and reports as missing a raster image whose declared size exceeds the canvas limit,
/// since resvg would decode it at that size on every render.
fn within_canvas(
    kind: ImageKind,
    href: impl FnOnce() -> String,
    reports: &Mutex<Vec<Diagnostic>>,
) -> Option<ImageKind> {
    let (ImageKind::JPEG(data)
    | ImageKind::PNG(data)
    | ImageKind::GIF(data)
    | ImageKind::WEBP(data)) = &kind
    else {
        return Some(kind);
    };
    // A header usvg cannot size is left out by usvg itself.
    let declared = imagesize::blob_size(data).ok()?;
    let side = |value: usize| u32::try_from(value).unwrap_or(u32::MAX);
    if check_canvas(side(declared.width), side(declared.height)).is_err() {
        reports
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Diagnostic::MissingImage { href: href() });
        return None;
    }
    Some(kind)
}

/// Decoded raster pixels `group` draws, counted per draw since resvg decodes on every draw.
fn image_pixels(group: &usvg::Group) -> u64 {
    let mut pixels = 0_u64;
    for node in group.children() {
        match node {
            Node::Group(inner) => pixels = pixels.saturating_add(image_pixels(inner)),
            Node::Image(image) => pixels = pixels.saturating_add(raster_pixels(image.kind())),
            Node::Path(_) | Node::Text(_) => {}
        }
        // Clip paths, masks, filter images, patterns, and a nested SVG's own tree.
        node.subroots(|root| pixels = pixels.saturating_add(image_pixels(root)));
    }
    pixels
}

/// Declared raster pixels; zero for a nested SVG, whose images are counted where drawn.
fn raster_pixels(kind: &ImageKind) -> u64 {
    match kind {
        ImageKind::JPEG(data)
        | ImageKind::PNG(data)
        | ImageKind::GIF(data)
        | ImageKind::WEBP(data) => imagesize::blob_size(data).map_or(0, |size| {
            u64::try_from(size.width)
                .unwrap_or(u64::MAX)
                .saturating_mul(u64::try_from(size.height).unwrap_or(u64::MAX))
        }),
        ImageKind::SVG(_) => 0,
    }
}

enum Registered {
    Png,
    Jpeg,
    Gif,
    Webp,
    /// Plain or gzipped.
    Svg,
}

fn registered_image(bytes: &[u8]) -> Option<Registered> {
    if bytes.starts_with(b"\x89PNG") {
        Some(Registered::Png)
    } else if bytes.starts_with(&[0xff, 0xd8]) {
        Some(Registered::Jpeg)
    } else if bytes.starts_with(b"GIF8") {
        Some(Registered::Gif)
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(Registered::Webp)
    } else if looks_like_svg(bytes) || bytes.starts_with(&[0x1f, 0x8b]) {
        Some(Registered::Svg)
    } else {
        None
    }
}

struct SvgSource {
    tree: Tree,
    report: ProbeReport,
    /// The last rasterization, reused while the size holds.
    last: Option<VisualFrame>,
}

impl VisualSource for SvgSource {
    fn report(&self) -> &ProbeReport {
        &self.report
    }

    fn render(&mut self, _time: MediaTime, fit: (u32, u32)) -> Result<VisualFrame, ExtensionError> {
        let size = self.tree.size();
        let (width, height) = fitted(size.width(), size.height(), fit);
        if let Some(last) = self
            .last
            .as_ref()
            .filter(|last| (last.width, last.height) == (width, height))
        {
            return Ok(last.clone());
        }
        render_cost::check(&self.tree, width, height)?;
        let mut pixmap = Pixmap::new(width, height).ok_or_else(|| {
            ExtensionError::Budget(format!("cannot rasterize at {width}x{height}"))
        })?;
        // Sizes are within MAX_CANVAS_SIDE, which f32 holds exactly.
        #[allow(clippy::cast_precision_loss)]
        let transform =
            Transform::from_scale(width as f32 / size.width(), height as f32 / size.height());
        resvg::render(&self.tree, transform, &mut pixmap.as_mut());
        let frame = VisualFrame {
            width,
            height,
            pixels: FramePixels::Rgba8(pixmap.take().into()),
            index: 0,
            start: MediaTime::from_ticks(i64::MIN),
            end: None,
            diagnostics: Vec::new(),
        };
        self.last = Some(frame.clone());
        Ok(frame)
    }

    fn resident_bytes(&self) -> usize {
        self.last.as_ref().map_or(0, |last| last.pixels.byte_len())
    }
}

/// The largest whole-pixel size that fits in `fit` keeping aspect; the own size if `fit` is empty.
// Every size is positive and within MAX_CANVAS_SIDE, which f32 holds exactly.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn fitted(width: f32, height: f32, fit: (u32, u32)) -> (u32, u32) {
    if fit.0 == 0 || fit.1 == 0 {
        return (
            width.round().max(1.0) as u32,
            height.round().max(1.0) as u32,
        );
    }
    let scale = (fit.0 as f32 / width).min(fit.1 as f32 / height);
    (
        (width * scale).round().clamp(1.0, fit.0 as f32) as u32,
        (height * scale).round().clamp(1.0, fit.1 as f32) as u32,
    )
}

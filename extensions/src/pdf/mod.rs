//! PDF pages, converted to SVG by hayro-svg and drawn by the SVG provider.
//!
//! A font the PDF does not embed is drawn with hayro's bundled standard 14 fonts, as viewers do,
//! and reported unless it is one of them. A page is checked by [`scan`] before hayro reads it,
//! and its SVG markup and embedded image data are bounded separately before usvg parses them.

mod scan;

use std::sync::{Arc, Mutex, PoisonError};

use alpha2_model::{MAX_PDF_PAGES, MAX_SVG_DOCUMENT_BYTES, ProviderSource, Rgba};
use hayro_svg::hayro_interpret::font::FontQuery;
use hayro_svg::hayro_interpret::{InterpreterSettings, InterpreterWarning};
use hayro_svg::hayro_syntax::{DecryptionError, LoadPdfError, Pdf};
use hayro_svg::{RenderCache, SvgRenderSettings};

use crate::api::{
    Diagnostic, ExtensionError, Format, NoResources, ProbeReport, ResourceResolver, Timing,
    VisualProvider, VisualSource,
};
use crate::budget::{check_canvas, check_document_size};
use crate::svg::open_document_within;

/// Pixels per PDF point, the 96 dpi CSS and usvg use.
const PIXELS_PER_POINT: f32 = 4.0 / 3.0;

/// Most bytes of `data:` URLs (the page's images) one page's SVG may hold. Its other markup is
/// held to [`MAX_SVG_DOCUMENT_BYTES`] like any SVG document.
pub(crate) const MAX_IMAGE_DATA_BYTES: usize = 256 * 1024 * 1024;

/// Reads pages of PDF documents.
pub struct PdfProvider;

impl VisualProvider for PdfProvider {
    fn name(&self) -> &'static str {
        "pdf"
    }

    fn probe(&self, bytes: &[u8]) -> Option<Result<ProbeReport, ExtensionError>> {
        if !looks_like_pdf(bytes) {
            return None;
        }
        Some(load(bytes.to_vec().into()).and_then(|pdf| {
            let first = &pdf.pages()[0];
            let (width, height) = page_size(first.render_dimensions());
            check_canvas(width, height)?;
            Ok(ProbeReport {
                format: Format::Pdf,
                width,
                height,
                timing: Timing::Static,
                loop_count: None,
                items: page_numbers(pdf.pages().len()),
                item: Some("1".into()),
                color: None,
                diagnostics: Vec::new(),
            })
        }))
    }

    fn open(
        &self,
        bytes: Arc<[u8]>,
        source: &ProviderSource,
        _resolver: &dyn ResourceResolver,
    ) -> Result<Box<dyn VisualSource>, ExtensionError> {
        if !looks_like_pdf(&bytes) {
            return Err(ExtensionError::Mismatch("not a PDF document".into()));
        }
        let ProviderSource::Pdf { page, background } = source else {
            return Err(ExtensionError::Mismatch("not a PDF source".into()));
        };
        Ok(Box::new(open_page(bytes, *page, *background)?))
    }
}

/// Whether `%PDF-` starts within the first 1024 bytes, as PDF readers accept.
fn looks_like_pdf(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(1024)]
        .windows(5)
        .any(|window| window == b"%PDF-")
}

/// Parses a document within its size and page limits.
fn load(bytes: Arc<[u8]>) -> Result<Pdf, ExtensionError> {
    check_document_size(&any_page(), &bytes, bytes.len() as u64)?;
    let pdf = Pdf::new(Arc::new(bytes)).map_err(|error| match error {
        LoadPdfError::Decryption(DecryptionError::PasswordProtected) => {
            ExtensionError::Unsupported("the PDF is password-protected".into())
        }
        LoadPdfError::Decryption(error) => {
            ExtensionError::Unsupported(format!("the PDF's encryption is not read: {error:?}"))
        }
        LoadPdfError::Invalid => ExtensionError::Malformed("not a readable PDF document".into()),
    })?;
    let pages = pdf.pages().len();
    if pages == 0 {
        return Err(ExtensionError::Malformed("the PDF has no pages".into()));
    }
    if pages > MAX_PDF_PAGES as usize {
        return Err(ExtensionError::Budget(format!(
            "the PDF has {pages} pages, more than the {MAX_PDF_PAGES} a provider reads"
        )));
    }
    Ok(pdf)
}

fn any_page() -> ProviderSource {
    ProviderSource::Pdf {
        page: 1,
        background: Rgba::new(1.0, 1.0, 1.0, 1.0),
    }
}

/// A page's size in pixels, as the SVG source it opens as rounds it.
// An absurd page size saturates, and `check_canvas` then refuses it.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn page_size((width, height): (f32, f32)) -> (u32, u32) {
    let side = |points: f32| (points * PIXELS_PER_POINT).round().max(1.0) as u32;
    (side(width), side(height))
}

fn page_numbers(pages: usize) -> Vec<String> {
    (1..=pages).map(|page| page.to_string()).collect()
}

/// Converts page `number` (from 1) and opens it as SVG over `background`. The document is
/// dropped once the page is converted.
fn open_page(
    bytes: Arc<[u8]>,
    number: u32,
    background: Rgba,
) -> Result<crate::svg::SvgSource, ExtensionError> {
    let pdf = load(bytes)?;
    let pages = pdf.pages();
    let page_count = pages.len();
    let page = usize::try_from(number)
        .ok()
        .and_then(|number| number.checked_sub(1))
        .and_then(|index| pages.get(index))
        .ok_or_else(|| {
            ExtensionError::Malformed(format!("page {number} of {} does not exist", pages.len()))
        })?;
    let (width, height) = page_size(page.render_dimensions());
    check_canvas(width, height)?;
    let substituted = scan::check_page(page)?;
    let missing_fonts = Arc::new(Mutex::new(substituted));
    let warnings = Arc::new(Mutex::new(Vec::<InterpreterWarning>::new()));
    let settings = InterpreterSettings {
        font_resolver: {
            let missing = Arc::clone(&missing_fonts);
            Arc::new(move |query| match query {
                FontQuery::Standard(font) => Some(font.get_font_data()),
                FontQuery::Fallback(query) => {
                    let name = query
                        .post_script_name
                        .clone()
                        .or_else(|| query.font_name.clone())
                        .or_else(|| query.font_family.clone())
                        .unwrap_or_else(|| "an unnamed font".into());
                    let mut missing = missing.lock().unwrap_or_else(PoisonError::into_inner);
                    if !missing.contains(&name) {
                        missing.push(name);
                    }
                    Some(query.pick_standard_font().get_font_data())
                }
            })
        },
        warning_sink: {
            let warnings = Arc::clone(&warnings);
            Arc::new(move |warning| {
                warnings
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(warning);
            })
        },
        ..InterpreterSettings::default()
    };
    let svg = hayro_svg::convert(
        page,
        &RenderCache::new(),
        &settings,
        &SvgRenderSettings::default(),
    );
    drop(settings);
    drop(pdf);
    check_svg(&svg)?;
    let mut source = open_document_within(svg.as_bytes(), &NoResources, svg.len())?
        .scaled(PIXELS_PER_POINT)?
        .with_background(background);
    let report = source.report_mut();
    report.format = Format::Pdf;
    report.items = page_numbers(page_count);
    report.item = Some(number.to_string());
    let missing_fonts = missing_fonts
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let warnings = warnings
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    report
        .diagnostics
        .extend(diagnostics(number, &missing_fonts, &warnings));
    Ok(source)
}

/// Bounds the converted page's markup and its images' `data:` URLs separately, since images
/// cost usvg about their size while markup costs many times its size.
fn check_svg(svg: &str) -> Result<(), ExtensionError> {
    let mut data = 0;
    let mut rest = svg;
    while let Some(at) = rest.find("\"data:") {
        let url = &rest[at + 1..];
        let end = url.find('"').unwrap_or(url.len());
        data += end;
        rest = &url[end..];
    }
    if data > MAX_IMAGE_DATA_BYTES {
        return Err(ExtensionError::Budget(format!(
            "the page's images are {data} bytes in SVG, more than the {MAX_IMAGE_DATA_BYTES} a page may hold"
        )));
    }
    let markup = svg.len() - data;
    if markup > MAX_SVG_DOCUMENT_BYTES {
        return Err(ExtensionError::Budget(format!(
            "the page is {markup} bytes of SVG besides its images, more than the {MAX_SVG_DOCUMENT_BYTES} a document may be"
        )));
    }
    Ok(())
}

fn diagnostics(
    page: u32,
    missing_fonts: &[String],
    warnings: &[InterpreterWarning],
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    if !missing_fonts.is_empty() {
        diagnostics.push(Diagnostic::SubstitutedFont {
            families: missing_fonts.join(", "),
        });
    }
    let count = |wanted: fn(&InterpreterWarning) -> bool| {
        warnings.iter().filter(|warning| wanted(warning)).count()
    };
    let images = count(|warning| matches!(warning, InterpreterWarning::ImageDecodeFailure));
    if images > 0 {
        diagnostics.push(Diagnostic::UnreadableImage {
            href: format!("page {page}"),
            reason: format!("{images} images do not decode"),
        });
    }
    for (feature, found) in [
        (
            "an annotation appearance that does not load",
            count(|warning| matches!(warning, InterpreterWarning::UnresolvedAnnotationAppearance)),
        ),
        (
            "an embedded font that does not load",
            count(|warning| matches!(warning, InterpreterWarning::UnsupportedFont)),
        ),
    ] {
        if found > 0 {
            diagnostics.push(Diagnostic::UnsupportedFeature {
                feature: feature.into(),
                path: format!("page {page}"),
                count: found,
            });
        }
    }
    diagnostics
}

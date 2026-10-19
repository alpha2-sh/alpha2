//! Lottie JSON and dotLottie: velato's animation model, rasterized with
//! tiny-skia, the rasterizer SVG uses, at the size it is shown.
//!
//! An animation is evaluated at its own whole frames: a source time shows
//! the frame it falls in, as an image sequence's does. velato reads only
//! the document [`normalize`] makes of the file: what the backend does not
//! draw is reported with where it appears and left out, and what would stop
//! it (a panic, unbounded recursion or expansion) is refused before it is
//! read.

mod device;
mod normalize;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::rc::Rc;
use std::sync::Arc;

use alpha2_time::{FrameRate, MediaTime};
use base64::Engine as _;
use kurbo::{Affine, PathEl, Shape};
use peniko::{BlendMode as PenikoBlend, Brush, Compose, Extend, GradientKind, Mix};
use resvg::tiny_skia::{
    self, BlendMode, Color, FillRule, GradientStop, IntRect, IntSize, LineCap, LineJoin,
    LinearGradient, Mask, Paint, PathBuilder, PathStroker, Pixmap, PixmapPaint, Point,
    RadialGradient, Shader, SpreadMode, Stroke, StrokeDash, SweepGradient, Transform,
};
use serde_json::Value;
use velato::model::ImageAsset;
use velato::{Composition, RenderSink, Renderer};

use alpha2_model::{MAX_LOTTIE_DOCUMENT_BYTES, ProviderSource};

use crate::api::{
    Diagnostic, ExtensionError, Format, FramePixels, ProbeReport, ResourceResolver, Timing,
    VisualFrame, VisualProvider, VisualSource,
};
use crate::budget::{check_canvas, check_document_size, check_image_pixels};

/// Most frames an animation may span.
const MAX_LOTTIE_FRAMES: f64 = 100_000.0;
/// Most bytes a dotLottie package unpacks to, all its files together. The
/// package file itself is read up to
/// [`alpha2_model::MAX_PROVIDER_DOCUMENT_BYTES`], and its manifest and the
/// animation shown, like a Lottie JSON file, up to
/// [`MAX_LOTTIE_DOCUMENT_BYTES`] each.
const MAX_LOTTIE_BYTES: u64 = 256 * 1024 * 1024;
/// Most files a dotLottie package holds.
const MAX_DOTLOTTIE_FILES: usize = 1_024;
/// Most bytes a dotLottie package's central directory, which lists its
/// files, takes: a kibibyte for each file it may hold.
const MAX_DOTLOTTIE_DIRECTORY_BYTES: u64 = 1_024 * 1_024;

/// Reads Lottie JSON and dotLottie packages.
pub struct LottieProvider;

impl VisualProvider for LottieProvider {
    fn name(&self) -> &'static str {
        "lottie"
    }

    fn probe(&self, bytes: &[u8]) -> Option<Result<ProbeReport, ExtensionError>> {
        let size = check_document_size(
            &ProviderSource::Lottie { animation: None },
            bytes,
            bytes.len() as u64,
        );
        if is_zip(bytes) {
            return Some(
                size.and_then(|()| Package::read(bytes))
                    .and_then(|package| package.probe()),
            );
        }
        if is_json_object(bytes)
            && let Err(error) = size
        {
            return Some(Err(error));
        }
        let json = parse_lottie(bytes)?;
        Some(inspect(json, Format::Lottie).map(|(report, _)| report))
    }

    fn open(
        &self,
        bytes: Arc<[u8]>,
        source: &ProviderSource,
        resolver: &dyn ResourceResolver,
    ) -> Result<Box<dyn VisualSource>, ExtensionError> {
        Ok(Box::new(LottieSource::open(&bytes, source, resolver)?))
    }
}

impl LottieSource {
    fn open(
        bytes: &[u8],
        source: &ProviderSource,
        resolver: &dyn ResourceResolver,
    ) -> Result<Self, ExtensionError> {
        let item = match source {
            ProviderSource::Lottie { animation } => animation.as_deref(),
            _ => return Err(ExtensionError::Mismatch("not a Lottie source".into())),
        };
        check_document_size(source, bytes, bytes.len() as u64)?;
        let (json, package, format) = if is_zip(bytes) {
            let package = Package::read(bytes)?;
            (package.animation(item)?, Some(package), Format::DotLottie)
        } else {
            if item.is_some() {
                return Err(ExtensionError::Mismatch(
                    "a Lottie JSON file holds one animation".into(),
                ));
            }
            let json = parse_lottie(bytes)
                .ok_or_else(|| ExtensionError::Mismatch("not a Lottie animation".into()))?;
            (json, None, Format::Lottie)
        };
        let (mut report, sanitized) = inspect(json, format)?;
        if let Some(package) = &package {
            report.items.clone_from(&package.ids);
            report.item = Some(item.unwrap_or(&package.ids[0]).to_owned());
        }
        // The parsed document takes many times the file's size, and velato's parse as much again,
        // so it is dropped before velato reads its text.
        let text = serde_json::to_vec(&sanitized)
            .map_err(|error| ExtensionError::Malformed(error.to_string()))?;
        drop(sanitized);
        let composition = Composition::from_slice(&text)
            .map_err(|error| ExtensionError::Malformed(format!("{error:?}")))?;
        drop(text);
        // The assets' decoded size is checked from their headers before any is decoded; one larger
        // than a canvas is reported missing below and does not count.
        let assets = composition
            .images
            .iter()
            .map(|(id, asset)| Ok((id, asset, image_bytes(asset, package.as_ref(), resolver)?)))
            .collect::<Result<Vec<_>, ExtensionError>>()?;
        check_image_pixels(
            assets
                .iter()
                .filter_map(|(_, _, bytes)| crate::raster::png_size(bytes.as_deref()?))
                .filter(|(width, height)| check_canvas(*width, *height).is_ok())
                .map(|(width, height)| u64::from(width) * u64::from(height))
                .fold(0, u64::saturating_add),
        )?;
        let mut images = HashMap::new();
        for (id, asset, bytes) in assets {
            match bytes.and_then(|bytes| decode_image(&bytes)) {
                Some(pixmap) => {
                    images.insert(id.clone(), pixmap);
                }
                None => report.diagnostics.push(Diagnostic::MissingImage {
                    href: format!(
                        "{}{}",
                        asset.directory.clone().unwrap_or_default(),
                        short(&asset.file_name)
                    ),
                }),
            }
        }
        Ok(Self {
            composition,
            renderer: Renderer::new(),
            images,
            report,
            last: None,
        })
    }
}

fn is_zip(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04")
}

/// Whether the bytes start as a JSON object does, as a Lottie document must.
fn is_json_object(bytes: &[u8]) -> bool {
    bytes.iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'{')
}

/// A Lottie document: JSON whose root has a canvas, a rate, a frame span,
/// and layers.
fn parse_lottie(bytes: &[u8]) -> Option<Value> {
    if !is_json_object(bytes) {
        return None;
    }
    let json: Value = serde_json::from_slice(bytes).ok()?;
    let root = json.as_object()?;
    ["w", "h", "fr", "ip", "op"]
        .iter()
        .all(|key| root.get(*key).is_some_and(Value::is_number))
        .then_some(())
        .filter(|()| root.get("layers").is_some_and(Value::is_array))
        .map(|()| json)
}

/// Refuses a package whose central directory lists more files than a
/// package may hold, or takes more bytes than such a directory needs, from
/// what its end record says, before the directory is read: the ZIP reader
/// indexes every file a directory lists, which for an archive of empty
/// files is several times its size.
///
/// The end record read is the one the ZIP reader starts from: the last in
/// the file that fits in it, with its comment, within the last 22 and
/// 65,535 bytes the record and its comment take. A ZIP64 record, which the
/// end record points to when its own fields overflow, is found as the ZIP
/// reader finds it, and a package that points to one it does not have is
/// malformed. When the reader cannot read the directory a record
/// describes, it may look for an earlier record; a package that holds too
/// many files through one is still refused once it is read.
fn check_directory(bytes: &[u8]) -> Result<(), ExtensionError> {
    const END: &[u8] = b"PK\x05\x06";
    const END64_LOCATOR: &[u8] = b"PK\x06\x07";
    const END64: &[u8] = b"PK\x06\x06";
    let malformed = |what: &str| ExtensionError::Malformed(format!("dotLottie: {what}"));
    let field = |at: usize, size: usize| {
        bytes.get(at..at + size).map(|field| {
            field
                .iter()
                .rev()
                .fold(0_u64, |value, byte| value << 8 | u64::from(*byte))
        })
    };
    let from = bytes.len().saturating_sub(22 + usize::from(u16::MAX));
    let end = (from..bytes.len().saturating_sub(21))
        .rev()
        .find(|&at| {
            bytes[at..].starts_with(END)
                && field(at + 20, 2)
                    .is_some_and(|comment| at as u64 + 22 + comment <= bytes.len() as u64)
        })
        .ok_or_else(|| malformed("no end of central directory record"))?;
    let (mut files, mut size) = (field(end + 10, 2), field(end + 12, 4));
    let zip64 =
        files == Some(u64::from(u16::MAX)) || field(end + 16, 4) == Some(u64::from(u32::MAX));
    if zip64
        && let Some(locator) = end.checked_sub(20)
        && bytes[locator..].starts_with(END64_LOCATOR)
    {
        let at = field(locator + 8, 8)
            .and_then(|at| usize::try_from(at).ok())
            .filter(|&at| at < locator)
            .and_then(|start| {
                bytes[start..locator]
                    .windows(END64.len())
                    .position(|window| window == END64)
                    .map(|found| start + found)
            })
            .ok_or_else(|| malformed("no ZIP64 end of central directory record"))?;
        (files, size) = (field(at + 32, 8), field(at + 40, 8));
    }
    let (Some(files), Some(size)) = (files, size) else {
        return Err(malformed("the end of central directory record is cut off"));
    };
    if files > MAX_DOTLOTTIE_FILES as u64 {
        return Err(ExtensionError::Budget(format!(
            "the dotLottie package holds more than {MAX_DOTLOTTIE_FILES} files"
        )));
    }
    if size > MAX_DOTLOTTIE_DIRECTORY_BYTES {
        return Err(ExtensionError::Budget(format!(
            "the dotLottie package's central directory takes more than \
             {MAX_DOTLOTTIE_DIRECTORY_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Bytes the ZIP reader may read while it looks for a package's directory,
/// besides twice the package: the last 64 KiB its end record is in and the
/// directory itself, many times over.
const MAX_DIRECTORY_SEARCH: u64 = 4 * 1_024 * 1_024;

/// A package's bytes as the ZIP reader reads them, within what is `left`.
///
/// The ZIP reader looks for the end record from the end, and when the
/// directory a record describes does not read, tries the record before it.
/// zip 2.4 searched forward for the directory each time, so a package of
/// nothing but such records cost the square of its size (a 0.7 MB one,
/// 264 ms; 256 MiB, hours); zip 9 remembers what it found, but the bound
/// here does not depend on how the reader searches. Reading the right record
/// and its directory takes one pass from the end; once the bytes left run
/// out, reading fails, which ends the search.
struct Metered<'a> {
    bytes: Cursor<&'a [u8]>,
    left: Rc<Cell<u64>>,
}

impl Read for Metered<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.left.get();
        if left == 0 {
            return Err(std::io::Error::other("the search for a directory is over"));
        }
        let take = usize::try_from(left).map_or(buf.len(), |left| left.min(buf.len()));
        let read = self.bytes.read(&mut buf[..take])?;
        // Every call costs something, a read of nothing too.
        self.left.set(left.saturating_sub((read as u64).max(1)));
        Ok(read)
    }
}

impl Seek for Metered<'_> {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        self.bytes.seek(to)
    }
}

/// A dotLottie package: its animations by ID, and its files, each unpacked
/// only when it is read: the manifest, the animation shown, and that
/// animation's images, never the package's other files.
struct Package<'a> {
    ids: Vec<String>,
    archive: RefCell<zip::ZipArchive<Metered<'a>>>,
    /// Each file's index in the archive, by its name without a leading `/`.
    names: HashMap<String, usize>,
    /// The bytes unpacked so far, all files together.
    unpacked: Cell<u64>,
}

impl<'a> Package<'a> {
    fn read(bytes: &'a [u8]) -> Result<Self, ExtensionError> {
        check_directory(bytes)?;
        let left = Rc::new(Cell::new(
            MAX_DIRECTORY_SEARCH.saturating_add((bytes.len() as u64).saturating_mul(2)),
        ));
        let metered = Metered {
            bytes: Cursor::new(bytes),
            left: Rc::clone(&left),
        };
        let archive = zip::ZipArchive::new(metered).map_err(|error| {
            if left.get() == 0 {
                ExtensionError::Malformed(
                    "dotLottie: no end of central directory record describes a directory".into(),
                )
            } else {
                ExtensionError::Malformed(format!("dotLottie: {error}"))
            }
        })?;
        // Its files are each held to what they unpack (see `Self::file`).
        left.set(u64::MAX);
        if archive.len() > MAX_DOTLOTTIE_FILES {
            return Err(ExtensionError::Budget(format!(
                "the dotLottie package holds more than {MAX_DOTLOTTIE_FILES} files"
            )));
        }
        let mut names = HashMap::new();
        for (index, name) in archive.file_names().enumerate() {
            let name =
                name.map_err(|error| ExtensionError::Malformed(format!("dotLottie: {error}")))?;
            if !name.ends_with('/') {
                names.insert(name.trim_start_matches('/').to_owned(), index);
            }
        }
        let package = Self {
            ids: Vec::new(),
            archive: RefCell::new(archive),
            names,
            unpacked: Cell::new(0),
        };
        let manifest: Value = serde_json::from_slice(
            &package
                .file("manifest.json", Some(MAX_LOTTIE_DOCUMENT_BYTES))?
                .ok_or_else(|| ExtensionError::Malformed("dotLottie: no manifest.json".into()))?,
        )
        .map_err(|error| ExtensionError::Malformed(format!("dotLottie: {error}")))?;
        let ids = manifest
            .get("animations")
            .and_then(Value::as_array)
            .map(|animations| {
                animations
                    .iter()
                    .filter_map(|animation| animation.get("id")?.as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if ids.is_empty() {
            return Err(ExtensionError::Malformed(
                "dotLottie: the manifest lists no animation".into(),
            ));
        }
        Ok(Self { ids, ..package })
    }

    /// A file's bytes, unpacked now; `None` when the package has no such
    /// file. Every file read counts toward one budget, and a JSON document,
    /// parsed whole, is also within `document` bytes on its own.
    ///
    /// The size a package declares is the writer's word: a large one is
    /// refused before unpacking, but the budgets count the bytes unpacked.
    /// A file is read to its end, where its checksum is checked, or past
    /// what is left of a budget, and refused. One that unpacks past the size
    /// it declares is malformed: the ZIP reader stops there and fails.
    fn file(&self, name: &str, document: Option<usize>) -> Result<Option<Vec<u8>>, ExtensionError> {
        let Some(&index) = self.names.get(name) else {
            return Ok(None);
        };
        let malformed = |error: &dyn std::fmt::Display| {
            ExtensionError::Malformed(format!("dotLottie: {error}"))
        };
        let total = self.unpacked.get();
        let left = MAX_LOTTIE_BYTES - total;
        let (within, too_large) = match document {
            Some(most) if (most as u64) < left => (
                most as u64,
                format!(
                    "the dotLottie package's {name} unpacks to more than the {most} bytes a \
                     provider reads"
                ),
            ),
            _ => (left, "the dotLottie package unpacks past 256 MB".to_owned()),
        };
        let too_large = || ExtensionError::Budget(too_large.clone());
        let mut archive = self.archive.borrow_mut();
        let mut file = archive.by_index(index).map_err(|error| malformed(&error))?;
        if file.size() > within {
            return Err(too_large());
        }
        let mut contents = Vec::new();
        let read = file
            .by_ref()
            .take(within + 1)
            .read_to_end(&mut contents)
            .map_err(|error| malformed(&error))? as u64;
        if read > within {
            return Err(too_large());
        }
        self.unpacked.set(total + read);
        Ok(Some(contents))
    }

    /// An animation's JSON: version 2 packages keep them under `a/`,
    /// version 1 under `animations/`.
    fn animation(&self, id: Option<&str>) -> Result<Value, ExtensionError> {
        let id = match id {
            Some(id) if self.ids.iter().any(|known| known == id) => id,
            Some(id) => {
                return Err(ExtensionError::Mismatch(format!(
                    "the dotLottie package has no animation {id}"
                )));
            }
            None => &self.ids[0],
        };
        let bytes = self
            .first_file(
                &[format!("a/{id}.json"), format!("animations/{id}.json")],
                Some(MAX_LOTTIE_DOCUMENT_BYTES),
            )?
            .ok_or_else(|| {
                ExtensionError::Malformed(format!("dotLottie: animation {id} has no file"))
            })?;
        parse_lottie(&bytes).ok_or_else(|| {
            ExtensionError::Malformed(format!("dotLottie: animation {id} is not Lottie"))
        })
    }

    /// The package as its first animation that opens describes it, with
    /// those before it that do not reported: a source can choose any that
    /// opens. Refused, for its first, when none does.
    fn probe(&self) -> Result<ProbeReport, ExtensionError> {
        let mut unreadable = Vec::new();
        let mut first = None;
        for id in &self.ids {
            match self
                .animation(Some(id))
                .and_then(|json| inspect(json, Format::DotLottie))
            {
                Ok((report, _)) => {
                    unreadable.extend(report.diagnostics);
                    return Ok(ProbeReport {
                        items: self.ids.clone(),
                        item: Some(id.clone()),
                        diagnostics: unreadable,
                        ..report
                    });
                }
                Err(error) => {
                    unreadable.push(Diagnostic::UnreadableItem {
                        item: id.clone(),
                        reason: error.to_string(),
                    });
                    first.get_or_insert(error);
                }
            }
        }
        Err(first.unwrap_or_else(|| {
            ExtensionError::Malformed("dotLottie: the manifest lists no animation".into())
        }))
    }

    /// An image the package holds: by its path, or under `i/` (version 2)
    /// or `images/` (version 1).
    fn image(&self, name: &str) -> Result<Option<Vec<u8>>, ExtensionError> {
        let name = name.trim_start_matches('/');
        self.first_file(
            &[
                name.to_owned(),
                format!("i/{name}"),
                format!("images/{name}"),
            ],
            None,
        )
    }

    /// The first of `paths` the package holds, unpacked, as [`Self::file`]
    /// unpacks it.
    fn first_file(
        &self,
        paths: &[String],
        document: Option<usize>,
    ) -> Result<Option<Vec<u8>>, ExtensionError> {
        for path in paths {
            if let Some(bytes) = self.file(path, document)? {
                return Ok(Some(bytes));
            }
        }
        Ok(None)
    }
}

/// What an animation is, and its JSON with what the backend cannot read
/// either refused or reported.
fn inspect(json: Value, format: Format) -> Result<(ProbeReport, Value), ExtensionError> {
    let root = json
        .as_object()
        .ok_or_else(|| ExtensionError::Malformed("the animation is not an object".into()))?;
    let number = |key: &str| root.get(key).and_then(Value::as_f64).unwrap_or(0.0);
    let (width, height) = (number("w"), number("h"));
    let (rate, first, last) = (number("fr"), number("ip"), number("op"));
    if !(width >= 1.0 && height >= 1.0 && rate > 0.0 && last > first)
        || last - first > MAX_LOTTIE_FRAMES
    {
        return Err(ExtensionError::Malformed(
            "the animation has no canvas, rate, or frames".into(),
        ));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (width, height, frames) = (
        width.round() as u32,
        height.round() as u32,
        (last - first).ceil() as usize,
    );
    check_canvas(width, height)?;
    let (diagnostics, sanitized) = normalize::normalize(json)?;
    Ok((
        ProbeReport {
            format,
            width,
            height,
            timing: Timing::Rate {
                rate: rational_rate(rate).ok_or_else(|| {
                    ExtensionError::Malformed(format!("the frame rate {rate} is not usable"))
                })?,
                frames,
            },
            loop_count: None,
            items: Vec::new(),
            item: None,
            color: None,
            diagnostics,
        },
        sanitized,
    ))
}

/// A Lottie rate as the rational a stream declares: NTSC and whole rates
/// exactly, others to a thousandth.
fn rational_rate(rate: f64) -> Option<FrameRate> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (num, den) = [24_u32, 30, 48, 60, 120]
        .into_iter()
        .find(|base| (rate - f64::from(*base) * 1_000.0 / 1_001.0).abs() < 0.005)
        .map(|base| (base * 1_000, 1_001))
        .or_else(|| ((rate - rate.round()).abs() < 0.005).then(|| (rate.round() as u32, 1)))
        .unwrap_or_else(|| ((rate * 1_000.0).round() as u32, 1_000));
    FrameRate::new(num, den).ok()
}

/// An image asset's bytes: a data URL, a file in the package, or a resource;
/// `None` when it is none of them. A package file that cannot be unpacked
/// fails the package, as it would have had it been read whole.
fn image_bytes(
    asset: &ImageAsset,
    package: Option<&Package>,
    resolver: &dyn ResourceResolver,
) -> Result<Option<Vec<u8>>, ExtensionError> {
    if let Some(data) = asset.file_name.strip_prefix("data:") {
        return Ok(data.split_once(";base64,").and_then(|(_, encoded)| {
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()
        }));
    }
    let name = format!(
        "{}{}",
        asset.directory.clone().unwrap_or_default(),
        asset.file_name
    );
    if let Some(package) = package {
        for candidate in [&name, &asset.file_name] {
            if let Some(bytes) = package.image(candidate)? {
                return Ok(Some(bytes));
            }
        }
    }
    Ok(resolver.bytes(&name).ok().map(|bytes| bytes.to_vec()))
}

/// A PNG image asset as a premultiplied pixmap. Other formats are reported
/// missing.
fn decode_image(bytes: &[u8]) -> Option<Pixmap> {
    let (width, height, mut pixels) = crate::raster::decode_png(bytes).ok()?;
    crate::api::premultiply(&mut pixels);
    Pixmap::from_vec(pixels, IntSize::from_wh(width, height)?)
}

fn short(name: &str) -> String {
    if name.starts_with("data:") {
        "an embedded image".into()
    } else {
        name.to_owned()
    }
}

/// An open Lottie animation, with its PNG image assets decoded once at open.
struct LottieSource {
    composition: Composition,
    renderer: Renderer,
    images: HashMap<String, Pixmap>,
    report: ProbeReport,
    last: Option<VisualFrame>,
}

impl VisualSource for LottieSource {
    fn report(&self) -> &ProbeReport {
        &self.report
    }

    fn render(&mut self, time: MediaTime, fit: (u32, u32)) -> Result<VisualFrame, ExtensionError> {
        let (index, start, end) = self.report.timing.frame_at(time);
        let (width, height) = fitted(self.report.width, self.report.height, fit);
        if let Some(last) = self
            .last
            .as_ref()
            .filter(|last| last.index == index && (last.width, last.height) == (width, height))
        {
            return Ok(last.clone());
        }
        let mut sink = SkiaSink::new(width, height, &self.images)?;
        #[allow(clippy::cast_precision_loss)]
        let frame = self.composition.frames.start + index as f64;
        let transform = Affine::scale_non_uniform(
            f64::from(width) / f64::from(self.report.width),
            f64::from(height) / f64::from(self.report.height),
        );
        self.renderer
            .append(&self.composition, frame, transform, 1.0, &mut sink);
        let frame = VisualFrame {
            width,
            height,
            pixels: FramePixels::Rgba8(sink.finish().into()),
            index,
            start,
            end,
            diagnostics: Vec::new(),
        };
        self.last = Some(frame.clone());
        Ok(frame)
    }

    fn resident_bytes(&self) -> usize {
        self.images
            .values()
            .map(|image| image.data().len())
            .sum::<usize>()
            + self.last.as_ref().map_or(0, |last| last.pixels.byte_len())
    }
}

/// The largest whole-pixel size of the canvas that fits in `fit`, keeping
/// its shape; the canvas itself when `fit` is empty.
fn fitted(width: u32, height: u32, fit: (u32, u32)) -> (u32, u32) {
    if fit.0 == 0 || fit.1 == 0 {
        return (width, height);
    }
    let scale = (f64::from(fit.0) / f64::from(width)).min(f64::from(fit.1) / f64::from(height));
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (
        (f64::from(width) * scale)
            .round()
            .clamp(1.0, f64::from(fit.0)) as u32,
        (f64::from(height) * scale)
            .round()
            .clamp(1.0, f64::from(fit.1)) as u32,
    )
}

/// velato's scene, drawn with tiny-skia: layers are pixmaps composited on
/// pop with their blend mode, opacity, and clip.
///
/// A layer is composited only where its clip lets anything through and,
/// when it blends normally, only where anything was drawn on it, so a mask,
/// matte, image clip, or fading repeater copy costs what it covers, not the
/// frame: elsewhere the composite would leave every pixel as it was. A
/// layer that clips nothing, blends normally, and is opaque, pushed on a
/// layer nothing has been drawn on yet (velato's clip to the animation's
/// own canvas, every frame), is drawn straight onto that layer: compositing
/// it there would give the same pixels.
struct SkiaSink<'a> {
    width: u32,
    height: u32,
    layers: Vec<Layer>,
    images: &'a HashMap<String, Pixmap>,
    /// The mask of a clip that covers the canvas, all of it 255: what
    /// rasterizing such a clip makes, made once a frame.
    covering: Option<Rc<Mask>>,
    /// Layers popped, cleared, for the next pushed to draw on.
    pool: Vec<Pixmap>,
    /// Whether every layer is composited whole, in tests (see [`Self::whole`]).
    #[cfg(test)]
    whole: bool,
    /// Pixels composited, in tests.
    #[cfg(test)]
    composited: u64,
    /// Layer pixmaps allocated, in tests.
    #[cfg(test)]
    allocated: usize,
}

/// One pushed layer of a [`SkiaSink`].
struct Layer {
    /// `None` for a layer drawn straight onto the one below.
    pixmap: Option<Pixmap>,
    blend: BlendMode,
    alpha: f32,
    clip: Option<Rc<Mask>>,
    /// The part of the canvas the clip lets through; `None` when it lets
    /// nothing through.
    bounds: Option<IntRect>,
    /// The part of it anything has been drawn on; `None` while nothing has.
    dirty: Option<IntRect>,
}

impl<'a> SkiaSink<'a> {
    fn new(
        width: u32,
        height: u32,
        images: &'a HashMap<String, Pixmap>,
    ) -> Result<Self, ExtensionError> {
        let pixmap = Pixmap::new(width, height).ok_or_else(|| {
            ExtensionError::Budget(format!("cannot rasterize at {width}x{height}"))
        })?;
        Ok(Self {
            width,
            height,
            layers: vec![Layer {
                pixmap: Some(pixmap),
                blend: BlendMode::SourceOver,
                alpha: 1.0,
                clip: None,
                bounds: IntRect::from_xywh(0, 0, width, height),
                dirty: None,
            }],
            images,
            covering: None,
            pool: Vec::new(),
            #[cfg(test)]
            whole: false,
            #[cfg(test)]
            composited: 0,
            #[cfg(test)]
            allocated: 0,
        })
    }

    fn finish(mut self) -> Vec<u8> {
        while self.layers.len() > 1 {
            self.pop_layer();
        }
        self.layers
            .pop()
            .and_then(|layer| layer.pixmap)
            .map_or_else(Vec::new, Pixmap::take)
    }

    /// Whether every layer is composited whole, as `draw_pixmap` would:
    /// the reference the bounded compositing must match, in tests.
    #[allow(clippy::unused_self)]
    fn whole(&self) -> bool {
        #[cfg(test)]
        return self.whole;
        #[cfg(not(test))]
        false
    }

    /// The layer drawing goes onto: the last with a pixmap of its own.
    fn target(&mut self) -> Option<&mut Layer> {
        self.layers
            .iter_mut()
            .rev()
            .find(|layer| layer.pixmap.is_some())
    }

    /// The pixmap drawing goes onto, now drawn on within `bounds` (device
    /// pixels, `None` for all of it).
    fn draw_on(&mut self, bounds: Option<IntRect>) -> &mut Pixmap {
        let canvas = IntRect::from_xywh(0, 0, self.width, self.height);
        let layer = self.target().expect("the canvas is a layer");
        layer.dirty = match (layer.dirty, bounds.or(canvas)) {
            (Some(dirty), Some(bounds)) => union(dirty, bounds),
            (dirty, bounds) => dirty.or(bounds),
        };
        layer.pixmap.as_mut().expect("a layer with a pixmap")
    }

    /// The device pixels a path in `bounds` can touch once drawn with
    /// `transform`, and two more around (antialiasing touches one).
    fn reach(&self, bounds: tiny_skia::Rect, transform: Transform) -> Option<IntRect> {
        let device = PathBuilder::from_rect(bounds)
            .transform(transform)?
            .bounds();
        // Sides are at most 16384: exact in f32.
        #[allow(clippy::cast_precision_loss)]
        let (width, height) = (self.width as f32, self.height as f32);
        #[allow(clippy::cast_possible_truncation)]
        IntRect::from_ltrb(
            device.left().floor().max(-2.0) as i32 - 2,
            device.top().floor().max(-2.0) as i32 - 2,
            device.right().ceil().min(width) as i32 + 2,
            device.bottom().ceil().min(height) as i32 + 2,
        )?
        .intersect(&IntRect::from_xywh(0, 0, self.width, self.height)?)
    }

    fn push(&mut self, blend: BlendMode, alpha: f32, affine: Affine, shape: &impl Shape) {
        let transform = skia_transform(affine);
        let path = path_of(shape);
        let canvas = IntRect::from_xywh(0, 0, self.width, self.height);
        // Where the clip is, as the mask rasterizes it.
        let device = path
            .clone()
            .map(|path| path.transform(transform).map(|path| path.bounds()));
        // Sides are at most 16384: exact in f32.
        #[allow(clippy::cast_precision_loss)]
        let (width, height) = (self.width as f32, self.height as f32);
        let clips_nothing = shape.as_rect().is_some()
            && transform.kx == 0.0
            && transform.ky == 0.0
            && matches!(device, Some(Some(bounds)) if bounds.left() <= 0.0
                && bounds.top() <= 0.0
                && bounds.right() >= width
                && bounds.bottom() >= height);
        #[allow(clippy::float_cmp)]
        let opaque = alpha == 1.0;
        if clips_nothing
            && blend == BlendMode::SourceOver
            && opaque
            && !self.whole()
            && self.target().is_some_and(|layer| layer.dirty.is_none())
        {
            self.layers.push(Layer {
                pixmap: None,
                blend,
                alpha,
                clip: None,
                bounds: None,
                dirty: None,
            });
            return;
        }
        let bounds = match &path {
            // No clip: the whole layer.
            None => canvas,
            _ if self.whole() => canvas,
            // Every pixel the mask can cover.
            Some(path) => self.reach(path.bounds(), transform),
        };
        let clip = if clips_nothing && !self.whole() {
            if self.covering.is_none() {
                self.covering = IntSize::from_wh(self.width, self.height)
                    .and_then(|size| {
                        let coverage = vec![255; size.width() as usize * size.height() as usize];
                        Mask::from_vec(coverage, size)
                    })
                    .map(Rc::new);
            }
            self.covering.clone()
        } else {
            path.and_then(|path| {
                let mut mask = Mask::new(self.width, self.height)?;
                if device::within(shape, affine, 0.0, (self.width, self.height)) {
                    mask.fill_path(&path, FillRule::Winding, true, transform);
                } else if let Some(far) =
                    device::fill_path(shape, affine, (self.width, self.height))
                {
                    mask.fill_path(&far, FillRule::Winding, true, Transform::identity());
                }
                Some(Rc::new(mask))
            })
        };
        let pooled = if self.whole() { None } else { self.pool.pop() };
        #[cfg(test)]
        if pooled.is_none() {
            self.allocated += 1;
        }
        let Some(pixmap) = pooled.or_else(|| Pixmap::new(self.width, self.height)) else {
            return;
        };
        self.layers.push(Layer {
            pixmap: Some(pixmap),
            blend,
            alpha,
            clip,
            bounds,
            dirty: None,
        });
    }
}

impl SkiaSink<'_> {
    /// Fills `shape`, which reaches farther past the canvas than tiny-skia
    /// draws safely, through `transform`: brought within the canvas first
    /// ([`device`]), with the paint's shader moved as tiny-skia would move it.
    fn fill_far(&mut self, shape: &impl Shape, transform: Affine, mut paint: Paint<'_>) {
        let Some(path) = device::fill_path(shape, transform, (self.width, self.height)) else {
            return;
        };
        paint.shader.transform(skia_transform(transform));
        let bounds = self.reach(path.bounds(), Transform::identity());
        self.draw_on(bounds).fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    /// Composites a popped layer onto the one below within `bounds`, as
    /// `draw_pixmap` composites it whole.
    fn composite(&mut self, pixmap: &Pixmap, layer: &Layer, bounds: IntRect) {
        #[cfg(test)]
        {
            self.composited += u64::from(bounds.width()) * u64::from(bounds.height());
        }
        let paint = Paint {
            shader: tiny_skia::Pattern::new(
                pixmap.as_ref(),
                SpreadMode::Pad,
                tiny_skia::FilterQuality::Nearest,
                layer.alpha,
                Transform::identity(),
            ),
            blend_mode: layer.blend,
            anti_alias: false,
            ..Paint::default()
        };
        self.draw_on(Some(bounds)).fill_rect(
            bounds.to_rect(),
            &paint,
            Transform::identity(),
            layer.clip.as_deref(),
        );
    }
}

/// The smallest rectangle holding both.
fn union(a: IntRect, b: IntRect) -> Option<IntRect> {
    IntRect::from_ltrb(
        a.left().min(b.left()),
        a.top().min(b.top()),
        a.right().max(b.right()),
        a.bottom().max(b.bottom()),
    )
}

impl RenderSink for SkiaSink<'_> {
    fn push_layer(
        &mut self,
        blend: impl Into<PenikoBlend>,
        alpha: f32,
        transform: Affine,
        shape: &impl Shape,
    ) {
        self.push(blend_mode(blend.into()), alpha, transform, shape);
    }

    fn push_clip_layer(&mut self, transform: Affine, shape: &impl Shape) {
        self.push(BlendMode::SourceOver, 1.0, transform, shape);
    }

    fn pop_layer(&mut self) {
        if self.layers.len() < 2 {
            return;
        }
        let Some(layer) = self.layers.pop() else {
            return;
        };
        let mut layer = layer;
        let Some(mut pixmap) = layer.pixmap.take() else {
            return;
        };
        // Within its clip, and over the layer below where anything was drawn on it: elsewhere a
        // pixel is left as it was.
        let bounds = layer.bounds.and_then(|bounds| match layer.blend {
            BlendMode::SourceOver if !self.whole() => layer.dirty?.intersect(&bounds),
            _ => Some(bounds),
        });
        if let Some(bounds) = bounds {
            self.composite(&pixmap, &layer, bounds);
        }
        // Cleared where it was drawn on, it is a new layer again.
        if let Some(dirty) = layer.dirty {
            let at = |value: i32| usize::try_from(value).unwrap_or(0);
            let stride = pixmap.width() as usize * 4;
            let (left, right) = (at(dirty.left()) * 4, at(dirty.right()) * 4);
            for row in pixmap
                .data_mut()
                .chunks_exact_mut(stride)
                .skip(at(dirty.top()))
                .take(dirty.height() as usize)
            {
                row[left..right].fill(0);
            }
        }
        self.pool.push(pixmap);
    }
    fn draw(
        &mut self,
        stroke: Option<&kurbo::Stroke>,
        transform: Affine,
        brush: &Brush,
        shape: &impl Shape,
    ) {
        let Some(path) = path_of(shape) else {
            return;
        };
        let Some(shader) = shader_of(brush) else {
            return;
        };
        let paint = Paint {
            shader,
            anti_alias: true,
            ..Paint::default()
        };
        let affine = transform;
        let transform = skia_transform(affine);
        match stroke {
            // tiny-skia draws a stroke of no width as a hairline; Lottie,
            // like SVG, draws nothing.
            Some(stroke) if stroke.width.is_nan() || stroke.width <= 0.0 => {}
            Some(stroke) => {
                let style = Stroke {
                    #[allow(clippy::cast_possible_truncation)]
                    width: stroke.width as f32,
                    #[allow(clippy::cast_possible_truncation)]
                    miter_limit: stroke.miter_limit as f32,
                    line_cap: match stroke.start_cap {
                        kurbo::Cap::Butt => LineCap::Butt,
                        kurbo::Cap::Round => LineCap::Round,
                        kurbo::Cap::Square => LineCap::Square,
                    },
                    line_join: match stroke.join {
                        kurbo::Join::Bevel => LineJoin::Bevel,
                        kurbo::Join::Miter => LineJoin::Miter,
                        kurbo::Join::Round => LineJoin::Round,
                    },
                    #[allow(clippy::cast_possible_truncation)]
                    dash: (!stroke.dash_pattern.is_empty())
                        .then(|| {
                            StrokeDash::new(
                                stroke
                                    .dash_pattern
                                    .iter()
                                    .map(|value| *value as f32)
                                    .collect(),
                                stroke.dash_offset as f32,
                            )
                        })
                        .flatten(),
                };
                // A stroke reaches half its width out, a miter or square
                // cap as far as the miter limit or sqrt(2) times that; a whole
                // width more covers how its outline is flattened.
                let reach = style.width / 2.0 * style.miter_limit.max(std::f32::consts::SQRT_2)
                    + style.width;
                if !device::within(shape, affine, f64::from(reach), (self.width, self.height)) {
                    // Its outline, as tiny-skia strokes one wider than a pixel, filled
                    // within the canvas.
                    let scale = PathStroker::compute_resolution_scale(&transform);
                    let dashed = match &style.dash {
                        Some(dash) => path.dash(dash, scale),
                        None => Some(path),
                    };
                    if let Some(outline) = dashed.and_then(|path| path.stroke(&style, scale)) {
                        self.fill_far(&device::kurbo_path(&outline), affine, paint);
                    }
                    return;
                }
                let bounds = tiny_skia::Rect::from_ltrb(
                    path.bounds().left() - reach,
                    path.bounds().top() - reach,
                    path.bounds().right() + reach,
                    path.bounds().bottom() + reach,
                )
                .and_then(|bounds| self.reach(bounds, transform));
                self.draw_on(bounds)
                    .stroke_path(&path, &paint, &style, transform, None);
            }
            None if !device::within(shape, affine, 0.0, (self.width, self.height)) => {
                self.fill_far(shape, affine, paint);
            }
            None => {
                let bounds = self.reach(path.bounds(), transform);
                self.draw_on(bounds)
                    .fill_path(&path, &paint, FillRule::Winding, transform, None);
            }
        }
    }

    fn draw_image(&mut self, image: &ImageAsset, transform: Affine, alpha: f64) {
        let Some(pixmap) = self.images.get(&image.id) else {
            return;
        };
        // A picture is shown at its asset's authored size, the whole of it
        // fitted and centered, as velato's own sink and lottie-web draw it;
        // velato clips the layer to that size.
        let picture = (f64::from(pixmap.width()), f64::from(pixmap.height()));
        let transform = match image.width.zip(image.height) {
            Some((width, height)) if width > 0.0 && height > 0.0 && (width, height) != picture => {
                let scale = (width / picture.0).min(height / picture.1);
                transform
                    * Affine::translate((
                        (width - picture.0 * scale) / 2.0,
                        (height - picture.1 * scale) / 2.0,
                    ))
                    * Affine::scale(scale)
            }
            _ => transform,
        };
        let paint = PixmapPaint {
            #[allow(clippy::cast_possible_truncation)]
            opacity: alpha as f32,
            blend_mode: BlendMode::SourceOver,
            quality: tiny_skia::FilterQuality::Bilinear,
        };
        let picture = kurbo::Rect::new(0.0, 0.0, picture.0, picture.1);
        if !device::within(&picture, transform, 0.0, (self.width, self.height)) {
            // As tiny-skia draws a picture: a rectangle filled with it.
            let paint = Paint {
                shader: tiny_skia::Pattern::new(
                    pixmap.as_ref(),
                    SpreadMode::Pad,
                    paint.quality,
                    paint.opacity,
                    Transform::identity(),
                ),
                blend_mode: paint.blend_mode,
                anti_alias: false,
                ..Paint::default()
            };
            self.fill_far(&picture, transform, paint);
            return;
        }
        let transform = skia_transform(transform);
        let bounds = IntRect::from_xywh(0, 0, pixmap.width(), pixmap.height())
            .and_then(|picture| self.reach(picture.to_rect(), transform));
        self.draw_on(bounds)
            .draw_pixmap(0, 0, pixmap.as_ref(), &paint, transform, None);
    }
}

fn skia_transform(affine: Affine) -> Transform {
    #[allow(clippy::cast_possible_truncation)]
    let [sx, ky, kx, sy, tx, ty] = affine.as_coeffs().map(|value| value as f32);
    Transform::from_row(sx, ky, kx, sy, tx, ty)
}

/// A kurbo shape as a tiny-skia path, flattened to a tenth of a unit.
fn path_of(shape: &impl Shape) -> Option<tiny_skia::Path> {
    let mut builder = PathBuilder::new();
    #[allow(clippy::cast_possible_truncation)]
    let point = |p: kurbo::Point| (p.x as f32, p.y as f32);
    for element in shape.path_elements(0.1) {
        match element {
            PathEl::MoveTo(p) => {
                let (x, y) = point(p);
                builder.move_to(x, y);
            }
            PathEl::LineTo(p) => {
                let (x, y) = point(p);
                builder.line_to(x, y);
            }
            PathEl::QuadTo(p1, p2) => {
                let ((x1, y1), (x, y)) = (point(p1), point(p2));
                builder.quad_to(x1, y1, x, y);
            }
            PathEl::CurveTo(p1, p2, p3) => {
                let ((x1, y1), (x2, y2), (x, y)) = (point(p1), point(p2), point(p3));
                builder.cubic_to(x1, y1, x2, y2, x, y);
            }
            PathEl::ClosePath => builder.close(),
        }
    }
    builder.finish()
}

fn shader_of(brush: &Brush) -> Option<Shader<'static>> {
    let color = |components: [f32; 4]| {
        Color::from_rgba(
            components[0].clamp(0.0, 1.0),
            components[1].clamp(0.0, 1.0),
            components[2].clamp(0.0, 1.0),
            components[3].clamp(0.0, 1.0),
        )
    };
    match brush {
        Brush::Solid(solid) => color(solid.components).map(Shader::SolidColor),
        Brush::Gradient(gradient) => {
            let stops = gradient
                .stops
                .iter()
                .filter_map(|stop| {
                    let srgb = stop.color.to_alpha_color::<peniko::color::Srgb>();
                    color(srgb.components).map(|c| GradientStop::new(stop.offset, c))
                })
                .collect::<Vec<_>>();
            let mode = match gradient.extend {
                Extend::Pad => SpreadMode::Pad,
                Extend::Repeat => SpreadMode::Repeat,
                Extend::Reflect => SpreadMode::Reflect,
            };
            #[allow(clippy::cast_possible_truncation)]
            let point = |p: kurbo::Point| Point::from_xy(p.x as f32, p.y as f32);
            match &gradient.kind {
                GradientKind::Linear(linear) => LinearGradient::new(
                    point(linear.start),
                    point(linear.end),
                    stops,
                    mode,
                    Transform::identity(),
                ),
                GradientKind::Radial(radial) => RadialGradient::new(
                    point(radial.start_center),
                    radial.start_radius,
                    point(radial.end_center),
                    radial.end_radius,
                    stops,
                    mode,
                    Transform::identity(),
                ),
                GradientKind::Sweep(sweep) => SweepGradient::new(
                    point(sweep.center),
                    sweep.start_angle.to_degrees(),
                    sweep.end_angle.to_degrees(),
                    stops,
                    mode,
                    Transform::identity(),
                ),
            }
        }
        Brush::Image(_) => None,
    }
}

/// A peniko blend as tiny-skia draws it: a separable or non-separable mix
/// over the backdrop, otherwise the Porter-Duff composition.
fn blend_mode(blend: PenikoBlend) -> BlendMode {
    match blend.mix {
        Mix::Multiply => BlendMode::Multiply,
        Mix::Screen => BlendMode::Screen,
        Mix::Overlay => BlendMode::Overlay,
        Mix::Darken => BlendMode::Darken,
        Mix::Lighten => BlendMode::Lighten,
        Mix::ColorDodge => BlendMode::ColorDodge,
        Mix::ColorBurn => BlendMode::ColorBurn,
        Mix::HardLight => BlendMode::HardLight,
        Mix::SoftLight => BlendMode::SoftLight,
        Mix::Difference => BlendMode::Difference,
        Mix::Exclusion => BlendMode::Exclusion,
        Mix::Hue => BlendMode::Hue,
        Mix::Saturation => BlendMode::Saturation,
        Mix::Color => BlendMode::Color,
        Mix::Luminosity => BlendMode::Luminosity,
        Mix::Normal => match blend.compose {
            Compose::Clear => BlendMode::Clear,
            Compose::Copy => BlendMode::Source,
            Compose::Dest => BlendMode::Destination,
            Compose::DestOver => BlendMode::DestinationOver,
            Compose::SrcIn => BlendMode::SourceIn,
            Compose::DestIn => BlendMode::DestinationIn,
            Compose::SrcOut => BlendMode::SourceOut,
            Compose::DestOut => BlendMode::DestinationOut,
            Compose::SrcAtop => BlendMode::SourceAtop,
            Compose::DestAtop => BlendMode::DestinationAtop,
            Compose::Xor => BlendMode::Xor,
            Compose::Plus | Compose::PlusLighter => BlendMode::Plus,
            Compose::SrcOver => BlendMode::SourceOver,
        },
    }
}

#[cfg(test)]
mod tests;

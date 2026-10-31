//! OpenEXR flat RGB(A) or Y(A), half or 32-bit float, read as premultiplied linear half floats in
//! BT.709 primaries.
//!
//! The canvas is the display window; outside the data window is transparent. Alpha is already
//! premultiplied per the OpenEXR spec. Color is the file's chromaticities (BT.709 if absent or
//! degenerate) or the project's primaries, adapted to D65 with Bradford. Out-of-range values are
//! kept; non-finite samples become `0` and over-range floats clamp to the half range, both
//! reported. Deep data and integer color channels are refused; other parts and channels are
//! reported and skipped.

use std::io::Cursor;
use std::sync::Arc;

use alpha2_model::{LinearPrimaries, ProviderSource};
use alpha2_time::MediaTime;
use exr::block::reader::ChunksReader as _;
use exr::meta::attribute::{ChannelDescription, ChannelList, SampleType};
use half::f16;

use crate::api::{
    ColorOrigin, Diagnostic, ExtensionError, Format, FramePixels, LinearColor, ProbeReport,
    ResourceResolver, Timing, VisualFrame, VisualProvider, VisualSource,
};
use crate::budget::{MAX_CANVAS_PIXELS, check_canvas};

const MAGIC: [u8; 4] = [0x76, 0x2f, 0x31, 0x01];

pub(crate) fn is_exr(bytes: &[u8]) -> bool {
    bytes.starts_with(&MAGIC)
}

/// Reads still OpenEXR images.
pub struct ExrProvider;

impl VisualProvider for ExrProvider {
    fn name(&self) -> &'static str {
        "exr"
    }

    fn probe(&self, bytes: &[u8]) -> Option<Result<ProbeReport, ExtensionError>> {
        is_exr(bytes).then(|| decode_exr(bytes, None).map(|image| still_report(&image)))
    }

    fn open(
        &self,
        bytes: Arc<[u8]>,
        source: &ProviderSource,
        _resolver: &dyn ResourceResolver,
    ) -> Result<Box<dyn VisualSource>, ExtensionError> {
        let ProviderSource::Exr { primaries } = source else {
            return Err(ExtensionError::Mismatch("not an OpenEXR source".into()));
        };
        if !is_exr(&bytes) {
            return Err(ExtensionError::Mismatch("not an OpenEXR file".into()));
        }
        let image = decode_exr(&bytes, primaries.map(project_color))?;
        let report = still_report(&image);
        Ok(Box::new(ExrSource {
            report,
            frame: VisualFrame {
                width: image.width,
                height: image.height,
                pixels: FramePixels::LinearF16(image.pixels.into()),
                index: 0,
                start: MediaTime::from_ticks(i64::MIN),
                end: None,
                diagnostics: Vec::new(),
            },
        }))
    }
}

fn still_report(image: &ExrImage) -> ProbeReport {
    ProbeReport {
        format: Format::Exr,
        width: image.width,
        height: image.height,
        timing: Timing::Static,
        loop_count: None,
        items: Vec::new(),
        item: None,
        color: Some(image.color.clone()),
        diagnostics: image.diagnostics.clone(),
    }
}

struct ExrSource {
    report: ProbeReport,
    frame: VisualFrame,
}

impl VisualSource for ExrSource {
    fn report(&self) -> &ProbeReport {
        &self.report
    }

    fn render(
        &mut self,
        _time: MediaTime,
        _fit: (u32, u32),
    ) -> Result<VisualFrame, ExtensionError> {
        Ok(self.frame.clone())
    }

    fn resident_bytes(&self) -> usize {
        self.frame.pixels.byte_len()
    }
}

pub(crate) struct ExrImage {
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// Premultiplied linear BT.709 RGBA half floats.
    pub(crate) pixels: Vec<u16>,
    pub(crate) color: LinearColor,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

pub(crate) fn project_color(primaries: LinearPrimaries) -> LinearColor {
    LinearColor {
        primaries: Some(primaries),
        chromaticities: named_chromaticities(primaries),
        origin: ColorOrigin::Project,
    }
}

/// Decodes an OpenEXR file, overriding its color with `color` when given. Only the shown part's
/// shown channels are decompressed, placed into the canvas block by block.
pub(crate) fn decode_exr(
    bytes: &[u8],
    color: Option<LinearColor>,
) -> Result<ExrImage, ExtensionError> {
    check_channel_lists(bytes)?;
    let reader = exr::block::read(Cursor::new(bytes), false).map_err(exr_error)?;
    let headers = reader.headers();
    if headers.iter().all(|header| header.deep) {
        return Err(ExtensionError::Unsupported(
            "deep OpenEXR data is not supported".into(),
        ));
    }
    let Some(part) = headers.iter().position(|header| {
        !header.deep
            && channels_shown(header.channels.list.iter().map(|c| c.name.to_string())).is_some()
    }) else {
        let names = headers
            .iter()
            .flat_map(|header| {
                header
                    .channels
                    .list
                    .iter()
                    .map(|channel| channel.name.to_string())
            })
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ExtensionError::Unsupported(format!(
            "no part has R, G, and B channels or a Y channel (it has {names})"
        )));
    };
    let header = headers[part].clone();
    let display = header.shared_attributes.display_window;
    let (width, height) = (
        u32::try_from(display.size.x()).unwrap_or(u32::MAX),
        u32::try_from(display.size.y()).unwrap_or(u32::MAX),
    );
    let mut diagnostics = check_part(&header, width, height, part, headers.len())?;
    let color = color.unwrap_or_else(|| {
        let declared = file_color(header.shared_attributes.chromaticities);
        if defines_color_space(declared.chromaticities) {
            return declared;
        }
        // Degenerate primaries or a point at y = 0 would turn every pixel into NaN, so ignore them.
        diagnostics.push(Diagnostic::UnsupportedFeature {
            feature: "chromaticities that define no color space".into(),
            path: "/chromaticities".into(),
            count: 1,
        });
        file_color(None)
    });

    let channels = &header.channels.list;
    let shown = channels_shown(channels.iter().map(|channel| channel.name.to_string()))
        .ok_or_else(|| ExtensionError::Malformed("the part's channels could not be read".into()))?;
    let others = (0..channels.len())
        .filter(|index| !shown.contains(&Some(*index)))
        .collect::<Vec<_>>();
    if let Some(first) = others.first() {
        diagnostics.push(Diagnostic::UnsupportedFeature {
            feature: "channels besides R, G, B, A, and Y".into(),
            path: format!("/channels/{}", channels[*first].name),
            count: others.len(),
        });
    }
    for channel in shown.iter().flatten().map(|index| &channels[*index]) {
        check_channel(channel)?;
    }
    if shown
        .iter()
        .flatten()
        .any(|index| channels[*index].sample_type == SampleType::F32)
    {
        diagnostics.push(Diagnostic::ReducedPrecision);
    }

    let matrix = to_bt709(color.chromaticities);
    let mut canvas = Canvas::new(&header, display, (width, height), shown, matrix);
    reader
        .filter_chunks(false, |_, tile, block| {
            block.layer == part && tile.is_largest_resolution_level()
        })
        .map_err(exr_error)?
        .decompress_sequential(false, |_, block| {
            canvas.place_block(&block, &header.channels);
            Ok(())
        })
        .map_err(exr_error)?;
    let Canvas { pixels, counts, .. } = canvas;
    if counts.non_finite > 0 {
        diagnostics.push(Diagnostic::NonFiniteSamples {
            count: counts.non_finite,
        });
    }
    if counts.out_of_range > 0 {
        diagnostics.push(Diagnostic::OutOfHalfRange {
            count: counts.out_of_range,
        });
    }
    Ok(ExrImage {
        width,
        height,
        pixels,
        color,
        diagnostics,
    })
}

/// Refuses a channel list stored under a name other than `channels`. The exr reader checks such
/// attributes in hash order, so probe and open could otherwise fail for different reasons.
///
/// Walks the header as the reader does: a null where a name starts ends the header, and a name
/// or type is at least one byte and then up to a null.
fn check_channel_lists(bytes: &[u8]) -> Result<(), ExtensionError> {
    // A name or type: the first byte, then up to a null, at most 256 bytes.
    fn text(bytes: &[u8], at: &mut usize) -> Option<Vec<u8>> {
        let rest = bytes.get(*at..)?;
        let end = 1 + rest
            .get(1..)?
            .iter()
            .take(256)
            .position(|byte| *byte == 0)?;
        *at += end + 1;
        Some(rest[..end].to_vec())
    }
    let Some(flags) = bytes.get(4..8) else {
        return Ok(());
    };
    let multipart = flags[1] & 0x10 != 0;
    let mut at = 8;
    loop {
        let mut attributes = 0;
        loop {
            match bytes.get(at) {
                None => return Ok(()),
                Some(0) => {
                    at += 1;
                    break;
                }
                Some(_) => {}
            }
            let Some(name) = text(bytes, &mut at) else {
                return Ok(());
            };
            attributes += 1;
            let Some(kind) = text(bytes, &mut at) else {
                return Ok(());
            };
            let Some(size) = bytes
                .get(at..at + 4)
                .and_then(|size| usize::try_from(i32::from_le_bytes(size.try_into().ok()?)).ok())
            else {
                return Ok(());
            };
            if kind == b"chlist" && name != b"channels" {
                return Err(ExtensionError::Malformed(format!(
                    "a header holds a channel list besides `channels`: `{}`",
                    String::from_utf8_lossy(&name)
                )));
            }
            at = at.saturating_add(4).saturating_add(size);
        }
        // A multi-part file's headers end with an empty one.
        if !multipart || attributes == 0 {
            return Ok(());
        }
    }
}

/// Refuses a channel shown that is not one sample of color per pixel.
fn check_channel(channel: &ChannelDescription) -> Result<(), ExtensionError> {
    if channel.sampling.x() != 1 || channel.sampling.y() != 1 {
        return Err(ExtensionError::Unsupported(format!(
            "channel {} is subsampled",
            channel.name
        )));
    }
    if channel.sample_type == SampleType::U32 {
        return Err(ExtensionError::Unsupported(format!(
            "channel {} holds integers, not colors",
            channel.name
        )));
    }
    Ok(())
}

/// Refuses a part too large to open, and reports what is not shown.
fn check_part(
    header: &exr::meta::header::Header,
    width: u32,
    height: u32,
    part: usize,
    parts: usize,
) -> Result<Vec<Diagnostic>, ExtensionError> {
    check_canvas(width, height)?;
    let data_pixels = u64::try_from(header.layer_size.x())
        .unwrap_or(u64::MAX)
        .saturating_mul(u64::try_from(header.layer_size.y()).unwrap_or(u64::MAX));
    if data_pixels > MAX_CANVAS_PIXELS {
        return Err(ExtensionError::Budget(format!(
            "a {}x{} data window is larger than {MAX_CANVAS_PIXELS} pixels",
            header.layer_size.x(),
            header.layer_size.y()
        )));
    }
    let mut diagnostics = Vec::new();
    if parts > 1 {
        diagnostics.push(Diagnostic::UnsupportedFeature {
            feature: "more than one part".into(),
            path: format!("/parts/{}", usize::from(part == 0)),
            count: parts - 1,
        });
    }
    let aspect = header.shared_attributes.pixel_aspect;
    if (aspect - 1.0).abs() > 1e-6 {
        diagnostics.push(Diagnostic::UnsupportedFeature {
            feature: format!("pixel aspect ratio {aspect}"),
            path: "/pixelAspectRatio".into(),
            count: 1,
        });
    }
    Ok(diagnostics)
}

/// Where data-window position `at` lands in the display window, if inside.
fn place(offset: i64, at: usize, size: usize) -> Option<usize> {
    usize::try_from(offset + i64::try_from(at).ok()?)
        .ok()
        .filter(|position| *position < size)
}

/// The display-window canvas, filled row by row as blocks decompress.
struct Canvas {
    width: usize,
    height: usize,
    /// Data window offset in the display window; both are within `i32`.
    offset: (i64, i64),
    /// Channel indices for red, green, blue, and alpha.
    shown: [Option<usize>; 4],
    matrix: Option<[[f32; 3]; 3]>,
    /// One block row per channel, until the last channel is read.
    row: Vec<Vec<f32>>,
    pixels: Vec<u16>,
    counts: Counts,
}

impl Canvas {
    fn new(
        header: &exr::meta::header::Header,
        display: exr::meta::attribute::IntegerBounds,
        (width, height): (u32, u32),
        shown: [Option<usize>; 4],
        matrix: Option<[[f32; 3]; 3]>,
    ) -> Self {
        let position = header.own_attributes.layer_position;
        let mut canvas = Self {
            width: width as usize,
            height: height as usize,
            offset: (
                i64::from(position.x()) - i64::from(display.position.x()),
                i64::from(position.y()) - i64::from(display.position.y()),
            ),
            shown,
            matrix,
            row: vec![Vec::new(); header.channels.list.len()],
            pixels: vec![0; width as usize * height as usize * 4],
            counts: Counts::default(),
        };
        // Pixels a block leaves unwritten show as zero samples.
        let blank = canvas.pixel(|_| Some(0.0));
        let size = header.layer_size;
        for data_y in 0..size.y() {
            let Some(y) = place(canvas.offset.1, data_y, canvas.height) else {
                continue;
            };
            for data_x in 0..size.x() {
                if let Some(x) = place(canvas.offset.0, data_x, canvas.width) {
                    canvas.pixels[(y * canvas.width + x) * 4..][..4].copy_from_slice(&blank);
                }
            }
        }
        canvas
    }

    /// One pixel from its channel samples (`None` if absent), counting replaced samples.
    fn pixel(&mut self, mut sample: impl FnMut(usize) -> Option<f32>) -> [u16; 4] {
        let shown = self.shown;
        let counts = &mut self.counts;
        let mut value = |slot: usize, missing: f32| {
            shown[slot].and_then(&mut sample).map_or(missing, |value| {
                if value.is_finite() {
                    value
                } else {
                    counts.non_finite += 1;
                    0.0
                }
            })
        };
        let alpha = value(3, 1.0).clamp(0.0, 1.0);
        let mut rgb = [value(0, 0.0), value(1, 0.0), value(2, 0.0)];
        if let Some(matrix) = &self.matrix {
            rgb = multiply(matrix, rgb);
        }
        [rgb[0], rgb[1], rgb[2], alpha].map(|value| to_half(value, counts))
    }

    /// Places a decompressed block; each row is placed once its last channel is read.
    fn place_block(&mut self, block: &exr::block::UncompressedBlock, channels: &ChannelList) {
        let last = channels.list.len() - 1;
        for line in block.lines(channels) {
            let channel = line.location.channel;
            if self.shown.contains(&Some(channel)) {
                let row = &mut self.row[channel];
                row.clear();
                match channels.list[channel].sample_type {
                    SampleType::F16 => row.extend(
                        line.read_samples::<f16>()
                            .map(|sample| sample.map_or(f32::NAN, f16::to_f32)),
                    ),
                    _ => row.extend(
                        line.read_samples::<f32>()
                            .map(|sample| sample.unwrap_or(f32::NAN)),
                    ),
                }
            }
            if channel != last {
                continue;
            }
            let Some(y) = place(self.offset.1, line.location.position.y(), self.height) else {
                continue;
            };
            for at in 0..line.location.sample_count {
                let data_x = line.location.position.x() + at;
                let Some(x) = place(self.offset.0, data_x, self.width) else {
                    continue;
                };
                let row = std::mem::take(&mut self.row);
                let pixel = self.pixel(|channel| row[channel].get(at).copied());
                self.row = row;
                self.pixels[(y * self.width + x) * 4..][..4].copy_from_slice(&pixel);
            }
        }
    }
}

/// Replaced samples, for diagnostics.
#[derive(Default)]
struct Counts {
    non_finite: usize,
    out_of_range: usize,
}

/// A value as half-float bits, clamping and counting out-of-range values; NaN becomes `0` and is
/// counted as non-finite.
fn to_half(value: f32, counts: &mut Counts) -> u16 {
    if value.is_nan() {
        counts.non_finite += 1;
        return 0;
    }
    let half = f16::from_f32(value);
    if half.is_infinite() {
        counts.out_of_range += 1;
        return if value > 0.0 { f16::MAX } else { f16::MIN }.to_bits();
    }
    half.to_bits()
}

/// Channel indices shown as RGBA: R, G, B or Y for all three, plus A. `None` with neither.
fn channels_shown(names: impl Iterator<Item = String>) -> Option<[Option<usize>; 4]> {
    let names = names.collect::<Vec<_>>();
    let find = |name: &str| names.iter().position(|candidate| candidate == name);
    let alpha = find("A");
    match (find("R"), find("G"), find("B"), find("Y")) {
        (Some(red), Some(green), Some(blue), _) => {
            Some([Some(red), Some(green), Some(blue), alpha])
        }
        (_, _, _, Some(luma)) => Some([Some(luma), Some(luma), Some(luma), alpha]),
        _ => None,
    }
}

fn exr_error(error: exr::error::Error) -> ExtensionError {
    match error {
        exr::error::Error::NotSupported(message) => ExtensionError::Unsupported(message.into()),
        other => ExtensionError::Malformed(other.to_string()),
    }
}

/// The file's declared chromaticities, or the OpenEXR default.
fn file_color(declared: Option<exr::meta::attribute::Chromaticities>) -> LinearColor {
    let Some(declared) = declared else {
        return LinearColor {
            primaries: Some(LinearPrimaries::Bt709),
            chromaticities: named_chromaticities(LinearPrimaries::Bt709),
            origin: ColorOrigin::FormatDefault,
        };
    };
    let chromaticities = [declared.red, declared.green, declared.blue, declared.white]
        .map(|point| [point.x(), point.y()]);
    LinearColor {
        primaries: PRIMARIES.into_iter().find(|named| {
            named_chromaticities(*named)
                .iter()
                .flatten()
                .zip(chromaticities.iter().flatten())
                .all(|(a, b)| (a - b).abs() < 5e-4)
        }),
        chromaticities,
        origin: ColorOrigin::File,
    }
}

const PRIMARIES: [LinearPrimaries; 5] = [
    LinearPrimaries::Bt709,
    LinearPrimaries::Bt2020,
    LinearPrimaries::P3D65,
    LinearPrimaries::AcesAp0,
    LinearPrimaries::AcesAp1,
];

const D65: [f32; 2] = [0.3127, 0.3290];
const ACES_WHITE: [f32; 2] = [0.32168, 0.33767];

/// Red, green, blue, and white, CIE 1931 `[x, y]`.
pub(crate) const fn named_chromaticities(primaries: LinearPrimaries) -> [[f32; 2]; 4] {
    match primaries {
        LinearPrimaries::Bt709 => [[0.64, 0.33], [0.30, 0.60], [0.15, 0.06], D65],
        LinearPrimaries::Bt2020 => [[0.708, 0.292], [0.170, 0.797], [0.131, 0.046], D65],
        LinearPrimaries::P3D65 => [[0.680, 0.320], [0.265, 0.690], [0.150, 0.060], D65],
        LinearPrimaries::AcesAp0 => [[0.7347, 0.2653], [0.0, 1.0], [0.0001, -0.0770], ACES_WHITE],
        LinearPrimaries::AcesAp1 => [[0.713, 0.293], [0.165, 0.830], [0.128, 0.044], ACES_WHITE],
    }
}

type Matrix = [[f64; 3]; 3];

/// The matrix from linear RGB in `chromaticities` to linear BT.709; `None` if already BT.709.
fn to_bt709(chromaticities: [[f32; 2]; 4]) -> Option<[[f32; 3]; 3]> {
    let bt709 = named_chromaticities(LinearPrimaries::Bt709);
    if chromaticities == bt709 {
        return None;
    }
    let to_xyz = rgb_to_xyz(chromaticities);
    let adapt = bradford(chromaticities[3], D65);
    let from_xyz = invert(&rgb_to_xyz(bt709));
    let matrix = product(&from_xyz, &product(&adapt, &to_xyz));
    // The largest coefficient is a few units; f32 keeps it to 1e-7.
    #[allow(clippy::cast_possible_truncation)]
    Some(matrix.map(|row| row.map(|value| value as f32)))
}

/// Whether the primaries span a triangle and no point lies on `y = 0`.
fn defines_color_space(chromaticities: [[f32; 2]; 4]) -> bool {
    to_bt709(chromaticities)
        .is_none_or(|matrix| matrix.iter().flatten().all(|value| value.is_finite()))
}

fn multiply(matrix: &[[f32; 3]; 3], rgb: [f32; 3]) -> [f32; 3] {
    matrix.map(|row| row[0] * rgb[0] + row[1] * rgb[1] + row[2] * rgb[2])
}

fn xyz_of(point: [f32; 2]) -> [f64; 3] {
    let (x, y) = (f64::from(point[0]), f64::from(point[1]));
    [x / y, 1.0, (1.0 - x - y) / y]
}

/// RGB to CIE XYZ for primaries and white `[r, g, b, w]`, white at `Y = 1`.
fn rgb_to_xyz(chromaticities: [[f32; 2]; 4]) -> Matrix {
    let columns = [0, 1, 2].map(|index| xyz_of(chromaticities[index]));
    let primaries = [0, 1, 2].map(|row| columns.map(|column| column[row]));
    let white = xyz_of(chromaticities[3]);
    let scale = apply(&invert(&primaries), white);
    primaries.map(|row| [row[0] * scale[0], row[1] * scale[1], row[2] * scale[2]])
}

/// The Bradford chromatic adaptation from white `from` to white `to`.
fn bradford(from: [f32; 2], to: [f32; 2]) -> Matrix {
    const CONE: Matrix = [
        [0.8951, 0.2664, -0.1614],
        [-0.7502, 1.7135, 0.0367],
        [0.0389, -0.0685, 1.0296],
    ];
    let source = apply(&CONE, xyz_of(from));
    let target = apply(&CONE, xyz_of(to));
    let scale = [0, 1, 2].map(|index| {
        let mut row = [0.0; 3];
        row[index] = target[index] / source[index];
        row
    });
    product(&invert(&CONE), &product(&scale, &CONE))
}

fn apply(matrix: &Matrix, vector: [f64; 3]) -> [f64; 3] {
    matrix.map(|row| row[0] * vector[0] + row[1] * vector[1] + row[2] * vector[2])
}

fn product(a: &Matrix, b: &Matrix) -> Matrix {
    let mut out = [[0.0; 3]; 3];
    for (row, out_row) in out.iter_mut().enumerate() {
        for (column, value) in out_row.iter_mut().enumerate() {
            *value = (0..3).map(|k| a[row][k] * b[k][column]).sum();
        }
    }
    out
}

fn invert(m: &Matrix) -> Matrix {
    let cofactor =
        |r0: usize, r1: usize, c0: usize, c1: usize| m[r0][c0] * m[r1][c1] - m[r0][c1] * m[r1][c0];
    let adjugate = [
        [
            cofactor(1, 2, 1, 2),
            -cofactor(0, 2, 1, 2),
            cofactor(0, 1, 1, 2),
        ],
        [
            -cofactor(1, 2, 0, 2),
            cofactor(0, 2, 0, 2),
            -cofactor(0, 1, 0, 2),
        ],
        [
            cofactor(1, 2, 0, 1),
            -cofactor(0, 2, 0, 1),
            cofactor(0, 1, 0, 1),
        ],
    ];
    let determinant =
        m[0][0] * adjugate[0][0] + m[0][1] * adjugate[1][0] + m[0][2] * adjugate[2][0];
    adjugate.map(|row| row.map(|value| value / determinant))
}

#[cfg(test)]
pub(crate) fn matrix_to_bt709(primaries: LinearPrimaries) -> Option<[[f32; 3]; 3]> {
    to_bt709(named_chromaticities(primaries))
}

//! Still images played in order at a fixed rate: PNGs, or OpenEXR images.
//!
//! The project lists each frame's locator; the host resolves their bytes.
//! The source keeps the resolver and reads a frame when it shows it: what a
//! sequence holds is the frames it decoded lately, within a byte budget,
//! never its files. Every frame must be the first frame's format and size;
//! a gap in the numbering is shown as the sequence's policy says. OpenEXR
//! frames all take the color the first declares, or the project names.

use std::collections::VecDeque;
use std::sync::Arc;

use alpha2_model::{LinearPrimaries, MissingFrames};
use alpha2_time::{FrameRate, MediaTime};

use crate::api::{
    Diagnostic, ExtensionError, Format, FramePixels, LinearColor, ProbeReport, SharedResolver,
    Timing, VisualFrame, VisualSource, premultiply,
};
use crate::budget::{DEFAULT_FRAME_BUDGET, check_canvas};
use crate::raster::decode_png;

/// Opens an image sequence: `frames` in play order (`None` for a gap in the
/// numbering), at `rate`, their bytes from `resolver` as they are shown;
/// OpenEXR frames in `primaries` when given. Only the first frame is read
/// here.
///
/// # Errors
/// Why the first frame cannot be read, or the settings are incoherent.
pub fn open_sequence(
    frames: &[Option<String>],
    rate: FrameRate,
    missing: MissingFrames,
    primaries: Option<LinearPrimaries>,
    resolver: SharedResolver,
) -> Result<Box<dyn VisualSource>, ExtensionError> {
    open_within(
        frames,
        rate,
        missing,
        primaries,
        resolver,
        DEFAULT_FRAME_BUDGET,
    )
}

/// [`open_sequence`], keeping at most `budget` decoded bytes (and always
/// the frame last shown).
pub(crate) fn open_within(
    frames: &[Option<String>],
    rate: FrameRate,
    missing: MissingFrames,
    primaries: Option<LinearPrimaries>,
    resolver: SharedResolver,
    budget: usize,
) -> Result<Box<dyn VisualSource>, ExtensionError> {
    let first = frames
        .first()
        .and_then(Option::as_ref)
        .ok_or_else(|| ExtensionError::Malformed("the sequence's first frame is missing".into()))?;
    if missing == MissingFrames::Reject && frames.iter().any(Option::is_none) {
        return Err(ExtensionError::Mismatch(
            "the sequence has a gap and refuses missing frames".into(),
        ));
    }
    let first = decode(&resolver.bytes(first)?, None, primaries)?;
    if first.color.is_none() && primaries.is_some() {
        return Err(ExtensionError::Mismatch(
            "primaries apply to OpenEXR frames, and these are PNGs".into(),
        ));
    }
    check_canvas(first.width, first.height)?;
    let report = ProbeReport {
        format: first.format,
        width: first.width,
        height: first.height,
        timing: Timing::Rate {
            rate,
            frames: frames.len(),
        },
        loop_count: None,
        items: Vec::new(),
        item: None,
        color: first.color.clone(),
        diagnostics: first.diagnostics.clone(),
    };
    let mut source = SequenceSource {
        frames: frames.to_vec(),
        resolver,
        missing,
        report,
        kept: VecDeque::new(),
        kept_bytes: 0,
        budget,
    };
    source.keep(0, Arc::new(first));
    Ok(Box::new(source))
}

/// One decoded frame.
struct Decoded {
    format: Format,
    width: u32,
    height: u32,
    pixels: FramePixels,
    /// For an OpenEXR frame, the color it was read in.
    color: Option<LinearColor>,
    diagnostics: Vec<Diagnostic>,
}

impl Decoded {
    /// The bytes its pixels take.
    fn bytes(&self) -> usize {
        match &self.pixels {
            FramePixels::Rgba8(pixels) => pixels.len(),
            FramePixels::LinearF16(pixels) => pixels.len() * 2,
        }
    }
}

/// Decodes a PNG or OpenEXR frame; an OpenEXR frame in `color` when given
/// (the sequence's), else in `primaries` or what it declares.
fn decode(
    bytes: &[u8],
    color: Option<&LinearColor>,
    primaries: Option<LinearPrimaries>,
) -> Result<Decoded, ExtensionError> {
    #[cfg(feature = "exr")]
    if crate::exr::is_exr(bytes) {
        let color = color
            .cloned()
            .or_else(|| primaries.map(crate::exr::project_color));
        let image = crate::exr::decode_exr(bytes, color)?;
        return Ok(Decoded {
            format: Format::ExrSequence,
            width: image.width,
            height: image.height,
            pixels: FramePixels::LinearF16(image.pixels.into()),
            color: Some(image.color),
            diagnostics: image.diagnostics,
        });
    }
    let _ = (color, primaries);
    if bytes.starts_with(&[0x76, 0x2f, 0x31, 0x01]) {
        return Err(ExtensionError::Unsupported(
            "this build reads no OpenEXR frames".into(),
        ));
    }
    let (width, height, mut pixels) = decode_png(bytes)?;
    premultiply(&mut pixels);
    Ok(Decoded {
        format: Format::PngSequence,
        width,
        height,
        pixels: FramePixels::Rgba8(pixels.into()),
        color: None,
        diagnostics: Vec::new(),
    })
}

/// An open image sequence, decoding frames as they are shown.
struct SequenceSource {
    /// Each frame's locator; `None` for a gap.
    frames: Vec<Option<String>>,
    resolver: SharedResolver,
    missing: MissingFrames,
    report: ProbeReport,
    /// Recently decoded frames by index; newest last.
    kept: VecDeque<(usize, Arc<Decoded>)>,
    /// The bytes `kept` holds.
    kept_bytes: usize,
    /// Decoded bytes kept between requests; the newest frame is kept
    /// whatever its size.
    budget: usize,
}

impl SequenceSource {
    /// The frame shown at `time`: its index, its start and end, and the index
    /// whose picture it shows (a held gap shows the nearest present frame
    /// before it).
    fn shown(&self, time: MediaTime) -> (usize, MediaTime, Option<MediaTime>, usize) {
        let (index, start, end) = self.report.timing.frame_at(time);
        let shown = match self.missing {
            MissingFrames::Hold => (0..=index)
                .rev()
                .find(|at| self.frames[*at].is_some())
                .unwrap_or(0),
            MissingFrames::Blank | MissingFrames::Reject => index,
        };
        (index, start, end, shown)
    }

    fn keep(&mut self, index: usize, decoded: Arc<Decoded>) {
        self.kept_bytes += decoded.bytes();
        self.kept.push_back((index, decoded));
        while self.kept.len() > 1 && self.kept_bytes > self.budget {
            if let Some((_, oldest)) = self.kept.pop_front() {
                self.kept_bytes -= oldest.bytes();
            }
        }
    }

    fn decoded(&mut self, index: usize) -> Result<Arc<Decoded>, ExtensionError> {
        if let Some((_, decoded)) = self.kept.iter().find(|(kept, _)| *kept == index) {
            return Ok(Arc::clone(decoded));
        }
        let (width, height) = (self.report.width, self.report.height);
        let decoded = if let Some(locator) = &self.frames[index] {
            let decoded = decode(
                &self.resolver.bytes(locator)?,
                self.report.color.as_ref(),
                None,
            )?;
            if decoded.format != self.report.format {
                return Err(ExtensionError::Mismatch(format!(
                    "frame {index} is not in the first frame's format"
                )));
            }
            if (decoded.width, decoded.height) != (width, height) {
                return Err(ExtensionError::Mismatch(format!(
                    "frame {index} is {}x{}; the sequence is {width}x{height}",
                    decoded.width, decoded.height
                )));
            }
            decoded
        } else {
            let samples = width as usize * height as usize * 4;
            Decoded {
                format: self.report.format,
                width,
                height,
                pixels: if self.report.color.is_some() {
                    FramePixels::LinearF16(vec![0; samples].into())
                } else {
                    FramePixels::Rgba8(vec![0; samples].into())
                },
                color: self.report.color.clone(),
                diagnostics: Vec::new(),
            }
        };
        let decoded = Arc::new(decoded);
        self.keep(index, Arc::clone(&decoded));
        Ok(decoded)
    }
}

impl VisualSource for SequenceSource {
    fn report(&self) -> &ProbeReport {
        &self.report
    }

    fn render(&mut self, time: MediaTime, _fit: (u32, u32)) -> Result<VisualFrame, ExtensionError> {
        let (index, start, end, shown) = self.shown(time);
        let decoded = self.decoded(shown)?;
        Ok(VisualFrame {
            width: self.report.width,
            height: self.report.height,
            pixels: decoded.pixels.clone(),
            index,
            start,
            end,
            diagnostics: decoded.diagnostics.clone(),
        })
    }

    fn needs(&self, time: MediaTime) -> Vec<String> {
        let (.., shown) = self.shown(time);
        if self.kept.iter().any(|(kept, _)| *kept == shown) {
            return Vec::new();
        }
        self.frames[shown].iter().cloned().collect()
    }

    fn resident_bytes(&self) -> usize {
        self.kept_bytes
    }
}

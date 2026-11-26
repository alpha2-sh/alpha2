//! Rust extractors, the inputs they are supplied, and the loop that runs them.

use super::descriptor::{Descriptor, Direction, VideoFormat};
use super::session::{
    AudioInput, Delivery, Emit, ExtractSession, Need, RunContext, SessionError, Step,
    SuppliedAudio, SuppliedFrame, VideoInput,
};
use alpha2_model::{DataInterval, DataSet, DataWord};
use alpha2_time::MediaTime;
use std::fmt;

/// A pass beginning, as an extractor is told.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PassInfo {
    /// The pass's position in the request.
    pub pass: usize,
    /// Its direction.
    pub direction: Direction,
}

/// An extractor's own failure, carried to the caller as is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtractorError {
    /// Why it failed.
    pub message: String,
    /// The error code a host reports instead of `EXTRACTOR_FAILED`, such as `PLUGIN_TRAPPED`.
    pub code: Option<&'static str>,
    /// What a caller reads besides the code, such as the slot and reason of a refused resource.
    pub details: Option<serde_json::Map<String, serde_json::Value>>,
}

impl ExtractorError {
    /// A failure the host reports as `EXTRACTOR_FAILED`.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
            details: None,
        }
    }

    /// A failure the host reports with `code`.
    pub fn with_code(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: Some(code),
            details: None,
        }
    }

    /// The failure with `details`.
    #[must_use]
    pub fn with_details(mut self, details: serde_json::Map<String, serde_json::Value>) -> Self {
        self.details = Some(details);
        self
    }
}

impl fmt::Display for ExtractorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ExtractorError {}

/// Collects what an extractor emits for one call; the session validates it afterwards.
#[derive(Debug, Default)]
pub struct Emitter {
    emits: Vec<Emit>,
}

impl Emitter {
    /// A sample of a `points`, `scalars` or `similarity` output; `None` marks a lost channel.
    pub fn sample(&mut self, output: &str, time: MediaTime, values: &[Option<f32>]) {
        self.emits.push(Emit::Sample {
            output: output.to_owned(),
            time,
            values: values.to_vec(),
        });
    }

    /// A word of a `words` output.
    pub fn word(&mut self, output: &str, word: DataWord) {
        self.emits.push(Emit::Word {
            output: output.to_owned(),
            word,
        });
    }

    /// An interval of an `intervals` output.
    pub fn interval(&mut self, output: &str, interval: DataInterval) {
        self.emits.push(Emit::Interval {
            output: output.to_owned(),
            interval,
        });
    }

    /// An emit as it is, such as one parsed from a client's JSON.
    pub fn push(&mut self, emit: Emit) {
        self.emits.push(emit);
    }

    /// The emits collected so far, in order.
    #[must_use]
    pub fn emits(&self) -> &[Emit] {
        &self.emits
    }

    fn take(&mut self) -> Vec<Emit> {
        std::mem::take(&mut self.emits)
    }
}

/// A function that is supplied frames or audio and returns primitives. Runs ahead of time over
/// a range; what it emits is recorded as data sets.
pub trait Extractor {
    /// What the extractor reads and emits.
    fn describe(&self) -> &Descriptor;

    /// The run begins; model loading belongs here.
    ///
    /// # Errors
    /// The extractor's failure, which ends the run.
    fn begin(&mut self, context: &RunContext) -> Result<(), ExtractorError>;

    /// A pass begins.
    ///
    /// # Errors
    /// The extractor's failure, which ends the run.
    fn pass(&mut self, pass: &PassInfo) -> Result<(), ExtractorError>;

    /// A video frame of the current pass.
    ///
    /// # Errors
    /// The extractor's failure, which ends the run.
    fn video(&mut self, frame: &VideoInput, out: &mut Emitter) -> Result<(), ExtractorError>;

    /// Memory of exactly `len` bytes for the next supplied frame's delivered pixels, for an
    /// extractor that would otherwise copy them. When it gives some and the frame is written
    /// there, the `video` call that delivers it gets empty [`VideoInput::bytes`]. `None`, the
    /// default, keeps the pixels in `bytes`.
    fn frame_buffer(&mut self, len: usize) -> Option<&mut [u8]> {
        let _ = len;
        None
    }

    /// An audio block of the current pass.
    ///
    /// # Errors
    /// The extractor's failure, which ends the run.
    fn audio(&mut self, block: &AudioInput, out: &mut Emitter) -> Result<(), ExtractorError>;

    /// Every pass ended; whole-range work belongs here and may emit any time in the range.
    ///
    /// # Errors
    /// The extractor's failure, which ends the run.
    fn end(&mut self, out: &mut Emitter) -> Result<(), ExtractorError>;
}

/// Supplies the inputs a session asks for: decoded and presented frames, or audio.
pub trait Supplier {
    /// The frame a [`Need::SourceFrame`] or [`Need::TimelineFrame`] asks for, or `None` when no
    /// such source frame exists.
    ///
    /// # Errors
    /// A decode or render failure, which ends the run.
    fn frame(&mut self, need: &Need) -> Result<Option<SuppliedFrame>, String>;

    /// [`Self::frame`] for a [`Need`] of RGBA frames, writing the pixels into the memory
    /// [`FramePlace::buffer`] gives for their length when it gives some; the returned frame's
    /// `rgba` is then empty. The default supplies the frame as [`Self::frame`] does.
    ///
    /// # Errors
    /// As [`Self::frame`].
    fn frame_into(
        &mut self,
        need: &Need,
        place: &mut FramePlace<'_>,
    ) -> Result<Option<SuppliedFrame>, String> {
        let _ = place;
        self.frame(need)
    }

    /// The block a [`Need::SourceAudio`] or [`Need::TimelineAudio`] asks for.
    ///
    /// # Errors
    /// A decode or mix failure, which ends the run.
    fn audio(&mut self, need: &Need) -> Result<SuppliedAudio, String>;
}

/// Why a run did not complete.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum DriveError {
    /// The extractor failed.
    Extractor(ExtractorError),
    /// The supplier failed.
    Supplier(String),
    /// The session refused a supply or an emit, or the result.
    Session(SessionError),
}

impl fmt::Display for DriveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Extractor(error) => write!(formatter, "extractor failed: {error}"),
            Self::Supplier(error) => write!(formatter, "supplier failed: {error}"),
            Self::Session(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for DriveError {}

impl From<SessionError> for DriveError {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}

/// Where a supplier may write a frame's pixels: memory the extractor offers.
pub struct FramePlace<'a> {
    extractor: &'a mut dyn Extractor,
    placed: Option<usize>,
}

impl FramePlace<'_> {
    /// The extractor's memory for a frame of `len` bytes, which the supplier then fills; `None`
    /// when it offers none, and the frame goes in `rgba`.
    pub fn buffer(&mut self, len: usize) -> Option<&mut [u8]> {
        let buffer = self.extractor.frame_buffer(len)?;
        self.placed = Some(len);
        Some(buffer)
    }
}

/// Answers a frame need of `session` from `supplier`, in memory `extractor` offers when it
/// offers some: the supplier writes RGBA frames there, the session a frame's luma; [`drive`]
/// does this for every frame need.
///
/// # Errors
/// [`DriveError::Supplier`] for the supplier's failure, [`DriveError::Session`] for a frame the
/// session refuses.
pub fn supply_frame(
    session: &mut ExtractSession,
    need: &Need,
    extractor: &mut dyn Extractor,
    supplier: &mut dyn Supplier,
) -> Result<(), DriveError> {
    let rgba = matches!(
        need,
        Need::SourceFrame {
            format: VideoFormat::Rgba8,
            ..
        } | Need::TimelineFrame {
            format: VideoFormat::Rgba8,
            ..
        }
    );
    if !rgba {
        let Some(frame) = supplier.frame(need).map_err(DriveError::Supplier)? else {
            return Ok(session.supply_end()?);
        };
        let luma = usize::try_from(u64::from(frame.width) * u64::from(frame.height)).ok();
        return match luma.and_then(|len| extractor.frame_buffer(len)) {
            Some(buffer) => Ok(session.supply_video_into(frame, buffer)?),
            None => Ok(session.supply_video(frame)?),
        };
    }
    let mut place = FramePlace {
        extractor,
        placed: None,
    };
    let frame = supplier
        .frame_into(need, &mut place)
        .map_err(DriveError::Supplier)?;
    match (frame, place.placed) {
        (None, _) => Ok(session.supply_end()?),
        (Some(frame), None) => Ok(session.supply_video(frame)?),
        (Some(frame), Some(len)) => {
            let packed = u64::from(frame.width) * u64::from(frame.height) * 4;
            if u64::try_from(len).ok() != Some(packed) {
                return Err(DriveError::Supplier(format!(
                    "a frame of {}x{} placed in {len} bytes",
                    frame.width, frame.height
                )));
            }
            Ok(session.supply_placed_video(frame)?)
        }
    }
}

/// Runs `extractor` through `session`, supplying inputs from `supplier`, and returns the
/// session's data sets.
///
/// # Errors
/// [`DriveError`] for the first failure of the extractor, the supplier or the session.
pub fn drive(
    mut session: ExtractSession,
    extractor: &mut dyn Extractor,
    supplier: &mut dyn Supplier,
) -> Result<Vec<DataSet>, DriveError> {
    let mut out = Emitter::default();
    extractor
        .begin(&session.context())
        .map_err(DriveError::Extractor)?;
    loop {
        match session.next() {
            Step::Need(need) => match &need {
                Need::SourceFrame { .. } | Need::TimelineFrame { .. } => {
                    supply_frame(&mut session, &need, extractor, supplier)?;
                }
                Need::SourceAudio { .. } | Need::TimelineAudio { .. } => {
                    let block = supplier.audio(&need).map_err(DriveError::Supplier)?;
                    session.supply_audio(block)?;
                }
            },
            Step::Deliver(delivery) => {
                match &delivery {
                    Delivery::Video(frame) => extractor.video(frame, &mut out),
                    Delivery::Audio(block) => extractor.audio(block, &mut out),
                }
                .map_err(DriveError::Extractor)?;
                session.emit(&out.take())?;
            }
            Step::PassBoundary { pass, direction } => {
                extractor
                    .pass(&PassInfo { pass, direction })
                    .map_err(DriveError::Extractor)?;
            }
            Step::Done => break,
        }
    }
    extractor.end(&mut out).map_err(DriveError::Extractor)?;
    session.emit(&out.take())?;
    Ok(session.finish()?)
}

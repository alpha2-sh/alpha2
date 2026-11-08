//! Rust extractors, the inputs they are supplied, and the loop that runs them.

use super::descriptor::{Descriptor, Direction};
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
pub struct ExtractorError(pub String);

impl fmt::Display for ExtractorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
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
                    match supplier.frame(&need).map_err(DriveError::Supplier)? {
                        Some(frame) => session.supply_video(frame)?,
                        None => session.supply_end()?,
                    }
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

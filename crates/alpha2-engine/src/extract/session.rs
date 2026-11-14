//! The host-driven step machine one run goes through.

use super::descriptor::{
    AudioChannels, BLOCK_FRAMES_RANGE, Descriptor, Direction, ExtractRequest, InputFrom, InputSpec,
    MAX_EXTRACTOR_OUTPUTS, MAX_EXTRACTOR_PASSES, OutputSpec, Pass, VideoFormat,
};
use super::pixels::luma8_from_rgba8;
use alpha2_media::SourceId;
use alpha2_model::{
    DataAnchor, DataInterval, DataKind, DataSamples, DataSet, DataSetId, DataValues, DataWord,
    MadeBy, MediaKind, Project,
};
use alpha2_time::{FrameRate, MediaTime, TICKS_PER_SECOND, TimeRange};
use std::collections::{BTreeMap, HashSet};
use std::fmt;

/// Where a source frame is asked for, relative to what the pass has seen.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FrameFrom {
    /// The frame shown at this time, the last with pts at or before it, else the first frame.
    Start(MediaTime),
    /// The next frame after the one with this pts, in the pass direction.
    After(MediaTime),
}

/// What the host must supply next.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Need {
    /// A source frame; [`ExtractSession::supply_video`] or [`ExtractSession::supply_end`] answers.
    SourceFrame {
        /// The source.
        source: SourceId,
        /// Which frame.
        from: FrameFrom,
        /// The pass direction `from` is relative to.
        direction: Direction,
        /// The longest edge of the delivered frame ([`super::delivered_size`]).
        size: u32,
        /// Always supplied as `rgba8`; the session converts.
        format: VideoFormat,
    },
    /// Source audio, answered by [`ExtractSession::supply_audio`]: exactly `frames` frames,
    /// silence past the end of the stream.
    SourceAudio {
        /// The source.
        source: SourceId,
        /// Its audio stream ordinal.
        stream: u32,
        /// The first frame, in samples at `rate` from source time zero.
        start_sample: i64,
        /// How many frames.
        frames: u32,
        /// The sample rate.
        rate: u32,
        /// The channel layout.
        channels: AudioChannels,
    },
    /// A rendered timeline frame, answered by [`ExtractSession::supply_video`].
    TimelineFrame {
        /// The frame's timeline time.
        time: MediaTime,
        /// The longest edge of the delivered frame.
        size: u32,
        /// Always supplied as `rgba8`; the session converts.
        format: VideoFormat,
    },
    /// Timeline mix audio, answered like [`Need::SourceAudio`].
    TimelineAudio {
        /// The first frame, in samples at `rate` from timeline time zero.
        start_sample: i64,
        /// How many frames.
        frames: u32,
        /// The sample rate.
        rate: u32,
        /// The channel layout.
        channels: AudioChannels,
    },
}

/// A supplied video frame: tightly packed straight-alpha sRGB RGBA8 at its delivered size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SuppliedFrame {
    /// The frame's presentation time on the input's clock.
    pub pts: MediaTime,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes, rows top to bottom.
    pub rgba: Vec<u8>,
}

/// A supplied audio block: planar f32, channel after channel.
#[derive(Clone, Debug, PartialEq)]
pub struct SuppliedAudio {
    /// The channel count.
    pub channels: u32,
    /// `channels * frames` samples.
    pub samples: Vec<f32>,
}

/// A frame handed to the extractor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoInput {
    /// Presentation time on the input's clock.
    pub time: MediaTime,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The pixel format of `bytes`.
    pub format: VideoFormat,
    /// Tightly packed pixels, rows top to bottom.
    pub bytes: Vec<u8>,
}

/// An audio block handed to the extractor.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioInput {
    /// The block's first sample, at `rate` from the clock's zero.
    pub start_sample: i64,
    /// The block's first sample time, the tick at or before it.
    pub start: MediaTime,
    /// Frames in the block.
    pub frames: u32,
    /// The sample rate.
    pub rate: u32,
    /// The channel count.
    pub channels: u32,
    /// `channels * frames` planar samples, channel after channel.
    pub samples: Vec<f32>,
}

/// An input handed to the extractor.
#[derive(Clone, Debug, PartialEq)]
pub enum Delivery {
    /// A video frame.
    Video(VideoInput),
    /// An audio block.
    Audio(AudioInput),
}

/// What the session asks for or hands over next.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// The host supplies this.
    Need(Need),
    /// The extractor reads this and emits through [`ExtractSession::emit`].
    Deliver(Delivery),
    /// A pass begins.
    PassBoundary {
        /// The pass's position in the request.
        pass: usize,
        /// Its direction.
        direction: Direction,
    },
    /// Every pass ended; the extractor's end and [`ExtractSession::finish`] follow.
    Done,
}

/// One value an extractor emits.
#[derive(Clone, Debug, PartialEq)]
pub enum Emit {
    /// A sample of a `points`, `scalars` or `similarity` output.
    Sample {
        /// The output's name.
        output: String,
        /// The sample time on the input's clock.
        time: MediaTime,
        /// The sample's numbers, `None` for a lost channel.
        values: Vec<Option<f32>>,
    },
    /// A word of a `words` output.
    Word {
        /// The output's name.
        output: String,
        /// The word.
        word: DataWord,
    },
    /// An interval of an `intervals` output.
    Interval {
        /// The output's name.
        output: String,
        /// The interval.
        interval: DataInterval,
    },
}

impl Emit {
    fn output(&self) -> &str {
        match self {
            Self::Sample { output, .. }
            | Self::Word { output, .. }
            | Self::Interval { output, .. } => output,
        }
    }
}

/// Why a session could not start.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RequestError {
    /// The request names another extractor or version than the descriptor.
    WrongExtractor,
    /// The descriptor breaks a rule (outputs, block size, size).
    InvalidDescriptor(String),
    /// The request reads an input the descriptor does not, or a source the project lacks or
    /// that cannot feed the input.
    InvalidInput(String),
    /// The range is empty or negative, or a pass starts outside it.
    InvalidRange,
    /// `size` is zero or above the descriptor's `max_size`.
    InvalidSize,
    /// The audio rate or channels conflict with the descriptor's fixed ones, or are invalid.
    InvalidAudio,
    /// The outputs map names an undeclared output, or repeats a data set id.
    InvalidOutputs(String),
}

impl fmt::Display for RequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongExtractor => {
                formatter.write_str("the request names another extractor or version")
            }
            Self::InvalidDescriptor(why) => write!(formatter, "invalid descriptor: {why}"),
            Self::InvalidInput(why) => write!(formatter, "invalid input: {why}"),
            Self::InvalidRange => {
                formatter.write_str("the range is empty or negative, or a pass starts outside it")
            }
            Self::InvalidSize => {
                formatter.write_str("size is zero or above the descriptor's max_size")
            }
            Self::InvalidAudio => formatter.write_str(
                "the audio rate or channels are invalid or conflict with the descriptor",
            ),
            Self::InvalidOutputs(why) => write!(formatter, "invalid outputs: {why}"),
        }
    }
}

impl std::error::Error for RequestError {}

/// Why the session refused something.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum SessionError {
    /// A supply that does not answer the outstanding need.
    UnexpectedSupply(String),
    /// An emit that breaks a rule.
    InvalidEmit {
        /// Its position among the emits handed over at once.
        index: usize,
        /// The rule broken.
        reason: String,
    },
    /// `finish` before the run is done, or a time emitted twice.
    InvalidResult(String),
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedSupply(why) => write!(formatter, "unexpected supply: {why}"),
            Self::InvalidEmit { index, reason } => {
                write!(formatter, "invalid emit {index}: {reason}")
            }
            Self::InvalidResult(why) => write!(formatter, "invalid result: {why}"),
        }
    }
}

impl std::error::Error for SessionError {}

/// Completed and total deliveries of one pass; the total is unknown for source frames.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PassProgress {
    /// Deliveries handed over.
    pub completed: u64,
    /// Deliveries the pass will hand over, when known.
    pub total: Option<u64>,
}

/// What the extractor is told when the run begins.
#[derive(Clone, Debug, PartialEq)]
pub struct RunContext {
    /// The interval read.
    pub range: TimeRange,
    /// The passes, in order.
    pub passes: Vec<Pass>,
    /// The clock read.
    pub from: InputFrom,
    /// For video, the longest edge of delivered frames and their format.
    pub video: Option<(u32, VideoFormat)>,
    /// For audio, the rate, channel layout and block size.
    pub audio: Option<(u32, AudioChannels, u32)>,
    /// For timeline frames, their rate.
    pub frame_rate: Option<FrameRate>,
}

#[derive(Clone, Debug)]
enum Input {
    SourceVideo {
        source: SourceId,
        size: u32,
        format: VideoFormat,
    },
    TimelineVideo {
        size: u32,
        format: VideoFormat,
        rate: FrameRate,
    },
    SourceAudio {
        source: SourceId,
        stream: u32,
        rate: u32,
        channels: AudioChannels,
        block: u32,
    },
    TimelineAudio {
        rate: u32,
        channels: AudioChannels,
        block: u32,
    },
}

/// Where the session is.
#[derive(Clone, Debug)]
enum Phase {
    /// The next pass begins with a boundary.
    BeforePass(usize),
    /// In a pass, asking for its next input.
    Running(Cursor),
    /// A need is outstanding.
    Waiting(Cursor, Need),
    /// A supplied input waits to be handed over; the cursor continues after it.
    Delivering(Cursor, Delivery),
    /// All passes ended.
    Done,
    /// The result was taken.
    Finished,
}

#[derive(Clone, Copy, Debug)]
struct Cursor {
    pass: usize,
    /// The last delivered pts for source frames, the next frame index for timeline frames, and the
    /// next block's first sample (forward) or exclusive end (backward) for audio.
    position: Option<i64>,
}

/// The emits of one output, gathered across passes.
#[derive(Default, Debug)]
struct Gathered {
    samples: Vec<(MediaTime, Vec<Option<f32>>)>,
    words: Vec<DataWord>,
    intervals: Vec<DataInterval>,
}

impl Gathered {
    /// Items of the one kind an output gathers.
    fn len(&self) -> usize {
        self.samples.len() + self.words.len() + self.intervals.len()
    }
}

/// Why an emit to `output` was refused at its data limit.
fn over_limit(output: &OutputSpec) -> String {
    let (limit, what) = match output.kind {
        DataKind::Points | DataKind::Scalars | DataKind::Similarity => {
            (alpha2_model::MAX_DATA_VALUES, "values")
        }
        DataKind::Words => (alpha2_model::MAX_DATA_WORDS, "words"),
        DataKind::Intervals => (alpha2_model::MAX_DATA_INTERVALS, "intervals"),
    };
    format!("output {} would hold more than {limit} {what}", output.name)
}

/// One run of an extractor over a range, driven by the host.
///
/// The host loops on [`Self::next`]: it supplies each [`Need`], hands each [`Delivery`] to the
/// extractor and passes the extractor's [`Emit`]s to [`Self::emit`], and stops at
/// [`Step::Done`]. Cancelling is not calling `next` again. [`Self::finish`] returns validated
/// data sets. The session never decodes, renders or reads files.
#[derive(Debug)]
pub struct ExtractSession {
    descriptor: Descriptor,
    request: ExtractRequest,
    input: Input,
    anchor: DataAnchor,
    passes: Vec<Pass>,
    phase: Phase,
    progress: Vec<PassProgress>,
    /// The latest delivery's bound in the current pass, a frame pts or a block's edge.
    delivered: Option<MediaTime>,
    /// The pts of every frame delivered, which emits may name even before the range.
    delivered_pts: HashSet<MediaTime>,
    /// Per output, the latest emitted time in the current pass.
    last_time: BTreeMap<String, MediaTime>,
    gathered: BTreeMap<String, Gathered>,
}

pub(super) fn validate_descriptor(descriptor: &Descriptor) -> Result<(), RequestError> {
    let invalid = |why: &str| Err(RequestError::InvalidDescriptor(why.to_owned()));
    if descriptor.id.is_empty() {
        return invalid("empty id");
    }
    if descriptor.outputs.is_empty() || descriptor.outputs.len() > MAX_EXTRACTOR_OUTPUTS {
        return invalid("needs 1 to 32 outputs");
    }
    let mut names = HashSet::new();
    for output in &descriptor.outputs {
        if output.name.is_empty() || !names.insert(&output.name) {
            return invalid("an output name is empty or repeated");
        }
        let sampled = matches!(output.kind, DataKind::Points | DataKind::Scalars);
        if sampled == output.channels.is_empty() {
            return invalid("points and scalars outputs need channels; others take none");
        }
        if output.channels.len() > alpha2_model::MAX_DATA_CHANNELS {
            return invalid("an output has too many channels");
        }
        let mut channels = HashSet::new();
        if output.channels.iter().any(|channel| {
            channel.is_empty()
                || channel.len() > alpha2_model::MAX_DATA_LABEL_BYTES
                || !channels.insert(channel)
        }) {
            return invalid("a channel name is empty, too long or repeated");
        }
        if output.interpolation.is_some() && output.kind.arity().is_none() {
            return invalid("only sampled outputs take an interpolation");
        }
    }
    if descriptor.input.from().is_empty() {
        return invalid("reads no input");
    }
    match &descriptor.input {
        InputSpec::Video { max_size, .. } if *max_size == 0 => invalid("max_size is zero"),
        InputSpec::Audio {
            block_frames,
            sample_rate,
            ..
        } if !BLOCK_FRAMES_RANGE.contains(block_frames)
            || sample_rate.is_some_and(|rate| !alpha2_model::SAMPLE_RATE_RANGE.contains(&rate)) =>
        {
            invalid("block_frames or sample_rate out of range")
        }
        _ => Ok(()),
    }
}

impl ExtractSession {
    /// Starts a run of the extractor `descriptor` describes, as `request` asks, over `project`.
    ///
    /// # Errors
    /// [`RequestError`] when the descriptor or request breaks a rule.
    #[allow(clippy::too_many_lines)]
    pub fn new(
        project: &Project,
        descriptor: &Descriptor,
        request: ExtractRequest,
    ) -> Result<Self, RequestError> {
        validate_descriptor(descriptor)?;
        if request.extractor != descriptor.id || request.version != descriptor.version {
            return Err(RequestError::WrongExtractor);
        }
        let read = &request.input;
        if !descriptor.input.from().contains(&read.from) {
            return Err(RequestError::InvalidInput(
                "the descriptor does not read this input".to_owned(),
            ));
        }
        let range = read.range;
        if range.is_empty() || range.start().ticks() < 0 {
            return Err(RequestError::InvalidRange);
        }
        if read.passes.len() > MAX_EXTRACTOR_PASSES
            || read.passes.iter().any(|pass| !range.contains(pass.start))
        {
            return Err(RequestError::InvalidRange);
        }
        let source = match read.from {
            InputFrom::Source => {
                let id = read.source_id.as_ref().ok_or_else(|| {
                    RequestError::InvalidInput("source input needs source_id".to_owned())
                })?;
                let source = project
                    .sources
                    .iter()
                    .find(|source| &source.id == id)
                    .ok_or_else(|| RequestError::InvalidInput(format!("unknown source {id}")))?;
                let fits = match &descriptor.input {
                    InputSpec::Video { .. } => {
                        matches!(source.kind, MediaKind::Video | MediaKind::Image)
                    }
                    InputSpec::Audio { .. } => source.kind == MediaKind::Audio || source.has_audio,
                };
                if !fits {
                    return Err(RequestError::InvalidInput(format!(
                        "source {id} cannot feed this input"
                    )));
                }
                Some(id.clone())
            }
            InputFrom::Timeline => {
                if read.source_id.is_some() {
                    return Err(RequestError::InvalidInput(
                        "timeline input takes no source_id".to_owned(),
                    ));
                }
                None
            }
        };
        let input = match (&descriptor.input, source.clone()) {
            (
                InputSpec::Video {
                    format, max_size, ..
                },
                source,
            ) => {
                if read.stream.is_some() || read.sample_rate.is_some() || read.channels.is_some() {
                    return Err(RequestError::InvalidInput(
                        "video input takes no audio fields".to_owned(),
                    ));
                }
                let size = read.size.unwrap_or(*max_size);
                if size == 0 || size > *max_size {
                    return Err(RequestError::InvalidSize);
                }
                match source {
                    Some(source) => {
                        if read.frame_rate.is_some() {
                            return Err(RequestError::InvalidInput(
                                "source input takes no frame_rate".to_owned(),
                            ));
                        }
                        Input::SourceVideo {
                            source,
                            size,
                            format: *format,
                        }
                    }
                    None => Input::TimelineVideo {
                        size,
                        format: *format,
                        rate: read.frame_rate.unwrap_or(project.settings.frame_rate),
                    },
                }
            }
            (
                InputSpec::Audio {
                    sample_rate,
                    channels,
                    block_frames,
                    ..
                },
                source,
            ) => {
                if read.size.is_some() || read.frame_rate.is_some() {
                    return Err(RequestError::InvalidInput(
                        "audio input takes no video fields".to_owned(),
                    ));
                }
                let rate = match (sample_rate, read.sample_rate) {
                    (Some(fixed), Some(asked)) if *fixed != asked => {
                        return Err(RequestError::InvalidAudio);
                    }
                    (Some(fixed), _) => *fixed,
                    (None, Some(asked)) => asked,
                    (None, None) => project.settings.sample_rate,
                };
                if !alpha2_model::SAMPLE_RATE_RANGE.contains(&rate) {
                    return Err(RequestError::InvalidAudio);
                }
                let layout = match (channels, read.channels) {
                    (Some(fixed), Some(asked)) if *fixed != asked => {
                        return Err(RequestError::InvalidAudio);
                    }
                    (Some(fixed), _) => *fixed,
                    (None, Some(asked)) => asked,
                    (None, None) => AudioChannels::Mono,
                };
                if let Some(source) = source {
                    Input::SourceAudio {
                        source,
                        stream: read.stream.unwrap_or(0),
                        rate,
                        channels: layout,
                        block: *block_frames,
                    }
                } else {
                    if read.stream.is_some() {
                        return Err(RequestError::InvalidInput(
                            "timeline input takes no stream".to_owned(),
                        ));
                    }
                    Input::TimelineAudio {
                        rate,
                        channels: layout,
                        block: *block_frames,
                    }
                }
            }
        };
        let mut ids = HashSet::new();
        for (name, id) in &request.outputs {
            if !descriptor.outputs.iter().any(|output| &output.name == name) {
                return Err(RequestError::InvalidOutputs(format!(
                    "undeclared output {name}"
                )));
            }
            if !ids.insert(id) {
                return Err(RequestError::InvalidOutputs(format!(
                    "data set id {id} repeated"
                )));
            }
        }
        let passes = if read.passes.is_empty() {
            vec![Pass {
                start: range.start(),
                direction: Direction::Forward,
            }]
        } else {
            read.passes.clone()
        };
        let anchor = source.map_or(DataAnchor::Timeline, |source_id| DataAnchor::Source {
            source_id,
        });
        let mut session = Self {
            descriptor: descriptor.clone(),
            request,
            input,
            anchor,
            progress: Vec::new(),
            passes,
            phase: Phase::BeforePass(0),
            delivered: None,
            delivered_pts: HashSet::new(),
            last_time: BTreeMap::new(),
            gathered: BTreeMap::new(),
        };
        session.progress = (0..session.passes.len())
            .map(|pass| PassProgress {
                completed: 0,
                total: session.pass_total(pass),
            })
            .collect();
        Ok(session)
    }

    /// The run's context, for the extractor's `begin`.
    #[must_use]
    pub fn context(&self) -> RunContext {
        let (video, audio, frame_rate) = match &self.input {
            Input::SourceVideo { size, format, .. } => (Some((*size, *format)), None, None),
            Input::TimelineVideo { size, format, rate } => {
                (Some((*size, *format)), None, Some(*rate))
            }
            Input::SourceAudio {
                rate,
                channels,
                block,
                ..
            }
            | Input::TimelineAudio {
                rate,
                channels,
                block,
            } => (None, Some((*rate, *channels, *block)), None),
        };
        RunContext {
            range: self.request.input.range,
            passes: self.passes.clone(),
            from: self.request.input.from,
            video,
            audio,
            frame_rate,
        }
    }

    /// The descriptor the session runs.
    #[must_use]
    pub const fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    /// The request the session runs.
    #[must_use]
    pub const fn request(&self) -> &ExtractRequest {
        &self.request
    }

    /// Completed and total deliveries of each pass.
    #[must_use]
    pub fn progress(&self) -> &[PassProgress] {
        &self.progress
    }

    fn range(&self) -> TimeRange {
        self.request.input.range
    }

    /// Samples of the audio grid at `rate` whose start lies in the range: `[first, last)`.
    fn sample_span(&self, rate: u32) -> (i64, i64) {
        let range = self.range();
        (
            samples_ceil(range.start(), rate),
            samples_ceil(range.end(), rate),
        )
    }

    fn pass_total(&self, pass: usize) -> Option<u64> {
        let Pass { start, direction } = self.passes[pass];
        match &self.input {
            Input::SourceVideo { .. } => None,
            Input::TimelineVideo { rate, .. } => {
                let (first, last) = timeline_frames(self.range(), *rate)?;
                let at = start_frame(start, *rate, first, last)?;
                let count = match direction {
                    Direction::Forward => last - at,
                    Direction::Backward => at - first + 1,
                };
                u64::try_from(count).ok()
            }
            Input::SourceAudio { rate, block, .. } | Input::TimelineAudio { rate, block, .. } => {
                let (first, last) = self.sample_span(*rate);
                let at = samples_ceil(start, *rate).clamp(first, last);
                let span = match direction {
                    Direction::Forward => last - at,
                    Direction::Backward => at - first,
                };
                u64::try_from(span)
                    .ok()
                    .map(|span| span.div_ceil(u64::from(*block)))
            }
        }
    }

    /// The next step. Calling it again before supplying an outstanding need repeats the need.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Step {
        loop {
            match std::mem::replace(&mut self.phase, Phase::Done) {
                Phase::BeforePass(pass) if pass >= self.passes.len() => {
                    self.phase = Phase::Done;
                    self.last_time.clear();
                    self.delivered = None;
                    return Step::Done;
                }
                Phase::BeforePass(pass) => {
                    self.phase = Phase::Running(Cursor {
                        pass,
                        position: None,
                    });
                    self.delivered = None;
                    self.last_time.clear();
                    return Step::PassBoundary {
                        pass,
                        direction: self.passes[pass].direction,
                    };
                }
                Phase::Running(cursor) => match self.need_at(cursor) {
                    Some(need) => {
                        self.phase = Phase::Waiting(cursor, need.clone());
                        return Step::Need(need);
                    }
                    None => self.phase = Phase::BeforePass(cursor.pass + 1),
                },
                Phase::Waiting(cursor, need) => {
                    self.phase = Phase::Waiting(cursor, need.clone());
                    return Step::Need(need);
                }
                Phase::Delivering(cursor, delivery) => {
                    self.progress[cursor.pass].completed += 1;
                    self.phase = Phase::Running(cursor);
                    return Step::Deliver(delivery);
                }
                Phase::Done => {
                    self.phase = Phase::Done;
                    return Step::Done;
                }
                Phase::Finished => {
                    self.phase = Phase::Finished;
                    return Step::Done;
                }
            }
        }
    }

    /// The need at `cursor`, or `None` when its pass has ended.
    fn need_at(&self, cursor: Cursor) -> Option<Need> {
        let Pass { start, direction } = self.passes[cursor.pass];
        match &self.input {
            Input::SourceVideo {
                source,
                size,
                format,
            } => {
                if cursor.position == Some(i64::MIN) {
                    return None;
                }
                let from = cursor.position.map_or(FrameFrom::Start(start), |pts| {
                    FrameFrom::After(MediaTime(pts))
                });
                Some(Need::SourceFrame {
                    source: source.clone(),
                    from,
                    direction,
                    size: *size,
                    format: *format,
                })
            }
            Input::TimelineVideo { size, format, rate } => {
                let (first, last) = timeline_frames(self.range(), *rate)?;
                let frame = match cursor.position {
                    Some(frame) => frame,
                    None => start_frame(start, *rate, first, last)?,
                };
                if frame < first || frame >= last {
                    return None;
                }
                Some(Need::TimelineFrame {
                    time: rate.frame_start(frame)?,
                    size: *size,
                    format: *format,
                })
            }
            Input::SourceAudio {
                rate,
                block,
                channels,
                ..
            }
            | Input::TimelineAudio {
                rate,
                block,
                channels,
            } => {
                let (first, last) = self.sample_span(*rate);
                let at = cursor
                    .position
                    .unwrap_or_else(|| samples_ceil(start, *rate).clamp(first, last));
                let block = i64::from(*block);
                let (start_sample, end_sample) = match direction {
                    Direction::Forward => (at, (at + block).min(last)),
                    Direction::Backward => ((at - block).max(first), at),
                };
                if start_sample >= end_sample {
                    return None;
                }
                let frames = u32::try_from(end_sample - start_sample).ok()?;
                Some(match &self.input {
                    Input::SourceAudio { source, stream, .. } => Need::SourceAudio {
                        source: source.clone(),
                        stream: *stream,
                        start_sample,
                        frames,
                        rate: *rate,
                        channels: *channels,
                    },
                    _ => Need::TimelineAudio {
                        start_sample,
                        frames,
                        rate: *rate,
                        channels: *channels,
                    },
                })
            }
        }
    }

    /// Answers an outstanding frame need.
    ///
    /// # Errors
    /// [`SessionError::UnexpectedSupply`] when no frame is asked for, or the frame's size, buffer
    /// or pts does not answer the need.
    pub fn supply_video(&mut self, frame: SuppliedFrame) -> Result<(), SessionError> {
        let Phase::Waiting(cursor, need) = &self.phase else {
            return Err(SessionError::UnexpectedSupply(
                "no frame is asked for".to_owned(),
            ));
        };
        let (cursor, need) = (*cursor, need.clone());
        let (size, format, expected_pts, from) = match &need {
            Need::SourceFrame {
                size, format, from, ..
            } => (*size, *format, None, Some(*from)),
            Need::TimelineFrame { time, size, format } => (*size, *format, Some(*time), None),
            _ => {
                return Err(SessionError::UnexpectedSupply(
                    "audio is asked for".to_owned(),
                ));
            }
        };
        if frame.width == 0 || frame.height == 0 || frame.width.max(frame.height) > size {
            return Err(SessionError::UnexpectedSupply(
                "the frame's size breaks the asked size".to_owned(),
            ));
        }
        let pixels = u64::from(frame.width) * u64::from(frame.height);
        if u64::try_from(frame.rgba.len()).ok() != pixels.checked_mul(4) {
            return Err(SessionError::UnexpectedSupply(
                "the buffer is not width * height * 4 bytes".to_owned(),
            ));
        }
        if expected_pts.is_some_and(|time| time != frame.pts) {
            return Err(SessionError::UnexpectedSupply(
                "the frame is not the asked time".to_owned(),
            ));
        }
        let Pass { direction, .. } = self.passes[cursor.pass];
        if let Some(FrameFrom::After(previous)) = from {
            let ordered = match direction {
                Direction::Forward => frame.pts > previous,
                Direction::Backward => frame.pts < previous,
            };
            if !ordered {
                return Err(SessionError::UnexpectedSupply(
                    "the frame does not follow the last in the pass direction".to_owned(),
                ));
            }
        }
        let range = self.range();
        // Which frames are shown inside the range, and when a pass ends.
        let (deliver, next) = match &self.input {
            Input::SourceVideo { .. } => match direction {
                Direction::Forward => {
                    if frame.pts >= range.end() {
                        (false, Some(i64::MIN))
                    } else {
                        (true, Some(frame.pts.ticks()))
                    }
                }
                Direction::Backward => {
                    let last = frame.pts <= range.start();
                    (true, Some(if last { i64::MIN } else { frame.pts.ticks() }))
                }
            },
            Input::TimelineVideo { rate, .. } => {
                let frame_index = rate.frame_at_or_before(frame.pts);
                let step = match direction {
                    Direction::Forward => 1,
                    Direction::Backward => -1,
                };
                (true, frame_index.map(|index| index + step))
            }
            _ => unreachable!("a frame need comes only from video input"),
        };
        if !deliver {
            self.phase = Phase::Running(Cursor {
                pass: cursor.pass,
                position: next,
            });
            return Ok(());
        }
        self.delivered = Some(frame.pts);
        self.delivered_pts.insert(frame.pts);
        let bytes = match format {
            VideoFormat::Rgba8 => frame.rgba,
            VideoFormat::Luma8 => luma8_from_rgba8(&frame.rgba),
        };
        let delivery = Delivery::Video(VideoInput {
            time: frame.pts,
            width: frame.width,
            height: frame.height,
            format,
            bytes,
        });
        self.phase = Phase::Delivering(
            Cursor {
                pass: cursor.pass,
                position: next,
            },
            delivery,
        );
        Ok(())
    }

    /// Answers an outstanding source-frame need: no such frame exists, so the pass ends.
    ///
    /// # Errors
    /// [`SessionError::UnexpectedSupply`] when no source frame is asked for.
    pub fn supply_end(&mut self) -> Result<(), SessionError> {
        match &self.phase {
            Phase::Waiting(cursor, Need::SourceFrame { .. }) => {
                self.phase = Phase::Running(Cursor {
                    pass: cursor.pass,
                    position: Some(i64::MIN),
                });
                Ok(())
            }
            _ => Err(SessionError::UnexpectedSupply(
                "no source frame is asked for".to_owned(),
            )),
        }
    }

    /// Answers an outstanding audio need with exactly the asked frames.
    ///
    /// # Errors
    /// [`SessionError::UnexpectedSupply`] when no audio is asked for, or the block's channel
    /// count or length does not answer the need.
    pub fn supply_audio(&mut self, block: SuppliedAudio) -> Result<(), SessionError> {
        let Phase::Waiting(cursor, need) = &self.phase else {
            return Err(SessionError::UnexpectedSupply(
                "no audio is asked for".to_owned(),
            ));
        };
        let (cursor, need) = (*cursor, need.clone());
        let (Need::SourceAudio {
            start_sample,
            frames,
            rate,
            channels,
            ..
        }
        | Need::TimelineAudio {
            start_sample,
            frames,
            rate,
            channels,
        }) = need
        else {
            return Err(SessionError::UnexpectedSupply(
                "a frame is asked for".to_owned(),
            ));
        };
        if block.channels == 0
            || (channels == AudioChannels::Mono && block.channels != 1)
            || block.channels > u32::try_from(alpha2_media::MAX_AUDIO_CHANNELS).unwrap_or(u32::MAX)
        {
            return Err(SessionError::UnexpectedSupply(
                "the block's channel count does not answer the need".to_owned(),
            ));
        }
        if block.samples.len()
            != usize::try_from(block.channels).unwrap_or(0) * usize::try_from(frames).unwrap_or(0)
        {
            return Err(SessionError::UnexpectedSupply(
                "the block is not channels * frames samples".to_owned(),
            ));
        }
        if block.samples.iter().any(|sample| !sample.is_finite()) {
            return Err(SessionError::UnexpectedSupply(
                "the block holds a sample that is not finite".to_owned(),
            ));
        }
        let end_sample = start_sample + i64::from(frames);
        let direction = self.passes[cursor.pass].direction;
        self.delivered = Some(MediaTime(match direction {
            Direction::Forward => samples_to_ticks_ceil(end_sample, rate),
            Direction::Backward => alpha2_media::sample_ticks_floor(start_sample, rate),
        }));
        let next = match direction {
            Direction::Forward => end_sample,
            Direction::Backward => start_sample,
        };
        let delivery = Delivery::Audio(AudioInput {
            start_sample,
            start: MediaTime(alpha2_media::sample_ticks_floor(start_sample, rate)),
            frames,
            rate,
            channels: block.channels,
            samples: block.samples,
        });
        self.phase = Phase::Delivering(
            Cursor {
                pass: cursor.pass,
                position: Some(next),
            },
            delivery,
        );
        Ok(())
    }

    /// Validates and records emits; on a refusal nothing of `emits` is recorded.
    ///
    /// During a pass, times lie inside the range (or are a delivered frame's pts), go no further
    /// than what the pass has delivered, and move in the pass direction per output. After
    /// [`Step::Done`] any time inside the range is accepted. A requested output holds at most
    /// [`OutputSpec::max_items`]; emits to a declared output the request does not name are
    /// checked and dropped.
    ///
    /// # Errors
    /// [`SessionError::InvalidEmit`] naming the first offending emit and the rule.
    pub fn emit(&mut self, emits: &[Emit]) -> Result<(), SessionError> {
        let mut last = self.last_time.clone();
        let mut added = BTreeMap::<&str, usize>::new();
        for (index, emit) in emits.iter().enumerate() {
            let invalid = |reason| SessionError::InvalidEmit { index, reason };
            self.check_emit(emit, &mut last).map_err(invalid)?;
            let name = emit.output();
            if !self.request.outputs.contains_key(name) {
                continue;
            }
            let count = added.entry(name).or_default();
            *count += 1;
            let held = self.gathered.get(name).map_or(0, Gathered::len);
            if let Some(output) = self.output(name)
                && held + *count > output.max_items()
            {
                return Err(invalid(over_limit(output)));
            }
        }
        self.last_time = last;
        for emit in emits {
            if !self.request.outputs.contains_key(emit.output()) {
                continue;
            }
            let gathered = self.gathered.entry(emit.output().to_owned()).or_default();
            match emit {
                Emit::Sample { time, values, .. } => gathered.samples.push((*time, values.clone())),
                Emit::Word { word, .. } => gathered.words.push(word.clone()),
                Emit::Interval { interval, .. } => gathered.intervals.push(interval.clone()),
            }
        }
        Ok(())
    }

    fn output(&self, name: &str) -> Option<&OutputSpec> {
        self.descriptor
            .outputs
            .iter()
            .find(|output| output.name == name)
    }

    #[allow(clippy::too_many_lines)]
    fn check_emit(
        &self,
        emit: &Emit,
        latest: &mut BTreeMap<String, MediaTime>,
    ) -> Result<(), String> {
        let output = self
            .output(emit.output())
            .ok_or_else(|| format!("undeclared output {}", emit.output()))?;
        let range = self.range();
        let in_pass = match &self.phase {
            Phase::Running(cursor) | Phase::Waiting(cursor, _) | Phase::Delivering(cursor, _) => {
                Some(cursor.pass)
            }
            Phase::Done => None,
            Phase::BeforePass(_) | Phase::Finished => {
                return Err("no input has been delivered".to_owned());
            }
        };
        let direction = in_pass.map(|pass| self.passes[pass].direction);
        let delivered = self.delivered;
        if in_pass.is_some() && delivered.is_none() {
            return Err("nothing has been delivered in this pass".to_owned());
        }
        let reached = |time: MediaTime| match (direction, delivered) {
            (Some(Direction::Forward), Some(bound)) => time <= bound,
            (Some(Direction::Backward), Some(bound)) => time >= bound,
            _ => true,
        };
        let (time, key) = match emit {
            Emit::Sample { time, values, .. } => {
                if output.kind.arity().is_none() {
                    return Err(format!(
                        "output {} takes {} items, not samples",
                        output.name, output.kind
                    ));
                }
                if values.len() != output.sample_width() {
                    return Err(format!(
                        "output {} takes {} numbers a sample",
                        output.name,
                        output.sample_width()
                    ));
                }
                let arity = output.kind.arity().unwrap_or(1);
                for (group, chunk) in values.chunks(arity).enumerate() {
                    let lost = chunk[0].is_none();
                    for (component, value) in chunk.iter().enumerate() {
                        let ok = match value {
                            None => lost,
                            Some(number) => {
                                !lost
                                    && alpha2_model::value_in_bounds(
                                        output.kind,
                                        component,
                                        *number,
                                    )
                            }
                        };
                        if !ok {
                            return Err(format!(
                                "value {} is lost in part, not finite or out of bounds",
                                group * arity + component
                            ));
                        }
                    }
                }
                let inside = (range.contains(*time) || self.delivered_pts.contains(time))
                    && time.ticks() >= 0;
                if !inside {
                    return Err("the time is outside the range".to_owned());
                }
                if !reached(*time) {
                    return Err("the time is beyond what this pass has delivered".to_owned());
                }
                (*time, true)
            }
            Emit::Word { word, .. } => {
                if output.kind != DataKind::Words {
                    return Err(format!("output {} takes no words", output.name));
                }
                if !alpha2_model::valid_word(word) {
                    return Err(
                        "the word is empty, too long, or does not end after it starts".to_owned(),
                    );
                }
                if word.start < range.start() || word.end > range.end() {
                    return Err("the word lies outside the range".to_owned());
                }
                if !reached(word.start) || !reached(word.end) {
                    return Err("the word is beyond what this pass has delivered".to_owned());
                }
                (word.start, false)
            }
            Emit::Interval { interval, .. } => {
                if output.kind != DataKind::Intervals {
                    return Err(format!("output {} takes no intervals", output.name));
                }
                if !alpha2_model::valid_interval(interval) {
                    return Err(
                        "the interval ends before it starts, or its label or score breaks a limit"
                            .to_owned(),
                    );
                }
                if interval.start < range.start() || interval.end > range.end() {
                    return Err("the interval lies outside the range".to_owned());
                }
                if !reached(interval.start) || !reached(interval.end) {
                    return Err("the interval is beyond what this pass has delivered".to_owned());
                }
                (interval.start, false)
            }
        };
        if let Some(direction) = direction
            && let Some(previous) = latest.get(&output.name)
        {
            // Samples move strictly; items by their starts, ties allowed.
            let ordered = match (direction, key) {
                (Direction::Forward, true) => time > *previous,
                (Direction::Backward, true) => time < *previous,
                (Direction::Forward, false) => time >= *previous,
                (Direction::Backward, false) => time <= *previous,
            };
            if !ordered {
                return Err("times do not move in the pass direction".to_owned());
            }
        }
        if direction.is_some() {
            latest.insert(output.name.clone(), time);
        }
        Ok(())
    }

    /// The recorded outputs as data sets: merged across passes, sorted, with `made_by` set and
    /// `made_from` left for the surface helper.
    ///
    /// # Errors
    /// [`SessionError::InvalidResult`] before [`Step::Done`], for a time emitted twice, or for a
    /// set that breaks a data limit.
    pub fn finish(&mut self) -> Result<Vec<DataSet>, SessionError> {
        if !matches!(self.phase, Phase::Done) {
            return Err(SessionError::InvalidResult(
                "the run is not done".to_owned(),
            ));
        }
        self.phase = Phase::Finished;
        let made_by = MadeBy {
            extractor: self.descriptor.id.clone(),
            version: self.descriptor.version,
            params: self.request.params.clone(),
            plugin: None,
        };
        let mut sets = Vec::new();
        for (name, id) in &self.request.outputs {
            let output = self
                .output(name)
                .cloned()
                .ok_or_else(|| SessionError::InvalidResult(format!("undeclared output {name}")))?;
            let gathered = self.gathered.remove(name).unwrap_or_default();
            let values = Self::values_of(&output, id, gathered)?;
            let set = DataSet {
                id: id.clone(),
                anchor: self.anchor.clone(),
                values,
                made_by: Some(made_by.clone()),
                made_from: None,
            };
            set.validate(None).map_err(|problem| {
                SessionError::InvalidResult(format!("data set {id} {problem}"))
            })?;
            sets.push(set);
        }
        Ok(sets)
    }

    fn values_of(
        output: &OutputSpec,
        id: &DataSetId,
        mut gathered: Gathered,
    ) -> Result<DataValues, SessionError> {
        Ok(match output.kind {
            DataKind::Points | DataKind::Scalars | DataKind::Similarity => {
                gathered.samples.sort_by_key(|(time, _)| *time);
                if let Some(pair) = gathered
                    .samples
                    .windows(2)
                    .find(|pair| pair[0].0 == pair[1].0)
                {
                    return Err(SessionError::InvalidResult(format!(
                        "output {} has time {} twice (data set {id})",
                        output.name,
                        pair[0].0.ticks()
                    )));
                }
                let samples = DataSamples {
                    interpolation: output.interpolation.unwrap_or_default(),
                    channels: output.channels.clone(),
                    times: gathered.samples.iter().map(|(time, _)| *time).collect(),
                    values: gathered
                        .samples
                        .into_iter()
                        .flat_map(|(_, values)| values)
                        .collect(),
                };
                match output.kind {
                    DataKind::Points => DataValues::Points(samples),
                    DataKind::Scalars => DataValues::Scalars(samples),
                    _ => DataValues::Similarity(samples),
                }
            }
            DataKind::Words => {
                gathered.words.sort_by_key(|word| (word.start, word.end));
                DataValues::Words(gathered.words)
            }
            DataKind::Intervals => {
                gathered
                    .intervals
                    .sort_by_key(|interval| (interval.start, interval.end));
                DataValues::Intervals(gathered.intervals)
            }
        })
    }
}

/// The first sample starting at or after `time` at `rate`.
fn samples_ceil(time: MediaTime, rate: u32) -> i64 {
    let numerator = i128::from(time.ticks()) * i128::from(rate);
    let denominator = i128::from(TICKS_PER_SECOND);
    let samples =
        numerator.div_euclid(denominator) + i128::from(numerator.rem_euclid(denominator) != 0);
    i64::try_from(samples).unwrap_or(i64::MAX)
}

/// The first tick at or after sample `sample` at `rate`.
fn samples_to_ticks_ceil(sample: i64, rate: u32) -> i64 {
    let numerator = i128::from(sample) * i128::from(TICKS_PER_SECOND);
    let denominator = i128::from(rate.max(1));
    let ticks =
        numerator.div_euclid(denominator) + i128::from(numerator.rem_euclid(denominator) != 0);
    i64::try_from(ticks).unwrap_or(i64::MAX)
}

/// The frames of `rate` whose start lies in `range`: `[first, last)`.
fn timeline_frames(range: TimeRange, rate: FrameRate) -> Option<(i64, i64)> {
    let first_at_or_before = rate.frame_at_or_before(range.start())?;
    let first = if rate.frame_start(first_at_or_before)? < range.start() {
        first_at_or_before + 1
    } else {
        first_at_or_before
    };
    let last = rate.frame_at_or_before(MediaTime(range.end().ticks() - 1))? + 1;
    Some((first, last.max(first)))
}

/// The frame a pass starting at `start` begins with, the one shown there, kept in `[first, last)`.
fn start_frame(start: MediaTime, rate: FrameRate, first: i64, last: i64) -> Option<i64> {
    let frame = rate.frame_at_or_before(start)?;
    (first < last).then(|| frame.clamp(first, last - 1))
}

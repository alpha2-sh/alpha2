//! Converts the audio a host feeds the worklet at its own rate (the project's, which the engine
//! mixes at) to the context's rate, as a stream.
//!
//! An `AudioContext` need not run at the project's rate: a browser or device can refuse the rate
//! a host asks for. The engine-mixed ring then carries samples at the project's rate, and played
//! one-for-one they would run fast or slow, at the wrong pitch. The stream converts them with the
//! engine's own band-limited [`Resampler`], so what is heard is the project's audio at its own
//! speed; it is not bit-identical to the export, which never converts its mix.

use std::ops::Range;

use alpha2_audio::Resampler;

/// The project-rate samples the worklet holds and converts.
///
/// The host writes the samples [`Self::need`] asks for into [`Self::append`], one plane of
/// [`Self::append_frames`] per channel; [`Self::convert`] appends them to the history and writes
/// the next outputs. Output `n` (counted from the first one the stream made) sits at source
/// position `n * source_rate / target_rate`, source sample 0 being the first the host wrote, and
/// weighs the source samples around it; samples before the first are silence.
pub(crate) struct SourceStream {
    resampler: Resampler,
    channels: usize,
    /// Planar host-written region; channel `c` starts at `c * append_frames`.
    append: Vec<f32>,
    append_frames: usize,
    /// Planar source history; channel `c` starts at `c * history_frames`.
    history: Vec<f32>,
    history_frames: usize,
    /// Source index of each plane's first kept sample.
    start: i64,
    /// Samples kept per channel.
    kept: usize,
    /// The next output's index.
    next: i64,
}

impl SourceStream {
    /// A stream from `source_rate` to `target_rate` for `channels` planes, converting at most
    /// `max_frames` outputs per call. `None` when the rates cannot be converted.
    ///
    /// Builds the converter's coefficient table, so call it before the node plays.
    pub(crate) fn new(
        source_rate: u32,
        target_rate: u32,
        channels: usize,
        max_frames: usize,
    ) -> Option<Self> {
        let resampler = Resampler::new(source_rate, target_rate).ok()?;
        let half = usize::try_from(resampler.half_taps()).ok()?;
        // The most source samples `max_frames` outputs can advance past, and the filter's reach
        // either side of them.
        let advance = (max_frames as u64)
            .checked_mul(u64::from(source_rate))?
            .div_ceil(u64::from(target_rate));
        let append_frames = usize::try_from(advance).ok()?.checked_add(2 * half + 2)?;
        let history_frames = append_frames.checked_add(2 * half + 2)?;
        Some(Self {
            resampler,
            channels,
            append: vec![0.0; append_frames.checked_mul(channels)?],
            append_frames,
            history: vec![0.0; history_frames.checked_mul(channels)?],
            history_frames,
            start: 0,
            kept: 0,
            next: 0,
        })
    }

    /// The host-written region, for its address.
    pub(crate) fn append(&mut self) -> &mut [f32] {
        &mut self.append
    }

    /// Samples per channel of [`Self::append`]'s planes.
    pub(crate) const fn append_frames(&self) -> usize {
        self.append_frames
    }

    /// How many source samples per channel the host must write before the next `frames` outputs:
    /// those that end what they weigh. Never more than [`Self::append_frames`] for a valid
    /// `frames`.
    pub(crate) fn need(&self, frames: usize) -> usize {
        let span = self.span(frames);
        let have = self.start + i64::try_from(self.kept).unwrap_or(i64::MAX);
        usize::try_from(span.end - have)
            .unwrap_or(0)
            .min(self.append_frames)
    }

    /// The source samples the host's samples run behind the outputs by: an output weighs this
    /// many samples past its own position, which must be read before it can be made.
    pub(crate) fn delay_source_samples(&self) -> i64 {
        self.resampler.half_taps()
    }

    /// [`Self::delay_source_samples`] at the target rate, rounded to the nearest sample.
    pub(crate) fn delay_target_samples(&self) -> u32 {
        let delay = u64::try_from(self.delay_source_samples()).unwrap_or(0);
        let source = u64::from(self.resampler.source_rate());
        let target = u64::from(self.resampler.target_rate());
        u32::try_from((delay * target * 2 + source) / (source * 2)).unwrap_or(u32::MAX)
    }

    /// Appends the first `written` samples of each [`Self::append`] plane and writes the next
    /// `frames` outputs into `output`, whose plane `c` starts at `c * stride`. `false`, with
    /// nothing changed, for more samples than the region holds or an output that cannot take
    /// `frames`.
    pub(crate) fn convert(
        &mut self,
        written: usize,
        frames: usize,
        output: &mut [f32],
        stride: usize,
    ) -> bool {
        if written > self.append_frames
            || frames > stride
            || output.len() < stride * (self.channels - 1) + frames
        {
            return false;
        }
        // Drop what neither these outputs nor any later one weighs.
        let span = self.span(frames);
        let drop = usize::try_from(span.start - self.start)
            .unwrap_or(0)
            .min(self.kept);
        if self.kept - drop + written > self.history_frames {
            return false;
        }
        for channel in 0..self.channels {
            let base = channel * self.history_frames;
            self.history
                .copy_within(base + drop..base + self.kept, base);
            let from = channel * self.append_frames;
            let at = base + self.kept - drop;
            self.history[at..at + written].copy_from_slice(&self.append[from..from + written]);
        }
        self.start += i64::try_from(drop).unwrap_or(0);
        self.kept = self.kept - drop + written;
        for channel in 0..self.channels {
            let base = channel * self.history_frames;
            let to = channel * stride;
            self.resampler.resample(
                &self.history[base..base + self.kept],
                self.start,
                self.next,
                &mut output[to..to + frames],
            );
        }
        self.next += i64::try_from(frames).unwrap_or(0);
        true
    }

    /// Forgets every sample: the next output is the stream's first again.
    pub(crate) fn reset(&mut self) {
        self.start = 0;
        self.kept = 0;
        self.next = 0;
    }

    /// The source samples the next `frames` outputs weigh.
    fn span(&self, frames: usize) -> Range<i64> {
        let frames = i64::try_from(frames.max(1)).unwrap_or(i64::MAX);
        self.resampler.source_span(self.next..self.next + frames)
    }
}

//! Decoded source audio placed where its timestamps say, and turned into
//! project-rate samples on demand by `alpha2_audio::Resampler`.
//!
//! The source is its samples at their source positions and silence
//! everywhere else; an output sample is the resampler's band-limited reading
//! of that signal. Because the reading depends only on the samples and their
//! positions, a decoder that seeks and one that reads straight through give
//! the same output, gaps included.

use std::collections::VecDeque;

use alpha2_audio::Resampler;
use alpha2_media::DecodeError;

/// The most project-rate samples resampled in one step: bounds the scratch
/// copy of the source they read.
const STEP_SAMPLES: usize = 4_096;

/// Samples that follow one another at one rate.
struct Piece {
    /// Source index, at the segment's rate, of `planes[*][0]`.
    start: i64,
    planes: Vec<VecDeque<f32>>,
}

impl Piece {
    fn len(&self) -> i64 {
        i64::try_from(self.planes.first().map_or(0, VecDeque::len)).unwrap_or(i64::MAX)
    }

    fn end(&self) -> i64 {
        self.start.saturating_add(self.len())
    }
}

/// Source audio at one rate: pieces in order, silence between them.
struct Segment {
    resampler: Resampler,
    pieces: VecDeque<Piece>,
    /// No sample follows the last piece: the source is silent after it.
    ended: bool,
}

impl Segment {
    fn start(&self) -> i64 {
        self.pieces.front().map_or(0, |piece| piece.start)
    }

    fn end(&self) -> i64 {
        self.pieces.back().map_or(0, Piece::end)
    }

    /// The first output sample at or after the segment's first sample.
    fn first_output(&self) -> i64 {
        self.resampler.target_at_or_after(self.start())
    }

    /// The output sample after the last one this segment can give now.
    fn output_limit(&self) -> i64 {
        if self.ended {
            self.resampler.target_at_or_after(self.end())
        } else {
            self.resampler.target_known_before(self.end())
        }
    }

    /// Outputs `first..first + output.len()` of channel `channel`.
    fn resample(&self, channel: usize, first: i64, output: &mut [f32]) {
        let last = first + i64::try_from(output.len()).unwrap_or(i64::MAX);
        let span = self.resampler.source_span(first..last);
        let overlapping = self
            .pieces
            .iter()
            .filter(|piece| piece.start < span.end && piece.end() > span.start);
        let mut scratch = None::<Vec<f32>>;
        for piece in overlapping {
            let scratch = scratch.get_or_insert_with(|| {
                vec![0.0; usize::try_from(span.end - span.start).unwrap_or(0)]
            });
            let from = piece.start.max(span.start);
            let to = piece.end().min(span.end);
            let skip = usize::try_from(from - piece.start).unwrap_or(0);
            let count = usize::try_from(to - from).unwrap_or(0);
            let at = usize::try_from(from - span.start).unwrap_or(0);
            for (slot, sample) in scratch[at..at + count]
                .iter_mut()
                .zip(piece.planes[channel].range(skip..skip + count))
            {
                *slot = *sample;
            }
        }
        match scratch {
            Some(scratch) => self.resampler.resample(&scratch, span.start, first, output),
            // Nothing the outputs read was decoded: the source is silent there.
            None => output.fill(0.0),
        }
    }

    /// Drops the samples no output from `next` on reads.
    fn discard_before(&mut self, next: i64) {
        let keep = self.resampler.source_span(next..next + 1).start;
        while self.pieces.len() > 1 && self.pieces[0].end() <= keep {
            self.pieces.pop_front();
        }
        if let Some(piece) = self.pieces.front_mut() {
            let drop = usize::try_from(keep.min(piece.end()) - piece.start).unwrap_or(0);
            if drop > 0 {
                for plane in &mut piece.planes {
                    plane.drain(..drop);
                }
                piece.start += i64::try_from(drop).unwrap_or(0);
            }
        }
    }
}

/// The decoded source and the project-rate samples it becomes.
pub(crate) struct SourceTimeline {
    target_rate: u32,
    channels: usize,
    segments: VecDeque<Segment>,
    /// The next output sample [`produce`](Self::produce) gives; `None` until
    /// a sample is placed.
    next: Option<i64>,
    /// No output before this one is given: a seek's target.
    first: Option<i64>,
    /// The most source samples per channel held at once.
    #[cfg(test)]
    peak_held: usize,
}

impl SourceTimeline {
    /// An empty timeline whose outputs start no earlier than `first`.
    pub(crate) fn new(target_rate: u32, channels: usize, first: Option<i64>) -> Self {
        Self {
            target_rate,
            channels,
            segments: VecDeque::new(),
            next: None,
            first,
            #[cfg(test)]
            peak_held: 0,
        }
    }

    /// The most source samples per channel this timeline held at once.
    #[cfg(test)]
    pub(crate) const fn peak_held(&self) -> usize {
        self.peak_held
    }

    /// Where a sample at `rate` continuing the last one placed would sit, in
    /// source samples at `rate`; `None` before anything is placed.
    pub(crate) fn expected(&self, rate: u32) -> Option<i64> {
        let segment = self.segments.back()?;
        let end = segment.end();
        let from = segment.resampler.source_rate();
        if from == rate {
            return Some(end);
        }
        let scaled = i128::from(end) * i128::from(rate);
        let rounded = (scaled * 2 + i128::from(from)).div_euclid(i128::from(from) * 2);
        i64::try_from(rounded).ok()
    }

    /// The output sample at or after source sample `source` at `rate`.
    pub(crate) fn output_at(&self, rate: u32, source: i64) -> i64 {
        let scaled = i128::from(source) * i128::from(self.target_rate);
        let rate = i128::from(rate);
        let quotient = scaled.div_euclid(rate) + i128::from(scaled.rem_euclid(rate) != 0);
        i64::try_from(quotient).unwrap_or(i64::MAX)
    }

    /// Places `planes`, decoded at `rate`, from source sample `at`. `at` is
    /// never before [`expected`](Self::expected): the caller drops what
    /// overlaps.
    ///
    /// # Errors
    /// A rate the resampler cannot take, or planes of the wrong shape.
    pub(crate) fn place(
        &mut self,
        rate: u32,
        at: i64,
        planes: Vec<Vec<f32>>,
    ) -> Result<(), DecodeError> {
        if planes.len() != self.channels
            || planes.iter().any(|plane| plane.len() != planes[0].len())
        {
            return Err(DecodeError::Adapter(
                "decoded audio has the wrong channel shape".into(),
            ));
        }
        if planes[0].is_empty() {
            return Ok(());
        }
        let continues = self
            .segments
            .back()
            .is_some_and(|segment| !segment.ended && segment.resampler.source_rate() == rate);
        if !continues {
            if let Some(segment) = self.segments.back_mut() {
                segment.ended = true;
            }
            let resampler = Resampler::new(rate, self.target_rate)
                .map_err(|error| DecodeError::Unsupported(error.to_string()))?;
            self.segments.push_back(Segment {
                resampler,
                pieces: VecDeque::new(),
                ended: false,
            });
        }
        let segment = self
            .segments
            .back_mut()
            .expect("a segment was just ensured");
        let planes = planes.into_iter().map(VecDeque::from);
        match segment.pieces.back_mut() {
            Some(piece) if piece.end() == at => {
                for (plane, more) in piece.planes.iter_mut().zip(planes) {
                    plane.extend(more);
                }
            }
            Some(piece) if piece.end() > at => {
                return Err(DecodeError::Adapter(
                    "decoded audio overlaps the samples before it".into(),
                ));
            }
            _ => segment.pieces.push_back(Piece {
                start: at,
                planes: planes.collect(),
            }),
        }
        if self.next.is_none() {
            let start = self.segments[0].first_output();
            self.next = Some(self.first.map_or(start, |first| first.max(start)));
        }
        #[cfg(test)]
        {
            let held = self
                .segments
                .iter()
                .flat_map(|segment| &segment.pieces)
                .map(|piece| piece.planes.first().map_or(0, VecDeque::len))
                .sum::<usize>();
            self.peak_held = self.peak_held.max(held);
        }
        Ok(())
    }

    /// No sample follows the ones placed.
    pub(crate) fn finish(&mut self) {
        if let Some(segment) = self.segments.back_mut() {
            segment.ended = true;
        }
    }

    /// Whether [`finish`](Self::finish) was called and every output given.
    pub(crate) fn finished(&self) -> bool {
        self.segments.back().is_none_or(|segment| segment.ended)
    }

    /// Appends up to `wanted` output samples to `output`, one deque per
    /// channel, and returns the first one's index and how many. Fewer, or
    /// none, until more of the source is placed.
    pub(crate) fn produce(
        &mut self,
        output: &mut [VecDeque<f32>],
        wanted: usize,
    ) -> Option<(i64, usize)> {
        let mut next = self.next?;
        let start = next;
        let mut given = 0;
        let mut step = vec![0.0; STEP_SAMPLES.min(wanted)];
        while given < wanted {
            let Some(segment) = self.segments.front() else {
                break;
            };
            // A segment after another may start later: the source is silent until it does.
            let first = segment.first_output();
            if next < first {
                let count = usize::try_from(first - next)
                    .unwrap_or(usize::MAX)
                    .min(wanted - given);
                for plane in output.iter_mut() {
                    plane.extend(std::iter::repeat_n(0.0, count));
                }
                next += i64::try_from(count).unwrap_or(i64::MAX);
                given += count;
                continue;
            }
            let limit = segment.output_limit();
            if next >= limit {
                if segment.ended && self.segments.len() > 1 {
                    self.segments.pop_front();
                    continue;
                }
                // Nothing placed yet reaches `next` (a far seek target): drop what no output reads
                // now, so what is held stays bounded however far decoding goes.
                if let Some(segment) = self.segments.front_mut() {
                    segment.discard_before(next);
                }
                break;
            }
            let count = usize::try_from(limit - next)
                .unwrap_or(usize::MAX)
                .min(wanted - given)
                .min(STEP_SAMPLES);
            let step = &mut step[..count];
            for (channel, plane) in output.iter_mut().enumerate() {
                segment.resample(channel, next, step);
                plane.extend(step.iter().copied());
            }
            next += i64::try_from(count).unwrap_or(i64::MAX);
            given += count;
            if let Some(segment) = self.segments.front_mut() {
                segment.discard_before(next);
            }
        }
        self.next = Some(next);
        (given > 0).then_some((start, given))
    }
}

#[cfg(test)]
mod tests {
    use super::SourceTimeline;
    use std::collections::VecDeque;

    fn drain(timeline: &mut SourceTimeline) -> (i64, Vec<f32>) {
        let mut output = vec![VecDeque::new()];
        let mut start = None;
        while let Some((first, _)) = timeline.produce(&mut output, 1_000) {
            start.get_or_insert(first);
        }
        (start.unwrap_or(0), output.remove(0).into())
    }

    /// The output is the same however the source arrives: in one frame or
    /// many, read in one call or many.
    #[test]
    fn output_does_not_depend_on_how_frames_or_reads_split() {
        let source: Vec<f32> = (0..9_000_u16).map(|n| f32::from(n % 97) / 97.0).collect();
        let mut whole = SourceTimeline::new(48_000, 1, None);
        whole
            .place(44_100, 100, vec![source.clone()])
            .expect("place");
        whole.finish();
        let expected = drain(&mut whole);
        let mut split = SourceTimeline::new(48_000, 1, None);
        let mut output = vec![VecDeque::new()];
        let mut start = None;
        for (index, chunk) in source.chunks(1_024).enumerate() {
            let at = 100 + i64::try_from(index * 1_024).expect("small");
            split
                .place(44_100, at, vec![chunk.to_vec()])
                .expect("place");
            while let Some((first, _)) = split.produce(&mut output, 37) {
                start.get_or_insert(first);
            }
        }
        split.finish();
        while let Some((first, _)) = split.produce(&mut output, 37) {
            start.get_or_insert(first);
        }
        assert_eq!(
            (start.expect("output"), Vec::from(output.remove(0))),
            expected
        );
        // 100 samples at 44.1 kHz end before output sample 109 at 48 kHz; 9,100 before 9,905.
        assert_eq!(expected.0, 109);
        assert_eq!(
            expected.0 + i64::try_from(expected.1.len()).expect("small"),
            9_905
        );
    }

    /// A stream that changes rate continues at its new rate where the new
    /// rate's first sample sits, with silence between.
    #[test]
    #[allow(clippy::float_cmp)]
    fn a_rate_change_starts_a_new_segment_after_silence() {
        let mut timeline = SourceTimeline::new(48_000, 1, None);
        timeline
            .place(48_000, 0, vec![vec![0.5; 4_800]])
            .expect("place");
        assert_eq!(timeline.expected(24_000), Some(2_400));
        timeline
            .place(24_000, 2_500, vec![vec![0.25; 2_400]])
            .expect("place");
        timeline.finish();
        let (start, samples) = drain(&mut timeline);
        assert_eq!(start, 0);
        assert_eq!(samples.len(), 9_800);
        assert!(samples[..4_800].iter().all(|sample| *sample == 0.5));
        assert!(samples[4_800..5_000].iter().all(|sample| *sample == 0.0));
        assert!(
            samples[5_100..9_700]
                .iter()
                .all(|sample| (sample - 0.25).abs() < 1e-6)
        );
    }

    #[test]
    fn a_seek_target_is_the_first_output() {
        let mut timeline = SourceTimeline::new(48_000, 1, Some(1_000));
        timeline
            .place(48_000, 0, vec![vec![0.5; 2_000]])
            .expect("place");
        timeline.finish();
        let (start, samples) = drain(&mut timeline);
        assert_eq!((start, samples.len()), (1_000, 1_000));
        // A target before the stream's first sample starts at that sample.
        let mut timeline = SourceTimeline::new(48_000, 1, Some(10));
        timeline
            .place(48_000, 500, vec![vec![0.5; 100]])
            .expect("place");
        timeline.finish();
        assert_eq!(drain(&mut timeline).0, 500);
    }
}

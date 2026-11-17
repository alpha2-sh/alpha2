//! Project audio cut into an encoder's frames at the encoder's rate.

use std::collections::VecDeque;

use alpha2_media::{AudioBlock, EncodeError};

use crate::audio_timeline::SourceTimeline;

/// The rate an encoder that supports `rates` (`None`: any) encodes a `project_rate` project at:
/// that rate if supported, else the lowest above it, else its highest.
#[must_use]
pub fn encoder_sample_rate(rates: Option<&[u32]>, project_rate: u32) -> u32 {
    let Some(rates) = rates else {
        return project_rate;
    };
    let rates = rates
        .iter()
        .copied()
        .filter(|rate| *rate > 0)
        .collect::<Vec<_>>();
    if rates.contains(&project_rate) {
        return project_rate;
    }
    rates
        .iter()
        .copied()
        .filter(|rate| *rate > project_rate)
        .min()
        .or_else(|| rates.iter().copied().max())
        .unwrap_or(project_rate)
}

/// Project-rate audio blocks cut into the encoder's frames at its own rate.
pub struct EncoderAudioInput {
    project_rate: u32,
    channels: usize,
    frame_size: usize,
    /// Brings the project's audio to the encoder's rate, when they differ.
    resampler: Option<SourceTimeline>,
    /// Samples at the encoder's rate not yet handed over.
    pending: Vec<VecDeque<f32>>,
    /// Project-rate samples accepted.
    received: u64,
}

impl EncoderAudioInput {
    /// Cuts `channels` planes at `project_rate` into `frame_size`-sample frames at `encoder_rate`.
    #[must_use]
    pub fn new(project_rate: u32, encoder_rate: u32, channels: usize, frame_size: usize) -> Self {
        Self {
            project_rate,
            channels,
            frame_size,
            resampler: (encoder_rate != project_rate)
                .then(|| SourceTimeline::new(encoder_rate, channels, Some(0))),
            pending: (0..channels).map(|_| VecDeque::new()).collect(),
            received: 0,
        }
    }

    /// Takes the next block, which must start where the last ended, and returns the
    /// whole frames it completes.
    ///
    /// # Errors
    /// A block of another rate or channel count, of unequal channels, or out of order.
    pub fn push(&mut self, block: &AudioBlock) -> Result<Vec<Vec<Vec<f32>>>, EncodeError> {
        if block.sample_rate != self.project_rate
            || block.channels.len() != self.channels
            || block.channels.is_empty()
        {
            return Err(EncodeError::Adapter("audio block format changed".into()));
        }
        let frames = block.channels[0].len();
        if block.channels.iter().any(|channel| channel.len() != frames) {
            return Err(EncodeError::Adapter("audio channel lengths differ".into()));
        }
        if u64::try_from(block.start_sample).ok() != Some(self.received) {
            return Err(EncodeError::InvalidTimestamp);
        }
        match self.resampler.as_mut() {
            Some(resampler) => {
                resampler
                    .place(
                        self.project_rate,
                        block.start_sample,
                        block.channels.clone(),
                    )
                    .map_err(|error| EncodeError::Adapter(error.to_string()))?;
                resampler.produce(&mut self.pending, usize::MAX);
            }
            None => {
                for (pending, channel) in self.pending.iter_mut().zip(&block.channels) {
                    pending.extend(channel.iter().copied());
                }
            }
        }
        self.received = self
            .received
            .saturating_add(u64::try_from(frames).unwrap_or(u64::MAX));
        let mut ready = Vec::new();
        while self.pending[0].len() >= self.frame_size {
            ready.push(self.take(self.frame_size));
        }
        Ok(ready)
    }

    /// Ends the input and returns the remaining frames, the last as long as what is left.
    pub fn finish(&mut self) -> Vec<Vec<Vec<f32>>> {
        if let Some(resampler) = self.resampler.as_mut() {
            resampler.finish();
            resampler.produce(&mut self.pending, usize::MAX);
        }
        let mut frames = Vec::new();
        while !self.pending[0].is_empty() {
            frames.push(self.take(self.frame_size.min(self.pending[0].len())));
        }
        frames
    }

    /// The input time accepted so far in ticks, reported back as consumed
    /// once its frames are encoded.
    #[must_use]
    pub fn received_ticks(&self) -> i64 {
        i64::try_from(self.received)
            .unwrap_or(i64::MAX)
            .saturating_mul(alpha2_time::TICKS_PER_SECOND)
            .div_euclid(i64::from(self.project_rate))
    }

    fn take(&mut self, samples: usize) -> Vec<Vec<f32>> {
        self.pending
            .iter_mut()
            .map(|pending| pending.drain(..samples).collect())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_project_rate_is_kept_else_the_next_higher_else_the_highest() {
        assert_eq!(encoder_sample_rate(None, 44_100), 44_100);
        let opus = [48_000, 24_000, 16_000, 12_000, 8_000];
        assert_eq!(encoder_sample_rate(Some(&opus), 44_100), 48_000);
        assert_eq!(encoder_sample_rate(Some(&opus), 16_000), 16_000);
        assert_eq!(encoder_sample_rate(Some(&opus), 96_000), 48_000);
        assert_eq!(encoder_sample_rate(Some(&[0]), 32_000), 32_000);
    }

    #[test]
    fn blocks_are_cut_into_whole_frames_and_the_tail_is_kept_short() {
        let mut input = EncoderAudioInput::new(48_000, 48_000, 2, 1_024);
        let block = |start: i64, frames: usize| AudioBlock {
            start_sample: start,
            sample_rate: 48_000,
            speakers: alpha2_media::SpeakerLayout::STEREO,
            channels: vec![vec![0.25; frames]; 2],
        };
        assert_eq!(input.push(&block(0, 1_000)).expect("first").len(), 0);
        let ready = input.push(&block(1_000, 2_000)).expect("second");
        assert_eq!(ready.len(), 2);
        assert!(ready.iter().all(|frame| frame[0].len() == 1_024));
        assert_eq!(
            input.push(&block(10, 4)),
            Err(EncodeError::InvalidTimestamp)
        );
        let tail = input.finish();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0][0].len(), 3_000 - 2_048);
        assert_eq!(input.received_ticks(), alpha2_time::TICKS_PER_SECOND / 16);
    }
}

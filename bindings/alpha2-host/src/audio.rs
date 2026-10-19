//! Windowed audio sources and tick/sample conversion.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::{AUDIO_INTERPOLATION_MARGIN, error_message};
use alpha2_media::{
    AudioBlock, AudioBlockProvider, AudioSpan, CancelToken, DecodeError, DecodeEvent,
    DecodeRequest, DecodeRequestKind, Decoder, SourceId,
};
use alpha2_model::Project;
use alpha2_native::NativeAudioDecoder;
use alpha2_time::{MediaTime, TICKS_PER_SECOND};

/// One decoder and the window of samples it has decoded for one source range.
pub struct WindowedAudioSource {
    pub decoder: NativeAudioDecoder,
    pub window: Option<AudioBlock>,
    /// The requested interval can start before the stream's first sample.
    /// That prefix is known silence, represented without allocating a gap.
    covered_start: i64,
    /// Whether the decoder reached the end of the stream while filling the
    /// window: past the window the source is silent.
    ended: bool,
    /// Where the previous read started. A read before it moves backward, as a
    /// reversed clip reads.
    last_start: Option<i64>,
}

/// How far before a backward read a window seeks, in seconds, so the reads
/// after it, each a little earlier, continue the window instead of seeking.
const BACKWARD_READAHEAD_SECONDS: i64 = 1;

/// Frames in each block a window's decoder hands over: a window filled to a
/// sample ends at most one block past it.
const AUDIO_DECODE_BLOCK_FRAMES: usize = 4_096;

impl WindowedAudioSource {
    /// Whether a window starting at `start` extends this one without a seek.
    fn continues_at(&self, start: i64) -> bool {
        self.window.as_ref().is_some_and(|window| {
            let window_start = window.start_sample;
            let window_end = window_start.saturating_add(audio_block_frames(window));
            start >= self.covered_start && (start <= window_end || self.ended)
        })
    }

    /// Makes the window hold `start..end`, holding at most `limit` bytes
    /// while it does: what it will grow to is reserved, and checked against
    /// `limit`, before anything past its first block is decoded.
    fn ensure_window(
        &mut self,
        source: &SourceId,
        (start, end): (i64, i64),
        sample_rate: u32,
        limit: u64,
        cancel: &CancelToken,
    ) -> Result<(), String> {
        let backward = self.last_start.is_some_and(|last| start < last);
        self.last_start = Some(start);
        if !self.continues_at(start) {
            let seek_start = if backward {
                start
                    .saturating_sub(BACKWARD_READAHEAD_SECONDS * i64::from(sample_rate))
                    .max(0)
            } else {
                start
            };
            let request_time =
                MediaTime::from_ticks(samples_to_ticks_ceil(seek_start, sample_rate));
            let result = self.decoder.seek(
                DecodeRequest {
                    source: source.clone(),
                    time: request_time,
                    kind: DecodeRequestKind::Seek,
                    generation: 0,
                },
                cancel,
            );
            match result {
                Ok(result) => {
                    let DecodeEvent::Audio(block) = result.event else {
                        return Err(format!(
                            "audio source {} did not decode a block",
                            source.as_str()
                        ));
                    };
                    self.window = Some(block);
                    self.ended = false;
                }
                // Past the stream's end, as a clip of a video whose audio stops early reads: the
                // source is silent there.
                Err(DecodeError::OutOfRange) => {
                    self.window = Some(AudioBlock {
                        start_sample: seek_start,
                        sample_rate,
                        speakers: self.decoder.speakers(),
                        channels: vec![Vec::new(); self.decoder.channels()],
                    });
                    self.ended = true;
                }
                Err(error) => return Err(error_message(error)),
            }
            self.covered_start = seek_start.min(start);
        }
        self.reserve_to(source, end, limit)?;
        loop {
            let window = self
                .window
                .as_ref()
                .ok_or_else(|| "audio window disappeared".to_owned())?;
            let window_start = window.start_sample;
            let window_end = window_start.saturating_add(audio_block_frames(window));
            if window_end >= end || self.ended {
                break;
            }
            match self.decoder.decode_next(cancel).map_err(error_message)? {
                DecodeEvent::Audio(block) => append_audio_block(
                    self.window
                        .as_mut()
                        .ok_or_else(|| "audio window disappeared".to_owned())?,
                    block,
                )?,
                DecodeEvent::EndOfStream => {
                    self.ended = true;
                    break;
                }
                DecodeEvent::Retry | DecodeEvent::Video(_) => {}
            }
        }
        let declared_duration = self.decoder.audio_duration();
        let window = self
            .window
            .as_mut()
            .ok_or_else(|| "audio window disappeared".to_owned())?;
        if self.ended {
            pad_audio_window_to_declared_end(window, end, declared_duration, sample_rate);
        }
        // Reading forward, what lies before this read is consumed; reading backward, it is what the
        // next reads want.
        if !backward {
            trim_audio_window(window, start);
            self.covered_start = start;
        }
        Ok(())
    }

    /// Reserves what filling the window to `end` holds, every channel of
    /// it and the decoder block that may run past `end`, failing with
    /// `RESOURCE_LIMIT` when that is over `limit`. Reserved exactly, the
    /// window's growth never doubles past what was checked.
    fn reserve_to(&mut self, source: &SourceId, end: i64, limit: u64) -> Result<(), String> {
        let ended = self.ended;
        let Some(window) = self.window.as_mut() else {
            return Ok(());
        };
        let held = audio_block_frames(window);
        if ended || window.start_sample.saturating_add(held) >= end {
            return Ok(());
        }
        let frames = usize::try_from(end.saturating_sub(window.start_sample))
            .unwrap_or(usize::MAX)
            .saturating_add(AUDIO_DECODE_BLOCK_FRAMES);
        let bytes = u64::try_from(frames)
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::try_from(window.channels.len()).unwrap_or(u64::MAX))
            .saturating_mul(4);
        if bytes > limit {
            return Err(crate::memory::resource_limit(format!(
                "the audio mix reads {frames} samples of {} channels of source {} at once, \
                 {bytes} bytes, over the {limit} bytes the audio window budget has left",
                window.channels.len(),
                source.as_str(),
            )));
        }
        for channel in &mut window.channels {
            channel.reserve_exact(frames.saturating_sub(channel.len()));
        }
        Ok(())
    }
}

/// Decoded audio for every audible source of an export, served to the mixer through
/// [`AudioBlockProvider`] and held within the audio window budget.
pub struct WindowedAudioSources {
    pub sample_rate: u32,
    pub paths: HashMap<SourceId, PathBuf>,
    /// One window per disjoint source range. Two clips cut from the same media
    /// read unrelated source ranges, and a block that straddles their join
    /// needs both at once. Covering them with one window would decode, and
    /// retain, everything between the two.
    pub sources: HashMap<SourceId, Vec<WindowedAudioSource>>,
    /// The audio window budget. A mix block that needs more than a scaled
    /// budget still gets what it needs, up to the budget before `memory.scale`
    /// ([`crate::memory::ByteBudget::holding`]).
    pub byte_budget: crate::memory::ByteBudget,
    pub retained_bytes: u64,
    pub peak_bytes: u64,
    /// The export's decode policy, and where its decoders report.
    pub decoding: std::sync::Arc<crate::recovery::ExportDecoding>,
}

impl WindowedAudioSources {
    /// Records the media path of every audible source the project uses; no decoder opens yet.
    ///
    /// # Errors
    /// Fails when a used source has no entry in `assets`.
    pub fn new(
        project: &Project,
        assets: &HashMap<String, PathBuf>,
        sample_rate: u32,
        byte_budget: impl Into<crate::memory::ByteBudget>,
        decoding: std::sync::Arc<crate::recovery::ExportDecoding>,
    ) -> Result<Self, String> {
        let mut paths = HashMap::new();
        for source in project
            .used_sources()
            .into_iter()
            .filter(|source| source.is_audible())
        {
            let path = assets.get(source.id.as_str()).ok_or_else(|| {
                crate::protocol::missing_input(format!(
                    "missing uploaded asset {}",
                    source.id.as_str()
                ))
            })?;
            paths.insert(source.id.clone(), path.clone());
        }
        Ok(Self {
            sample_rate,
            paths,
            sources: HashMap::new(),
            byte_budget: byte_budget.into(),
            retained_bytes: 0,
            peak_bytes: 0,
            decoding,
        })
    }

    /// Decodes what `needs` reads, plus the interpolation margin on each side, into windows
    /// and drops windows nothing needs any more.
    ///
    /// # Errors
    /// Fails with `RESOURCE_LIMIT` when the windows cannot fit the budget, and with the
    /// decoder's error or a cancellation otherwise.
    pub fn prepare(
        &mut self,
        needs: &[alpha2_engine::AudioNeed],
        cancel: &CancelToken,
    ) -> Result<(), String> {
        let mut ranges = HashMap::<SourceId, Vec<(i64, i64)>>::new();
        for need in needs {
            let start = ticks_to_samples(need.source_range.start().ticks(), self.sample_rate)
                .saturating_sub(AUDIO_INTERPOLATION_MARGIN)
                .max(0);
            let end = ticks_to_samples_ceil(need.source_range.end().ticks(), self.sample_rate)
                .saturating_add(AUDIO_INTERPOLATION_MARGIN);
            if end > start {
                ranges
                    .entry(need.source_id.clone())
                    .or_default()
                    .push((start, end));
            }
        }
        // The least the windows will hold: every span in 32-bit samples per channel. A mix that
        // cannot fit is refused before decoding; each window reserves its whole growth first
        // (`reserve_to`).
        let least = ranges.iter().fold(0_u64, |bytes, (source, spans)| {
            let channels = self
                .sources
                .get(source)
                .and_then(|states| states.first())
                .map_or(1, |state| state.decoder.channels().max(1));
            spans.iter().fold(bytes, |bytes, &(start, end)| {
                bytes.saturating_add(
                    u64::try_from(end.saturating_sub(start))
                        .unwrap_or(u64::MAX)
                        .saturating_mul(u64::try_from(channels).unwrap_or(u64::MAX))
                        .saturating_mul(4),
                )
            })
        });
        let budget = self.byte_budget.holding(least);
        if least > budget {
            return Err(crate::memory::resource_limit(format!(
                "the audio mix needs at least {least} bytes of decoded audio at once, over the {budget} byte audio window budget"
            )));
        }
        self.sources.retain(|source, _| ranges.contains_key(source));
        for (source, spans) in ranges {
            self.ensure_windows(&source, &merge_sample_ranges(spans), budget, cancel)?;
        }
        self.retained_bytes = window_bytes(self.sources.values().flatten());
        if self.retained_bytes > budget {
            return Err(crate::memory::resource_limit(format!(
                "decoded audio window exceeds {budget} byte budget"
            )));
        }
        self.peak_bytes = self.peak_bytes.max(self.retained_bytes);
        Ok(())
    }

    /// Gives each sorted, disjoint `spans` entry its own window.
    ///
    /// A span that continues an existing window keeps that window's decoder,
    /// so a clip playing through is read sequentially. Any other span seeks a
    /// window no span continued, or opens a decoder when none is left, and
    /// windows no span wants are dropped.
    fn ensure_windows(
        &mut self,
        source: &SourceId,
        spans: &[(i64, i64)],
        budget: u64,
        cancel: &CancelToken,
    ) -> Result<(), String> {
        let sample_rate = self.sample_rate;
        let mut idle = self.sources.remove(source).unwrap_or_default();
        let continued = spans
            .iter()
            .map(|&(start, _)| {
                idle.iter()
                    .position(|state| state.continues_at(start))
                    .map(|index| idle.swap_remove(index))
            })
            .collect::<Vec<_>>();
        let mut windows = Vec::with_capacity(spans.len());
        let mut continued = continued.into_iter();
        for &span in spans {
            let mut state = match continued.next().flatten().or_else(|| idle.pop()) {
                Some(state) => state,
                None => self.open(source)?,
            };
            // What every other window holds now, this source's included, is not this one's to use.
            let held = window_bytes(self.sources.values().flatten())
                .saturating_add(window_bytes(&idle))
                .saturating_add(window_bytes(&windows))
                .saturating_add(window_bytes(continued.as_slice().iter().flatten()));
            state.ensure_window(
                source,
                span,
                sample_rate,
                budget.saturating_sub(held),
                cancel,
            )?;
            windows.push(state);
        }
        self.sources.insert(source.clone(), windows);
        Ok(())
    }

    fn open(&self, source: &SourceId) -> Result<WindowedAudioSource, String> {
        let path = self.paths.get(source).ok_or_else(|| {
            crate::protocol::missing_input(format!("missing audio source {}", source.as_str()))
        })?;
        let mut decoder = crate::recovery::audio(
            NativeAudioDecoder::open(path, self.sample_rate).map_err(error_message)?,
            source,
            path,
            &self.decoding,
        );
        decoder
            .set_block_frames(AUDIO_DECODE_BLOCK_FRAMES)
            .map_err(error_message)?;
        Ok(WindowedAudioSource {
            decoder,
            window: None,
            covered_start: 0,
            ended: false,
            last_start: None,
        })
    }

    pub const fn peak_bytes(&self) -> u64 {
        self.peak_bytes
    }
}

impl AudioBlockProvider for WindowedAudioSources {
    /// The window that covers the whole request, or else one that holds its
    /// start. Windows of one source are decoded in whole decoder blocks, so
    /// one can run past the start of another: the first window holding a
    /// read's start is not always the one that holds the read.
    fn block(&self, source: &SourceId, start_sample: i64, frames: usize) -> Option<AudioSpan<'_>> {
        let request_end = start_sample.saturating_add(i64::try_from(frames).unwrap_or(i64::MAX));
        let spans = self.sources.get(source)?.iter().filter_map(|state| {
            let window = state.window.as_ref()?;
            let window_start = window.start_sample;
            let window_end = window_start.saturating_add(audio_block_frames(window));
            (start_sample >= state.covered_start && (start_sample < window_end || state.ended))
                .then_some(AudioSpan {
                    block: window,
                    // A seek before the stream's first sample decodes from that sample on: what
                    // lies before it is silence.
                    starts_source: state.covered_start < window_start,
                    // Past a stream's end the source is silent too; a clip ending exactly at the
                    // end reads
                    // interpolation samples past it.
                    ends_source: state.ended,
                })
        });
        let mut first = None;
        for span in spans {
            if span.covers(start_sample, request_end) {
                return Some(span);
            }
            first.get_or_insert(span);
        }
        first
    }
}

/// Sorts sample ranges and joins the ones that overlap or touch.
pub fn merge_sample_ranges(mut ranges: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    ranges.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// Frames in `block`, read from its first channel; zero when it has none.
pub fn audio_block_frames(block: &AudioBlock) -> i64 {
    block
        .channels
        .first()
        .and_then(|channel| i64::try_from(channel.len()).ok())
        .unwrap_or(0)
}

/// The bytes the windows of `states` hold.
fn window_bytes<'a>(states: impl IntoIterator<Item = &'a WindowedAudioSource>) -> u64 {
    states.into_iter().fold(0_u64, |bytes, state| {
        bytes.saturating_add(state.window.as_ref().map_or(0, audio_block_bytes))
    })
}

/// Bytes `block` holds as 32-bit samples.
pub fn audio_block_bytes(block: &AudioBlock) -> u64 {
    block.channels.iter().fold(0_u64, |bytes, channel| {
        bytes.saturating_add(
            u64::try_from(channel.len())
                .unwrap_or(u64::MAX)
                .saturating_mul(4),
        )
    })
}

/// Appends `block` to `window`.
///
/// # Errors
/// Fails when the format changes or `block` does not start exactly where `window` ends.
pub fn append_audio_block(window: &mut AudioBlock, block: AudioBlock) -> Result<(), String> {
    if window.sample_rate != block.sample_rate || window.channels.len() != block.channels.len() {
        return Err("decoded audio format changed inside a source".to_owned());
    }
    if block.start_sample
        != window
            .start_sample
            .saturating_add(audio_block_frames(window))
    {
        return Err("decoded audio has an unreported gap or overlap inside AudioSpan".to_owned());
    }
    for (output, input) in window.channels.iter_mut().zip(block.channels) {
        output.extend(input);
    }
    Ok(())
}

/// Pads `window` with silence up to `requested_end`, but no further than the stream's
/// declared duration plus the interpolation margin. Does nothing without a declared duration.
pub fn pad_audio_window_to_declared_end(
    window: &mut AudioBlock,
    requested_end: i64,
    declared_duration: Option<MediaTime>,
    sample_rate: u32,
) {
    let window_start = window.start_sample;
    let window_end = window_start.saturating_add(audio_block_frames(window));
    let Some(declared_end) =
        declared_duration.map(|duration| ticks_to_samples_ceil(duration.ticks(), sample_rate))
    else {
        return;
    };
    let padded_end = requested_end.min(declared_end.saturating_add(AUDIO_INTERPOLATION_MARGIN));
    let Ok(padding) = usize::try_from(padded_end.saturating_sub(window_end).max(0)) else {
        return;
    };
    if padding == 0 {
        return;
    }
    for channel in &mut window.channels {
        channel.resize(channel.len().saturating_add(padding), 0.0);
    }
}

/// Drops the samples before `requested_start` from every channel.
pub fn trim_audio_window(window: &mut AudioBlock, requested_start: i64) {
    let window_start = window.start_sample;
    let discard = usize::try_from(requested_start.saturating_sub(window_start).max(0))
        .unwrap_or(usize::MAX)
        .min(window.channels.first().map_or(0, Vec::len));
    if discard == 0 {
        return;
    }
    for channel in &mut window.channels {
        channel.drain(..discard.min(channel.len()));
    }
    window.start_sample = window_start.saturating_add(i64::try_from(discard).unwrap_or(i64::MAX));
}

/// Converts media ticks to a sample index, rounding toward negative infinity.
pub fn ticks_to_samples(ticks: i64, sample_rate: u32) -> i64 {
    ticks
        .saturating_mul(i64::from(sample_rate))
        .div_euclid(TICKS_PER_SECOND)
}

/// Converts media ticks to a sample index, rounding up.
pub fn ticks_to_samples_ceil(ticks: i64, sample_rate: u32) -> i64 {
    let numerator = i128::from(ticks).saturating_mul(i128::from(sample_rate));
    let denominator = i128::from(TICKS_PER_SECOND);
    let samples =
        numerator.div_euclid(denominator) + i128::from(numerator.rem_euclid(denominator) != 0);
    i64::try_from(samples).unwrap_or(i64::MAX)
}

/// Converts a sample index to media ticks, rounding up. A zero rate is treated as 1.
pub fn samples_to_ticks_ceil(samples: i64, sample_rate: u32) -> i64 {
    let numerator = i128::from(samples).saturating_mul(i128::from(TICKS_PER_SECOND));
    let denominator = i128::from(sample_rate.max(1));
    let ticks =
        numerator.div_euclid(denominator) + i128::from(numerator.rem_euclid(denominator) != 0);
    i64::try_from(ticks).unwrap_or(i64::MAX)
}

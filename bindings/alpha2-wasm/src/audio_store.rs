//! Browser audio provider: host-decoded, project-rate samples held in per-source windows under
//! one byte budget.

use std::collections::HashMap;
use std::ops::Range;

use alpha2_media::{AudioBlock, AudioBlockProvider, AudioSpan, SourceId, SpeakerLayout};
use alpha2_time::{MediaTime, TICKS_PER_SECOND};

/// Longest window, in seconds at the project rate.
const MAX_RETAINED_SECONDS: usize = 600;
/// About 23 minutes of 48 kHz stereo. wasm32 memory tops out at 4 GB, so bytes, not a count of
/// sources or windows, bound the store.
pub(crate) const MAX_RETAINED_BYTES: usize = 512 << 20;

/// A join that has to cut a window drops an extra `1 / SLIDE_FRACTION` of a window, so a host
/// continuing a full window moves its samples once per that much audio instead of every upload.
const SLIDE_FRACTION: usize = 4;

/// The samples of one upload: a plane per speaker, end to end.
///
/// The store copies them once, straight into their window, so a JS `Float32Array` need not be
/// copied into wasm memory first.
pub(crate) trait UploadSamples {
    fn values(&self) -> usize;
    fn append(&self, range: Range<usize>, plane: &mut Vec<f32>);
}

impl UploadSamples for [f32] {
    fn values(&self) -> usize {
        self.len()
    }

    fn append(&self, range: Range<usize>, plane: &mut Vec<f32>) {
        plane.extend_from_slice(&self[range]);
    }
}

/// A source's audio stream (`AudioNeed::stream`); `None` is the source's default stream.
pub(crate) type StreamKey = (String, Option<u32>);

/// Host-uploaded, project-rate planar audio held per source audio stream.
///
/// An upload replaces every window it overlaps (a seek or re-decode) and joins same-speaker
/// windows it meets at either end, so a held read always falls in one window regardless of
/// decode order. Any other upload is its own window. A window holds at most
/// [`MAX_RETAINED_SECONDS`]; past the byte budget the least recently uploaded window of any
/// source is evicted first.
pub(crate) struct AudioStore {
    sources: HashMap<StreamKey, Vec<Window>>,
    /// Where each stream's audio ends, as declared by an upload; the stream is silent after.
    source_ends: HashMap<StreamKey, i64>,
    sample_rate: u32,
    uploads: u64,
    byte_budget: usize,
}

struct Window {
    block: AudioBlock,
    /// Upload counter value when the window last grew or was created.
    uploaded: u64,
}

/// Why the store refused an upload. The store is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UploadRefusal {
    /// Not at the project rate, which is `expected`.
    WrongRate { expected: u32 },
    /// No samples, or not a full plane of one length for each speaker.
    Planes,
    /// Longer than one window or the whole store can hold.
    TooLarge,
}

impl Default for AudioStore {
    fn default() -> Self {
        Self::new(0)
    }
}

impl AudioStore {
    pub(crate) fn new(sample_rate: u32) -> Self {
        Self {
            sources: HashMap::new(),
            source_ends: HashMap::new(),
            sample_rate,
            uploads: 0,
            byte_budget: MAX_RETAINED_BYTES,
        }
    }

    #[cfg(test)]
    pub(crate) fn upload(
        &mut self,
        source: &str,
        start_sample: i64,
        sample_rate: u32,
        speakers: SpeakerLayout,
        samples: &[f32],
        ends_source: bool,
    ) -> Result<(), UploadRefusal> {
        self.upload_from(
            source,
            None,
            start_sample,
            sample_rate,
            speakers,
            samples,
            ends_source,
        )
    }

    /// Adds an upload as its own window, replacing the windows it overlaps and joining the
    /// ones it meets.
    ///
    /// `samples` holds one equal-length plane per speaker, end to end, starting at
    /// `start_sample` at the project rate. With `ends_source`, the source is silent after the
    /// upload. NaN or infinite samples are kept; the mixer reads them as silence.
    ///
    /// # Errors
    /// The [`UploadRefusal`] [`Self::admits`] answers; the store is then unchanged.
    ///
    /// A joined window keeps the whole upload and as much of its neighbours as fits, samples
    /// after the upload first since playback reads forward. Dropping samples before the upload
    /// drops another [`SLIDE_FRACTION`] of a window, and a window continued after its end grows
    /// by doubling, so samples move a few times per window rather than on every upload. Other
    /// sources' windows are evicted, least recently uploaded first, before allocating, and
    /// never just to make spare room.
    #[allow(clippy::too_many_arguments)] // `uploadAudioBlock`'s arguments, passed through.
    pub(crate) fn upload_from<S: UploadSamples + ?Sized>(
        &mut self,
        source: &str,
        stream: Option<u32>,
        start_sample: i64,
        sample_rate: u32,
        speakers: SpeakerLayout,
        samples: &S,
        ends_source: bool,
    ) -> Result<(), UploadRefusal> {
        let channels = speakers.channels();
        self.admits(sample_rate, channels, samples.values())?;
        let frames = samples.values() / channels;
        let limit = self.window_frame_limit(channels);
        let end = start_sample.saturating_add(i64::try_from(frames).unwrap_or(i64::MAX));
        let key = (source.to_owned(), stream);
        let (left, right) = self.take_neighbours(&key, start_sample, end, speakers);
        let room = limit - frames;
        let right_frames = right.as_ref().map_or(0, AudioBlock::frames).min(room);
        let left_held = left.as_ref().map_or(0, AudioBlock::frames);
        let mut left_frames = left_held.min(room - right_frames);
        if left_frames < left_held {
            left_frames = left_frames.saturating_sub(limit / SLIDE_FRACTION);
        }
        let needed = left_frames + frames + right_frames;
        let frame_bytes = channels.saturating_mul(std::mem::size_of::<f32>());
        // The neighbours are already out of the store, so this is what every other window holds.
        let free_frames = self.byte_budget.saturating_sub(self.retained_bytes()) / frame_bytes;
        let capacity = left.as_ref().map_or(needed, |left| {
            let held = left.channels.iter().map(Vec::capacity).min().unwrap_or(0);
            if held >= needed {
                held
            } else {
                held.saturating_mul(2)
                    .min(limit)
                    .min(free_frames)
                    .max(needed)
            }
        });
        self.enforce_total(
            self.byte_budget
                .saturating_sub(capacity.saturating_mul(frame_bytes)),
        );
        let upload = |channel: usize| channel * frames..(channel + 1) * frames;
        let head = |channel: usize| {
            right
                .as_ref()
                .map_or(&[][..], |right| &right.channels[channel][..right_frames])
        };
        let block = if let Some(mut left) = left {
            // Kept samples move to the front of the left neighbour's planes, which reallocate
            // only to grow.
            let dropped = left_held - left_frames;
            for (channel, plane) in left.channels.iter_mut().enumerate() {
                plane.drain(..dropped);
                plane.reserve_exact(capacity.saturating_sub(plane.len()));
                samples.append(upload(channel), plane);
                plane.extend_from_slice(head(channel));
            }
            left.start_sample = start_sample - i64::try_from(left_frames).unwrap_or(0);
            left
        } else {
            AudioBlock {
                start_sample,
                sample_rate,
                speakers,
                channels: (0..channels)
                    .map(|channel| {
                        let mut plane = Vec::with_capacity(capacity);
                        samples.append(upload(channel), &mut plane);
                        plane.extend_from_slice(head(channel));
                        plane
                    })
                    .collect(),
            }
        };
        self.uploads = self.uploads.saturating_add(1);
        self.sources.entry(key.clone()).or_default().insert(
            0,
            Window {
                block,
                uploaded: self.uploads,
            },
        );
        self.enforce_total(self.byte_budget);
        if ends_source {
            self.source_ends.insert(key, end);
        }
        Ok(())
    }

    /// Drops the windows of `source` that overlap `start..end` and takes out
    /// the ones of the same speakers that end at `start` and begin at `end`.
    fn take_neighbours(
        &mut self,
        source: &StreamKey,
        start: i64,
        end: i64,
        speakers: SpeakerLayout,
    ) -> (Option<AudioBlock>, Option<AudioBlock>) {
        let Some(windows) = self.sources.get_mut(source) else {
            return (None, None);
        };
        windows.retain(|window| {
            let (window_start, window_end) = span(&window.block);
            window_end <= start || end <= window_start
        });
        let sample_rate = self.sample_rate;
        let mut meeting = |edge: fn(&AudioBlock) -> i64, at: i64| {
            windows
                .iter()
                .position(|window| {
                    window.block.speakers == speakers
                        && window.block.sample_rate == sample_rate
                        && edge(&window.block) == at
                })
                .map(|index| windows.remove(index).block)
        };
        let neighbours = (
            meeting(AudioBlock::end_sample, start),
            meeting(|block| block.start_sample, end),
        );
        if windows.is_empty() {
            self.sources.remove(source);
        }
        neighbours
    }

    /// Whether [`Self::upload`] would take an upload of this size. Checked before copying so an
    /// upload no window can hold costs no memory.
    pub(crate) fn admits(
        &self,
        sample_rate: u32,
        channels: usize,
        samples: usize,
    ) -> Result<(), UploadRefusal> {
        if sample_rate != self.sample_rate {
            return Err(UploadRefusal::WrongRate {
                expected: self.sample_rate,
            });
        }
        if samples == 0 || channels == 0 || !samples.is_multiple_of(channels) {
            return Err(UploadRefusal::Planes);
        }
        if samples / channels > self.window_frame_limit(channels) {
            return Err(UploadRefusal::TooLarge);
        }
        Ok(())
    }

    /// [`MAX_RETAINED_SECONDS`], capped by the store's byte budget.
    fn window_frame_limit(&self, channels: usize) -> usize {
        let seconds = usize::try_from(self.sample_rate)
            .unwrap_or(usize::MAX)
            .saturating_mul(MAX_RETAINED_SECONDS);
        let bytes = self.byte_budget / channels.max(1) / std::mem::size_of::<f32>();
        seconds.min(bytes)
    }

    /// Evicts least recently uploaded windows until allocated capacity fits `budget`. Growing a
    /// plane may still briefly hold both the old and new allocation.
    fn enforce_total(&mut self, budget: usize) {
        while self.retained_bytes() > budget {
            let oldest = self
                .sources
                .iter()
                .flat_map(|(name, windows)| {
                    windows
                        .iter()
                        .enumerate()
                        .map(move |(index, window)| (window.uploaded, name.clone(), index))
                })
                .min();
            let Some((_, name, index)) = oldest else {
                break;
            };
            if let Some(windows) = self.sources.get_mut(&name) {
                windows.remove(index);
                if windows.is_empty() {
                    self.sources.remove(&name);
                }
            }
        }
    }

    /// Allocated sample capacity across every source, including spare capacity.
    pub(crate) fn retained_bytes(&self) -> usize {
        self.sources
            .values()
            .flatten()
            .map(|window| {
                window
                    .block
                    .channels
                    .iter()
                    .map(|plane| plane.capacity().saturating_mul(std::mem::size_of::<f32>()))
                    .sum::<usize>()
            })
            .sum()
    }

    /// Drops every stream of `source`.
    pub(crate) fn release(&mut self, source: &str) -> bool {
        self.source_ends.retain(|(held, _), _| held != source);
        let before = self.sources.len();
        self.sources.retain(|(held, _), _| held != source);
        self.sources.len() != before
    }

    pub(crate) fn clear(&mut self) {
        self.sources.clear();
        self.source_ends.clear();
    }

    pub(crate) fn retain_sources(&mut self, keep: impl Fn(&str) -> bool) {
        self.sources.retain(|(source, _), _| keep(source));
        self.source_ends.retain(|(source, _), _| keep(source));
    }

    pub(crate) const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Discards the samples of every stream of `source` that end before `before`, a time on
    /// that source's timeline.
    ///
    /// Bounds memory while scrubbing forward. Other sources are untouched because a source time
    /// means nothing on another source's timeline.
    pub(crate) fn trim_before(&mut self, source: &str, before: MediaTime) {
        let rate = self.sample_rate;
        let cut = samples_at(before, rate);
        for (_, windows) in self
            .sources
            .iter_mut()
            .filter(|((held, _), _)| held == source)
        {
            windows.retain_mut(|Window { block, .. }| {
                let (start, end) = span(block);
                if cut <= start {
                    return true;
                }
                if cut >= end {
                    return false;
                }
                let drop = usize::try_from(cut - start).unwrap_or(0);
                for plane in &mut block.channels {
                    plane.drain(..drop.min(plane.len()));
                    // Avoid reallocating on every playback block, but release large consumed
                    // prefixes.
                    if plane.len() < plane.capacity() / 2 {
                        // Copy the tail out; shrinking in place leaves small pinned prefixes
                        // that fragment the linear heap.
                        let compact = plane.clone();
                        *plane = compact;
                    }
                }
                block.start_sample = cut;
                true
            });
        }
        self.sources.retain(|_, windows| !windows.is_empty());
    }

    /// Half-open sample intervals of the windows of `source`'s `stream`, most recently uploaded
    /// first.
    ///
    /// The store may have extended, replaced or dropped windows, so a host checks these before
    /// decoding instead of tracking its own uploads.
    pub(crate) fn windows(&self, source: &str, stream: Option<u32>) -> Vec<(i64, i64)> {
        let key = (source.to_owned(), stream);
        self.sources.get(&key).map_or_else(Vec::new, |windows| {
            windows.iter().map(|window| span(&window.block)).collect()
        })
    }

    /// Retained samples per channel across every window.
    pub(crate) fn retained_frames(&self) -> usize {
        self.sources
            .values()
            .flatten()
            .map(|window| block_frames(&window.block))
            .sum()
    }
}

impl AudioBlockProvider for AudioStore {
    /// Returns the window covering the whole request, else the one holding `start_sample`, else
    /// the earliest one starting inside the request.
    ///
    /// Uploads include silence, so a window says nothing about the source beyond it, except
    /// that nothing precedes sample zero or follows a declared end. A window that does not
    /// cover the request reads as audio that has not arrived.
    fn block(
        &self,
        source: &SourceId,
        stream: Option<u32>,
        start_sample: i64,
        frames: usize,
    ) -> Option<AudioSpan<'_>> {
        let key = (source.as_str().to_owned(), stream);
        let windows = self.sources.get(&key)?;
        let request_start = start_sample;
        let request_end = request_start.saturating_add(i64::try_from(frames).unwrap_or(i64::MAX));
        let blocks = windows.iter().map(|window| &window.block);
        blocks
            .clone()
            .find(|block| {
                let (start, end) = span(block);
                start <= request_start && request_end <= end
            })
            .or_else(|| {
                blocks.clone().find(|block| {
                    let (start, end) = span(block);
                    start <= request_start && request_start < end
                })
            })
            .or_else(|| {
                blocks
                    .filter(|block| {
                        let (start, _) = span(block);
                        request_start < start && start < request_end
                    })
                    .min_by_key(|block| block.start_sample)
            })
            .map(|block| AudioSpan {
                block,
                starts_source: block.start_sample <= 0,
                ends_source: self
                    .source_ends
                    .get(&key)
                    .is_some_and(|end| block.end_sample() >= *end),
            })
    }
}

fn block_frames(block: &AudioBlock) -> usize {
    block.frames()
}

fn span(block: &AudioBlock) -> (i64, i64) {
    (block.start_sample, block.end_sample())
}

fn samples_at(time: MediaTime, sample_rate: u32) -> i64 {
    let rate = i128::from(sample_rate.max(1));
    let value = (i128::from(time.ticks()) * rate).div_euclid(i128::from(TICKS_PER_SECOND));
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
#[path = "audio_store_tests.rs"]
mod tests;

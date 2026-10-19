//! The browser binding's audio provider: project-rate samples a host decoded
//! and uploaded, held in per-source windows under one byte budget.

use std::collections::HashMap;
use std::ops::Range;

use alpha2_media::{AudioBlock, AudioBlockProvider, AudioSpan, SourceId, SpeakerLayout};
use alpha2_time::{MediaTime, TICKS_PER_SECOND};

/// Longest window, in seconds at the project rate.
const MAX_RETAINED_SECONDS: usize = 600;
/// Bytes of samples kept across every source: about 23 minutes of 48 kHz
/// stereo. A wasm32 memory grows to 4 GB at most, so this, not a count of
/// sources or windows, is what bounds the store.
pub(crate) const MAX_RETAINED_BYTES: usize = 512 << 20;

/// Of a window that a join has to cut, the part dropped beyond what the
/// join needs, as a fraction (`1 / SLIDE_FRACTION`) of what one window
/// holds. A host continuing a full window then moves its samples once per
/// that much audio, not on every upload.
const SLIDE_FRACTION: usize = 4;

/// The samples of one upload: a plane per speaker, end to end.
///
/// The store copies them once, straight into the window that keeps them, so
/// a host's buffer (a JS `Float32Array`) need not be copied into wasm memory
/// first.
pub(crate) trait UploadSamples {
    /// Values across every plane.
    fn values(&self) -> usize;
    /// Appends values `range` to `plane`.
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

/// Host-uploaded, project-rate planar audio held per source.
///
/// A host decodes a source interval and uploads it. An upload replaces every
/// window it overlaps, which is what a seek or a re-decode produces, and joins
/// the windows of the same speakers it meets at either end into one, so a read
/// the store holds is always inside one window whichever order the host
/// decoded the pieces in. Any other upload is a window of its own: the clips
/// a replay reads from different parts of one source each read their own
/// window. A window holds at most [`MAX_RETAINED_SECONDS`]; every window of
/// every source together holds at most the byte budget, and past it the least
/// recently uploaded window of any source goes first. Nothing else limits how
/// many windows or sources the store holds.
pub(crate) struct AudioStore {
    sources: HashMap<String, Vec<Window>>,
    /// Where each source's audio ends, as a host declared with an upload
    /// that ran to it: from there on the source is silent.
    source_ends: HashMap<String, i64>,
    sample_rate: u32,
    uploads: u64,
    byte_budget: usize,
}

struct Window {
    block: AudioBlock,
    /// Upload counter value when the window last grew or was created.
    uploaded: u64,
}

/// Why the store refused an upload. It holds nothing new then, and what it
/// held before is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UploadRefusal {
    /// Not at the project rate, which is `expected`.
    WrongRate { expected: u32 },
    /// No samples, or not a full plane of one length for each speaker.
    Planes,
    /// Longer than one window holds, or more bytes than the whole store
    /// keeps: no eviction makes room for it.
    TooLarge,
}

impl Default for AudioStore {
    fn default() -> Self {
        Self::new(0)
    }
}

impl AudioStore {
    /// An empty store that takes uploads at `sample_rate`, the project rate.
    pub(crate) fn new(sample_rate: u32) -> Self {
        Self {
            sources: HashMap::new(),
            source_ends: HashMap::new(),
            sample_rate,
            uploads: 0,
            byte_budget: MAX_RETAINED_BYTES,
        }
    }

    /// Adds an upload of a slice, as [`Self::upload_from`] does.
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
            start_sample,
            sample_rate,
            speakers,
            samples,
            ends_source,
        )
    }

    /// Adds an upload as a window of its own, replacing the windows it
    /// overlaps and joining the ones it meets.
    ///
    /// `samples` holds a plane per speaker of `speakers`, end to end, every
    /// plane of one length, the first value at `start_sample` at the project
    /// rate. They are copied once, into the window that keeps them. With
    /// `ends_source`, the upload runs to the source's end, so past it the
    /// source is silent. A NaN or infinite sample is kept as a native host
    /// keeps it: the mixer reads it as silence.
    ///
    /// # Errors
    /// The [`UploadRefusal`] [`Self::admits`] answers; the store is then
    /// unchanged.
    ///
    /// A joined window keeps the whole upload and as much of its neighbours
    /// as one window holds, the samples after the upload first: playback
    /// reads forward, so what precedes an upload is the likelier to be done
    /// with. A join that has to drop samples before the upload drops another
    /// [`SLIDE_FRACTION`] of a window with them, so a host continuing a full
    /// window moves its samples once per that much audio rather than on
    /// every upload. A window continued after its end keeps its planes and
    /// grows them by doubling, up to what one window holds and what the
    /// budget leaves free, so its samples move a few times as it fills, not
    /// on every upload. Other sources' windows are evicted, least recently
    /// uploaded first, before the new samples are allocated, and never to
    /// make spare room.
    pub(crate) fn upload_from<S: UploadSamples + ?Sized>(
        &mut self,
        source: &str,
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
        let (left, right) = self.take_neighbours(source, start_sample, end, speakers);
        let room = limit - frames;
        let right_frames = right.as_ref().map_or(0, AudioBlock::frames).min(room);
        let left_held = left.as_ref().map_or(0, AudioBlock::frames);
        let mut left_frames = left_held.min(room - right_frames);
        if left_frames < left_held {
            left_frames = left_frames.saturating_sub(limit / SLIDE_FRACTION);
        }
        let needed = left_frames + frames + right_frames;
        let frame_bytes = channels.saturating_mul(std::mem::size_of::<f32>());
        // The neighbours are out of the store: this is what every other window holds.
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
            // The left neighbour's planes: what they keep moves to their front, and they
            // reallocate only to grow.
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
        self.sources.entry(source.to_owned()).or_default().insert(
            0,
            Window {
                block,
                uploaded: self.uploads,
            },
        );
        self.enforce_total(self.byte_budget);
        if ends_source {
            self.source_ends.insert(source.to_owned(), end);
        }
        Ok(())
    }

    /// Drops the windows of `source` that overlap `start..end` and takes out
    /// the ones of the same speakers that end at `start` and begin at `end`.
    fn take_neighbours(
        &mut self,
        source: &str,
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

    /// Whether an upload of `samples` values in `channels` planes at
    /// `sample_rate` is one [`Self::upload`] takes, from its length alone:
    /// a caller checks it before copying the samples in, so an upload no
    /// window can hold costs no memory.
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

    /// Frames one window of `channels` holds: [`MAX_RETAINED_SECONDS`], and
    /// no more than the whole store's byte budget.
    fn window_frame_limit(&self, channels: usize) -> usize {
        let seconds = usize::try_from(self.sample_rate)
            .unwrap_or(usize::MAX)
            .saturating_mul(MAX_RETAINED_SECONDS);
        let bytes = self.byte_budget / channels.max(1) / std::mem::size_of::<f32>();
        seconds.min(bytes)
    }

    /// Evicts the least recently uploaded windows until what is held, by
    /// actual capacity, fits `budget`. An upload calls it before allocating
    /// with room for its window, so the store never holds more than its
    /// budget; growing a plane may still hold the old and the new allocation
    /// for a moment.
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

    /// Drops one source's retained windows.
    pub(crate) fn release(&mut self, source: &str) -> bool {
        self.source_ends.remove(source);
        self.sources.remove(source).is_some()
    }

    /// Drops every retained window.
    pub(crate) fn clear(&mut self) {
        self.sources.clear();
        self.source_ends.clear();
    }

    /// Keeps only the windows of sources `keep` accepts.
    pub(crate) fn retain_sources(&mut self, keep: impl Fn(&str) -> bool) {
        self.sources.retain(|source, _| keep(source));
        self.source_ends.retain(|source, _| keep(source));
    }

    /// Project sample rate every retained window is stored at.
    pub(crate) const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Discards `source`'s samples that end before `before`, a time on that
    /// source's own timeline.
    ///
    /// Hosts call this to bound memory while scrubbing forward. A window that
    /// ends before `before` is dropped; a window containing it is trimmed.
    /// Other sources keep theirs: a source time means nothing on another
    /// source's timeline.
    pub(crate) fn trim_before(&mut self, source: &str, before: MediaTime) {
        let rate = self.sample_rate;
        let cut = samples_at(before, rate);
        if let Some(windows) = self.sources.get_mut(source) {
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
                    // prefixes instead of hiding their capacity.
                    if plane.len() < plane.capacity() / 2 {
                        // Move the tail out of the large allocation; shrinking in place would leave
                        // small pinned prefixes that fragment the linear heap.
                        let compact = plane.clone();
                        *plane = compact;
                    }
                }
                block.start_sample = cut;
                true
            });
        }
        if self.sources.get(source).is_some_and(Vec::is_empty) {
            self.sources.remove(source);
        }
    }

    /// Half-open sample intervals of one source's retained windows, at the
    /// store's rate, most recently uploaded first.
    ///
    /// A host checks these before decoding rather than tracking what it
    /// uploaded, since the store may have extended, replaced or dropped a
    /// window since.
    pub(crate) fn windows(&self, source: &str) -> Vec<(i64, i64)> {
        self.sources.get(source).map_or_else(Vec::new, |windows| {
            windows.iter().map(|window| span(&window.block)).collect()
        })
    }

    /// Returns the number of retained samples per channel across every window.
    pub(crate) fn retained_frames(&self) -> usize {
        self.sources
            .values()
            .flatten()
            .map(|window| block_frames(&window.block))
            .sum()
    }
}

impl AudioBlockProvider for AudioStore {
    /// Returns the window that covers the whole request, or failing that the
    /// one holding `start_sample`, or the earliest one starting inside the
    /// request.
    ///
    /// Uploads are the samples a host decoded, silence included: a host
    /// decoding past a source's end uploads zeros there, or declares the end
    /// with its last upload. So a window says nothing about the source
    /// beyond it, except that nothing precedes sample zero or follows a
    /// declared end, and one that does not cover the request reads as audio
    /// that has not arrived.
    fn block(&self, source: &SourceId, start_sample: i64, frames: usize) -> Option<AudioSpan<'_>> {
        let windows = self.sources.get(source.as_str())?;
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
                    .get(source.as_str())
                    .is_some_and(|end| block.end_sample() >= *end),
            })
    }
}

fn block_frames(block: &AudioBlock) -> usize {
    block.frames()
}

/// Half-open sample interval a block covers.
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

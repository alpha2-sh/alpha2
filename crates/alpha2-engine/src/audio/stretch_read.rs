//! Reading a clip that keeps its pitch: through [`StretchReader`] instead of
//! the resampling read.
//!
//! Grains read in read positions: source samples, negated for a reversed
//! clip, so a grain always reads forward and the stretch runs on the reversed
//! signal. Each grain's centre reads where the clip's exact [`SourceMap`] puts
//! that project sample, so the speed and the alignment with the picture are
//! those of the plain read, up to the search tolerance. Nothing outside the
//! clip's source range is read.

use std::collections::HashMap;

use alpha2_audio::{Stretch, StretchReader, StretchSignal};
use alpha2_eval::ActiveAudioClip;
use alpha2_media::{AudioBlockProvider, SourceId};
use alpha2_model::{ClipId, TimeMap};
use alpha2_time::TimeRange;

use super::{AudioRenderer, Fill, SourceMap, mixed_sample, sample_at_or_after};
use crate::downmix::Downmix;

/// Whether `clip` reads through the stretch: it keeps its pitch at a speed
/// other than 1.
pub(super) fn stretches(clip: &ActiveAudioClip) -> bool {
    clip.audio.preserve_pitch
        && matches!(clip.time_map, TimeMap::Forward | TimeMap::Reverse)
        && clip.timeline_range.duration() != clip.source_range.duration()
}

/// Where a stretched clip's grains read.
struct GrainMap {
    map: SourceMap,
    reversed: bool,
    /// The clip's first project sample, the stretch's output sample zero.
    origin: i64,
    hop: i64,
    /// The read positions of the clip's source range, `[from, to)`.
    bounds: (i64, i64),
}

impl GrainMap {
    fn new(clip: &ActiveAudioClip, rate: u32, hop: i64) -> Option<Self> {
        let reversed = clip.time_map == TimeMap::Reverse;
        let map = SourceMap::stretched(clip, reversed, rate)?;
        let low = sample_at_or_after(clip.source_range.start().ticks(), rate);
        let high = sample_at_or_after(clip.source_range.end().ticks(), rate);
        Some(Self {
            map,
            reversed,
            origin: sample_at_or_after(clip.timeline_range.start().ticks(), rate),
            hop,
            bounds: if reversed {
                (1 - high, 1 - low)
            } else {
                (low, high)
            },
        })
    }

    /// Grain `grain`'s read position: its centre, a hop after its start, at
    /// the source position of that project sample, floored in read order.
    fn position(&self, grain: i64) -> Option<i64> {
        let centre = grain.checked_add(1)?.checked_mul(self.hop)?;
        let (whole, fraction) = self.map.position(self.origin.checked_add(centre)?)?;
        let at = if self.reversed {
            whole
                .checked_add(i64::from(fraction > 0.0))?
                .checked_neg()?
        } else {
            whole
        };
        at.checked_sub(self.hop)
    }

    /// The source sample at read position `at`.
    const fn source(&self, at: i64) -> i64 {
        if self.reversed { -at } else { at }
    }

    /// The source samples, `[from, to)`, of the read positions `[from, to)`
    /// inside the clip's source range; `None` when none is.
    fn source_window(&self, from: i64, to: i64) -> Option<(i64, i64)> {
        let (from, to) = (from.max(self.bounds.0), to.min(self.bounds.1));
        if from >= to {
            return None;
        }
        Some(if self.reversed {
            (1 - to, 1 - from)
        } else {
            (from, to)
        })
    }
}

/// What a stretched read of project samples takes.
struct Window {
    grains: GrainMap,
    /// The stretch's first output sample.
    first: i64,
    /// The read positions the stretch can touch, `[from, to)`.
    read: (i64, i64),
    /// Their source samples inside the clip's source range.
    source: Option<(i64, i64)>,
}

/// The window reading `count` project samples of `clip` from `first_sample`
/// takes, `None` where a position does not fit.
fn window(
    stretch: &Stretch,
    clip: &ActiveAudioClip,
    first_sample: i64,
    count: usize,
    rate: u32,
) -> Option<Window> {
    let grains = GrainMap::new(clip, rate, stretch.hop())?;
    let first = first_sample.checked_sub(grains.origin)?;
    let end = first.checked_add(i64::try_from(count).ok()?)?;
    // Every grain the reach visits lies between the first chain's start and the last chain's end,
    // so checking those two positions checks them all: the map is monotonic.
    let (low, high) = stretch.grains(first, end);
    grains.position(Stretch::chain(Stretch::segment(low)).0)?;
    grains.position(Stretch::chain(Stretch::segment(high)).1)?;
    let read = stretch.reach_output(first, end, |grain| grains.position(grain).unwrap_or(0));
    let source = grains.source_window(read.0, read.1);
    Some(Window {
        grains,
        first,
        read,
        source,
    })
}

/// The source samples, `[from, to)`, reading `count` project samples of a
/// stretched `clip` from `first_sample` takes, as [`AudioRenderer::read_stretched`]
/// asks for them.
pub(super) fn stretched_request(
    stretch: &Stretch,
    clip: &ActiveAudioClip,
    first_sample: i64,
    count: usize,
    rate: u32,
) -> Option<(i64, i64)> {
    window(stretch, clip, first_sample, count, rate)?.source
}

/// A clip's reader, with what it was made for: a reader keeps offsets that
/// hold only for the same source, ranges and direction.
pub(super) struct StretchSlot {
    source: SourceId,
    timeline_range: TimeRange,
    source_range: TimeRange,
    time_map: TimeMap,
    reader: StretchReader,
    /// The block that last read through it.
    pub(super) used_in: u64,
}

/// The reader of `clip` in `slots`, made afresh when there is none or the
/// clip has changed, marked as read in block `block`.
fn reader_for<'a>(
    slots: &'a mut HashMap<ClipId, StretchSlot>,
    clip: &ActiveAudioClip,
    rate: u32,
    block: u64,
) -> &'a mut StretchReader {
    let fresh = || StretchSlot {
        source: clip.source_id.clone(),
        timeline_range: clip.timeline_range,
        source_range: clip.source_range,
        time_map: clip.time_map,
        reader: StretchReader::new(rate),
        used_in: block,
    };
    let slot = slots.entry(clip.clip_id.clone()).or_insert_with(fresh);
    if slot.source != *clip.source_id
        || slot.timeline_range != clip.timeline_range
        || slot.source_range != clip.source_range
        || slot.time_map != clip.time_map
    {
        *slot = fresh();
    }
    slot.used_in = block;
    &mut slot.reader
}

impl AudioRenderer {
    /// Reads `count` project samples of a stretched `clip` from
    /// `first_sample` into `clip_a`, or with [`Fill::Presence`] only checks
    /// that it could.
    ///
    /// Returns whether every requested sample came from supplied media.
    pub(super) fn read_stretched(
        &mut self,
        clip: &ActiveAudioClip,
        first_sample: i64,
        count: usize,
        provider: &dyn AudioBlockProvider,
        fill: Fill,
    ) -> bool {
        let rate = self.sample_rate;
        let Some(window) = window(&self.stretch, clip, first_sample, count, rate) else {
            return false;
        };
        // A read wholly outside the source range is silence, which `clip_a` already holds.
        let Some((source_from, source_to)) = window.source else {
            return true;
        };
        let frames = usize::try_from(source_to - source_from).unwrap_or(0);
        let Some(span) = provider.block(clip.source_id, source_from, frames) else {
            return false;
        };
        if !span.covers(source_from, source_to) {
            return false;
        }
        let source = span.block;
        if source.sample_rate != rate || source.speakers.channels() != source.channels.len() {
            return false;
        }
        let Some(downmix) = Downmix::new(source.speakers.speakers(), self.layout) else {
            return false;
        };
        if fill == Fill::Presence {
            return true;
        }
        #[cfg(test)]
        {
            self.samples_read += count;
        }

        let grains = &window.grains;
        let (from, to) = window.read;
        let channels = self.layout.channels();
        let planes: Vec<Vec<f32>> = (0..channels)
            .map(|channel| {
                let mix = downmix.channel(channel);
                (from..to)
                    .map(|at| {
                        if at < grains.bounds.0 || at >= grains.bounds.1 {
                            0.0
                        } else {
                            mixed_sample(
                                mix,
                                &source.channels,
                                grains.source(at) - source.start_sample,
                            )
                        }
                    })
                    .collect()
            })
            .collect();
        // The search runs on the channel mean, summed in channel order.
        #[allow(clippy::cast_precision_loss)]
        let scale = 1.0 / channels as f32;
        let mono: Vec<f32> = (0..planes[0].len())
            .map(|index| planes.iter().map(|plane| plane[index]).sum::<f32>() * scale)
            .collect();
        let signals: Vec<StretchSignal<'_>> = planes
            .iter()
            .map(|plane| StretchSignal {
                start: from,
                samples: plane,
            })
            .collect();

        let reader = reader_for(&mut self.stretch_slots, clip, rate, self.blocks_mixed);
        let mut outputs: Vec<&mut [f32]> = self
            .clip_a
            .iter_mut()
            .take(channels)
            .map(|plane| &mut plane[..count])
            .collect();
        reader.render(
            window.first,
            &mut outputs,
            |grain| grains.position(grain).unwrap_or(0),
            &signals,
            StretchSignal {
                start: from,
                samples: &mono,
            },
        );
        true
    }
}

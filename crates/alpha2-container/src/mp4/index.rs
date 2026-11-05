//! Where each sample of a track is and when it plays after edit lists, indexed as `FFmpeg`'s MOV
//! demuxer indexes it so that packet timestamps match.

use super::describe::Family;
use super::movie::{AudioFields, Trak};
use crate::{DemuxError, TrackKind};

/// A sync sample.
pub(crate) const KEY: u8 = 1;
/// A sample outside every edit, decoded only to reach a later one.
pub(crate) const DISCARD: u8 = 2;

/// The most index entries one track may have.
pub(crate) const MAX_ENTRIES: usize = 1 << 23;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Entry {
    pub pos: u64,
    pub dts: i64,
    pub size: u32,
    /// The `stts` delta and composition offset, when the tables cover the sample.
    pub tts: Option<(u32, i32)>,
    pub flags: u8,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Index {
    pub entries: Vec<Entry>,
    /// Packet durations come from `stts` (or `trun`).
    pub has_stts: bool,
    /// Presentation times add composition offsets.
    pub has_ctts: bool,
    pub dts_shift: i32,
    /// The stream duration in the track timescale (`FFmpeg`'s `st->duration`).
    pub duration: i64,
    /// Samples to skip at the start (`FFmpeg`'s `skip_samples`).
    pub skip_samples: i64,
    /// Audio priming (`FFmpeg`'s `start_pad`).
    pub start_pad: i64,
    pub time_offset: i64,
    /// The decode time after the last sample, where an untimed fragment continues.
    pub track_end: i64,
}

/// File-wide state that `FFmpeg` carries from one track to the next.
pub(crate) struct Context {
    pub movie_timescale: u32,
    /// `FFmpeg`'s `advanced_editlist`, turned off for good by the first track without `stts`.
    pub advanced: bool,
    pub file_len: u64,
}

fn invalid(reason: &str) -> DemuxError {
    DemuxError::Invalid(reason.into())
}

/// `a * b / c` rounded to nearest, halves away from zero (`av_rescale`).
pub(crate) fn rescale(a: i64, b: i64, c: i64) -> i64 {
    if c == 0 {
        return 0;
    }
    let product = i128::from(a) * i128::from(b);
    let c = i128::from(c);
    let half = c.abs() / 2;
    let rounded = if (product < 0) == (c < 0) {
        (product.abs() + half) / c.abs()
    } else {
        -((product.abs() + half) / c.abs())
    };
    i64::try_from(rounded).unwrap_or(if rounded < 0 { i64::MIN } else { i64::MAX })
}

/// What indexing needs to know of a track's codec and sound description.
pub(crate) struct Layout<'a> {
    pub kind: TrackKind,
    pub family: Family,
    pub pcm_frame_bytes: u32,
    pub audio: &'a AudioFields,
}

/// Builds the index of `trak` from its sample tables and edit list.
pub(crate) fn build(
    trak: &Trak,
    layout: &Layout<'_>,
    context: &mut Context,
) -> Result<Index, DemuxError> {
    let tables = &trak.tables;
    let mut stsc = tables.stsc.clone();
    if tables.chunks.is_empty() && tables.stts.is_empty() && !stsc.is_empty() {
        stsc.clear();
    }
    let sample_size = if layout.pcm_frame_bytes > 0 {
        layout.pcm_frame_bytes
    } else {
        tables.constant_size
    };
    let mut index = Index {
        dts_shift: tables.dts_shift,
        duration: i64::try_from(trak.media_duration).unwrap_or(i64::MAX),
        ..Index::default()
    };
    if tables.constant_size > 0 && u64::from(tables.sample_count) > context.file_len {
        return Err(invalid(
            "a track declares more samples than the file has bytes",
        ));
    }
    let deltas = corrected_deltas(&tables.stts, &mut index);
    if (!tables.chunks.is_empty()
        && (tables.stts.is_empty()
            || stsc.is_empty()
            || (sample_size == 0 && tables.sample_count == 0)))
        || (tables.chunks.is_empty() && tables.sample_count > 0)
    {
        return Ok(index);
    }
    if stsc
        .last()
        .is_some_and(|last| last.first as usize > tables.chunks.len())
    {
        return Err(invalid(
            "the stsc box names chunks the stco box does not have",
        ));
    }
    if tables.stts.is_empty() && context.advanced {
        context.advanced = false;
    }
    let mut current_dts = edit_prepass(trak, layout, context, &mut index);
    let chunked_pcm = layout.kind == TrackKind::Audio && deltas.len() == 1 && deltas[0].1 == 1;
    let complete = if chunked_pcm {
        chunked_entries(
            trak,
            &stsc,
            sample_size,
            layout.audio,
            &mut current_dts,
            &mut index,
            context.file_len,
        )?
    } else {
        current_dts = current_dts.saturating_sub(i64::from(index.dts_shift));
        sample_entries(
            trak,
            &stsc,
            &deltas,
            sample_size,
            current_dts,
            &mut index,
            layout.kind,
        )?
    };
    if complete && context.advanced && !trak.edits.is_empty() && !index.entries.is_empty() {
        fix_index(trak, layout, context.movie_timescale, &mut index);
    }
    Ok(index)
}

/// `stts` deltas as `FFmpeg` corrects them: deltas past `max_stts_delta` are clipped to 1, with
/// negative ones (stored as `int32`) pulling later deltas back. Sets the duration they sum to.
fn corrected_deltas(stts: &[(u32, u32)], index: &mut Index) -> Vec<(u32, u32)> {
    const MAX_DELTA: u32 = u32::MAX - 48_000 * 10;
    let mut duration: i64 = 0;
    let mut current: i64 = 0;
    let mut corrected: i64 = 0;
    let mut out = Vec::with_capacity(stts.len());
    for &(count, delta) in stts {
        let mut delta = delta;
        let count64 = i64::from(count);
        if delta > MAX_DELTA {
            let magnitude = i32::from_be_bytes(delta.to_be_bytes());
            delta = 1;
            let step = if magnitude < 0 {
                i64::from(magnitude)
            } else {
                1
            };
            corrected = corrected.saturating_add(step.saturating_mul(count64));
        } else {
            corrected = corrected.saturating_add(i64::from(delta).saturating_mul(count64));
        }
        current = current.saturating_add(i64::from(delta).saturating_mul(count64));
        if current > corrected {
            let drift = current.saturating_sub(corrected) / count64.max(1);
            // `FFmpeg` computes the correction in 32-bit unsigned arithmetic, wrapping included.
            let correction = if i64::from(delta) > drift {
                u32::try_from(drift).unwrap_or(u32::MAX)
            } else {
                delta.wrapping_sub(1)
            };
            current = current.saturating_sub(i64::from(correction).saturating_mul(count64));
            delta = delta.wrapping_sub(correction);
        }
        duration = duration.saturating_add(i64::from(delta).saturating_mul(count64));
        out.push((count, delta));
    }
    if duration != 0 {
        index.duration = index.duration.min(duration);
    }
    index.track_end = duration;
    out
}

/// The first-dts adjustment and priming `FFmpeg` takes from the edit list before indexing.
fn edit_prepass(trak: &Trak, layout: &Layout<'_>, context: &Context, index: &mut Index) -> i64 {
    if trak.edits.is_empty() {
        return 0;
    }
    let mut start = 0;
    let mut empty_duration: i64 = 0;
    let mut start_time: i64 = 0;
    let mut multiple = false;
    for (position, edit) in trak.edits.iter().enumerate() {
        if position == 0 && edit.media_time == -1 {
            empty_duration = i64::try_from(edit.duration).unwrap_or(i64::MAX);
            start = 1;
        } else if position == start && edit.media_time >= 0 {
            start_time = edit.media_time;
        } else {
            multiple = true;
        }
    }
    let mut current_dts = 0;
    if (empty_duration != 0 || start_time != 0) && context.movie_timescale > 0 {
        if empty_duration != 0 {
            empty_duration = rescale(
                empty_duration,
                i64::from(trak.timescale),
                i64::from(context.movie_timescale),
            );
        }
        index.time_offset = start_time.wrapping_sub(empty_duration);
        if !context.advanced {
            current_dts = index.time_offset.wrapping_neg();
        }
    }
    if !multiple && !context.advanced && layout.family == Family::Aac && start_time > 0 {
        index.start_pad = start_time;
    }
    current_dts
}

/// Per-sample `stts` deltas and `ctts` offsets, as far as each table reaches.
fn time_to_sample(
    deltas: &[(u32, u32)],
    ctts: &[(u32, i32)],
    samples: usize,
    with_stts: bool,
) -> Vec<(u32, i32)> {
    fn expand<T: Copy>(entries: &[(u32, T)], samples: usize) -> Vec<T> {
        entries
            .iter()
            .flat_map(|&(count, value)| {
                std::iter::repeat_n(value, usize::try_from(count).unwrap_or(usize::MAX))
            })
            .take(samples)
            .collect()
    }
    let offsets = expand(ctts, samples);
    let durations = if with_stts {
        expand(deltas, samples)
    } else {
        Vec::new()
    };
    (0..offsets.len().max(durations.len()))
        .map(|at| {
            (
                durations.get(at).copied().unwrap_or(0),
                offsets.get(at).copied().unwrap_or(0),
            )
        })
        .collect()
}

/// The index of an ordinary track, one entry per sample. Returns whether every sample was
/// indexed (`FFmpeg` stops at the first broken one and then skips the edit list).
#[allow(clippy::too_many_lines)]
fn sample_entries(
    trak: &Trak,
    stsc: &[super::movie::Stsc],
    deltas: &[(u32, u32)],
    sample_size: u32,
    mut current_dts: i64,
    index: &mut Index,
    kind: TrackKind,
) -> Result<bool, DemuxError> {
    let tables = &trak.tables;
    let sample_count = tables.sample_count as usize;
    if sample_count == 0 {
        return Ok(true);
    }
    if sample_count > MAX_ENTRIES {
        return Err(invalid("a track has too many samples"));
    }
    index.has_stts = !deltas.is_empty();
    index.has_ctts = !tables.ctts.is_empty();
    let tts = time_to_sample(deltas, &tables.ctts, sample_count, true);
    let keyframes = tables.stss.as_deref();
    let keyframe_absent = keyframes.is_some_and(<[u32]>::is_empty);
    let keyframes = keyframes.unwrap_or_default();
    let key_off = u32::from(
        keyframes.first().is_some_and(|first| *first > 0)
            || tables.stps.first().is_some_and(|first| *first > 0),
    );
    let rap_present = !tables.rap_groups.is_empty();
    let mut stsz_size = tables.constant_size;
    let mut entries = Vec::with_capacity(sample_count.min(1 << 16));
    let (mut stsc_index, mut sync_at, mut partial_at) = (0_usize, 0_usize, 0_usize);
    let (mut rap_index, mut rap_sample) = (0_usize, 0_u32);
    let mut current_sample = 0_usize;
    let mut tts_index = 0_usize;
    for (chunk, &offset) in tables.chunks.iter().enumerate() {
        let next_offset = tables
            .chunks
            .get(chunk + 1)
            .map_or(i64::MAX, |next| i64::try_from(*next).unwrap_or(i64::MAX));
        let mut current_offset = i64::try_from(offset).unwrap_or(i64::MAX);
        while stsc_index + 1 < stsc.len() && chunk + 1 == stsc[stsc_index + 1].first as usize {
            stsc_index += 1;
        }
        let per_chunk = stsc[stsc_index].count;
        if next_offset > current_offset
            && sample_size > 0
            && sample_size < stsz_size
            && i64::from(per_chunk) * i64::from(stsz_size) > next_offset - current_offset
        {
            stsz_size = sample_size;
        }
        if stsz_size > 0 && stsz_size < sample_size {
            stsz_size = sample_size;
        }
        for _ in 0..per_chunk {
            if current_sample >= sample_count {
                index.entries = entries;
                return Ok(false);
            }
            let number = u32::try_from(current_sample)
                .unwrap_or(u32::MAX)
                .saturating_add(key_off);
            let mut keyframe = false;
            if !keyframe_absent && (keyframes.is_empty() || keyframes.get(sync_at) == Some(&number))
            {
                keyframe = true;
                if sync_at + 1 < keyframes.len() {
                    sync_at += 1;
                }
            } else if tables.stps.get(partial_at) == Some(&number) {
                keyframe = true;
                if partial_at + 1 < tables.stps.len() {
                    partial_at += 1;
                }
            }
            if let Some(&(count, description)) = tables.rap_groups.get(rap_index) {
                if description > 0 {
                    keyframe = true;
                }
                rap_sample += 1;
                if rap_sample == count {
                    rap_sample = 0;
                    rap_index += 1;
                }
            }
            if keyframe_absent
                && tables.stps.is_empty()
                && !rap_present
                && (kind == TrackKind::Audio || current_sample == 0)
            {
                keyframe = true;
            }
            let size = if stsz_size > 0 {
                stsz_size
            } else {
                tables.sizes.get(current_sample).copied().unwrap_or(0)
            };
            if current_offset > i64::MAX - i64::from(size) {
                index.entries = entries;
                return Ok(false);
            }
            let wanted = trak
                .pseudo_entry
                .is_none_or(|entry| stsc[stsc_index].id.wrapping_sub(1) == entry);
            if wanted {
                if size > 0x3fff_ffff {
                    index.entries = entries;
                    return Ok(false);
                }
                entries.push(Entry {
                    pos: current_offset.unsigned_abs(),
                    dts: current_dts,
                    size,
                    tts: tts.get(current_sample).copied(),
                    flags: if keyframe { KEY } else { 0 },
                });
            }
            current_offset += i64::from(size);
            current_dts =
                current_dts.saturating_add(i64::from(tts.get(tts_index).map_or(0, |slot| slot.0)));
            current_sample += 1;
            if tts_index + 1 < tts.len() {
                tts_index += 1;
            }
        }
    }
    index.entries = entries;
    Ok(true)
}

/// The index of PCM audio whose `stts` counts single samples: each chunk is cut into packets of
/// up to 1024 samples, as `FFmpeg` does.
fn chunked_entries(
    trak: &Trak,
    stsc: &[super::movie::Stsc],
    sample_size: u32,
    audio: &AudioFields,
    current_dts: &mut i64,
    index: &mut Index,
    file_len: u64,
) -> Result<bool, DemuxError> {
    let tables = &trak.tables;
    if tables.chunks.is_empty() {
        return Ok(true);
    }
    index.has_ctts = !tables.ctts.is_empty();
    let per_frame = audio.samples_per_frame;
    let mut total: u64 = 0;
    for (position, entry) in stsc.iter().enumerate() {
        let chunk_samples = entry.count;
        if position + 1 != stsc.len() && per_frame > 0 && chunk_samples % per_frame != 0 {
            return Ok(false);
        }
        let count = if per_frame >= 160 {
            chunk_samples / per_frame
        } else if per_frame > 1 {
            let samples = (1024 / per_frame) * per_frame;
            chunk_samples.div_ceil(samples)
        } else {
            chunk_samples.div_ceil(1024)
        };
        let chunk_count = match stsc.get(position + 1) {
            Some(next) => next.first.wrapping_sub(entry.first),
            None => u32::try_from(tables.chunks.len())
                .unwrap_or(u32::MAX)
                .wrapping_sub(entry.first.wrapping_sub(1)),
        };
        total = total.saturating_add(u64::from(chunk_count) * u64::from(count));
    }
    let Some(total) = usize::try_from(total)
        .ok()
        .filter(|total| *total <= MAX_ENTRIES && *total as u64 <= file_len)
    else {
        return Err(invalid("a PCM track has too many packets"));
    };
    let tts = time_to_sample(&[], &tables.ctts, total, false);
    let mut entries = Vec::with_capacity(total.min(1 << 16));
    let mut stsc_index = 0;
    for (chunk, &offset) in tables.chunks.iter().enumerate() {
        let mut current_offset = offset;
        if stsc_index + 1 < stsc.len() && chunk + 1 == stsc[stsc_index + 1].first as usize {
            stsc_index += 1;
        }
        let mut chunk_samples = stsc[stsc_index].count;
        while chunk_samples > 0 {
            if per_frame > 1 && audio.bytes_per_frame == 0 {
                index.entries = entries;
                return Ok(false);
            }
            let (samples, size) = if per_frame >= 160 {
                (per_frame, audio.bytes_per_frame)
            } else if per_frame > 1 {
                let samples = ((1024 / per_frame) * per_frame).min(chunk_samples);
                (
                    samples,
                    (samples / per_frame).wrapping_mul(audio.bytes_per_frame),
                )
            } else {
                let samples = chunk_samples.min(1024);
                (samples, samples.wrapping_mul(sample_size))
            };
            if entries.len() >= total || size > 0x3fff_ffff {
                index.entries = entries;
                return Ok(false);
            }
            let position = entries.len();
            entries.push(Entry {
                pos: current_offset,
                dts: *current_dts,
                size,
                tts: tts.get(position).copied(),
                flags: KEY,
            });
            current_offset = current_offset.saturating_add(u64::from(size));
            *current_dts = current_dts.saturating_add(i64::from(samples));
            chunk_samples = chunk_samples.saturating_sub(samples);
        }
    }
    index.entries = entries;
    Ok(true)
}

/// `av_index_search_timestamp` with `AVSEEK_FLAG_BACKWARD`: the last entry at or before
/// `wanted`, skipping back to a keyframe unless `any`.
fn search_backward(entries: &[Entry], wanted: i64, any: bool) -> Option<usize> {
    let count = entries.len();
    // `low` is `FFmpeg`'s `a`, with `None` for -1.
    let mut low: Option<usize> = None;
    let mut high = count;
    if count > 0 && entries[count - 1].dts < wanted {
        low = Some(count - 1);
    }
    while high > low.map_or(0, |low| low + 1) {
        let mut middle = (low.map_or(0, |low| low + 1) + high - 1) / 2;
        while entries[middle].flags & DISCARD != 0 && middle < high && middle + 1 < count {
            middle += 1;
            if middle == high && entries[middle].dts >= wanted {
                middle = high - 1;
                break;
            }
        }
        let timestamp = entries[middle].dts;
        if timestamp >= wanted {
            high = middle;
        }
        if timestamp <= wanted {
            low = Some(middle);
        }
    }
    let mut found = low;
    if !any {
        while let Some(at) = found.filter(|at| entries[*at].flags & KEY == 0) {
            found = at.checked_sub(1);
        }
    }
    found
}

/// `find_prev_closest_index`: the entry to start decoding an edit that begins at `timestamp`.
fn previous_closest(
    entries: &[Entry],
    has_ctts: bool,
    dts_shift: i32,
    timestamp: i64,
    any: bool,
) -> Option<usize> {
    let timestamp = if dts_shift > 0 {
        timestamp.saturating_sub(i64::from(dts_shift))
    } else {
        timestamp
    };
    let mut index = search_backward(entries, timestamp, any)?;
    let mut at = index;
    while at > 0 && entries[at].dts == entries[at - 1].dts {
        if any || entries[at - 1].flags & KEY != 0 {
            index = at - 1;
        }
        at -= 1;
    }
    if !has_ctts {
        return Some(index);
    }
    loop {
        let entry = entries[index];
        let Some((_, offset)) = entry.tts else {
            return Some(index);
        };
        if entry.dts.saturating_add(i64::from(offset)) <= timestamp && entry.flags & KEY != 0 {
            return Some(index);
        }
        index = index.checked_sub(1)?;
    }
}

/// `mov_fix_index`: keeps the samples each edit shows (and those needed to decode them) and
/// moves their timestamps onto the presentation timeline.
#[allow(clippy::too_many_lines)]
fn fix_index(trak: &Trak, layout: &Layout<'_>, movie_timescale: u32, index: &mut Index) {
    let old = std::mem::take(&mut index.entries);
    let audio = layout.kind == TrackKind::Audio;
    let vorbis = layout.family == Family::Vorbis;
    let timescale = i64::from(trak.timescale);
    let dts_shift = i64::from(index.dts_shift);
    let mut entries: Vec<Entry> = Vec::with_capacity(old.len());
    // Edits may show samples more than once, but not without bound.
    let most = old.len().saturating_mul(8).min(MAX_ENTRIES);
    let mut entry_end = -dts_shift;
    let start_dts = entry_end;
    let mut min_corrected_pts: i64 = -1;
    let mut empty_edits: i64 = 0;
    let mut found_non_empty = false;
    let mut first_audio_edit: i32 = -1;
    let mut skip_samples = index.skip_samples;
    for edit in &trak.edits {
        if movie_timescale == 0 {
            break;
        }
        let media_time = edit.media_time;
        let mut duration = rescale(
            i64::try_from(edit.duration).unwrap_or(i64::MAX),
            timescale,
            i64::from(movie_timescale),
        );
        if duration
            .cast_unsigned()
            .wrapping_add(media_time.cast_unsigned())
            > i64::MAX.cast_unsigned()
        {
            duration = 0;
        }
        let mut counter = entry_end;
        entry_end = entry_end.saturating_add(duration);
        let mut discarded_begin: Vec<i64> = Vec::new();
        if !found_non_empty && media_time == -1 {
            empty_edits = empty_edits.saturating_add(duration);
            continue;
        }
        found_non_empty = true;
        if audio {
            first_audio_edit = i32::from(first_audio_edit < 0);
            if first_audio_edit > 0 {
                skip_samples = 0;
                index.start_pad = 0;
            }
        }
        let mut search = media_time;
        if audio {
            search = search.saturating_sub(timescale).max(old[0].dts);
        }
        let start = previous_closest(&old, index.has_ctts, index.dts_shift, search, false)
            .or_else(|| previous_closest(&old, index.has_ctts, index.dts_shift, search, true))
            .unwrap_or(0);
        let mut started = false;
        let mut keyframe_after_edit = false;
        let mut position = start;
        while position < old.len() {
            let current = old[position];
            let frame_duration = old
                .get(position + 1)
                .map_or(duration, |next| next.dts.saturating_sub(current.dts));
            let mut flags = current.flags;
            let mut cts = current.dts.saturating_add(dts_shift);
            let mut composition = 0;
            if let Some((_, offset)) = current.tts {
                composition = i64::from(offset);
                cts = cts.saturating_add(composition);
            }
            let edit_end = duration.saturating_add(media_time);
            if cts < media_time || cts >= edit_end {
                if audio
                    && !vorbis
                    && cts < media_time
                    && cts.saturating_add(frame_duration) > media_time
                    && first_audio_edit > 0
                {
                    let packet_skip = media_time.saturating_sub(cts);
                    skip_samples = skip_samples.saturating_add(packet_skip);
                    counter = counter.saturating_sub(packet_skip);
                    if !started {
                        started = true;
                        rewrite_discarded(&mut entries, counter, &discarded_begin);
                        discarded_begin.clear();
                    }
                } else {
                    flags |= DISCARD;
                    if !started {
                        discarded_begin.push(frame_duration);
                        if audio && first_audio_edit > 0 && !vorbis {
                            skip_samples = skip_samples.saturating_add(frame_duration);
                        }
                    }
                }
            } else {
                let corrected = counter
                    .saturating_add(composition)
                    .saturating_add(dts_shift);
                min_corrected_pts = if min_corrected_pts < 0 {
                    corrected
                } else {
                    min_corrected_pts.min(corrected)
                };
                if !started {
                    started = true;
                    rewrite_discarded(&mut entries, counter, &discarded_begin);
                    discarded_begin.clear();
                }
            }
            if entries.len() >= most {
                break;
            }
            entries.push(Entry {
                dts: counter,
                flags,
                ..current
            });
            if started {
                counter = counter.saturating_add(frame_duration);
            }
            position += 1;
            if cts.saturating_add(frame_duration) >= edit_end && (flags & KEY != 0 || audio) {
                if index.has_ctts && !audio && !keyframe_after_edit {
                    keyframe_after_edit = true;
                    continue;
                }
                break;
            }
        }
    }
    min_corrected_pts = min_corrected_pts.saturating_sub(empty_edits);
    if layout.kind == TrackKind::Video && min_corrected_pts > 0 {
        for entry in &mut entries {
            entry.dts = entry.dts.saturating_sub(min_corrected_pts);
        }
    }
    index.duration = index.duration.min(entry_end.saturating_sub(start_dts));
    index.skip_samples = skip_samples;
    index.start_pad = skip_samples;
    index.entries = entries;
}

/// `fix_index_entry_timestamps`: discarded samples before an edit's first shown one end where it
/// starts, each its own duration earlier.
fn rewrite_discarded(entries: &mut [Entry], end: i64, durations: &[i64]) {
    let mut timestamp = end;
    let count = entries.len();
    for (back, duration) in durations.iter().rev().enumerate() {
        timestamp = timestamp.saturating_sub(*duration);
        if let Some(entry) = count
            .checked_sub(back + 1)
            .and_then(|at| entries.get_mut(at))
        {
            entry.dts = timestamp;
        }
    }
}

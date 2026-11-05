//! Movie fragments (`moof`): their `trun` samples appended to each track's index the way
//! `FFmpeg` appends them.

use bytes::Bytes;

use super::boxes::{RawBox, Reader, children};
use super::index::{DISCARD, Entry, Index, KEY, MAX_ENTRIES};
use super::movie::Trex;
use crate::DemuxError;

/// What a fragment needs to know of a track.
pub(crate) struct FragmentTrack<'a> {
    pub track_id: u32,
    /// The sample entry the track reads, or `None` for all (`pseudo_stream_id`).
    pub pseudo_entry: Option<u32>,
    pub index: &'a mut Index,
}

const BASE_DATA_OFFSET: u32 = 0x1;
const STSD_ID: u32 = 0x2;
const DEFAULT_DURATION: u32 = 0x8;
const DEFAULT_SIZE: u32 = 0x10;
const DEFAULT_FLAGS: u32 = 0x20;
const DEFAULT_BASE_IS_MOOF: u32 = 0x2_0000;

const DATA_OFFSET: u32 = 0x1;
const FIRST_SAMPLE_FLAGS: u32 = 0x4;
const SAMPLE_DURATION: u32 = 0x100;
const SAMPLE_SIZE: u32 = 0x200;
const SAMPLE_FLAGS: u32 = 0x400;
const SAMPLE_CTS: u32 = 0x800;

const NON_SYNC: u32 = 0x1_0000;
const DEPENDS_YES: u32 = 0x100_0000;

struct Defaults {
    track: usize,
    base_data_offset: u64,
    stsd_id: u32,
    duration: u32,
    size: u32,
    flags: u32,
}

/// Appends the samples of `moof`, which starts at `moof_offset` in a file of `file_len` bytes.
pub(crate) fn read_moof(
    moof: &RawBox,
    moof_offset: u64,
    trex: &[Trex],
    tracks: &mut [FragmentTrack<'_>],
    file_len: u64,
) -> Result<(), DemuxError> {
    let mut implicit_offset = moof_offset;
    for traf in children(&moof.bytes.slice(moof.header..)) {
        if &traf.kind.0 != b"traf" {
            continue;
        }
        let mut defaults: Option<Defaults> = None;
        let mut tfdt: Option<i64> = None;
        let mut next_trun_dts: Option<i64> = None;
        for child in children(&traf.bytes.slice(traf.header..)) {
            match &child.kind.0 {
                b"tfhd" => {
                    defaults = tfhd(&child, moof_offset, implicit_offset, trex, tracks)?;
                    next_trun_dts = None;
                }
                b"tfdt" => {
                    let mut reader = Reader::new(child.payload(), "tfdt");
                    let (version, _) = reader.version_flags()?;
                    let time = if version == 1 {
                        reader.u64()? as i64
                    } else {
                        i64::from(reader.u32()?)
                    };
                    if let Some(defaults) = &defaults {
                        let track = &tracks[defaults.track];
                        if wanted(track, defaults.stsd_id) {
                            tfdt = Some(time);
                            track_end(tracks, defaults.track, time);
                        }
                    }
                }
                b"trun" => {
                    let Some(defaults) = &defaults else {
                        continue;
                    };
                    if !wanted(&tracks[defaults.track], defaults.stsd_id) {
                        continue;
                    }
                    let end = trun(
                        &child.bytes.slice(child.header..),
                        defaults,
                        tfdt,
                        next_trun_dts,
                        tracks[defaults.track].index,
                        file_len,
                    )?;
                    if let Some((end_dts, end_offset)) = end {
                        next_trun_dts = Some(end_dts);
                        implicit_offset = end_offset;
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn wanted(track: &FragmentTrack<'_>, stsd_id: u32) -> bool {
    track
        .pseudo_entry
        .is_none_or(|entry| entry.wrapping_add(1) == stsd_id)
}

fn track_end(tracks: &mut [FragmentTrack<'_>], track: usize, time: i64) {
    tracks[track].index.track_end = time;
}

fn tfhd(
    tfhd: &RawBox,
    moof_offset: u64,
    implicit_offset: u64,
    trex: &[Trex],
    tracks: &[FragmentTrack<'_>],
) -> Result<Option<Defaults>, DemuxError> {
    let mut reader = Reader::new(tfhd.payload(), "tfhd");
    let (_, flags) = reader.version_flags()?;
    let track_id = reader.u32()?;
    if track_id == 0 {
        return Err(DemuxError::Invalid("a tfhd box names track 0".into()));
    }
    let Some(defaults) = trex.iter().find(|trex| trex.track_id == track_id) else {
        return Ok(None);
    };
    let Some(track) = tracks.iter().position(|track| track.track_id == track_id) else {
        return Ok(None);
    };
    let base_data_offset = if flags & BASE_DATA_OFFSET != 0 {
        reader.u64()?
    } else if flags & DEFAULT_BASE_IS_MOOF != 0 {
        moof_offset
    } else {
        implicit_offset
    };
    let mut field = |flag: u32, fallback: u32| -> Result<u32, DemuxError> {
        if flags & flag != 0 {
            reader.u32()
        } else {
            Ok(fallback)
        }
    };
    Ok(Some(Defaults {
        track,
        base_data_offset,
        stsd_id: field(STSD_ID, defaults.stsd_id)?,
        duration: field(DEFAULT_DURATION, defaults.duration)?,
        size: field(DEFAULT_SIZE, defaults.size)?,
        flags: field(DEFAULT_FLAGS, defaults.flags)?,
    }))
}

/// Appends one `trun`'s samples; returns the decode time and file offset after them.
fn trun(
    payload: &Bytes,
    defaults: &Defaults,
    tfdt: Option<i64>,
    next_trun_dts: Option<i64>,
    index: &mut Index,
    file_len: u64,
) -> Result<Option<(i64, u64)>, DemuxError> {
    let mut reader = Reader::new(payload, "trun");
    let (_, flags) = reader.version_flags()?;
    let entries = reader.u32()?;
    let data_offset = if flags & DATA_OFFSET != 0 {
        i64::from(reader.i32()?)
    } else {
        0
    };
    let first_flags = if flags & FIRST_SAMPLE_FLAGS != 0 {
        reader.u32()?
    } else {
        defaults.flags
    };
    let start = match (next_trun_dts, tfdt) {
        (Some(next), _) => next,
        (None, Some(tfdt)) => tfdt,
        (None, None) => index.track_end,
    };
    let mut dts = start.wrapping_sub(index.time_offset);
    if entries == 0 {
        return Ok(None);
    }
    let per_sample = [SAMPLE_DURATION, SAMPLE_SIZE, SAMPLE_FLAGS, SAMPLE_CTS]
        .iter()
        .filter(|flag| flags & **flag != 0)
        .count()
        * 4;
    let entries = entries as usize;
    if (per_sample > 0 && entries > reader.remaining() / per_sample)
        || entries as u64 > file_len
        || index.entries.len() + entries > MAX_ENTRIES
    {
        return Err(DemuxError::Invalid(
            "a trun box declares more samples than it holds".into(),
        ));
    }
    let mut offset = (defaults.base_data_offset as i64).wrapping_add(data_offset) as u64;
    let previous = index.entries.last().map(|entry| entry.dts);
    index.has_stts = true;
    if flags & SAMPLE_CTS != 0 {
        index.has_ctts = true;
    }
    for sample in 0..entries {
        let duration = if flags & SAMPLE_DURATION != 0 {
            reader.u32()?
        } else {
            defaults.duration
        };
        let size = if flags & SAMPLE_SIZE != 0 {
            reader.u32()?
        } else {
            defaults.size
        };
        let sample_flags = if flags & SAMPLE_FLAGS != 0 {
            reader.u32()?
        } else if sample == 0 {
            first_flags
        } else {
            defaults.flags
        };
        let composition = if flags & SAMPLE_CTS != 0 {
            reader.u32()? as i32
        } else {
            0
        };
        if composition < 0 {
            index.dts_shift = index.dts_shift.max(composition.saturating_neg());
        }
        let mut entry_flags = 0;
        if sample_flags & (NON_SYNC | DEPENDS_YES) == 0 {
            entry_flags |= KEY;
        }
        if previous.is_some_and(|previous| previous >= dts) {
            entry_flags |= DISCARD;
        }
        if size == 0 {
            return Err(DemuxError::Invalid("a fragment sample has no bytes".into()));
        }
        index.entries.push(Entry {
            pos: offset,
            dts,
            size,
            tts: Some((duration, composition)),
            flags: entry_flags,
        });
        dts = dts
            .checked_add(i64::from(duration))
            .ok_or_else(|| DemuxError::Invalid("fragment times overflow".into()))?;
        offset = offset.wrapping_add(u64::from(size));
    }
    let end = dts.wrapping_add(index.time_offset);
    index.track_end = end;
    index.duration = index.duration.max(end);
    Ok(Some((end, offset)))
}

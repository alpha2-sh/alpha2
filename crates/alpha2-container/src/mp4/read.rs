//! The MP4/MOV/M4A demuxer.

use std::io::{Read, Seek, SeekFrom};

use alpha2_time::FrameRate;
use bytes::Bytes;

use super::boxes::RawBox;
use super::describe::{self, Family};
use super::fragment::{self, FragmentTrack};
use super::index::{self, Index, KEY};
use super::movie::{self, Brands};
use crate::{ContainerReader, DemuxError, FourCc, Packet, Source, TrackId, TrackInfo, TrackKind};

/// The largest `moov` or `moof` box read into memory.
const MAX_HEADER_BOX: u64 = 256 << 20;

/// Reads MP4, MOV and M4A files: their tracks and each track's packets, timed as `FFmpeg`'s MOV
/// demuxer times them.
pub struct Mp4Reader {
    source: Box<dyn Source>,
    file_len: u64,
    tracks: Vec<TrackInfo>,
    states: Vec<TrackState>,
}

struct TrackState {
    index: Index,
    family: Family,
    kind: TrackKind,
    sample_rate: u32,
    timescale: u32,
    next: usize,
    /// Skip-samples for the next packet delivered.
    pending_skip: u32,
}

impl std::fmt::Debug for Mp4Reader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Mp4Reader")
            .field("tracks", &self.tracks)
            .finish_non_exhaustive()
    }
}

/// A top-level box header: type, offset of the box and of its payload, and its clamped size.
struct TopBox {
    kind: FourCc,
    at: u64,
    header: u64,
    size: u64,
}

fn read_top_box(
    source: &mut dyn Source,
    at: u64,
    file_len: u64,
) -> Result<Option<TopBox>, DemuxError> {
    if file_len.saturating_sub(at) < 8 {
        return Ok(None);
    }
    source.seek(SeekFrom::Start(at))?;
    let mut head = [0; 16];
    source.read_exact(&mut head[..8])?;
    let declared = u64::from(u32::from_be_bytes([head[0], head[1], head[2], head[3]]));
    let kind = FourCc([head[4], head[5], head[6], head[7]]);
    let remaining = file_len - at;
    let (size, header) = match declared {
        0 => (remaining, 8),
        1 => {
            if remaining < 16 {
                return Ok(None);
            }
            source.read_exact(&mut head[8..16])?;
            let mut wide = [0; 8];
            wide.copy_from_slice(&head[8..16]);
            (u64::from_be_bytes(wide), 16)
        }
        size => (size, 8),
    };
    if size < header {
        return Ok(None);
    }
    Ok(Some(TopBox {
        kind,
        at,
        header,
        size: size.min(remaining),
    }))
}

fn load(source: &mut dyn Source, top: &TopBox) -> Result<RawBox, DemuxError> {
    if top.size > MAX_HEADER_BOX {
        return Err(DemuxError::Invalid(format!(
            "the {} box is larger than this layer reads",
            top.kind
        )));
    }
    source.seek(SeekFrom::Start(top.at))?;
    let mut bytes = vec![0; usize::try_from(top.size).unwrap_or(usize::MAX)];
    source.read_exact(&mut bytes)?;
    Ok(RawBox {
        kind: top.kind,
        header: usize::try_from(top.header).unwrap_or(8),
        bytes: Bytes::from(bytes),
    })
}

/// The `ftyp` brands, `moov`, and where every `moof` is.
type TopLevel = (Brands, RawBox, Vec<TopBox>);

/// The top-level boxes the reader keeps.
fn scan(source: &mut dyn Source, file_len: u64) -> Result<TopLevel, DemuxError> {
    let mut brands = Brands::default();
    let mut moov = None;
    let mut moofs = Vec::new();
    let mut at = 0;
    while let Some(top) = read_top_box(source, at, file_len)? {
        match &top.kind.0 {
            b"ftyp" if moov.is_none() => brands = Brands::of_ftyp(load(source, &top)?.payload()),
            b"moov" if moov.is_none() => moov = Some(load(source, &top)?),
            _ => {}
        }
        at = top.at + top.size;
        if &top.kind.0 == b"moof" {
            moofs.push(top);
        }
    }
    let moov = moov.ok_or_else(|| DemuxError::Invalid("the file has no moov box".into()))?;
    Ok((brands, moov, moofs))
}

impl Mp4Reader {
    /// Opens the MP4 file at `path`.
    ///
    /// # Errors
    /// A file that cannot be read, has no `moov`, or breaks this layer's limits.
    pub fn open(path: &std::path::Path) -> Result<Self, DemuxError> {
        Self::new(std::fs::File::open(path)?)
    }

    /// Reads an MP4 file from `source`.
    ///
    /// # Errors
    /// A source that cannot be read, has no `moov`, or breaks this layer's limits.
    pub fn new<R: Read + Seek + Send + 'static>(source: R) -> Result<Self, DemuxError> {
        let mut source: Box<dyn Source> = Box::new(source);
        let file_len = source.seek(SeekFrom::End(0))?;
        let (brands, moov, fragments) = scan(source.as_mut(), file_len)?;
        if movie::has_compressed_header(&moov) {
            return Err(DemuxError::Unsupported(
                "compressed QuickTime headers are not read".into(),
            ));
        }
        let movie = movie::parse_moov(&moov.bytes.slice(moov.header..), brands)?;
        let mut context = index::Context {
            movie_timescale: movie.timescale,
            advanced: true,
            file_len,
        };
        let mut tracks = Vec::with_capacity(movie.tracks.len());
        let mut states = Vec::with_capacity(movie.tracks.len());
        for trak in &movie.tracks {
            let mut trak = trak.clone();
            if trak.timescale == 0 {
                trak.timescale = movie.timescale.max(1);
            }
            let described = describe::describe(&trak)?;
            let layout = index::Layout {
                kind: described.kind,
                family: described.family,
                pcm_frame_bytes: described.pcm_frame_bytes,
                audio: &described.audio_fields,
            };
            let built = index::build(&trak, &layout, &mut context)?;
            tracks.push(TrackInfo {
                id: TrackId(trak.track_id),
                kind: described.kind,
                timescale: trak.timescale,
                duration: None,
                language: describe::language(trak.language),
                title: trak.title.clone(),
                default: trak.enabled,
                enabled: trak.enabled,
                video: described.video,
                audio: described.audio,
            });
            states.push(TrackState {
                index: built,
                family: described.family,
                kind: described.kind,
                sample_rate: 0,
                timescale: trak.timescale,
                next: 0,
                pending_skip: 0,
            });
        }
        if let Some((track, priming)) = movie.smpb
            && let Some(state) = states.get_mut(track)
        {
            state.index.start_pad = i64::from(priming);
        }
        for top in &fragments {
            let fragment_box = load(source.as_mut(), top)?;
            let mut views = states
                .iter_mut()
                .zip(&movie.tracks)
                .map(|(state, trak)| FragmentTrack {
                    track_id: trak.track_id,
                    pseudo_entry: trak.pseudo_entry,
                    index: &mut state.index,
                })
                .collect::<Vec<_>>();
            fragment::read_moof(&fragment_box, top.at, &movie.trex, &mut views, file_len)?;
        }
        for (track, state) in tracks.iter_mut().zip(&mut states) {
            if state.family == Family::Aac {
                state.index.skip_samples = state.index.start_pad;
            }
            let skip = u32::try_from(state.index.skip_samples.max(0)).unwrap_or(u32::MAX);
            state.pending_skip = skip;
            track.duration = (state.index.duration > 0).then_some(state.index.duration);
            if let Some(audio) = &mut track.audio {
                audio.priming = skip;
                state.sample_rate = audio.sample_rate;
            }
            if let Some(video) = &mut track.video {
                video.frame_rate = frame_rate(&state.index, state.timescale);
            }
        }
        Ok(Self {
            source,
            file_len,
            tracks,
            states,
        })
    }

    fn state(&mut self, track: TrackId) -> Result<usize, DemuxError> {
        self.tracks
            .iter()
            .position(|info| info.id == track)
            .ok_or(DemuxError::NoSuchTrack(track.0))
    }
}

/// The rate of a track whose samples all last alike but for the last, as `FFmpeg`'s
/// `r_frame_rate` from `stts`.
fn frame_rate(index: &Index, timescale: u32) -> Option<FrameRate> {
    if !index.has_stts {
        return None;
    }
    let mut durations = index.entries.iter().map(|entry| entry.tts.map(|tts| tts.0));
    let first = durations.next()??;
    let count = index.entries.len();
    let constant = index
        .entries
        .iter()
        .take(count.saturating_sub(1))
        .all(|entry| entry.tts.map(|tts| tts.0) == Some(first));
    (constant && first > 0)
        .then(|| FrameRate::new(timescale, first).ok())
        .flatten()
}

/// The frame length of an E-AC-3 syncframe, from its `numblkscod`.
fn eac3_samples(data: &[u8]) -> Option<i64> {
    if data.get(..2)? != [0x0b, 0x77] {
        return None;
    }
    let byte = *data.get(4)?;
    let blocks = if byte >> 6 == 3 {
        6
    } else {
        [1, 2, 3, 6][usize::from((byte >> 4) & 3)]
    };
    Some(256 * blocks)
}

impl TrackState {
    /// The packet times of entry `at`, as `FFmpeg` hands them out.
    fn times(&self, at: usize, data: &[u8]) -> (i64, i64, i64) {
        let index = &self.index;
        let entry = index.entries[at];
        let dts = entry.dts;
        let mut duration = match entry.tts {
            Some((delta, _)) if index.has_stts => i64::from(delta),
            _ => 0,
        };
        let pts = match entry.tts {
            Some((_, offset)) if index.has_ctts => dts
                .saturating_add(i64::from(index.dts_shift))
                .saturating_add(i64::from(offset)),
            _ => {
                if duration == 0 {
                    let next = index
                        .entries
                        .get(at + 1)
                        .map_or(index.duration, |next| next.dts);
                    if next >= dts {
                        duration = next.saturating_sub(dts);
                    }
                }
                dts
            }
        };
        match self.family {
            Family::Ac3 => duration = 1536,
            Family::Eac3 => {
                if let Some(samples) = eac3_samples(data) {
                    duration = index::rescale(
                        samples,
                        i64::from(self.timescale),
                        i64::from(self.sample_rate.max(1)),
                    );
                }
            }
            _ => {}
        }
        if duration <= 0 {
            duration = match self.family {
                Family::Pcm(frame) if frame > 0 => {
                    i64::try_from(data.len()).unwrap_or(0) / i64::from(frame)
                }
                _ => duration.max(0),
            };
        }
        (pts, dts, duration)
    }

    fn keyframe(&self, at: usize) -> bool {
        self.index.entries[at].flags & KEY != 0
            || matches!(
                self.family,
                Family::IntraOnly | Family::Pcm(_) | Family::Ac3 | Family::Eac3
            )
            || self.kind == TrackKind::Other
    }

    /// Skip-samples after seeking to entry `at`: what is left of the priming.
    fn skip_after_seek(&self, at: usize) -> u32 {
        if self.kind != TrackKind::Audio || self.sample_rate == 0 {
            return 0;
        }
        let entries = &self.index.entries;
        let elapsed = index::rescale(
            entries[at].dts.saturating_sub(entries[0].dts),
            i64::from(self.sample_rate),
            i64::from(self.timescale),
        );
        u32::try_from(self.index.start_pad.saturating_sub(elapsed).max(0)).unwrap_or(u32::MAX)
    }
}

impl ContainerReader for Mp4Reader {
    fn tracks(&self) -> &[TrackInfo] {
        &self.tracks
    }

    fn seek(&mut self, track: TrackId, pts: i64) -> Result<Option<i64>, DemuxError> {
        let position = self.state(track)?;
        let state = &mut self.states[position];
        if state.index.entries.is_empty() {
            state.next = 0;
            return Ok(None);
        }
        let chosen = (0..state.index.entries.len())
            .rev()
            .find(|at| state.keyframe(*at) && state.times(*at, &[]).0 <= pts)
            .unwrap_or(0);
        state.next = chosen;
        state.pending_skip = state.skip_after_seek(chosen);
        Ok(Some(state.times(chosen, &[]).0))
    }

    fn next_packet(&mut self, track: TrackId) -> Result<Option<Packet>, DemuxError> {
        let position = self.state(track)?;
        let state = &self.states[position];
        let at = state.next;
        let Some(entry) = state.index.entries.get(at).copied() else {
            return Ok(None);
        };
        let size = u64::from(entry.size);
        let available = self.file_len.saturating_sub(entry.pos).min(size);
        if available == 0 && size > 0 {
            self.states[position].next = state.index.entries.len();
            return Ok(None);
        }
        let mut data = vec![0; usize::try_from(available).unwrap_or(0)];
        self.source.seek(SeekFrom::Start(entry.pos))?;
        self.source.read_exact(&mut data)?;
        let state = &mut self.states[position];
        let (pts, dts, duration) = state.times(at, &data);
        let keyframe = state.keyframe(at);
        state.next = at + 1;
        let skip = std::mem::take(&mut state.pending_skip);
        Ok(Some(Packet {
            track,
            pts,
            dts,
            duration,
            keyframe,
            data: Bytes::from(data),
            discard: (skip, 0),
        }))
    }
}

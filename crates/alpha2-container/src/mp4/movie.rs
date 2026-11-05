//! The `moov` and `moof` boxes, parsed into typed fields. Boxes this layer does not interpret are
//! kept byte for byte beside the fields of the box that holds them.

use bytes::Bytes;

use super::boxes::{RawBox, Reader, children, find};
use crate::{DemuxError, FourCc};

/// A 3x3 display matrix: 16.16 fixed point, the last column 2.30.
pub(crate) type Matrix = [[i32; 3]; 3];

pub(crate) const IDENTITY: Matrix = [[1 << 16, 0, 0], [0, 1 << 16, 0], [0, 0, 1 << 30]];

/// What `ftyp` says about how to read QuickTime sound descriptions.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Brands {
    /// The major brand is not `qt  `.
    pub isom: bool,
    /// A compatible brand is `qt  `.
    pub quicktime_compatible: bool,
}

impl Brands {
    pub fn of_ftyp(payload: &[u8]) -> Self {
        let major = payload.get(..4);
        let compatible = payload
            .get(8..)
            .unwrap_or_default()
            .chunks_exact(4)
            .any(|brand| brand == b"qt  ");
        Self {
            isom: major.is_some_and(|major| major != b"qt  "),
            quicktime_compatible: compatible,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Movie {
    /// `mvhd` timescale; 0 without one, 1 for an invalid one.
    pub timescale: u32,
    pub matrix: Matrix,
    pub tracks: Vec<Trak>,
    pub trex: Vec<Trex>,
    /// `iTunSMPB` priming and the index of the track parsed before it.
    pub smpb: Option<(usize, u32)>,
    pub other: Vec<RawBox>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Trex {
    pub track_id: u32,
    pub stsd_id: u32,
    pub duration: u32,
    pub size: u32,
    pub flags: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Edit {
    /// In the movie timescale.
    pub duration: u64,
    /// In the media timescale; -1 for an empty edit.
    pub media_time: i64,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Trak {
    pub track_id: u32,
    pub enabled: bool,
    pub matrix: Matrix,
    pub timescale: u32,
    pub media_duration: u64,
    pub language: u16,
    pub handler: [u8; 4],
    pub title: Option<String>,
    pub edits: Vec<Edit>,
    pub stsd_version: u8,
    pub entries: Vec<SampleEntry>,
    /// Index of the sample entry whose packets the track delivers, or `None` for every entry
    /// (as `FFmpeg`'s `pseudo_stream_id`).
    pub pseudo_entry: Option<u32>,
    pub tables: Tables,
    /// Uninterpreted boxes of `trak`, `mdia`, `minf` and `stbl`, in that order of nesting.
    pub other: Vec<RawBox>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Tables {
    /// `stts` entries: sample count and delta.
    pub stts: Vec<(u32, u32)>,
    /// `ctts` entries with a positive count: count and signed offset.
    pub ctts: Vec<(u32, i32)>,
    /// The largest negative composition offset, negated (`FFmpeg`'s `dts_shift`).
    pub dts_shift: i32,
    /// `stss` sample numbers; `Some` and empty when the box lists none.
    pub stss: Option<Vec<u32>>,
    pub stps: Vec<u32>,
    pub stsc: Vec<Stsc>,
    /// `stsz` constant size, 0 when sizes are listed.
    pub constant_size: u32,
    pub sample_count: u32,
    pub sizes: Vec<u32>,
    pub chunks: Vec<u64>,
    /// `sbgp` `rap ` groups: count and description index.
    pub rap_groups: Vec<(u32, u32)>,
    /// `sbgp` `sync` groups: count and description index.
    pub sync_groups: Vec<(u32, u32)>,
    /// `sgpd` `sync` NAL unit types.
    pub sync_descriptions: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Stsc {
    pub first: u32,
    pub count: u32,
    pub id: u32,
}

/// One `stsd` entry.
#[derive(Clone, Debug)]
pub(crate) struct SampleEntry {
    pub format: FourCc,
    pub fields: EntryFields,
    /// The child boxes after the fixed fields (`avcC`, `esds`, `colr`, `wave`, ...).
    pub children: Vec<RawBox>,
    /// The whole entry as stored.
    pub raw: RawBox,
}

#[derive(Clone, Debug)]
pub(crate) enum EntryFields {
    Video { width: u16, height: u16, depth: u16 },
    Audio(AudioFields),
    Other,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct AudioFields {
    pub version: u16,
    pub channels: u32,
    pub sample_size: u32,
    pub compression_id: i16,
    pub sample_rate: f64,
    pub samples_per_frame: u32,
    pub bytes_per_frame: u32,
    /// The `lpcm` format flags of a version 2 description.
    pub lpcm_flags: Option<u32>,
}

/// Parses `moov`.
pub(crate) fn parse_moov(data: &Bytes, brands: Brands) -> Result<Movie, DemuxError> {
    let mut movie = Movie {
        matrix: IDENTITY,
        ..Movie::default()
    };
    for child in children(data) {
        match &child.kind.0 {
            b"mvhd" => parse_mvhd(&child, &mut movie)?,
            b"trak" => {
                let trak = parse_trak(&child, brands, movie.matrix)?;
                movie.tracks.push(trak);
            }
            b"mvex" => {
                for entry in children(&child.bytes.slice(child.header..)) {
                    if &entry.kind.0 == b"trex" {
                        movie.trex.push(parse_trex(&entry)?);
                    } else {
                        movie.other.push(entry);
                    }
                }
            }
            b"udta" => {
                if let Some(priming) = udta_smpb(&child) {
                    if let Some(last) = movie.tracks.len().checked_sub(1) {
                        movie.smpb = Some((last, priming));
                    }
                }
                movie.other.push(child);
            }
            _ => movie.other.push(child),
        }
    }
    Ok(movie)
}

fn parse_mvhd(mvhd: &RawBox, movie: &mut Movie) -> Result<(), DemuxError> {
    let mut reader = Reader::new(mvhd.payload(), "mvhd");
    let (version, _) = reader.version_flags()?;
    reader.skip(if version == 1 { 16 } else { 8 })?;
    let timescale = reader.u32()?;
    movie.timescale = if timescale == 0 || timescale > i32::MAX as u32 {
        1
    } else {
        timescale
    };
    reader.skip(if version == 1 { 8 } else { 4 })?;
    reader.skip(4 + 2 + 10)?;
    movie.matrix = read_matrix(&mut reader)?;
    Ok(())
}

fn read_matrix(reader: &mut Reader<'_>) -> Result<Matrix, DemuxError> {
    let mut matrix = [[0; 3]; 3];
    for row in &mut matrix {
        for value in row {
            *value = reader.i32()?;
        }
    }
    Ok(matrix)
}

fn parse_trex(trex: &RawBox) -> Result<Trex, DemuxError> {
    let mut reader = Reader::new(trex.payload(), "trex");
    reader.version_flags()?;
    Ok(Trex {
        track_id: reader.u32()?,
        stsd_id: reader.u32()?,
        duration: reader.u32()?,
        size: reader.u32()?,
        flags: reader.u32()?,
    })
}

fn body(raw: &RawBox) -> Bytes {
    raw.bytes.slice(raw.header..)
}

fn parse_trak(trak_box: &RawBox, brands: Brands, movie_matrix: Matrix) -> Result<Trak, DemuxError> {
    let mut trak = Trak {
        matrix: IDENTITY,
        ..Trak::default()
    };
    let mut tkhd_seen = false;
    for child in children(&body(trak_box)) {
        match &child.kind.0 {
            b"tkhd" if !tkhd_seen => {
                tkhd_seen = true;
                parse_tkhd(&child, &mut trak, movie_matrix)?;
            }
            b"tkhd" => {
                return Err(DemuxError::Invalid("a track has two tkhd boxes".into()));
            }
            b"edts" => {
                for edit in children(&body(&child)) {
                    if &edit.kind.0 == b"elst" {
                        trak.edits = parse_elst(&edit)?;
                    }
                }
                trak.other.push(child);
            }
            b"mdia" => parse_mdia(&child, &mut trak, brands)?,
            b"udta" => {
                trak.title = udta_title(&child);
                trak.other.push(child);
            }
            _ => trak.other.push(child),
        }
    }
    Ok(trak)
}

fn parse_tkhd(tkhd: &RawBox, trak: &mut Trak, movie_matrix: Matrix) -> Result<(), DemuxError> {
    let mut reader = Reader::new(tkhd.payload(), "tkhd");
    let (version, flags) = reader.version_flags()?;
    trak.enabled = flags & 1 != 0;
    reader.skip(if version == 1 { 16 } else { 8 })?;
    trak.track_id = reader.u32()?;
    reader.skip(4)?;
    reader.skip(if version == 1 { 8 } else { 4 })?;
    reader.skip(8 + 8)?;
    let own = read_matrix(&mut reader)?;
    // The movie matrix applies after the track's, in the fixed-point arithmetic of `FFmpeg`.
    let shifts = [16, 16, 30];
    let mut combined = [[0_i64; 3]; 3];
    for (row, own_row) in combined.iter_mut().zip(own) {
        for (column, value) in row.iter_mut().enumerate() {
            for (inner, shift) in shifts.iter().enumerate() {
                *value +=
                    (i64::from(own_row[inner]) * i64::from(movie_matrix[inner][column])) >> shift;
            }
        }
    }
    trak.matrix = combined.map(|row| row.map(|value| value as i32));
    Ok(())
}

fn parse_elst(elst: &RawBox) -> Result<Vec<Edit>, DemuxError> {
    let mut reader = Reader::new(elst.payload(), "elst");
    let (version, _) = reader.version_flags()?;
    let declared = reader.u32()? as usize;
    let entry = if version == 1 { 20 } else { 12 };
    let count = declared.min(reader.remaining() / entry);
    let mut edits = Vec::with_capacity(count);
    for _ in 0..count {
        let (duration, media_time) = if version == 1 {
            (reader.u64()?, reader.u64()? as i64)
        } else {
            (u64::from(reader.u32()?), i64::from(reader.i32()?))
        };
        reader.skip(4)?;
        edits.push(Edit {
            duration,
            media_time,
        });
    }
    Ok(edits)
}

fn parse_mdia(mdia: &RawBox, trak: &mut Trak, brands: Brands) -> Result<(), DemuxError> {
    let mut stbl = None;
    for child in children(&body(mdia)) {
        match &child.kind.0 {
            b"mdhd" => {
                let mut reader = Reader::new(child.payload(), "mdhd");
                let (version, _) = reader.version_flags()?;
                if version > 1 {
                    return Err(DemuxError::Unsupported(format!(
                        "mdhd version {version} is not read"
                    )));
                }
                reader.skip(if version == 1 { 16 } else { 8 })?;
                trak.timescale = reader.u32()?;
                if trak.timescale == 0 || trak.timescale > i32::MAX as u32 {
                    trak.timescale = 1;
                }
                trak.media_duration = if version == 1 {
                    match reader.u64()? {
                        u64::MAX => 0,
                        duration => duration,
                    }
                } else {
                    match reader.u32()? {
                        u32::MAX => 0,
                        duration => u64::from(duration),
                    }
                };
                trak.language = reader.u16()?;
            }
            b"hdlr" => {
                if let Some(handler) = child.payload().get(8..12) {
                    trak.handler.copy_from_slice(handler);
                }
                trak.other.push(child);
            }
            b"minf" => {
                for leaf in children(&body(&child)) {
                    if &leaf.kind.0 == b"stbl" {
                        stbl = Some(leaf);
                    } else {
                        trak.other.push(leaf);
                    }
                }
            }
            _ => trak.other.push(child),
        }
    }
    if let Some(stbl) = stbl {
        parse_stbl(&stbl, trak, brands)?;
    }
    Ok(())
}

fn parse_stbl(stbl: &RawBox, trak: &mut Trak, brands: Brands) -> Result<(), DemuxError> {
    for child in children(&body(stbl)) {
        let payload = child.payload();
        let tables = &mut trak.tables;
        match &child.kind.0 {
            b"stsd" => parse_stsd(&child, trak, brands)?,
            b"stts" => tables.stts = pairs(payload, "stts")?,
            b"ctts" => {
                let entries = pairs(payload, "ctts")?;
                let total = entries.len();
                for (index, (count, offset)) in entries.into_iter().enumerate() {
                    let offset = offset as i32;
                    if count == 0 || count > i32::MAX as u32 {
                        continue;
                    }
                    tables.ctts.push((count, offset));
                    // `FFmpeg` ignores the last two entries when it shifts decode times.
                    if index + 2 < total && offset < 0 {
                        tables.dts_shift =
                            tables.dts_shift.max(offset.saturating_neg().max(-i32::MAX));
                    }
                }
            }
            b"stss" => tables.stss = Some(words(payload, "stss")?),
            b"stps" => tables.stps = words(payload, "stps")?,
            b"stsc" => tables.stsc = parse_stsc(payload)?,
            b"stsz" | b"stz2" => parse_stsz(&child, tables)?,
            b"stco" | b"co64" => {
                if tables.chunks.is_empty() {
                    tables.chunks = parse_stco(&child)?;
                }
            }
            b"sbgp" => parse_sbgp(payload, tables)?,
            b"sgpd" => parse_sgpd(payload, tables)?,
            _ => trak.other.push(child),
        }
    }
    Ok(())
}

/// A full box's entry count, capped at what its payload holds at `size` bytes an entry.
fn entry_count(reader: &mut Reader<'_>, size: usize) -> Result<usize, DemuxError> {
    let declared = reader.u32()? as usize;
    Ok(declared.min(reader.remaining() / size))
}

fn pairs(payload: &[u8], what: &'static str) -> Result<Vec<(u32, u32)>, DemuxError> {
    let mut reader = Reader::new(payload, what);
    reader.version_flags()?;
    let count = entry_count(&mut reader, 8)?;
    (0..count)
        .map(|_| Ok((reader.u32()?, reader.u32()?)))
        .collect()
}

fn words(payload: &[u8], what: &'static str) -> Result<Vec<u32>, DemuxError> {
    let mut reader = Reader::new(payload, what);
    reader.version_flags()?;
    let count = entry_count(&mut reader, 4)?;
    (0..count).map(|_| reader.u32()).collect()
}

/// `stsc` entries, repaired the way `FFmpeg` repairs out-of-order or zero entries.
fn parse_stsc(payload: &[u8]) -> Result<Vec<Stsc>, DemuxError> {
    let mut reader = Reader::new(payload, "stsc");
    reader.version_flags()?;
    let declared = reader.u32()? as usize;
    if declared.saturating_mul(12) > reader.remaining() {
        return Err(DemuxError::Invalid("the stsc box is truncated".into()));
    }
    let mut entries = (0..declared)
        .map(|_| {
            Ok(Stsc {
                first: reader.u32()?,
                count: reader.u32()?,
                id: reader.u32()?,
            })
        })
        .collect::<Result<Vec<_>, DemuxError>>()?;
    let mut index = entries.len();
    while index > 0 {
        index -= 1;
        let first_min = index as u64 + 1;
        let entry = entries[index];
        let next = entries.get(index + 1).copied();
        let previous = index.checked_sub(1).map(|at| entries[at]);
        let invalid = next.is_some_and(|next| entry.first >= next.first)
            || previous.is_some_and(|previous| entry.first <= previous.first)
            || u64::from(entry.first) < first_min
            || entry.count < 1
            || entry.id < 1;
        if !invalid {
            continue;
        }
        match next {
            None => {
                if entry.count == 0 && index > 0 {
                    entries.pop();
                    continue;
                }
                let mut fixed = entry;
                fixed.first = fixed
                    .first
                    .max(u32::try_from(first_min).unwrap_or(u32::MAX));
                if let Some(previous) = previous {
                    if fixed.first <= previous.first {
                        fixed.first = previous.first.saturating_add(1).min(i32::MAX as u32);
                    }
                }
                fixed.count = fixed.count.max(1);
                fixed.id = fixed.id.max(1);
                entries[index] = fixed;
            }
            Some(next) => {
                entries[index] = Stsc {
                    first: next.first.saturating_sub(1).max(1),
                    count: next.count,
                    id: next.id,
                };
            }
        }
    }
    Ok(entries)
}

fn parse_stsz(stsz: &RawBox, tables: &mut Tables) -> Result<(), DemuxError> {
    let mut reader = Reader::new(stsz.payload(), "stsz");
    reader.version_flags()?;
    let field = if &stsz.kind.0 == b"stsz" {
        tables.constant_size = reader.u32()?;
        32
    } else {
        reader.skip(3)?;
        u32::from(reader.u8()?)
    };
    let count = reader.u32()?;
    tables.sample_count = count;
    if tables.constant_size != 0 {
        return Ok(());
    }
    if ![4, 8, 16, 32].contains(&field) {
        return Err(DemuxError::Invalid(format!(
            "the sample size field is {field} bits wide"
        )));
    }
    let available = (reader.remaining() as u64 * 8) / u64::from(field);
    if u64::from(count) > available {
        // `FFmpeg` drops a truncated size table, which leaves the track without samples.
        tables.sample_count = 0;
        return Ok(());
    }
    let data = reader.rest();
    tables.sizes = (0..count as usize)
        .map(|index| match field {
            32 => u32::from_be_bytes([
                data[index * 4],
                data[index * 4 + 1],
                data[index * 4 + 2],
                data[index * 4 + 3],
            ]),
            16 => u32::from(u16::from_be_bytes([data[index * 2], data[index * 2 + 1]])),
            8 => u32::from(data[index]),
            _ => u32::from(if index % 2 == 0 {
                data[index / 2] >> 4
            } else {
                data[index / 2] & 0x0f
            }),
        })
        .collect();
    Ok(())
}

fn parse_stco(stco: &RawBox) -> Result<Vec<u64>, DemuxError> {
    let mut reader = Reader::new(stco.payload(), "stco");
    reader.version_flags()?;
    let wide = &stco.kind.0 == b"co64";
    let count = entry_count(&mut reader, if wide { 8 } else { 4 })?;
    (0..count)
        .map(|_| {
            if wide {
                let offset = reader.u64()?;
                Ok(if offset > i64::MAX as u64 { 0 } else { offset })
            } else {
                reader.u32().map(u64::from)
            }
        })
        .collect()
}

fn parse_sbgp(payload: &[u8], tables: &mut Tables) -> Result<(), DemuxError> {
    let mut reader = Reader::new(payload, "sbgp");
    let (version, _) = reader.version_flags()?;
    let grouping = reader.fourcc()?;
    let target = match &grouping.0 {
        b"rap " => &mut tables.rap_groups,
        b"sync" => &mut tables.sync_groups,
        _ => return Ok(()),
    };
    if version == 1 {
        reader.skip(4)?;
    }
    let count = entry_count(&mut reader, 8)?;
    if count == 0 {
        return Ok(());
    }
    *target = (0..count)
        .map(|_| Ok((reader.u32()?, reader.u32()?)))
        .collect::<Result<_, DemuxError>>()?;
    Ok(())
}

fn parse_sgpd(payload: &[u8], tables: &mut Tables) -> Result<(), DemuxError> {
    let mut reader = Reader::new(payload, "sgpd");
    let (version, _) = reader.version_flags()?;
    if &reader.fourcc()?.0 != b"sync" {
        return Ok(());
    }
    let default_length = if version >= 1 { reader.u32()? } else { 0 };
    if version >= 2 {
        reader.skip(4)?;
    }
    let count = entry_count(&mut reader, 1)?;
    let mut types = Vec::with_capacity(count);
    for _ in 0..count {
        let length = if version >= 1 && default_length == 0 {
            reader.u32()?
        } else {
            default_length
        };
        types.push(reader.u8()? & 0x3f);
        reader.skip((length as usize).saturating_sub(1))?;
    }
    tables.sync_descriptions = types;
    Ok(())
}

/// Video sample entries' fixed fields after the 8-byte header: 78 bytes.
const VIDEO_FIELDS: usize = 78;
/// Sound sample entries' version 0 fields: 28 bytes.
const AUDIO_FIELDS: usize = 28;

fn parse_stsd(stsd: &RawBox, trak: &mut Trak, brands: Brands) -> Result<(), DemuxError> {
    if !trak.entries.is_empty() {
        return Err(DemuxError::Invalid("a track has two stsd boxes".into()));
    }
    let mut reader = Reader::new(stsd.payload(), "stsd");
    let (version, _) = reader.version_flags()?;
    trak.stsd_version = version;
    let declared = reader.u32()?;
    if declared == 0 || declared > 1024 || declared as usize > stsd.payload().len() / 8 {
        return Err(DemuxError::Invalid(format!(
            "the stsd box declares {declared} entries"
        )));
    }
    let entries = children(&stsd.bytes.slice(stsd.header + 8..));
    let mut first_format: Option<FourCc> = None;
    for (index, raw) in entries.into_iter().take(declared as usize).enumerate() {
        if raw.bytes.len() < 16 {
            return Err(DemuxError::Invalid("an stsd entry is truncated".into()));
        }
        let format = raw.kind;
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        if let Some(first) = first_format {
            if !same_codec(first, format) {
                // `FFmpeg` reads one codec per track and skips entries of another.
                trak.entries.push(SampleEntry {
                    format,
                    fields: EntryFields::Other,
                    children: Vec::new(),
                    raw,
                });
                continue;
            }
            trak.pseudo_entry = None;
        } else {
            first_format = Some(format);
            trak.pseudo_entry = Some(index);
        }
        trak.entries
            .push(parse_entry(raw, trak.handler, version, brands)?);
    }
    Ok(())
}

fn same_codec(first: FourCc, format: FourCc) -> bool {
    first == format
        || matches!(&first.0, b"apcn" | b"apch" | b"dvpp" | b"dvcp")
        || (&first.0 == b"AV1x" && &format.0 == b"AVup")
        || &first.0 == b"jpeg"
}

fn parse_entry(
    raw: RawBox,
    handler: [u8; 4],
    stsd_version: u8,
    brands: Brands,
) -> Result<SampleEntry, DemuxError> {
    let payload = raw.payload();
    let mut reader = Reader::new(payload, "sample entry");
    reader.skip(8)?;
    let (fields, fields_end) = match &handler {
        b"vide" if payload.len() >= VIDEO_FIELDS => {
            reader.skip(16)?;
            let width = reader.u16()?;
            let height = reader.u16()?;
            reader.skip(14 + 32)?;
            let depth = reader.u16()?;
            (
                EntryFields::Video {
                    width,
                    height,
                    depth,
                },
                VIDEO_FIELDS,
            )
        }
        b"soun" if payload.len() >= AUDIO_FIELDS => {
            let (fields, end) = parse_audio_fields(&mut reader, stsd_version, brands)?;
            (EntryFields::Audio(fields), end)
        }
        _ => (EntryFields::Other, payload.len()),
    };
    let children = if fields_end + 8 < payload.len() {
        children(&raw.bytes.slice(raw.header + fields_end..))
    } else {
        Vec::new()
    };
    Ok(SampleEntry {
        format: raw.kind,
        fields,
        children,
        raw,
    })
}

fn parse_audio_fields(
    reader: &mut Reader<'_>,
    stsd_version: u8,
    brands: Brands,
) -> Result<(AudioFields, usize), DemuxError> {
    let version = reader.u16()?;
    reader.skip(2 + 4)?;
    let mut fields = AudioFields {
        version,
        channels: u32::from(reader.u16()?),
        sample_size: u32::from(reader.u16()?),
        compression_id: reader.u16()? as i16,
        ..AudioFields::default()
    };
    reader.skip(2)?;
    fields.sample_rate = f64::from(reader.u32()? >> 16);
    let quicktime =
        !brands.isom || brands.quicktime_compatible || (stsd_version == 0 && version > 0);
    if quicktime && version == 1 {
        fields.samples_per_frame = reader.u32()?;
        reader.skip(4)?;
        fields.bytes_per_frame = reader.u32()?;
        reader.skip(4)?;
    } else if quicktime && version == 2 {
        reader.skip(4)?;
        fields.sample_rate = f64::from_bits(reader.u64()?);
        fields.channels = reader.u32()?;
        reader.skip(4)?;
        fields.sample_size = reader.u32()?;
        fields.lpcm_flags = Some(reader.u32()?);
        fields.bytes_per_frame = reader.u32()?;
        fields.samples_per_frame = reader.u32()?;
    }
    Ok((fields, reader.position()))
}

/// The `iTunSMPB` priming in a `udta` box's iTunes metadata, as `FFmpeg` accepts it.
fn udta_smpb(udta: &RawBox) -> Option<u32> {
    let meta = children(&body(udta))
        .into_iter()
        .find(|child| &child.kind.0 == b"meta")?;
    let payload = body(&meta);
    let start = if payload.get(4..8) == Some(b"hdlr") {
        0
    } else {
        4
    };
    let ilst = children(&payload.slice(start.min(payload.len())..))
        .into_iter()
        .find(|child| &child.kind.0 == b"ilst")?;
    for item in children(&body(&ilst)) {
        if &item.kind.0 != b"----" {
            continue;
        }
        let parts = children(&body(&item));
        let name = find(&parts, b"name")?.payload().get(4..)?;
        if name != b"iTunSMPB" {
            continue;
        }
        let value = find(&parts, b"data")?.payload().get(8..)?;
        let text = std::str::from_utf8(value).ok()?;
        let mut words = text.split_whitespace().skip(1);
        let priming = u32::from_str_radix(words.next()?, 16).ok()?;
        words.next()?;
        words.next()?;
        return (priming > 0 && priming < 16_384).then_some(priming);
    }
    None
}

/// Whether `moov` holds a zlib-compressed QuickTime header (`cmov`).
pub(crate) fn has_compressed_header(moov: &RawBox) -> bool {
    children(&body(moov))
        .iter()
        .any(|child| &child.kind.0 == b"cmov")
}

/// A track title from `udta` `name` or `(c)nam`.
fn udta_title(udta: &RawBox) -> Option<String> {
    for child in children(&body(udta)) {
        let text = match &child.kind.0 {
            b"name" => child.payload(),
            [0xa9, b'n', b'a', b'm'] => {
                let payload = child.payload();
                let length = usize::from(u16::from_be_bytes([*payload.first()?, *payload.get(1)?]));
                payload.get(4..4 + length)?
            }
            _ => continue,
        };
        let text = String::from_utf8_lossy(text)
            .trim_end_matches('\0')
            .to_string();
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

//! Codec, decoder configuration, picture and sound facts of a `trak`, read by `FFmpeg`'s rules.

use alpha2_media::{
    ColorMatrix, ColorMetadata, ColorPrimaries, ColorRange, DynamicRange, MAX_AUDIO_CHANNELS,
    Orientation, Rect, Rotation, SampleAspectRatio, Speaker, SpeakerLayout, TransferFunction,
    frame_size_supported,
};
use bytes::{BufMut, Bytes, BytesMut};

use super::boxes::{RawBox, Reader, children, find};
use super::movie::{AudioFields, EntryFields, Matrix, SampleEntry, Trak};
use crate::{AudioCodec, AudioTrack, DemuxError, PcmFormat, TrackKind, VideoCodec, VideoTrack};

/// The codec family as `FFmpeg` sees it, where it changes how packets are timed or flagged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Family {
    Aac,
    Ac3,
    Eac3,
    Vorbis,
    /// Uncompressed PCM, with the bytes of one sample of every channel.
    Pcm(u32),
    /// Every packet decodes on its own (`AV_CODEC_PROP_INTRA_ONLY`), or a data track.
    IntraOnly,
    Other,
}

/// What [`describe`] finds.
pub(crate) struct Described {
    pub kind: TrackKind,
    pub family: Family,
    pub video: Option<VideoTrack>,
    pub audio: Option<AudioTrack>,
    /// Bytes per PCM sample of all channels, as `FFmpeg`'s `sample_size` for chunked PCM.
    pub pcm_frame_bytes: u32,
    pub audio_fields: AudioFields,
}

/// The facts of `trak`, read from its first sample entry.
pub(crate) fn describe(trak: &Trak) -> Result<Described, DemuxError> {
    let kind = match &trak.handler {
        b"vide" => TrackKind::Video,
        b"soun" => TrackKind::Audio,
        _ => TrackKind::Other,
    };
    let mut described = Described {
        kind,
        family: Family::IntraOnly,
        video: None,
        audio: None,
        pcm_frame_bytes: 0,
        audio_fields: AudioFields::default(),
    };
    let Some(entry) = trak.entries.first() else {
        return Ok(described);
    };
    match &entry.fields {
        EntryFields::Video { width, height } if kind == TrackKind::Video => {
            let (codec, family) = video_codec(entry);
            let (width, height) = (u32::from(*width), u32::from(*height));
            if !frame_size_supported(width, height) {
                return Err(DemuxError::Invalid(format!(
                    "{width}x{height} pictures are larger than alpha2 decodes"
                )));
            }
            described.family = family;
            let private = video_private(entry, codec);
            let reorder_depth = crate::mp4::nal::declared_reorder_depth(codec, &private);
            described.video = Some(VideoTrack {
                codec,
                codec_private: private,
                in_band_parameter_sets: matches!(&entry.format.0, b"hev1" | b"avc3"),
                width,
                height,
                orientation: orientation(&trak.matrix),
                sample_aspect_ratio: pixel_aspect(entry, trak.display_size, (width, height)),
                color: color(entry),
                clean_aperture: clean_aperture(entry, width, height),
                frame_rate: None,
                alpha: codec == VideoCodec::Hevc
                    && find(&entry.children, b"hvcC")
                        .is_some_and(|hvcc| hevc_alpha_layer(hvcc.payload())),
                reorder_depth,
            });
        }
        EntryFields::Audio(fields) if kind == TrackKind::Audio => {
            let audio = audio_track(entry, fields, trak.timescale);
            described.family = audio_family(&audio);
            if let Family::Pcm(frame_bytes) = described.family {
                described.pcm_frame_bytes = frame_bytes;
            }
            described.audio_fields = *fields;
            described.audio = Some(audio);
        }
        _ => described.kind = TrackKind::Other,
    }
    Ok(described)
}

fn audio_family(audio: &AudioTrack) -> Family {
    match audio.codec {
        AudioCodec::Aac => Family::Aac,
        AudioCodec::Ac3 => Family::Ac3,
        AudioCodec::Eac3 => Family::Eac3,
        AudioCodec::Vorbis => Family::Vorbis,
        AudioCodec::Pcm(format) => {
            Family::Pcm(u32::from(format.bits / 8).saturating_mul(audio.channels))
        }
        AudioCodec::Mp3
        | AudioCodec::Opus
        | AudioCodec::Flac
        | AudioCodec::Alac
        | AudioCodec::Dts => Family::IntraOnly,
        AudioCodec::Other(_) => Family::Other,
    }
}

/// The object type of an `esds` decoder configuration, and its decoder-specific info.
fn esds_config(esds: &RawBox) -> Option<(u8, Bytes)> {
    let payload = esds.bytes.slice(esds.header..);
    let mut at = 4;
    let descriptor = |at: &mut usize| -> Option<(u8, usize)> {
        let tag = *payload.get(*at)?;
        *at += 1;
        let mut length = 0_usize;
        for _ in 0..4 {
            let byte = *payload.get(*at)?;
            *at += 1;
            length = (length << 7) | usize::from(byte & 0x7f);
            if byte & 0x80 == 0 {
                break;
            }
        }
        Some((tag, length))
    };
    let (tag, _) = descriptor(&mut at)?;
    if tag == 3 {
        at += 2;
        let flags = *payload.get(at)?;
        at += 1;
        if flags & 0x80 != 0 {
            at += 2;
        }
        if flags & 0x40 != 0 {
            at += 1 + usize::from(*payload.get(at)?);
        }
        if flags & 0x20 != 0 {
            at += 2;
        }
    } else {
        at += 2;
    }
    let (tag, _) = descriptor(&mut at)?;
    if tag != 4 {
        return None;
    }
    let object_type = *payload.get(at)?;
    at += 13;
    let info = match descriptor(&mut at) {
        Some((5, length)) => {
            let end = at.checked_add(length)?.min(payload.len());
            payload.slice(at.min(end)..end)
        }
        _ => Bytes::new(),
    };
    Some((object_type, info))
}

/// The `esds` of a sample entry, directly or inside its `QuickTime` `wave` box.
fn find_esds(entry: &SampleEntry) -> Option<RawBox> {
    find(&entry.children, b"esds").cloned().or_else(|| {
        let wave = find(&entry.children, b"wave")?;
        children(&wave.bytes.slice(wave.header..))
            .into_iter()
            .find(|child| &child.kind.0 == b"esds")
    })
}

fn video_codec(entry: &SampleEntry) -> (VideoCodec, Family) {
    let format = entry.format;
    match &format.0 {
        b"avc1" | b"avc3" => (VideoCodec::H264, Family::Other),
        b"hvc1" | b"hev1" => (VideoCodec::Hevc, Family::Other),
        b"dvh1" if find(&entry.children, b"hvcC").is_some() => (VideoCodec::Hevc, Family::Other),
        b"vp08" => (VideoCodec::Vp8, Family::Other),
        b"vp09" => (VideoCodec::Vp9, Family::Other),
        b"av01" => (VideoCodec::Av1, Family::Other),
        b"mp4v" => match find_esds(entry).and_then(|esds| esds_config(&esds)) {
            Some((0x20, _)) | None => (VideoCodec::Mpeg4Part2, Family::Other),
            Some((0x6c, _)) => (VideoCodec::Other(format), Family::IntraOnly),
            Some(_) => (VideoCodec::Other(format), Family::Other),
        },
        b"apch" | b"apcn" | b"apcs" | b"apco" | b"ap4h" | b"ap4x" => {
            (VideoCodec::ProRes(format), Family::IntraOnly)
        }
        b"jpeg" | b"mjpa" | b"mjpb" | b"AVDJ" | b"dmb1" => {
            (VideoCodec::Other(format), Family::IntraOnly)
        }
        _ => (VideoCodec::Other(format), Family::Other),
    }
}

fn video_private(entry: &SampleEntry, codec: VideoCodec) -> Bytes {
    let payload_of =
        |kind: &[u8; 4]| find(&entry.children, kind).map(|found| found.bytes.slice(found.header..));
    match codec {
        VideoCodec::H264 => payload_of(b"avcC"),
        VideoCodec::Hevc => payload_of(b"hvcC"),
        VideoCodec::Vp9 | VideoCodec::Vp8 => payload_of(b"vpcC"),
        VideoCodec::Av1 => payload_of(b"av1C"),
        VideoCodec::Mpeg4Part2 => find_esds(entry)
            .and_then(|esds| esds_config(&esds))
            .map(|(_, info)| info),
        VideoCodec::ProRes(_) | VideoCodec::Other(_) => None,
    }
    .unwrap_or_default()
}

/// `FFmpeg`'s reading of a display matrix: a mirroring matrix (negative determinant) loses its
/// horizontal flip first, then the counter-clockwise angle becomes a clockwise rotation.
pub(crate) fn orientation(matrix: &Matrix) -> Orientation {
    if *matrix == super::movie::IDENTITY {
        return Orientation::UPRIGHT;
    }
    let [[first, second, _], [third, fourth, _], _] = *matrix;
    let mirror = i64::from(first) * i64::from(fourth) - i64::from(second) * i64::from(third) < 0;
    let sign = if mirror { -1.0 } else { 1.0 };
    let fixed = |value: i32| f64::from(value) / 65536.0;
    let (x_cos, y_sin) = (sign * fixed(first), fixed(second));
    let (x_sin, y_cos) = (sign * fixed(third), fixed(fourth));
    let scale_x = x_cos.hypot(x_sin);
    let scale_y = y_sin.hypot(y_cos);
    if scale_x == 0.0 || scale_y == 0.0 {
        return Orientation::UPRIGHT;
    }
    let counter_clockwise = -(y_sin / scale_y).atan2(x_cos / scale_x).to_degrees();
    let clockwise = if mirror {
        counter_clockwise
    } else {
        -counter_clockwise
    }
    .rem_euclid(360.0);
    let turns = (clockwise / 90.0).round();
    if (clockwise - turns * 90.0).abs() >= 0.5 {
        return Orientation::UPRIGHT;
    }
    #[allow(clippy::cast_possible_truncation)]
    Orientation {
        rotation: Rotation::from_quarter_turns(turns as i32),
        mirror,
    }
}

/// The `pasp` ratio, else the one a `tkhd` size other than the coded size implies (`FFmpeg`'s
/// fallback).
fn pixel_aspect(
    entry: &SampleEntry,
    display: (u32, u32),
    coded: (u32, u32),
) -> Option<SampleAspectRatio> {
    let declared = find(&entry.children, b"pasp").and_then(|pasp| {
        let mut reader = Reader::new(pasp.payload(), "pasp");
        let (num, den) = (reader.i32().ok()?, reader.i32().ok()?);
        (num != 0 && den != 0).then_some((num, den))
    });
    if let Some((num, den)) = declared {
        return SampleAspectRatio::new(u64::try_from(num).ok()?, u64::try_from(den).ok()?);
    }
    let ((display_width, display_height), (width, height)) = (display, coded);
    if display_width == 0 || display_height == 0 || width == 0 || height == 0 || display == coded {
        return None;
    }
    SampleAspectRatio::new(
        u64::from(height) * u64::from(display_width),
        u64::from(width) * u64::from(display_height),
    )
}

/// `colr` `nclx` and `nclc` as `FFmpeg` reads them, then mapped the way alpha2's `FFmpeg` host maps
/// codec parameters: `None` when nothing is declared, otherwise unrecognized values read as
/// BT.709 limited range.
fn color(entry: &SampleEntry) -> Option<ColorMetadata> {
    const UNSPECIFIED: u16 = 2;
    let mut tags: Option<(u16, u16, u16, Option<bool>)> = None;
    for colr in entry
        .children
        .iter()
        .filter(|child| &child.kind.0 == b"colr")
    {
        let payload = colr.payload();
        let full = match payload.get(..4) {
            Some(b"nclx") => Some(payload.get(10).is_some_and(|byte| byte >> 7 == 1)),
            Some(b"nclc") => None,
            _ => continue,
        };
        let Some(values) = payload.get(4..10) else {
            continue;
        };
        let read = |at: usize| u16::from_be_bytes([values[at], values[at + 1]]);
        let (primaries, transfer, matrix) = (read(0), read(2), read(4));
        let range = full.or(tags.and_then(|tags| tags.3));
        tags = Some((
            if matches!(primaries, 0..=12 | 22) {
                primaries
            } else {
                UNSPECIFIED
            },
            if transfer <= 18 {
                transfer
            } else {
                UNSPECIFIED
            },
            if matrix <= 17 { matrix } else { UNSPECIFIED },
            range,
        ));
    }
    let (primaries, transfer, matrix, full) = tags?;
    if primaries == UNSPECIFIED
        && transfer == UNSPECIFIED
        && matrix == UNSPECIFIED
        && full.is_none()
    {
        return None;
    }
    let transfer = match transfer {
        16 => TransferFunction::Pq,
        18 => TransferFunction::Hlg,
        13 => TransferFunction::Srgb,
        _ => TransferFunction::Bt709,
    };
    Some(ColorMetadata {
        primaries: match primaries {
            9 => ColorPrimaries::Bt2020,
            5 | 6 => ColorPrimaries::Bt601,
            _ => ColorPrimaries::Bt709,
        },
        transfer,
        matrix: match matrix {
            9 => ColorMatrix::Bt2020Ncl,
            5 | 6 => ColorMatrix::Bt601,
            0 => ColorMatrix::Rgb,
            _ => ColorMatrix::Bt709,
        },
        range: if full == Some(true) {
            ColorRange::Full
        } else {
            ColorRange::Limited
        },
        dynamic_range: if matches!(transfer, TransferFunction::Pq | TransferFunction::Hlg) {
            DynamicRange::Hdr
        } else {
            DynamicRange::Sdr
        },
    })
}

/// The `clap` clean aperture inside a `width` x `height` picture, by `FFmpeg`'s arithmetic;
/// `None` when it is the whole picture or does not fit.
fn clean_aperture(entry: &SampleEntry, width: u32, height: u32) -> Option<Rect> {
    let clap = find(&entry.children, b"clap")?;
    let mut reader = Reader::new(clap.payload(), "clap");
    let mut fraction = || -> Option<(i32, i32)> { Some((reader.i32().ok()?, reader.i32().ok()?)) };
    let (aperture_width, aperture_height, horizontal, vertical) =
        (fraction()?, fraction()?, fraction()?, fraction()?);
    if aperture_width.0 < 0
        || aperture_width.1 <= 0
        || aperture_height.0 < 0
        || aperture_height.1 <= 0
        || horizontal.1 <= 0
        || vertical.1 <= 0
        || width == 0
        || height == 0
    {
        return None;
    }
    let ratio = |fraction: (i32, i32)| f64::from(fraction.0) / f64::from(fraction.1);
    let center_x = f64::from(width - 1) / 2.0 + ratio(horizontal);
    let center_y = f64::from(height - 1) / 2.0 + ratio(vertical);
    let half_width = (ratio(aperture_width) - 1.0) / 2.0;
    let half_height = (ratio(aperture_height) - 1.0) / 2.0;
    let edge = |value: f64| -> Option<u64> {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        (value >= 0.0 && value.is_finite()).then_some(value as u64)
    };
    let left = edge(center_x - half_width)?;
    let right = edge(center_x + half_width)?;
    let top = edge(center_y - half_height)?;
    let bottom = edge(center_y + half_height)?;
    let (width, height) = (u64::from(width), u64::from(height));
    if bottom > height - 1 || right > width - 1 {
        return None;
    }
    let (right, bottom) = (width - 1 - right, height - 1 - bottom);
    if left | right | top | bottom == 0 || left + right >= width || top + bottom >= height {
        return None;
    }
    Some(Rect {
        x: u32::try_from(left).ok()?,
        y: u32::try_from(top).ok()?,
        width: u32::try_from(width - left - right).ok()?,
        height: u32::try_from(height - top - bottom).ok()?,
    })
}

fn audio_track(entry: &SampleEntry, fields: &AudioFields, timescale: u32) -> AudioTrack {
    let format = entry.format;
    let mut channels = fields.channels;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let mut sample_rate = if fields.sample_rate > 0.0 && fields.sample_rate < 4_294_967_295.0 {
        fields.sample_rate as u32
    } else {
        0
    };
    let mut bits_per_sample = None;
    let mut codec_private = Bytes::new();
    let codec = match &format.0 {
        b"mp4a" => match find_esds(entry).and_then(|esds| esds_config(&esds)) {
            Some((0x40 | 0x66..=0x68, info)) => {
                if let Some((rate, count)) = audio_specific_config(&info) {
                    sample_rate = rate;
                    if count > 0 {
                        channels = count;
                    }
                }
                codec_private = info;
                AudioCodec::Aac
            }
            Some((0x69 | 0x6b, _)) => AudioCodec::Mp3,
            Some((0xa5, _)) => AudioCodec::Ac3,
            Some((0xa6, _)) => AudioCodec::Eac3,
            Some((0xa9, _)) => AudioCodec::Dts,
            Some((0xad, _)) => AudioCodec::Opus,
            Some((0xdd, info)) => {
                codec_private = info;
                AudioCodec::Vorbis
            }
            _ => AudioCodec::Other(format),
        },
        b".mp3" | b"mp3 " => AudioCodec::Mp3,
        b"Opus" => {
            if let Some(dops) = find(&entry.children, b"dOps") {
                codec_private = opus_head(dops.payload()).unwrap_or_default();
            }
            AudioCodec::Opus
        }
        b"fLaC" => {
            if let Some(dfla) = find(&entry.children, b"dfLa")
                && let Some(blocks) = dfla.payload().get(4..)
            {
                let mut header = BytesMut::with_capacity(4 + blocks.len());
                header.put_slice(b"fLaC");
                header.put_slice(blocks);
                codec_private = header.freeze();
                bits_per_sample = flac_bits(blocks);
            }
            AudioCodec::Flac
        }
        b"alac" => {
            let cookie = find(&entry.children, b"alac").cloned().or_else(|| {
                let wave = find(&entry.children, b"wave")?;
                children(&wave.bytes.slice(wave.header..))
                    .into_iter()
                    .find(|child| &child.kind.0 == b"alac")
            });
            if let Some(config) = cookie.as_ref().and_then(|cookie| cookie.payload().get(4..)) {
                if config.len() >= 24 {
                    bits_per_sample = Some(u16::from(config[5]));
                    channels = u32::from(config[9]);
                    sample_rate =
                        u32::from_be_bytes([config[20], config[21], config[22], config[23]]);
                }
                codec_private = Bytes::copy_from_slice(config);
            }
            AudioCodec::Alac
        }
        b"ac-3" => AudioCodec::Ac3,
        b"ec-3" => AudioCodec::Eac3,
        b"dtsc" | b"dtsh" | b"dtsl" | b"dtse" => AudioCodec::Dts,
        _ => match pcm_format(entry, fields) {
            Some(pcm) => {
                bits_per_sample = Some(pcm.bits);
                AudioCodec::Pcm(pcm)
            }
            None => AudioCodec::Other(format),
        },
    };
    if sample_rate == 0 && timescale > 1 {
        sample_rate = timescale;
    }
    AudioTrack {
        codec,
        codec_private,
        sample_rate,
        channels,
        speakers: speakers(entry, channels),
        bits_per_sample,
        priming: 0,
        seek_preroll: None,
    }
}

/// `dOps` (big-endian) as the Ogg `OpusHead` (little-endian) Opus decoders take.
fn opus_head(dops: &[u8]) -> Option<Bytes> {
    if dops.len() < 11 || dops[0] != 0 {
        return None;
    }
    let mut head = BytesMut::with_capacity(dops.len() + 8);
    head.put_slice(b"OpusHead");
    head.put_u8(1);
    head.put_u8(dops[1]);
    head.put_u16_le(u16::from_be_bytes([dops[2], dops[3]]));
    head.put_u32_le(u32::from_be_bytes([dops[4], dops[5], dops[6], dops[7]]));
    head.put_u16_le(u16::from_be_bytes([dops[8], dops[9]]));
    head.put_slice(&dops[10..]);
    Some(head.freeze())
}

/// The bits per sample of a FLAC `STREAMINFO` block, the first metadata block.
fn flac_bits(blocks: &[u8]) -> Option<u16> {
    if blocks.first()? & 0x7f != 0 {
        return None;
    }
    let info = blocks.get(4..4 + 34)?;
    Some((u16::from(info[12] & 0x01) << 4 | u16::from(info[13] >> 4)) + 1)
}

/// The output sample rate and channel count of an AAC `AudioSpecificConfig`.
pub(crate) fn audio_specific_config(config: &[u8]) -> Option<(u32, u32)> {
    const RATES: [u32; 13] = [
        96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
        8_000, 7_350,
    ];
    let mut bits = super::nal::Bits::new(config);
    let mut object_type = bits.read(5)?;
    if object_type == 31 {
        object_type = 32 + bits.read(6)?;
    }
    let rate_of = |bits: &mut super::nal::Bits<'_>| -> Option<u32> {
        let index = bits.read(4)?;
        if index == 15 {
            bits.read(24)
        } else {
            RATES.get(index as usize).copied()
        }
    };
    let rate = rate_of(&mut bits)?;
    let configuration = bits.read(4)?;
    let mut output_rate = rate;
    if object_type == 5 || object_type == 29 {
        output_rate = rate_of(&mut bits)?;
    }
    let channels = match configuration {
        1..=6 => configuration,
        7 => 8,
        _ => 0,
    };
    Some((output_rate, channels))
}

/// The PCM layout of an uncompressed sound entry, by `FFmpeg`'s codec choice for its type,
/// version 2 flags, `enda` and `pcmC`.
fn pcm_format(entry: &SampleEntry, fields: &AudioFields) -> Option<PcmFormat> {
    let bits = u16::try_from(fields.sample_size).unwrap_or(0);
    let format = |bits, float, big_endian, signed| PcmFormat {
        bits,
        float,
        big_endian,
        signed,
    };
    let mut pcm = match &entry.format.0 {
        b"raw " | b"NONE" => format(8, false, false, false),
        b"twos" => format(16, false, true, true),
        b"sowt" => format(16, false, false, true),
        b"in24" => format(24, false, true, true),
        b"in32" => format(32, false, true, true),
        b"fl32" => format(32, true, true, true),
        b"fl64" => format(64, true, true, true),
        b"lpcm" => {
            let flags = fields.lpcm_flags?;
            let float = flags & 1 != 0;
            let big_endian = flags & 2 != 0;
            let signed = flags & 4 != 0;
            match (bits, float) {
                (32 | 64, true) => format(bits, true, big_endian, true),
                (8, false) => format(8, false, false, signed),
                (16 | 24 | 32 | 64, false) => format(bits, false, big_endian, signed),
                _ => return None,
            }
        }
        b"ipcm" | b"fpcm" => {
            let pcmc = find(&entry.children, b"pcmC")?;
            let payload = pcmc.payload();
            if payload.len() < 6 || payload[..4] != [0, 0, 0, 0] {
                return None;
            }
            let little = payload[4] & 1 != 0;
            let size = u16::from(payload[5]);
            match (&entry.format.0, size) {
                (b"fpcm", 32 | 64) => format(size, true, !little, true),
                (b"ipcm", 16 | 24 | 32) => format(size, false, !little, true),
                _ => return None,
            }
        }
        [0, 0, 0, 0] => match bits {
            8 => format(8, false, false, false),
            16 => format(16, false, true, true),
            _ => return None,
        },
        _ => return None,
    };
    if matches!(&entry.format.0, b"raw " | b"twos" | b"sowt" | b"NONE") || entry.format.0 == [0; 4]
    {
        pcm = match (pcm.bits, bits) {
            (8, 16) => format(16, false, true, true),
            (16, 8) => format(8, false, false, true),
            (16, 24 | 32) => format(bits, false, pcm.big_endian, true),
            _ => pcm,
        };
    }
    let little_endian = find(&entry.children, b"wave").is_some_and(|wave| {
        children(&wave.bytes.slice(wave.header..))
            .iter()
            .any(|child| &child.kind.0 == b"enda" && child.payload().get(1) == Some(&1))
    }) || find(&entry.children, b"enda")
        .is_some_and(|enda| enda.payload().get(1) == Some(&1));
    if little_endian && pcm.big_endian && pcm.bits >= 16 {
        pcm.big_endian = false;
    }
    Some(pcm)
}

/// `FFmpeg`'s channel number for a Core Audio channel label.
fn channel_of_label(label: u32) -> i64 {
    match label {
        1..=18 => i64::from(label) - 1,
        35 => 31,
        36 => 32,
        37 => 35,
        38 => 29,
        39 => 30,
        0 => 0x200,
        _ => 0x300,
    }
}

/// Core Audio layout tags (`kAudioChannelLayoutTag_*`) and their channels by `FFmpeg` channel
/// number, from `FFmpeg`'s `mov_chan.c`.
const LAYOUT_TAGS: &[(u32, &[i64])] = &[
    (100, &[2]),
    (101, &[0, 1]),
    (102, &[0, 1]),
    (106, &[0, 1]),
    (104, &[0, 1]),
    (105, &[0, 1]),
    (103, &[29, 30]),
    (149, &[2, 3]),
    (113, &[0, 1, 2]),
    (114, &[2, 0, 1]),
    (150, &[0, 2, 1]),
    (131, &[0, 1, 8]),
    (133, &[0, 1, 3]),
    (107, &[0x400, 0x403, 0x401, 0x402]),
    (108, &[0, 1, 4, 5]),
    (115, &[0, 1, 2, 8]),
    (116, &[2, 0, 1, 8]),
    (151, &[0, 2, 1, 8]),
    (132, &[0, 1, 9, 10]),
    (134, &[0, 1, 3, 8]),
    (153, &[0, 1, 8, 3]),
    (136, &[0, 1, 2, 3]),
    (152, &[0, 2, 1, 3]),
    (168, &[2, 0, 1, 3]),
    (109, &[0, 1, 4, 5, 2]),
    (117, &[0, 1, 2, 9, 10]),
    (118, &[0, 1, 9, 10, 2]),
    (119, &[0, 2, 1, 9, 10]),
    (120, &[2, 0, 1, 9, 10]),
    (135, &[0, 1, 3, 9, 10]),
    (138, &[0, 1, 9, 10, 3]),
    (137, &[0, 1, 2, 3, 8]),
    (154, &[0, 2, 1, 8, 3]),
    (169, &[2, 0, 1, 8, 3]),
    (110, &[0, 1, 4, 5, 2, 8]),
    (172, &[2, 8, 0, 1, 4, 5]),
    (121, &[0, 1, 2, 3, 9, 10]),
    (122, &[0, 1, 9, 10, 2, 3]),
    (123, &[0, 2, 1, 9, 10, 3]),
    (124, &[2, 0, 1, 9, 10, 3]),
    (139, &[0, 1, 9, 10, 2, 8]),
    (141, &[2, 0, 1, 9, 10, 8]),
    (155, &[0, 2, 1, 9, 10, 8]),
    (170, &[6, 7, 0, 1, 9, 10]),
    (171, &[2, 0, 1, 4, 5, 11]),
    (125, &[0, 1, 2, 3, 9, 10, 8]),
    (142, &[2, 0, 1, 9, 10, 8, 3]),
    (157, &[0, 2, 1, 9, 10, 3, 8]),
    (182, &[2, 0, 1, 9, 10, 3, 8]),
    (140, &[0, 1, 9, 10, 2, 4, 5]),
    (143, &[2, 0, 1, 9, 10, 4, 5]),
    (156, &[0, 2, 1, 9, 10, 4, 5]),
    (148, &[0, 1, 9, 10, 2, 6, 7]),
    (176, &[6, 2, 7, 0, 1, 9, 10]),
    (158, &[0, 2, 1, 9, 10, 3, 11]),
    (159, &[0, 2, 1, 9, 10, 3, 13]),
    (173, &[6, 7, 0, 1, 9, 10, 3]),
    (174, &[2, 0, 1, 4, 5, 11, 3]),
    (175, &[2, 8, 0, 1, 4, 5, 3]),
    (111, &[0, 1, 4, 5, 2, 8, 9, 10]),
    (144, &[2, 0, 1, 9, 10, 4, 5, 8]),
    (112, &[0, 1, 4, 5, 12, 14, 15, 17]),
    (126, &[0, 1, 2, 3, 9, 10, 6, 7]),
    (127, &[2, 6, 7, 0, 1, 9, 10, 3]),
    (129, &[0, 1, 9, 10, 2, 3, 6, 7]),
    (161, &[0, 2, 1, 9, 10, 3, 6, 7]),
    (177, &[6, 2, 7, 0, 1, 9, 10, 3]),
    (128, &[0, 1, 2, 3, 9, 10, 4, 5]),
    (160, &[0, 2, 1, 9, 10, 3, 4, 5]),
    (130, &[0, 1, 2, 3, 9, 10, 29, 30]),
    (162, &[0, 2, 1, 9, 10, 3, 33, 34]),
    (163, &[0, 2, 1, 9, 10, 3, 31, 32]),
    (164, &[0, 2, 1, 9, 10, 3, 12, 14]),
    (165, &[0, 2, 1, 9, 10, 3, 8, 11]),
    (166, &[0, 2, 1, 9, 10, 3, 8, 13]),
    (167, &[0, 2, 1, 9, 10, 3, 11, 13]),
    (178, &[6, 7, 0, 1, 9, 10, 4, 5]),
    (179, &[6, 2, 7, 0, 1, 9, 8, 10]),
    (180, &[6, 7, 0, 1, 9, 10, 4, 5, 3]),
    (181, &[6, 2, 7, 0, 1, 9, 8, 10, 3]),
];

/// ISO/IEC 23001-8 channel configurations by `FFmpeg` channel number.
const ISO_CONFIGURATIONS: &[(u32, &[i64])] = &[
    (1, &[2]),
    (2, &[0, 1]),
    (3, &[2, 0, 1]),
    (4, &[2, 0, 1, 8]),
    (5, &[2, 0, 1, 9, 10]),
    (6, &[2, 0, 1, 9, 10, 3]),
    (7, &[2, 6, 7, 0, 1, 9, 10, 3]),
    (9, &[0, 1, 8]),
    (10, &[0, 1, 9, 10]),
    (11, &[2, 0, 1, 9, 10, 8, 3]),
    (12, &[2, 0, 1, 9, 10, 4, 5, 3]),
    (
        13,
        &[
            2, 6, 7, 0, 1, 41, 42, 4, 5, 8, 3, 35, 13, 12, 14, 36, 37, 11, 15, 17, 16, 38, 39, 40,
        ],
    ),
    (14, &[2, 0, 1, 9, 10, 3, 12, 14]),
];

/// ISO/IEC 23001-8 speaker positions by `FFmpeg` channel number; -1 has no counterpart.
const ISO_POSITIONS: [i64; 43] = [
    0, 1, 2, 3, 9, 10, 6, 7, 4, 5, 8, 33, 34, 41, 42, 31, 32, 12, 14, 13, 15, 17, 16, 36, 37, 11,
    35, 39, 40, 38, 43, 44, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
];

/// The speaker layout `chan`, `chnl`, `SA3D` or an AC-3 `dac3`/`dec3` declares for `channels`
/// channels, as `FFmpeg` reads it; `None` when they declare none or name no loudspeaker.
fn speakers(entry: &SampleEntry, channels: u32) -> Option<SpeakerLayout> {
    if channels as usize > MAX_AUDIO_CHANNELS {
        return None;
    }
    let mut declared: Option<Vec<i64>> = None;
    let wave = find(&entry.children, b"wave")
        .map(|wave| children(&wave.bytes.slice(wave.header..)))
        .unwrap_or_default();
    for child in entry.children.iter().chain(wave.iter()) {
        match &child.kind.0 {
            b"chan" => {
                if let Some(found) = chan_layout(child.payload(), channels) {
                    declared = Some(found);
                }
            }
            b"chnl" => {
                if let Some(found) = chnl_layout(child.payload(), channels) {
                    declared = Some(found);
                }
            }
            b"SA3D" => {
                if let Some(found) = spatial_audio_layout(child.payload()) {
                    declared = Some(found);
                }
            }
            b"dac3" | b"dec3" if declared.is_none() => {
                declared = dolby_layout(child.kind.0 == *b"dec3", child.payload())
                    .filter(|found| found.len() == channels as usize);
            }
            _ => {}
        }
    }
    let speakers = declared?
        .into_iter()
        .map(Speaker::from_ffmpeg_channel)
        .collect::<Vec<_>>();
    if speakers.iter().all(|speaker| *speaker == Speaker::Unnamed) {
        return None;
    }
    SpeakerLayout::new(&speakers)
}

/// The `FFmpeg` channels a `dac3` or `dec3` box's audio coding mode and LFE flag name (for
/// E-AC-3, of the first independent substream, with no dependent ones).
fn dolby_layout(enhanced: bool, payload: &[u8]) -> Option<Vec<i64>> {
    /// `acmod` 0 to 7: 1+1, 1/0, 2/0, 3/0, 2/1, 3/1, 2/2, 3/2 (`FL` 0, `FR` 1, `FC` 2, `BC` 8,
    /// `SL` 9, `SR` 10).
    const MODES: [&[i64]; 8] = [
        &[0, 1],
        &[2],
        &[0, 1],
        &[0, 1, 2],
        &[0, 1, 8],
        &[0, 1, 2, 8],
        &[0, 1, 9, 10],
        &[0, 1, 2, 9, 10],
    ];
    let (bytes, mode_shift) = if enhanced {
        (payload.get(2..5)?, 9)
    } else {
        (payload.get(..3)?, 11)
    };
    let info = u32::from(bytes[0]) << 16 | u32::from(bytes[1]) << 8 | u32::from(bytes[2]);
    if enhanced && (info >> 1) & 0xf != 0 {
        return None;
    }
    let mut channels = MODES[(info >> mode_shift & 7) as usize].to_vec();
    if info >> (mode_shift - 1) & 1 == 1 {
        channels.push(3);
    }
    channels.sort_unstable();
    Some(channels)
}

fn chan_layout(payload: &[u8], channels: u32) -> Option<Vec<i64>> {
    let mut reader = Reader::new(payload, "chan");
    reader.skip(4).ok()?;
    let size = payload.len().checked_sub(4)?;
    if size < 12 {
        return None;
    }
    let tag = reader.u32().ok()?;
    let bitmap = reader.u32().ok()?;
    let descriptions = reader.u32().ok()?;
    if (size as u64) < 12 + u64::from(descriptions) * 20 {
        return None;
    }
    match tag {
        0 => {
            if descriptions == 0 || descriptions < channels {
                return None;
            }
            let count = descriptions.min(channels);
            let mut layout = vec![0x300; channels as usize];
            for slot in layout.iter_mut().take(count as usize) {
                let label = reader.u32().ok()?;
                reader.skip(16).ok()?;
                *slot = channel_of_label(label);
            }
            Some(layout)
        }
        0x0001_0000 => ((channels == 0 || bitmap.count_ones() == channels) && bitmap < 0x40000)
            .then(|| {
                (0..32)
                    .filter(|bit| bitmap & (1 << bit) != 0)
                    .map(i64::from)
                    .collect()
            }),
        _ if tag & 0xffff != 0 => {
            let count = tag & 0xffff;
            if count != channels {
                return None;
            }
            LAYOUT_TAGS
                .iter()
                .find(|(code, list)| (code << 16) | u32::try_from(list.len()).unwrap_or(0) == tag)
                .map(|(_, list)| list.to_vec())
        }
        _ => None,
    }
}

/// The ambisonic channels (and head-locked stereo) of a Spatial Audio `SA3D` box, as `FFmpeg`
/// accepts it: version 0, ACN order, SN3D normalization.
fn spatial_audio_layout(payload: &[u8]) -> Option<Vec<i64>> {
    const AMBISONIC_BASE: i64 = 0x400;
    if payload.len() < 16 {
        return None;
    }
    let mut reader = Reader::new(payload, "SA3D");
    let version = reader.u8().ok()?;
    let kind = reader.u8().ok()?;
    if version != 0 || kind & 0x7f != 0 {
        return None;
    }
    let head_locked = i64::from(kind >> 7) * 2;
    let order = i64::from(reader.i32().ok()?);
    if reader.u8().ok()? != 0 || reader.u8().ok()? != 0 {
        return None;
    }
    let count = i64::from(reader.u32().ok()?);
    if !(0..=31).contains(&order) || count != (order + 1) * (order + 1) + head_locked {
        return None;
    }
    let ambisonic = count - head_locked;
    (0..count)
        .map(|_| {
            let channel = i64::from(reader.u32().ok()?);
            if channel >= count {
                None
            } else if channel >= ambisonic {
                Some(channel - ambisonic)
            } else {
                Some(AMBISONIC_BASE + channel)
            }
        })
        .collect()
}

fn chnl_layout(payload: &[u8], channels: u32) -> Option<Vec<i64>> {
    let mut reader = Reader::new(payload, "chnl");
    if reader.u32().ok()? != 0 {
        return None;
    }
    let structure = reader.u8().ok()?;
    if structure & 1 == 0 {
        return None;
    }
    let configuration = reader.u8().ok()?;
    if configuration == 0 {
        let mut layout = Vec::with_capacity(channels as usize);
        for _ in 0..channels {
            let position = reader.u8().ok()?;
            if position == 126 {
                reader.skip(3).ok()?;
            }
            let channel = ISO_POSITIONS
                .get(usize::from(position))
                .copied()
                .unwrap_or(-1);
            layout.push(if channel < 0 { 0x300 } else { channel });
        }
        return Some(layout);
    }
    let omitted = reader.u64().ok()?;
    let total = channels.checked_add(omitted.count_ones())?;
    let (_, list) = ISO_CONFIGURATIONS.iter().find(|(code, list)| {
        *code == u32::from(configuration) && u32::try_from(list.len()) == Ok(total)
    })?;
    if omitted >> list.len() != 0 {
        return None;
    }
    Some(
        list.iter()
            .enumerate()
            .filter(|(index, _)| omitted & (1 << index) == 0)
            .map(|(_, channel)| *channel)
            .take(channels as usize)
            .collect(),
    )
}

/// Whether the first VPS of an `hvcC` declares an auxiliary alpha layer
/// (`scalability_mask_flag[3]`, ITU-T H.265 F.7.4.3.1.1).
fn hevc_alpha_layer(hvcc: &[u8]) -> bool {
    super::nal::hevc_alpha_layer(hvcc).unwrap_or(false)
}

/// The ISO 639-2 code of an `mdhd` language, `FFmpeg`'s way; `None` for `und` or none.
pub(crate) fn language(code: u16) -> Option<String> {
    const MACINTOSH: &[&str] = &[
        "eng", "fra", "ger", "ita", "dut", "sve", "spa", "dan", "por", "nor", "heb", "jpn", "ara",
        "fin", "gre", "ice", "mlt", "tur", "hr ", "chi", "urd", "hin", "tha", "kor", "lit", "pol",
        "hun", "est", "lav", "smi", "fo ", "per", "rus", "chi", "", "iri", "alb", "ron", "ces",
        "slk", "slv", "yid", "sr ", "mac", "bul", "ukr", "bel", "uzb", "kaz", "aze", "aze", "arm",
        "geo", "mol", "kir", "tgk", "tuk", "mon", "", "pus", "kur", "kas", "snd", "tib", "nep",
        "san", "mar", "ben", "asm", "guj", "pa ", "ori", "mal", "kan", "tam", "tel", "sin", "bur",
        "khm", "lao", "vie", "ind", "tgl", "may", "may", "amh", "tir", "orm", "som", "swa", "kin",
        "run", "nya", "mlg", "epo", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
        "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "wel", "baq",
        "cat", "lat", "que", "grn", "aym", "tat", "uig", "dzo", "jav",
    ];
    let text = if code >= 0x400 && code != 0x7fff {
        let letters = [(code >> 10) & 0x1f, (code >> 5) & 0x1f, code & 0x1f];
        letters
            .iter()
            .map(|letter| char::from(0x60 + u8::try_from(*letter).unwrap_or(0)))
            .collect::<String>()
    } else {
        (*MACINTOSH.get(usize::from(code))?).to_string()
    };
    (!text.is_empty() && text != "und").then_some(text)
}

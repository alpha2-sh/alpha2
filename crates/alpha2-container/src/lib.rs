//! alpha2's own media container layer, for hosts without libavformat.
//!
//! [`open`] sniffs a file and returns a [`ContainerReader`] that lists its tracks and hands out
//! compressed packets per track; [`ContainerWriter`]s write MP4/MOV and Matroska/WebM. Timestamps
//! are in each track's timescale, on the presentation timeline after edit lists, and match what
//! `FFmpeg`'s demuxer reports for the same file, which the tests use as the oracle.
//!
//! Inputs are untrusted: sizes, counts and dimensions are bounded before anything is allocated
//! (frames at most 16384 pixels per side and 2^26 pixels in all, as the engine refuses larger).

use std::path::Path;

use alpha2_media::{ColorMetadata, Orientation, Rect, SampleAspectRatio, SpeakerLayout};
use alpha2_time::FrameRate;
use bytes::Bytes;

mod error;

pub use error::{DemuxError, MuxError};

/// A track's number within its container (MP4 `track_ID`, Matroska `TrackNumber`).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TrackId(pub u32);

/// What a track carries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrackKind {
    /// Pictures.
    Video,
    /// Sound.
    Audio,
    /// Anything else (subtitles, timecode, metadata), listed but never decoded.
    Other,
}

/// A four-character code, as MP4 sample entries name codecs.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FourCc(pub [u8; 4]);

impl std::fmt::Display for FourCc {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            let shown = if byte.is_ascii_graphic() || byte == b' ' {
                char::from(byte)
            } else {
                '?'
            };
            write!(formatter, "{shown}")?;
        }
        Ok(())
    }
}

/// A video codec, as the container declares it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoCodec {
    /// H.264/AVC; `codec_private` is the `AVCDecoderConfigurationRecord` (`avcC`).
    H264,
    /// H.265/HEVC; `codec_private` is the `HEVCDecoderConfigurationRecord` (`hvcC`).
    Hevc,
    /// VP8; no `codec_private`.
    Vp8,
    /// VP9; `codec_private` is the `vpcC` body (version, flags and config), when present.
    Vp9,
    /// AV1; `codec_private` is the `AV1CodecConfigurationRecord` (`av1C`).
    Av1,
    /// MPEG-4 Part 2; `codec_private` is the decoder-specific info from `esds`.
    Mpeg4Part2,
    /// Apple `ProRes`, by its sample-entry four-character code.
    ProRes(FourCc),
    /// Anything else, by its four-character code or Matroska codec ID's first four bytes.
    Other(FourCc),
}

/// How PCM samples are stored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PcmFormat {
    /// Bits per sample: 8, 16, 24, 32 or 64.
    pub bits: u16,
    /// IEEE float rather than integer.
    pub float: bool,
    /// Big-endian rather than little-endian.
    pub big_endian: bool,
    /// Signed integers (8-bit WAV is unsigned).
    pub signed: bool,
}

/// An audio codec, as the container declares it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioCodec {
    /// AAC; `codec_private` is the `AudioSpecificConfig`.
    Aac,
    /// MPEG-1/2 Layer III; no `codec_private`.
    Mp3,
    /// Opus; `codec_private` is the `OpusHead` (identification header).
    Opus,
    /// Vorbis; `codec_private` holds the three headers, Xiph-laced as Matroska stores them.
    Vorbis,
    /// FLAC; `codec_private` is `fLaC` followed by the metadata blocks (at least `STREAMINFO`).
    Flac,
    /// Apple Lossless; `codec_private` is the `ALACSpecificConfig` (magic cookie).
    Alac,
    /// Uncompressed PCM.
    Pcm(PcmFormat),
    /// AC-3; no `codec_private`.
    Ac3,
    /// E-AC-3; no `codec_private`.
    Eac3,
    /// DTS; no `codec_private`.
    Dts,
    /// Anything else, by its four-character code or Matroska codec ID's first four bytes.
    Other(FourCc),
}

/// A video track's picture facts.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoTrack {
    /// The codec.
    pub codec: VideoCodec,
    /// The decoder configuration record (see [`VideoCodec`]); empty when the codec has none.
    pub codec_private: Bytes,
    /// Whether parameter sets also travel in the packets (`hev1`, `avc3`).
    pub in_band_parameter_sets: bool,
    /// Coded width in pixels, as the sample entry stores it.
    pub width: u32,
    /// Coded height in pixels.
    pub height: u32,
    /// How the stored picture is shown: the track matrix, or the Matroska projection pose, with
    /// `FFmpeg`'s rotation and mirror semantics.
    pub orientation: Orientation,
    /// The pixel aspect ratio the container declares (`pasp`, Matroska display size).
    pub sample_aspect_ratio: Option<SampleAspectRatio>,
    /// The color the container declares (`colr` `nclx`, Matroska `Colour`).
    pub color: Option<ColorMetadata>,
    /// The clean aperture (`clap`), when declared.
    pub clean_aperture: Option<Rect>,
    /// The nominal frame rate, when the container states one.
    pub frame_rate: Option<FrameRate>,
    /// Whether the track carries alpha the way alpha2 reads it (HEVC alpha layer, VP9 block
    /// additions).
    pub alpha: bool,
}

/// An audio track's sound facts.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioTrack {
    /// The codec.
    pub codec: AudioCodec,
    /// The codec's setup data (see [`AudioCodec`]); empty when it has none.
    pub codec_private: Bytes,
    /// Samples per second.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u32,
    /// Speaker positions the container declares (`chan`, `WAVEFORMATEXTENSIBLE` mask), when it
    /// does.
    pub speakers: Option<SpeakerLayout>,
    /// Bits per decoded sample, when stated.
    pub bits_per_sample: Option<u16>,
    /// Decoder output samples to drop at the start: AAC priming from the edit list or
    /// `iTunSMPB`, Matroska `CodecDelay`, Opus pre-skip.
    pub priming: u32,
}

/// One track of a container.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackInfo {
    /// The track's number.
    pub id: TrackId,
    /// What it carries.
    pub kind: TrackKind,
    /// Units per second of its timestamps.
    pub timescale: u32,
    /// Presentation duration in `timescale` units, after edits, when known.
    pub duration: Option<i64>,
    /// ISO 639-2 language, when tagged and not `und`.
    pub language: Option<String>,
    /// The track's name, when tagged.
    pub title: Option<String>,
    /// Flagged as a default track (`tkhd` enabled for MP4, `FlagDefault` for Matroska).
    pub default: bool,
    /// Flagged enabled (MP4 `tkhd`, Matroska `FlagEnabled`).
    pub enabled: bool,
    /// Picture facts of a video track.
    pub video: Option<VideoTrack>,
    /// Sound facts of an audio track.
    pub audio: Option<AudioTrack>,
}

/// One compressed frame of one track.
#[derive(Clone, Debug, PartialEq)]
pub struct Packet {
    /// The track it belongs to.
    pub track: TrackId,
    /// Presentation time in the track's timescale, after edits; negative for priming and
    /// pre-roll.
    pub pts: i64,
    /// Decode time in the track's timescale.
    pub dts: i64,
    /// Duration in the track's timescale; 0 when unknown.
    pub duration: i64,
    /// Decodable on its own (a sync sample).
    pub keyframe: bool,
    /// The frame, as the codec stores it (length-prefixed NAL units for `avcC`/`hvcC` tracks).
    pub data: Bytes,
    /// Decoded samples to drop from the start and end of this packet's output (Matroska
    /// `DiscardPadding`, the last AAC frame's padding), as `FFmpeg`'s skip-samples side data.
    pub discard: (u32, u32),
}

/// A demuxer: the tracks of one file and their packets.
///
/// Each track is read on its own, in decode order, as if no other track existed. Hosts open one
/// reader per consumer when they read several tracks at different positions.
pub trait ContainerReader: Send {
    /// The tracks, in container order.
    fn tracks(&self) -> &[TrackInfo];

    /// The index into [`Self::tracks`] of the default video track: the first enabled video
    /// track flagged default, else the first video track.
    fn default_video(&self) -> Option<usize> {
        default_track(self.tracks(), TrackKind::Video)
    }

    /// The index into [`Self::tracks`] of the default audio track, by the same rule.
    fn default_audio(&self) -> Option<usize> {
        default_track(self.tracks(), TrackKind::Audio)
    }

    /// Positions `track` so its next packet is the latest keyframe whose presentation time is at
    /// or before `pts`, or its first packet when none is. Returns that keyframe's `pts`; `None`
    /// for a track without packets.
    ///
    /// # Errors
    /// An unknown track, or a file that cannot be read there.
    fn seek(&mut self, track: TrackId, pts: i64) -> Result<Option<i64>, DemuxError>;

    /// The next packet of `track` in decode order; `None` at its end.
    ///
    /// # Errors
    /// An unknown track, damaged data, or a read failure.
    fn next_packet(&mut self, track: TrackId) -> Result<Option<Packet>, DemuxError>;
}

/// A muxer: writes tracks' packets into one file.
pub trait ContainerWriter: Send {
    /// Adds a track described by `track` (its `id` is ignored) and returns the id it gets.
    ///
    /// # Errors
    /// A codec the container cannot carry, or a track added after the first packet.
    fn add_track(&mut self, track: &TrackInfo) -> Result<TrackId, MuxError>;

    /// Writes `packet`. Packets of each track arrive in decode order; tracks may interleave in
    /// any order, and the writer interleaves them itself.
    ///
    /// # Errors
    /// Timestamps that go backward, or a write failure.
    fn write(&mut self, packet: Packet) -> Result<(), MuxError>;

    /// Writes the index and closes the file.
    ///
    /// # Errors
    /// A write failure.
    fn finish(self: Box<Self>) -> Result<(), MuxError>;
}

/// The first enabled track of `kind` flagged default, else the first of `kind`.
#[must_use]
pub fn default_track(tracks: &[TrackInfo], kind: TrackKind) -> Option<usize> {
    tracks
        .iter()
        .position(|track| track.kind == kind && track.enabled && track.default)
        .or_else(|| tracks.iter().position(|track| track.kind == kind))
}

/// The container formats this layer reads, as a sniff of the first bytes finds them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContainerFormat {
    /// ISO base media: MP4, MOV, M4A.
    Mp4,
    /// Matroska and `WebM`.
    Matroska,
    /// RIFF WAVE.
    Wav,
    /// MPEG audio (MP3), with or without an `ID3v2` tag.
    Mp3,
    /// AAC in ADTS frames.
    Adts,
    /// Native FLAC.
    Flac,
    /// Ogg (Opus, Vorbis, FLAC).
    Ogg,
}

/// The format of a file starting with `head` (at least its first 4096 bytes when it has them).
///
/// # Errors
/// [`DemuxError::Unsupported`] for MPEG-TS, AVI and anything unrecognized.
pub fn sniff(head: &[u8]) -> Result<ContainerFormat, DemuxError> {
    let at = |offset: usize, bytes: &[u8]| head.get(offset..offset + bytes.len()) == Some(bytes);
    if at(4, b"ftyp") || at(4, b"moov") || at(4, b"mdat") || at(4, b"wide") || at(4, b"free") {
        return Ok(ContainerFormat::Mp4);
    }
    if at(0, &[0x1a, 0x45, 0xdf, 0xa3]) {
        return Ok(ContainerFormat::Matroska);
    }
    if at(0, b"RIFF") && at(8, b"WAVE") {
        return Ok(ContainerFormat::Wav);
    }
    if at(0, b"RIFF") && at(8, b"AVI ") {
        return Err(DemuxError::Unsupported(
            "AVI files are not read on this platform; convert to MP4 or MOV".into(),
        ));
    }
    if at(0, b"fLaC") {
        return Ok(ContainerFormat::Flac);
    }
    if at(0, b"OggS") {
        return Ok(ContainerFormat::Ogg);
    }
    if head.first() == Some(&0x47) && head.get(188) == Some(&0x47) {
        return Err(DemuxError::Unsupported(
            "MPEG-TS files are not read on this platform; convert to MP4 or MOV".into(),
        ));
    }
    if at(0, b"ID3") {
        return Ok(ContainerFormat::Mp3);
    }
    if head.len() >= 2 && head[0] == 0xff && head[1] & 0xf6 == 0xf0 {
        return Ok(ContainerFormat::Adts);
    }
    if head.len() >= 2 && head[0] == 0xff && head[1] & 0xe0 == 0xe0 {
        return Ok(ContainerFormat::Mp3);
    }
    Err(DemuxError::Unsupported(
        "the file is not a container this platform reads".into(),
    ))
}

/// Opens `path` with the demuxer its first bytes call for.
///
/// # Errors
/// [`DemuxError::Unsupported`] for a format this layer does not read; read or parse failures.
pub fn open(path: &Path) -> Result<Box<dyn ContainerReader>, DemuxError> {
    use std::io::Read;
    let mut head = Vec::with_capacity(4096);
    std::fs::File::open(path)?
        .take(4096)
        .read_to_end(&mut head)?;
    let format = sniff(&head)?;
    Err(DemuxError::Unsupported(format!(
        "{format:?} is recognized but its demuxer is not built"
    )))
}

#[cfg(test)]
mod tests;

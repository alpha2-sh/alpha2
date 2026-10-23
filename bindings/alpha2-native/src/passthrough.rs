//! Copying a source's compressed video packets into an export unchanged.
//!
//! The project side of the decision (one untouched clip covering the range)
//! is the engine's ([`alpha2_engine::passthrough_plan`] in the host). This is
//! the file side: whether the packets under that range decode, on their own,
//! into exactly the frames rendering would show, and then the copy itself.
//!
//! The test is on timestamps, not on flags. The copy takes one contiguous run
//! of video packets in decode order. It must start at an IDR access unit, so
//! nothing in it refers to a picture before it (a keyframe flag alone does not
//! promise that), and the presentation times in the run must be exactly the
//! export's frame times, one packet each. A B picture that needs a reference
//! outside the range puts that reference inside the run with a presentation
//! time outside it, and the run is refused; so is a stream whose frames are off
//! the export's grid, a variable rate, or a duplicate. What passes is a
//! self-contained stretch of the stream whose decoded frames are the rendered
//! ones, which is the only thing a copy may claim.
//!
//! Several such runs (clips cut one after another) join into one output
//! ([`PacketCopy::join`]) when one MP4 sample description fits them all and
//! their decode times keep increasing across each join; each run starts at an
//! IDR picture, so a decoder restarts there.

use std::path::{Path, PathBuf};

use alpha2_ffmpeg_bridge::DemuxInput;
use alpha2_media::{
    CancelToken, ChromaLocation, ColorMetadata, EncodeError, StreamKind, YuvEncoding,
};
use alpha2_time::MediaTime;
use rsmpeg::avutil::{AVRational, av_rescale_q};
use rsmpeg::ffi;

use crate::NativeVideoCodec;
use crate::convert::{encode_adapter_error, encode_adapter_error_at, tick_base};
use crate::mux::{AudioStream, Muxer, VideoStream};

/// The stream an export copies from and what it must match.
#[derive(Clone, Debug)]
pub struct PacketCopyRequest<'a> {
    /// The source file.
    pub source: &'a Path,
    /// Each output frame's source time, in order: what rendering would show.
    pub frame_times: &'a [MediaTime],
    /// The output size, which the stream must already have.
    pub width: u32,
    /// See `width`.
    pub height: u32,
    /// The codec the export asked for, which the stream must already be.
    pub codec: NativeVideoCodec,
    /// The file format the export writes, which must offer the copied
    /// streams' codecs.
    pub container: alpha2_media::Container,
    /// The color the output is tagged with when it renders; a copy keeps the
    /// source's tags, so they must already say this.
    pub color: ColorMetadata,
    /// Where this run's first frame lands in the output: zero for a single
    /// copy, a later segment's start in a join.
    pub output_start: MediaTime,
    /// The mix is the source's own audio, untouched: copy it too where that
    /// can be exact, which is over the whole file. Where it cannot, the plan
    /// says why ([`PacketCopyPlan::audio_copy_refusal`]) and the caller
    /// encodes the mix instead; the video copy stands either way.
    pub audio: Option<PacketCopyAudio>,
}

/// What a copied audio stream must match.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PacketCopyAudio {
    /// The project's sample rate.
    pub sample_rate: u32,
    /// The export's channel count.
    pub channels: u32,
}

/// A copy that can be made, and what it copies.
#[derive(Clone, Debug)]
pub struct PacketCopyPlan {
    source: PathBuf,
    video_stream: usize,
    audio_stream: Option<usize>,
    /// Why audio the request offered to copy is not copied.
    audio_refusal: Option<String>,
    /// The copied run, as positions in the video stream's decode order.
    first_packet: u64,
    last_packet: u64,
    /// The run's first packet's timestamps, in the video stream's time base:
    /// what a writer that seeks to it recognizes it by.
    first_timestamps: (i64, i64),
    /// The first output frame's presentation time, in the video stream's
    /// time base: it lands at `offset` in the output.
    origin: i64,
    /// `output_start`, in the video stream's time base.
    offset: i64,
    video_time_base: AVRational,
    /// What an MP4 sample description records of the stream: runs that join
    /// must agree on all of it.
    contract: StreamContract,
    /// The run's first and last decode times in the output, in ticks.
    decode_span: (i64, i64),
}

/// The part of a video stream a single MP4 sample description fixes.
#[derive(Clone, Debug, Eq, PartialEq)]
struct StreamContract {
    codec_id: ffi::AVCodecID,
    width: i32,
    height: i32,
    format: i32,
    /// The parameter sets.
    extradata: Vec<u8>,
    time_base: (i32, i32),
}

/// One or more copy runs written back to back into one output.
#[derive(Clone, Debug)]
pub struct PacketCopy {
    segments: Vec<PacketCopyPlan>,
}

impl PacketCopy {
    /// Joins runs, in output order, into one copy.
    ///
    /// Each run is already a self-contained stretch of its stream starting
    /// at an IDR picture, so a decoder restarts at every join. What a join
    /// adds is that one sample description must describe every run (the
    /// same codec, size, pixel format, parameter sets, and time base), and
    /// that decode times keep increasing across it, which fails when the
    /// runs reorder pictures differently.
    ///
    /// # Errors
    /// Names the join that breaks either, or a run that copies audio in a
    /// join (copied audio travels only with one whole file).
    pub fn join(segments: Vec<PacketCopyPlan>) -> Result<Self, String> {
        let first = segments
            .first()
            .ok_or_else(|| "a copy needs at least one run".to_owned())?;
        if segments.len() > 1 && segments.iter().any(PacketCopyPlan::copies_audio) {
            return Err("copied audio travels only with one whole file".to_owned());
        }
        for (index, pair) in segments.windows(2).enumerate() {
            if pair[1].contract != first.contract {
                return Err(format!(
                    "the source of run {} is not encoded like the first run's (codec, size, pixel format, parameter sets, or time base differ), and one MP4 sample description must fit both",
                    index + 2
                ));
            }
            if pair[1].decode_span.0 <= pair[0].decode_span.1 {
                return Err(format!(
                    "at the join into run {}, decode times go back: the runs reorder pictures differently",
                    index + 2
                ));
            }
        }
        Ok(Self { segments })
    }

    /// Video packets the copy writes: one per output frame.
    #[must_use]
    pub fn video_packets(&self) -> u64 {
        self.segments
            .iter()
            .map(PacketCopyPlan::video_packets)
            .sum()
    }

    /// Runs joined.
    #[must_use]
    pub fn segments(&self) -> usize {
        self.segments.len()
    }

    /// Whether the source's audio is copied too.
    #[must_use]
    pub fn copies_audio(&self) -> bool {
        matches!(&self.segments[..], [only] if only.copies_audio())
    }

    /// Why the source's audio, which the request offered to copy, is not.
    #[must_use]
    pub fn audio_copy_refusal(&self) -> Option<&str> {
        self.segments
            .first()
            .and_then(PacketCopyPlan::audio_copy_refusal)
    }
}

impl PacketCopyPlan {
    /// Video packets the copy writes: one per output frame.
    #[must_use]
    pub const fn video_packets(&self) -> u64 {
        self.last_packet - self.first_packet + 1
    }

    /// Whether the source's audio is copied too.
    #[must_use]
    pub const fn copies_audio(&self) -> bool {
        self.audio_stream.is_some()
    }

    /// Why the source's audio, which the request offered to copy, is not.
    #[must_use]
    pub fn audio_copy_refusal(&self) -> Option<&str> {
        self.audio_refusal.as_deref()
    }
}

/// The export's mixed audio, block by block, for a copy that encodes it.
pub trait AudioBlockFeed {
    /// The next block, starting where the previous one ended, at sample zero
    /// of the output; `None` once the export's audio is complete.
    ///
    /// # Errors
    /// Says why the mix could not be produced.
    fn next_block(&mut self) -> Result<Option<alpha2_media::AudioBlock>, String>;
}

/// The audio a packet copy writes beside its video.
pub enum CopyAudio<'a> {
    /// None.
    None,
    /// The plan's copied audio packets; see [`PacketCopyPlan::copies_audio`].
    Source,
    /// The export's mix, encoded.
    Encode {
        /// The encoder's settings.
        config: crate::AudioStreamConfig,
        /// Where the mix comes from.
        feed: &'a mut dyn AudioBlockFeed,
    },
}

/// What a copy wrote.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct PacketCopyReport {
    /// Video packets written.
    pub video_packets: u64,
    /// Audio packets copied.
    pub audio_packets: u64,
    /// Audio sample frames encoded from the mix.
    pub encoded_audio_frames: u64,
    /// Payload bytes written, both streams.
    pub bytes: u64,
}

/// One video packet as the scan saw it.
#[derive(Clone)]
struct ScannedPacket {
    pts: i64,
    dts: i64,
    idr: bool,
    /// The stream's sample description changes here
    /// (`AV_PKT_DATA_NEW_EXTRADATA`).
    new_description: bool,
}

/// Decides whether `request` can be served by copying packets.
///
/// A host planning several runs of one export uses one
/// [`PacketCopyPlanner`], which reads each source once.
///
/// # Errors
/// Names why not: the container, codec, size, color, or frame timing that a
/// copy would not reproduce, or a run that does not start at an IDR picture.
pub fn plan_packet_copy(request: &PacketCopyRequest<'_>) -> Result<PacketCopyPlan, String> {
    PacketCopyPlanner::new(CancelToken::default()).plan(request)
}

/// Plans the runs of one export, reading each source file once however many
/// runs it serves, and stopping when the export is cancelled.
pub struct PacketCopyPlanner {
    cancel: CancelToken,
    sources: std::collections::HashMap<PathBuf, SourceFacts>,
}

/// What a plan learns of one source file, each part read when first needed.
#[derive(Default)]
struct SourceFacts {
    probe: Option<Result<crate::ProbeResult, String>>,
    video: Option<Result<VideoParameters, String>>,
    scan: Option<Result<std::rc::Rc<Scan>, String>>,
}

/// The selected video stream's codec parameters a copy depends on.
#[derive(Clone)]
struct VideoParameters {
    codec_id: ffi::AVCodecID,
    format: i32,
    field_order: ffi::AVFieldOrder,
    sample_aspect_ratio: AVRational,
    extradata: Vec<u8>,
    time_base: AVRational,
    width: i32,
    height: i32,
    tags: (
        ffi::AVColorPrimaries,
        ffi::AVColorTransferCharacteristic,
        ffi::AVColorSpace,
        ffi::AVColorRange,
    ),
}

impl PacketCopyPlanner {
    /// A planner that gives up as soon as `cancel` is set.
    #[must_use]
    pub fn new(cancel: CancelToken) -> Self {
        Self {
            cancel,
            sources: std::collections::HashMap::new(),
        }
    }

    /// [`plan_packet_copy`], from what this planner has read of the source.
    ///
    /// # Errors
    /// As [`plan_packet_copy`], and a cancel.
    pub fn plan(&mut self, request: &PacketCopyRequest<'_>) -> Result<PacketCopyPlan, String> {
        if request.frame_times.is_empty() {
            return Err("the range has no frames".to_owned());
        }
        let probed = self.probe(request.source)?;
        let (video_index, audio_stream) = check_metadata(&probed, request)?;
        let parameters = self.video(request.source, video_index)?;
        let (length_size, contract) = stream_framing(request, &parameters)?;
        let time_base = parameters.time_base;
        let expected = request
            .frame_times
            .iter()
            .map(|time| av_rescale_q(time.ticks(), tick_base(), time_base))
            .collect::<Vec<_>>();
        // Exact frame identity: the ticks each expected pts converts back to must be the frame time
        // itself, or the grid does not land on the stream's time base.
        if expected
            .iter()
            .zip(request.frame_times)
            .any(|(pts, time)| av_rescale_q(*pts, time_base, tick_base()) != time.ticks())
        {
            return Err(
                "the export's frame times do not land on the source's time base".to_owned(),
            );
        }
        let scanned = self.scan(
            request.source,
            video_index,
            probed.selected_audio,
            request.codec,
            length_size,
        )?;
        plan_run(
            request,
            &scanned,
            &expected,
            video_index,
            audio_stream,
            contract,
        )
    }

    fn facts(&mut self, source: &Path) -> &mut SourceFacts {
        self.sources.entry(source.to_owned()).or_default()
    }

    fn probe(&mut self, source: &Path) -> Result<crate::ProbeResult, String> {
        self.facts(source)
            .probe
            .get_or_insert_with(|| {
                crate::probe(source).map_err(|error| format!("probe failed: {error}"))
            })
            .clone()
    }

    fn video(&mut self, source: &Path, index: usize) -> Result<VideoParameters, String> {
        self.facts(source)
            .video
            .get_or_insert_with(|| video_parameters(source, index))
            .clone()
    }

    fn scan(
        &mut self,
        source: &Path,
        index: usize,
        audio: Option<usize>,
        codec: NativeVideoCodec,
        length_size: usize,
    ) -> Result<std::rc::Rc<Scan>, String> {
        let cancel = self.cancel.clone();
        self.facts(source)
            .scan
            .get_or_insert_with(|| {
                scan(source, index, codec, length_size, audio, &cancel).map(std::rc::Rc::new)
            })
            .clone()
    }
}

/// The plan of one run, from its source's scan.
fn plan_run(
    request: &PacketCopyRequest<'_>,
    scanned: &Scan,
    expected: &[i64],
    video_index: usize,
    audio_stream: Option<Result<usize, String>>,
    contract: StreamContract,
) -> Result<PacketCopyPlan, String> {
    let time_base = AVRational {
        num: contract.time_base.0,
        den: contract.time_base.1,
    };
    let packets = &scanned.video;
    let first = packets
        .iter()
        .position(|packet| packet.pts == expected[0])
        .ok_or_else(|| "no source frame is at the range's first frame time".to_owned())?;
    let last = first + expected.len() - 1;
    let run = packets
        .get(first..=last)
        .ok_or_else(|| "the source ends before the range does".to_owned())?;
    if !run[0].idr {
        return Err(
            "the range does not start at an IDR picture, so its first frames may refer to earlier ones"
                .to_owned(),
        );
    }
    // The stream's parameter sets are its first sample description's; a run under another is
    // decoded with the wrong ones.
    if packets[1..=last]
        .iter()
        .any(|packet| packet.new_description)
    {
        return Err(
            "the source changes its sample description (its parameter sets) before or inside the range, and a copy carries only the first"
                .to_owned(),
        );
    }
    let mut presented = run.iter().map(|packet| packet.pts).collect::<Vec<_>>();
    presented.sort_unstable();
    if presented != expected {
        return Err(
            "the packets under the range are not exactly its frames: the source is off the export's frame grid, or a picture in it depends on one outside it"
                .to_owned(),
        );
    }
    // Anywhere but the whole file the boundary audio packets straddle the range, and cutting them
    // is not a copy.
    let (audio_stream, audio_refusal) = match audio_stream {
        Some(Ok(_)) if first != 0 || last + 1 != packets.len() => (
            None,
            Some("audio is copied only with the whole of its video".to_owned()),
        ),
        Some(Ok(index)) => match check_audio_span(scanned.audio, expected[0], time_base, request) {
            Ok(()) => (Some(index), None),
            Err(reason) => (None, Some(reason)),
        },
        Some(Err(reason)) => (None, Some(reason)),
        None => (None, None),
    };
    let offset = av_rescale_q(request.output_start.ticks(), tick_base(), time_base);
    let output_decode = |dts: i64| av_rescale_q(dts - expected[0] + offset, time_base, tick_base());
    let decode_span = (
        output_decode(run.iter().map(|packet| packet.dts).min().unwrap_or(0)),
        output_decode(run.iter().map(|packet| packet.dts).max().unwrap_or(0)),
    );
    Ok(PacketCopyPlan {
        source: request.source.to_owned(),
        video_stream: video_index,
        audio_stream,
        audio_refusal,
        first_packet: u64::try_from(first).unwrap_or(u64::MAX),
        last_packet: u64::try_from(last).unwrap_or(u64::MAX),
        first_timestamps: (run[0].pts, run[0].dts),
        origin: expected[0],
        offset,
        video_time_base: time_base,
        contract,
        decode_span,
    })
}

/// The container and stream metadata a copy keeps: the video stream, and
/// the audio stream when the request copies audio.
fn check_metadata(
    probed: &crate::ProbeResult,
    request: &PacketCopyRequest<'_>,
) -> Result<(usize, Option<Result<usize, String>>), String> {
    if !probed
        .format
        .split(',')
        .any(|name| matches!(name, "mov" | "mp4"))
    {
        return Err(format!(
            "the source is {}, and only MP4/MOV sources are copied",
            probed.format
        ));
    }
    if !request.container.offers_video(request.codec.model()) {
        return Err(format!(
            "the source's {} is not offered in {}",
            request.codec.as_str(),
            request.container
        ));
    }
    let video_index = probed
        .selected_video
        .ok_or_else(|| "the source has no video stream".to_owned())?;
    check_picture(
        probed,
        video_index,
        (request.width, request.height),
        request.color,
    )?;
    let audio_stream = request
        .audio
        .map(|audio| copyable_audio(probed, audio, request.container));

    Ok((video_index, audio_stream))
}

/// Whether the video stream's pictures, shown as they are, already are the
/// output's: its size, upright with square pixels, tagged with its color.
fn check_picture(
    probed: &crate::ProbeResult,
    video_index: usize,
    (width, height): (u32, u32),
    color: ColorMetadata,
) -> Result<(), String> {
    let video = probed
        .streams
        .iter()
        .find(|stream| stream.index == video_index)
        .ok_or_else(|| "the source's video stream is missing".to_owned())?;
    if video.dimensions != Some((width, height)) {
        return Err(format!(
            "the source is {:?}, not the {width}x{height} output",
            video.dimensions
        ));
    }
    if video.orientation.mirror {
        return Err(
            "the source mirrors the picture in its display matrix, which a render applies"
                .to_owned(),
        );
    }
    if video.orientation.rotation != alpha2_media::Rotation::Deg0 {
        return Err("the source is rotated".to_owned());
    }
    let aspect = video.sample_aspect_ratio;
    if aspect != alpha2_media::SampleAspectRatio::default() {
        return Err(format!(
            "the source shows {}:{} pixels, which a render makes square",
            aspect.num, aspect.den
        ));
    }
    if video.color != Some(color) {
        return Err(format!(
            "the source is tagged {:?}, and the output must be tagged {color:?}",
            video.color
        ));
    }
    Ok(())
}

/// Whether the codec parameters a copy carries into the output show its
/// pictures as a render does. A render shows the pixel aspect ratio
/// [`check_picture`] reads: the container's declaration, which overrides the
/// codec's. A copy carries the codec's (in the bitstream, and in the `pasp`
/// box the output then writes), so a codec ratio a container overrides to
/// square pixels would show otherwise. A YUV transcode encodes its own
/// parameters, so only a copy asks this.
fn check_carried_display(aspect: AVRational) -> Result<(), String> {
    if aspect.num > 0 && aspect.den > 0 && aspect.num != aspect.den {
        return Err(format!(
            "the source's codec declares {}:{} pixels, which a copy keeps and its container overrides to square",
            aspect.num, aspect.den
        ));
    }
    Ok(())
}

/// Whether the stream's pictures are progressive 8-bit 4:2:0 in one of
/// `formats`.
fn check_sampling(
    format: i32,
    field_order: ffi::AVFieldOrder,
    formats: &[ffi::AVPixelFormat],
) -> Result<(), String> {
    if !formats.contains(&format) {
        return Err("the source is not 8-bit 4:2:0".to_owned());
    }
    if !matches!(
        field_order,
        ffi::AV_FIELD_PROGRESSIVE | ffi::AV_FIELD_UNKNOWN
    ) {
        return Err("the source is interlaced".to_owned());
    }
    Ok(())
}

/// Decides whether `source`'s decoded pictures can become a
/// `width`x`height` output's frames as they are, with no conversion: the
/// file side of a YUV transcode, whose project side is the same untouched
/// tiling a copy needs.
///
/// Unlike a copy, any container and codec qualify, and the frame grid is
/// free: the decoder picks each frame's picture exactly as it does for a
/// render, so only what the render would have changed (size, orientation,
/// pixel shape, color, sampling, and where the chroma samples sit) decides.
/// The output is encoded as `encoding`.
///
/// # Errors
/// Names what the render would have changed.
pub fn check_yuv_transcode(
    source: &Path,
    width: u32,
    height: u32,
    encoding: YuvEncoding,
) -> Result<(), String> {
    let input =
        crate::decode::open_input(source).map_err(|error| format!("probe failed: {error}"))?;
    let probed = crate::decode::probe_context(&input, source)
        .map_err(|error| format!("probe failed: {error}"))?;
    let video_index = probed
        .selected_video
        .ok_or_else(|| "the source has no video stream".to_owned())?;
    check_picture(&probed, video_index, (width, height), encoding.color)?;
    let parameters = input.streams()[video_index].codecpar();
    check_sampling(
        parameters.format,
        parameters.field_order,
        &[ffi::AV_PIX_FMT_YUV420P, ffi::AV_PIX_FMT_NV12],
    )?;
    let siting =
        crate::convert::decoded_chroma_location(parameters.codec_id, parameters.chroma_location);
    if !same_chroma_siting(siting, encoding.chroma) {
        return Err(format!(
            "the source's chroma sits {siting:?}, and the output's must sit {:?}",
            encoding.chroma
        ));
    }
    Ok(())
}

/// Whether chroma sited at `source` is where `output` puts it, reading an
/// unspecified location as centred, as import does.
#[must_use]
pub fn same_chroma_siting(source: ChromaLocation, output: ChromaLocation) -> bool {
    let centred = |location| {
        if location == ChromaLocation::Unspecified {
            ChromaLocation::Center
        } else {
            location
        }
    };
    centred(source) == centred(output)
}

/// The video stream's codec parameters, read from the file.
fn video_parameters(source: &Path, index: usize) -> Result<VideoParameters, String> {
    let input = open(source)?;
    let stream = input
        .streams()
        .get(index)
        .ok_or_else(|| "the source's video stream is missing".to_owned())?;
    let parameters = stream.codecpar();
    Ok(VideoParameters {
        codec_id: parameters.codec_id,
        format: parameters.format,
        field_order: parameters.field_order,
        sample_aspect_ratio: parameters.sample_aspect_ratio,
        extradata: alpha2_ffmpeg_bridge::codec_parameters_extradata(&parameters),
        time_base: stream.time_base,
        width: parameters.width,
        height: parameters.height,
        tags: (
            parameters.color_primaries,
            parameters.color_trc,
            parameters.color_space,
            parameters.color_range,
        ),
    })
}

/// The video stream's NAL framing and sample description, after the checks
/// only the codec parameters answer.
fn stream_framing(
    request: &PacketCopyRequest<'_>,
    parameters: &VideoParameters,
) -> Result<(usize, StreamContract), String> {
    if parameters.codec_id != request.codec.codec_id() {
        return Err(format!(
            "the source's video is not {}",
            request.codec.as_str()
        ));
    }
    check_sampling(
        parameters.format,
        parameters.field_order,
        &[ffi::AV_PIX_FMT_YUV420P],
    )?;
    check_carried_display(parameters.sample_aspect_ratio)?;
    // A copy keeps the stream's own tags, so they must be the output's exactly, not a summary that
    // reads an unspecified one as BT.709.
    if parameters.tags != crate::convert::color_tags(request.color) {
        return Err(format!(
            "the source is tagged primaries {}, transfer {}, matrix {}, range {} (FFmpeg's numbers), and the output must be tagged {:?}",
            parameters.tags.0,
            parameters.tags.1,
            parameters.tags.2,
            parameters.tags.3,
            request.color
        ));
    }
    let length_size = nal_length_size(request.codec, &parameters.extradata)?;
    Ok((
        length_size,
        StreamContract {
            codec_id: parameters.codec_id,
            width: parameters.width,
            height: parameters.height,
            format: parameters.format,
            extradata: parameters.extradata.clone(),
            time_base: (parameters.time_base.num, parameters.time_base.den),
        },
    ))
}

/// The source's audio stream, when it is what the export would mix.
fn copyable_audio(
    probed: &crate::ProbeResult,
    audio: PacketCopyAudio,
    container: alpha2_media::Container,
) -> Result<usize, String> {
    let index = probed
        .selected_audio
        .ok_or_else(|| "the source has no audio stream to copy".to_owned())?;
    let stream = probed
        .streams
        .iter()
        .find(|stream| stream.index == index && stream.kind == StreamKind::Audio)
        .ok_or_else(|| "the source's audio stream is missing".to_owned())?;
    if stream.codec != "aac" {
        return Err(format!("the source's audio is {}, not AAC", stream.codec));
    }
    if !container.offers_audio(alpha2_media::AudioCodec::Aac) {
        return Err(format!("the source's aac is not offered in {container}"));
    }
    if stream.sample_rate != Some(audio.sample_rate) || stream.channels != Some(audio.channels) {
        return Err(format!(
            "the source's audio is {:?} Hz x {:?}, not the export's {} Hz x {}",
            stream.sample_rate, stream.channels, audio.sample_rate, audio.channels
        ));
    }
    Ok(index)
}

fn open(path: &Path) -> Result<DemuxInput, String> {
    crate::decode::open_input(path).map_err(|error| format!("cannot open the source: {error}"))
}

/// The byte width of each NAL unit's length prefix, from the sample
/// description. Only length-prefixed (MP4) streams are copied: an Annex B
/// stream would need its framing rewritten, which is not a copy.
pub(crate) fn nal_length_size(codec: NativeVideoCodec, extradata: &[u8]) -> Result<usize, String> {
    let annex_b = || "the source's parameter sets are not in MP4 form".to_owned();
    match codec {
        NativeVideoCodec::H264 if extradata.len() >= 7 && extradata[0] == 1 => {
            Ok(usize::from(extradata[4] & 0b11) + 1)
        }
        NativeVideoCodec::Hevc if extradata.len() >= 23 && extradata[0] == 1 => {
            Ok(usize::from(extradata[21] & 0b11) + 1)
        }
        NativeVideoCodec::H264 | NativeVideoCodec::Hevc => Err(annex_b()),
        other => Err(format!(
            "{} is not copied; only H.264 and HEVC are",
            other.as_str()
        )),
    }
}

/// Whether a length-prefixed access unit holds an IDR picture.
pub(crate) fn is_idr(codec: NativeVideoCodec, length_size: usize, data: &[u8]) -> bool {
    let mut at = 0_usize;
    while at + length_size < data.len() {
        let length = data[at..at + length_size]
            .iter()
            .fold(0_usize, |value, byte| (value << 8) | usize::from(*byte));
        let Some(&header) = data.get(at + length_size) else {
            return false;
        };
        let idr = match codec {
            NativeVideoCodec::H264 => header & 0x1f == 5,
            // IDR_W_RADL and IDR_N_LP. A CRA may be followed by pictures that refer to the previous
            // GOP, so it does not count.
            NativeVideoCodec::Hevc => matches!((header >> 1) & 0x3f, 19 | 20),
            _ => false,
        };
        if idr {
            return true;
        }
        at = at.saturating_add(length_size).saturating_add(length);
    }
    false
}

/// What a plan reads of a source's packets.
struct Scan {
    /// Every video packet, in decode order.
    video: Vec<ScannedPacket>,
    /// The span of the audio packets a copy would carry, with their time
    /// base; `None` when the source has no audio stream or no timestamped
    /// audio packet.
    audio: Option<AudioSpan>,
}

/// The timestamps a copy would carry of the source's audio.
#[derive(Clone, Copy)]
struct AudioSpan {
    first: i64,
    last: i64,
    /// Where the last packet ends: the latest presentation time plus its
    /// duration, and that duration.
    end: i64,
    last_duration: i64,
    time_base: AVRational,
}

/// Reads the source through once: every packet of video stream `index`,
/// and the timestamps of audio stream `audio`'s. Every other stream is
/// skipped by the demuxer, and `cancel` stops it between packets.
fn scan(
    path: &Path,
    index: usize,
    codec: NativeVideoCodec,
    length_size: usize,
    audio: Option<usize>,
    cancel: &CancelToken,
) -> Result<Scan, String> {
    let mut input = open(path)?;
    crate::decode::interrupt_on_cancel(&input, cancel);
    input.keep_only(
        &[Some(index), audio]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>(),
    );
    let audio_time_base =
        audio.and_then(|audio| input.streams().get(audio).map(|stream| stream.time_base));
    let mut packets = Vec::new();
    let mut audio_span: Option<(i64, i64, i64, i64)> = None;
    let cancelled = || "the export was cancelled while its copy was planned".to_owned();
    loop {
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let packet = match input.read_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(_) if cancel.is_cancelled() => return Err(cancelled()),
            Err(error) => return Err(format!("cannot read the source: {error}")),
        };
        // A watcher outside (a host's) sees the scan alive, not blocked, packet by packet.
        crate::recovery::report_decode_progress();
        let stream = usize::try_from(packet.stream_index).ok();
        if stream.is_some() && stream == audio {
            for time in [packet.pts, packet.dts] {
                if time != ffi::AV_NOPTS_VALUE {
                    let span = audio_span.get_or_insert((time, time, time, 0));
                    span.0 = span.0.min(time);
                    span.1 = span.1.max(time);
                }
            }
            if packet.pts != ffi::AV_NOPTS_VALUE {
                let end = packet.pts.saturating_add(packet.duration.max(0));
                if let Some(span) = audio_span.as_mut()
                    && end >= span.2
                {
                    span.2 = end;
                    span.3 = packet.duration.max(0);
                }
            }
            continue;
        }
        if stream != Some(index) {
            continue;
        }
        if packet.pts == ffi::AV_NOPTS_VALUE {
            return Err("a source video packet has no presentation time".to_owned());
        }
        let key = packet.flags & i32::try_from(ffi::AV_PKT_FLAG_KEY).unwrap_or(0) != 0;
        packets.push(ScannedPacket {
            pts: packet.pts,
            dts: if packet.dts == ffi::AV_NOPTS_VALUE {
                packet.pts
            } else {
                packet.dts
            },
            idr: key
                && is_idr(
                    codec,
                    length_size,
                    &alpha2_ffmpeg_bridge::packet_bytes(&packet),
                ),
            new_description: alpha2_ffmpeg_bridge::packet_has_side_data(
                &packet,
                ffi::AV_PKT_DATA_NEW_EXTRADATA,
            ),
        });
    }
    Ok(Scan {
        video: packets,
        audio: audio_span.zip(audio_time_base).map(
            |((first, last, end, last_duration), time_base)| AudioSpan {
                first,
                last,
                end,
                last_duration,
                time_base,
            },
        ),
    })
}

/// How far outside the copied video's span copied audio may reach, in
/// ticks: more than an AAC encoder's priming before it and the last video
/// frame's duration after it, together.
const COPIED_AUDIO_SLACK: i64 = alpha2_time::TICKS_PER_SECOND;

/// Whether the audio a copy would carry lands where the render's mix does:
/// every timestamp, moved by the shift that puts the video's first frame at
/// its place in the output, within the output's video, give or take
/// [`COPIED_AUDIO_SLACK`]. The timestamps are the file's own, unchecked by
/// the demuxer (a fragment's `tfdt` is any 64-bit value), so the shift is
/// checked too: audio that cannot be moved, or would play elsewhere, is not
/// the mix.
fn check_audio_span(
    span: Option<AudioSpan>,
    origin: i64,
    video_time_base: AVRational,
    request: &PacketCopyRequest<'_>,
) -> Result<(), String> {
    let Some(span) = span else {
        return Ok(());
    };
    let out_of_range = || {
        "the source's audio timestamps lie outside its video, so copying them would not play the render's mix".to_owned()
    };
    let output_start = request.output_start.ticks();
    let video_ticks = request
        .frame_times
        .last()
        .map_or(0, |last| last.ticks() - request.frame_times[0].ticks());
    let earliest = output_start.saturating_sub(COPIED_AUDIO_SLACK);
    let latest = output_start
        .saturating_add(video_ticks)
        .saturating_add(COPIED_AUDIO_SLACK);
    let shift_by = av_rescale_q(
        origin - av_rescale_q(output_start, tick_base(), video_time_base),
        video_time_base,
        span.time_base,
    );
    for time in [span.first, span.last] {
        let moved = time.checked_sub(shift_by).ok_or_else(out_of_range)?;
        let ticks = av_rescale_q(moved, span.time_base, tick_base());
        if ticks == ffi::AV_NOPTS_VALUE || !(earliest..=latest).contains(&ticks) {
            return Err(out_of_range());
        }
    }
    // A render mixes exactly the range, so the copied audio may not run on past the last frame's
    // end by more than its own last packet, which a frame cannot be cut from.
    let frames = i64::try_from(request.frame_times.len()).unwrap_or(i64::MAX);
    let frame_ticks = if frames > 1 {
        video_ticks / (frames - 1)
    } else {
        0
    };
    let video_end = output_start
        .saturating_add(video_ticks)
        .saturating_add(frame_ticks);
    let end = av_rescale_q(
        span.end.checked_sub(shift_by).ok_or_else(out_of_range)?,
        span.time_base,
        tick_base(),
    );
    let last_packet = av_rescale_q(span.last_duration, span.time_base, tick_base());
    if end > video_end.saturating_add(last_packet) {
        return Err(format!(
            "the source's audio outlasts its video by {} ms, and a render mixes only the range",
            (end - video_end) * 1_000 / alpha2_time::TICKS_PER_SECOND
        ));
    }
    Ok(())
}

/// Writes `copy`'s runs, back to back, into a new `container` file at
/// `output`, with `audio`. The runs were planned for `container`
/// ([`PacketCopyRequest::container`]).
///
/// The output is transactional, as every export's is: it is written beside
/// `output` and renamed into place only when complete, and a failure or a
/// cancel removes it. Encoded audio is interleaved with the video as it is
/// written, a little ahead of it, so neither stream piles up in the muxer.
/// `progress` is called with the video packets written.
///
/// # Errors
/// Returns an adapter failure for an existing output, a partial a running export holds, a read
/// or write failure, a failed mix, audio the plan does not copy, or a cancel.
pub fn copy_packets(
    copy: &PacketCopy,
    output: &Path,
    container: alpha2_media::Container,
    audio: CopyAudio<'_>,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(u64),
) -> Result<PacketCopyReport, EncodeError> {
    let first = &copy.segments[0];
    let input = crate::decode::open_input(&first.source).map_err(encode_adapter_error)?;
    let video_stream = {
        let source = &input.streams()[first.video_stream];
        VideoStream {
            parameters: source.codecpar().clone(),
            time_base: source.time_base,
            copied: true,
        }
    };
    let audio_stream = match &audio {
        CopyAudio::None => None,
        CopyAudio::Source => {
            if !copy.copies_audio() {
                return Err(EncodeError::Adapter("the plan copies no audio".into()));
            }
            let index = first.audio_stream.unwrap_or(0);
            let source = &input.streams()[index];
            Some(AudioStream::Copy {
                parameters: source.codecpar().clone(),
                time_base: source.time_base,
            })
        }
        CopyAudio::Encode { config, .. } => Some(AudioStream::Encode(*config)),
    };
    drop(input);
    let copied_audio = matches!(audio, CopyAudio::Source);
    let mut writer = RunWriter {
        muxer: Muxer::open_streams(output, container, video_stream, audio_stream)?,
        mix: match audio {
            CopyAudio::Encode { feed, .. } => Some(MixWriter {
                feed,
                covered: 0,
                done: false,
            }),
            CopyAudio::None | CopyAudio::Source => None,
        },
        stages: crate::EncodeStageTimings::default(),
        report: PacketCopyReport::default(),
    };
    for segment in &copy.segments {
        writer.write_run(segment, copied_audio, cancel, progress)?;
    }
    if writer.report.video_packets != copy.video_packets() {
        return Err(EncodeError::Adapter(format!(
            "the copy wrote {} video packets of the planned {}",
            writer.report.video_packets,
            copy.video_packets()
        )));
    }
    let RunWriter {
        mut muxer,
        mut mix,
        mut stages,
        mut report,
    } = writer;
    if let Some(mix) = mix.as_mut() {
        mix.write_until(i64::MAX, &mut muxer, &mut stages, &mut report)?;
    }
    muxer.finish_audio()?;
    muxer.write_ready_audio(true, &mut stages)?;
    muxer.close()?;
    Ok(report)
}

/// The output a copy writes its runs into, and the mix it interleaves.
struct RunWriter<'a> {
    muxer: Muxer,
    mix: Option<MixWriter<'a>>,
    stages: crate::EncodeStageTimings,
    report: PacketCopyReport,
}

impl RunWriter<'_> {
    /// Copies one run's packets, and its source's audio packets when
    /// `copied_audio` (only ever for a single whole file).
    fn write_run(
        &mut self,
        run: &PacketCopyPlan,
        copied_audio: bool,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(u64),
    ) -> Result<(), EncodeError> {
        let mut input = crate::decode::open_input(&run.source).map_err(encode_adapter_error)?;
        crate::decode::interrupt_on_cancel(&input, cancel);
        let audio_stream = copied_audio.then_some(run.audio_stream).flatten();
        input.keep_only(
            &[Some(run.video_stream), audio_stream]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>(),
        );
        let shift_by = run.origin - run.offset;
        // A run after the first packet is sought to (its first packet is an indexed IDR). Copied
        // audio only travels with a whole file, which starts at the first packet.
        let mut video_position = if run.first_packet > 0 {
            let stream = i32::try_from(run.video_stream)
                .map_err(|_| EncodeError::Adapter("video stream index is out of range".into()))?;
            input
                .seek(
                    stream,
                    run.first_timestamps.1,
                    i32::try_from(ffi::AVSEEK_FLAG_BACKWARD).unwrap_or(0),
                )
                .map_err(|error| {
                    if cancel.is_cancelled() {
                        EncodeError::Cancelled
                    } else {
                        encode_adapter_error_at("seeking to the run", error)
                    }
                })?;
            None
        } else {
            Some(0_u64)
        };
        while let Some(mut packet) = input.read_packet().map_err(|error| {
            if cancel.is_cancelled() {
                EncodeError::Cancelled
            } else {
                encode_adapter_error(error)
            }
        })? {
            if cancel.is_cancelled() {
                return Err(EncodeError::Cancelled);
            }
            let stream = usize::try_from(packet.stream_index).ok();
            if stream == Some(run.video_stream) {
                let position = match video_position {
                    Some(position) => position,
                    None if (packet.pts, packet.dts) == run.first_timestamps => run.first_packet,
                    None => continue,
                };
                video_position = Some(position + 1);
                if position < run.first_packet {
                    continue;
                }
                if position > run.last_packet {
                    // Copied audio only travels with the whole video.
                    break;
                }
                packet.set_pts(shift(packet.pts, shift_by)?);
                packet.set_dts(shift(packet.dts, shift_by)?);
                packet.set_pos(-1);
                if let Some(mix) = self.mix.as_mut() {
                    let at = av_rescale_q(packet.dts, run.video_time_base, tick_base());
                    mix.write_until(
                        at.saturating_add(MIX_LEAD),
                        &mut self.muxer,
                        &mut self.stages,
                        &mut self.report,
                    )?;
                }
                self.report.bytes = self
                    .report
                    .bytes
                    .saturating_add(u64::try_from(packet.size).unwrap_or(0));
                self.muxer.write_video(&mut packet, run.video_time_base)?;
                self.report.video_packets += 1;
                progress(self.report.video_packets);
            } else if stream.is_some() && stream == audio_stream {
                let source_base = input.streams()[run.audio_stream.unwrap_or(0)].time_base;
                // The same zero as the video's; audio keeps its own offset from it, priming
                // included.
                let origin = av_rescale_q(shift_by, run.video_time_base, source_base);
                packet.set_pts(shift(packet.pts, origin)?);
                packet.set_dts(shift(packet.dts, origin)?);
                packet.set_pos(-1);
                self.report.bytes = self
                    .report
                    .bytes
                    .saturating_add(u64::try_from(packet.size).unwrap_or(0));
                self.muxer.write_copied_audio(&mut packet)?;
                self.report.audio_packets += 1;
            }
        }
        Ok(())
    }
}

/// How far ahead of the video the encoded mix is written, in ticks: enough
/// that the muxer always has audio to interleave, little enough that neither
/// stream piles up in it.
const MIX_LEAD: i64 = alpha2_time::TICKS_PER_SECOND / 10;

/// The export's mix, written into the muxer as the video advances.
struct MixWriter<'a> {
    feed: &'a mut dyn AudioBlockFeed,
    /// Output time the mix written so far reaches, in ticks.
    covered: i64,
    done: bool,
}

impl MixWriter<'_> {
    /// Writes blocks until the mix reaches `target`, or ends.
    fn write_until(
        &mut self,
        target: i64,
        muxer: &mut Muxer,
        stages: &mut crate::EncodeStageTimings,
        report: &mut PacketCopyReport,
    ) -> Result<(), EncodeError> {
        while !self.done && self.covered < target {
            let Some(block) = self.feed.next_block().map_err(EncodeError::Adapter)? else {
                self.done = true;
                break;
            };
            let frames = block.channels.first().map_or(0, Vec::len);
            muxer.write_audio(&block, stages)?;
            report.encoded_audio_frames = report
                .encoded_audio_frames
                .saturating_add(u64::try_from(frames).unwrap_or(u64::MAX));
            self.covered = i64::try_from(report.encoded_audio_frames)
                .unwrap_or(i64::MAX)
                .saturating_mul(alpha2_time::TICKS_PER_SECOND)
                .div_euclid(i64::from(block.sample_rate.max(1)));
        }
        Ok(())
    }
}

/// `value` moved back by `origin`, keeping a missing timestamp missing.
/// The plan checked the copied stream's timestamps; this still refuses one
/// that cannot be moved rather than overflowing.
fn shift(value: i64, origin: i64) -> Result<i64, EncodeError> {
    if value == ffi::AV_NOPTS_VALUE {
        return Ok(value);
    }
    value
        .checked_sub(origin)
        .filter(|moved| *moved != ffi::AV_NOPTS_VALUE)
        .ok_or_else(|| {
            EncodeError::Adapter(format!(
                "a source packet's timestamp {value} cannot be moved by {origin}"
            ))
        })
}

//! Container probing and the native audio/video decoders.

use std::collections::VecDeque;
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::audio_timeline::SourceTimeline;
use crate::recovery::{DecodeDiagnostics, DecodePolicy, Recovery, WorkBudget, submit_packet};

use crate::convert::{
    adapter_error, codec_thread_budget, color_metadata, convert_video_frame, declared_sar,
    frame_color_metadata, guess_sar, orientation_from_display, reject_unused_decode_options,
    stream_rotation, tick_base, timestamp, video_decoder_options,
};
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
use crate::video_frame::{HardwareDecoderSpec, HardwareOpenError};
use alpha2_ffmpeg_bridge::{DecodedFrame, DemuxInput, OpenInputError};
use alpha2_media::{
    AudioBlock, CancelToken, ColorMetadata, DecodeError, DecodeEvent, DecodeRequest, DecodeResult,
    DecodedVideoFrame, Decoder, MAX_AUDIO_CHANNELS, Orientation, SampleAspectRatio, SourceAlpha,
    Speaker, SpeakerLayout, StreamInfo, StreamKind,
};
use alpha2_time::{FrameRate, MediaTime, TICKS_PER_SECOND};
use rsmpeg::avcodec::{AVCodec, AVCodecContext, AVCodecRef, AVPacket};
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
use rsmpeg::avutil::AVHWDeviceContext;
use rsmpeg::avutil::{AVChannelLayout, AVFrame, AVRational, av_rescale_q, av_rescale_q_rnd, ra};
use rsmpeg::error::RsmpegError;
use rsmpeg::ffi;
use rsmpeg::swresample::SwrContext;

/// Container-level probe and deterministic stream selection.
#[derive(Clone, Debug, PartialEq)]
pub struct ProbeResult {
    /// Demuxer name.
    pub format: String,
    /// Container duration from the first presentation to the end of the last; media
    /// ends at `start + duration`.
    pub duration: Option<MediaTime>,
    /// The container's first presentation on the media's own timeline: zero for most files, later
    /// for a transport stream or a delayed start.
    pub start: Option<MediaTime>,
    /// Every audio/video stream in demuxer order, other streams left out; look one up by
    /// [`StreamInfo::index`], not by position.
    pub streams: Vec<StreamInfo>,
    /// The `AVStream` index of the video stream a reader shows (never an attached picture), `None`
    /// when no video stream decodes.
    pub selected_video: Option<usize>,
    /// The `AVStream` index of the audio stream a reader plays, `None` when none decodes.
    pub selected_audio: Option<usize>,
}

impl ProbeResult {
    /// The entry of `streams` with `AVStream` index `index`.
    #[must_use]
    pub fn stream(&self, index: usize) -> Option<&StreamInfo> {
        self.streams.iter().find(|stream| stream.index == index)
    }

    /// The selected video stream's entry.
    #[must_use]
    pub fn selected_video_stream(&self) -> Option<&StreamInfo> {
        self.selected_video.and_then(|index| self.stream(index))
    }

    /// The selected audio stream's entry.
    #[must_use]
    pub fn selected_audio_stream(&self) -> Option<&StreamInfo> {
        self.selected_audio.and_then(|index| self.stream(index))
    }
}

/// Opens a local resource using the system `FFmpeg` 8 shared libraries.
///
/// # Errors
/// Returns a platform-neutral adapter error for invalid paths or demux failures.
pub fn probe(path: impl AsRef<Path>) -> Result<ProbeResult, DecodeError> {
    let input = open_input(path.as_ref())?;
    probe_context(&input, path.as_ref())
}

/// Visits each packet time (seconds) of `AVStream` index `stream` in file order until `visit`
/// returns false. Falls back to decode time; packets with neither are skipped. Nothing is decoded.
///
/// # Errors
/// Returns why the file cannot be opened or read.
pub fn visit_packet_times(
    path: impl AsRef<Path>,
    stream: usize,
    mut visit: impl FnMut(f64) -> bool,
) -> Result<(), DecodeError> {
    let mut input = open_input(path.as_ref())?;
    let time_base = input
        .streams()
        .get(stream)
        .map(|found| found.time_base)
        .ok_or_else(|| DecodeError::Unsupported(format!("the file has no stream {stream}")))?;
    input.discard_all_but(stream);
    while let Some(packet) = input.read_packet().map_err(adapter_error)? {
        if usize::try_from(packet.stream_index).ok() != Some(stream) {
            continue;
        }
        let time = [packet.pts, packet.dts]
            .into_iter()
            .find(|time| *time != ffi::AV_NOPTS_VALUE);
        let Some(time) = time else { continue };
        #[allow(clippy::cast_precision_loss)] // A packet time is far below 2^52 time-base units.
        let seconds = time as f64 * f64::from(time_base.num) / f64::from(time_base.den);
        if !visit(seconds) {
            break;
        }
    }
    Ok(())
}

/// Software video decoder with exact timestamp conversion and drain semantics.
pub struct NativeDecoder {
    pub(crate) input: DemuxInput,
    pub(crate) streams: Vec<StreamInfo>,
    pub(crate) container_duration: Option<MediaTime>,
    pub(crate) video_index: usize,
    pub(crate) time_base: AVRational,
    pub(crate) decoder: AVCodecContext,
    pub(crate) orientation: Orientation,
    /// The probed pixel aspect ratio: the container's declaration, else the codec parameters'.
    pub(crate) sample_aspect_ratio: SampleAspectRatio,
    /// The color the stream's tags describe, as probed.
    pub(crate) color: Option<ColorMetadata>,
    /// The project's declared color, which overrides every tag ([`Self::declare_color`]).
    pub(crate) declared_color: Option<ColorMetadata>,
    pub(crate) video_scaler: crate::convert::FrameScaler,
    pub(crate) pending: VecDeque<DecodedFrame>,
    pub(crate) demux_eof: bool,
    pub(crate) decoder_flushed: bool,
    pub(crate) stages: DecodeStageTimings,
    /// When the stream's first frame is presented, once a seek from its first packet decoded it.
    first_video_pts: Option<MediaTime>,
    recovery: Recovery,
    /// How late a frame's timestamp may be and still be shown at a time
    /// ([`Self::presentation_deadline`]).
    slack: MediaTime,
    /// Packets read ahead for [`Self::slack`], consumed before the demuxer until a seek drops them.
    prefetched: Prefetched,
    /// What this decoder does not decode of its stream (an alpha channel, extra layers).
    dropped_layer: Option<DroppedLayer>,
}

/// Something a decoder leaves out of its stream (the frames still decode), for reports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DroppedLayer {
    /// `alpha-dropped` or `extra-layers-dropped`.
    pub code: &'static str,
    /// Human-readable explanation.
    pub reason: String,
}

#[cfg(test)]
thread_local! {
    /// Forces software VP9 opens on this thread onto `FFmpeg`'s own `vp9`, as without libvpx.
    pub(crate) static WITHOUT_LIBVPX_VP9: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Packets read ahead from the demuxer, consumed before it until a seek drops them.
type Prefetched = VecDeque<Result<Option<AVPacket>, RsmpegError>>;

/// Reads stream `index`'s first packets (up to [`alpha2_time::ROUNDING_SCAN_FRAMES`] or
/// [`alpha2_time::ROUNDING_SCAN_BYTES`]) and returns them with their presentation timestamps.
///
/// The decode replays these packets first, so the input never seeks back and a read failure
/// surfaces where the decode meets it.
fn read_first_packets(input: &mut DemuxInput, index: usize) -> (Prefetched, Vec<i64>) {
    let mut read = VecDeque::new();
    let mut timestamps = Vec::with_capacity(alpha2_time::ROUNDING_SCAN_FRAMES);
    let (mut packets, mut bytes) = (0, 0_u64);
    while packets < alpha2_time::ROUNDING_SCAN_FRAMES && bytes < alpha2_time::ROUNDING_SCAN_BYTES {
        let packet = input.read_packet();
        let more = match &packet {
            Ok(Some(packet)) => {
                if usize::try_from(packet.stream_index).ok() == Some(index) {
                    packets += 1;
                    bytes = bytes.saturating_add(u64::try_from(packet.size).unwrap_or(0));
                    if packet.pts != ffi::AV_NOPTS_VALUE {
                        timestamps.push(packet.pts);
                    }
                }
                true
            }
            Ok(None) | Err(_) => false,
        };
        read.push_back(packet);
        if !more {
            break;
        }
    }
    (read, timestamps)
}

/// The first packets of stream `video_index` and the presentation slack their timestamps show.
fn read_presentation_slack(
    input: &mut DemuxInput,
    video_index: usize,
    time_base: AVRational,
) -> (Prefetched, MediaTime) {
    let (read, timestamps) = read_first_packets(input, video_index);
    let slack = alpha2_time::presentation_slack(
        i64::from(time_base.num),
        i64::from(time_base.den),
        alpha2_time::rounding_quantum(&timestamps),
    );
    (read, slack)
}

/// Where one video decode session's wall time went, split so hardware decode is told
/// apart from mere submission.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecodeStageTimings {
    /// `av_read_frame`: container demux and file I/O.
    pub demux: Duration,
    /// `avcodec_send_packet`: handing a packet to the codec.
    pub send_packet: Duration,
    /// `avcodec_receive_frame`: producing one picture. A hardware session waits here for `FFmpeg`'s
    /// synchronization, so this separates decode from submission.
    pub receive_frame: Duration,
    /// Turning the picture into a host frame: pixel conversion (software) or
    /// surface metadata (hardware).
    pub convert: Duration,
    /// `av_seek_frame` and the codec flush that follows it.
    pub seek: Duration,
}

impl DecodeStageTimings {
    /// Adds another session's spans to these, saturating on overflow.
    #[must_use]
    pub fn merged(self, other: Self) -> Self {
        Self {
            demux: self.demux.saturating_add(other.demux),
            send_packet: self.send_packet.saturating_add(other.send_packet),
            receive_frame: self.receive_frame.saturating_add(other.receive_frame),
            convert: self.convert.saturating_add(other.convert),
            seek: self.seek.saturating_add(other.seek),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputPosition {
    /// Opened, with no read or seek since.
    Opened,
    Moved,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AudioContinuity {
    Continuous,
    Damaged,
}

/// Streaming `FFmpeg` audio decoder normalized to project-rate planar `f32`.
///
/// Blocks keep the stream's channel order ([`NativeAudioDecoder::speakers`]). `swresample` converts
/// each frame to `f32` at its own rate, rematrixing only a frame whose layout differs from the
/// stream's; the source timeline places it by timestamp and `alpha2_audio::Resampler` (shared with
/// the browser host) brings it to the project rate.
pub struct NativeAudioDecoder {
    pub(crate) path: PathBuf,
    pub(crate) input: DemuxInput,
    input_position: InputPosition,
    pub(crate) streams: Vec<StreamInfo>,
    pub(crate) container_duration: Option<MediaTime>,
    pub(crate) audio_index: usize,
    pub(crate) time_base: AVRational,
    pub(crate) decoder: AVCodecContext,
    /// Converts sample format, and the layout of a frame unlike the stream's; never the rate.
    converter: Option<SwrContext>,
    pub(crate) sample_rate: u32,
    pub(crate) channel_count: usize,
    /// The speakers the stream declares, or its count's default.
    speakers: SpeakerLayout,
    /// Whether the stream declared no speaker and `speakers` is assumed.
    speakers_assumed: bool,
    /// `speakers` as `FFmpeg`'s layout: what every frame is converted to.
    output_layout: AVChannelLayout,
    /// The decoded source, placed and not yet resampled.
    timeline: SourceTimeline,
    pub(crate) pending: Vec<VecDeque<f32>>,
    /// Source-timeline index, at the output rate, of the first pending sample; `None` until a
    /// frame's timestamp places it.
    pub(crate) next_sample: Option<i64>,
    pub(crate) block_frames: usize,
    pub(crate) demux_eof: bool,
    pub(crate) decoder_flushed: bool,
    recovery: Recovery,
    repair_audio: AudioContinuity,
    /// How a demuxer's made-up timestamps (raw ADTS) compare with the audio; `None` when
    /// the container has real ones.
    made_up: Option<MadeUpTimestamps>,
    /// The source rate and frame grid of the stream's first timestamps
    /// ([`alpha2_time::audio_frame_grid`]), used to place the first frame after an open or seek.
    grid: Option<(u32, alpha2_time::AudioFrameGrid)>,
    /// Packets read ahead for [`Self::grid`], consumed before the demuxer until a seek drops them.
    prefetched: Prefetched,
    /// Frames placed since open; placing one counts as progress before the target is reached.
    frames_placed: u64,
    /// The codec's seek pre-roll, in ticks.
    seek_preroll: i64,
}

/// Timestamps a demuxer made up by counting each packet's samples at one rate.
struct MadeUpTimestamps {
    /// The stream's first rate, which every packet is counted at.
    rate: u32,
    /// How far the timestamps have run ahead of the decoded frames since the last restart,
    /// in the stream's time base.
    drift: i64,
    /// Whether a frame came at another rate, after which the timestamps no longer match the audio.
    rate_changed: bool,
}

/// How far before a seek target decoding restarts.
///
/// A decoder resuming mid-stream lacks history (AAC's first frame misses the previous overlap), so
/// decoding from 100 ms early and discarding up to the target matches a straight decode.
const AUDIO_SEEK_PREROLL_TICKS: i64 = TICKS_PER_SECOND / 10;

/// How far before a seek target decoding restarts in Opus (400 ms).
///
/// RFC 7845 asks for at least 80 ms, but the decoder converges slowly: the target's samples are
/// still 2e-2 off from 100 ms, 1e-4 from 250 ms and under 1e-6 from 400 ms.
const OPUS_SEEK_PREROLL_TICKS: i64 = TICKS_PER_SECOND * 2 / 5;

impl NativeAudioDecoder {
    /// Selects strict failure (default) or bounded, reported recovery.
    pub fn set_decode_policy(&mut self, policy: DecodePolicy) {
        self.recovery.policy = policy;
    }

    /// Receives each diagnostic before recovery continues, including before errors.
    pub fn set_decode_reporter(
        &mut self,
        reporter: impl Fn(crate::DecodeDiagnostic) + Send + Sync + 'static,
    ) {
        self.recovery.reporter = Some(Box::new(reporter));
    }

    /// Cumulative corruption and concealment diagnostics, including seeks.
    #[must_use]
    pub fn decode_diagnostics(&self) -> &DecodeDiagnostics {
        &self.recovery.diagnostics
    }

    /// Decodes the selected audio stream through the linked `FFmpeg` 8 libraries.
    ///
    /// # Errors
    /// Returns a platform-neutral error for invalid rates, missing streams, unsupported layouts,
    /// codec failures, or malformed decoded audio.
    pub fn open(path: impl AsRef<Path>, project_sample_rate: u32) -> Result<Self, DecodeError> {
        if project_sample_rate == 0 {
            return Err(DecodeError::OutOfRange);
        }
        let path = path.as_ref().to_owned();
        let mut input = open_input(&path)?;
        let probe = probe_context(&input, &path)?;
        let selected = probe.selected_audio.ok_or_else(|| {
            DecodeError::Unsupported("source has no audio stream this runtime decodes".into())
        })?;
        let stream = probe
            .stream(selected)
            .ok_or_else(|| DecodeError::Adapter("selected audio stream disappeared".into()))?;
        check_source_sample_rate(i64::from(stream.sample_rate.unwrap_or(0)))?;
        let source_channels =
            usize::try_from(stream.channels.unwrap_or(0)).map_err(|_| DecodeError::OutOfRange)?;
        if source_channels == 0 {
            return Err(DecodeError::Unsupported(
                "audio stream declares no channels".into(),
            ));
        }
        check_audio_channels(source_channels)?;
        let decoder = open_audio_decoder(&mut input, selected)?;
        let (prefetched, timestamps) = read_first_packets(&mut input, selected);
        let stream = input
            .streams()
            .get(selected)
            .ok_or(DecodeError::OutOfRange)?;
        let time_base = stream.time_base;
        let seek_preroll = if stream.codecpar().codec_id == ffi::AV_CODEC_ID_OPUS {
            OPUS_SEEK_PREROLL_TICKS
        } else {
            AUDIO_SEEK_PREROLL_TICKS
        };
        // ADTS has no timestamps: the `aac` demuxer counts each packet as `frame_size` samples at
        // the starting rate, whatever rate a later header names.
        let made_up = (input.iformat().name() == c"aac")
            .then(|| u32::try_from(stream.codecpar().sample_rate).ok())
            .flatten()
            .filter(|rate| *rate > 0)
            .map(|rate| MadeUpTimestamps {
                rate,
                drift: 0,
                rate_changed: false,
            });
        // Made-up timestamps count samples exactly, so there is no rounding grid.
        let grid = u32::try_from(stream.codecpar().sample_rate)
            .ok()
            .filter(|_| made_up.is_none())
            .and_then(|rate| {
                alpha2_time::audio_frame_grid(
                    &timestamps,
                    i64::from(time_base.num),
                    i64::from(time_base.den),
                    rate,
                )
                .map(|grid| (rate, grid))
            });
        // Channels keep the stream's layout; the engine folds them by speaker. The layout comes
        // from the decoder, else the container, else the count's default.
        let declared = [decoder.ch_layout(), stream.codecpar().ch_layout()]
            .into_iter()
            .find(|layout| {
                layout.order != ffi::AV_CHANNEL_ORDER_UNSPEC
                    && usize::try_from(layout.nb_channels).ok() == Some(source_channels)
            })
            .map_or_else(
                || vec![Speaker::Unnamed; source_channels],
                |layout| layout_speakers(&layout),
            );
        let (speakers, speakers_assumed) = SpeakerLayout::declared_or_default(&declared)
            .ok_or_else(|| unsupported_channels(source_channels))?;
        let channel_count = speakers.channels();
        let output_layout = ffmpeg_layout(&speakers)?;
        Ok(Self {
            path,
            input,
            input_position: InputPosition::Opened,
            container_duration: probe.duration,
            streams: probe.streams,
            audio_index: selected,
            time_base,
            decoder,
            converter: None,
            sample_rate: project_sample_rate,
            channel_count,
            speakers,
            speakers_assumed,
            output_layout,
            timeline: SourceTimeline::new(project_sample_rate, channel_count, None),
            pending: (0..channel_count).map(|_| VecDeque::new()).collect(),
            next_sample: None,
            block_frames: 1_024,
            demux_eof: false,
            decoder_flushed: false,
            recovery: Recovery::new(time_base),
            repair_audio: AudioContinuity::Continuous,
            made_up,
            grid,
            prefetched,
            frames_placed: 0,
            seek_preroll,
        })
    }

    /// Sets the maximum observable block size returned by `decode_next`.
    ///
    /// # Errors
    /// Returns [`DecodeError::OutOfRange`] for zero frames.
    pub fn set_block_frames(&mut self, frames: usize) -> Result<(), DecodeError> {
        if frames == 0 {
            return Err(DecodeError::OutOfRange);
        }
        self.block_frames = frames;
        Ok(())
    }

    /// Channels of every block this decoder returns: the source's, 1 to [`MAX_AUDIO_CHANNELS`].
    #[must_use]
    pub const fn channels(&self) -> usize {
        self.channel_count
    }

    /// The speaker each plane feeds: the stream's declared layout, or its count's default.
    #[must_use]
    pub const fn speakers(&self) -> SpeakerLayout {
        self.speakers
    }

    /// Whether the stream declares no layout, so [`Self::speakers`] is a default.
    #[must_use]
    pub const fn speakers_assumed(&self) -> bool {
        self.speakers_assumed
    }

    /// Effective end of the selected audio source.
    ///
    /// Containers often declare a slightly longer duration than the stream (priming,
    /// time-base rounding). Projects authored in a browser use the container duration, so it
    /// is kept as the source boundary.
    #[must_use]
    pub fn audio_duration(&self) -> Option<MediaTime> {
        effective_media_duration(
            self.streams
                .iter()
                .find(|stream| stream.index == self.audio_index)
                .and_then(|stream| stream.duration),
            self.container_duration,
        )
    }

    /// The selected stream's first timestamp in its time base, or zero when unknown.
    fn stream_start(&self) -> i64 {
        self.input
            .streams()
            .get(self.audio_index)
            .map(|stream| stream.start_time)
            .filter(|start| *start != ffi::AV_NOPTS_VALUE)
            .unwrap_or(0)
    }

    /// The most decoded samples per channel held at once since the last open or seek.
    #[cfg(test)]
    pub(crate) const fn peak_held_samples(&self) -> usize {
        self.timeline.peak_held()
    }

    /// Goes back to the stream's first packet. An untouched input replays [`Self::prefetched`]; any
    /// other is reopened, since a raw elementary stream cannot seek before its first packet.
    pub(crate) fn reset_to_start(&mut self) -> Result<(), DecodeError> {
        if self.input_position == InputPosition::Opened {
            self.decoder.flush_buffers();
        } else {
            let mut input = open_input(&self.path)?;
            self.decoder = open_audio_decoder(&mut input, self.audio_index)?;
            self.input = input;
            self.input_position = InputPosition::Opened;
            self.prefetched.clear();
        }
        self.restart(None);
        Ok(())
    }

    /// Forgets everything decoded; the next output is no earlier than `first`.
    fn restart(&mut self, first: Option<i64>) {
        self.converter = None;
        self.timeline = SourceTimeline::new(self.sample_rate, self.channel_count, first);
        self.pending.iter_mut().for_each(VecDeque::clear);
        self.next_sample = None;
        self.demux_eof = false;
        self.decoder_flushed = false;
        self.repair_audio = AudioContinuity::Continuous;
        self.recovery.reset_progress();
        if let Some(made_up) = &mut self.made_up {
            made_up.drift = 0;
        }
    }

    fn made_up_rate_changed(&self) -> bool {
        self.made_up
            .as_ref()
            .is_some_and(|made_up| made_up.rate_changed)
    }

    /// The real time of a frame whose demuxer made up `pts` by counting every earlier packet at the
    /// stream's first rate. Other streams return `pts` unchanged.
    fn true_timestamp(&mut self, pts: Option<i64>, rate: u32, samples: i32) -> Option<i64> {
        let time_base = self.time_base;
        let Some(made_up) = &mut self.made_up else {
            return pts;
        };
        let declared = made_up.rate;
        made_up.rate_changed |= rate != declared;
        let corrected = pts.map(|pts| pts.saturating_sub(made_up.drift));
        let (num, den) = (
            i128::from(time_base.num.max(1)),
            i128::from(time_base.den.max(1)),
        );
        let samples = i128::from(samples.max(0));
        // The demuxer's duration rounds down (`compute_pkt_fields`); the
        // frame's own rounds to nearest.
        let counted = samples * den / (i128::from(declared) * num);
        let lasted = (2 * samples * den + i128::from(rate) * num) / (2 * i128::from(rate) * num);
        made_up.drift = made_up
            .drift
            .saturating_add(i64::try_from(counted - lasted).unwrap_or(0));
        corrected
    }

    pub(crate) fn receive_audio_frame(&mut self) -> Result<bool, DecodeError> {
        let mut frame = match self.decoder.receive_frame() {
            Ok(frame) => frame,
            Err(RsmpegError::DecoderDrainError | RsmpegError::DecoderFlushedError) => {
                return Ok(false);
            }
            Err(error) => {
                self.recovery.error(error, "audio receive", None)?;
                self.repair_audio = AudioContinuity::Damaged;
                return Ok(true);
            }
        };
        if frame.flags & i32::try_from(ffi::AV_FRAME_FLAG_CORRUPT).expect("flag fits i32") != 0
            || frame.decode_error_flags != 0
        {
            self.recovery.corrupt("audio frame", Some(frame.pts))?;
            self.repair_audio = AudioContinuity::Damaged;
            return Ok(true);
        }
        let pts = Some(if frame.best_effort_timestamp == ffi::AV_NOPTS_VALUE {
            frame.pts
        } else {
            frame.best_effort_timestamp
        })
        .filter(|pts| *pts != ffi::AV_NOPTS_VALUE);
        if pts.is_some_and(|pts| !audio_timestamp_in_range(pts, self.time_base)) {
            self.recovery.corrupt("audio timestamp", pts)?;
            self.repair_audio = AudioContinuity::Damaged;
            return Ok(true);
        }
        self.recovery.progress(pts.unwrap_or(ffi::AV_NOPTS_VALUE))?;
        normalize_audio_frame_layout(&mut frame, &self.output_layout)?;
        // A decoder may change rate mid-stream, so check the frame's own rate.
        check_source_sample_rate(i64::from(frame.sample_rate))?;
        let rate = u32::try_from(frame.sample_rate).map_err(|_| DecodeError::OutOfRange)?;
        let pts = self.true_timestamp(pts, rate, frame.nb_samples);
        let planes = self.convert_audio_frame(&frame)?;
        self.place_audio(rate, pts, planes)?;
        Ok(true)
    }

    /// The frame as `f32` planes feeding [`Self::speakers`], at its own rate.
    fn convert_audio_frame(&mut self, frame: &AVFrame) -> Result<Vec<Vec<f32>>, DecodeError> {
        let mut packed = audio_output_frame(frame.sample_rate, &self.output_layout);
        let mut converter = match self.converter.take() {
            Some(converter) => converter,
            None => audio_converter(frame, &self.output_layout)?,
        };
        if let Err(error) = converter.convert_frame(Some(frame), &mut packed) {
            if error.raw_error() != Some(ffi::AVERROR_INPUT_CHANGED) {
                return Err(adapter_error(error));
            }
            converter = audio_converter(frame, &self.output_layout)?;
            packed = audio_output_frame(frame.sample_rate, &self.output_layout);
            converter
                .convert_frame(Some(frame), &mut packed)
                .map_err(adapter_error)?;
        }
        self.converter = Some(converter);
        // At one rate `swresample` holds nothing back: what goes in comes out.
        if packed.nb_samples != frame.nb_samples {
            return Err(DecodeError::Adapter(format!(
                "converting {} decoded audio samples gave {}",
                frame.nb_samples, packed.nb_samples
            )));
        }
        let mut planes = (0..self.channel_count)
            .map(|_| VecDeque::new())
            .collect::<Vec<_>>();
        append_interleaved_audio(&packed, &mut planes)?;
        Ok(planes.into_iter().map(Vec::from).collect())
    }

    /// Places one decoded frame where its timestamp says.
    ///
    /// A timestamp within a frame duration, 2 ms, or two time-base units of the previous end
    /// continues it, since containers round timestamps (Matroska to the millisecond) and the
    /// browser demuxer ignores sub-frame jumps. Further ahead is a silent gap; further back repeats
    /// samples, which are dropped. After damage the timestamp is trusted exactly.
    fn place_audio(
        &mut self,
        rate: u32,
        pts: Option<i64>,
        mut planes: Vec<Vec<f32>>,
    ) -> Result<(), DecodeError> {
        let rate_i32 = i32::try_from(rate).map_err(|_| DecodeError::OutOfRange)?;
        let at = pts.map(|pts| {
            av_rescale_q_rnd(
                pts,
                self.time_base,
                ra(1, rate_i32),
                ffi::AV_ROUND_NEAR_INF | ffi::AV_ROUND_PASS_MINMAX,
            )
        });
        let damaged = self.repair_audio == AudioContinuity::Damaged;
        self.repair_audio = AudioContinuity::Continuous;
        self.frames_placed = self.frames_placed.wrapping_add(1);
        let frames = i64::try_from(planes.first().map_or(0, Vec::len)).unwrap_or(i64::MAX);
        let Some(expected) = self.timeline.expected(rate) else {
            // Snap the first frame after an open or seek to the stream's grid, so it lands where
            // a straight decode puts it.
            let at = match (at, self.grid) {
                (Some(at), Some((grid_rate, grid))) if !damaged && grid_rate == rate => {
                    grid.place(at, frames)
                }
                (at, _) => at.unwrap_or(0),
            };
            return self.timeline.place(rate, at, planes);
        };
        let Some(at) = at else {
            if damaged {
                return Err(DecodeError::Adapter(
                    "cannot locate damaged audio gap without PTS".into(),
                ));
            }
            return self.timeline.place(rate, expected, planes);
        };
        let ahead = at
            .checked_sub(expected)
            .ok_or_else(|| DecodeError::Adapter("audio timestamps are out of range".into()))?;
        if damaged {
            if ahead < 0 || ahead > i64::from(rate) * 10 {
                return Err(DecodeError::Adapter(
                    "damaged audio gap is ambiguous or exceeds 10 seconds".into(),
                ));
            }
            if ahead > 0 {
                self.recovery.audio_gap(
                    self.timeline.output_at(rate, expected),
                    self.timeline.output_at(rate, at),
                    self.sample_rate,
                    "corrupt input",
                )?;
            }
            return self.timeline.place(rate, at, planes);
        }
        // One time-base unit in source samples, rounded up; 2 ms likewise.
        let den = i64::from(self.time_base.den.max(1));
        let unit = (i64::from(rate) * i64::from(self.time_base.num) + den - 1) / den;
        let tolerance = frames
            .max((i64::from(rate) + 499) / 500)
            .max(unit.saturating_mul(2));
        if ahead.abs() < tolerance {
            return self.timeline.place(rate, expected, planes);
        }
        if ahead > 0 {
            self.recovery.timestamp_gap(
                self.timeline.output_at(rate, expected),
                self.timeline.output_at(rate, at),
                self.sample_rate,
            );
            return self.timeline.place(rate, at, planes);
        }
        let repeated = usize::try_from(-ahead).unwrap_or(usize::MAX);
        self.recovery.timestamp_overlap(
            self.timeline.output_at(rate, at),
            self.timeline
                .output_at(rate, at.saturating_add(frames.min(-ahead))),
            self.sample_rate,
        );
        for plane in &mut planes {
            plane.drain(..repeated.min(plane.len()));
        }
        self.timeline.place(rate, expected, planes)
    }

    pub(crate) fn fill_pending(
        &mut self,
        frames: usize,
        cancel: &CancelToken,
    ) -> Result<(), DecodeError> {
        interrupt_on_cancel(&self.input, cancel);
        let mut budget = WorkBudget::new();
        let mut foreign = false;
        let mut placed = self.frames_placed;
        loop {
            let held = self.pending.first().map_or(0, VecDeque::len);
            if held >= frames {
                return Ok(());
            }
            // A placed frame is progress even before it reaches the output, so a long
            // seek decode is not cut short.
            if self.frames_placed != placed {
                placed = self.frames_placed;
                budget.progressed();
            }
            // Skipped packets of other streams are not progress: a demuxer that ignores the
            // discard still reads them.
            if std::mem::take(&mut foreign) {
                if cancel.is_cancelled() {
                    return Err(DecodeError::Cancelled);
                }
            } else {
                budget.step(cancel)?;
            }
            if let Some((start, _)) = self.timeline.produce(&mut self.pending, frames - held) {
                if held == 0 {
                    self.next_sample = Some(start);
                }
                continue;
            }
            if self.receive_audio_frame()? {
                continue;
            }
            if self.demux_eof {
                if !self.decoder_flushed {
                    match self.decoder.send_packet(None) {
                        Ok(()) | Err(RsmpegError::DecoderFlushedError) => {}
                        Err(RsmpegError::DecoderFullError) => continue,
                        Err(error) => {
                            self.recovery.error(error, "flush", None)?;
                            continue;
                        }
                    }
                    self.decoder_flushed = true;
                    continue;
                }
                if self.repair_audio == AudioContinuity::Damaged {
                    // Damage running to the end (Tolerant only) ends the stream at its last healthy
                    // frame; the damage is already reported.
                    self.recovery.damaged_tail("audio")?;
                    self.repair_audio = AudioContinuity::Continuous;
                }
                if !self.timeline.finished() {
                    self.timeline.finish();
                    continue;
                }
                return Ok(());
            }
            self.input_position = InputPosition::Moved;
            let packet = match self
                .prefetched
                .pop_front()
                .unwrap_or_else(|| self.input.read_packet())
            {
                Ok(Some(packet)) => packet,
                Ok(None) => {
                    self.demux_eof = true;
                    continue;
                }
                Err(error) => {
                    let error = demux_failure(error, cancel)?;
                    self.recovery.error(error, "audio demux", None)?;
                    self.repair_audio = AudioContinuity::Damaged;
                    continue;
                }
            };
            if usize::try_from(packet.stream_index).ok() != Some(self.audio_index) {
                foreign = true;
                continue;
            }
            if packet.flags & i32::try_from(ffi::AV_PKT_FLAG_CORRUPT).expect("flag fits i32") != 0 {
                self.recovery.corrupt("audio packet", Some(packet.pts))?;
                self.repair_audio = AudioContinuity::Damaged;
                continue;
            }
            submit_packet(
                |drain| {
                    if drain {
                        return self.receive_audio_frame();
                    }
                    match self.decoder.send_packet(Some(&packet)) {
                        Ok(()) => Ok(true),
                        Err(RsmpegError::DecoderFullError) => Ok(false),
                        Err(error) => {
                            self.recovery.error(error, "audio send", Some(packet.pts))?;
                            self.repair_audio = AudioContinuity::Damaged;
                            Ok(true)
                        }
                    }
                },
                &mut budget,
                cancel,
            )?;
        }
    }
}

impl Decoder for NativeAudioDecoder {
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn flush(&mut self) -> Result<(), DecodeError> {
        self.reset_to_start()
    }
    fn seek(
        &mut self,
        request: DecodeRequest,
        cancel: &CancelToken,
    ) -> Result<DecodeResult, DecodeError> {
        if cancel.is_cancelled() {
            return Err(DecodeError::Cancelled);
        }
        let target = request
            .time
            .ticks()
            .saturating_mul(i64::from(self.sample_rate))
            .div_euclid(TICKS_PER_SECOND);
        let timestamp = av_rescale_q(
            request.time.ticks().saturating_sub(self.seek_preroll),
            tick_base(),
            self.time_base,
        );
        // Raw elementary streams (MP3, ADTS AAC, AC-3) index from the first packet, so a pre-roll
        // seek before the start fails with -1 (EPERM); reopen instead. The same applies when a rate
        // change has put the made-up timestamps off the audio.
        let rate_changed = self.made_up_rate_changed();
        let from_start = timestamp <= self.stream_start() || rate_changed;
        if from_start {
            self.reset_to_start()?;
        } else {
            interrupt_on_cancel(&self.input, cancel);
            self.input_position = InputPosition::Moved;
            self.prefetched.clear();
            self.input
                .seek(
                    i32::try_from(self.audio_index).map_err(|_| DecodeError::OutOfRange)?,
                    timestamp,
                    i32::try_from(ffi::AVSEEK_FLAG_BACKWARD).expect("seek flag fits i32"),
                )
                .map_err(|error| seek_failure(error, cancel))?;
        }
        self.decoder.flush_buffers();
        self.restart(Some(target));
        let mut event = self.decode_next(cancel)?;
        if !from_start && !rate_changed && self.made_up_rate_changed() {
            // The seek landed past a rate change the made-up timestamps hide.
            self.reset_to_start()?;
            self.decoder.flush_buffers();
            self.restart(Some(target));
            event = self.decode_next(cancel)?;
        }
        if matches!(event, DecodeEvent::EndOfStream) {
            return Err(DecodeError::OutOfRange);
        }
        Ok(DecodeResult { request, event })
    }
    fn decode_next(&mut self, cancel: &CancelToken) -> Result<DecodeEvent, DecodeError> {
        if cancel.is_cancelled() {
            return Err(DecodeError::Cancelled);
        }
        self.fill_pending(self.block_frames, cancel)?;
        let frames = self
            .pending
            .first()
            .map_or(0, VecDeque::len)
            .min(self.block_frames);
        if frames == 0 {
            return Ok(DecodeEvent::EndOfStream);
        }
        let start_sample = self.next_sample.unwrap_or(0);
        let block = AudioBlock {
            start_sample,
            sample_rate: self.sample_rate,
            speakers: self.speakers,
            channels: self
                .pending
                .iter_mut()
                .map(|channel| channel.drain(..frames).collect())
                .collect(),
        };
        self.next_sample =
            Some(start_sample.saturating_add(i64::try_from(frames).unwrap_or(i64::MAX)));
        Ok(DecodeEvent::Audio(block))
    }
}

/// Opens the decoder for stream `stream_index` and discards every other stream.
fn open_audio_decoder(
    input: &mut DemuxInput,
    stream_index: usize,
) -> Result<AVCodecContext, DecodeError> {
    input.discard_all_but(stream_index);
    let stream = input
        .streams()
        .get(stream_index)
        .ok_or(DecodeError::OutOfRange)?;
    let parameters = stream.codecpar();
    let codec = AVCodec::find_decoder(parameters.codec_id)
        .ok_or_else(|| DecodeError::Unsupported("audio decoder is unavailable".into()))?;
    let mut decoder = AVCodecContext::new(&codec);
    decoder.apply_codecpar(&parameters).map_err(adapter_error)?;
    decoder.set_pkt_timebase(stream.time_base);
    decoder.open(None).map_err(adapter_error)?;
    Ok(decoder)
}

/// Maximum audio timestamp distance from zero (2^31 s). Sample counts at the fastest rate then stay
/// below 2^51, so placement cannot overflow; anything further is damaged input.
const AUDIO_TIMESTAMP_LIMIT_SECONDS: i128 = 1 << 31;

fn audio_timestamp_in_range(pts: i64, time_base: AVRational) -> bool {
    let (num, den) = (i128::from(time_base.num), i128::from(time_base.den));
    num > 0 && den > 0 && (i128::from(pts) * num).abs() <= AUDIO_TIMESTAMP_LIMIT_SECONDS * den
}

/// Accepted source sample rates (1 kHz to 768 kHz, the `alpha2_audio::Resampler` range).
pub const SOURCE_SAMPLE_RATES: std::ops::RangeInclusive<u32> = 1_000..=768_000;

fn check_source_sample_rate(rate: i64) -> Result<(), DecodeError> {
    if u32::try_from(rate).is_ok_and(|rate| SOURCE_SAMPLE_RATES.contains(&rate)) {
        return Ok(());
    }
    Err(DecodeError::Unsupported(format!(
        "the source's audio is {rate} Hz, and alpha2 reads audio from {} Hz to {} Hz",
        SOURCE_SAMPLE_RATES.start(),
        SOURCE_SAMPLE_RATES.end()
    )))
}

/// Converts a frame to packed `f32` in the stream's layout `output`, keeping its rate; a frame with
/// a different layout is rematrixed.
pub(crate) fn audio_converter(
    frame: &AVFrame,
    output: &AVChannelLayout,
) -> Result<SwrContext, DecodeError> {
    if frame.sample_rate <= 0 || frame.format == ffi::AV_SAMPLE_FMT_NONE {
        return Err(DecodeError::Adapter(
            "decoded audio frame has an incomplete format".into(),
        ));
    }
    // Borrowed, never cloned: rsmpeg 0.18's `AVChannelLayout::clone` copies into uninitialized
    // memory that `av_channel_layout_copy` first frees.
    let input_layout = frame.ch_layout();
    let mut converter = SwrContext::new(
        output,
        ffi::AV_SAMPLE_FMT_FLT,
        frame.sample_rate,
        &input_layout,
        frame.format,
        frame.sample_rate,
    )
    .map_err(adapter_error)?;
    converter.init().map_err(adapter_error)?;
    Ok(converter)
}

/// The speaker each channel of `layout` feeds; all [`Speaker::Unnamed`] for a count-only layout.
pub(crate) fn layout_speakers(layout: &AVChannelLayout) -> Vec<Speaker> {
    let channels = alpha2_ffmpeg_bridge::channel_layout_channels(layout);
    if channels.is_empty() {
        return vec![Speaker::Unnamed; usize::try_from(layout.nb_channels).unwrap_or(0)];
    }
    channels
        .into_iter()
        .map(|channel| Speaker::from_ffmpeg_channel(i64::from(channel)))
        .collect()
}

/// `speakers` as an `FFmpeg` layout: native order when possible, custom otherwise.
pub(crate) fn ffmpeg_layout(speakers: &SpeakerLayout) -> Result<AVChannelLayout, DecodeError> {
    let channels = speakers
        .speakers()
        .iter()
        .map(|speaker| {
            ffi::AVChannel::try_from(speaker.ffmpeg_channel()).unwrap_or(ffi::AV_CHAN_UNKNOWN)
        })
        .collect::<Vec<_>>();
    alpha2_ffmpeg_bridge::channel_layout_of(&channels).map_err(|error| {
        DecodeError::Adapter(format!(
            "could not lay out audio channels {}: {error:?}",
            speakers.describe()
        ))
    })
}

fn unsupported_channels(channels: usize) -> DecodeError {
    DecodeError::Unsupported(format!(
        "audio with {channels} channels is unsupported: the engine takes 1 to {MAX_AUDIO_CHANNELS}"
    ))
}

fn check_audio_channels(channels: usize) -> Result<(), DecodeError> {
    if (1..=MAX_AUDIO_CHANNELS).contains(&channels) {
        Ok(())
    } else {
        Err(unsupported_channels(channels))
    }
}

/// Gives a frame whose layout names no speakers (`UNSPEC`, `AMBISONIC`, or a custom order with
/// unknown channels) the layout it is converted to, so `swresample` copies it channel for channel.
///
/// With the stream's count that is the stream's layout; with another count it is that count's
/// default, rematrixed where possible. Left `UNSPEC`, `swresample` would resolve the input itself,
/// see a format change and return `AVERROR_INPUT_CHANGED` on every retry.
pub(crate) fn normalize_audio_frame_layout(
    frame: &mut AVFrame,
    stream: &AVChannelLayout,
) -> Result<(), DecodeError> {
    let (channels, named) = {
        let layout = frame.ch_layout();
        let named = matches!(
            layout.order,
            ffi::AV_CHANNEL_ORDER_NATIVE | ffi::AV_CHANNEL_ORDER_CUSTOM
        ) && !layout_speakers(&layout).contains(&Speaker::Unnamed);
        (layout.nb_channels, named)
    };
    if channels <= 0 {
        return Err(DecodeError::Adapter(
            "decoded audio frame has no channels".into(),
        ));
    }
    if named {
        return Ok(());
    }
    if channels == stream.nb_channels {
        let mut owned = AVChannelLayout::from_nb_channels(1);
        owned.copy(stream);
        frame.set_ch_layout(owned.into_inner());
        return Ok(());
    }
    let count = usize::try_from(channels).map_err(|_| DecodeError::OutOfRange)?;
    let speakers = SpeakerLayout::default_for(count).ok_or_else(|| unsupported_channels(count))?;
    frame.set_ch_layout(ffmpeg_layout(&speakers)?.into_inner());
    Ok(())
}

pub(crate) fn audio_output_frame(sample_rate: i32, layout: &AVChannelLayout) -> AVFrame {
    let mut frame = AVFrame::new();
    frame.set_format(ffi::AV_SAMPLE_FMT_FLT);
    frame.set_sample_rate(sample_rate);
    let mut owned = AVChannelLayout::from_nb_channels(1);
    owned.copy(layout);
    frame.set_ch_layout(owned.into_inner());
    frame
}

pub(crate) fn append_interleaved_audio(
    frame: &AVFrame,
    output: &mut [VecDeque<f32>],
) -> Result<(), DecodeError> {
    let packed = alpha2_ffmpeg_bridge::audio_f32_interleaved(frame).map_err(adapter_error)?;
    if output.is_empty() || packed.len() % output.len() != 0 {
        return Err(DecodeError::Adapter(
            "FFmpeg returned a partial audio frame".into(),
        ));
    }
    for frame in packed.chunks_exact(output.len()) {
        for (channel, sample) in output.iter_mut().zip(frame) {
            channel.push_back(*sample);
        }
    }
    Ok(())
}

/// Whether an `AVCodecHWConfig`'s `methods` include `method`.
///
/// `bindgen` types the enum constants unsigned on Linux, macOS and `MinGW` and `int` on MSVC,
/// so both sides are widened.
pub(crate) fn has_hw_config_method(methods: i32, method: impl Into<i64>) -> bool {
    i64::from(methods) & method.into() != 0
}

/// A seek position below any indexed timestamp, for reading from a stream's first packet.
///
/// MPEG-TS needs it because its first packet's decode time precedes the stream start time. A
/// demuxer that refuses such a time (the generic search AVI uses) is sought to the start time.
const STREAM_START_SEEK: i64 = i64::MIN / 2;

enum Landing {
    /// The frame presented at the time.
    Presented(DecodedFrame),
    /// The first frame decoded was already later than the time.
    Late(DecodedFrame),
    /// No frame before the end of the stream.
    Empty,
}

impl NativeDecoder {
    /// Selects strict failure (default) or bounded, reported recovery.
    pub fn set_decode_policy(&mut self, policy: DecodePolicy) {
        self.recovery.policy = policy;
    }

    /// Sets the project's declared color for this source, which replaces the frame and stream tags.
    /// `None` leaves the tags in charge.
    pub fn declare_color(&mut self, color: Option<ColorMetadata>) {
        self.declared_color = color;
    }

    /// Where `frame`'s chroma sits as its codec defines it; only hardware sessions report it.
    #[cfg(all(
        any(target_os = "linux", target_os = "macos", target_os = "windows"),
        feature = "hwdecode"
    ))]
    pub(crate) fn picture_chroma_location(&self, frame: &AVFrame) -> alpha2_media::ChromaLocation {
        crate::convert::decoded_chroma_location(self.decoder.codec_id, frame.chroma_location)
    }

    /// The color `frame` is shown with: the declaration, else the frame's tags, else the stream's.
    pub(crate) fn picture_color(&self, frame: &AVFrame) -> Option<ColorMetadata> {
        self.declared_color
            .or_else(|| frame_color_metadata(frame))
            .or(self.color)
    }

    /// Receives each diagnostic before recovery continues, including before errors.
    pub fn set_decode_reporter(
        &mut self,
        reporter: impl Fn(crate::DecodeDiagnostic) + Send + Sync + 'static,
    ) {
        self.recovery.reporter = Some(Box::new(reporter));
    }

    /// Cumulative corruption and concealment diagnostics, including seeks.
    #[must_use]
    pub fn decode_diagnostics(&self) -> &DecodeDiagnostics {
        &self.recovery.diagnostics
    }

    /// Opens the deterministically selected video stream.
    ///
    /// # Errors
    /// Returns an error when no decodable video stream exists.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DecodeError> {
        Self::open_with_threads(path, codec_thread_budget())
    }

    /// Opens the selected stream with an explicit `FFmpeg` worker allocation.
    ///
    /// # Errors
    /// Returns an error when no decodable video stream exists.
    pub fn open_with_threads(path: impl AsRef<Path>, threads: i64) -> Result<Self, DecodeError> {
        if !(1..=64).contains(&threads) {
            return Err(DecodeError::OutOfRange);
        }
        let mut input = open_input(path.as_ref())?;
        let probed = probe_context(&input, path.as_ref())?;
        let video_index = probed.selected_video.ok_or_else(|| {
            DecodeError::Unsupported("resource has no decodable video stream".into())
        })?;
        input.discard_all_but(video_index);
        let selected = probed
            .streams
            .iter()
            .find(|stream| stream.index == video_index)
            .ok_or(DecodeError::OutOfRange)?;
        let orientation = selected.orientation;
        let sample_aspect_ratio = selected.sample_aspect_ratio;
        // HDR decodes like any 10-bit stream; the renderer tone-maps it or the project refuses it.
        let color = selected.color;
        let dropped_layer;
        let (time_base, decoder) = {
            let stream = &input.streams()[video_index];
            let parameters = stream.codecpar();
            let (codec, dropped) = software_decoder_for(&parameters, selected.alpha)?;
            dropped_layer = dropped;
            let mut decoder = AVCodecContext::new(&codec);
            decoder.apply_codecpar(&parameters).map_err(adapter_error)?;
            decoder.set_pkt_timebase(stream.time_base);
            let unused = alpha2_ffmpeg_bridge::open_codec_context(
                &mut decoder,
                video_decoder_options(threads),
            )
            .map_err(adapter_error)?;
            reject_unused_decode_options(unused)?;
            (stream.time_base, decoder)
        };
        let (prefetched, slack) = read_presentation_slack(&mut input, video_index, time_base);
        Ok(Self {
            time_base,
            input,
            container_duration: probed.duration,
            streams: probed.streams,
            video_index,
            decoder,
            orientation,
            sample_aspect_ratio,
            color,
            declared_color: None,
            video_scaler: crate::convert::FrameScaler::default(),
            pending: VecDeque::new(),
            demux_eof: false,
            decoder_flushed: false,
            stages: DecodeStageTimings::default(),
            first_video_pts: None,
            recovery: Recovery::new(time_base),
            slack,
            prefetched,
            dropped_layer,
        })
    }

    /// Opens one hardware decode session for `spec` on an existing device.
    ///
    /// Failures carry a typed reason so callers can record why a source fell back.
    #[cfg(all(
        any(target_os = "linux", target_os = "macos", target_os = "windows"),
        feature = "hwdecode"
    ))]
    #[allow(clippy::too_many_lines)]
    pub(crate) fn open_hardware(
        path: impl AsRef<Path>,
        threads: i64,
        device: AVHWDeviceContext,
        spec: &HardwareDecoderSpec<'_>,
    ) -> Result<Self, HardwareOpenError> {
        use crate::video_frame::HardwareDecodeFallback::{
            DecoderOpenFailed, NoHardwareConfig, SourceUnavailable, UnsupportedColor,
        };

        let unavailable = |error| HardwareOpenError::new(SourceUnavailable, error);
        let mut input = open_input(path.as_ref()).map_err(unavailable)?;
        let probed = probe_context(&input, path.as_ref()).map_err(unavailable)?;
        let video_index = probed.selected_video.ok_or_else(|| {
            HardwareOpenError::new(
                SourceUnavailable,
                DecodeError::Unsupported("resource has no decodable video stream".into()),
            )
        })?;
        input.discard_all_but(video_index);
        let selected = probed
            .streams
            .iter()
            .find(|stream| stream.index == video_index)
            .ok_or_else(|| HardwareOpenError::new(SourceUnavailable, DecodeError::OutOfRange))?;
        let orientation = selected.orientation;
        let sample_aspect_ratio = selected.sample_aspect_ratio;
        let color = selected.color;
        if selected.color.is_some_and(ColorMetadata::is_hdr) {
            return Err(HardwareOpenError::new(
                UnsupportedColor,
                DecodeError::Unsupported("hardware export currently supports SDR only".into()),
            ));
        }
        // A hardware session decodes only the base picture, so it would drop alpha
        // a software decoder keeps.
        if let Some(alpha) = selected.alpha {
            return Err(HardwareOpenError::new(
                crate::video_frame::HardwareDecodeFallback::AlphaChannel,
                DecodeError::Unsupported(format!(
                    "the stream carries alpha ({}), which {} decode drops",
                    alpha.as_str(),
                    spec.backend
                )),
            ));
        }
        let (time_base, decoder) = {
            let stream = &input.streams()[video_index];
            let parameters = stream.codecpar();
            let preferred = AVCodec::find_decoder(parameters.codec_id).ok_or_else(|| {
                HardwareOpenError::new(
                    NoHardwareConfig,
                    DecodeError::Unsupported("selected video codec has no decoder".into()),
                )
            })?;
            if let Some(codecs) = spec.hardware_codecs {
                let listed = hardware_codec_name(parameters.codec_id);
                if !listed.is_some_and(|name| codecs.contains(&name)) {
                    let name = listed.map_or_else(
                        || preferred.name().to_string_lossy().into_owned(),
                        str::to_owned,
                    );
                    return Err(HardwareOpenError::new(
                        NoHardwareConfig,
                        DecodeError::Unsupported(format!(
                            "{} has no hardware decoder for {name} here",
                            spec.backend
                        )),
                    ));
                }
            }
            let supported = |codec: &AVCodecRef<'_>| {
                (0..)
                    .map_while(|index| codec.hw_config(index))
                    .any(|config| {
                        config.device_type == spec.device_type
                            && config.pix_fmt == spec.pixel_format
                            && (!spec.require_device_context
                                || has_hw_config_method(
                                    config.methods,
                                    ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX,
                                ))
                    })
            };
            let Some(codec) =
                registered_video_decoders(parameters.codec_id).find(|codec| supported(codec))
            else {
                return Err(HardwareOpenError::new(
                    NoHardwareConfig,
                    DecodeError::Unsupported(format!(
                        "decoder {} has no {} hardware configuration",
                        preferred.name().to_string_lossy(),
                        spec.backend
                    )),
                ));
            };
            check_hardware_sample_profile(parameters.format, spec)?;
            check_hardware_crop(&parameters, spec)?;
            let open_failed = |error| HardwareOpenError::new(DecoderOpenFailed, error);
            let mut decoder = AVCodecContext::new(&codec);
            decoder
                .apply_codecpar(&parameters)
                .map_err(adapter_error)
                .map_err(open_failed)?;
            decoder.set_pkt_timebase(stream.time_base);
            decoder.set_get_format(Some(spec.get_format));
            decoder.set_hw_device_ctx(device);
            // Hardware decoders failing to open is expected here, so the open must fail cleanly.
            let unused = alpha2_ffmpeg_bridge::open_codec_context(
                &mut decoder,
                video_decoder_options(threads),
            )
            .map_err(adapter_error)
            .map_err(open_failed)?;
            reject_unused_decode_options(unused).map_err(open_failed)?;
            (stream.time_base, decoder)
        };
        let (prefetched, slack) = read_presentation_slack(&mut input, video_index, time_base);
        Ok(Self {
            time_base,
            input,
            container_duration: probed.duration,
            streams: probed.streams,
            video_index,
            decoder,
            orientation,
            sample_aspect_ratio,
            color,
            declared_color: None,
            video_scaler: crate::convert::FrameScaler::default(),
            pending: VecDeque::new(),
            demux_eof: false,
            decoder_flushed: false,
            stages: DecodeStageTimings::default(),
            first_video_pts: None,
            recovery: Recovery::hardware(time_base),
            slack,
            prefetched,
            dropped_layer: None,
        })
    }

    /// What this decoder leaves out (unreadable alpha, extra HEVC layers), or `None`.
    #[must_use]
    pub const fn dropped_layer(&self) -> Option<&DroppedLayer> {
        self.dropped_layer.as_ref()
    }

    /// Effective end of the selected video source.
    ///
    /// Includes the container duration, which browser APIs expose, since the stream can end a few
    /// ticks earlier from time-base rounding.
    #[must_use]
    pub fn video_duration(&self) -> Option<MediaTime> {
        effective_media_duration(
            self.streams
                .iter()
                .find(|stream| stream.index == self.video_index)
                .and_then(|stream| stream.duration),
            self.container_duration,
        )
    }

    /// Takes the spans accumulated since the last call.
    pub(crate) fn take_stage_timings(&mut self) -> DecodeStageTimings {
        std::mem::take(&mut self.stages)
    }

    pub(crate) fn receive_available_raw(
        &mut self,
        cancel: &CancelToken,
    ) -> Result<Option<DecodedFrame>, DecodeError> {
        let mut budget = WorkBudget::new();
        loop {
            budget.step(cancel)?;
            let started = Instant::now();
            let received = DecodedFrame::receive(&mut self.decoder);
            self.stages.receive_frame = self.stages.receive_frame.saturating_add(started.elapsed());
            match received {
                Ok(frame) => {
                    if frame.flags
                        & i32::try_from(ffi::AV_FRAME_FLAG_CORRUPT).expect("flag fits i32")
                        != 0
                        || frame.decode_error_flags != 0
                    {
                        self.recovery.corrupt("video frame", Some(frame.pts))?;
                        continue;
                    }
                    #[cfg(test)]
                    VIDEO_FRAMES_DECODED.with(|decoded| decoded.set(decoded.get() + 1));
                    self.recovery.progress(
                        if frame.best_effort_timestamp == ffi::AV_NOPTS_VALUE {
                            frame.pts
                        } else {
                            frame.best_effort_timestamp
                        },
                    )?;
                    return Ok(Some(frame));
                }
                Err(RsmpegError::DecoderDrainError | RsmpegError::DecoderFlushedError) => {
                    return Ok(None);
                }
                Err(error) => {
                    self.recovery.error(error, "video receive", None)?;
                }
            }
        }
    }

    pub(crate) fn convert_received(
        &mut self,
        frame: &DecodedFrame,
    ) -> Result<DecodedVideoFrame, DecodeError> {
        // HDR frames keep their components and color for the renderer to tone-map or refuse.
        let declared = self.picture_color(frame);
        let started = Instant::now();
        let converted = convert_video_frame(
            frame,
            self.decoder.codec_id,
            self.time_base,
            declared.unwrap_or(ColorMetadata::UNDECLARED),
            &mut self.video_scaler,
        );
        self.stages.convert = self.stages.convert.saturating_add(started.elapsed());
        let mut converted = converted?;
        // With no declared color the frame goes untagged, so the renderer applies and reports the
        // same fallback. A repack to the identity matrix keeps the color it names.
        if declared.is_none() && converted.color == Some(ColorMetadata::UNDECLARED) {
            converted.color = None;
        }
        converted.orientation = self.orientation;
        converted.sample_aspect_ratio = self.picture_sample_aspect_ratio(frame);
        Ok(converted)
    }

    /// The pixel aspect ratio `frame` is shown with: the container's declaration, else the
    /// picture's own (broadcast streams may change it), else the probed one.
    pub(crate) fn picture_sample_aspect_ratio(&self, frame: &AVFrame) -> SampleAspectRatio {
        // A ratio the container declares is the probed one, and overrides the picture's.
        if declared_sar(self.input.streams()[self.video_index].sample_aspect_ratio).is_some() {
            return self.sample_aspect_ratio;
        }
        declared_sar(frame.sample_aspect_ratio).unwrap_or(self.sample_aspect_ratio)
    }

    pub(crate) fn raw_frame_pts(&self, frame: &AVFrame) -> Result<MediaTime, DecodeError> {
        let pts = if frame.best_effort_timestamp == ffi::AV_NOPTS_VALUE {
            frame.pts
        } else {
            frame.best_effort_timestamp
        };
        timestamp(pts, self.time_base).ok_or(DecodeError::OutOfRange)
    }

    /// Drops queued packets and frames, keeping the stream selection.
    pub(crate) fn flush_video(&mut self) {
        let started = Instant::now();
        self.recovery.reset_progress();
        self.decoder.flush_buffers();
        self.stages.seek = self.stages.seek.saturating_add(started.elapsed());
        self.pending.clear();
        self.prefetched.clear();
        self.demux_eof = false;
        self.decoder_flushed = false;
    }

    /// Seeks to `target` and returns the raw frame presented there: the last frame at or before its
    /// presentation deadline ([`Self::presentation_deadline`]).
    ///
    /// The frame after it is kept for the next `decode_raw_next`. A stream starting after `target`
    /// answers with its first frame, one ending before it with its last.
    ///
    /// The landing is checked, not trusted: MPEG-TS/PS index every packet as a keyframe, and an
    /// open GOP's leading frames cannot decode from the I-frame. When the first decoded frame is
    /// past the deadline and not the stream's first, the seek restarts 1 s, 2 s, 4 s, ... earlier.
    ///
    /// # Errors
    /// Returns cancellation, seek, and decode failures, and [`DecodeError::OutOfRange`] for a
    /// stream with no frame at all.
    pub(crate) fn seek_raw(
        &mut self,
        target: MediaTime,
        cancel: &CancelToken,
    ) -> Result<DecodedFrame, DecodeError> {
        if cancel.is_cancelled() {
            return Err(DecodeError::Cancelled);
        }
        let start_time = self.input.streams()[self.video_index].start_time;
        let stream_start = timestamp(start_time, self.time_base).unwrap_or(MediaTime::ZERO);
        let deadline = self.presentation_deadline(target);
        let mut back_off = 0_i64;
        loop {
            // Seek from the deadline: a keyframe within the slack after the target
            // is the frame shown there.
            let from = MediaTime::from_ticks(deadline.ticks().saturating_sub(back_off));
            let from_start =
                from <= stream_start || self.first_video_pts.is_some_and(|first| deadline < first);
            if !from_start {
                // Rounded down, so a time just before a keyframe never seeks onto it.
                let position = av_rescale_q_rnd(
                    from.ticks(),
                    tick_base(),
                    self.time_base,
                    ffi::AV_ROUND_DOWN,
                );
                self.seek_video_to(position, cancel)?;
            } else if let Err(error) = self.seek_video_to(STREAM_START_SEEK, cancel) {
                if error == DecodeError::Cancelled {
                    return Err(error);
                }
                let start = if start_time == ffi::AV_NOPTS_VALUE {
                    0
                } else {
                    start_time
                };
                self.seek_video_to(start, cancel)?;
            }
            let mut first = None;
            let landing = self.present_after_seek(target, &mut first, cancel)?;
            if from_start {
                // Read from the first packet, the first frame out is the stream's first frame.
                self.first_video_pts = first.or(self.first_video_pts);
            }
            match landing {
                Landing::Presented(frame) => return Ok(frame),
                Landing::Late(frame) if from_start || first == self.first_video_pts => {
                    return Ok(frame);
                }
                Landing::Empty if from_start => return Err(DecodeError::OutOfRange),
                Landing::Late(_) | Landing::Empty => {}
            }
            back_off = if back_off == 0 {
                TICKS_PER_SECOND
            } else {
                back_off.saturating_mul(2)
            };
        }
    }

    fn seek_video_to(&mut self, position: i64, cancel: &CancelToken) -> Result<(), DecodeError> {
        interrupt_on_cancel(&self.input, cancel);
        let seek_started = Instant::now();
        let sought = self.input.seek(
            i32::try_from(self.video_index).map_err(|_| DecodeError::OutOfRange)?,
            position,
            i32::try_from(ffi::AVSEEK_FLAG_BACKWARD).expect("seek flag fits i32"),
        );
        self.stages.seek = self.stages.seek.saturating_add(seek_started.elapsed());
        sought.map_err(|error| seek_failure(error, cancel))?;
        self.flush_video();
        Ok(())
    }

    /// Decodes from a seek's landing to the frame presented at `target`, noting in `first` when the
    /// first decoded frame is presented.
    fn present_after_seek(
        &mut self,
        target: MediaTime,
        first: &mut Option<MediaTime>,
        cancel: &CancelToken,
    ) -> Result<Landing, DecodeError> {
        let deadline = self.presentation_deadline(target);
        let mut presented: Option<DecodedFrame> = None;
        let mut budget = WorkBudget::new();
        loop {
            budget.step(cancel)?;
            let Some(raw) = self.decode_raw_next(cancel)? else {
                return Ok(presented.map_or(Landing::Empty, Landing::Presented));
            };
            // A keyframe far before the target takes long to decode from, and is still progress.
            budget.progressed();
            #[cfg(test)]
            crate::recovery::take_frame_delay();
            let pts = self.raw_frame_pts(&raw)?;
            first.get_or_insert(pts);
            if pts > deadline {
                return Ok(match presented {
                    Some(frame) => {
                        self.pending.push_front(raw);
                        Landing::Presented(frame)
                    }
                    None => Landing::Late(raw),
                });
            }
            // A frame between the target and its deadline is the one shown.
            if pts >= target {
                return Ok(Landing::Presented(raw));
            }
            presented = Some(raw);
        }
    }

    /// The latest timestamp a frame may carry and still be shown at `time`: `time` plus
    /// the stream's [`alpha2_time::presentation_slack`].
    #[must_use]
    pub fn presentation_deadline(&self, time: MediaTime) -> MediaTime {
        time.presentation_deadline(self.slack)
    }

    /// When the frame the next [`Decoder::decode_next`] returns is presented, or `None` at the end.
    ///
    /// The frame is decoded now and kept for that call, so a caller can find the frame shown at a
    /// time without consuming the next one.
    ///
    /// # Errors
    /// Returns cancellation and decode failures.
    pub fn peek_video_pts(
        &mut self,
        cancel: &CancelToken,
    ) -> Result<Option<MediaTime>, DecodeError> {
        self.peek_raw_pts(cancel)
    }

    /// The presentation time of the frame `decode_raw_next` returns next (decoded now and
    /// kept); `None` at the end.
    ///
    /// # Errors
    /// Returns cancellation and decode failures.
    pub(crate) fn peek_raw_pts(
        &mut self,
        cancel: &CancelToken,
    ) -> Result<Option<MediaTime>, DecodeError> {
        let Some(raw) = self.decode_raw_next(cancel)? else {
            return Ok(None);
        };
        let pts = self.raw_frame_pts(&raw);
        self.pending.push_front(raw);
        pts.map(Some)
    }

    pub(crate) fn decode_raw_next(
        &mut self,
        cancel: &CancelToken,
    ) -> Result<Option<DecodedFrame>, DecodeError> {
        if cancel.is_cancelled() {
            return Err(DecodeError::Cancelled);
        }
        if let Some(frame) = self.pending.pop_front() {
            return Ok(Some(frame));
        }
        interrupt_on_cancel(&self.input, cancel);
        let mut budget = WorkBudget::new();
        let mut foreign = false;
        loop {
            // Another stream's packet is skipped, not work (see `fill_pending`).
            if std::mem::take(&mut foreign) {
                if cancel.is_cancelled() {
                    return Err(DecodeError::Cancelled);
                }
            } else {
                budget.step(cancel)?;
            }
            if let Some(frame) = self.pending.pop_front() {
                return Ok(Some(frame));
            }
            if let Some(frame) = self.receive_available_raw(cancel)? {
                return Ok(Some(frame));
            }
            if self.demux_eof {
                if !self.decoder_flushed {
                    match self.decoder.send_packet(None) {
                        Ok(()) | Err(RsmpegError::DecoderFlushedError) => {}
                        Err(RsmpegError::DecoderFullError) => continue,
                        Err(error) => {
                            self.recovery.error(error, "flush", None)?;
                            continue;
                        }
                    }
                    self.decoder_flushed = true;
                    continue;
                }
                self.recovery.video_eof()?;
                return Ok(None);
            }
            let demux_started = Instant::now();
            let packet = self
                .prefetched
                .pop_front()
                .unwrap_or_else(|| self.input.read_packet());
            self.stages.demux = self.stages.demux.saturating_add(demux_started.elapsed());
            let packet = match packet {
                Ok(Some(packet)) => packet,
                Ok(None) => {
                    self.demux_eof = true;
                    continue;
                }
                Err(error) => {
                    let error = demux_failure(error, cancel)?;
                    self.recovery.error(error, "video demux", None)?;
                    continue;
                }
            };
            if usize::try_from(packet.stream_index).ok() != Some(self.video_index) {
                foreign = true;
                continue;
            }
            if packet.flags & i32::try_from(ffi::AV_PKT_FLAG_CORRUPT).expect("flag fits i32") != 0 {
                self.recovery.corrupt("video packet", Some(packet.pts))?;
                continue;
            }
            submit_packet(
                |drain| {
                    if drain {
                        let corrupt_before = self.recovery.diagnostics.corrupt_packets.len();
                        if let Some(frame) = self.receive_available_raw(cancel)? {
                            self.pending.push_back(frame);
                            return Ok(true);
                        }
                        return Ok(self.recovery.diagnostics.corrupt_packets.len() > corrupt_before);
                    }
                    let started = Instant::now();
                    let sent = self.decoder.send_packet(Some(&packet));
                    self.stages.send_packet += started.elapsed();
                    match sent {
                        Ok(()) => Ok(true),
                        Err(RsmpegError::DecoderFullError) => Ok(false),
                        Err(error) => {
                            self.recovery.error(error, "video send", Some(packet.pts))?;
                            Ok(true)
                        }
                    }
                },
                &mut budget,
                cancel,
            )?;
        }
    }
}

impl Decoder for NativeDecoder {
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn flush(&mut self) -> Result<(), DecodeError> {
        self.flush_video();
        Ok(())
    }

    fn seek(
        &mut self,
        request: DecodeRequest,
        cancel: &CancelToken,
    ) -> Result<DecodeResult, DecodeError> {
        let raw = self.seek_raw(request.time, cancel)?;
        let frame = self.convert_received(&raw)?;
        Ok(DecodeResult {
            request,
            event: DecodeEvent::Video(frame),
        })
    }

    fn decode_next(&mut self, cancel: &CancelToken) -> Result<DecodeEvent, DecodeError> {
        if cancel.is_cancelled() {
            return Err(DecodeError::Cancelled);
        }
        match self.decode_raw_next(cancel)? {
            Some(frame) => Ok(DecodeEvent::Video(self.convert_received(&frame)?)),
            None => Ok(DecodeEvent::EndOfStream),
        }
    }
}

/// Decoders that open without a hardware session and then fail every packet (`FFmpeg`'s own `av1`
/// answers `ENOSYS`). The pinned SDK decodes AV1 with libdav1d instead.
const HWACCEL_ONLY_DECODERS: &[&str] = &["av1"];

/// Every decoder registered for `codec_id`, preferred first. External software decoders are
/// preferred, yet only `FFmpeg`'s own `av1` carries hardware configurations, so hardware sessions
/// look through all of them.
pub(crate) fn registered_video_decoders(
    codec_id: ffi::AVCodecID,
) -> impl Iterator<Item = AVCodecRef<'static>> {
    AVCodec::iterate().filter(move |codec| {
        codec.id == codec_id
            && AVCodec::find_decoder_by_name(codec.name())
                .is_some_and(|found| found.as_ptr() == codec.as_ptr())
    })
}

/// The first decoder `FFmpeg` would pick for a software session, skipping hardware-only ones.
pub(crate) fn software_video_decoder(
    codec_id: ffi::AVCodecID,
) -> Result<AVCodecRef<'static>, DecodeError> {
    let Some(preferred) = AVCodec::find_decoder(codec_id) else {
        return Err(DecodeError::Unsupported(
            "selected video codec has no decoder".into(),
        ));
    };
    let hardware_only =
        |codec: &AVCodecRef<'_>| HWACCEL_ONLY_DECODERS.contains(&&*codec.name().to_string_lossy());
    if !hardware_only(&preferred) {
        return Ok(preferred);
    }
    let experimental = i32::try_from(ffi::AV_CODEC_CAP_EXPERIMENTAL).unwrap_or(0);
    AVCodec::iterate()
        .find(|codec| {
            codec.id == codec_id
                && AVCodec::find_decoder_by_name(codec.name())
                    .is_some_and(|found| found.as_ptr() == codec.as_ptr())
                && codec.capabilities & experimental == 0
                && !hardware_only(codec)
        })
        .ok_or_else(|| DecodeError::Unsupported(hardware_only_reason(&preferred)))
}

const NO_LIBVPX_VP9: &str = "this FFmpeg runtime has no libvpx-vp9 decoder";
const HEVC_ALPHA_NOT_DECODED: &str = "alpha in an HEVC auxiliary layer is not decoded";

/// libvpx's VP9 decoder (the one that reads alpha), when present.
fn libvpx_vp9() -> Option<AVCodecRef<'static>> {
    #[cfg(test)]
    if WITHOUT_LIBVPX_VP9.with(std::cell::Cell::get) {
        return None;
    }
    AVCodec::find_decoder_by_name(c"libvpx-vp9")
}

/// Whether this runtime decodes VP9's alpha: it has libvpx's VP9 decoder.
#[must_use]
pub fn decodes_vp9_alpha() -> bool {
    libvpx_vp9().is_some()
}

/// Why a software session would decode `stream` without its alpha, for conform reports.
#[must_use]
pub fn alpha_dropped_reason(stream: &StreamInfo) -> Option<&'static str> {
    match stream.alpha? {
        SourceAlpha::Vp9SideData => libvpx_vp9().is_none().then_some(NO_LIBVPX_VP9),
        SourceAlpha::HevcAuxiliaryLayer => Some(HEVC_ALPHA_NOT_DECODED),
        SourceAlpha::Planar => None,
    }
}

/// The software decoder for `parameters` carrying `alpha`, and what it leaves out.
///
/// VP9 alpha in block additions needs libvpx's decoder; HEVC's auxiliary alpha layer and MV-HEVC
/// views have no decoder here, so the base layer decodes and the loss is reported.
fn software_decoder_for(
    parameters: &rsmpeg::avcodec::AVCodecParameters,
    alpha: Option<SourceAlpha>,
) -> Result<(AVCodecRef<'static>, Option<DroppedLayer>), DecodeError> {
    let codec = software_video_decoder(parameters.codec_id)?;
    let dropped = |code, reason: &str| {
        Some(DroppedLayer {
            code,
            reason: reason.to_owned(),
        })
    };
    match alpha {
        Some(SourceAlpha::Vp9SideData) => Ok(match libvpx_vp9() {
            Some(libvpx) => (libvpx, None),
            None => (codec, dropped("alpha-dropped", NO_LIBVPX_VP9)),
        }),
        Some(SourceAlpha::HevcAuxiliaryLayer) => {
            Ok((codec, dropped("alpha-dropped", HEVC_ALPHA_NOT_DECODED)))
        }
        _ if parameters.codec_id == ffi::AV_CODEC_ID_HEVC
            && crate::parameter_sets::hevc_layers(
                &alpha2_ffmpeg_bridge::codec_parameters_extradata(parameters),
            ) == Some(crate::parameter_sets::HevcLayers::Extra) =>
        {
            Ok((
                codec,
                dropped(
                    "extra-layers-dropped",
                    "the HEVC layers besides the base layer are not decoded",
                ),
            ))
        }
        _ => Ok((codec, None)),
    }
}

/// How `stream` carries alpha: VP9's `alpha_mode` tag, an HEVC auxiliary layer, or a pixel
/// format with an alpha plane.
fn source_alpha(
    metadata: Option<rsmpeg::avutil::AVDictionaryRef<'_>>,
    parameters: &rsmpeg::avcodec::AVCodecParameters,
) -> Option<SourceAlpha> {
    let tagged = || {
        metadata
            .and_then(|metadata| {
                metadata
                    .get(c"alpha_mode", None, 0)
                    .map(|entry| entry.value().to_bytes() == b"1")
            })
            .unwrap_or(false)
    };
    match parameters.codec_id {
        ffi::AV_CODEC_ID_VP9 if tagged() => return Some(SourceAlpha::Vp9SideData),
        ffi::AV_CODEC_ID_HEVC
            if crate::parameter_sets::hevc_layers(
                &alpha2_ffmpeg_bridge::codec_parameters_extradata(parameters),
            ) == Some(crate::parameter_sets::HevcLayers::AuxiliaryAlpha) =>
        {
            return Some(SourceAlpha::HevcAuxiliaryLayer);
        }
        _ => {}
    }
    alpha2_ffmpeg_bridge::sample_profile(parameters.format)
        .filter(|profile| profile.has_alpha)
        .map(|_| SourceAlpha::Planar)
}

fn hardware_only_reason(decoder: &AVCodecRef<'_>) -> String {
    let name = decoder.name().to_string_lossy();
    format!(
        "{name} needs a hardware decoder; this runtime has no software {} decoder",
        name.to_uppercase()
    )
}

/// Why this runtime cannot decode `stream` without a hardware session, or `None` when software can
/// (some preferred decoders are hardware-only; AV1 needs libdav1d).
#[must_use]
pub fn software_decode_unsupported(stream: &StreamInfo) -> Option<String> {
    if stream.kind != StreamKind::Video {
        return None;
    }
    let name = CString::new(stream.codec.as_str()).ok()?;
    let decoder = AVCodec::find_decoder_by_name(&name)?;
    software_video_decoder(decoder.id)
        .err()
        .map(|error| match error {
            DecodeError::Unsupported(reason) => reason,
            error => error.to_string(),
        })
}

/// The longer of a stream's and its container's duration.
pub(crate) fn effective_media_duration(
    stream_duration: Option<MediaTime>,
    container_duration: Option<MediaTime>,
) -> Option<MediaTime> {
    stream_duration.into_iter().chain(container_duration).max()
}

/// Unpadded bytes of one picture of `format`, or `None` for a format `FFmpeg` does not know.
fn decoded_frame_bytes(format: i32, (width, height): (u32, u32)) -> Option<u64> {
    let size = rsmpeg::avutil::AVImage::get_buffer_size(
        format,
        i32::try_from(width).ok()?,
        i32::try_from(height).ok()?,
        1,
    )?;
    u64::try_from(size).ok().filter(|bytes| *bytes > 0)
}

/// The `FFmpeg` name a device's hardware codec list uses for `codec_id`.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
const fn hardware_codec_name(codec_id: ffi::AVCodecID) -> Option<&'static str> {
    match codec_id {
        ffi::AV_CODEC_ID_H264 => Some("h264"),
        ffi::AV_CODEC_ID_HEVC => Some("hevc"),
        ffi::AV_CODEC_ID_VP9 => Some("vp9"),
        ffi::AV_CODEC_ID_AV1 => Some("av1"),
        _ => None,
    }
}

/// Refuses a stream whose SPS crops its left or top edge on a backend whose surfaces lose that crop
/// ([`HardwareDecoderSpec::keeps_leading_crop`]). An unreadable SPS is not refused.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
fn check_hardware_crop(
    parameters: &rsmpeg::avcodec::AVCodecParameters,
    spec: &HardwareDecoderSpec<'_>,
) -> Result<(), HardwareOpenError> {
    if spec.keeps_leading_crop {
        return Ok(());
    }
    let extradata = alpha2_ffmpeg_bridge::codec_parameters_extradata(parameters);
    match crate::parameter_sets::leading_crop(parameters.codec_id, &extradata) {
        Some((left, top)) if left > 0 || top > 0 => Err(HardwareOpenError::new(
            crate::video_frame::HardwareDecodeFallback::UnsupportedCrop,
            DecodeError::Unsupported(format!(
                "{} decode does not show a picture cropped {left} columns from the left and {top} rows from the top",
                spec.backend
            )),
        )),
        _ => Ok(()),
    }
}

/// Rejects sample formats with no resident presentation path.
///
/// Only 8-bit 4:2:0, or 10-bit where the renderer presents it, reaches the GPU path; refusing at
/// open avoids a mid-export surface failure. An undeclared format is left to the import check.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
fn check_hardware_sample_profile(
    format: ffi::AVPixelFormat,
    spec: &HardwareDecoderSpec<'_>,
) -> Result<(), HardwareOpenError> {
    let Some(profile) = alpha2_ffmpeg_bridge::sample_profile(format) else {
        return Ok(());
    };
    let maximum_depth = if spec.ten_bit { 10 } else { 8 };
    if profile.is_yuv_420 && !profile.has_alpha && profile.bit_depth <= maximum_depth {
        return Ok(());
    }
    Err(HardwareOpenError::new(
        crate::video_frame::HardwareDecodeFallback::UnsupportedProfile,
        DecodeError::Unsupported(format!(
            "{} decode does not present {}-bit {} input",
            spec.backend,
            profile.bit_depth,
            if profile.is_yuv_420 {
                "4:2:0"
            } else {
                "non-4:2:0"
            }
        )),
    ))
}

/// Demuxers a source may open with, by `FFmpeg` short name. Each reads only the file it is given;
/// demuxers that open other files (HLS, `ffconcat`, image-sequence patterns) are excluded so an
/// asset cannot pull in media a request never named.
pub(crate) const SOURCE_CONTAINERS: &CStr =
    c"mov,matroska,mpegts,avi,mp3,aac,wav,flac,ogg,png_pipe,jpeg_pipe,webp_pipe,gif";

/// Opens a source with [`SOURCE_CONTAINERS`] and only the `file` protocol, so nothing
/// reaches past a local path.
pub(crate) fn open_input(path: &Path) -> Result<DemuxInput, DecodeError> {
    #[cfg(test)]
    INPUTS_OPENED.with(|opened| opened.set(opened.get() + 1));
    alpha2_ffmpeg_bridge::open_input(path, SOURCE_CONTAINERS).map_err(|error| match error {
        OpenInputError::Path(error) => DecodeError::Unsupported(format!("media path {error}")),
        OpenInputError::Refused(format) => DecodeError::Unsupported(format!(
            "the file is {format} input, which is not a container alpha2 reads"
        )),
        OpenInputError::Ffmpeg(error) => adapter_error(error),
    })
}

#[cfg(test)]
thread_local! {
    /// How many inputs [`open_input`] has opened on this thread.
    pub(crate) static INPUTS_OPENED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// How many video frames the codec has given a [`NativeDecoder`] on this thread.
    pub(crate) static VIDEO_FRAMES_DECODED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Lets `cancel` interrupt a blocking `FFmpeg` call on `input` (it returns `AVERROR_EXIT`).
pub(crate) fn interrupt_on_cancel(input: &DemuxInput, cancel: &CancelToken) {
    let cancel = cancel.clone();
    input.interrupt_when(move || cancel.is_cancelled());
}

/// A demux or seek failure, as a cancel when the cancel stopped it.
fn demux_failure(error: RsmpegError, cancel: &CancelToken) -> Result<RsmpegError, DecodeError> {
    if cancel.is_cancelled() {
        Err(DecodeError::Cancelled)
    } else {
        Ok(error)
    }
}

/// A seek failure, as a cancel when the cancel stopped it.
fn seek_failure(error: RsmpegError, cancel: &CancelToken) -> DecodeError {
    demux_failure(error, cancel).map_or_else(|cancelled| cancelled, adapter_error)
}

/// Single-picture demuxers whose decoder, not the container, carries orientation (`FFmpeg` puts a
/// JPEG's Exif `Orientation` on the frame).
const STILL_IMAGE_FORMATS: [&CStr; 3] = [c"jpeg_pipe", c"png_pipe", c"webp_pipe"];

fn is_still_image_format(input: &DemuxInput) -> bool {
    STILL_IMAGE_FORMATS.contains(&input.iformat().name())
}

/// The orientation a still picture's decoder attaches, read by decoding it once at reduced size
/// when possible. `None` when there is none or the picture does not decode.
fn still_image_orientation(path: &Path, index: usize) -> Option<Orientation> {
    let mut input = open_input(path).ok()?;
    let (codec, mut decoder) = {
        let stream = input.streams().get(index)?;
        let parameters = stream.codecpar();
        let codec = software_video_decoder(parameters.codec_id).ok()?;
        let mut decoder = AVCodecContext::new(&codec);
        decoder.apply_codecpar(&parameters).ok()?;
        decoder.set_pkt_timebase(stream.time_base);
        (codec, decoder)
    };
    let mut options = video_decoder_options(1);
    if codec.max_lowres > 0 {
        options = options.set_int(c"lowres", i64::from(codec.max_lowres), 0);
    }
    alpha2_ffmpeg_bridge::open_codec_context(&mut decoder, options).ok()?;
    let mut sent = false;
    // One picture is one packet; a few more allow for a demuxer that splits it.
    for _ in 0..16 {
        match decoder.receive_frame() {
            Ok(frame) => {
                return alpha2_ffmpeg_bridge::frame_display_rotation(&frame)
                    .and_then(orientation_from_display);
            }
            Err(RsmpegError::DecoderDrainError) => {}
            Err(_) => return None,
        }
        match input.read_packet().ok()? {
            Some(packet) if usize::try_from(packet.stream_index).ok() == Some(index) => {
                decoder.send_packet(Some(&packet)).ok()?;
                sent = true;
            }
            Some(_) => {}
            None if sent => {
                decoder.send_packet(None).ok()?;
                sent = false;
            }
            None => return None,
        }
    }
    None
}

#[allow(clippy::too_many_lines)]
/// Initial tail read for [`parsed_end`], matching `FFmpeg`'s `DURATION_MAX_READ_SIZE`; doubled up
/// to five times while no timed packet is found.
const PARSED_END_READ: u64 = 250_000;

/// Where `input`'s packets end (latest `pts + duration`, `AV_TIME_BASE` units) when `FFmpeg`
/// estimated the duration from timestamps; `None` otherwise.
///
/// That estimate (MPEG-TS, MPEG-PS) reads unparsed packets, so one PES carrying several AAC frames
/// ends at the first frame, up to 0.19 s short. This rereads the tail through the parsers on a
/// separate input, leaving `input`'s position untouched.
fn parsed_end(input: &DemuxInput, path: &Path) -> Option<i64> {
    if input.duration_estimation_method != ffi::AVFMT_DURATION_FROM_PTS {
        return None;
    }
    let size = std::fs::metadata(path).ok()?.len();
    let mut tail = open_input(path).ok()?;
    let streams = tail
        .streams()
        .iter()
        .map(|stream| {
            let kind = stream.codecpar().codec_type();
            (kind.is_audio() || kind.is_video()).then_some(stream.time_base)
        })
        .collect::<Vec<_>>();
    let av_time_base = ra(1, i32::try_from(ffi::AV_TIME_BASE).ok()?);
    for doubling in 0..6 {
        let from = size.saturating_sub(PARSED_END_READ << doubling);
        tail.seek(
            -1,
            i64::try_from(from).ok()?,
            i32::try_from(ffi::AVSEEK_FLAG_BYTE).ok()?,
        )
        .ok()?;
        let mut end = None::<i64>;
        while let Ok(Some(packet)) = tail.read_packet() {
            let Some(Some(time_base)) = usize::try_from(packet.stream_index)
                .ok()
                .and_then(|index| streams.get(index))
            else {
                continue;
            };
            if packet.pts == ffi::AV_NOPTS_VALUE {
                continue;
            }
            let packet_end = av_rescale_q(
                packet.pts.saturating_add(packet.duration.max(0)),
                *time_base,
                av_time_base,
            );
            end = Some(end.map_or(packet_end, |end: i64| end.max(packet_end)));
        }
        if end.is_some() || from == 0 {
            return end;
        }
    }
    None
}

/// The [`ProbeResult`] of an opened input, with `FFmpeg`'s best video and audio streams selected.
pub(crate) fn probe_context(input: &DemuxInput, path: &Path) -> Result<ProbeResult, DecodeError> {
    let selected_video = input
        .best_stream(ffi::AVMEDIA_TYPE_VIDEO)
        .map_err(adapter_error)?;
    let selected_audio = input
        .best_stream(ffi::AVMEDIA_TYPE_AUDIO)
        .map_err(adapter_error)?;
    let streams = input
        .streams()
        .iter()
        .enumerate()
        .filter_map(|(index, stream)| {
            let parameters = stream.codecpar();
            let kind = if parameters.codec_type().is_video() {
                StreamKind::Video
            } else if parameters.codec_type().is_audio() {
                StreamKind::Audio
            } else {
                return None;
            };
            let codec = AVCodec::find_decoder(parameters.codec_id).map_or_else(
                || "unknown".to_owned(),
                |codec| codec.name().to_string_lossy().into_owned(),
            );
            let dimensions = (kind == StreamKind::Video).then(|| {
                (
                    u32::try_from(parameters.width).unwrap_or(0),
                    u32::try_from(parameters.height).unwrap_or(0),
                )
            });
            let frame_rate = stream
                .guess_framerate()
                .filter(|rate| rate.num > 0 && rate.den > 0)
                .and_then(|rate| {
                    FrameRate::new(u32::try_from(rate.num).ok()?, u32::try_from(rate.den).ok()?)
                        .ok()
                });
            Some(StreamInfo {
                index,
                kind,
                codec,
                duration: timestamp(stream.duration, stream.time_base),
                dimensions,
                frame_rate,
                sample_rate: (parameters.sample_rate > 0)
                    .then(|| u32::try_from(parameters.sample_rate).unwrap_or(0)),
                channels: (kind == StreamKind::Audio)
                    .then(|| u32::try_from(parameters.ch_layout.nb_channels).unwrap_or(0)),
                orientation: alpha2_ffmpeg_bridge::display_rotation(&parameters)
                    .and_then(orientation_from_display)
                    .or_else(|| {
                        (kind == StreamKind::Video
                            && Some(index) == selected_video
                            && is_still_image_format(input))
                        .then(|| still_image_orientation(path, index))
                        .flatten()
                    })
                    .unwrap_or_else(|| Orientation::rotated(stream_rotation(stream.metadata()))),
                sample_aspect_ratio: guess_sar(
                    stream.sample_aspect_ratio,
                    parameters.sample_aspect_ratio,
                ),
                color: if kind == StreamKind::Video {
                    color_metadata(&parameters)
                } else {
                    None
                },
                alpha: (kind == StreamKind::Video)
                    .then(|| source_alpha(stream.metadata(), &parameters))
                    .flatten(),
                frame_bytes: dimensions
                    .and_then(|size| decoded_frame_bytes(parameters.format, size)),
            })
        })
        .collect();
    let av_time_base = ra(
        1,
        i32::try_from(ffi::AV_TIME_BASE).expect("time base fits i32"),
    );
    let duration = parsed_end(input, path)
        .filter(|_| input.start_time != ffi::AV_NOPTS_VALUE)
        .map(|end| end.saturating_sub(input.start_time))
        .map_or(input.duration, |parsed| parsed.max(input.duration));
    Ok(ProbeResult {
        format: input.iformat().name().to_string_lossy().into_owned(),
        duration: (duration > 0)
            .then(|| MediaTime::from_ticks(av_rescale_q(duration, av_time_base, tick_base()))),
        start: (input.start_time != ffi::AV_NOPTS_VALUE).then(|| {
            MediaTime::from_ticks(av_rescale_q(
                input.start_time,
                ra(
                    1,
                    i32::try_from(ffi::AV_TIME_BASE).expect("time base fits i32"),
                ),
                tick_base(),
            ))
        }),
        streams,
        selected_video,
        selected_audio,
    })
}

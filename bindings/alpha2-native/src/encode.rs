//! Muxed A/V and WAV encode sinks.

use std::collections::VecDeque;
use std::fs;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::audio_input::{AudioStreamConfig, EncoderAudioInput, open_audio_encoder};
use crate::capabilities::{
    AvEncodeConfig, NativeVideoEncoderSelection, VideoEncoderOpenRequest, open_video_encoder,
};
use crate::convert::{
    codec_thread_budget, configure_rgba_to_yuv, encode_adapter_error, encode_adapter_error_at,
    tick_base,
};
use crate::partial::{OutputTarget, PartialOutput, PublishReport, SeekNeed};
use alpha2_ffmpeg_bridge::OutputFile;
use alpha2_ffmpeg_bridge::Yuv420pFrameBuffer;
use alpha2_media::{
    AudioBlock, AudioBlockSink, CancelToken, DecodedVideoFrame, EncodeError, EncodeEvent,
    EncodeSink, MAX_AUDIO_CHANNELS, PixelFormat, SpeakerLayout,
};
use alpha2_time::MediaTime;
use rsmpeg::avcodec::{AVCodecContext, AVPacket};
use rsmpeg::avutil::{AVFrameWithImage, AVImage, AVRational, av_rescale_q};
use rsmpeg::error::RsmpegError;
use rsmpeg::ffi;
use rsmpeg::swscale::SwsContext;

/// Where one encode sink's wall time went.
///
/// The codec, the packet drain and the mux write move for different reasons:
/// a codec change moves the first, a channel-depth change moves the second,
/// and a container change moves the third. A single total such as
/// `videoEncodeMs` cannot say which one did, so each is timed apart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EncodeStageTimings {
    /// Waiting, before `avcodec_send_frame`, for the GPU to finish writing a
    /// surface the encoder cannot wait for on its own. Zero on every path
    /// whose encoder waits on the GPU itself or takes CPU frames.
    pub video_input_ready_wait: Duration,
    /// `avcodec_send_frame` for video: the codec itself.
    pub video_send_frame: Duration,
    /// `avcodec_receive_packet` for video.
    pub video_receive_packet: Duration,
    /// `av_interleaved_write_frame` for video packets.
    pub video_mux_write: Duration,
    /// `avcodec_send_frame` for audio.
    pub audio_send_frame: Duration,
    /// `avcodec_receive_packet` for audio.
    pub audio_receive_packet: Duration,
    /// `av_interleaved_write_frame` for audio packets.
    pub audio_mux_write: Duration,
}

impl EncodeStageTimings {
    /// What elapsed between `self` and a later snapshot.
    #[must_use]
    pub fn since(self, earlier: Self) -> Self {
        Self {
            video_input_ready_wait: self
                .video_input_ready_wait
                .saturating_sub(earlier.video_input_ready_wait),
            video_send_frame: self
                .video_send_frame
                .saturating_sub(earlier.video_send_frame),
            video_receive_packet: self
                .video_receive_packet
                .saturating_sub(earlier.video_receive_packet),
            video_mux_write: self.video_mux_write.saturating_sub(earlier.video_mux_write),
            audio_send_frame: self
                .audio_send_frame
                .saturating_sub(earlier.audio_send_frame),
            audio_receive_packet: self
                .audio_receive_packet
                .saturating_sub(earlier.audio_receive_packet),
            audio_mux_write: self.audio_mux_write.saturating_sub(earlier.audio_mux_write),
        }
    }
}

thread_local! {
    /// What this thread calls before each encoder and muxer call; see
    /// [`watch_encode_calls`].
    static ENCODE_CALL_WATCH: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Calls `beat` on this thread before every encoder and muxer call an encode
/// sink makes on it, until the returned guard drops.
///
/// Some of those calls wait with no timeout: `h264_vulkan` waits on its
/// execution fences with `UINT64_MAX`, and `write(2)` to a hard-mounted share
/// that stopped answering is retried forever. Nothing can interrupt them, so
/// the thread that drives a sink reports where it is instead, and its owner
/// decides that a call which has not returned in time is stuck. A sink's calls
/// all run on the thread that calls the sink, except the audio encoder's own
/// thread, which the sink waits on in a call this beat already covers.
#[must_use = "the watch ends when the guard drops"]
pub fn watch_encode_calls(beat: impl Fn() + 'static) -> EncodeCallWatch {
    let previous = ENCODE_CALL_WATCH.with(|watch| watch.replace(Some(Box::new(beat))));
    EncodeCallWatch {
        previous,
        thread: std::marker::PhantomData,
    }
}

/// Ends a [`watch_encode_calls`] watch, restoring the one before it.
pub struct EncodeCallWatch {
    previous: Option<Box<dyn Fn()>>,
    /// The watch belongs to the thread that set it.
    thread: std::marker::PhantomData<*const ()>,
}

impl Drop for EncodeCallWatch {
    fn drop(&mut self) {
        let previous = self.previous.take();
        ENCODE_CALL_WATCH.with(|watch| {
            watch.replace(previous);
        });
    }
}

/// An encoder or muxer call starts now on this thread.
pub(crate) fn beat_encode_call() {
    ENCODE_CALL_WATCH.with(|watch| {
        if let Some(beat) = watch.borrow().as_ref() {
            beat();
        }
    });
}

/// Runs `body`, adding its elapsed time to `total`. `body` is an encoder or
/// muxer call, so it is watched ([`watch_encode_calls`]).
pub(crate) fn timed<T>(total: &mut Duration, body: impl FnOnce() -> T) -> T {
    beat_encode_call();
    let started = Instant::now();
    let value = body();
    *total = total.saturating_add(started.elapsed());
    value
}

/// Transactional IEEE-f32 WAV encoder used by native offline audio export.
pub struct NativeWavEncodeSink {
    pub(crate) file: Option<BufWriter<fs::File>>,
    /// The partial file `file` writes, until it is published or removed.
    /// Declared after `file`, which is closed first.
    pub(crate) partial: PartialOutput,
    /// How the finished file was published.
    pub(crate) published: Option<PublishReport>,
    pub(crate) sample_rate: u32,
    pub(crate) channels: u16,
    /// The speakers every block feeds.
    pub(crate) speakers: SpeakerLayout,
    /// The `WAVE_FORMAT_EXTENSIBLE` channel mask a layout other than its
    /// count's default is written with; `None` writes a plain IEEE-float
    /// header, which readers take as the default.
    pub(crate) channel_mask: Option<u32>,
    /// The block plane each interleaved channel comes from: WAVE requires
    /// the mask's bit order.
    pub(crate) plane_order: Vec<usize>,
    pub(crate) frames_written: u64,
    pub(crate) complete: bool,
    /// One block's samples, interleaved, reused across blocks.
    pub(crate) interleaved: Vec<u8>,
}

/// Bytes the WAV sink gathers before each write: about five seconds of
/// 48 kHz stereo, so a minute of audio takes a few dozen writes.
const WAV_WRITE_BUFFER: usize = 1 << 20;

/// Audio/video sink that encodes and interleaves through the linked `FFmpeg`
/// libraries without requiring command-line programs on the host. Video frames
/// and audio blocks must be submitted in nondecreasing start-timestamp order;
/// ready packets are muxed incrementally instead of retained for the full file.
pub struct NativeAvEncodeSink {
    pub(crate) output: Option<OutputFile>,
    /// The partial file `output` writes, until it is published or removed.
    /// Declared after `output`, which is closed first.
    pub(crate) partial: PartialOutput,
    /// How the finished file was published.
    pub(crate) published: Option<PublishReport>,
    pub(crate) video_encoder: crate::video_backend::VideoEncoderBackend,
    pub(crate) video_stream_index: i32,
    pub(crate) video_encoder_time_base: AVRational,
    /// One frame in `video_encoder_time_base`.
    pub(crate) video_frame_units: i64,
    pub(crate) video_stream_time_base: AVRational,
    pub(crate) video_scaler: SwsContext,
    /// Turns decoded YUV of any layout into the encoder's 4:2:0.
    pub(crate) yuv_input: crate::convert::Yuv420pConverter,
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) last_video_pts: Option<i64>,
    /// Audio encoding, backlog and packet draining run on a separate thread
    /// so video encoding cannot serialize audio work.
    pub(crate) audio: AudioEncodeThread,
    /// The project's audio, as the audio encoder takes it.
    pub(crate) audio_input: EncoderAudioInput,
    pub(crate) video_packets: VecDeque<QueuedPacket>,
    pub(crate) last_input_ticks: Option<i64>,
    pub(crate) complete: bool,
    pub(crate) video_selection: NativeVideoEncoderSelection,
    pub(crate) stages: EncodeStageTimings,
}

/// A command to the audio encode thread: one frame to encode, or the end of the input.
pub(crate) enum AudioCommand {
    /// One frame of samples to encode, and the input tick the input reached.
    /// Every frame is the encoder's frame size but the last, which may be
    /// shorter.
    Block(Vec<Vec<f32>>, i64),
    /// No more input: flush the encoder and stop, the input having reached
    /// the tick.
    Finish(i64),
}

/// What the audio encode thread has produced, and how far it has got.
///
/// The muxer stays on the video worker beside `pop_next_packet`, so the only
/// thing crossing threads is finished packets and a watermark.
#[derive(Default)]
pub(crate) struct AudioQueueState {
    /// Packets in production order, which for one stream is DTS order.
    pub(crate) packets: VecDeque<QueuedPacket>,
    /// Input ticks the thread has consumed and produced every packet for.
    ///
    /// The muxer will not write a video packet past this. That is what keeps
    /// the container's interleave independent of when the audio thread runs:
    /// it writes exactly what the single-threaded sink would have written,
    /// because the single-threaded sink had also consumed every audio block up
    /// to the video frame it was about to encode.
    pub(crate) consumed_ticks: i64,
    /// Set once the thread has flushed its encoder and will produce no more.
    pub(crate) finished: bool,
    /// The thread's first failure, surfaced by the muxer.
    pub(crate) failure: Option<String>,
    /// The thread's share of the sink's stage totals.
    pub(crate) stages: EncodeStageTimings,
}

/// Handle to the audio encode thread.
pub(crate) struct AudioEncodeThread {
    commands: Option<SyncSender<AudioCommand>>,
    join: Option<JoinHandle<()>>,
    state: Arc<Mutex<AudioQueueState>>,
}

impl AudioEncodeThread {
    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, AudioQueueState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Hands one command over, reporting the thread's failure if it has
    /// stopped.
    pub(crate) fn send(&self, command: AudioCommand) -> Result<(), EncodeError> {
        let sent = match self.commands.as_ref() {
            Some(sender) => sender.send(command).is_ok(),
            None => return Err(EncodeError::Adapter("audio encoder is closed".into())),
        };
        if sent {
            return Ok(());
        }
        let failure = self.lock().failure.clone();
        Err(EncodeError::Adapter(failure.unwrap_or_else(|| {
            "audio encoder stopped before accepting a block".into()
        })))
    }

    /// Stops the thread, leaving whatever it produced in the queue.
    pub(crate) fn shutdown(&mut self) {
        drop(self.commands.take());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for AudioEncodeThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// A finished packet waiting for the muxer, ordered by its DTS in ticks.
pub(crate) struct QueuedPacket {
    pub(crate) sort_ticks: i64,
    pub(crate) packet: AVPacket,
}

impl NativeAvEncodeSink {
    /// Creates linked-library video, audio, and container encoders without
    /// touching the final destination.
    ///
    /// # Errors
    /// Returns an encoder error for invalid configuration, paths, or stale
    /// component outputs.
    pub fn create(
        path: impl Into<OutputTarget>,
        config: AvEncodeConfig,
    ) -> Result<Self, EncodeError> {
        Self::create_selected(path, config, None, &codec_thread_budget)
    }

    /// Creates the requested concrete video encoder by its `FFmpeg` short name.
    ///
    /// # Errors
    /// Returns an encoder error when the name is unavailable, implements a
    /// different codec, or the remaining configuration is invalid.
    pub fn create_with_video_encoder(
        path: impl Into<OutputTarget>,
        config: AvEncodeConfig,
        encoder_name: &str,
    ) -> Result<Self, EncodeError> {
        Self::create_selected(path, config, Some(encoder_name), &codec_thread_budget)
    }

    /// Creates an encoder whose software thread count the caller decides.
    ///
    /// `software_threads` is asked, at most once, only when a candidate that
    /// runs on the CPU is opened: the caller sizes the rest of its work from
    /// [`NativeVideoEncoderSelection`] afterwards, so a hardware candidate that
    /// fails to open and falls through to software still gets a software
    /// encoder's share of the CPU.
    ///
    /// `encoder_name` is the concrete encoder the caller pinned. `None` lets
    /// the candidates for the codec be tried in preference order, which is
    /// what makes the open-time fallback reachable.
    ///
    /// # Errors
    /// Returns an encoder error when no candidate opens, or for invalid
    /// configuration, paths, codecs, or muxers.
    pub fn create_with_video_encoder_threads(
        path: impl Into<OutputTarget>,
        config: AvEncodeConfig,
        encoder_name: Option<&str>,
        software_threads: &dyn Fn() -> i64,
    ) -> Result<Self, EncodeError> {
        Self::create_selected(path, config, encoder_name, software_threads)
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn create_selected(
        path: impl Into<OutputTarget>,
        config: AvEncodeConfig,
        encoder_name: Option<&str>,
        software_threads: &dyn Fn() -> i64,
    ) -> Result<Self, EncodeError> {
        crate::video_encode::check_video_config(&config.video)?;
        if !config
            .video
            .container
            .offers_audio(config.audio_codec.model())
        {
            return Err(EncodeError::Adapter(format!(
                "{} does not offer audio codec {}",
                config.video.container,
                config.audio_codec.as_str()
            )));
        }
        if config.audio_sample_rate == 0
            || config.audio_channels == 0
            || config.audio_channels > 8
            || config.audio_bit_rate == 0
        {
            return Err(EncodeError::Adapter(format!(
                "the audio output ({} channels at {} Hz, {} bit/s) is refused: 1 to 8 channels, a positive rate and bitrate",
                config.audio_channels, config.audio_sample_rate, config.audio_bit_rate
            )));
        }
        let target = path.into();
        let partial = PartialOutput::claim(&target, SeekNeed::Required)?;
        let mut output = partial.open_muxer(config.video.container)?;
        let global_header =
            output.oformat().flags & i32::try_from(ffi::AVFMT_GLOBALHEADER).unwrap_or(0) != 0;

        let width = i32::try_from(config.video.width)
            .map_err(|_| EncodeError::Adapter("width is out of range".into()))?;
        let height = i32::try_from(config.video.height)
            .map_err(|_| EncodeError::Adapter("height is out of range".into()))?;
        let (video_encoder, video_selection) = open_video_encoder(&VideoEncoderOpenRequest {
            config: &config.video,
            requested_name: encoder_name,
            threads: crate::capabilities::SoftwareEncoderThreads::new(software_threads),
            global_header,
            width,
            height,
        })?;
        // The frame length, or the finest unit an encoder that cannot hold it takes.
        let video_encoder_time_base = video_encoder.time_base();

        let audio = open_audio_encoder(
            AudioStreamConfig {
                codec: config.audio_codec,
                sample_rate: config.audio_sample_rate,
                channels: config.audio_channels,
                bit_rate: config.audio_bit_rate,
            },
            global_header,
        )?;
        let audio_encoder_time_base = audio.encoder_time_base;

        let video_stream_index;
        {
            let mut stream = output.new_stream();
            stream.set_time_base(video_encoder_time_base);
            stream.set_codecpar(video_encoder.codec_parameters());
            crate::video_encode::describe_output_stream(
                &mut stream,
                video_encoder.codec_id(),
                config.video.display,
                config.video.container,
            )?;
            video_stream_index = stream.index;
        }
        let audio_stream_index;
        {
            let mut stream = output.new_stream();
            stream.set_time_base(audio_encoder_time_base);
            stream.set_codecpar(audio.encoder.extract_codecpar());
            audio_stream_index = stream.index;
        }
        output
            .write_header(&mut crate::convert::muxer_header_options(
                config.video.container,
            ))
            .map_err(encode_adapter_error)?;
        let video_stream_time_base =
            output.streams()[usize::try_from(video_stream_index).unwrap_or(0)].time_base;
        let audio_stream_time_base =
            output.streams()[usize::try_from(audio_stream_index).unwrap_or(0)].time_base;
        let mut video_scaler = SwsContext::get_context(
            width,
            height,
            ffi::AV_PIX_FMT_RGBA,
            width,
            height,
            ffi::AV_PIX_FMT_YUV420P,
            ffi::SWS_BILINEAR,
            None,
            None,
            None,
        )
        .ok_or_else(|| EncodeError::Adapter("cannot create RGBA to YUV scaler".into()))?;
        configure_rgba_to_yuv(&mut video_scaler, config.video.color)
            .map_err(encode_adapter_error)?;
        let audio_input = audio.input;
        let audio = spawn_audio_encoder(AudioEncoderConfig {
            encoder: audio.encoder,
            stream_index: audio_stream_index,
            encoder_time_base: audio_encoder_time_base,
            stream_time_base: audio_stream_time_base,
            sample_rate: audio.encoder_rate,
            sample_format: audio.sample_format,
        })?;
        Ok(Self {
            output: Some(output),
            partial,
            published: None,
            video_encoder,
            video_stream_index,
            video_encoder_time_base,
            video_frame_units: crate::mux::frame_units(
                config.video.frame_rate,
                video_encoder_time_base,
            ),
            video_stream_time_base,
            video_scaler,
            yuv_input: crate::convert::Yuv420pConverter::new(config.video.color),
            width,
            height,
            last_video_pts: None,
            audio,
            audio_input,
            video_packets: VecDeque::new(),
            last_input_ticks: None,
            complete: false,
            video_selection,
            stages: EncodeStageTimings::default(),
        })
    }

    /// Audio packets the thread has produced and the muxer has not written.
    #[cfg(test)]
    pub(crate) fn queued_audio_packets(&self) -> usize {
        self.audio.lock().packets.len()
    }

    /// Where this sink's time has gone so far, both threads together.
    #[must_use]
    pub fn stage_timings(&self) -> EncodeStageTimings {
        let audio = self.audio.lock().stages;
        EncodeStageTimings {
            audio_send_frame: audio.audio_send_frame,
            audio_receive_packet: audio.audio_receive_packet,
            ..self.stages
        }
    }

    /// The video encoder this sink opened, and the candidates it passed over.
    #[must_use]
    pub fn video_selection(&self) -> &NativeVideoEncoderSelection {
        &self.video_selection
    }

    /// How the finished file was published; `None` until it is.
    #[must_use]
    pub fn publish_report(&self) -> Option<PublishReport> {
        self.published
    }

    pub(crate) fn cleanup(&mut self) -> Result<(), EncodeError> {
        // Stop the audio thread before the muxer goes, so nothing is queued against a closed
        // output.
        self.audio.shutdown();
        drop(self.output.take());
        self.partial.discard()
    }

    /// Queues one finished video packet for the muxer.
    fn queue_video_packet(&mut self, mut packet: AVPacket) {
        packet.set_stream_index(self.video_stream_index);
        crate::mux::fill_frame_duration(&mut packet, self.video_frame_units);
        packet.rescale_ts(self.video_encoder_time_base, self.video_stream_time_base);
        let timestamp = if packet.dts == ffi::AV_NOPTS_VALUE {
            packet.pts
        } else {
            packet.dts
        };
        self.video_packets.push_back(QueuedPacket {
            sort_ticks: av_rescale_q(timestamp, self.video_stream_time_base, tick_base()),
            packet,
        });
    }

    /// Takes the next packet the muxer may write, or `None` to wait.
    ///
    /// Deterministic interleaving follows two rules:
    /// - Earlier DTS wins; video wins ties so thread scheduling cannot affect output.
    /// - Hold video until audio has consumed all input ticks through its timestamp.
    ///   The coordinator's channel bounds how far video can get ahead.
    fn pop_next_packet(&mut self, flushing: bool) -> Result<Option<QueuedPacket>, EncodeError> {
        let mut state = self.audio.lock();
        if let Some(error) = state.failure.clone() {
            return Err(EncodeError::Adapter(error));
        }
        let take_audio = match (self.video_packets.front(), state.packets.front()) {
            (Some(video), Some(audio)) => audio.sort_ticks < video.sort_ticks,
            (None, Some(_)) => true,
            (_, None) => false,
        };
        if take_audio {
            return Ok(state.packets.pop_front());
        }
        let Some(video) = self.video_packets.front() else {
            return Ok(None);
        };
        if !flushing && !state.finished && state.consumed_ticks < video.sort_ticks {
            return Ok(None);
        }
        drop(state);
        Ok(self.video_packets.pop_front())
    }

    pub(crate) fn write_ready_packets(&mut self, flushing: bool) -> Result<(), EncodeError> {
        while let Some(mut queued) = self.pop_next_packet(flushing)? {
            // The muxer is shared, so the write is charged to the stream whose packet it is.
            let video = queued.packet.stream_index == self.video_stream_index;
            let total = if video {
                &mut self.stages.video_mux_write
            } else {
                &mut self.stages.audio_mux_write
            };
            let output = self
                .output
                .as_mut()
                .ok_or_else(|| EncodeError::Adapter("muxer is already closed".into()))?;
            timed(total, || output.interleaved_write_frame(&mut queued.packet))
                .map_err(|error| encode_adapter_error_at("packet write failed", error))?;
        }
        Ok(())
    }

    pub(crate) fn drain_video_packets(&mut self, flushing: bool) -> Result<(), EncodeError> {
        loop {
            let received = timed(&mut self.stages.video_receive_packet, || {
                self.video_encoder.receive_packet()
            });
            match received {
                Ok(packet) => self.queue_video_packet(packet),
                Err(RsmpegError::EncoderDrainError) if !flushing => return Ok(()),
                Err(RsmpegError::EncoderFlushedError | RsmpegError::EncoderDrainError)
                    if flushing =>
                {
                    return Ok(());
                }
                Err(error) => {
                    return Err(encode_adapter_error_at(
                        "video encoder packet drain failed",
                        error,
                    ));
                }
            }
        }
    }

    /// Encodes a frame the caller already packed, without copying it.
    ///
    /// See [`crate::NativeVideoEncodeSink::write_prepared_video`]; this is the
    /// same handover for the muxed audio/video sink.
    ///
    /// # Errors
    /// Returns an error for a frame sized differently from this encoder, a
    /// non-monotonic timestamp, or a codec failure.
    pub fn write_prepared_video(
        &mut self,
        frame: &mut Yuv420pFrameBuffer,
        pts: MediaTime,
    ) -> Result<(), EncodeError> {
        let (width, height) = frame.size();
        if width != usize::try_from(self.width).unwrap_or(0)
            || height != usize::try_from(self.height).unwrap_or(0)
        {
            return Err(EncodeError::Adapter(
                "encoder input dimensions do not match its configuration".into(),
            ));
        }
        let pts = av_rescale_q(pts.ticks(), tick_base(), self.video_encoder_time_base);
        if self.last_video_pts.is_some_and(|previous| pts <= previous) {
            return Err(EncodeError::InvalidTimestamp);
        }
        frame.set_presentation(pts, self.video_encoder_time_base);
        timed(&mut self.stages.video_send_frame, || {
            self.video_encoder.send_frame(Some(frame.frame()))
        })
        .map_err(|error| {
            encode_adapter_error_at("video encoder rejected a YUV420P frame", error)
        })?;
        self.last_video_pts = Some(pts);
        self.drain_video_packets(false)
            .and_then(|()| self.write_ready_packets(false))
            .map_err(|error| self.partial.explain(error))
    }

    pub(crate) fn write_video(&mut self, frame: &DecodedVideoFrame) -> Result<(), EncodeError> {
        // Decoded and RGBA frames are converted to YUV420P; any other output takes frames the
        // caller packed ([`Self::write_prepared_video`]).
        if !self.video_selection.pixels.is_yuv420p() {
            return Err(EncodeError::Adapter(format!(
                "this encoder takes packed {} frames",
                self.video_selection.pixels.ffmpeg_name()
            )));
        }
        if frame.visible_rect.width != u32::try_from(self.width).unwrap_or(0)
            || frame.visible_rect.height != u32::try_from(self.height).unwrap_or(0)
        {
            return Err(EncodeError::Adapter(
                "encoder input dimensions do not match its configuration".into(),
            ));
        }
        let pts = av_rescale_q(frame.pts.ticks(), tick_base(), self.video_encoder_time_base);
        if self.last_video_pts.is_some_and(|previous| pts <= previous) {
            return Err(EncodeError::InvalidTimestamp);
        }
        if let Some(mut yuv) = self.yuv_input.convert(frame, self.width, self.height)? {
            yuv.set_pts(pts);
            yuv.set_time_base(self.video_encoder_time_base);
            timed(&mut self.stages.video_send_frame, || {
                self.video_encoder.send_frame(Some(&yuv))
            })
            .map_err(|error| {
                encode_adapter_error_at("video encoder rejected a YUV420P frame", error)
            })?;
            self.last_video_pts = Some(pts);
            self.drain_video_packets(false)?;
            return self.write_ready_packets(false);
        }
        if frame.pixel_format != PixelFormat::Rgba8 || frame.planes.len() != 1 {
            return Err(EncodeError::Adapter(
                "encoder expects YUV planes or one RGBA8 plane".into(),
            ));
        }
        let plane = &frame.planes[0];
        let expected = usize::try_from(self.width)
            .ok()
            .and_then(|width| {
                usize::try_from(self.height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| EncodeError::Adapter("frame dimensions overflow".into()))?;
        if plane.bytes.len() != expected
            || plane.stride != u32::try_from(self.width).unwrap_or(0) * 4
        {
            return Err(EncodeError::Adapter(
                "RGBA plane is not tightly packed".into(),
            ));
        }
        // No alpha in the output, so premultiplied RGB is kept: frames arrive opaque over the
        // project background, and dividing alpha out would show a translucent layer at full
        // strength.
        let rgba = alpha2_ffmpeg_bridge::rgba_frame(&plane.bytes, self.width, self.height)
            .map_err(encode_adapter_error)?;
        let yuv_image = AVImage::new(ffi::AV_PIX_FMT_YUV420P, self.width, self.height, 1)
            .ok_or_else(|| EncodeError::Adapter("cannot allocate YUV input".into()))?;
        let mut yuv = AVFrameWithImage::new(yuv_image);
        self.video_scaler
            .scale_frame(&rgba, 0, self.height, &mut yuv)
            .map_err(encode_adapter_error)?;
        yuv.set_pts(pts);
        yuv.set_time_base(self.video_encoder_time_base);
        timed(&mut self.stages.video_send_frame, || {
            self.video_encoder.send_frame(Some(&yuv))
        })
        .map_err(|error| {
            encode_adapter_error_at("video encoder rejected a converted YUV420P frame", error)
        })?;
        self.last_video_pts = Some(pts);
        self.drain_video_packets(false)?;
        self.write_ready_packets(false)
    }

    /// Buffers one audio block and hands the thread every whole encoder
    /// frame it completes.
    pub(crate) fn write_audio(&mut self, block: &AudioBlock) -> Result<(), EncodeError> {
        let frames = self.audio_input.push(block)?;
        let covered = self.audio_input.received_ticks();
        for frame in frames {
            self.audio.send(AudioCommand::Block(frame, covered))?;
        }
        // An audio-only stretch has no video packet to trigger a drain, so the queue would grow.
        self.write_ready_packets(false)
    }
}

/// What the audio encode thread needs from the sink that created it.
pub(crate) struct AudioEncoderConfig {
    pub(crate) encoder: AVCodecContext,
    pub(crate) stream_index: i32,
    pub(crate) encoder_time_base: AVRational,
    pub(crate) stream_time_base: AVRational,
    /// The rate the encoder runs at.
    pub(crate) sample_rate: u32,
    /// The sample format its frames carry.
    pub(crate) sample_format: ffi::AVSampleFormat,
}

/// Starts the audio encoder on its own thread.
///
/// The channel is two deep: the coordinator hands blocks in faster than the
/// video encoder consumes frames, and a deeper queue would only buy backlog
/// the muxer has to hold anyway.
///
/// # Errors
/// When the OS refuses the thread (a process or cgroup thread limit, or no
/// memory for its stack).
pub(crate) fn spawn_audio_encoder(
    config: AudioEncoderConfig,
) -> Result<AudioEncodeThread, EncodeError> {
    let (commands, receiver) = mpsc::sync_channel(2);
    let state = Arc::new(Mutex::new(AudioQueueState::default()));
    let worker_state = Arc::clone(&state);
    let run = move || run_audio_encoder(config, &receiver, &worker_state);
    let builder = thread::Builder::new().name("alpha2-audio-encode".to_owned());
    #[cfg(test)]
    let spawned = if REFUSE_AUDIO_ENCODE_THREAD.with(std::cell::Cell::take) {
        drop(run);
        Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
    } else {
        builder.spawn(run)
    };
    #[cfg(not(test))]
    let spawned = builder.spawn(run);
    let join = spawned.map_err(|error| {
        encode_adapter_error_at("failed to start the audio encode thread", error)
    })?;
    Ok(AudioEncodeThread {
        commands: Some(commands),
        join: Some(join),
        state,
    })
}

#[cfg(test)]
thread_local! {
    /// Makes the next audio encode thread on this thread fail to start, as a
    /// thread limit does.
    pub(crate) static REFUSE_AUDIO_ENCODE_THREAD: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// Encodes blocks into packets until the channel closes or a `Finish` lands.
fn run_audio_encoder(
    mut config: AudioEncoderConfig,
    commands: &Receiver<AudioCommand>,
    state: &Arc<Mutex<AudioQueueState>>,
) {
    let mut encoded_samples = 0_u64;
    loop {
        let Ok(command) = commands.recv() else {
            // The sink dropped the sender without finishing (abort or cancel); nothing will read
            // the queue.
            return;
        };
        let (channels, covered, last) = match command {
            AudioCommand::Block(channels, covered) => (Some(channels), covered, false),
            AudioCommand::Finish(covered) => (None, covered, true),
        };
        let mut stages = EncodeStageTimings::default();
        let outcome = encode_audio_block(
            &mut config,
            channels.as_deref(),
            &mut encoded_samples,
            &mut stages,
        );
        let mut guard = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.stages.audio_send_frame = guard
            .stages
            .audio_send_frame
            .saturating_add(stages.audio_send_frame);
        guard.stages.audio_receive_packet = guard
            .stages
            .audio_receive_packet
            .saturating_add(stages.audio_receive_packet);
        match outcome {
            Ok(packets) => guard.packets.extend(packets),
            Err(error) => {
                if guard.failure.is_none() {
                    guard.failure = Some(error.to_string());
                }
                guard.finished = true;
                return;
            }
        }
        guard.consumed_ticks = guard.consumed_ticks.max(covered);
        if last {
            guard.finished = true;
            return;
        }
    }
}

/// Encodes one audio frame, or with none flushes the encoder (which is where
/// its delayed tail packets come from), and drains whatever packets that
/// produced.
fn encode_audio_block(
    config: &mut AudioEncoderConfig,
    channels: Option<&[Vec<f32>]>,
    encoded_samples: &mut u64,
    stages: &mut EncodeStageTimings,
) -> Result<Vec<QueuedPacket>, EncodeError> {
    let last = channels.is_none();
    if let Some(channels) = channels {
        let pts = i64::try_from(*encoded_samples).map_err(|_| EncodeError::InvalidTimestamp)?;
        let frame = alpha2_ffmpeg_bridge::audio_frame(
            channels,
            config.sample_format,
            i32::try_from(config.sample_rate).unwrap_or(i32::MAX),
            pts,
        )
        .map_err(encode_adapter_error)?;
        timed(&mut stages.audio_send_frame, || {
            config.encoder.send_frame(Some(&frame))
        })
        .map_err(|error| encode_adapter_error_at("audio encoder rejected a frame", error))?;
        let samples = channels.first().map_or(0, Vec::len);
        *encoded_samples =
            encoded_samples.saturating_add(u64::try_from(samples).unwrap_or(u64::MAX));
    } else {
        timed(&mut stages.audio_send_frame, || {
            config.encoder.send_frame(None)
        })
        .map_err(encode_adapter_error)?;
    }
    let mut packets = Vec::new();
    loop {
        let received = timed(&mut stages.audio_receive_packet, || {
            config.encoder.receive_packet()
        });
        match received {
            Ok(mut packet) => {
                packet.set_stream_index(config.stream_index);
                packet.rescale_ts(config.encoder_time_base, config.stream_time_base);
                let timestamp = if packet.dts == ffi::AV_NOPTS_VALUE {
                    packet.pts
                } else {
                    packet.dts
                };
                packets.push(QueuedPacket {
                    sort_ticks: av_rescale_q(timestamp, config.stream_time_base, tick_base()),
                    packet,
                });
            }
            Err(RsmpegError::EncoderDrainError) if !last => return Ok(packets),
            Err(RsmpegError::EncoderFlushedError | RsmpegError::EncoderDrainError) if last => {
                return Ok(packets);
            }
            Err(error) => {
                return Err(encode_adapter_error_at(
                    "audio encoder packet drain failed",
                    error,
                ));
            }
        }
    }
}

impl EncodeSink for NativeAvEncodeSink {
    fn write(&mut self, event: EncodeEvent, cancel: &CancelToken) -> Result<(), EncodeError> {
        if cancel.is_cancelled() {
            self.cleanup()?;
            return Err(EncodeError::Cancelled);
        }
        let ticks = match &event {
            EncodeEvent::Video(frame) => frame.pts.ticks(),
            EncodeEvent::Audio(block) => block.start_time().ticks(),
        };
        if self
            .last_input_ticks
            .is_some_and(|previous| ticks < previous)
        {
            return Err(EncodeError::InvalidTimestamp);
        }
        match event {
            EncodeEvent::Video(frame) => self.write_video(&frame),
            EncodeEvent::Audio(block) => self.write_audio(&block),
        }
        .map_err(|error| self.partial.explain(error))?;
        self.last_input_ticks = Some(ticks);
        Ok(())
    }
    fn finish(&mut self, cancel: &CancelToken) -> Result<(), EncodeError> {
        if cancel.is_cancelled() {
            self.cleanup()?;
            return Err(EncodeError::Cancelled);
        }
        self.close_output()
            .map_err(|error| self.partial.explain(error))?;
        self.published = Some(self.partial.finish(cancel)?);
        self.complete = true;
        Ok(())
    }
    fn abort(&mut self) -> Result<(), EncodeError> {
        self.cleanup()
    }
}

impl NativeAvEncodeSink {
    /// Flushes both encoders, writes the trailer and closes the file: what
    /// [`EncodeSink::finish`] does before it publishes.
    fn close_output(&mut self) -> Result<(), EncodeError> {
        // Let the thread flush its encoder and wait, so every audio packet exists before the muxer
        // decides the final interleave.
        let covered = self.audio_input.received_ticks();
        for frame in self.audio_input.finish() {
            self.audio.send(AudioCommand::Block(frame, covered))?;
        }
        self.audio.send(AudioCommand::Finish(covered))?;
        self.audio.shutdown();
        if let Some(error) = self.audio.lock().failure.clone() {
            return Err(EncodeError::Adapter(error));
        }
        timed(&mut self.stages.video_send_frame, || {
            self.video_encoder.send_frame(None)
        })
        .map_err(encode_adapter_error)?;
        self.drain_video_packets(true)?;
        self.write_ready_packets(true)?;
        beat_encode_call();
        self.output
            .as_mut()
            .ok_or_else(|| EncodeError::Adapter("muxer is already closed".into()))?
            .write_trailer()
            .map_err(encode_adapter_error)?;
        // A write error reported only at close fails the export here, before the rename.
        if let Some(output) = self.output.take() {
            beat_encode_call();
            output
                .close()
                .map_err(|error| encode_adapter_error_at("closing the output", error))?;
        }
        Ok(())
    }
}

impl Drop for NativeAvEncodeSink {
    fn drop(&mut self) {
        if !self.complete {
            let _ = self.cleanup();
        }
    }
}

impl NativeWavEncodeSink {
    /// Creates a non-overwriting sibling-partial WAV output of `channels` in
    /// the count's default layout.
    ///
    /// # Errors
    /// Returns an error for invalid PCM format, existing paths, or filesystem
    /// failures.
    pub fn create(
        path: impl Into<OutputTarget>,
        sample_rate: u32,
        channels: u16,
    ) -> Result<Self, EncodeError> {
        let speakers = SpeakerLayout::default_for(usize::from(channels))
            .ok_or_else(|| EncodeError::Adapter("invalid WAV format".into()))?;
        Self::create_with_speakers(path, sample_rate, speakers)
    }

    /// Creates a WAV output whose blocks feed `speakers`.
    ///
    /// The count's default layout gets a plain header, which every reader
    /// takes as that layout. Any other is written as `WAVE_FORMAT_EXTENSIBLE`
    /// with its channel mask, the planes in the mask's bit order, which WAVE
    /// requires.
    ///
    /// # Errors
    /// Returns an error for invalid PCM format, a layout a WAVE channel mask
    /// cannot say (an unnamed or repeated speaker, or one past the 18 WAVE
    /// positions), existing paths, or filesystem failures.
    pub fn create_with_speakers(
        path: impl Into<OutputTarget>,
        sample_rate: u32,
        speakers: SpeakerLayout,
    ) -> Result<Self, EncodeError> {
        let channels = u16::try_from(speakers.channels())
            .ok()
            .filter(|_| sample_rate != 0 && speakers.channels() <= MAX_AUDIO_CHANNELS)
            .ok_or_else(|| EncodeError::Adapter("invalid WAV format".into()))?;
        let (channel_mask, plane_order) = wav_channel_layout(&speakers)?;
        let target = path.into();
        let partial = PartialOutput::claim(&target, SeekNeed::Required)?;
        let mut file = BufWriter::with_capacity(WAV_WRITE_BUFFER, partial.open_file()?);
        file.write_all(&wav_header_with_mask(
            sample_rate,
            channels,
            channel_mask,
            0,
        )?)
        .map_err(encode_adapter_error)?;
        Ok(Self {
            file: Some(file),
            partial,
            published: None,
            sample_rate,
            channels,
            speakers,
            channel_mask,
            plane_order,
            frames_written: 0,
            complete: false,
            interleaved: Vec::new(),
        })
    }

    pub(crate) fn write_block(&mut self, block: &AudioBlock) -> Result<(), EncodeError> {
        if block.sample_rate != self.sample_rate
            || block.channels.len() != usize::from(self.channels)
            || block.channels.is_empty()
        {
            return Err(EncodeError::Adapter("audio block format changed".into()));
        }
        if block.speakers != self.speakers {
            return Err(EncodeError::Adapter("audio block speakers changed".into()));
        }
        let frames = block.channels[0].len();
        if block.channels.iter().any(|channel| channel.len() != frames) {
            return Err(EncodeError::Adapter("audio channel lengths differ".into()));
        }
        if u64::try_from(block.start_sample).ok() != Some(self.frames_written) {
            return Err(EncodeError::InvalidTimestamp);
        }
        // Refused before writing, rather than after writing 4 GiB that is then removed.
        let frames_after = self
            .frames_written
            .saturating_add(u64::try_from(frames).unwrap_or(u64::MAX));
        self.data_bytes(frames_after)?;
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| EncodeError::Adapter("WAV sink is closed".into()))?;
        self.interleaved.clear();
        for frame in 0..frames {
            for plane in &self.plane_order {
                self.interleaved
                    .extend_from_slice(&block.channels[*plane][frame].to_le_bytes());
            }
        }
        file.write_all(&self.interleaved)
            .map_err(encode_adapter_error)?;
        self.frames_written = self
            .frames_written
            .saturating_add(u64::try_from(frames).unwrap_or(u64::MAX));
        Ok(())
    }

    /// The `data` chunk's size once `frames` frames are written, when the
    /// RIFF chunk holding it stays within its 32-bit size.
    fn data_bytes(&self, frames: u64) -> Result<u32, EncodeError> {
        let overhead = if self.channel_mask.is_some() {
            WAV_EXTENSIBLE_OVERHEAD
        } else {
            WAV_PLAIN_OVERHEAD
        };
        #[cfg(test)]
        let limit = RIFF_LIMIT.with(std::cell::Cell::get);
        #[cfg(not(test))]
        let limit = u64::from(u32::MAX);
        frames
            .checked_mul(u64::from(self.channels))
            .and_then(|samples| samples.checked_mul(4))
            .filter(|bytes| bytes.saturating_add(u64::from(overhead)) <= limit)
            .and_then(|bytes| u32::try_from(bytes).ok())
            .ok_or_else(|| {
                EncodeError::Adapter(format!(
                    "{frames} frames of {} channels are more than the 4 GiB a WAV file holds",
                    self.channels
                ))
            })
    }

    /// How the finished file was published; `None` until it is.
    #[must_use]
    pub fn publish_report(&self) -> Option<PublishReport> {
        self.published
    }

    pub(crate) fn commit(&mut self, cancel: &CancelToken) -> Result<(), EncodeError> {
        let data_bytes = self.data_bytes(self.frames_written)?;
        let header = wav_header_with_mask(
            self.sample_rate,
            self.channels,
            self.channel_mask,
            data_bytes,
        )?;
        let mut file = self
            .file
            .take()
            .ok_or_else(|| EncodeError::Adapter("WAV sink is closed".into()))?;
        file.seek(SeekFrom::Start(0))
            .and_then(|_| file.write_all(&header))
            .and_then(|()| file.flush())
            .map_err(|error| self.partial.explain(encode_adapter_error(error)))?;
        drop(file);
        self.published = Some(self.partial.finish(cancel)?);
        self.complete = true;
        Ok(())
    }
}

impl AudioBlockSink for NativeWavEncodeSink {
    fn write_audio(&mut self, block: &AudioBlock, cancel: &CancelToken) -> Result<(), EncodeError> {
        if cancel.is_cancelled() {
            self.abort_audio()?;
            return Err(EncodeError::Cancelled);
        }
        self.write_block(block)
            .map_err(|error| self.partial.explain(error))
    }
    fn finish_audio(&mut self, cancel: &CancelToken) -> Result<(), EncodeError> {
        if cancel.is_cancelled() {
            self.abort_audio()?;
            return Err(EncodeError::Cancelled);
        }
        self.commit(cancel)
    }
    fn abort_audio(&mut self) -> Result<(), EncodeError> {
        // Close without writing out samples the file is about to lose.
        drop(self.file.take().map(BufWriter::into_parts));
        self.partial.discard()
    }
}

impl EncodeSink for NativeWavEncodeSink {
    fn write(&mut self, event: EncodeEvent, cancel: &CancelToken) -> Result<(), EncodeError> {
        match event {
            EncodeEvent::Audio(block) => self.write_audio(&block, cancel),
            EncodeEvent::Video(_) => {
                Err(EncodeError::Adapter("WAV sink accepts audio only".into()))
            }
        }
    }
    fn finish(&mut self, cancel: &CancelToken) -> Result<(), EncodeError> {
        self.finish_audio(cancel)
    }
    fn abort(&mut self) -> Result<(), EncodeError> {
        self.abort_audio()
    }
}

impl Drop for NativeWavEncodeSink {
    fn drop(&mut self) {
        if !self.complete {
            let _ = self.abort_audio();
        }
    }
}

/// The channel mask a WAV of `speakers` is written with, `None` for the
/// count's default layout, and the plane each interleaved channel comes from.
fn wav_channel_layout(speakers: &SpeakerLayout) -> Result<(Option<u32>, Vec<usize>), EncodeError> {
    let identity = (0..speakers.channels()).collect::<Vec<_>>();
    if SpeakerLayout::default_for(speakers.channels()) == Some(*speakers) {
        return Ok((None, identity));
    }
    let mut mask = 0_u32;
    for speaker in speakers.speakers() {
        let bit = speaker.ffmpeg_channel();
        if bit >= 18 || mask & (1 << bit) != 0 {
            return Err(EncodeError::Adapter(format!(
                "a WAVE channel mask cannot say the speakers {}",
                speakers.describe()
            )));
        }
        mask |= 1 << bit;
    }
    let mut order = identity;
    order.sort_by_key(|plane| speakers.speakers()[*plane].ffmpeg_channel());
    Ok((Some(mask), order))
}

/// The RIFF chunk's size beyond its `data` chunk's samples, plain header.
const WAV_PLAIN_OVERHEAD: u32 = 36;
/// The same with a `WAVE_FORMAT_EXTENSIBLE` header.
const WAV_EXTENSIBLE_OVERHEAD: u32 = 60;

#[cfg(test)]
thread_local! {
    /// The most bytes a RIFF chunk holds, for WAV sinks on this thread: a
    /// test lowers it from `u32::MAX`.
    pub(crate) static RIFF_LIMIT: std::cell::Cell<u64> =
        const { std::cell::Cell::new(u32::MAX as u64) };
}

fn riff_size(data_bytes: u32, overhead: u32) -> Result<u32, EncodeError> {
    data_bytes
        .checked_add(overhead)
        .ok_or_else(|| EncodeError::Adapter("WAV exceeds the RIFF size limit".into()))
}

/// A WAV header: a plain IEEE-float one, or `WAVE_FORMAT_EXTENSIBLE` with
/// `channel_mask` and the IEEE-float subformat.
///
/// # Errors
/// When the RIFF chunk's size does not fit its 32 bits.
pub(crate) fn wav_header_with_mask(
    sample_rate: u32,
    channels: u16,
    channel_mask: Option<u32>,
    data_bytes: u32,
) -> Result<Vec<u8>, EncodeError> {
    let Some(mask) = channel_mask else {
        return Ok(wav_header(sample_rate, channels, data_bytes)?.to_vec());
    };
    let plain = wav_header(sample_rate, channels, 0)?;
    let mut header = Vec::with_capacity(68);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&riff_size(data_bytes, WAV_EXTENSIBLE_OVERHEAD)?.to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&40_u32.to_le_bytes());
    header.extend_from_slice(&0xfffe_u16.to_le_bytes());
    // Channels, rate, byte rate, block align and bits per sample, as the plain header has them.
    header.extend_from_slice(&plain[22..36]);
    header.extend_from_slice(&22_u16.to_le_bytes());
    header.extend_from_slice(&32_u16.to_le_bytes());
    header.extend_from_slice(&mask.to_le_bytes());
    // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT.
    header.extend_from_slice(&[
        0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b,
        0x71,
    ]);
    header.extend_from_slice(b"data");
    header.extend_from_slice(&data_bytes.to_le_bytes());
    Ok(header)
}

/// A plain IEEE-float WAV header.
///
/// # Errors
/// When the RIFF chunk's size does not fit its 32 bits.
pub(crate) fn wav_header(
    sample_rate: u32,
    channels: u16,
    data_bytes: u32,
) -> Result<[u8; 44], EncodeError> {
    let mut header = [0_u8; 44];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&riff_size(data_bytes, WAV_PLAIN_OVERHEAD)?.to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16_u32.to_le_bytes());
    header[20..22].copy_from_slice(&3_u16.to_le_bytes());
    header[22..24].copy_from_slice(&channels.to_le_bytes());
    header[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    let byte_rate = sample_rate
        .saturating_mul(u32::from(channels))
        .saturating_mul(4);
    header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    header[32..34].copy_from_slice(&channels.saturating_mul(4).to_le_bytes());
    header[34..36].copy_from_slice(&32_u16.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&data_bytes.to_le_bytes());
    Ok(header)
}

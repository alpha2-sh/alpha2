//! Video-only encode sink.

use std::fs;
use std::path::{Path, PathBuf};

use crate::capabilities::{
    NativeVideoCodec, NativeVideoEncoderSelection, VideoEncoderOpenRequest, open_video_encoder,
};
use crate::convert::{
    codec_thread_budget, configure_rgba_to_yuv, encode_adapter_error, encode_adapter_error_at,
    tick_base,
};
use crate::encode::{EncodeStageTimings, timed};
use crate::partial::PartialOutput;
use alpha2_ffmpeg_bridge::OutputFile;
use alpha2_ffmpeg_bridge::Yuv420pFrameBuffer;
use alpha2_media::{
    CancelToken, ColorMatrix, ColorMetadata, Container, DecodedVideoFrame, EncodeError,
    EncodeEvent, EncodeSink, Orientation, PixelFormat, SampleAspectRatio, VideoRateControl,
};
use alpha2_time::{FrameRate, MediaTime};
use rsmpeg::avcodec::AVCodecContext;
use rsmpeg::avutil::{AVFrameWithImage, AVImage, AVRational, av_rescale_q, ra};
use rsmpeg::error::RsmpegError;
use rsmpeg::ffi;
use rsmpeg::swscale::SwsContext;

/// Configuration for deterministic native video encoding.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct VideoEncodeConfig {
    /// Encoded frame width; 4:2:0 codecs require an even value.
    pub width: u32,
    /// Encoded frame height; 4:2:0 codecs require an even value.
    pub height: u32,
    /// Constant output frame rate.
    pub frame_rate: FrameRate,
    /// SDR output color tags.
    pub color: ColorMetadata,
    /// Resolved rate control. A sink encodes from these numbers and never
    /// from a quality level; the host resolves the level before this point.
    pub rate_control: VideoRateControl,
    /// Video codec selected by the downstream host.
    pub codec: NativeVideoCodec,
    /// How the pictures are shown, which the container says beside them.
    pub display: VideoDisplay,
    /// The file format the output is written in, which must offer `codec`.
    pub container: Container,
}

/// How encoded pictures are shown: what a container records beside them
/// rather than in them.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct VideoDisplay {
    /// Turned and mirrored as this says (MP4's `tkhd` display matrix).
    pub orientation: Orientation,
    /// Each pixel this wide for its height (the bitstream's and the
    /// container's sample aspect ratio).
    pub sample_aspect_ratio: SampleAspectRatio,
}

/// Refuses an output this sink cannot write, saying which condition fails.
pub(crate) fn check_video_config(config: &VideoEncodeConfig) -> Result<(), EncodeError> {
    let refuse = |reason: String| Err(EncodeError::Adapter(reason));
    if !config.container.offers_video(config.codec.model()) {
        return refuse(format!(
            "{} does not offer video codec {}",
            config.container,
            config.codec.as_str()
        ));
    }
    if config.width == 0
        || config.height == 0
        || !config.width.is_multiple_of(2)
        || !config.height.is_multiple_of(2)
    {
        return refuse(format!(
            "a {}x{} output is refused: 4:2:0 video needs an even, non-zero width and height",
            config.width, config.height
        ));
    }
    if config.rate_control.bitrate == 0 {
        return refuse("the output bitrate must be positive".into());
    }
    if config.color.is_hdr() {
        return refuse(format!(
            "the output is HDR ({:?}), and native encoding writes SDR only",
            config.color.transfer
        ));
    }
    if config.color.matrix == ColorMatrix::Rgb {
        return refuse(
            "the output is tagged as RGB, and 4:2:0 video is Y'CbCr: tag the matrix it is encoded with"
                .into(),
        );
    }
    let sar = config.display.sample_aspect_ratio;
    if sar.num == 0 || sar.den == 0 || i32::try_from(sar.num.max(sar.den)).is_err() {
        return refuse(format!(
            "the pixel aspect ratio {}:{} is out of range",
            sar.num, sar.den
        ));
    }
    Ok(())
}

/// Writes what `container` says beside an encoder's pictures on its stream:
/// the display matrix, the pixel aspect ratio, and the codec tag.
pub(crate) fn describe_output_stream(
    stream: &mut rsmpeg::avformat::AVStreamMut<'_>,
    encoder: &AVCodecContext,
    display: VideoDisplay,
    container: Container,
) -> Result<(), EncodeError> {
    let sar = display.sample_aspect_ratio;
    if sar != SampleAspectRatio::default() {
        stream.set_sample_aspect_ratio(ra(
            i32::try_from(sar.num).unwrap_or(1),
            i32::try_from(sar.den).unwrap_or(1),
        ));
    }
    if let Some(matrix) = crate::convert::display_of(display.orientation) {
        alpha2_ffmpeg_bridge::set_stream_display_matrix(stream, matrix)
            .map_err(encode_adapter_error)?;
    }
    let tag = crate::convert::container_codec_tag(
        container,
        encoder.codec_id,
        stream.codecpar().codec_tag,
        false,
    );
    alpha2_ffmpeg_bridge::set_stream_codec_tag(stream, tag);
    Ok(())
}

/// Transactional `FFmpeg` video encoder and muxer.
///
/// Frames are encoded into a sibling partial file. Only a successful encoder
/// drain and trailer write renames it to the requested destination.
pub struct NativeVideoEncodeSink {
    pub(crate) final_path: PathBuf,
    pub(crate) partial_path: PathBuf,
    /// Held until the partial file is renamed into place or removed.
    pub(crate) _partial_lock: crate::partial::PartialLock,
    pub(crate) output: Option<OutputFile>,
    pub(crate) encoder: AVCodecContext,
    pub(crate) stream_index: i32,
    pub(crate) encoder_time_base: AVRational,
    /// One frame in `encoder_time_base`.
    pub(crate) frame_units: i64,
    pub(crate) stream_time_base: AVRational,
    pub(crate) scaler: SwsContext,
    /// Turns decoded YUV of any layout into the encoder's 4:2:0.
    pub(crate) yuv_input: crate::convert::Yuv420pConverter,
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) last_pts: Option<i64>,
    pub(crate) complete: bool,
    pub(crate) selection: NativeVideoEncoderSelection,
    pub(crate) stages: EncodeStageTimings,
}

impl NativeVideoEncodeSink {
    /// Creates a sink that encodes `config.codec` into a `config.container` file at `path`.
    ///
    /// # Errors
    /// Returns an adapter failure for invalid dimensions, paths, codecs, or muxers.
    pub fn create(path: impl AsRef<Path>, config: VideoEncodeConfig) -> Result<Self, EncodeError> {
        Self::create_selected(path, config, None, &codec_thread_budget)
    }

    /// Creates a concrete encoder selected by its `FFmpeg` short name.
    ///
    /// # Errors
    /// Returns an adapter failure when the name is unavailable, implements a
    /// different codec, or the remaining configuration is invalid.
    pub fn create_with_encoder(
        path: impl AsRef<Path>,
        config: VideoEncodeConfig,
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
    /// what makes the open-time fallback reachable: a caller that names an
    /// encoder gets that one or an error.
    ///
    /// # Errors
    /// Returns an adapter failure when no candidate opens or the output
    /// configuration is invalid.
    pub fn create_with_encoder_threads(
        path: impl AsRef<Path>,
        config: VideoEncodeConfig,
        encoder_name: Option<&str>,
        software_threads: &dyn Fn() -> i64,
    ) -> Result<Self, EncodeError> {
        Self::create_selected(path, config, encoder_name, software_threads)
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn create_selected(
        path: impl AsRef<Path>,
        config: VideoEncodeConfig,
        encoder_name: Option<&str>,
        software_threads: &dyn Fn() -> i64,
    ) -> Result<Self, EncodeError> {
        check_video_config(&config)?;
        let final_path = path.as_ref().to_owned();
        let partial = PartialOutput::claim(&final_path)?;
        let partial_path = partial.path().to_owned();
        let mut output = partial.open_muxer(config.container)?;
        let width = i32::try_from(config.width)
            .map_err(|_| EncodeError::Adapter("width is out of range".into()))?;
        let height = i32::try_from(config.height)
            .map_err(|_| EncodeError::Adapter("height is out of range".into()))?;
        let (encoder, selection) = open_video_encoder(&VideoEncoderOpenRequest {
            config: &config,
            requested_name: encoder_name,
            threads: crate::capabilities::SoftwareEncoderThreads::new(software_threads),
            global_header: output.oformat().flags
                & i32::try_from(ffi::AVFMT_GLOBALHEADER).unwrap_or(0)
                != 0,
            width,
            height,
        })?;
        // The frame length, or the finest unit an encoder that cannot hold it takes.
        let encoder_time_base = encoder.time_base;
        let mut stream_time_base;
        let stream_index;
        {
            let mut stream = output.new_stream();
            stream_time_base = encoder_time_base;
            stream.set_time_base(stream_time_base);
            let parameters = encoder.extract_codecpar();
            stream.set_codecpar(parameters);
            describe_output_stream(&mut stream, &encoder, config.display, config.container)?;
            stream_index = stream.index;
        }
        output
            .write_header(&mut crate::convert::muxer_header_options(config.container))
            .map_err(encode_adapter_error)?;
        stream_time_base = output.streams()[usize::try_from(stream_index).unwrap_or(0)].time_base;
        let mut scaler = SwsContext::get_context(
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
        configure_rgba_to_yuv(&mut scaler, config.color).map_err(encode_adapter_error)?;
        let partial_lock = partial.keep();
        Ok(Self {
            final_path,
            partial_path,
            _partial_lock: partial_lock,
            output: Some(output),
            encoder,
            stream_index,
            encoder_time_base,
            frame_units: crate::mux::frame_units(config.frame_rate, encoder_time_base),
            stream_time_base,
            scaler,
            yuv_input: crate::convert::Yuv420pConverter::new(config.color),
            width,
            height,
            last_pts: None,
            complete: false,
            selection,
            stages: EncodeStageTimings::default(),
        })
    }

    /// Where this sink's time has gone so far.
    #[must_use]
    pub fn stage_timings(&self) -> EncodeStageTimings {
        self.stages
    }

    /// The encoder this sink opened, and the candidates it passed over.
    #[must_use]
    pub fn selection(&self) -> &NativeVideoEncoderSelection {
        &self.selection
    }

    pub(crate) fn drain_packets(&mut self, flushing: bool) -> Result<(), EncodeError> {
        loop {
            let received = timed(&mut self.stages.video_receive_packet, || {
                self.encoder.receive_packet()
            });
            match received {
                Ok(mut packet) => {
                    packet.set_stream_index(self.stream_index);
                    crate::mux::fill_frame_duration(&mut packet, self.frame_units);
                    packet.rescale_ts(self.encoder_time_base, self.stream_time_base);
                    let output = self
                        .output
                        .as_mut()
                        .ok_or_else(|| EncodeError::Adapter("muxer is already closed".into()))?;
                    timed(&mut self.stages.video_mux_write, || {
                        output.interleaved_write_frame(&mut packet)
                    })
                    .map_err(|error| encode_adapter_error_at("packet write failed", error))?;
                }
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
    /// An [`EncodeEvent::Video`] written to the sink takes engine planes and
    /// allocates an `AVFrame` to copy them into, on this thread: the encode
    /// worker, which is the saturated one. A caller that packed its own output
    /// straight into an encoder frame hands that frame over instead.
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
        let pts = av_rescale_q(pts.ticks(), tick_base(), self.encoder_time_base);
        if self.last_pts.is_some_and(|previous| pts <= previous) {
            return Err(EncodeError::InvalidTimestamp);
        }
        frame.set_presentation(pts, self.encoder_time_base);
        timed(&mut self.stages.video_send_frame, || {
            self.encoder.send_frame(Some(frame.frame()))
        })
        .map_err(|error| {
            encode_adapter_error_at("video encoder rejected a YUV420P frame", error)
        })?;
        self.last_pts = Some(pts);
        self.drain_packets(false)
    }

    pub(crate) fn write_video(&mut self, frame: &DecodedVideoFrame) -> Result<(), EncodeError> {
        if frame.visible_rect.width != u32::try_from(self.width).unwrap_or(0)
            || frame.visible_rect.height != u32::try_from(self.height).unwrap_or(0)
        {
            return Err(EncodeError::Adapter(
                "encoder input dimensions do not match its configuration".into(),
            ));
        }
        let pts = av_rescale_q(frame.pts.ticks(), tick_base(), self.encoder_time_base);
        if self.last_pts.is_some_and(|previous| pts <= previous) {
            return Err(EncodeError::InvalidTimestamp);
        }
        if let Some(mut yuv) = self.yuv_input.convert(frame, self.width, self.height)? {
            yuv.set_pts(pts);
            yuv.set_time_base(self.encoder_time_base);
            timed(&mut self.stages.video_send_frame, || {
                self.encoder.send_frame(Some(&yuv))
            })
            .map_err(|error| {
                encode_adapter_error_at("video encoder rejected a YUV420P frame", error)
            })?;
            self.last_pts = Some(pts);
            return self.drain_packets(false);
        }
        if frame.pixel_format != PixelFormat::Rgba8 || frame.planes.len() != 1 {
            return Err(EncodeError::Adapter(
                "encoder expects YUV planes or one tightly packed RGBA8 plane".into(),
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
        self.scaler
            .scale_frame(&rgba, 0, self.height, &mut yuv)
            .map_err(encode_adapter_error)?;
        yuv.set_pts(pts);
        yuv.set_time_base(self.encoder_time_base);
        timed(&mut self.stages.video_send_frame, || {
            self.encoder.send_frame(Some(&yuv))
        })
        .map_err(|error| {
            encode_adapter_error_at("video encoder rejected a converted YUV420P frame", error)
        })?;
        self.last_pts = Some(pts);
        self.drain_packets(false)
    }
}

impl EncodeSink for NativeVideoEncodeSink {
    fn write(&mut self, event: EncodeEvent, cancel: &CancelToken) -> Result<(), EncodeError> {
        if cancel.is_cancelled() {
            self.abort()?;
            return Err(EncodeError::Cancelled);
        }
        match event {
            EncodeEvent::Video(frame) => self.write_video(&frame),
            EncodeEvent::Audio(_) => Err(EncodeError::Adapter(
                "this sink was configured for video-only output".into(),
            )),
        }
    }

    fn finish(&mut self, cancel: &CancelToken) -> Result<(), EncodeError> {
        if cancel.is_cancelled() {
            self.abort()?;
            return Err(EncodeError::Cancelled);
        }
        timed(&mut self.stages.video_send_frame, || {
            self.encoder.send_frame(None)
        })
        .map_err(encode_adapter_error)?;
        self.drain_packets(true)?;
        crate::encode::beat_encode_call();
        self.output
            .as_mut()
            .ok_or_else(|| EncodeError::Adapter("muxer is already closed".into()))?
            .write_trailer()
            .map_err(encode_adapter_error)?;
        // A write error reported only at close fails the export here, before the rename.
        if let Some(output) = self.output.take() {
            crate::encode::beat_encode_call();
            output
                .close()
                .map_err(|error| encode_adapter_error_at("closing the output", error))?;
        }
        crate::partial::publish(&self.partial_path, &self.final_path)?;
        self.complete = true;
        Ok(())
    }

    fn abort(&mut self) -> Result<(), EncodeError> {
        drop(self.output.take());
        match fs::remove_file(&self.partial_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(EncodeError::Adapter(error.to_string())),
        }
    }
}

impl Drop for NativeVideoEncodeSink {
    fn drop(&mut self) {
        if !self.complete {
            drop(self.output.take());
            let _ = fs::remove_file(&self.partial_path);
        }
    }
}

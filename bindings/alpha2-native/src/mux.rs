//! The transactional output file GPU-resident and packet-copy exports mux into.
//!
//! Video packets come from a hardware encoder or a packet copy; audio is encoded on its own thread
//! or copied. Only a successful [`Muxer::close`] renames the partial file into place.

use std::collections::VecDeque;

use alpha2_ffmpeg_bridge::OutputFile;
use alpha2_media::{CancelToken, Container, EncodeError};
use rsmpeg::avcodec::{AVCodecContext, AVCodecParameters, AVPacket};
use rsmpeg::avutil::AVRational;
use rsmpeg::ffi;

use crate::audio_input::{
    AudioStreamConfig, EncoderAudioInput, OpenedAudioEncoder, open_audio_encoder,
};
use crate::convert::{encode_adapter_error, encode_adapter_error_at};
use crate::encode::{
    AudioCommand, AudioEncodeThread, AudioEncoderConfig, EncodeStageTimings, QueuedPacket,
    spawn_audio_encoder, timed,
};
use crate::partial::{OutputTarget, PartialOutput, PublishReport, SeekNeed};

pub(crate) struct VideoStream {
    /// Codec parameters, sample description included.
    pub(crate) parameters: AVCodecParameters,
    pub(crate) time_base: AVRational,
    /// The parameters came from another file, whose codec tag may not suit this container.
    pub(crate) copied: bool,
}

pub(crate) enum AudioStream {
    /// Encoded here, from audio blocks.
    Encode(AudioStreamConfig),
    /// Packets copied from another file.
    Copy {
        parameters: AVCodecParameters,
        time_base: AVRational,
    },
}

enum MuxedAudioTrack {
    Encoded(MuxedAudio),
    Copied {
        stream_index: i32,
        source_time_base: AVRational,
        stream_time_base: AVRational,
    },
}

/// A transactional output with one video stream and at most one audio stream.
pub(crate) struct Muxer {
    format: Option<OutputFile>,
    /// The partial file `format` writes. Declared after `format`, which closes first.
    partial: PartialOutput,
    published: Option<PublishReport>,
    stream_index: i32,
    /// The container's video time base; the encoder's until a deferred header is written.
    stream_time_base: AVRational,
    complete: bool,
    audio: Option<MuxedAudioTrack>,
    audio_stream_index: i32,
    /// Finished audio packets from the audio thread; they wait here before a deferred header.
    audio_packets: VecDeque<QueuedPacket>,
    /// The header [`Self::open_deferred`] holds back until the first video
    /// packet; `None` once written.
    pending_header: Option<PendingHeader>,
    /// The parameter sets a deferred header was written with, which every repeated set must equal.
    parameter_sets: Option<Vec<u8>>,
    /// The audio thread's rescale target and the stream's time base, when the thread started before
    /// a deferred header fixed the latter.
    audio_rescale: Option<(AVRational, AVRational)>,
}

struct MuxedAudio {
    thread: AudioEncodeThread,
    input: EncoderAudioInput,
}

impl Muxer {
    /// Opens a transactional `container` file beside `path` for an encoder's stream.
    ///
    /// `video` must have opened with global headers, since every export container carries
    /// parameter sets in its header.
    ///
    /// # Errors
    /// As [`Self::open_streams`].
    // Used by the Vulkan and VideoToolbox backends; D3D12VA uses [`Self::open_deferred`].
    #[cfg_attr(
        not(all(feature = "hwencode", any(target_os = "linux", target_os = "macos"))),
        allow(dead_code)
    )]
    pub(crate) fn open(
        path: impl Into<OutputTarget>,
        container: Container,
        video: &AVCodecContext,
        video_time_base: AVRational,
        audio: Option<AudioStreamConfig>,
    ) -> Result<Self, EncodeError> {
        Self::open_streams(
            path,
            container,
            VideoStream {
                parameters: video.extract_codecpar(),
                time_base: video_time_base,
                copied: false,
            },
            audio.map(AudioStream::Encode),
        )
    }

    /// Opens a transactional `container` file beside `path` and writes its header.
    ///
    /// # Errors
    /// Returns an adapter failure for an existing output, a partial a running export holds, a
    /// codec the container does not offer, or a muxer that cannot be created.
    pub(crate) fn open_streams(
        path: impl Into<OutputTarget>,
        container: Container,
        video: VideoStream,
        audio: Option<AudioStream>,
    ) -> Result<Self, EncodeError> {
        let mut unwritten = Unwritten::open(path, container, video, audio)?;
        unwritten
            .format
            .write_header(&mut crate::convert::muxer_header_options(container))
            .map_err(encode_adapter_error)?;
        let stream_time_base = unwritten.time_base(unwritten.stream_index);
        let audio_stream_time_base = unwritten.time_base(unwritten.audio_stream_index);
        unwritten.start(stream_time_base, audio_stream_time_base, None)
    }

    /// Opens a transactional `container` file beside `path` for an encoder that writes parameter
    /// sets in-band, behind an [`EncodedVideoFilter`](crate::bitstream::EncodedVideoFilter).
    ///
    /// The partial file exists from here, so later failures are the export's. The header waits
    /// for the first video packet's `AV_PKT_DATA_NEW_EXTRADATA`, since the encoder has no
    /// parameter sets before its first picture; audio packets wait in order meanwhile, so
    /// interleaving matches [`Self::open`].
    ///
    /// # Errors
    /// As [`Self::open_streams`], short of the header.
    // Used by the D3D12VA and Vulkan HEVC backends (Windows and Linux only) and the tests.
    #[cfg_attr(
        not(any(
            test,
            all(feature = "hwencode", any(target_os = "windows", target_os = "linux"))
        )),
        allow(dead_code)
    )]
    pub(crate) fn open_deferred(
        path: impl Into<OutputTarget>,
        container: Container,
        video: &AVCodecContext,
        video_time_base: AVRational,
        audio: Option<AudioStreamConfig>,
    ) -> Result<Self, EncodeError> {
        let unwritten = Unwritten::open(
            path,
            container,
            VideoStream {
                parameters: video.extract_codecpar(),
                time_base: video_time_base,
                copied: false,
            },
            audio.map(AudioStream::Encode),
        )?;
        // The audio thread must start before the header, which waits on video, but the
        // container fixes the audio time base only in the header, so the thread rescales
        // into its encoder's for now.
        let audio_time_base = unwritten
            .opened_audio
            .as_ref()
            .map(|opened| opened.encoder_time_base);
        unwritten.start(
            video_time_base,
            audio_time_base.unwrap_or(video_time_base),
            Some(PendingHeader {
                audio_time_base,
                container,
            }),
        )
    }

    /// Writes a deferred header with `packet`'s parameter sets as sample description,
    /// removing them from the packet.
    fn write_deferred_header(&mut self, packet: &mut AVPacket) -> Result<(), EncodeError> {
        let Some(pending) = self.pending_header.as_ref() else {
            return Ok(());
        };
        let parameter_sets =
            alpha2_ffmpeg_bridge::take_packet_side_data(packet, ffi::AV_PKT_DATA_NEW_EXTRADATA)
                .filter(|sets| !sets.is_empty())
                .map(|sets| distinct_parameter_sets(&sets))
                .ok_or_else(|| {
                    EncodeError::Adapter(
                        "the encoder's first packet carried no parameter sets for the header"
                            .into(),
                    )
                })?;
        let (audio_time_base, container) = (pending.audio_time_base, pending.container);
        let format = self
            .format
            .as_mut()
            .ok_or_else(|| EncodeError::Adapter("muxer is already closed".into()))?;
        alpha2_ffmpeg_bridge::set_output_stream_extradata(
            format,
            usize::try_from(self.stream_index).unwrap_or(0),
            &parameter_sets,
        )
        .map_err(|error| encode_adapter_error_at("storing the parameter sets", error))?;
        crate::encode::beat_encode_call();
        format
            .write_header(&mut crate::convert::muxer_header_options(container))
            .map_err(encode_adapter_error)?;
        self.stream_time_base =
            format.streams()[usize::try_from(self.stream_index).unwrap_or(0)].time_base;
        if let (Some(from), Some(MuxedAudioTrack::Encoded(_))) = (audio_time_base, &self.audio) {
            let to =
                format.streams()[usize::try_from(self.audio_stream_index).unwrap_or(0)].time_base;
            self.audio_rescale = Some((from, to));
        }
        self.pending_header = None;
        self.parameter_sets = Some(parameter_sets);
        Ok(())
    }

    /// Writes one copied audio packet, in its stream's opening time base.
    ///
    /// # Errors
    /// Returns an adapter failure when this output copies no audio, the muxer is
    /// closed, or the write fails.
    pub(crate) fn write_copied_audio(&mut self, packet: &mut AVPacket) -> Result<(), EncodeError> {
        self.write_copied_audio_packet(packet)
            .map_err(|error| self.partial.explain(error))
    }

    fn write_copied_audio_packet(&mut self, packet: &mut AVPacket) -> Result<(), EncodeError> {
        let Some(MuxedAudioTrack::Copied {
            stream_index,
            source_time_base,
            stream_time_base,
        }) = self.audio
        else {
            return Err(EncodeError::Adapter("this output copies no audio".into()));
        };
        packet.set_stream_index(stream_index);
        packet.rescale_ts(source_time_base, stream_time_base);
        self.format
            .as_mut()
            .ok_or_else(|| EncodeError::Adapter("muxer is already closed".into()))?
            .interleaved_write_frame(packet)
            .map_err(|error| encode_adapter_error_at("packet write failed", error))
    }

    /// Writes one encoder video packet, in `encoder_time_base`.
    ///
    /// Behind a deferred header, the first packet's parameter sets become the sample description
    /// and later repeats are stripped.
    ///
    /// # Errors
    /// Returns an adapter failure when the muxer is closed or the write fails, and
    /// behind a deferred header when the first packet has no parameter sets or a
    /// later one has different ones.
    pub(crate) fn write_video(
        &mut self,
        packet: &mut AVPacket,
        encoder_time_base: AVRational,
    ) -> Result<(), EncodeError> {
        self.write_video_packet(packet, encoder_time_base)
            .map_err(|error| self.partial.explain(error))
    }

    fn write_video_packet(
        &mut self,
        packet: &mut AVPacket,
        encoder_time_base: AVRational,
    ) -> Result<(), EncodeError> {
        if self.pending_header.is_some() {
            self.write_deferred_header(packet)?;
        } else if let Some(header) = self.parameter_sets.as_deref() {
            let repeated =
                alpha2_ffmpeg_bridge::take_packet_side_data(packet, ffi::AV_PKT_DATA_NEW_EXTRADATA);
            // Parameter sets live outside the bitstream, so a picture coded against other sets
            // would decode against the header's.
            if repeated.is_some_and(|sets| distinct_parameter_sets(&sets) != header) {
                return Err(EncodeError::Adapter(
                    "the encoder changed its parameter sets mid-stream, which one stream \
                     header cannot carry"
                        .into(),
                ));
            }
        }
        packet.set_stream_index(self.stream_index);
        // A hardware encoder's time base is the frame period.
        fill_frame_duration(packet, 1);
        packet.rescale_ts(encoder_time_base, self.stream_time_base);
        crate::encode::beat_encode_call();
        self.format
            .as_mut()
            .ok_or_else(|| EncodeError::Adapter("muxer is already closed".into()))?
            .interleaved_write_frame(packet)
            .map_err(|error| encode_adapter_error_at("packet write failed", error))
    }

    /// Buffers one audio block and hands every whole encoder frame to the thread.
    ///
    /// # Errors
    /// Returns an adapter failure for a format change, a non-monotonic block, or a codec failure
    /// the thread already reported.
    pub(crate) fn write_audio(
        &mut self,
        block: &alpha2_media::AudioBlock,
        stages: &mut EncodeStageTimings,
    ) -> Result<(), EncodeError> {
        let Some(MuxedAudioTrack::Encoded(state)) = self.audio.as_mut() else {
            return Err(EncodeError::Adapter(
                "this output encodes no audio stream".into(),
            ));
        };
        let frames = state.input.push(block)?;
        let covered = state.input.received_ticks();
        for frame in frames {
            state.thread.send(AudioCommand::Block(frame, covered))?;
        }
        // Drain here too: an audio-only stretch has no video packet to trigger it.
        self.write_ready_audio(false, stages)
    }

    /// Hands over the audio tail, lets the thread flush, and waits, so every audio packet exists
    /// before the final interleave.
    ///
    /// # Errors
    /// Returns the audio thread's failure.
    pub(crate) fn finish_audio(&mut self) -> Result<(), EncodeError> {
        let Some(MuxedAudioTrack::Encoded(state)) = self.audio.as_mut() else {
            return Ok(());
        };
        let covered = state.input.received_ticks();
        for frame in state.input.finish() {
            state.thread.send(AudioCommand::Block(frame, covered))?;
        }
        state.thread.send(AudioCommand::Finish(covered))?;
        state.thread.shutdown();
        if let Some(error) = state.thread.lock().failure.clone() {
            return Err(EncodeError::Adapter(error));
        }
        Ok(())
    }

    /// Collects what the audio thread has produced and writes it.
    ///
    /// `av_interleaved_write_frame` already orders both streams by DTS. `flushing` only reports
    /// the thread's failure sooner.
    ///
    /// # Errors
    /// Returns the audio thread's failure or a failed write.
    pub(crate) fn write_ready_audio(
        &mut self,
        flushing: bool,
        stages: &mut EncodeStageTimings,
    ) -> Result<(), EncodeError> {
        self.write_ready_audio_packets(flushing, stages)
            .map_err(|error| self.partial.explain(error))
    }

    fn write_ready_audio_packets(
        &mut self,
        flushing: bool,
        stages: &mut EncodeStageTimings,
    ) -> Result<(), EncodeError> {
        let Some(MuxedAudioTrack::Encoded(audio)) = self.audio.as_ref() else {
            return Ok(());
        };
        {
            let mut state = audio.thread.lock();
            if let Some(error) = state.failure.clone() {
                return Err(EncodeError::Adapter(error));
            }
            if flushing && !state.finished {
                return Err(EncodeError::Adapter(
                    "audio encoder stopped before it finished".into(),
                ));
            }
            self.audio_packets.extend(state.packets.drain(..));
        }
        // Nothing can be written before the header; the packets wait in order.
        if self.pending_header.is_some() {
            return Ok(());
        }
        let rescale = self.audio_rescale;
        while let Some(mut queued) = self.audio_packets.pop_front() {
            if let Some((from, to)) = rescale {
                queued.packet.rescale_ts(from, to);
            }
            let format = self
                .format
                .as_mut()
                .ok_or_else(|| EncodeError::Adapter("muxer is already closed".into()))?;
            timed(&mut stages.audio_mux_write, || {
                format.interleaved_write_frame(&mut queued.packet)
            })
            .map_err(|error| encode_adapter_error_at("packet write failed", error))?;
        }
        Ok(())
    }

    /// The audio thread's own codec timings, merged into `stages`.
    // Used by the hardware-encode backends.
    #[cfg_attr(
        not(all(
            feature = "hwencode",
            any(target_os = "linux", target_os = "macos", target_os = "windows")
        )),
        allow(dead_code)
    )]
    pub(crate) fn with_audio_timings(&self, stages: EncodeStageTimings) -> EncodeStageTimings {
        let Some(MuxedAudioTrack::Encoded(audio)) = self.audio.as_ref() else {
            return stages;
        };
        let thread = audio.thread.lock().stages;
        EncodeStageTimings {
            audio_send_frame: thread.audio_send_frame,
            audio_receive_packet: thread.audio_receive_packet,
            ..stages
        }
    }

    /// Writes the trailer, closes the file and publishes it ([`PartialOutput::finish`]).
    ///
    /// # Errors
    /// Returns an adapter failure when the trailer or close fails, or when a deferred header was
    /// never written (no video packet); the partial then goes. A failed publish keeps the finished
    /// file ([`EncodeError::Publish`]); `cancel` stopping its retries removes it.
    pub(crate) fn close(&mut self, cancel: &CancelToken) -> Result<(), EncodeError> {
        self.close_format()
            .map_err(|error| self.partial.explain(error))?;
        self.published = Some(self.partial.finish(cancel)?);
        self.complete = true;
        Ok(())
    }

    pub(crate) fn publish_report(&self) -> Option<PublishReport> {
        self.published
    }

    fn close_format(&mut self) -> Result<(), EncodeError> {
        if self.pending_header.is_some() {
            return Err(EncodeError::Adapter(
                "the encoder produced no parameter sets: no video packet reached the header".into(),
            ));
        }
        crate::encode::beat_encode_call();
        self.format
            .as_mut()
            .ok_or_else(|| EncodeError::Adapter("muxer is already closed".into()))?
            .write_trailer()
            .map_err(encode_adapter_error)?;
        // A write error reported only at close fails the export here, before the rename.
        if let Some(output) = self.format.take() {
            crate::encode::beat_encode_call();
            output
                .close()
                .map_err(|error| encode_adapter_error_at("closing the output", error))?;
        }
        Ok(())
    }

    /// Discards a partial output without writing a trailer, removing the partial file.
    pub(crate) fn abort(&mut self) {
        // Stop the audio thread first so nothing is queued against a closed output.
        if let Some(MuxedAudioTrack::Encoded(state)) = self.audio.as_mut() {
            state.thread.shutdown();
        }
        if !self.complete {
            drop(self.format.take());
            let _ = self.partial.discard();
        }
    }
}

impl Drop for Muxer {
    fn drop(&mut self) {
        self.abort();
    }
}

/// A transactional output whose streams exist but whose header is not written yet.
struct Unwritten {
    /// Declared before `partial` so a failed header closes the file before the partial is removed
    /// (Windows cannot remove an open file).
    format: OutputFile,
    partial: PartialOutput,
    stream_index: i32,
    audio_stream_index: i32,
    opened_audio: Option<OpenedAudioEncoder>,
    copied_audio: Option<AVRational>,
}

impl Unwritten {
    /// Claims the partial beside `path`, opens the muxer on it, and adds the streams.
    ///
    /// # Errors
    /// As [`Muxer::open_streams`], short of the header.
    fn open(
        path: impl Into<OutputTarget>,
        container: Container,
        video: VideoStream,
        audio: Option<AudioStream>,
    ) -> Result<Self, EncodeError> {
        let target = path.into();
        let partial = PartialOutput::claim(&target, SeekNeed::Required)?;
        let mut format = partial.open_muxer(container)?;
        let global_header =
            format.oformat().flags & i32::try_from(ffi::AVFMT_GLOBALHEADER).unwrap_or(0) != 0;
        // Parameter sets exist only in the header, so a container without global headers would get
        // an undecodable stream. Every export container has them; this guards the muxer table.
        if !global_header {
            return Err(EncodeError::Adapter(format!(
                "the muxer for {container} has no global header"
            )));
        }
        let stream_index;
        {
            let mut stream = format.new_stream();
            stream.set_time_base(video.time_base);
            let (codec_id, tag) = (video.parameters.codec_id, video.parameters.codec_tag);
            stream.set_codecpar(video.parameters);
            // Another container's tag need not suit this one; in MP4 and MOV HEVC is `hvc1`
            // unless a copy says otherwise.
            alpha2_ffmpeg_bridge::set_stream_codec_tag(
                &mut stream,
                crate::convert::container_codec_tag(container, codec_id, tag, video.copied),
            );
            stream_index = stream.index;
        }
        let mut opened_audio = None;
        let mut copied_audio = None;
        let mut audio_stream_index = 0_i32;
        match audio {
            Some(AudioStream::Encode(config)) => {
                if !container.offers_audio(config.codec.model()) {
                    return Err(EncodeError::Adapter(format!(
                        "{container} does not offer audio codec {}",
                        config.codec.as_str()
                    )));
                }
                let opened = open_audio_encoder(config, global_header)?;
                let mut stream = format.new_stream();
                stream.set_time_base(opened.encoder_time_base);
                stream.set_codecpar(opened.encoder.extract_codecpar());
                audio_stream_index = stream.index;
                opened_audio = Some(opened);
            }
            Some(AudioStream::Copy {
                parameters,
                time_base,
            }) => {
                let mut stream = format.new_stream();
                stream.set_time_base(time_base);
                let (codec_id, tag) = (parameters.codec_id, parameters.codec_tag);
                stream.set_codecpar(parameters);
                alpha2_ffmpeg_bridge::set_stream_codec_tag(
                    &mut stream,
                    crate::convert::container_codec_tag(container, codec_id, tag, true),
                );
                audio_stream_index = stream.index;
                copied_audio = Some(time_base);
            }
            None => {}
        }
        Ok(Self {
            format,
            partial,
            stream_index,
            audio_stream_index,
            opened_audio,
            copied_audio,
        })
    }

    fn time_base(&self, index: i32) -> AVRational {
        self.format.streams()[usize::try_from(index).unwrap_or(0)].time_base
    }

    /// Starts the audio thread, rescaling into `audio_stream_time_base`, and hands
    /// the partial to the muxer.
    ///
    /// # Errors
    /// Returns an adapter failure when the audio thread cannot start.
    fn start(
        self,
        stream_time_base: AVRational,
        audio_stream_time_base: AVRational,
        pending_header: Option<PendingHeader>,
    ) -> Result<Muxer, EncodeError> {
        // Bound after `partial`, so `format` drops first if the thread cannot start.
        let Self {
            partial,
            format,
            stream_index,
            audio_stream_index,
            opened_audio,
            copied_audio,
        } = self;
        // Started after the header, which fixes the stream index and time base, except for a
        // deferred header ([`Muxer::open_deferred`]).
        let audio = match (opened_audio, copied_audio) {
            (Some(opened), _) => Some(MuxedAudioTrack::Encoded(MuxedAudio {
                input: opened.input,
                thread: spawn_audio_encoder(AudioEncoderConfig {
                    encoder: opened.encoder,
                    stream_index: audio_stream_index,
                    encoder_time_base: opened.encoder_time_base,
                    stream_time_base: audio_stream_time_base,
                    sample_rate: opened.encoder_rate,
                    sample_format: opened.sample_format,
                })?,
            })),
            (None, Some(source_time_base)) => Some(MuxedAudioTrack::Copied {
                stream_index: audio_stream_index,
                source_time_base,
                stream_time_base: audio_stream_time_base,
            }),
            (None, None) => None,
        };
        Ok(Muxer {
            format: Some(format),
            partial,
            published: None,
            stream_index,
            stream_time_base,
            complete: false,
            audio,
            audio_stream_index,
            audio_packets: VecDeque::new(),
            pending_header,
            parameter_sets: None,
            audio_rescale: None,
        })
    }
}

/// What a deferred header ([`Muxer::open_deferred`]) still needs.
struct PendingHeader {
    /// The audio thread's rescale target until the header fixes the stream's, with encoded audio.
    audio_time_base: Option<AVRational>,
    container: Container,
}

/// Parameter sets as a sample description takes them: for Annex B (H.264, HEVC) each distinct NAL
/// unit once, in first-seen order, behind a four-byte start code; otherwise (AV1) unchanged.
///
/// Encoders may repeat a set within an access unit, and `extract_extradata` passes every copy, so
/// comparing as unit sets tells a repeat from a change.
fn distinct_parameter_sets(sets: &[u8]) -> Vec<u8> {
    if !(sets.starts_with(&[0, 0, 1]) || sets.starts_with(&[0, 0, 0, 1])) {
        return sets.to_vec();
    }
    let mut starts = Vec::new();
    let mut at = 0;
    while at + 3 <= sets.len() {
        if sets[at..at + 3] == [0, 0, 1] {
            starts.push(at + 3);
            at += 3;
        } else {
            at += 1;
        }
    }
    let mut units: Vec<&[u8]> = Vec::new();
    for (index, &start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).map_or(sets.len(), |next| next - 3);
        // A four-byte start code's leading zero, or trailing padding, belongs to no unit.
        let mut unit = &sets[start..end];
        while let [rest @ .., 0] = unit {
            unit = rest;
        }
        if !unit.is_empty() && !units.contains(&unit) {
            units.push(unit);
        }
    }
    units
        .into_iter()
        .flat_map(|unit| [&[0, 0, 0, 1][..], unit])
        .flatten()
        .copied()
        .collect()
}

/// Gives a video packet with no duration one frame (`frame_units` of its time base).
///
/// `FFmpeg` clears frame durations unless the encoder opened with
/// `AV_CODEC_FLAG_FRAME_DURATION`, and the MP4 muxer takes the last sample's duration from the
/// last packet, so without this the track is a frame short.
pub(crate) fn fill_frame_duration(packet: &mut AVPacket, frame_units: i64) {
    if packet.duration == 0 {
        packet.set_duration(frame_units);
    }
}

/// Units of `time_base` per frame at `rate`, rounded to nearest and at least one.
pub(crate) fn frame_units(rate: alpha2_time::FrameRate, time_base: AVRational) -> i64 {
    rsmpeg::avutil::av_rescale_q(
        1,
        rsmpeg::avutil::ra(
            i32::try_from(rate.denominator()).unwrap_or(1),
            i32::try_from(rate.numerator()).unwrap_or(i32::MAX),
        ),
        time_base,
    )
    .max(1)
}

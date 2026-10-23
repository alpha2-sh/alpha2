//! The transactional output file the GPU-resident and packet-copy exports
//! mux into, in any export container.
//!
//! Nothing here depends on where the video packets come from: a hardware
//! session hands over what its encoder produced, a packet copy what it read
//! from its source. This writes them beside an audio stream that is either
//! encoded on its own thread or copied packets. Every such export therefore
//! shares one partial-file contract: only a successful [`Muxer::close`]
//! renames the partial file into place, and anything else removes it.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use alpha2_ffmpeg_bridge::OutputFile;
use alpha2_media::{Container, EncodeError};
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
use crate::partial::PartialOutput;

/// The video stream a [`Muxer`] carries.
pub(crate) struct VideoStream {
    /// Its codec parameters, sample description included.
    pub(crate) parameters: AVCodecParameters,
    /// The time base its packets arrive in.
    pub(crate) time_base: AVRational,
    /// The parameters were read from another file, whose container codec tag
    /// is not necessarily this one's.
    pub(crate) copied: bool,
}

/// The audio stream a [`Muxer`] carries.
pub(crate) enum AudioStream {
    /// Encoded here, from audio blocks.
    Encode(AudioStreamConfig),
    /// Packets copied from another file.
    Copy {
        /// Their codec parameters.
        parameters: AVCodecParameters,
        /// The time base they arrive in.
        time_base: AVRational,
    },
}

/// The audio half of a muxed export.
enum MuxedAudioTrack {
    Encoded(MuxedAudio),
    Copied {
        stream_index: i32,
        source_time_base: AVRational,
        stream_time_base: AVRational,
    },
}

/// A transactional output with one video stream and at most one audio
/// stream.
pub(crate) struct Muxer {
    final_path: PathBuf,
    partial_path: PathBuf,
    /// Held until the partial file is renamed into place or removed.
    _partial_lock: crate::partial::PartialLock,
    format: Option<OutputFile>,
    stream_index: i32,
    /// The container's time base for the video stream; the encoder's until
    /// a deferred header is written.
    stream_time_base: AVRational,
    complete: bool,
    audio: Option<MuxedAudioTrack>,
    audio_stream_index: i32,
    /// Finished audio packets collected from the audio thread, on their way
    /// to the muxer. Before a deferred header they wait here.
    audio_packets: VecDeque<QueuedPacket>,
    /// The header [`Self::open_deferred`] holds back until the first video
    /// packet; `None` once it is written, or for a header written at open.
    pending_header: Option<PendingHeader>,
    /// The parameter sets a deferred header was written with, which every
    /// set the encoder repeats must equal; `None` for a header written at
    /// open.
    parameter_sets: Option<Vec<u8>>,
    /// The time base the audio thread rescales into, and the audio stream's,
    /// when the thread started before a deferred header fixed the latter.
    audio_rescale: Option<(AVRational, AVRational)>,
}

/// An audio stream encoded on its own thread.
struct MuxedAudio {
    thread: AudioEncodeThread,
    input: EncoderAudioInput,
}

impl Muxer {
    /// Opens a transactional `container` file beside `path` for an
    /// encoder's stream.
    ///
    /// `video` is the opened encoder, whose parameters describe the stream;
    /// it must have opened with global headers, because every export
    /// container carries the parameter sets in its header rather than
    /// in-band.
    ///
    /// # Errors
    /// As [`Self::open_streams`].
    // Used by the Vulkan and VideoToolbox backends; the D3D12VA one opens [`Self::open_deferred`].
    #[cfg_attr(
        not(all(feature = "hwencode", any(target_os = "linux", target_os = "macos"))),
        allow(dead_code)
    )]
    pub(crate) fn open(
        path: &Path,
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

    /// Opens a transactional `container` file beside `path` and writes its
    /// header.
    ///
    /// # Errors
    /// Returns an adapter failure for an existing output, a partial a running export holds, a
    /// codec the container does not offer, or a muxer that cannot be created.
    pub(crate) fn open_streams(
        path: &Path,
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

    /// Opens a transactional `container` file beside `path` for an encoder
    /// that writes its parameter sets in-band rather than as extradata,
    /// behind an [`EncodedVideoFilter`](crate::bitstream::EncodedVideoFilter).
    ///
    /// The partial file and its lock exist from here on, as with
    /// [`Self::open`], so a failure past this point is the export's.
    /// The header waits for the first video packet, which brings the
    /// parameter sets as `AV_PKT_DATA_NEW_EXTRADATA`: every export container
    /// writes them into its header (an MP4's sample description, a Matroska
    /// track's `CodecPrivate`), and the encoder has none before it has
    /// encoded a picture. Until then, audio packets wait in
    /// order, and nothing is written, so the interleave is decided exactly
    /// as it is for [`Self::open`].
    ///
    /// # Errors
    /// As [`Self::open_streams`], short of the header.
    // Used by the D3D12VA backend, which exists on Windows only, and by the tests.
    #[cfg_attr(
        not(any(test, all(feature = "hwencode", target_os = "windows"))),
        allow(dead_code)
    )]
    pub(crate) fn open_deferred(
        path: &Path,
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
        // The audio thread must run before the header, which waits on the video; the container
        // fixes the audio time base only in the header, so the thread rescales into its encoder's
        // for now.
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

    /// Writes a deferred header, with the parameter sets `packet` brings as
    /// its sample description, and takes them off the packet.
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

    /// Writes one copied audio packet, in the time base its stream was
    /// opened with.
    ///
    /// # Errors
    /// Returns an adapter failure when this output copies no audio, the
    /// muxer is closed, or the write fails.
    pub(crate) fn write_copied_audio(&mut self, packet: &mut AVPacket) -> Result<(), EncodeError> {
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

    /// Writes one video packet the encoder produced, in `encoder_time_base`.
    ///
    /// Behind a deferred header, the first packet's parameter sets become the
    /// sample description and the header is written before it. Every later
    /// packet that repeats them has them taken off, since they are already
    /// in the header.
    ///
    /// # Errors
    /// Returns an adapter failure when the muxer is closed or the write fails,
    /// and behind a deferred header when the first packet brings no parameter
    /// sets or a later one brings different ones.
    pub(crate) fn write_video(
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
            // would be decoded against the header's.
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

    /// Buffers one audio block and hands the thread every whole encoder
    /// frame it completes.
    ///
    /// # Errors
    /// Returns an adapter failure for a format change, a non-monotonic block,
    /// or a codec failure the thread has already reported.
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
        // An audio-only stretch has no video packet to trigger a drain, so the queue would grow.
        self.write_ready_audio(false, stages)
    }

    /// Hands the audio tail over, lets the thread flush its encoder, and
    /// waits for it, so every audio packet exists before the muxer decides
    /// the final interleave.
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
    /// `av_interleaved_write_frame` buffers both streams by DTS, so a separate
    /// ordering queue would duplicate the muxer's work. `flushing` only reports
    /// the thread's failure more eagerly; the thread has already been joined
    /// by then, so its queue is complete either way.
    ///
    /// # Errors
    /// Returns the audio thread's failure or a failed write.
    pub(crate) fn write_ready_audio(
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

    /// Writes the trailer and renames the partial output into place.
    ///
    /// # Errors
    /// Returns an adapter failure when the trailer or the rename fails, or
    /// when a deferred header was never written because no video packet
    /// arrived; the partial file then goes with the muxer.
    pub(crate) fn close(&mut self) -> Result<(), EncodeError> {
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
        crate::partial::publish(&self.partial_path, &self.final_path)?;
        self.complete = true;
        Ok(())
    }

    /// Discards a partial output without writing a trailer: the muxer is
    /// dropped where it stands and the partial file is removed, so nothing
    /// half-written is left behind under the caller's name.
    pub(crate) fn abort(&mut self) {
        // Stop the audio thread before the muxer goes, so nothing is queued against a closed
        // output.
        if let Some(MuxedAudioTrack::Encoded(state)) = self.audio.as_mut() {
            state.thread.shutdown();
        }
        if !self.complete {
            drop(self.format.take());
            let _ = fs::remove_file(&self.partial_path);
        }
    }
}

impl Drop for Muxer {
    fn drop(&mut self) {
        // A failed export leaves no half-written file, as the software sinks do.
        self.abort();
    }
}

/// A transactional output whose streams exist and whose header is not
/// written yet.
struct Unwritten {
    final_path: PathBuf,
    /// Declared before `partial`, so a failed header closes the file before
    /// the partial is removed: Windows removes no file that is still open.
    format: OutputFile,
    partial: PartialOutput,
    stream_index: i32,
    audio_stream_index: i32,
    opened_audio: Option<OpenedAudioEncoder>,
    copied_audio: Option<AVRational>,
}

impl Unwritten {
    /// Claims the partial file beside `path`, opens the muxer on it, and
    /// adds the streams.
    ///
    /// # Errors
    /// As [`Muxer::open_streams`], short of the header.
    fn open(
        path: &Path,
        container: Container,
        video: VideoStream,
        audio: Option<AudioStream>,
    ) -> Result<Self, EncodeError> {
        let final_path = path.to_owned();
        let partial = PartialOutput::claim(&final_path)?;
        let mut format = partial.open_muxer(container)?;
        let global_header =
            format.oformat().flags & i32::try_from(ffi::AVFMT_GLOBALHEADER).unwrap_or(0) != 0;
        // Parameter sets exist only in the header; a container without one would get a stream no
        // decoder can start. Every export container has one, so this only guards the muxer table.
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
            // Another container's tag need not be this one's; in MP4 and MOV HEVC is `hvc1`
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
            final_path,
            format,
            partial,
            stream_index,
            audio_stream_index,
            opened_audio,
            copied_audio,
        })
    }

    /// The time base the container gave stream `index`.
    fn time_base(&self, index: i32) -> AVRational {
        self.format.streams()[usize::try_from(index).unwrap_or(0)].time_base
    }

    /// Starts the audio thread, rescaling into `audio_stream_time_base`, and
    /// keeps the partial file as the muxer's.
    ///
    /// # Errors
    /// Returns an adapter failure when the audio thread cannot start.
    fn start(
        self,
        stream_time_base: AVRational,
        audio_stream_time_base: AVRational,
        pending_header: Option<PendingHeader>,
    ) -> Result<Muxer, EncodeError> {
        // Bound after `partial`, `format` is dropped before it when the thread cannot start.
        let Self {
            final_path,
            partial,
            format,
            stream_index,
            audio_stream_index,
            opened_audio,
            copied_audio,
        } = self;
        let partial_path = partial.path().to_owned();
        // Started after the header: the stream index and container time base it rescales into exist
        // only then. A deferred header is the exception ([`Muxer::open_deferred`]).
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
        let partial_lock = partial.keep();
        Ok(Muxer {
            final_path,
            partial_path,
            _partial_lock: partial_lock,
            format: Some(format),
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
    /// The time base the audio thread rescales into until the header fixes
    /// the stream's, when there is an encoded audio stream.
    audio_time_base: Option<AVRational>,
    /// The container the header is written for.
    container: Container,
}

/// Parameter sets as a sample description takes them: for Annex B (H.264
/// and HEVC), each distinct NAL unit once, in the order it first appears,
/// behind a four-byte start code; anything else (an AV1 sequence header) as
/// it is.
///
/// An encoder may repeat a parameter set within one access unit, and
/// `extract_extradata` hands over every copy, so the same sets can arrive
/// as different bytes from one IDR picture to the next. Comparing them as
/// sets of units tells a repeat from a change.
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

/// Gives a video packet with no duration one frame: `frame_units` of its
/// time base, which is 1 where the time base is the frame period and the
/// frame length rounded to the unit where an encoder could not hold that
/// (MPEG-4 Part 2 at 119.88 fps).
///
/// `FFmpeg` clears a frame's duration unless the encoder was opened with
/// `AV_CODEC_FLAG_FRAME_DURATION`, the encoders copy that zero into their
/// packets, and the MP4 muxer takes the last sample's duration from the last
/// packet: without this, the last frame lasts no time and the track is a
/// frame short.
pub(crate) fn fill_frame_duration(packet: &mut AVPacket, frame_units: i64) {
    if packet.duration == 0 {
        packet.set_duration(frame_units);
    }
}

/// How many units of `time_base` one frame at `rate` lasts, to the nearest
/// unit and at least one.
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

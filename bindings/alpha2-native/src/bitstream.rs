//! The bitstream filter chain a hardware export runs its encoder's packets
//! through on their way to the muxer.
//!
//! A container takes two things from a stream's own bitstream that some hardware
//! encoders do not write. `FFmpeg`'s D3D12 Video encoders (8.1) are the case
//! in point: they put their parameter sets in-band on every IDR picture and
//! leave the extradata empty, and their sequence parameter sets carry no
//! colour description. The chain repairs both, and runs the same way for an
//! encoder that already writes them, so there is one code path:
//! - `h264_metadata`, `hevc_metadata` or `av1_metadata` writes the export's
//!   colour into every sequence header, in-band and in the extradata alike;
//! - `extract_extradata` with `remove=1` then cuts the parameter sets out of
//!   the packets and hands them over as `AV_PKT_DATA_NEW_EXTRADATA`, so they
//!   end up only in the stream header (an MP4 sample description, which `hvc1`
//!   requires and `avc1` expects, or a Matroska `CodecPrivate`).
//!
//! [`crate::mux::Muxer::open_deferred`] is the muxer half: it holds its
//! header back until the first packet out of this chain brings the
//! parameter sets.

// The D3D12VA and Vulkan HEVC backends, which exist on Windows and Linux only, and the tests run
// their packets through this.
#![cfg_attr(
    not(any(
        test,
        all(feature = "hwencode", any(target_os = "windows", target_os = "linux"))
    )),
    allow(dead_code)
)]

use std::ffi::CString;

use alpha2_media::{ColorMetadata, EncodeError};
use rsmpeg::avcodec::{AVBSFContext, AVCodecParameters, AVPacket};
use rsmpeg::avutil::AVRational;
use rsmpeg::error::RsmpegError;
use rsmpeg::ffi;

use crate::convert::encode_adapter_error_at;

/// An encoder's packets on their way to a container: its colour written into
/// every sequence header, and its parameter sets moved out of the packets
/// into `AV_PKT_DATA_NEW_EXTRADATA` side data.
pub(crate) struct EncodedVideoFilter {
    chain: AVBSFContext,
    /// The encoder's own extradata after the colour rewrite, for an encoder
    /// that wrote one: the first packet out carries it as its new extradata
    /// unless it brings its parameter sets in-band. That way the muxer has
    /// one place to look, whichever kind of encoder it is.
    initial_extradata: Option<Vec<u8>>,
}

impl EncodedVideoFilter {
    /// Opens the chain for the stream `parameters` describe (an encoder's
    /// `extract_codecpar`), which is to be tagged `color` and whose packets
    /// are timed in `time_base`.
    ///
    /// # Errors
    /// Returns an adapter failure for a codec other than H.264, HEVC or AV1,
    /// or when the loaded `FFmpeg` lacks one of the filters or refuses the
    /// stream.
    pub(crate) fn new(
        parameters: &AVCodecParameters,
        color: ColorMetadata,
        time_base: AVRational,
    ) -> Result<Self, EncodeError> {
        let spec = chain_spec(parameters.codec_id, color)?;
        let chain = alpha2_ffmpeg_bridge::bitstream_filter_list(&spec, parameters, time_base)
            .map_err(|error| {
                encode_adapter_error_at(
                    &format!("opening the bitstream filters {}", spec.to_string_lossy()),
                    error,
                )
            })?;
        let initial_extradata = Some(alpha2_ffmpeg_bridge::codec_parameters_extradata(
            &chain.par_out(),
        ))
        .filter(|extradata| !extradata.is_empty());
        Ok(Self {
            chain,
            initial_extradata,
        })
    }

    /// Filters one packet the encoder produced and returns every packet the
    /// chain has ready, in order, in the time base the chain was opened
    /// with.
    ///
    /// # Errors
    /// Returns an adapter failure when a filter rejects the packet.
    pub(crate) fn filter(&mut self, mut packet: AVPacket) -> Result<Vec<AVPacket>, EncodeError> {
        // `av_bsf_send_packet` reads an empty packet as end of stream and refuses everything after,
        // so an empty packet is dropped rather than ending the chain early.
        if packet.size <= 0 {
            return Ok(Vec::new());
        }
        self.chain
            .send_packet(Some(&mut packet))
            .map_err(|error| encode_adapter_error_at("bitstream filter send failed", error))?;
        self.drain()
    }

    /// Ends the stream and returns the packets the chain still held.
    ///
    /// # Errors
    /// Returns an adapter failure when a filter fails on its tail.
    pub(crate) fn flush(&mut self) -> Result<Vec<AVPacket>, EncodeError> {
        self.chain
            .send_packet(None)
            .map_err(|error| encode_adapter_error_at("bitstream filter flush failed", error))?;
        self.drain()
    }

    /// Every packet the chain has ready.
    fn drain(&mut self) -> Result<Vec<AVPacket>, EncodeError> {
        let mut packets = Vec::new();
        loop {
            let mut packet = AVPacket::new();
            match self.chain.receive_packet(&mut packet) {
                Ok(()) => {
                    if let Some(extradata) = self.initial_extradata.take() {
                        let in_band = alpha2_ffmpeg_bridge::packet_has_side_data(
                            &packet,
                            ffi::AV_PKT_DATA_NEW_EXTRADATA,
                        );
                        if !in_band {
                            alpha2_ffmpeg_bridge::set_packet_side_data(
                                &mut packet,
                                ffi::AV_PKT_DATA_NEW_EXTRADATA,
                                &extradata,
                            )
                            .map_err(|error| {
                                encode_adapter_error_at("attaching the parameter sets", error)
                            })?;
                        }
                    }
                    packets.push(packet);
                }
                Err(RsmpegError::BitstreamDrainError | RsmpegError::BitstreamFlushedError) => {
                    return Ok(packets);
                }
                Err(error) => {
                    return Err(encode_adapter_error_at(
                        "bitstream filter receive failed",
                        error,
                    ));
                }
            }
        }
    }
}

/// The chain for `codec` tagged `color`, in `FFmpeg`'s list syntax.
///
/// The colour fields come from [`crate::convert::color_tags`], the table the
/// encoder's own options and the container's `colr` box are set from, so
/// the three always agree. `FFmpeg` numbers them as ITU-T H.273 does, which
/// is what the filters write.
fn chain_spec(codec: ffi::AVCodecID, color: ColorMetadata) -> Result<CString, EncodeError> {
    let (primaries, transfer, matrix, range) = crate::convert::color_tags(color);
    let full = u8::from(range == ffi::AVCOL_RANGE_JPEG);
    // H.264 and HEVC name the fields as their VUI does; an AV1 sequence header spells colour and
    // its range flag its own way.
    let metadata = match codec {
        ffi::AV_CODEC_ID_H264 | ffi::AV_CODEC_ID_HEVC => format!(
            "{}=colour_primaries={primaries}:transfer_characteristics={transfer}:\
             matrix_coefficients={matrix}:video_full_range_flag={full}",
            if codec == ffi::AV_CODEC_ID_H264 {
                "h264_metadata"
            } else {
                "hevc_metadata"
            }
        ),
        ffi::AV_CODEC_ID_AV1 => format!(
            "av1_metadata=color_primaries={primaries}:transfer_characteristics={transfer}:\
             matrix_coefficients={matrix}:color_range={full}"
        ),
        other => {
            return Err(EncodeError::Adapter(format!(
                "the export's bitstream filters take H.264, HEVC or AV1, not codec {other}"
            )));
        }
    };
    CString::new(format!("{metadata},extract_extradata=remove=1"))
        .map_err(|_| EncodeError::Adapter("a bitstream filter list holds a NUL".into()))
}

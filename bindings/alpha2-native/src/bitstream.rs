//! The bitstream filter chain between a hardware encoder and the muxer.
//!
//! Some hardware encoders (`FFmpeg` 8.1's D3D12 Video ones) put parameter sets in-band on every
//! IDR, leave the extradata empty and write no colour description. Every encoder goes through:
//! - `h264_metadata`, `hevc_metadata` or `av1_metadata` writes colour into every sequence header;
//! - `extract_extradata` with `remove=1` moves the parameter sets out of the packets into
//!   `AV_PKT_DATA_NEW_EXTRADATA`, so they end up only in the stream header (`hvc1` requires it).
//!
//! [`crate::mux::Muxer::open_deferred`] holds the header back until the first filtered packet.

// Used by the D3D12VA and Vulkan HEVC backends (Windows and Linux only) and the tests.
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

/// Rewrites colour into every sequence header and moves parameter sets into side data.
pub(crate) struct EncodedVideoFilter {
    chain: AVBSFContext,
    /// The encoder's own extradata after the colour rewrite. The first packet carries it as new
    /// extradata unless it brings its parameter sets in-band.
    initial_extradata: Option<Vec<u8>>,
}

impl EncodedVideoFilter {
    /// Opens the chain for `parameters` (an encoder's `extract_codecpar`), tagged `color`, with
    /// packets timed in `time_base`.
    ///
    /// # Errors
    /// Fails for a codec other than H.264, HEVC or AV1, or when a filter is
    /// missing or refuses the stream.
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

    /// Filters one encoder packet and returns every packet ready, in the chain's time base.
    ///
    /// # Errors
    /// Fails when a filter rejects the packet.
    pub(crate) fn filter(&mut self, mut packet: AVPacket) -> Result<Vec<AVPacket>, EncodeError> {
        // `av_bsf_send_packet` treats an empty packet as end of stream, so drop it instead.
        if packet.size <= 0 {
            return Ok(Vec::new());
        }
        self.chain
            .send_packet(Some(&mut packet))
            .map_err(|error| encode_adapter_error_at("bitstream filter send failed", error))?;
        self.drain()
    }

    /// Ends the stream and returns the remaining packets.
    ///
    /// # Errors
    /// Fails when a filter fails on its tail.
    pub(crate) fn flush(&mut self) -> Result<Vec<AVPacket>, EncodeError> {
        self.chain
            .send_packet(None)
            .map_err(|error| encode_adapter_error_at("bitstream filter flush failed", error))?;
        self.drain()
    }

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

/// The filter chain for `codec` tagged `color`, in `FFmpeg`'s list syntax.
///
/// Colour fields come from [`crate::convert::color_tags`], like the encoder options and the
/// container's `colr` box, so all three agree.
fn chain_spec(codec: ffi::AVCodecID, color: ColorMetadata) -> Result<CString, EncodeError> {
    let (primaries, transfer, matrix, range) = crate::convert::color_tags(color);
    let full = u8::from(range == ffi::AVCOL_RANGE_JPEG);
    // AV1 spells the colour fields and range flag differently from the H.264/HEVC VUI names.
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

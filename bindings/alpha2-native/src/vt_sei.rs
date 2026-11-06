//! Strips `VideoToolbox`'s run-dependent SEI from `FFmpeg` packets.

use rsmpeg::avcodec::AVPacket;

/// Drops the SEI NAL units of `packet` whose every message is Apple's `user_data_unregistered`
/// ([`alpha2_host_core::vt_sei::strip_bytes`]).
///
/// # Errors
/// When the packet's buffer cannot be made writable.
pub(crate) fn strip_vt_user_data_sei(
    packet: &mut AVPacket,
) -> Result<(), alpha2_media::EncodeError> {
    let bytes = alpha2_ffmpeg_bridge::packet_bytes(packet);
    match alpha2_host_core::vt_sei::strip_bytes(&bytes) {
        Some(stripped) => alpha2_ffmpeg_bridge::shrink_packet_to(packet, &stripped)
            .map_err(|error| alpha2_media::EncodeError::Adapter(error.to_string())),
        None => Ok(()),
    }
}

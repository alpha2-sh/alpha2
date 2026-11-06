//! `FFmpeg` error classification for the host-core decode recovery.
pub(crate) use alpha2_host_core::recovery::*;

use crate::convert::adapter_error;
use alpha2_media::DecodeError;
use rsmpeg::{error::RsmpegError, ffi};

/// `FFmpeg`'s timestamp, or `None` for `AV_NOPTS_VALUE`.
pub(crate) fn pts(raw: i64) -> Option<i64> {
    (raw != ffi::AV_NOPTS_VALUE).then_some(raw)
}

/// A time base as [`Recovery`] takes it.
pub(crate) fn time_base(base: rsmpeg::avutil::AVRational) -> (i32, i32) {
    (base.num, base.den)
}

/// [`Recovery`] fed `FFmpeg` errors.
pub(crate) trait FfmpegRecovery {
    /// Records `error` at `stage` as damage when it counts as damage, else returns it as fatal.
    fn error(
        &mut self,
        error: RsmpegError,
        stage: &'static str,
        pts: Option<i64>,
    ) -> Result<(), DecodeError>;
}

impl FfmpegRecovery for Recovery {
    fn error(
        &mut self,
        error: RsmpegError,
        stage: &'static str,
        at: Option<i64>,
    ) -> Result<(), DecodeError> {
        let failure = match error.raw_error() {
            Some(ffi::AVERROR_INVALIDDATA) => DecodeFailure::InvalidData,
            // Hardware sessions report an undecodable picture with the hwaccel's error, never
            // INVALIDDATA (`VideoToolbox`: AVERROR_UNKNOWN; Vulkan, D3D12: AVERROR_EXTERNAL).
            Some(ffi::AVERROR_UNKNOWN | ffi::AVERROR_EXTERNAL) => DecodeFailure::HardwarePicture,
            _ => DecodeFailure::Other,
        };
        if !self.counts_as_damage(failure) {
            return Err(adapter_error(error));
        }
        self.corrupt(stage, at.and_then(pts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_errors_are_classified_before_recovery() {
        let base = (1, 30);
        for code in [ffi::AVERROR_UNKNOWN, ffi::AVERROR_EXTERNAL] {
            let mut tolerant = Recovery::hardware(base);
            tolerant.policy = DecodePolicy::Tolerant;
            assert!(
                tolerant
                    .error(RsmpegError::AVError(code), "video send", Some(3))
                    .is_ok()
            );
            assert_eq!(tolerant.diagnostics.corrupt_packets.len(), 1);
            assert!(
                Recovery::hardware(base)
                    .error(RsmpegError::AVError(code), "video send", None)
                    .is_err()
            );
        }
        let mut software = Recovery::new(base);
        software.policy = DecodePolicy::Tolerant;
        assert!(
            software
                .error(
                    RsmpegError::AVError(ffi::AVERROR_INVALIDDATA),
                    "send",
                    Some(ffi::AV_NOPTS_VALUE)
                )
                .is_ok()
        );
        assert_eq!(software.diagnostics.corrupt_packets[0].pts, None);
        assert!(
            software
                .error(RsmpegError::AVError(-12), "send", None)
                .is_err()
        );
    }
}

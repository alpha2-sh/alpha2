//! The backend conformance suite on the `FFmpeg` backend, which reads every fixture.

use alpha2_backend_conformance::{Backend, VideoDecoder, conformance_tests};

use super::*;

impl VideoDecoder for NativeDecoder {
    fn presentation_slack(&self) -> MediaTime {
        self.presentation_deadline(MediaTime::ZERO)
    }
}

struct Ffmpeg;

fn ffmpeg() -> Ffmpeg {
    crate::require_pinned_ffmpeg_for_tests();
    Ffmpeg
}

impl Backend for Ffmpeg {
    fn name(&self) -> &'static str {
        "ffmpeg"
    }

    fn open_video(&self, path: &Path) -> Result<Box<dyn VideoDecoder>, DecodeError> {
        Ok(Box::new(NativeDecoder::open(path)?))
    }

    fn open_audio(
        &self,
        path: &Path,
        sample_rate: u32,
        stream: Option<u32>,
    ) -> Result<Box<dyn Decoder>, DecodeError> {
        Ok(Box::new(NativeAudioDecoder::open_stream(
            path,
            sample_rate,
            stream,
        )?))
    }

    fn refuses(&self, _name: &str) -> Option<&'static str> {
        None
    }
}

conformance_tests!(ffmpeg());

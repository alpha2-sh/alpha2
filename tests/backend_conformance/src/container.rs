//! A [`Backend`] over the in-house container layer and a platform's decoder sessions, as the
//! mobile hosts read files.

use std::path::Path;
use std::sync::Arc;

use alpha2_host_core::DecodePolicy;
use alpha2_host_core::audio_stream::{AudioBackends, AudioStream};
use alpha2_host_core::stills::{StillStream, still_format};
use alpha2_host_core::video_stream::{StreamOptions, VideoBackends, VideoStream};
use alpha2_media::{DecodeError, Decoder};
use alpha2_time::MediaTime;

use crate::{Backend, DefaultAudioRule, VideoDecoder};

impl VideoDecoder for VideoStream {
    fn presentation_slack(&self) -> MediaTime {
        Self::presentation_slack(self)
    }
}

impl VideoDecoder for StillStream {
    fn presentation_slack(&self) -> MediaTime {
        MediaTime::ZERO
    }
}

/// Decoder sessions behind the container layer.
pub struct ContainerBackend {
    name: &'static str,
    video: Arc<dyn VideoBackends>,
    audio: Arc<dyn AudioBackends>,
    refuses: fn(&str) -> Option<&'static str>,
}

impl ContainerBackend {
    /// A backend named `name` whose sessions come from `video` and `audio`, refusing the fixtures
    /// `refuses` names.
    #[must_use]
    pub fn new(
        name: &'static str,
        video: Arc<dyn VideoBackends>,
        audio: Arc<dyn AudioBackends>,
        refuses: fn(&str) -> Option<&'static str>,
    ) -> Self {
        Self {
            name,
            video,
            audio,
            refuses,
        }
    }
}

impl Backend for ContainerBackend {
    fn name(&self) -> &str {
        self.name
    }

    fn open_video(&self, path: &Path) -> Result<Box<dyn VideoDecoder>, DecodeError> {
        let mut head = [0; 12];
        let read = std::fs::File::open(path)
            .and_then(|mut file| std::io::Read::read(&mut file, &mut head))
            .map_err(|error| DecodeError::Adapter(error.to_string()))?;
        if still_format(&head[..read]).is_some() {
            return Ok(Box::new(StillStream::open(path)?));
        }
        Ok(Box::new(VideoStream::open_file(
            path,
            Arc::clone(&self.video),
            StreamOptions::default(),
        )?))
    }

    fn open_audio(
        &self,
        path: &Path,
        sample_rate: u32,
        stream: Option<u32>,
    ) -> Result<Box<dyn Decoder>, DecodeError> {
        Ok(Box::new(AudioStream::open_file(
            path,
            self.audio.as_ref(),
            sample_rate,
            stream,
            DecodePolicy::default(),
        )?))
    }

    fn refuses(&self, name: &str) -> Option<&'static str> {
        (self.refuses)(name)
    }

    fn default_audio(&self) -> DefaultAudioRule {
        DefaultAudioRule::FirstDefaultFlagged
    }
}

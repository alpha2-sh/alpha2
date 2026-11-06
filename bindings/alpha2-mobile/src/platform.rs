//! The media backend a session runs on: the platform's codecs and the container layer.

use std::sync::Arc;

use alpha2_media::CancelToken;
use serde_json::{Value, json};

use crate::error::Alpha2Error;
use crate::session::{
    ConformRequest, EngineConfig, ExportOutcome, ExportProgress, ExportRequest, LoadedFont,
    PauseGate,
};

/// Everything an export thread needs.
pub(crate) struct ExportJob {
    pub request: ExportRequest,
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub config: EngineConfig,
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fonts: Vec<LoadedFont>,
    pub cancel: CancelToken,
    pub paused: Arc<PauseGate>,
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub progress: Arc<dyn ExportProgress>,
}

/// One platform's media backend.
pub(crate) trait Platform: Send + Sync {
    /// The codecs and containers discovered here, as the capabilities' `media` member.
    fn media_capabilities(&self) -> Value;

    /// The conform result for `request`.
    fn conform(&self, request: &ConformRequest) -> Result<Value, Alpha2Error>;

    /// Runs `job` to its end on the calling thread.
    fn export(&self, job: ExportJob) -> Result<ExportOutcome, Alpha2Error>;

    /// The video decoders a preview reads sources with, when the platform has any.
    fn video_decoders(&self) -> Option<Arc<dyn alpha2_host_core::video_stream::VideoBackends>> {
        None
    }

    /// The audio decoders a preview reads sources with, when the platform has any.
    fn audio_decoders(&self) -> Option<Arc<dyn alpha2_host_core::audio_stream::AudioBackends>> {
        None
    }
}

/// The backend of the platform this library runs on.
pub(crate) fn current() -> Arc<dyn Platform> {
    #[cfg(target_os = "android")]
    {
        Arc::new(MediaPlatform {
            name: "android",
            media: crate::export::MediaBackends {
                video_decoders: Arc::new(alpha2_android_media::AndroidVideo::default()),
                audio_decoders: Arc::new(alpha2_host_core::audio_stream::WithPcm(
                    alpha2_android_media::AndroidAudio,
                )),
                video_encoders: Arc::new(alpha2_android_media::AndroidVideoEncoders),
                audio_encoders: Arc::new(alpha2_android_media::AndroidAudioEncoders),
            },
        })
    }
    #[cfg(not(target_os = "android"))]
    Arc::new(NoMedia)
}

/// A platform's codecs behind the container layer.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) struct MediaPlatform {
    pub name: &'static str,
    pub media: crate::export::MediaBackends,
}

impl Platform for MediaPlatform {
    fn media_capabilities(&self) -> Value {
        json!({
            "platform": self.name,
            "containers": ["mp4", "mov"],
            "videoEncoders": ["h264", "hevc"],
            "audioEncoders": ["aac", "opus"],
        })
    }

    fn conform(&self, request: &ConformRequest) -> Result<Value, Alpha2Error> {
        // Whether each source's file opens: its container is read and a decoder takes it.
        let sources = request
            .assets
            .iter()
            .map(|(id, path)| {
                let opened = crate::export::probe_source(std::path::Path::new(path), &self.media);
                json!({
                    "id": id,
                    "path": path,
                    "readable": opened.is_ok(),
                    "reason": opened.err(),
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({ "sources": sources }))
    }

    fn export(&self, job: ExportJob) -> Result<ExportOutcome, Alpha2Error> {
        crate::export::export(&job, &self.media)
    }

    fn video_decoders(&self) -> Option<Arc<dyn alpha2_host_core::video_stream::VideoBackends>> {
        Some(Arc::clone(&self.media.video_decoders))
    }

    fn audio_decoders(&self) -> Option<Arc<dyn alpha2_host_core::audio_stream::AudioBackends>> {
        Some(Arc::clone(&self.media.audio_decoders))
    }
}

/// A build without a media backend: renders nothing that reads or writes media.
#[cfg(not(target_os = "android"))]
struct NoMedia;

#[cfg(not(target_os = "android"))]
impl Platform for NoMedia {
    fn media_capabilities(&self) -> Value {
        json!({ "containers": [], "videoDecoders": [], "videoEncoders": [], "audioEncoders": [] })
    }

    fn conform(&self, _request: &ConformRequest) -> Result<Value, Alpha2Error> {
        Err(unsupported())
    }

    fn export(&self, job: ExportJob) -> Result<ExportOutcome, Alpha2Error> {
        job.paused.wait_while_paused(&job.cancel);
        if job.cancel.is_cancelled() {
            return Err(Alpha2Error::Aborted);
        }
        let _ = job.request;
        Err(unsupported())
    }
}

#[cfg(not(target_os = "android"))]
fn unsupported() -> Alpha2Error {
    Alpha2Error::Unsupported {
        reason: format!(
            "this build has no media backend for {}",
            std::env::consts::OS
        ),
    }
}

//! The media backend a session runs on: the platform's codecs and the container layer.

use std::sync::Arc;

use alpha2_media::CancelToken;
use serde_json::{Value, json};

use crate::error::Alpha2Error;
use crate::session::{
    ConformRequest, EngineConfig, ExportOutcome, ExportProgress, ExportRequest, PauseGate,
};

/// Everything an export thread needs.
pub(crate) struct ExportJob {
    pub request: ExportRequest,
    #[allow(dead_code)]
    pub config: EngineConfig,
    #[allow(dead_code)]
    pub fonts: Vec<Arc<[u8]>>,
    pub cancel: CancelToken,
    pub paused: Arc<PauseGate>,
    #[allow(dead_code)]
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
}

/// The backend of the platform this library runs on.
pub(crate) fn current() -> Arc<dyn Platform> {
    Arc::new(NoMedia)
}

/// A build without a media backend: renders nothing that reads or writes media.
struct NoMedia;

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

fn unsupported() -> Alpha2Error {
    Alpha2Error::Unsupported {
        reason: format!(
            "this build has no media backend for {}",
            std::env::consts::OS
        ),
    }
}

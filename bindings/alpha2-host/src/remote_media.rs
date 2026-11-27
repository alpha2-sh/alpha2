//! A request's remote sources: its URLs checked against the operator's policy before anything is
//! contacted, then probed once each through the request's own session, which the request's
//! cancel stops.

use std::collections::HashMap;
use std::sync::Arc;

use alpha2_host_core::remote::{
    RemoteCounts, RemoteFailure, RemoteObject, RemoteSession, SourceLocation,
};
use alpha2_host_core::requests::{MediaLocation, RemoteContentHash};
use serde_json::{Value, json};

use crate::media_policy::{media_error, refuse_unlisted};
use crate::protocol::HostError;

pub struct RemoteInputs {
    session: RemoteSession,
    hash: RemoteContentHash,
    _cancel: crate::termination::ExportCancel,
}

/// Where a source's original is, once its URL was probed.
pub enum Located {
    Found(SourceLocation),
    /// Nothing there: the shown location, as a conform issue names it.
    Offline(String),
    Unsupported(String),
}

impl RemoteInputs {
    /// `None` when the request names no URL.
    ///
    /// # Errors
    /// `MEDIA_REFUSED` for the first URL the operator's policy refuses, before anything is
    /// contacted.
    pub fn for_request(
        assets: &HashMap<String, MediaLocation>,
        resources: &HashMap<String, HashMap<String, MediaLocation>>,
        cache_bytes: u64,
        hash: RemoteContentHash,
    ) -> Result<Option<Self>, HostError> {
        let policy = crate::media_policy::current();
        refuse_unlisted(&policy, assets, resources)?;
        let named = assets
            .values()
            .chain(resources.values().flat_map(HashMap::values))
            .any(|location| location.url().is_some());
        if !named {
            return Ok(None);
        }
        let cancel = crate::termination::export_cancel();
        let transport = Arc::new(crate::media_fetch::MediaTransport::new(policy));
        Ok(Some(Self {
            session: RemoteSession::new(transport, (*cancel).clone(), cache_bytes),
            hash,
            _cancel: cancel,
        }))
    }

    /// The object at `url`, probed on its first open in the request.
    ///
    /// # Errors
    /// The probe's failure, as the protocol answers it for `source`.
    pub fn open(&self, source: &str, url: &str) -> Result<Arc<RemoteObject>, HostError> {
        self.session
            .open(url)
            .map_err(|error| media_error(source, url, &error))
    }

    /// `source`'s original at `url`: offline for `404` and `410` at the probe, unsupported for
    /// an empty object.
    ///
    /// # Errors
    /// Any other probe failure, which fails the request.
    pub fn locate(&self, source: &str, url: &str) -> Result<Located, HostError> {
        match self.session.open(url) {
            Ok(object) => Ok(Located::Found(SourceLocation::Remote(object))),
            Err(error) => match error.failure {
                RemoteFailure::Offline => {
                    Ok(Located::Offline(alpha2_host_core::remote::shown_url(url)))
                }
                RemoteFailure::Empty => Ok(Located::Unsupported(error.message)),
                _ => Err(media_error(source, url, &error)),
            },
        }
    }

    /// Whether a remote original whose project records a hash is read whole and hashed.
    pub fn verifies(&self) -> bool {
        self.hash == RemoteContentHash::Verify
    }

    pub fn counts(&self) -> RemoteCounts {
        self.session.counts()
    }

    /// The `performance` counters of the request's remote reads.
    pub fn performance(&self) -> [(&'static str, Value); 4] {
        let counts = self.counts();
        [
            ("remoteBytes", json!(counts.bytes)),
            ("remoteRequests", json!(counts.requests)),
            ("remoteRetries", json!(counts.retries)),
            ("remoteReadWaitMs", json!(counts.read_wait_ms)),
        ]
    }
}

/// `error` naming in `details.source` the source whose URL a remote failure names, when it named
/// none.
pub fn name_source(mut error: HostError, assets: &HashMap<String, MediaLocation>) -> HostError {
    let Some(details) = error.details.as_mut() else {
        return error;
    };
    if !error.code.starts_with("MEDIA_") || details.get("source").is_some() {
        return error;
    }
    let Some(url) = details.get("url").and_then(Value::as_str) else {
        return error;
    };
    let mut named = assets
        .iter()
        .filter(|(_, location)| {
            location
                .url()
                .is_some_and(|own| alpha2_host_core::remote::shown_url(own) == url)
        })
        .map(|(id, _)| id.as_str())
        .collect::<Vec<_>>();
    named.sort_unstable();
    if let Some(first) = named.first() {
        details["source"] = json!(first);
    }
    error
}

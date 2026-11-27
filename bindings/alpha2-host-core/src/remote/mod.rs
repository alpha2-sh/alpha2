//! Remote media read by byte range, behind a [`RangeTransport`] the host supplies.
//!
//! One [`RemoteSession`] per request probes each URL once ([`RemoteObject`]), holds the blocks
//! fetched for it, and hands out readers ([`RemoteReader`], [`RemoteStream`]) that wait for bytes
//! while marking the request's watches, so a network wait is never taken for a stalled decoder.

mod cache;
mod fetch;
mod location;
mod protocol;
mod reader;

use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use alpha2_media::CancelToken;

pub use location::{SourceLocation, decode_error};
pub use reader::{RemoteReader, RemoteStream};

use cache::BlockCache;
use protocol::ObjectFacts;

/// Blocks are this long and aligned to the object's offsets.
pub const BLOCK_BYTES: u64 = 256 * 1024;
/// The smallest block cache a request may ask for.
pub const MIN_CACHE_BYTES: u64 = 4 * 1024 * 1024;
/// The block cache of a request that names none, before it is fitted to the machine.
pub const DEFAULT_CACHE_BYTES: u64 = 64 * 1024 * 1024;
/// Attempts at one range before it fails.
pub const MAX_ATTEMPTS: usize = 5;
/// The waits between attempts.
pub const BACKOFF: [Duration; MAX_ATTEMPTS - 1] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];
/// A read waiting this long with no byte of what it waits for fails as unreachable.
pub const NO_PROGRESS_LIMIT: Duration = Duration::from_secs(30);
/// The longest `Retry-After` a busy failure reports.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
/// How long a transport may take to connect, TLS included.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a transport may wait for a response head.
pub const HEAD_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a response body may go without a byte.
pub const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
const WAIT_POLL: Duration = Duration::from_millis(5);

/// One `GET` of a byte range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeRequest {
    /// The URL, query included.
    pub url: String,
    /// The first byte asked for.
    pub first: u64,
    /// The last byte asked for, inclusive.
    pub last: u64,
    /// The validator every range after the probe sends.
    pub precondition: Option<Precondition>,
}

impl RangeRequest {
    /// The `Range` header value.
    #[must_use]
    pub fn range_header(&self) -> String {
        format!("bytes={}-{}", self.first, self.last)
    }
}

/// A conditional request header (RFC 9110 section 13.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Precondition {
    /// `If-Match` with the probe's strong entity tag.
    IfMatch(String),
    /// `If-Unmodified-Since` with the probe's `Last-Modified`.
    IfUnmodifiedSince(String),
}

impl Precondition {
    /// The header's name and value.
    #[must_use]
    pub fn header(&self) -> (&'static str, &str) {
        match self {
            Self::IfMatch(tag) => ("If-Match", tag),
            Self::IfUnmodifiedSince(date) => ("If-Unmodified-Since", date),
        }
    }
}

/// The response headers a range read looks at, each its first value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResponseHeaders {
    /// `Content-Range`.
    pub content_range: Option<String>,
    /// `Content-Encoding`.
    pub content_encoding: Option<String>,
    /// `Content-Type`.
    pub content_type: Option<String>,
    /// `ETag`.
    pub etag: Option<String>,
    /// `Last-Modified`.
    pub last_modified: Option<String>,
    /// `Retry-After`.
    pub retry_after: Option<String>,
}

/// A response that is not a redirect, with its body unread.
pub struct RangeResponse {
    /// The status code.
    pub status: u16,
    /// The headers read.
    pub headers: ResponseHeaders,
    /// The body; dropping it drops the connection.
    pub body: Box<dyn Read + Send>,
}

impl RangeResponse {
    fn retry_delay(&self) -> Option<Duration> {
        self.headers
            .retry_after
            .as_deref()
            .and_then(|value| protocol::retry_after(value, std::time::SystemTime::now()))
    }
}

/// Why the transport has no response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportFailure {
    /// The operator's policy refused the target before it was contacted.
    Refused {
        /// What refused it.
        policy: RefusalPolicy,
        /// Why, with the URL shown as [`shown_url`] shows it.
        message: String,
    },
    /// The certificate verifier or the TLS handshake refused the server; never tried again.
    Tls(String),
    /// A resolve, connect, TLS I/O or timeout failure; tried again.
    Unreachable(String),
    /// The server answered something no range read can use, such as too many redirects; never
    /// tried again.
    Rejected(String),
}

/// The operator rule that refused a URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalPolicy {
    /// The origin is not listed, or no origin is.
    Origins,
    /// A redirect left the listed origins.
    Redirect,
    /// The address is not global unicast.
    Address,
}

impl RefusalPolicy {
    /// The name `details.policy` carries.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Origins => "origins",
            Self::Redirect => "redirect",
            Self::Address => "address",
        }
    }
}

/// Sends range requests; the host's uses HTTPS, tests script the answers.
pub trait RangeTransport: Send + Sync {
    /// One `GET` of `request`, its redirects followed.
    ///
    /// # Errors
    /// Why there is no response.
    fn get(&self, request: &RangeRequest) -> Result<RangeResponse, TransportFailure>;
}

/// How a remote read failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteFailure {
    /// `MEDIA_REFUSED`.
    Refused(RefusalPolicy),
    /// `MEDIA_TLS_FAILED`.
    TlsFailed,
    /// `MEDIA_UNREACHABLE`.
    Unreachable,
    /// `MEDIA_BUSY`, with the server's delay in milliseconds, at most 60000.
    Busy {
        /// From `Retry-After`.
        retry_after_ms: u64,
    },
    /// `MEDIA_RANGE_UNSUPPORTED`.
    RangeUnsupported,
    /// `MEDIA_REJECTED`.
    Rejected,
    /// `MEDIA_CHANGED`.
    Changed,
    /// The probe answered `404` or `410`: the source is offline, as a missing file is.
    Offline,
    /// The probe answered `416` for an empty object, which holds no media.
    Empty,
    /// The request's cancel was seen.
    Cancelled,
}

impl RemoteFailure {
    /// The protocol code a request fails with.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Refused(_) => "MEDIA_REFUSED",
            Self::TlsFailed => "MEDIA_TLS_FAILED",
            Self::Unreachable => "MEDIA_UNREACHABLE",
            Self::Busy { .. } => "MEDIA_BUSY",
            Self::RangeUnsupported | Self::Empty => "MEDIA_RANGE_UNSUPPORTED",
            Self::Rejected | Self::Offline => "MEDIA_REJECTED",
            Self::Changed => "MEDIA_CHANGED",
            Self::Cancelled => "CANCELLED",
        }
    }
}

/// A remote read's failure, its message showing the URL as [`shown_url`] does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteError {
    /// What failed.
    pub failure: RemoteFailure,
    /// What happened.
    pub message: String,
    /// The status the server answered, when it answered.
    pub status: Option<u16>,
}

impl RemoteError {
    /// A failure with no status.
    pub fn new(failure: RemoteFailure, message: impl Into<String>) -> Self {
        Self {
            failure,
            message: message.into(),
            status: None,
        }
    }

    /// The failure with the status the server answered.
    #[must_use]
    pub const fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    fn cancelled() -> Self {
        Self::new(RemoteFailure::Cancelled, "the request was cancelled")
    }

    /// The remote failure an I/O error from a reader carries, if it carries one.
    #[must_use]
    pub fn of(error: &std::io::Error) -> Option<&Self> {
        error.get_ref()?.downcast_ref()
    }
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RemoteError {}

impl From<TransportFailure> for RemoteError {
    fn from(failure: TransportFailure) -> Self {
        match failure {
            TransportFailure::Refused { policy, message } => {
                Self::new(RemoteFailure::Refused(policy), message)
            }
            TransportFailure::Tls(message) => Self::new(RemoteFailure::TlsFailed, message),
            TransportFailure::Unreachable(message) => {
                Self::new(RemoteFailure::Unreachable, message)
            }
            TransportFailure::Rejected(message) => Self::new(RemoteFailure::Rejected, message),
        }
    }
}

impl From<RemoteError> for std::io::Error {
    fn from(error: RemoteError) -> Self {
        Self::other(error)
    }
}

/// `url` as a message shows it: scheme, host, port and path, without user info, query or
/// fragment, which may carry credentials (a signed URL's signature). A path is shown whole.
#[must_use]
pub fn shown_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    if scheme.is_empty()
        || !scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"+-.".contains(&byte))
    {
        return url.to_owned();
    }
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let (authority, path) = rest.find('/').map_or((rest, ""), |at| rest.split_at(at));
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    format!("{scheme}://{host}{path}")
}

pub(crate) trait Clock: Send + Sync {
    fn now(&self) -> Duration;
    fn sleep(&self, duration: Duration, stop: &dyn Fn() -> bool);
}

struct AwakeClock(alpha2_os::AwakeInstant);

impl Clock for AwakeClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }

    fn sleep(&self, duration: Duration, stop: &dyn Fn() -> bool) {
        let until = self.now() + duration;
        while !stop() {
            let left = until.saturating_sub(self.now());
            if left.is_zero() {
                break;
            }
            std::thread::sleep(left.min(WAIT_POLL));
        }
    }
}

/// What a request's remote reads cost, for its `performance` report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RemoteCounts {
    /// Body bytes received.
    pub bytes: u64,
    /// `GET`s sent, retries included.
    pub requests: u64,
    /// `GET`s sent again after a failed attempt.
    pub retries: u64,
    /// Milliseconds readers waited for bytes.
    pub read_wait_ms: u64,
}

#[derive(Default)]
struct Counters {
    bytes: AtomicU64,
    requests: AtomicU64,
    retries: AtomicU64,
    read_wait_ns: AtomicU64,
}

struct Shared {
    transport: Arc<dyn RangeTransport>,
    cancel: CancelToken,
    clock: Arc<dyn Clock>,
    cache: Mutex<BlockCache>,
    counters: Counters,
    readers: AtomicUsize,
    next_object: AtomicU64,
}

impl Shared {
    fn cache(&self) -> std::sync::MutexGuard<'_, BlockCache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn window_cap(&self) -> u64 {
        let readers = self.readers.load(Ordering::Acquire).max(1) as u64;
        let share = self.cache().budget() / 2 / readers;
        (share / BLOCK_BYTES).max(1) * BLOCK_BYTES
    }

    fn waiting(&self) -> Result<(), RemoteError> {
        crate::request_watch::beat_request();
        crate::recovery::report_decode_progress();
        if self.cancel.is_cancelled() {
            return Err(RemoteError::cancelled());
        }
        Ok(())
    }

    fn waited(&self, since: Duration) {
        let waited = self.clock.now().saturating_sub(since);
        crate::recovery::report_input_wait(waited);
        self.counters.read_wait_ns.fetch_add(
            u64::try_from(waited.as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
}

/// The remote reads of one request: each URL probed once, one block cache, one cancel.
///
/// Sessions share nothing, so a request reads only bytes its own URLs were answered with.
pub struct RemoteSession {
    shared: Arc<Shared>,
    objects: Mutex<HashMap<String, Arc<RemoteObject>>>,
}

impl RemoteSession {
    /// A session reading through `transport`, holding at most `cache_bytes` of blocks (at least
    /// one per reader), stopped by `cancel`.
    #[must_use]
    pub fn new(transport: Arc<dyn RangeTransport>, cancel: CancelToken, cache_bytes: u64) -> Self {
        Self::with_clock(
            transport,
            cancel,
            cache_bytes,
            Arc::new(AwakeClock(alpha2_os::AwakeInstant::now())),
        )
    }

    pub(crate) fn with_clock(
        transport: Arc<dyn RangeTransport>,
        cancel: CancelToken,
        cache_bytes: u64,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                transport,
                cancel,
                clock,
                cache: Mutex::new(BlockCache::new(cache_bytes)),
                counters: Counters::default(),
                readers: AtomicUsize::new(0),
                next_object: AtomicU64::new(0),
            }),
            objects: Mutex::new(HashMap::new()),
        }
    }

    /// The object at `url`, probed on its first open in this session: a `GET` of the first block
    /// that must answer `206` with a numeric total, an identity body and a validator.
    ///
    /// # Errors
    /// The probe's failure: [`RemoteFailure::Offline`] for `404` and `410`,
    /// [`RemoteFailure::Empty`] for an empty object, or the code the failure maps to.
    pub fn open(&self, url: &str) -> Result<Arc<RemoteObject>, RemoteError> {
        if let Some(object) = self
            .objects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(url)
        {
            return Ok(Arc::clone(object));
        }
        let id = self.shared.next_object.fetch_add(1, Ordering::Relaxed);
        let (facts, first_block) = fetch::probe(&self.shared, url)?;
        self.shared.cache().insert((id, 0), Arc::from(first_block));
        let object = Arc::new(RemoteObject {
            id,
            url: url.to_owned(),
            shown: shown_url(url),
            facts,
            shared: Arc::clone(&self.shared),
        });
        Ok(Arc::clone(
            self.objects
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(url.to_owned())
                .or_insert(object),
        ))
    }

    /// What the session's reads cost so far.
    #[must_use]
    pub fn counts(&self) -> RemoteCounts {
        let counters = &self.shared.counters;
        RemoteCounts {
            bytes: counters.bytes.load(Ordering::Relaxed),
            requests: counters.requests.load(Ordering::Relaxed),
            retries: counters.retries.load(Ordering::Relaxed),
            read_wait_ms: counters.read_wait_ns.load(Ordering::Relaxed) / 1_000_000,
        }
    }

    /// The bytes the block cache holds now.
    #[must_use]
    pub fn cached_bytes(&self) -> u64 {
        self.shared.cache().held()
    }
}

/// One URL as the probe found it: its length and validator, its blocks in the session's cache.
pub struct RemoteObject {
    id: u64,
    url: String,
    shown: String,
    facts: ObjectFacts,
    shared: Arc<Shared>,
}

impl RemoteObject {
    /// The object's length in bytes.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.facts.total
    }

    /// Whether the object holds no byte; a probed object never is.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.facts.total == 0
    }

    /// The URL as messages show it.
    #[must_use]
    pub fn shown_url(&self) -> &str {
        &self.shown
    }

    /// The probe's strong entity tag, if it named one.
    #[must_use]
    pub fn etag(&self) -> Option<&str> {
        self.facts.etag.as_deref()
    }

    /// The probe's `Last-Modified`, if it named one.
    #[must_use]
    pub fn last_modified(&self) -> Option<&str> {
        self.facts.last_modified.as_deref()
    }

    /// The validator the object is identified by in the request: its strong entity tag, else its
    /// `Last-Modified`.
    #[must_use]
    pub fn validator(&self) -> &str {
        self.etag()
            .or_else(|| self.last_modified())
            .unwrap_or_default()
    }

    /// A reader at offset 0 that reads through the session's block cache.
    #[must_use]
    pub fn reader(self: &Arc<Self>) -> RemoteReader {
        RemoteReader::new(Arc::clone(self))
    }

    /// A reader of the whole object from its first byte that keeps only what it has not yet
    /// returned, past the block cache.
    #[must_use]
    pub fn stream(self: &Arc<Self>) -> RemoteStream {
        RemoteStream::new(Arc::clone(self))
    }
}

#[cfg(test)]
#[path = "../remote_tests.rs"]
mod tests;

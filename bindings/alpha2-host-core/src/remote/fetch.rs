//! One range fetched with its retries, and the probe.

use std::io::{ErrorKind, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use super::protocol::{self, ObjectFacts, StatusClass};
use super::{
    BACKOFF, BLOCK_BYTES, MAX_ATTEMPTS, MAX_RETRY_AFTER, NO_PROGRESS_LIMIT, RangeRequest,
    RemoteError, RemoteFailure, Shared, TransportFailure, WAIT_POLL,
};

const CHUNK_BYTES: usize = 64 * 1024;

pub(super) enum Known {
    Nothing,
    Facts(ObjectFacts),
}

pub(super) struct Progress {
    last_byte: AtomicU64,
}

impl Progress {
    pub const fn new() -> Self {
        Self {
            last_byte: AtomicU64::new(0),
        }
    }

    pub fn mark(&self, shared: &Shared) {
        self.last_byte.store(
            u64::try_from(shared.clock.now().as_nanos()).unwrap_or(u64::MAX),
            Ordering::Release,
        );
    }

    pub fn last_byte(&self) -> Duration {
        Duration::from_nanos(self.last_byte.load(Ordering::Acquire))
    }

    fn wait_left(&self, shared: &Shared, started: Duration) -> Duration {
        let quiet = shared
            .clock
            .now()
            .saturating_sub(started.max(self.last_byte()));
        NO_PROGRESS_LIMIT.saturating_sub(quiet)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Ended {
    Whole,
    Stopped,
}

fn transport_failure(shown: &str, failure: TransportFailure) -> Result<String, RemoteError> {
    match failure {
        TransportFailure::Refused { policy, message } => Err(RemoteError::new(
            super::RemoteFailure::Refused(policy),
            message,
        )),
        TransportFailure::Tls(why) => Err(RemoteError::new(
            RemoteFailure::TlsFailed,
            format!("{shown}: the server's certificate or handshake was refused: {why}"),
        )),
        TransportFailure::Rejected(why) => Err(RemoteError::new(
            RemoteFailure::Rejected,
            format!("{shown}: {why}"),
        )),
        TransportFailure::Unreachable(why) => Ok(why),
    }
}

#[expect(clippy::too_many_arguments, reason = "one fetch's whole context")]
pub(super) fn fetch_range(
    shared: &Shared,
    url: &str,
    shown: &str,
    known: &mut Known,
    first: u64,
    end: u64,
    progress: &Progress,
    stop: &dyn Fn() -> bool,
    sink: &mut dyn FnMut(&[u8]) -> bool,
) -> Result<Ended, RemoteError> {
    let started = shared.clock.now();
    let mut next = first;
    let mut attempts = 0;
    let mut chunk = vec![0; CHUNK_BYTES];
    loop {
        if stop() {
            return Ok(Ended::Stopped);
        }
        attempts += 1;
        shared.counters.requests.fetch_add(1, Ordering::Relaxed);
        if attempts > 1 {
            shared.counters.retries.fetch_add(1, Ordering::Relaxed);
        }
        let precondition = match known {
            Known::Nothing => None,
            Known::Facts(facts) => facts.precondition(),
        };
        let last = end - 1;
        let request = RangeRequest {
            url: url.to_owned(),
            first: next,
            last,
            precondition,
        };
        let (why, delay) = match shared.transport.get(&request) {
            Err(failure) => (transport_failure(shown, failure)?, None),
            Ok(mut response) => {
                let class = match known {
                    Known::Nothing => protocol::probe_status(shown, &response),
                    Known::Facts(_) => protocol::range_status(shown, &response),
                };
                match class {
                    StatusClass::Failed(error) => return Err(error),
                    StatusClass::Retry(delay) => (
                        format!("the server answered {}", response.status),
                        delay.map(|delay| (delay, response.status)),
                    ),
                    StatusClass::Partial => {
                        match known {
                            Known::Nothing => {
                                *known = Known::Facts(protocol::probe_facts(shown, &response)?);
                            }
                            Known::Facts(facts) => {
                                protocol::check_range(shown, facts, &response, next, last)?;
                            }
                        }
                        let Known::Facts(facts) = known else {
                            unreachable!("the probe's facts were just set");
                        };
                        let until = end.min(facts.total);
                        match read_body(
                            shared,
                            &mut *response.body,
                            &mut chunk,
                            &mut next,
                            until,
                            progress,
                            stop,
                            sink,
                        ) {
                            Ok(ended) => return Ok(ended),
                            Err(why) => (why, None),
                        }
                    }
                }
            }
        };
        if attempts >= MAX_ATTEMPTS {
            return Err(RemoteError::new(
                RemoteFailure::Unreachable,
                format!("{shown}: unreachable after {MAX_ATTEMPTS} attempts: {why}"),
            ));
        }
        let wait = match delay {
            Some((delay, status)) => {
                if delay > progress.wait_left(shared, started) {
                    let capped = delay.min(MAX_RETRY_AFTER);
                    return Err(RemoteError::new(
                        RemoteFailure::Busy {
                            retry_after_ms: u64::try_from(capped.as_millis()).unwrap_or(u64::MAX),
                        },
                        format!(
                            "{shown}: the server is busy; it asks to wait {} ms",
                            capped.as_millis()
                        ),
                    )
                    .with_status(status));
                }
                delay
            }
            None => BACKOFF[attempts - 1],
        };
        shared.clock.sleep(wait, stop);
    }
}

#[expect(clippy::too_many_arguments, reason = "one fetch's whole context")]
fn read_body(
    shared: &Shared,
    body: &mut dyn Read,
    chunk: &mut [u8],
    next: &mut u64,
    until: u64,
    progress: &Progress,
    stop: &dyn Fn() -> bool,
    sink: &mut dyn FnMut(&[u8]) -> bool,
) -> Result<Ended, String> {
    while *next < until {
        if stop() {
            return Ok(Ended::Stopped);
        }
        let wanted = usize::try_from(until - *next)
            .unwrap_or(usize::MAX)
            .min(chunk.len());
        let read = match body.read(&mut chunk[..wanted]) {
            Ok(0) => return Err(format!("the body ended at byte {next}")),
            Ok(read) => read,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("the body failed: {error}")),
        };
        *next += read as u64;
        shared
            .counters
            .bytes
            .fetch_add(read as u64, Ordering::Relaxed);
        progress.mark(shared);
        if !sink(&chunk[..read]) {
            return Ok(Ended::Stopped);
        }
    }
    Ok(Ended::Whole)
}

pub(super) fn wait_for<T>(
    shared: &Shared,
    shown: &str,
    progress: &Progress,
    mut ready: impl FnMut(Duration) -> Option<Result<T, RemoteError>>,
) -> Result<T, RemoteError> {
    let since = shared.clock.now();
    let result = loop {
        if let Err(error) = shared.waiting() {
            break Err(error);
        }
        if let Some(result) = ready(WAIT_POLL) {
            break result;
        }
        let quiet = shared
            .clock
            .now()
            .saturating_sub(since.max(progress.last_byte()));
        if quiet >= NO_PROGRESS_LIMIT {
            break Err(RemoteError::new(
                RemoteFailure::Unreachable,
                format!(
                    "{shown}: no byte arrived for {} s",
                    NO_PROGRESS_LIMIT.as_secs()
                ),
            ));
        }
    };
    shared.waited(since);
    result
}

pub(super) fn probe(
    shared: &Arc<Shared>,
    url: &str,
) -> Result<(ObjectFacts, Vec<u8>), RemoteError> {
    let shown = super::shown_url(url);
    let progress = Arc::new(Progress::new());
    let stopped = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::channel();
    {
        let (shared, url, thread_shown, progress, stopped) = (
            Arc::clone(shared),
            url.to_owned(),
            shown.clone(),
            Arc::clone(&progress),
            Arc::clone(&stopped),
        );
        std::thread::Builder::new()
            .name("alpha2-remote-probe".into())
            .spawn(move || {
                let mut known = Known::Nothing;
                let mut bytes = Vec::new();
                let stop = || stopped.load(Ordering::Acquire) || shared.cancel.is_cancelled();
                let result = fetch_range(
                    &shared,
                    &url,
                    &thread_shown,
                    &mut known,
                    0,
                    BLOCK_BYTES,
                    &progress,
                    &stop,
                    &mut |chunk| {
                        bytes.extend_from_slice(chunk);
                        true
                    },
                )
                .and_then(|ended| match (ended, known) {
                    (Ended::Whole, Known::Facts(facts)) => Ok((facts, bytes)),
                    _ => Err(RemoteError::cancelled()),
                });
                let _ = sender.send(result);
            })
            .map_err(|error| {
                RemoteError::new(
                    RemoteFailure::Unreachable,
                    format!("{shown}: no thread for the probe: {error}"),
                )
            })?;
    }
    let result = wait_for(shared, &shown, &progress, |poll| {
        match receiver.recv_timeout(poll) {
            Ok(result) => Some(result),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => Some(Err(RemoteError::new(
                RemoteFailure::Unreachable,
                format!("{shown}: the probe ended without an answer"),
            ))),
        }
    });
    stopped.store(true, Ordering::Release);
    result
}

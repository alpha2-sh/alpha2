use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;

const URL: &str = "https://media.example/take.mp4?signature=secret";
const MIB: u64 = 1024 * 1024;

fn content(length: u64) -> Vec<u8> {
    (0..length)
        .map(|at| u8::try_from((at * 7 + at / 251) % 256).unwrap_or_default())
        .collect()
}

fn at(offset: u64) -> usize {
    usize::try_from(offset).unwrap()
}

struct ServedObject {
    bytes: Vec<u8>,
    etag: Option<String>,
    last_modified: Option<String>,
}

type Answer = Result<RangeResponse, TransportFailure>;
type Hook = dyn Fn(&Script, &RangeRequest, usize) -> Option<Answer> + Send + Sync;

struct Script {
    object: Mutex<ServedObject>,
    requests: Mutex<Vec<RangeRequest>>,
    hook: Box<Hook>,
}

impl Script {
    fn new(length: u64) -> Arc<Self> {
        Self::with_hook(length, |_, _, _| None)
    }

    fn with_hook(
        length: u64,
        hook: impl Fn(&Script, &RangeRequest, usize) -> Option<Answer> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            object: Mutex::new(ServedObject {
                bytes: content(length),
                etag: Some("\"v1\"".into()),
                last_modified: Some("Sun, 06 Nov 1994 08:49:37 GMT".into()),
            }),
            requests: Mutex::new(Vec::new()),
            hook: Box::new(hook),
        })
    }

    fn requests(&self) -> Vec<RangeRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn set_etag(&self, etag: Option<&str>) {
        self.object.lock().unwrap().etag = etag.map(str::to_owned);
    }

    fn served(&self, request: &RangeRequest, body: Box<dyn Read + Send>) -> RangeResponse {
        let object = self.object.lock().unwrap();
        let total = object.bytes.len() as u64;
        let last = request.last.min(total - 1);
        RangeResponse {
            status: 206,
            headers: ResponseHeaders {
                content_range: Some(format!("bytes {}-{last}/{total}", request.first)),
                etag: object.etag.clone(),
                last_modified: object.last_modified.clone(),
                ..ResponseHeaders::default()
            },
            body,
        }
    }

    fn range_bytes(&self, request: &RangeRequest) -> Vec<u8> {
        let object = self.object.lock().unwrap();
        let last = request.last.min(object.bytes.len() as u64 - 1);
        object.bytes[at(request.first)..=at(last)].to_vec()
    }
}

impl RangeTransport for Script {
    fn get(&self, request: &RangeRequest) -> Answer {
        let index = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request.clone());
            requests.len() - 1
        };
        if let Some(answer) = (self.hook)(self, request, index) {
            return answer;
        }
        let bytes = self.range_bytes(request);
        Ok(self.served(request, Box::new(std::io::Cursor::new(bytes))))
    }
}

fn status(status: u16, headers: ResponseHeaders) -> RangeResponse {
    RangeResponse {
        status,
        headers,
        body: Box::new(std::io::empty()),
    }
}

struct Hanging {
    released: Arc<AtomicBool>,
}

impl Read for Hanging {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        let until = Instant::now() + Duration::from_secs(60);
        while !self.released.load(Ordering::Acquire) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(1));
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timed out",
        ))
    }
}

struct Slow {
    bytes: std::io::Cursor<Vec<u8>>,
    delay: Duration,
    step: usize,
    dropped: Arc<AtomicBool>,
}

impl Read for Slow {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        std::thread::sleep(self.delay);
        let length = buffer.len().min(self.step);
        self.bytes.read(&mut buffer[..length])
    }
}

impl Drop for Slow {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}

#[derive(Default)]
struct ManualClock {
    now: Mutex<Duration>,
    sleeps: Mutex<Vec<Duration>>,
}

impl ManualClock {
    fn advance(&self, by: Duration) {
        *self.now.lock().unwrap() += by;
    }

    fn sleeps(&self) -> Vec<Duration> {
        self.sleeps.lock().unwrap().clone()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        *self.now.lock().unwrap()
    }

    fn sleep(&self, duration: Duration, _: &dyn Fn() -> bool) {
        self.sleeps.lock().unwrap().push(duration);
        self.advance(duration);
    }
}

fn session(script: &Arc<Script>) -> RemoteSession {
    RemoteSession::new(
        Arc::clone(script) as Arc<dyn RangeTransport>,
        CancelToken::default(),
        DEFAULT_CACHE_BYTES,
    )
}

fn cancellable_session(script: &Arc<Script>, cancel: &CancelToken) -> RemoteSession {
    RemoteSession::new(
        Arc::clone(script) as Arc<dyn RangeTransport>,
        cancel.clone(),
        DEFAULT_CACHE_BYTES,
    )
}

fn manual_session(script: &Arc<Script>, clock: &Arc<ManualClock>) -> RemoteSession {
    RemoteSession::with_clock(
        Arc::clone(script) as Arc<dyn RangeTransport>,
        CancelToken::default(),
        DEFAULT_CACHE_BYTES,
        Arc::clone(clock) as Arc<dyn Clock>,
    )
}

fn read_all(reader: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut chunk = vec![0; 64 * 1024];
    loop {
        match reader.read(&mut chunk)? {
            0 => return Ok(bytes),
            read => bytes.extend_from_slice(&chunk[..read]),
        }
    }
}

fn failure(error: &std::io::Error) -> RemoteFailure {
    RemoteError::of(error)
        .unwrap_or_else(|| panic!("not a remote failure: {error}"))
        .failure
}

fn await_requests(script: &Script, count: usize) {
    let until = Instant::now() + Duration::from_secs(10);
    while script.requests.lock().unwrap().len() < count {
        assert!(
            Instant::now() < until,
            "the transport never saw request {count}"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn asked(script: &Script) -> Vec<u64> {
    script.requests()[1..]
        .iter()
        .map(|request| request.last + 1 - request.first)
        .collect()
}

fn hanging_after_probe(released: &Arc<AtomicBool>) -> Arc<Script> {
    let released = Arc::clone(released);
    Script::with_hook(4 * MIB, move |script, request, index| {
        (index > 0).then(|| {
            let body = Hanging {
                released: Arc::clone(&released),
            };
            Ok(script.served(request, Box::new(body)))
        })
    })
}

#[test]
fn a_probe_answered_200_is_refused_without_reading_its_body() {
    struct Untouched(Arc<AtomicBool>);
    impl Read for Untouched {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            self.0.store(true, Ordering::Release);
            Ok(0)
        }
    }
    let read = Arc::new(AtomicBool::new(false));
    let body_read = Arc::clone(&read);
    let script = Script::with_hook(MIB, move |_, _, _| {
        Some(Ok(RangeResponse {
            status: 200,
            headers: ResponseHeaders::default(),
            body: Box::new(Untouched(Arc::clone(&body_read))),
        }))
    });
    let error = session(&script).open(URL).err().expect("a 200 is refused");
    assert_eq!(error.failure, RemoteFailure::RangeUnsupported);
    assert_eq!(error.status, Some(200));
    assert!(!read.load(Ordering::Acquire));
    assert_eq!(script.requests().len(), 1);
}

#[test]
fn a_probe_without_a_numeric_total_is_refused() {
    let script = Script::with_hook(MIB, |_, request, _| {
        Some(Ok(status(
            206,
            ResponseHeaders {
                content_range: Some(format!("bytes 0-{}/*", request.last)),
                etag: Some("\"v1\"".into()),
                ..ResponseHeaders::default()
            },
        )))
    });
    let error = session(&script)
        .open(URL)
        .err()
        .expect("no total is refused");
    assert_eq!(error.failure, RemoteFailure::RangeUnsupported);
    assert!(error.message.contains("no length"), "{}", error.message);
}

#[test]
fn a_source_without_a_strong_etag_or_last_modified_is_refused() {
    let script = Script::new(MIB);
    {
        let mut object = script.object.lock().unwrap();
        object.etag = Some("W/\"weak\"".into());
        object.last_modified = None;
    }
    let error = session(&script)
        .open(URL)
        .err()
        .expect("a weak tag alone is refused");
    assert_eq!(error.failure, RemoteFailure::RangeUnsupported);
    assert!(error.message.contains("no validator"), "{}", error.message);
    script.object.lock().unwrap().last_modified = Some("Sun, 06 Nov 1994 08:49:37 GMT".into());
    let object = session(&script)
        .open(URL)
        .expect("Last-Modified is a validator");
    assert_eq!(object.etag(), None);
    assert_eq!(object.validator(), "Sun, 06 Nov 1994 08:49:37 GMT");
}

#[test]
fn every_range_after_the_probe_sends_if_match() {
    let script = Script::new(3 * MIB);
    let object = session(&script).open(URL).unwrap();
    assert_eq!(read_all(&mut object.reader()).unwrap(), content(3 * MIB));
    let requests = script.requests();
    assert!(requests.len() > 2, "{requests:?}");
    assert_eq!(requests[0].precondition, None);
    assert_eq!(requests[0].range_header(), "bytes=0-262143");
    for request in &requests[1..] {
        assert_eq!(
            request.precondition,
            Some(Precondition::IfMatch("\"v1\"".into()))
        );
    }

    let script = Script::new(MIB);
    script.set_etag(None);
    let object = session(&script).open(URL).unwrap();
    read_all(&mut object.reader()).unwrap();
    for request in &script.requests()[1..] {
        assert_eq!(
            request.precondition,
            Some(Precondition::IfUnmodifiedSince(
                "Sun, 06 Nov 1994 08:49:37 GMT".into()
            ))
        );
    }
}

#[test]
fn an_etag_change_between_ranges_fails_as_media_changed() {
    let script = Script::new(2 * MIB);
    let object = session(&script).open(URL).unwrap();
    script.set_etag(Some("\"v2\""));
    let error = read_all(&mut object.reader()).expect_err("a new tag is a change");
    assert_eq!(failure(&error), RemoteFailure::Changed);

    let script = Script::new(2 * MIB);
    let object = session(&script).open(URL).unwrap();
    script.set_etag(Some("W/\"v1\""));
    assert_eq!(
        read_all(&mut object.reader()).unwrap(),
        content(2 * MIB),
        "a weak tag compares weakly"
    );
}

#[test]
fn a_total_change_between_ranges_fails_as_media_changed() {
    let script = Script::new(2 * MIB);
    let object = session(&script).open(URL).unwrap();
    script.object.lock().unwrap().bytes = content(3 * MIB);
    let error = read_all(&mut object.reader()).expect_err("a new length is a change");
    assert_eq!(failure(&error), RemoteFailure::Changed);
    assert_eq!(script.requests().len(), 2, "never retried");
}

#[test]
fn a_412_fails_as_media_changed_and_is_not_retried() {
    let script = Script::with_hook(2 * MIB, |_, _, index| {
        (index > 0).then(|| Ok(status(412, ResponseHeaders::default())))
    });
    let object = session(&script).open(URL).unwrap();
    let error = read_all(&mut object.reader()).expect_err("a 412 is a change");
    assert_eq!(failure(&error), RemoteFailure::Changed);
    assert_eq!(RemoteError::of(&error).unwrap().status, Some(412));
    assert_eq!(script.requests().len(), 2);
}

#[test]
fn a_body_that_ends_early_resumes_from_the_first_missing_byte() {
    const SENT: usize = 100_000;
    let clock = Arc::new(ManualClock::default());
    let script = Script::with_hook(2 * MIB, |script, request, index| {
        (index == 1).then(|| {
            let mut bytes = script.range_bytes(request);
            bytes.truncate(SENT);
            Ok(script.served(request, Box::new(std::io::Cursor::new(bytes))))
        })
    });
    let object = manual_session(&script, &clock).open(URL).unwrap();
    assert_eq!(read_all(&mut object.reader()).unwrap(), content(2 * MIB));
    let requests = script.requests();
    assert_eq!(requests[2].first, requests[1].first + SENT as u64);
    assert_eq!(requests[2].last, requests[1].last);
    assert_eq!(clock.sleeps(), [BACKOFF[0]]);
}

#[test]
fn a_reset_is_retried_at_most_five_times_with_the_backoff() {
    let clock = Arc::new(ManualClock::default());
    let script = Script::with_hook(2 * MIB, |_, _, index| {
        (index > 0).then(|| Err(TransportFailure::Unreachable("connection reset".into())))
    });
    let session = manual_session(&script, &clock);
    let object = session.open(URL).unwrap();
    let error = read_all(&mut object.reader()).expect_err("five resets fail");
    assert_eq!(failure(&error), RemoteFailure::Unreachable);
    assert!(error.to_string().contains("connection reset"), "{error}");
    assert_eq!(script.requests().len(), 1 + MAX_ATTEMPTS);
    assert_eq!(clock.sleeps(), BACKOFF);
    assert_eq!(session.counts().retries, 4);
}

#[test]
fn a_429_with_a_long_retry_after_is_media_busy_with_its_delay() {
    let busy = |code: u16, seconds: &'static str| {
        move |_: &Script, _: &RangeRequest, index: usize| {
            (index == 1).then(|| {
                Ok(status(
                    code,
                    ResponseHeaders {
                        retry_after: Some(seconds.into()),
                        ..ResponseHeaders::default()
                    },
                ))
            })
        }
    };
    let clock = Arc::new(ManualClock::default());
    let script = Script::with_hook(2 * MIB, busy(429, "45"));
    let object = manual_session(&script, &clock).open(URL).unwrap();
    let error = read_all(&mut object.reader()).expect_err("a long Retry-After is busy");
    assert_eq!(
        failure(&error),
        RemoteFailure::Busy {
            retry_after_ms: 45_000
        }
    );
    assert_eq!(script.requests().len(), 2);
    assert_eq!(clock.sleeps(), []);

    let script = Script::with_hook(2 * MIB, busy(503, "600"));
    let object = manual_session(&script, &clock).open(URL).unwrap();
    let error = read_all(&mut object.reader()).expect_err("a long Retry-After is busy");
    assert_eq!(
        failure(&error),
        RemoteFailure::Busy {
            retry_after_ms: 60_000
        }
    );

    let clock = Arc::new(ManualClock::default());
    let script = Script::with_hook(2 * MIB, busy(503, "2"));
    let object = manual_session(&script, &clock).open(URL).unwrap();
    assert_eq!(read_all(&mut object.reader()).unwrap(), content(2 * MIB));
    assert_eq!(clock.sleeps(), [Duration::from_secs(2)]);
}

#[test]
fn a_403_is_media_rejected_and_is_not_retried() {
    let script = Script::with_hook(MIB, |_, _, _| {
        Some(Ok(status(403, ResponseHeaders::default())))
    });
    let error = session(&script).open(URL).err().expect("a 403 is rejected");
    assert_eq!(error.failure, RemoteFailure::Rejected);
    assert!(error.message.contains("signed URL"), "{}", error.message);
    assert!(!error.message.contains("secret"), "{}", error.message);
    assert_eq!(script.requests().len(), 1);
}

#[test]
fn a_404_or_410_at_the_probe_is_offline_and_an_empty_object_has_no_media() {
    for code in [404, 410] {
        let script = Script::with_hook(MIB, move |_, _, _| {
            Some(Ok(status(code, ResponseHeaders::default())))
        });
        let error = session(&script).open(URL).err().unwrap();
        assert_eq!(error.failure, RemoteFailure::Offline);
    }
    let script = Script::with_hook(MIB, |_, _, _| {
        Some(Ok(status(
            416,
            ResponseHeaders {
                content_range: Some("bytes */0".into()),
                ..ResponseHeaders::default()
            },
        )))
    });
    assert_eq!(
        session(&script).open(URL).err().unwrap().failure,
        RemoteFailure::Empty
    );
}

#[test]
fn a_read_with_no_byte_for_30_seconds_is_media_unreachable() {
    let released = Arc::new(AtomicBool::new(false));
    let script = hanging_after_probe(&released);
    let clock = Arc::new(ManualClock::default());
    let object = manual_session(&script, &clock).open(URL).unwrap();
    let mut reader = object.reader();
    reader.seek(SeekFrom::Start(2 * MIB)).unwrap();
    let reading = std::thread::spawn(move || reader.read(&mut [0; 16]));
    await_requests(&script, 2);
    clock.advance(Duration::from_secs(29));
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !reading.is_finished(),
        "29 s without a byte is still a wait"
    );
    clock.advance(Duration::from_secs(1));
    let error = reading.join().unwrap().expect_err("30 s without a byte");
    released.store(true, Ordering::Release);
    assert_eq!(failure(&error), RemoteFailure::Unreachable);
    assert!(
        error.to_string().contains("no byte arrived for 30 s"),
        "{error}"
    );
}

#[test]
fn a_network_wait_is_not_a_stall_under_a_lowered_stall_limit() {
    const STALL_LIMIT: Duration = Duration::from_millis(100);
    const SERVER_DELAY: Duration = Duration::from_millis(300);
    let script = Script::with_hook(2 * MIB, |script, request, index| {
        (index > 0).then(|| {
            let body = Slow {
                bytes: std::io::Cursor::new(script.range_bytes(request)),
                delay: SERVER_DELAY,
                step: usize::MAX,
                dropped: Arc::new(AtomicBool::new(false)),
            };
            Ok(script.served(request, Box::new(body)))
        })
    });
    let object = session(&script).open(URL).unwrap();
    let request_mark = Arc::new(Mutex::new(Instant::now()));
    let operation_mark = Arc::new(Mutex::new(Instant::now()));
    let stalled = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let watcher = {
        let (request_mark, operation_mark, stalled, done) = (
            Arc::clone(&request_mark),
            Arc::clone(&operation_mark),
            Arc::clone(&stalled),
            Arc::clone(&done),
        );
        std::thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                let quiet = request_mark
                    .lock()
                    .unwrap()
                    .elapsed()
                    .max(operation_mark.lock().unwrap().elapsed());
                if quiet > STALL_LIMIT {
                    stalled.store(true, Ordering::Release);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };
    let started = Instant::now();
    {
        let (request, operation) = (Arc::clone(&request_mark), Arc::clone(&operation_mark));
        let _request = crate::request_watch::watch_request(move || {
            *request.lock().unwrap() = Instant::now();
        });
        let _operation = crate::recovery::watch_decode_progress(move || {
            *operation.lock().unwrap() = Instant::now();
        });
        let mut reader = object.reader();
        reader.seek(SeekFrom::Start(MIB)).unwrap();
        let mut bytes = [0; 16];
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, content(MIB + 16)[at(MIB)..]);
    }
    done.store(true, Ordering::Release);
    watcher.join().unwrap();
    assert!(started.elapsed() >= SERVER_DELAY);
    assert!(
        !stalled.load(Ordering::Acquire),
        "the wait went unmarked past the stall limit"
    );
}

#[test]
fn a_cancel_ends_a_waiting_read_within_10_ms() {
    let released = Arc::new(AtomicBool::new(false));
    let script = hanging_after_probe(&released);
    let cancel = CancelToken::default();
    let session = cancellable_session(&script, &cancel);
    let object = session.open(URL).unwrap();
    let mut reader = object.reader();
    reader.seek(SeekFrom::Start(2 * MIB)).unwrap();
    let reading = std::thread::spawn(move || {
        let result = reader.read(&mut [0; 16]);
        (result, Instant::now())
    });
    await_requests(&script, 2);
    std::thread::sleep(Duration::from_millis(50));
    assert!(!reading.is_finished());
    let cancelled = Instant::now();
    cancel.cancel();
    let (result, returned) = reading.join().unwrap();
    released.store(true, Ordering::Release);
    assert_eq!(failure(&result.unwrap_err()), RemoteFailure::Cancelled);
    let late = returned.duration_since(cancelled);
    assert!(late <= Duration::from_millis(10), "{late:?}");
}

#[test]
fn a_cancelled_read_is_not_error_kind_interrupted() {
    let released = Arc::new(AtomicBool::new(false));
    let script = hanging_after_probe(&released);
    let cancel = CancelToken::default();
    let session = cancellable_session(&script, &cancel);
    let object = session.open(URL).unwrap();
    let mut reader = object.reader();
    reader.seek(SeekFrom::Start(2 * MIB)).unwrap();
    cancel.cancel();
    let error = reader.read_exact(&mut [0; 16]).unwrap_err();
    assert_ne!(error.kind(), std::io::ErrorKind::Interrupted);
    assert_eq!(failure(&error), RemoteFailure::Cancelled);
    let error = read_all(&mut object.stream()).unwrap_err();
    assert_ne!(error.kind(), std::io::ErrorKind::Interrupted);
    assert_eq!(failure(&error), RemoteFailure::Cancelled);
    released.store(true, Ordering::Release);
}

#[test]
fn a_cancelled_probe_is_cancelled() {
    let cancel = CancelToken::default();
    cancel.cancel();
    let script = Script::new(MIB);
    let error = cancellable_session(&script, &cancel)
        .open(URL)
        .err()
        .unwrap();
    assert_eq!(error.failure, RemoteFailure::Cancelled);
}

#[test]
fn sequential_reads_double_the_window_up_to_8_mib() {
    let script = Script::new(40 * MIB);
    let object = session(&script).open(URL).unwrap();
    assert_eq!(read_all(&mut object.reader()).unwrap(), content(40 * MIB));
    let lengths = asked(&script);
    assert_eq!(
        lengths[..5],
        [MIB, 2 * MIB, 4 * MIB, 8 * MIB, 8 * MIB],
        "{lengths:?}"
    );
    assert!(
        lengths.iter().all(|length| *length <= 8 * MIB),
        "{lengths:?}"
    );
    let requests = script.requests();
    for pair in requests.windows(2) {
        assert_eq!(pair[1].first, pair[0].last + 1, "every byte asked for once");
    }
}

#[test]
fn a_far_seek_drops_the_open_get() {
    let dropped = Arc::new(AtomicBool::new(false));
    let first_dropped = Arc::clone(&dropped);
    let script = Script::with_hook(32 * MIB, move |script, request, index| {
        (index == 1).then(|| {
            let body = Slow {
                bytes: std::io::Cursor::new(script.range_bytes(request)),
                delay: Duration::from_millis(20),
                step: 16 * 1024,
                dropped: Arc::clone(&first_dropped),
            };
            Ok(script.served(request, Box::new(body)))
        })
    });
    let session = session(&script);
    let object = session.open(URL).unwrap();
    let mut reader = object.reader();
    let mut bytes = vec![0; 1024];
    reader.read_exact(&mut bytes).unwrap();
    await_requests(&script, 2);
    let far = 20 * MIB + 1000;
    reader.seek(SeekFrom::Start(far)).unwrap();
    reader.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes, content(far + 1024)[at(far)..]);
    let requests = script.requests();
    assert_eq!(
        requests[1].range_header(),
        format!("bytes={}-{}", BLOCK_BYTES, BLOCK_BYTES + MIB - 1)
    );
    assert_eq!(requests[2].first, 80 * BLOCK_BYTES);
    assert_eq!(
        requests[2].last,
        81 * BLOCK_BYTES - 1,
        "one block after a far seek"
    );
    let until = Instant::now() + Duration::from_secs(5);
    while !dropped.load(Ordering::Acquire) {
        assert!(Instant::now() < until, "the open GET was not dropped");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(session.counts().bytes < BLOCK_BYTES * 2 + MIB);
}

#[test]
fn the_cache_stays_under_its_budget_and_shrinks_read_ahead() {
    let script = Script::new(24 * MIB);
    let session = RemoteSession::new(
        Arc::clone(&script) as Arc<dyn RangeTransport>,
        CancelToken::default(),
        MIN_CACHE_BYTES,
    );
    let object = session.open(URL).unwrap();
    let (mut first, mut second) = (object.reader(), object.reader());
    second.seek(SeekFrom::Start(12 * MIB)).unwrap();
    let (mut one, mut two) = (Vec::new(), Vec::new());
    let mut chunk = vec![0; 64 * 1024];
    let mut most = 0;
    loop {
        let a = first.read(&mut chunk).unwrap();
        one.extend_from_slice(&chunk[..a]);
        let b = second.read(&mut chunk).unwrap();
        two.extend_from_slice(&chunk[..b]);
        most = most.max(session.cached_bytes());
        if a == 0 && b == 0 {
            break;
        }
    }
    let whole = content(24 * MIB);
    assert_eq!(one, whole);
    assert_eq!(two, whole[at(12 * MIB)..]);
    assert!(most <= MIN_CACHE_BYTES, "{most}");
    let lengths = asked(&script);
    assert!(
        lengths.iter().all(|length| *length <= MIN_CACHE_BYTES / 4),
        "{lengths:?}"
    );
}

#[test]
fn two_requests_never_share_an_object() {
    let script = Script::new(2 * MIB);
    let (first, second) = (session(&script), session(&script));
    let one = first.open(URL).unwrap();
    assert!(
        Arc::ptr_eq(&one, &first.open(URL).unwrap()),
        "one probe per URL in a request"
    );
    assert_eq!(script.requests().len(), 1);
    script.set_etag(Some("\"v2\""));
    let two = second.open(URL).unwrap();
    assert_eq!(script.requests().len(), 2, "each request probes for itself");
    assert_eq!(two.etag(), Some("\"v2\""));
    assert_eq!(read_all(&mut two.reader()).unwrap(), content(2 * MIB));
    let error = read_all(&mut one.reader()).expect_err("the first request read v1");
    assert_eq!(failure(&error), RemoteFailure::Changed);
}

#[test]
fn a_stream_reads_the_whole_object_past_the_cache() {
    let script = Script::new(3 * MIB);
    let session = session(&script);
    let object = session.open(URL).unwrap();
    let held = session.cached_bytes();
    assert_eq!(read_all(&mut object.stream()).unwrap(), content(3 * MIB));
    assert_eq!(session.cached_bytes(), held);
    assert_eq!(asked(&script), [3 * MIB]);
}

#[test]
fn a_seek_to_the_end_needs_no_request() {
    let script = Script::new(3 * MIB + 5);
    let object = session(&script).open(URL).unwrap();
    let mut reader = object.reader();
    assert_eq!(reader.seek(SeekFrom::End(0)).unwrap(), 3 * MIB + 5);
    assert_eq!(reader.read(&mut [0; 4]).unwrap(), 0);
    assert_eq!(script.requests().len(), 1);
    assert_eq!(object.shown_url(), "https://media.example/take.mp4");
}

#[test]
fn a_shown_url_has_no_user_info_query_or_fragment() {
    assert_eq!(
        shown_url("https://user:pass@media.example:8443/a/b.mov?sig=1#t=2"),
        "https://media.example:8443/a/b.mov"
    );
    assert_eq!(shown_url("/srv/take:1.mov"), "/srv/take:1.mov");
}

use super::https_server::{HttpsServer, TestRequest, TestResponse, ranges};
use super::*;
use crate::media_policy::{MediaOrigins, MediaPolicy, PrivateAddresses};

const SECRET: &str = "signature=secret";

/// Serves `bytes` by range under its tag, trusted by the media transports made after it.
fn serving(bytes: Vec<u8>) -> (HttpsServer, Arc<Mutex<String>>) {
    let etag = Arc::new(Mutex::new("\"v1\"".to_owned()));
    let server = HttpsServer::start(ranges(Arc::new(bytes), Arc::clone(&etag)));
    crate::media_fetch::trust_test_root(server.root());
    (server, etag)
}

fn listing(origins: &[&HttpsServer], private: PrivateAddresses) -> crate::media_policy::TestPolicy {
    crate::media_policy::set_for_test(MediaPolicy {
        origins: MediaOrigins::List(
            origins
                .iter()
                .map(|server| crate::origin::Origin::of_url(&server.origin()).expect("origin"))
                .collect(),
        ),
        private_addresses: private,
    })
}

fn conform(project: &Value, assets: &Value, hash: &str) -> Result<Value, HostError> {
    let line = json!({
        "protocolVersion": 2,
        "operation": "conform",
        "requestId": "remote-conform",
        "project": project,
        "assets": assets,
        "remoteContentHash": hash,
    })
    .to_string();
    match parse_request(&line)? {
        Request::Conform(request) => {
            crate::host::conform_media(request, VideoDecodePolicy::Software)
        }
        _ => unreachable!("a conform line parses as a conform"),
    }
}

/// Stereo 16-bit PCM at 48 kHz, `seconds` long.
fn wav(seconds: u32) -> Vec<u8> {
    let frames = 48_000 * seconds;
    let data = frames * 4;
    let mut bytes = Vec::with_capacity(44 + data as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&48_000_u32.to_le_bytes());
    bytes.extend_from_slice(&(48_000_u32 * 4).to_le_bytes());
    bytes.extend_from_slice(&4_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data.to_le_bytes());
    for frame in 0..frames {
        let sample = i16::try_from(frame % 20_000).expect("fits").to_le_bytes();
        bytes.extend_from_slice(&sample);
        bytes.extend_from_slice(&sample);
    }
    bytes
}

fn tone_entry(conformed: &Value) -> &Value {
    conformed["catalog"]["assets"]
        .as_array()
        .expect("assets")
        .iter()
        .find(|entry| entry["source_id"] == "tone")
        .expect("the tone's entry")
}

fn video_project() -> Value {
    serde_json::from_str(include_str!(
        "../../../../tests/cases/host-video-export-project.json"
    ))
    .expect("project")
}

fn export(
    host: &mut Host,
    source: &str,
    output: &Path,
    cache: Option<&Path>,
) -> Result<Value, HostError> {
    let mut request = json!({
        "protocolVersion": 2,
        "operation": "export",
        "requestId": "remote-export",
        "project": video_project(),
        "assets": {"video-source": source},
        "startTicks": 0,
        "endTicks": 120_000,
        "frameRate": {"num": 30, "den": 1},
        "includeAudio": false,
        "videoCodec": "mpeg4-part2",
        "output": output,
    });
    if let Some(cache) = cache {
        request["renderCache"] = json!({"directory": cache, "backlog": "wait"});
    }
    host.handle(parse_request(&request.to_string())?, &mut |_| Ok(()))
}

#[test]
fn an_export_of_a_remote_source_is_byte_identical_to_the_file() {
    let _gpu = gpu_test_lock();
    let file = corpus("h264-bt709-copy.mp4");
    let (server, _) = serving(std::fs::read(&file).expect("the file"));
    let _policy = listing(&[&server], PrivateAddresses::Allow);
    let directory = scratch_directory("remote-export");
    let mut host = create_gpu_test_host(VideoDecodePolicy::Software).expect("host");
    let local = export(
        &mut host,
        &file.to_string_lossy(),
        &directory.join("file.mp4"),
        None,
    )
    .expect("the file exports");
    let url = server.url(&format!("/h264-bt709-copy.mp4?{SECRET}"));
    let remote =
        export(&mut host, &url, &directory.join("remote.mp4"), None).expect("the URL exports");
    assert_eq!(
        std::fs::read(directory.join("file.mp4")).expect("output"),
        std::fs::read(directory.join("remote.mp4")).expect("output"),
        "the outputs differ"
    );
    assert_eq!(local["frames"], remote["frames"]);
    let performance = &remote["performance"];
    assert!(
        performance["remoteRequests"].as_u64() > Some(0),
        "{performance}"
    );
    assert!(
        performance["remoteBytes"].as_u64() > Some(0),
        "{performance}"
    );
    assert!(
        performance.get("remoteRetries").is_some() && performance.get("remoteReadWaitMs").is_some()
    );
    assert!(local["performance"].get("remoteBytes").is_none());
    assert!(!remote.to_string().contains(SECRET));
}

#[test]
fn conform_fetches_a_fraction_of_a_long_source() {
    let bytes = wav(300);
    let total = bytes.len();
    let (server, _) = serving(bytes);
    let _policy = listing(&[&server], PrivateAddresses::Allow);
    let project = image_and_tone_project(alpha2_time::TICKS_PER_SECOND, 48_000);
    let conformed = conform(
        &project,
        &json!({"tone": server.url(&format!("/tone.wav?{SECRET}"))}),
        "skip",
    )
    .expect("conform");
    let tone = tone_entry(&conformed);
    assert_eq!(tone["original"]["state"], "online", "{tone}");
    assert_eq!(
        tone["original"]["fingerprint"]["byte_length"],
        json!(total),
        "{tone}"
    );
    let fetched = server.served_bytes();
    assert!(fetched < total / 4, "fetched {fetched} of {total} bytes");
}

#[test]
fn a_url_is_refused_under_the_default_policy_before_any_connection() {
    let (server, _) = serving(wav(1));
    let project = image_and_tone_project(alpha2_time::TICKS_PER_SECOND, 48_000);
    let error = conform(&project, &json!({"tone": server.url("/tone.wav")}), "skip")
        .expect_err("no origin is listed by default");
    assert_eq!(error.code, "MEDIA_REFUSED");
    assert_eq!(
        error.details.as_ref().expect("details")["policy"],
        "origins"
    );
    assert_eq!(server.connections(), 0);
}

#[test]
fn a_redirect_to_an_unlisted_origin_is_refused_before_it_is_contacted() {
    let (elsewhere, _) = serving(wav(1));
    let target = elsewhere.url(&format!("/tone.wav?{SECRET}"));
    let listed = HttpsServer::start(move |_: &TestRequest| {
        TestResponse::status(302).header("Location", &target)
    });
    crate::media_fetch::trust_test_root(listed.root());
    let _policy = listing(&[&listed], PrivateAddresses::Allow);
    let project = image_and_tone_project(alpha2_time::TICKS_PER_SECOND, 48_000);
    let error = conform(&project, &json!({"tone": listed.url("/tone.wav")}), "skip")
        .expect_err("the redirect leaves the listed origins");
    assert_eq!(error.code, "MEDIA_REFUSED", "{}", error.message);
    let details = error.details.as_ref().expect("details");
    assert_eq!(details["policy"], "redirect");
    assert_eq!(details["source"], "tone");
    assert_eq!(elsewhere.connections(), 0);
    assert!(!error.message.contains(SECRET), "{}", error.message);
}

#[test]
fn a_loopback_address_is_refused_unless_private_addresses_are_allowed() {
    let (server, _) = serving(wav(1));
    let project = image_and_tone_project(alpha2_time::TICKS_PER_SECOND, 48_000);
    let url = server.url("/tone.wav");
    {
        let _policy = listing(&[&server], PrivateAddresses::Refuse);
        let error = conform(&project, &json!({"tone": &url}), "skip")
            .expect_err("localhost is a loopback address");
        assert_eq!(error.code, "MEDIA_REFUSED", "{}", error.message);
        assert_eq!(
            error.details.as_ref().expect("details")["policy"],
            "address"
        );
        assert!(
            error.message.contains("--media-private-addresses allow"),
            "{}",
            error.message
        );
        assert_eq!(server.connections(), 0);
    }
    let _policy = listing(&[&server], PrivateAddresses::Allow);
    let conformed = conform(&project, &json!({"tone": &url}), "skip").expect("allowed");
    assert_eq!(tone_entry(&conformed)["original"]["state"], "online");
}

#[test]
fn a_404_at_the_probe_conforms_offline_with_the_shown_url() {
    let server = HttpsServer::start(|_: &TestRequest| TestResponse::status(404));
    crate::media_fetch::trust_test_root(server.root());
    let _policy = listing(&[&server], PrivateAddresses::Allow);
    let project = image_and_tone_project(alpha2_time::TICKS_PER_SECOND, 48_000);
    let conformed = conform(
        &project,
        &json!({"tone": server.url(&format!("/tone.wav?{SECRET}#t=1"))}),
        "skip",
    )
    .expect("an offline source is a conform answer");
    let tone = tone_entry(&conformed);
    assert_eq!(tone["original"]["state"], "offline", "{tone}");
    assert_eq!(tone["original"]["path"], server.url("/tone.wav"));
    assert_eq!(
        server.requests()[0].path,
        format!("/tone.wav?{SECRET}"),
        "the query is sent, the fragment is not"
    );
    assert!(!conformed.to_string().contains(SECRET));
}

#[test]
fn no_message_or_diagnostic_carries_the_query() {
    let rejecting = HttpsServer::start(|_: &TestRequest| TestResponse::status(403));
    crate::media_fetch::trust_test_root(rejecting.root());
    let untrusted = HttpsServer::start(|_: &TestRequest| TestResponse::status(403));
    let _policy = listing(&[&rejecting, &untrusted], PrivateAddresses::Allow);
    let project = image_and_tone_project(alpha2_time::TICKS_PER_SECOND, 48_000);
    let refused = |server: &HttpsServer| {
        conform(
            &project,
            &json!({"tone": server.url(&format!("/tone.wav?{SECRET}"))}),
            "skip",
        )
        .expect_err("refused")
    };
    let rejected = refused(&rejecting);
    assert_eq!(rejected.code, "MEDIA_REJECTED");
    assert!(
        rejected.message.contains("signed URL"),
        "{}",
        rejected.message
    );
    let tls = refused(&untrusted);
    assert_eq!(tls.code, "MEDIA_TLS_FAILED", "{}", tls.message);
    assert_eq!(untrusted.requests().len(), 0, "nothing was asked of it");
    for error in [&rejected, &tls] {
        let answered = serde_json::to_string(error).expect("json");
        assert!(!answered.contains(SECRET), "{answered}");
        assert_eq!(
            error.details.as_ref().expect("details")["url"],
            json!(format!(
                "{}/tone.wav",
                if error.code == "MEDIA_TLS_FAILED" {
                    untrusted.origin()
                } else {
                    rejecting.origin()
                }
            ))
        );
    }
}

#[test]
fn verify_hashes_a_remote_original_and_skip_does_not() {
    let bytes = wav(120);
    let total = bytes.len();
    let digest = crate::assets::hex(&<sha2::Sha256 as sha2::Digest>::digest(&bytes));
    let (server, _) = serving(bytes);
    let _policy = listing(&[&server], PrivateAddresses::Allow);
    let mut project = image_and_tone_project(alpha2_time::TICKS_PER_SECOND, 48_000);
    project["sources"][1]["fingerprint"] = json!({"content_sha256": digest});
    let url = server.url(&format!("/tone.wav?{SECRET}"));
    let skipped = conform(&project, &json!({"tone": &url}), "skip").expect("skip");
    let fingerprint = &tone_entry(&skipped)["original"]["fingerprint"];
    assert!(fingerprint.get("content_sha256").is_none(), "{fingerprint}");
    let before = server.served_bytes();
    assert!(before < total, "skip reads a part");
    let verified = conform(&project, &json!({"tone": &url}), "verify").expect("verify");
    let fingerprint = &tone_entry(&verified)["original"]["fingerprint"];
    assert_eq!(
        fingerprint["content_sha256"],
        json!(digest),
        "{fingerprint}"
    );
    assert!(
        server.served_bytes() - before >= total,
        "verify reads the whole object"
    );
}

#[test]
fn a_conform_reading_a_remote_source_is_cancelled() {
    let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let tone = Arc::new(wav(30));
    let etag = Arc::new(Mutex::new("\"v1\"".to_owned()));
    let answer = ranges(Arc::clone(&tone), etag);
    let held = Arc::clone(&released);
    let server = HttpsServer::start(move |request: &TestRequest| {
        let until = Instant::now() + Duration::from_secs(20);
        while !held.load(std::sync::atomic::Ordering::Acquire) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        answer(request)
    });
    crate::media_fetch::trust_test_root(server.root());
    let origin = crate::origin::Origin::of_url(&server.origin()).expect("origin");
    let url = server.url("/tone.wav");
    let project = image_and_tone_project(alpha2_time::TICKS_PER_SECOND, 48_000);
    let conforming = std::thread::spawn(move || {
        let _request = crate::termination::terminations().begin_request("remote-conform");
        let _policy = crate::media_policy::set_for_test(MediaPolicy {
            origins: MediaOrigins::List(vec![origin]),
            private_addresses: PrivateAddresses::Allow,
        });
        let started = Instant::now();
        (conform(&project, &json!({"tone": url}), "skip"), started)
    });
    let until = Instant::now() + Duration::from_secs(10);
    while server.requests().is_empty() {
        assert!(Instant::now() < until, "the probe never reached the server");
        std::thread::sleep(Duration::from_millis(5));
    }
    let cancelled = Instant::now();
    assert_eq!(
        crate::termination::terminations().cancel_request("remote-conform"),
        "running"
    );
    let (result, _) = conforming.join().expect("conform thread");
    released.store(true, std::sync::atomic::Ordering::Release);
    let error = result.expect_err("a cancelled conform");
    assert_eq!(error.code, "CANCELLED", "{}", error.message);
    assert!(cancelled.elapsed() < Duration::from_secs(2));
}

#[test]
fn a_new_signature_of_the_same_object_hits_the_render_cache() {
    let _gpu = gpu_test_lock();
    let (server, _) = serving(std::fs::read(corpus("h264-bt709-copy.mp4")).expect("the file"));
    let _policy = listing(&[&server], PrivateAddresses::Allow);
    let directory = scratch_directory("remote-render-cache");
    let cache = directory.join("cache");
    let mut host = create_gpu_test_host(VideoDecodePolicy::Software).expect("host");
    let first = export(
        &mut host,
        &server.url("/take.mp4?signature=one"),
        &directory.join("one.mp4"),
        Some(&cache),
    )
    .expect("first export");
    assert!(
        first["renderCache"]["writes"].as_u64() > Some(0),
        "{}",
        first["renderCache"]
    );
    let second = export(
        &mut host,
        &server.url("/take.mp4?signature=two"),
        &directory.join("two.mp4"),
        Some(&cache),
    )
    .expect("second export");
    assert_eq!(
        second["renderCache"]["hits"], first["renderCache"]["writes"],
        "{}",
        second["renderCache"]
    );
    assert_eq!(
        std::fs::read(directory.join("one.mp4")).expect("output"),
        std::fs::read(directory.join("two.mp4")).expect("output")
    );
}

#[test]
fn a_busy_server_is_media_busy_with_its_delay() {
    let server =
        HttpsServer::start(|_: &TestRequest| TestResponse::status(429).header("Retry-After", "45"));
    crate::media_fetch::trust_test_root(server.root());
    let _policy = listing(&[&server], PrivateAddresses::Allow);
    let project = image_and_tone_project(alpha2_time::TICKS_PER_SECOND, 48_000);
    let error = conform(&project, &json!({"tone": server.url("/tone.wav")}), "skip")
        .expect_err("a long Retry-After is busy");
    assert_eq!(error.code, "MEDIA_BUSY", "{}", error.message);
    let details = error.details.as_ref().expect("details");
    assert_eq!(details["retryAfterMs"], 45_000);
    assert_eq!(details["status"], 429);
    assert_eq!(details["source"], "tone");
    assert_eq!(server.requests().len(), 1, "not tried again");
}

use super::requests::{SERVE_CHILD, ServeChild};
use super::*;
use alpha2_engine::extract::fixtures::{digest_values, fnv1a};

const VIDEO: &str = "h264-bt709-copy.mp4";

fn project() -> Value {
    json!({
        "schema_version": 1, "id": "p", "name": "P",
        "settings": { "width": 320, "height": 180, "frame_rate": { "num": 30, "den": 1 }, "sample_rate": 48_000 },
        "sources": [{ "id": "v", "kind": "video", "locator": "v.mp4" }],
        "timeline": { "tracks": [{ "id": "t", "name": "T", "kind": "video", "clips": [
            { "id": "c", "source_id": "v", "timeline_range": { "start": 0, "end": 60_000 }, "source_range": { "start": 0, "end": 60_000 } }
        ] }] }
    })
}

fn descriptor() -> Value {
    serde_json::to_value(alpha2_engine::extract::fixtures::digest_descriptor(
        alpha2_engine::extract::fixtures::video_input(
            alpha2_engine::extract::VideoFormat::Luma8,
            960,
        ),
    ))
    .expect("descriptor")
}

fn extract(id: &str, end: i64) -> String {
    json!({
        "protocolVersion": 2, "requestId": id, "operation": "extract",
        "project": project(),
        "assets": { "v": corpus(VIDEO) },
        "descriptor": descriptor(),
        "request": {
            "extractor": "alpha2.test.digest", "version": 1,
            "input": { "from": "source", "source_id": "v", "range": { "start": 0, "end": end }, "size": 64 },
            "outputs": { "digest": "frames" }
        }
    })
    .to_string()
}

/// Answers every step of `id` as the digest fixture would, until the response; returns it and
/// how many steps came. `misbehave(seq)` replaces an ack.
fn run_client(
    serving: &mut ServeChild,
    id: &str,
    mut misbehave: impl FnMut(u64, &Value) -> Option<Option<String>>,
) -> (Value, u64) {
    let mut steps = 0;
    loop {
        let (message, attachments) = serving.message_with_attachments(Duration::from_secs(120));
        if message["type"] == "state" || message["type"] == "progress" || message["requestId"] != id
        {
            continue;
        }
        if message["type"] != "extract.step" {
            return (message, steps);
        }
        steps += 1;
        let seq = message["seq"].as_u64().expect("seq");
        if let Some(replacement) = misbehave(seq, &message) {
            if let Some(ack) = replacement {
                serving.write(&ack);
            }
            continue;
        }
        let emits = if message["step"] == "video" {
            let [hi, lo] = digest_values(fnv1a(attachments.iter().copied()));
            json!([{ "output": "digest", "time": message["time"], "values": [hi, lo] }])
        } else {
            json!([])
        };
        serving.write(&json!({ "protocolVersion": 2, "operation": "extract.ack", "requestId": id, "seq": seq, "emits": emits }).to_string());
    }
}

/// The same run in process, with the Rust digest fixture over the host-core reader.
fn in_process(end: i64) -> Vec<alpha2_model::DataSet> {
    let project = alpha2_model::Project::from_json(&project().to_string()).expect("project");
    let request: alpha2_engine::extract::ExtractRequest = serde_json::from_value(json!({
        "extractor": "alpha2.test.digest", "version": 1,
        "input": { "from": "source", "source_id": "v", "range": { "start": 0, "end": end }, "size": 64 },
        "outputs": { "digest": "frames" }
    }))
    .expect("request");
    let mut extractor = alpha2_engine::extract::fixtures::Digest::new(
        alpha2_engine::extract::fixtures::video_input(
            alpha2_engine::extract::VideoFormat::Luma8,
            960,
        ),
    );
    let renderer = alpha2_render::CpuRenderer::new(1);
    let decoder = alpha2_native::NativeDecoder::open(corpus(VIDEO)).expect("decoder");
    let reader = alpha2_host_core::extract::VideoReader::new(
        decoder,
        SourceId::new("v").expect("id"),
        alpha2_media::CancelToken::default(),
    );
    let mut supplier = alpha2_host_core::extract::ReaderSupplier::<
        _,
        alpha2_native::NativeAudioDecoder,
    >::new(&renderer, Some(reader), None);
    alpha2_host_core::extract::run(&project, request, &mut extractor, &mut supplier, None)
        .expect("run")
}

#[test]
fn a_client_extractor_gives_the_data_the_rust_one_does() {
    let _gpu = gpu_test_lock();
    let mut serving = ServeChild::start(SERVE_CHILD, &[]);
    serving.write(&extract("x", 40_000));
    let (answer, steps) = run_client(&mut serving, "x", |_, _| None);
    assert_eq!(answer["ok"], true, "{answer}");
    let sets: Vec<alpha2_model::DataSet> =
        serde_json::from_value(answer["result"]["dataSets"].clone()).expect("data sets");
    let mut expected = in_process(40_000);
    // The host records the conformed original; the in-process run was given none.
    expected[0].made_from.clone_from(&sets[0].made_from);
    assert_eq!(sets, expected);
    assert!(sets[0].made_from.is_some());
    // begin, pass, ten frames, end.
    assert_eq!(steps, 13);
    assert_eq!(serving.finish(), Some(0));
}

#[test]
fn an_invalid_emit_ends_the_request() {
    let _gpu = gpu_test_lock();
    let mut serving = ServeChild::start(SERVE_CHILD, &[]);
    serving.write(&extract("x", 40_000));
    let (answer, _) = run_client(&mut serving, "x", |seq, message| {
        (message["step"] == "video").then(|| {
            Some(json!({ "protocolVersion": 2, "operation": "extract.ack", "requestId": "x", "seq": seq,
                         "emits": [{ "output": "digest", "time": message["time"], "values": [2e10, 0] }] }).to_string())
        })
    });
    assert_eq!(answer["error"]["code"], "INVALID_EMIT", "{answer}");
    assert_eq!(answer["error"]["details"]["index"], 0, "{answer}");
    assert_eq!(serving.finish(), Some(0));
}

#[test]
fn a_silent_client_times_out_and_frees_its_slot() {
    let _gpu = gpu_test_lock();
    let mut serving = ServeChild::start(
        SERVE_CHILD,
        &[(
            "ALPHA2_TEST_SERVE_ARGS",
            "--extract-step-timeout 300 --max-running 1",
        )],
    );
    serving.write(&extract("x", 40_000));
    let (answer, _) = run_client(&mut serving, "x", |_, message| {
        (message["step"] == "video").then_some(None)
    });
    assert_eq!(answer["error"]["code"], "EXTRACTOR_TIMEOUT", "{answer}");
    serving.write(r#"{"protocolVersion":2,"requestId":"next","operation":"capabilities"}"#);
    let next = loop {
        let message = serving.message(Duration::from_secs(60));
        if message["requestId"] == "next" {
            break message;
        }
    };
    assert_eq!(next["ok"], true, "{next}");
    assert_eq!(serving.finish(), Some(0));
}

#[test]
fn a_cancel_mid_pass_answers_cancelled() {
    let _gpu = gpu_test_lock();
    let mut serving = ServeChild::start(SERVE_CHILD, &[]);
    serving.write(&extract("x", 40_000));
    let mut cancelled = false;
    let (answer, _) = run_client(&mut serving, "x", |seq, message| {
        (message["step"] == "video" && seq == 4 && !cancelled).then(|| {
            cancelled = true;
            Some(
                r#"{"protocolVersion":2,"requestId":"x:cancel","operation":"cancel","target":"x"}"#
                    .to_owned(),
            )
        })
    });
    assert_eq!(answer["error"]["code"], "CANCELLED", "{answer}");
    assert_eq!(serving.finish(), Some(0));
}

#[test]
fn an_extract_runs_beside_an_export_and_a_still() {
    let _gpu = gpu_test_lock();
    let dir = scratch_directory("extract-beside");
    let mut serving = ServeChild::start(
        SERVE_CHILD,
        &[("ALPHA2_TEST_SERVE_ARGS", "--max-running 3")],
    );
    serving.write(&extract("x", 40_000));
    // Wait for the extraction to begin, then start the others while it waits on its client.
    let begin = loop {
        let message = serving.message(Duration::from_secs(60));
        if message["type"] == "extract.step" {
            break message;
        }
    };
    let export = json!({ "protocolVersion": 2, "requestId": "e", "operation": "export", "project": project(),
                         "assets": { "v": corpus(VIDEO) }, "output": dir.join("out.mp4"), "includeAudio": false,
                         "startTicks": 0, "endTicks": 40_000, "frameRate": { "num": 30, "den": 1 },
                         "videoCodec": "mpeg4-part2", "overwrite": true });
    let still = json!({ "protocolVersion": 2, "requestId": "s", "operation": "still", "project": project(),
                        "assets": { "v": corpus(VIDEO) }, "stills": [{ "time": 1_000 }] });
    serving.write(&export.to_string());
    serving.write(&still.to_string());
    serving.write(&json!({ "protocolVersion": 2, "operation": "extract.ack", "requestId": "x", "seq": begin["seq"], "emits": [] }).to_string());
    let mut answers = std::collections::HashMap::new();
    let mut pending = begin;
    while answers.len() < 3 {
        let (message, attachments) = if pending.is_null() {
            serving.message_with_attachments(Duration::from_secs(120))
        } else {
            (std::mem::take(&mut pending), Vec::new())
        };
        if message["type"] == "extract.step" {
            if message["seq"] == 1 {
                continue;
            }
            let emits = if message["step"] == "video" {
                let [hi, lo] = digest_values(fnv1a(attachments.iter().copied()));
                json!([{ "output": "digest", "time": message["time"], "values": [hi, lo] }])
            } else {
                json!([])
            };
            serving.write(&json!({ "protocolVersion": 2, "operation": "extract.ack", "requestId": "x", "seq": message["seq"], "emits": emits }).to_string());
        } else if message.get("ok").is_some() {
            answers.insert(
                message["requestId"].as_str().unwrap_or_default().to_owned(),
                message,
            );
        }
    }
    for id in ["x", "e", "s"] {
        assert_eq!(answers[id]["ok"], true, "{id}: {}", answers[id]);
    }
    assert_eq!(serving.finish(), Some(0));
}

/// No acknowledgement can arrive once the input ends: the step is answered `CANCELLED`.
#[test]
fn an_extraction_whose_input_ends_before_an_acknowledgement_is_cancelled() {
    let _gpu = gpu_test_lock();
    let mut serving = ServeChild::start(SERVE_CHILD, &[]);
    serving.write(&extract("x", 40_000));
    loop {
        let message = serving.message(Duration::from_secs(60));
        if message["type"] == "extract.step" {
            break;
        }
    }
    drop(serving.stdin.take());
    let answer = loop {
        let message = serving.message(Duration::from_secs(60));
        if message.get("ok").is_some() {
            break message;
        }
    };
    assert_eq!(answer["error"]["code"], "CANCELLED", "{answer}");
    assert!(
        answer["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("the input ended before step")),
        "{answer}"
    );
    assert_eq!(serving.finish(), Some(0));
}

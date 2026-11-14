use std::time::Duration;

use sha2::{Digest, Sha256};

use super::requests::{SERVE_CHILD, ServeChild};
use super::*;

fn corpus() -> Value {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/cases/surface-conformance.json");
    serde_json::from_slice(&std::fs::read(path).expect("the corpus")).expect("json")
}

fn line(id: &str, project: &Value, edit: &Value) -> String {
    json!({ "protocolVersion": 2, "requestId": id, "operation": "edit",
            "project": project, "edit": edit })
    .to_string()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

#[test]
fn each_corpus_case_answers_as_the_engine() {
    let corpus = corpus();
    for case in corpus["edit"].as_array().expect("cases") {
        let name = case["name"].as_str().expect("name");
        let project = &corpus["projects"][case["project"].as_str().expect("project")];
        let Ok(Request::Edit(request)) = parse_request(&line(name, project, &case["request"]))
        else {
            panic!("{name}: an edit request");
        };
        let expected = &case["expected"];
        match request.answer() {
            Ok(answer) => {
                let text = answer["projectJson"].as_str().expect("projectJson");
                assert_eq!(
                    hex(&Sha256::digest(text.as_bytes())),
                    expected["project_sha256"],
                    "{name}"
                );
                assert_eq!(answer["report"], expected["report"], "{name}");
            }
            Err(error) => {
                assert_eq!(error.code, expected["code"], "{name}");
                assert_eq!(
                    error.details.as_ref().map(|details| &details["refusal"]),
                    Some(&expected["refusal"]),
                    "{name}"
                );
            }
        }
    }
}

#[test]
fn a_served_edit_answers_without_a_device() {
    let corpus = corpus();
    let case = &corpus["edit"][0];
    let mut serving = ServeChild::start(SERVE_CHILD, &[]);
    assert_eq!(serving.message(Duration::from_secs(30))["state"], "ready");
    serving.write(&line(
        "edit",
        &corpus["projects"][case["project"].as_str().expect("project")],
        &case["request"],
    ));
    let answer = serving.message(Duration::from_secs(30));
    assert_eq!(answer["ok"], true, "{answer}");
    let text = answer["result"]["projectJson"]
        .as_str()
        .expect("projectJson");
    assert_eq!(
        hex(&Sha256::digest(text.as_bytes())),
        case["expected"]["project_sha256"]
    );
    serving.write(&line(
        "refused",
        &json!({ "schema_version": 1 }),
        &json!({ "op": "bake" }),
    ));
    let refused = serving.message(Duration::from_secs(30));
    assert_eq!(refused["error"]["code"], "INVALID_INPUT", "{refused}");
    assert_eq!(
        refused["error"]["details"]["refusal"], "invalid_project",
        "{refused}"
    );
    assert_eq!(serving.finish(), Some(0));
}

/// A timeline `points` data set of `samples` samples of `channels` coordinates.
fn points_set(json: &mut String, id: &str, channels: usize, samples: usize) {
    use std::fmt::Write as _;
    write!(json, r#"{{"id":"{id}","anchor":{{"type":"timeline"}},"kind":"points","interpolation":"hold","channels":["#).expect("writes");
    for channel in 0..channels {
        write!(json, "{}\"k{channel}\"", if channel > 0 { "," } else { "" }).expect("writes");
    }
    json.push_str(r#"],"times":["#);
    for sample in 0..samples {
        write!(
            json,
            "{}{}",
            if sample > 0 { "," } else { "" },
            sample * 4004
        )
        .expect("writes");
    }
    json.push_str(r#"],"values":["#);
    for value in 0..samples * channels * 2 {
        // Ten-byte coordinates that vary, as measured data does.
        write!(
            json,
            "{}0.{:08}",
            if value > 0 { "," } else { "" },
            (value * 7_919) % 100_000_000
        )
        .expect("writes");
    }
    json.push_str("]}");
}

#[test]
fn an_edit_that_grows_the_project_past_the_document_cap_is_refused() {
    let cap = alpha2_model::MAX_PROJECT_JSON_BYTES;
    let tracked = 20_000_usize;
    let mut project = String::with_capacity(cap);
    project.push_str(r#"{"schema_version":1,"id":"p","name":"p","settings":{"width":1920,"height":1080,"frame_rate":{"num":30,"den":1},"sample_rate":48000,"background":{"red":0.0,"green":0.0,"blue":0.0}},"sources":[{"id":"v","locator":"asset://v","kind":"video"}],"data":["#);
    points_set(&mut project, "track", 1, tracked);
    let follow = r#"{"mode":"data","data":"track","channel":"k0"}"#;
    let tail = format!(
        r#"],"timeline":{{"tracks":[{{"id":"t","name":"V","kind":"video","clips":[{{"id":"c0","source_id":"v","timeline_range":{{"start":0,"end":{end}}},"source_range":{{"start":0,"end":{end}}},"visual":{{"position":{follow},"scale":{follow}}}}}]}}]}}}}"#,
        end = tracked * 4004
    );
    // Bake adds about 3.6 MB of keyframes; the rest fills the document to 1 MB under the cap.
    let mut set = 0;
    while cap - project.len() - tail.len() > 1_000_000 {
        let samples = ((cap - project.len() - tail.len() - 1_000_000) / (128 * 11)).min(31_250);
        if samples == 0 {
            break;
        }
        project.push(',');
        points_set(&mut project, &format!("fill{set}"), 64, samples);
        set += 1;
    }
    project.push_str(&tail);
    assert!(project.len() <= cap, "{} bytes", project.len());
    let line = format!(
        r#"{{"protocolVersion":2,"requestId":"bake","operation":"edit","project":{project},"edit":{{"op":"bake","targets":[{{"clip":"c0","property":"visual.position"}},{{"clip":"c0","property":"visual.scale"}}]}}}}"#
    );
    let Ok(Request::Edit(request)) = parse_request(&line) else {
        panic!("an edit request");
    };
    let Err(error) = request.answer() else {
        panic!("the edit applied");
    };
    assert_eq!(error.code, "INVALID_INPUT");
    assert_eq!(
        error.details.as_ref().map(|details| &details["refusal"]),
        Some(&json!("result_too_large"))
    );
}

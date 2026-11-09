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

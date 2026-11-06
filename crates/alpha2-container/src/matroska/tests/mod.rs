mod corpus;
mod hostile;
mod mux;
mod seek;
mod write;

use crate::mp4::tests::cases_dir;

pub(super) fn expected() -> serde_json::Value {
    let text = std::fs::read_to_string(cases_dir().join("container-matroska-expected.json"))
        .expect("container-matroska-expected.json");
    serde_json::from_str(&text).expect("expected JSON")
}

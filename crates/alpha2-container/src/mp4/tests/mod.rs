mod corpus;
mod mux;

use std::path::PathBuf;

use crate::{ContainerReader, Packet, TrackId};

pub(super) fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus")
}

pub(super) fn cases_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/cases")
}

pub(super) fn expected() -> serde_json::Value {
    let text = std::fs::read_to_string(cases_dir().join("container-mp4-expected.json"))
        .expect("container-mp4-expected.json");
    serde_json::from_str(&text).expect("expected JSON")
}

/// Every packet of `track`, in decode order.
pub(super) fn packets(reader: &mut dyn ContainerReader, track: TrackId) -> Vec<Packet> {
    let mut packets = Vec::new();
    while let Some(packet) = reader.next_packet(track).expect("packet") {
        packets.push(packet);
    }
    packets
}

mod corpus;
mod demux;
mod mux;
mod write;

use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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

/// An in-memory file the test keeps a handle to after the writer is done with it.
#[derive(Clone, Default)]
pub(super) struct Shared(Arc<Mutex<Cursor<Vec<u8>>>>);

impl Shared {
    pub(super) fn contents(&self) -> Vec<u8> {
        self.0.lock().expect("buffer").get_ref().clone()
    }
}

impl Read for Shared {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.0.lock().expect("buffer").read(buffer)
    }
}

impl Write for Shared {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("buffer").write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Seek for Shared {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.0.lock().expect("buffer").seek(position)
    }
}

#![no_main]

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use alpha2_container::{
    ContainerReader, ContainerWriter, MatroskaFlavor, MatroskaReader, MatroskaWriter,
};
use libfuzzer_sys::fuzz_target;

/// A file in memory that stays readable after the writer is done with it.
#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Cursor<Vec<u8>>>>);

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

// Any bytes open as a Matroska file or are refused, never panic or hang, and every track reads to
// its end with packets no larger than the input. What was read writes back into a file that
// reads again with the tracks the writer took.
fuzz_target!(|input: (i64, &[u8])| {
    let (target, data) = input;
    let Ok(mut reader) = MatroskaReader::new(Cursor::new(data.to_vec())) else {
        return;
    };
    let tracks = reader.tracks().to_vec();
    let mut read = Vec::new();
    for track in &tracks {
        let mut packets = Vec::new();
        while let Ok(Some(packet)) = reader.next_packet(track.id) {
            assert!(packet.data.len() <= data.len());
            assert_eq!(packet.track, track.id);
            packets.push(packet);
            if packets.len() >= 4096 {
                break;
            }
        }
        if reader.seek(track.id, target).is_ok() {
            let _ = reader.next_packet(track.id);
        }
        read.push(packets);
    }
    let buffer = Shared::default();
    let mut writer = Box::new(MatroskaWriter::new(
        buffer.clone(),
        MatroskaFlavor::Matroska,
    ));
    let ids = tracks
        .iter()
        .map(|track| writer.add_track(track).ok())
        .collect::<Vec<_>>();
    for (packets, id) in read.into_iter().zip(&ids) {
        let Some(id) = id else {
            continue;
        };
        for mut packet in packets {
            packet.track = *id;
            if writer.write(packet).is_err() {
                return;
            }
        }
    }
    if writer.finish().is_err() {
        return;
    }
    let written = buffer.0.lock().expect("buffer").get_ref().clone();
    let again = MatroskaReader::new(Cursor::new(written)).expect("a written file reads");
    assert_eq!(again.tracks().len(), ids.iter().flatten().count());
});

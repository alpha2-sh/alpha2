use std::io::Cursor;
use std::path::PathBuf;

use super::expected;
use crate::mp4::tests::{cases_dir, corpus_dir, packets};
use crate::{ContainerReader, MatroskaReader, Packet};

fn every_file() -> Vec<PathBuf> {
    let expected = expected();
    let mut paths = expected["files"]
        .as_object()
        .expect("files")
        .keys()
        .map(|name| corpus_dir().join(name))
        .collect::<Vec<_>>();
    paths.extend(
        expected["fixtures"]
            .as_object()
            .expect("fixtures")
            .keys()
            .map(|name| cases_dir().join(name)),
    );
    paths
}

/// The packet a seek to `target` should land on: the last keyframe at or before it in decode
/// order, else the first packet.
fn landing(read: &[Packet], target: i64) -> usize {
    read.iter()
        .rposition(|packet| packet.keyframe && packet.pts <= target)
        .unwrap_or(0)
}

#[test]
fn seeks_land_on_the_last_keyframe_at_or_before_the_target() {
    let mut failures = Vec::new();
    for path in every_file() {
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        let mut reader = MatroskaReader::open(&path).expect("open");
        let tracks = reader.tracks().to_vec();
        let linear = tracks
            .iter()
            .map(|track| packets(&mut reader, track.id))
            .collect::<Vec<_>>();
        for (track, read) in tracks.iter().zip(&linear) {
            if read.is_empty() {
                continue;
            }
            let mut targets = vec![i64::MIN, read[0].pts - 1, i64::MAX];
            for packet in read.iter().step_by(3) {
                targets.extend([packet.pts - 1, packet.pts, packet.pts + 1]);
            }
            for target in targets {
                let expected = landing(read, target);
                let found = reader.seek(track.id, target).expect("seek");
                let next = reader.next_packet(track.id).expect("packet");
                let ours = (
                    found,
                    next.as_ref()
                        .map(|packet| (packet.pts, packet.data.clone())),
                );
                let theirs = (
                    Some(read[expected].pts),
                    Some((read[expected].pts, read[expected].data.clone())),
                );
                if ours.0 != theirs.0 || ours.1 != theirs.1 {
                    failures.push(format!(
                        "{name} track {} seek {target}: lands at {:?}, expected {:?}",
                        track.id.0,
                        ours.1.map(|(pts, _)| pts),
                        read[expected].pts
                    ));
                    continue;
                }
                // The packets after the landing follow in file order with the same times.
                let after = (0..4)
                    .map_while(|_| reader.next_packet(track.id).expect("packet"))
                    .collect::<Vec<_>>();
                let same = after.iter().zip(&read[expected + 1..]).all(|(a, b)| {
                    (a.pts, a.keyframe, &a.data, a.discard)
                        == (b.pts, b.keyframe, &b.data, b.discard)
                });
                if !same || after.len() != (read.len() - expected - 1).min(4) {
                    failures.push(format!(
                        "{name} track {} seek {target}: the packets after the landing differ",
                        track.id.0
                    ));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn tracks_read_at_different_positions_from_one_reader() {
    let path = corpus_dir().join("matroska-laced.mkv");
    let mut reader = MatroskaReader::open(&path).expect("open");
    let tracks = reader.tracks().to_vec();
    let linear = tracks
        .iter()
        .map(|track| packets(&mut reader, track.id))
        .collect::<Vec<_>>();
    for track in &tracks {
        reader.seek(track.id, i64::MIN).expect("seek");
    }
    let mut interleaved = vec![Vec::new(); tracks.len()];
    let mut open = vec![true; tracks.len()];
    while open.iter().any(|open| *open) {
        for (index, track) in tracks.iter().enumerate() {
            // Track 2 runs ahead of the others three packets at a time.
            for _ in 0..if index == 1 { 3 } else { 1 } {
                match reader.next_packet(track.id).expect("packet") {
                    Some(packet) => interleaved[index].push(packet),
                    None => open[index] = false,
                }
            }
        }
    }
    assert_eq!(interleaved, linear);
}

#[test]
fn a_file_held_in_memory_reads_as_the_file_does() {
    let path = corpus_dir().join("matroska-groups.mkv");
    let mut from_file = MatroskaReader::open(&path).expect("open");
    let mut from_memory =
        MatroskaReader::new(Cursor::new(std::fs::read(&path).expect("read"))).expect("open");
    assert_eq!(from_file.tracks(), from_memory.tracks());
    for track in from_file.tracks().to_vec() {
        assert_eq!(
            packets(&mut from_file, track.id),
            packets(&mut from_memory, track.id)
        );
    }
}

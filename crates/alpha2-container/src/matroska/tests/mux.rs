//! The muxer: corpus files demuxed, written again and read back, against the packets they came
//! from and against `FFmpeg`'s reading of its own stream copy.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::corpus::{Scope, compare_reader};
use super::expected;
use crate::mp4::tests::{Shared, cases_dir, corpus_dir, packets};
use crate::{
    ContainerReader, ContainerWriter, MatroskaFlavor, MatroskaReader, MatroskaWriter, Packet,
    TrackId, TrackInfo,
};

fn flavor_of(name: &str) -> MatroskaFlavor {
    if Path::new(name)
        .extension()
        .is_some_and(|extension| extension == "webm")
    {
        MatroskaFlavor::WebM
    } else {
        MatroskaFlavor::Matroska
    }
}

/// Every track and packet of a file.
pub(super) fn demux(path: &Path) -> (Vec<TrackInfo>, Vec<Vec<Packet>>) {
    let mut reader = MatroskaReader::open(path).expect("open");
    let tracks = reader.tracks().to_vec();
    let read = tracks
        .iter()
        .map(|track| packets(&mut reader, track.id))
        .collect();
    (tracks, read)
}

/// Writes `tracks` and their packets, handing packets over in decode-time order across tracks.
pub(super) fn mux(tracks: &[TrackInfo], read: &[Vec<Packet>], flavor: MatroskaFlavor) -> Vec<u8> {
    let buffer = Shared::default();
    let mut writer = Box::new(MatroskaWriter::new(buffer.clone(), flavor));
    let ids = tracks
        .iter()
        .map(|track| writer.add_track(track).expect("add track"))
        .collect::<Vec<_>>();
    let mut next = vec![0_usize; tracks.len()];
    loop {
        let earliest = (0..tracks.len())
            .filter(|track| next[*track] < read[*track].len())
            .min_by_key(|track| {
                let packet = &read[*track][next[*track]];
                (
                    i128::from(packet.dts) * 1_000_000 / i128::from(tracks[*track].timescale),
                    *track,
                )
            });
        let Some(track) = earliest else {
            break;
        };
        let mut packet = read[track][next[track]].clone();
        packet.track = ids[track];
        writer.write(packet).expect("write");
        next[track] += 1;
    }
    writer.finish().expect("finish");
    buffer.contents()
}

fn remuxable(expected: &Value) -> Vec<(String, PathBuf, Value)> {
    let mut found = Vec::new();
    for (group, dir) in [("files", corpus_dir()), ("fixtures", cases_dir())] {
        for (name, file) in expected[group].as_object().expect("group") {
            if let Some(remux) = file.get("remux") {
                found.push((name.clone(), dir.join(name), remux.clone()));
            }
        }
    }
    found
}

#[test]
fn corpus_files_round_trip_through_the_muxer() {
    let expected = expected();
    let files = remuxable(&expected);
    assert!(files.len() >= 35, "only {} remuxable files", files.len());
    let mut failures = Vec::new();
    for (name, path, remux) in &files {
        let (tracks, read) = demux(path);
        let flavor = flavor_of(name);
        let written = mux(&tracks, &read, flavor);
        if mux(&tracks, &read, flavor) != written {
            failures.push(format!("{name}: two runs write different bytes"));
        }
        let mut reader = MatroskaReader::new(Cursor::new(written.clone())).expect("read back");
        compare_reader(&mut reader, name, remux, Scope::Remux, &mut failures);
        let mut reader = MatroskaReader::new(Cursor::new(written)).expect("read back");
        let again = reader.tracks().to_vec();
        for ((before, after), packets_before) in tracks.iter().zip(&again).zip(&read) {
            let mut facts_before = before.clone();
            let mut facts_after = after.clone();
            for facts in [&mut facts_before, &mut facts_after] {
                facts.duration = None;
                if let Some(video) = &mut facts.video {
                    // The muxer writes the frame rate back only as a millisecond-rounded duration.
                    video.frame_rate = None;
                }
            }
            if facts_before != facts_after {
                failures.push(format!(
                    "{name} track {}: facts change\n    before {facts_before:?}\n    after  {facts_after:?}",
                    before.id.0
                ));
            }
            let packets_after = packets(&mut reader, after.id);
            let key = |packet: &Packet| {
                (
                    packet.pts,
                    packet.keyframe,
                    packet.data.clone(),
                    packet.discard,
                )
            };
            let same = packets_before.len() == packets_after.len()
                && packets_before
                    .iter()
                    .zip(&packets_after)
                    .all(|(a, b)| key(a) == key(b));
            if !same {
                let first = packets_before
                    .iter()
                    .zip(&packets_after)
                    .position(|(a, b)| key(a) != key(b));
                failures.push(format!(
                    "{name} track {}: packets change ({} before, {} after, first difference at {first:?})",
                    before.id.0,
                    packets_before.len(),
                    packets_after.len(),
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn an_empty_segment_is_still_a_readable_file() {
    for flavor in [MatroskaFlavor::Matroska, MatroskaFlavor::WebM] {
        let buffer = Shared::default();
        Box::new(MatroskaWriter::new(buffer.clone(), flavor))
            .finish()
            .expect("finish");
        let reader = MatroskaReader::new(Cursor::new(buffer.contents())).expect("open");
        assert_eq!(reader.tracks(), []);
        assert_eq!(
            crate::sniff(&buffer.contents()).ok(),
            Some(crate::ContainerFormat::Matroska)
        );
    }
}

#[test]
fn written_files_open_through_the_sniffer() {
    let (tracks, read) = demux(&corpus_dir().join("matroska-laced.mkv"));
    let written = mux(&tracks, &read, MatroskaFlavor::Matroska);
    let mut reader = crate::open_source(Box::new(Cursor::new(written))).expect("open");
    assert_eq!(reader.tracks().len(), tracks.len());
    assert_eq!(packets(reader.as_mut(), TrackId(2)), read[1]);
}

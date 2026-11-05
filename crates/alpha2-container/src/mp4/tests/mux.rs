//! The muxer: corpus files demuxed, written again and read back, against the packets they came
//! from and against `FFmpeg`'s reading of its own stream copy.

use std::fmt::Write as _;
use std::io::Cursor;
use std::path::Path;
use std::process::Command;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::corpus::{Scope, compare_reader};
use super::{Shared, corpus_dir, expected, packets};
use crate::{ContainerReader, ContainerWriter, Mp4Flavor, Mp4Reader, Mp4Writer, Packet, TrackInfo};

fn flavor_of(name: &str) -> Mp4Flavor {
    if Path::new(name)
        .extension()
        .is_some_and(|extension| extension == "mov")
    {
        Mp4Flavor::Mov
    } else {
        Mp4Flavor::Mp4
    }
}

/// Every track and packet of a file.
fn demux(path: &Path) -> (Vec<TrackInfo>, Vec<Vec<Packet>>) {
    let mut reader = Mp4Reader::open(path).expect("open");
    let tracks = reader.tracks().to_vec();
    let read = tracks
        .iter()
        .map(|track| packets(&mut reader, track.id))
        .collect();
    (tracks, read)
}

/// Writes `tracks` and their packets, handing packets over in decode-time order across tracks.
fn mux(tracks: &[TrackInfo], read: &[Vec<Packet>], flavor: Mp4Flavor) -> Vec<u8> {
    let buffer = Shared::default();
    let mut writer = Box::new(Mp4Writer::new(buffer.clone(), flavor));
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

fn remuxable(expected: &Value) -> Vec<(String, Value)> {
    expected["files"]
        .as_object()
        .expect("files")
        .iter()
        .filter_map(|(name, file)| file.get("remux").map(|remux| (name.clone(), remux.clone())))
        .collect()
}

#[test]
fn corpus_files_round_trip_through_the_muxer() {
    let expected = expected();
    let files = remuxable(&expected);
    assert!(files.len() >= 50, "only {} remuxable files", files.len());
    let mut failures = Vec::new();
    for (name, remux) in &files {
        let (tracks, read) = demux(&corpus_dir().join(name));
        let flavor = flavor_of(name);
        let written = mux(&tracks, &read, flavor);
        if mux(&tracks, &read, flavor) != written {
            failures.push(format!("{name}: two runs write different bytes"));
        }
        let mut reader = Mp4Reader::new(Cursor::new(written)).expect("read back");
        compare_reader(&mut reader, name, remux, Scope::Remux, &mut failures);
        let mut reader =
            Mp4Reader::new(Cursor::new(mux(&tracks, &read, flavor))).expect("read back");
        let again = reader.tracks().to_vec();
        for ((before, after), packets_before) in tracks.iter().zip(&again).zip(&read) {
            if after.timescale != before.timescale {
                // The muxer doubles video timescales below 10000, as FFmpeg does.
                continue;
            }
            let mut facts_before = before.clone();
            let mut facts_after = after.clone();
            for facts in [&mut facts_before, &mut facts_after] {
                facts.id = crate::TrackId(0);
                facts.duration = None;
                facts.title = None;
                facts.language = facts.language.take().filter(|code| code != "und");
            }
            if facts_before != facts_after {
                failures.push(format!(
                    "{name} track {}: facts change\n    before {facts_before:?}\n    after  {facts_after:?}",
                    before.id.0
                ));
            }
            let packets_after = packets(&mut reader, after.id);
            let same = packets_before.len() == packets_after.len()
                && packets_before.iter().zip(&packets_after).all(|(a, b)| {
                    (a.pts, a.dts, a.duration, a.keyframe, &a.data, a.discard)
                        == (b.pts, b.dts, b.duration, b.keyframe, &b.data, b.discard)
                });
            if !same {
                let first = packets_before
                    .iter()
                    .zip(&packets_after)
                    .position(|(a, b)| {
                        (a.pts, a.dts, a.duration, a.keyframe, a.discard)
                            != (b.pts, b.dts, b.duration, b.keyframe, b.discard)
                    });
                failures.push(format!(
                    "{name} track {}: packets change ({} before, {} after, first difference at {first:?}: {:?} / {:?})",
                    before.id.0,
                    packets_before.len(),
                    packets_after.len(),
                    first.map(|at| (packets_before[at].pts, packets_before[at].dts, packets_before[at].duration, packets_before[at].discard)),
                    first.map(|at| (packets_after[at].pts, packets_after[at].dts, packets_after[at].duration, packets_after[at].discard)),
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

/// `ffprobe`'s packet lines for each stream of `path`, hashed as the expectations are.
fn ffprobe_packets(path: &Path) -> Option<Vec<(u64, usize, String)>> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_streams",
            "-show_packets",
            "-show_data_hash",
            "SHA256",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .ok()?;
    let report: Value = serde_json::from_slice(&output.stdout).ok()?;
    let streams = report["streams"].as_array()?;
    let packets = report["packets"].as_array()?;
    Some(
        streams
            .iter()
            .map(|stream| {
                let index = stream["index"].as_u64().unwrap_or(0);
                let mut digest = Sha256::new();
                let mut count = 0;
                for packet in packets
                    .iter()
                    .filter(|packet| packet["stream_index"].as_u64() == Some(index))
                {
                    let (mut skip, mut padding) = (0, 0);
                    for side in packet["side_data_list"].as_array().into_iter().flatten() {
                        if side["side_data_type"] == "Skip Samples" {
                            skip = side["skip_samples"].as_i64().unwrap_or(0);
                            padding = side["discard_padding"].as_i64().unwrap_or(0);
                        }
                    }
                    let hash = packet["data_hash"]
                        .as_str()
                        .unwrap_or_default()
                        .split_once(':')
                        .map_or("", |(_, hex)| hex);
                    let key = u8::from(
                        packet["flags"]
                            .as_str()
                            .unwrap_or_default()
                            .starts_with('K'),
                    );
                    let line = format!(
                        "{},{},{},{key},{},{hash},{skip},{padding}\n",
                        packet["pts"],
                        packet["dts"],
                        packet["duration"],
                        packet["size"].as_str().unwrap_or_default()
                    );
                    digest.update(line.as_bytes());
                    count += 1;
                }
                let hex = digest
                    .finalize()
                    .iter()
                    .fold(String::new(), |mut hex, byte| {
                        let _ = write!(hex, "{byte:02x}");
                        hex
                    });
                (
                    stream["id"]
                        .as_str()
                        .and_then(|id| u64::from_str_radix(id.trim_start_matches("0x"), 16).ok())
                        .unwrap_or(0),
                    count,
                    hex,
                )
            })
            .collect(),
    )
}

#[test]
fn ffprobe_reads_written_files_as_it_reads_ffmpeg_stream_copies() {
    if Command::new("ffprobe").arg("-version").output().is_err() {
        eprintln!("skipped: ffprobe is not on PATH");
        return;
    }
    let expected = expected();
    let scratch = std::env::temp_dir().join(format!("alpha2-container-mux-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).expect("scratch directory");
    let mut failures = Vec::new();
    for (name, remux) in remuxable(&expected) {
        let (tracks, read) = demux(&corpus_dir().join(&name));
        let target = scratch.join(format!("written-{name}"));
        std::fs::write(&target, mux(&tracks, &read, flavor_of(&name))).expect("write file");
        let Some(streams) = ffprobe_packets(&target) else {
            failures.push(format!("{name}: ffprobe cannot read the written file"));
            continue;
        };
        for theirs in remux["tracks"].as_array().into_iter().flatten() {
            let id = theirs["id"].as_u64().unwrap_or(0);
            let Some((_, count, hash)) = streams.iter().find(|(stream, _, _)| *stream == id) else {
                failures.push(format!("{name}: ffprobe sees no track {id}"));
                continue;
            };
            if Some(*count as u64) != theirs["packet_count"].as_u64()
                || Some(hash.as_str()) != theirs["packets_sha256"].as_str()
            {
                failures.push(format!(
                    "{name} track {id}: ffprobe reads other packets than from FFmpeg's copy"
                ));
            }
        }
        let _ = std::fs::remove_file(&target);
    }
    let _ = std::fs::remove_dir_all(&scratch);
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

use std::io::Cursor;

use crate::mp4::tests::{corpus_dir, packets};
use crate::{ContainerReader, DemuxError, MatroskaReader, Packet};

/// Opens `data` and reads and seeks every track; damaged data may fail but must not panic.
fn exercise(data: Vec<u8>) -> Result<usize, DemuxError> {
    let mut reader = MatroskaReader::new(Cursor::new(data))?;
    let mut count = 0;
    for track in reader.tracks().to_vec() {
        while reader.next_packet(track.id)?.is_some() {
            count += 1;
        }
        for target in [i64::MIN, 0, 500, i64::MAX] {
            reader.seek(track.id, target)?;
            reader.next_packet(track.id)?;
        }
    }
    Ok(count)
}

#[test]
fn truncated_files_read_up_to_where_they_end() {
    for name in [
        "matroska-laced.mkv",
        "matroska-groups.mkv",
        "subtitle-first.mkv",
    ] {
        let data = std::fs::read(corpus_dir().join(name)).expect("read");
        let whole = exercise(data.clone()).expect("whole file");
        let mut previous = 0;
        for length in (0..data.len()).step_by(97) {
            if let Ok(count) = exercise(data[..length].to_vec()) {
                assert!(count <= whole, "{name} cut at {length}");
                previous = previous.max(count);
            }
        }
        assert!(previous > 0, "{name}: no truncation read a packet");
    }
}

#[test]
fn corrupted_bytes_never_panic() {
    let data = std::fs::read(corpus_dir().join("matroska-laced.mkv")).expect("read");
    let mut state = 0x2545_f491_u64;
    for _ in 0..300 {
        let mut damaged = data.clone();
        for _ in 0..8 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let at = usize::try_from(state % damaged.len() as u64).unwrap_or(0);
            damaged[at] = state.to_le_bytes()[4];
        }
        let _ = exercise(damaged);
    }
}

#[test]
fn hostile_sizes_are_refused_without_allocating() {
    let data = std::fs::read(corpus_dir().join("audio-pts-ahead.mkv")).expect("read");
    // The Info element's size field becomes 2^48 bytes.
    let info = data
        .windows(4)
        .position(|bytes| bytes == [0x15, 0x49, 0xa9, 0x66])
        .expect("Info");
    let mut huge = data[..info + 4].to_vec();
    huge.extend_from_slice(&[0x01, 0x00, 0x01, 0, 0, 0, 0, 0]);
    huge.extend_from_slice(&data[info + 5..]);
    assert!(matches!(
        MatroskaReader::new(Cursor::new(huge)),
        Err(DemuxError::Invalid(_))
    ));
    let mut not_matroska = data.clone();
    not_matroska[0] = 0;
    assert!(MatroskaReader::new(Cursor::new(not_matroska)).is_err());
    assert!(MatroskaReader::new(Cursor::new(Vec::new())).is_err());
}

fn from_hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("hex"))
        .collect()
}

/// The EBML header, an unknown-sized Segment and a Tracks element of `tracks` bytes (hex) that
/// holds one entry, followed by `rest`.
fn file(tracks: &str, rest: &str) -> Vec<u8> {
    from_hex(&format!(
        "1a45dfa38b4282886d6174726f736b611853806701ffffffffffffff1654ae6b{tracks}{rest}"
    ))
}

/// A track entry for 16-bit stereo PCM at `rate` (a big-endian `f64` in hex).
fn pcm_track(rate: &str) -> String {
    format!("aaaea8d78101838102868d415f50434d2f494e542f4c4954e191b588{rate}9f810262648110")
}

const RATE_48K: &str = "40e7700000000000";

fn only_packets(data: Vec<u8>) -> Vec<Packet> {
    let mut reader = MatroskaReader::new(Cursor::new(data)).expect("opens");
    let track = reader.tracks()[0].id;
    packets(&mut reader, track)
}

#[test]
fn a_block_whose_header_crosses_its_cluster_end_ends_the_cluster() {
    // The cluster's last payload byte is the ID of a SimpleBlock or BlockGroup whose size byte
    // lies after the cluster.
    for element in ["a3", "a0"] {
        let data = file(
            &pcm_track(RATE_48K),
            &format!("1f43b67584e78100{element}8100"),
        );
        assert_eq!(exercise(data).expect("reads"), 0);
    }
}

#[test]
fn a_dts_frame_shorter_than_its_header_has_no_duration() {
    let dts = |frame: &str| {
        let tracks = "a2aea0d781018381028685415f445453e191b58840e77000000000009f810262648110";
        let bytes = frame.len() / 2;
        let rest = format!(
            "1f43b675{:02x}e78100a3{:02x}81000080{frame}",
            0x89 + bytes,
            0x84 + bytes
        );
        file(tracks, &rest)
    };
    for short in ["7ffe8001", "7ffe8001ff"] {
        let packets = only_packets(dts(short));
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].duration, 0);
    }
    let packets = only_packets(dts("7ffe8001ffff"));
    assert_eq!(packets.len(), 1);
    assert!(packets[0].duration > 0);
}

#[test]
fn an_avc_sequence_with_the_largest_dimensions_opens() {
    // Both dimensions code 2^32 - 1 macroblocks, in field pictures.
    let tracks = "cbaec9d78101838101868f565f4d504547342f49534f2f41564363a2a70142001effe1001867420\
                  01edc0000030003fffffffc0000030007fffffff82001000468ce3c80e086b08140ba8130";
    let reader = MatroskaReader::new(Cursor::new(file(tracks, ""))).expect("opens");
    assert_eq!(reader.tracks().len(), 1);
}

#[test]
fn the_most_negative_discard_padding_reads_at_any_rate() {
    for (rate, discard) in [("41cdcd6500000000", u32::MAX), (RATE_48K, u32::MAX)] {
        let rest = "1f43b6759ae78100a095a188810000000102030475a2888000000000000000";
        let packets = only_packets(file(&pcm_track(rate), rest));
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].discard, (discard, 0));
    }
}

#[test]
fn hostile_matroska_inputs_read_and_seek_without_a_panic() {
    for (_, input) in crate::mp4::tests::hostile_inputs("container_matroska") {
        // The seek target, then the file.
        let target = i64::from_le_bytes(input[..8].try_into().expect("8 bytes"));
        let Ok(mut reader) = MatroskaReader::new(Cursor::new(input[8..].to_vec())) else {
            continue;
        };
        for track in reader.tracks().to_vec() {
            while let Ok(Some(_)) = reader.next_packet(track.id) {}
            if reader.seek(track.id, target).is_ok() {
                let _ = reader.next_packet(track.id);
            }
        }
    }
}

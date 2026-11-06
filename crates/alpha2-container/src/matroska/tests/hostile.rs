use std::io::Cursor;

use crate::mp4::tests::corpus_dir;
use crate::{ContainerReader, DemuxError, MatroskaReader};

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

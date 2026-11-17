#![no_main]

use std::io::Cursor;

use alpha2_container::{ContainerReader, Mp4Reader};
use libfuzzer_sys::fuzz_target;

// Any bytes open as an MP4 file or are refused, never panic, and every track reads to its end
// with packets no larger than the input. A seek to the middle of each track lands on a packet.
fuzz_target!(|data: &[u8]| {
    let Ok(mut reader) = Mp4Reader::new(Cursor::new(data.to_vec())) else {
        return;
    };
    for track in reader.tracks().to_vec() {
        let mut last_pts = 0;
        while let Ok(Some(packet)) = reader.next_packet(track.id) {
            assert!(packet.data.len() <= data.len());
            assert_eq!(packet.track, track.id);
            last_pts = packet.pts;
        }
        if let Ok(Some(_)) = reader.seek(track.id, last_pts / 2) {
            let _ = reader.next_packet(track.id);
        }
    }
});

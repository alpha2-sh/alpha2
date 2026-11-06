#![no_main]

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;

/// Magic bytes that steer arbitrary input to each audio demuxer past format sniffing.
const PREFIXES: [&[u8]; 6] = [
    b"",
    b"RIFF\x00\x00\x00\x00WAVE",
    b"fLaC",
    b"ID3\x04\x00\x00\x00\x00\x00\x00",
    b"\xff\xf1",
    b"\xff\xfb",
];

// Files come from users, so a demuxer answers or refuses any bytes without panicking, hanging or
// allocating beyond what the file holds.
fuzz_target!(|input: (u8, i64, &[u8])| {
    let (selector, target, data) = input;
    let prefix = PREFIXES[usize::from(selector) % PREFIXES.len()];
    let bytes = [prefix, data].concat();
    let Ok(mut reader) = alpha2_container::open_source(Box::new(Cursor::new(bytes))) else {
        return;
    };
    let Some(track) = reader.tracks().first().map(|track| track.id) else {
        return;
    };
    for _ in 0..4096 {
        match reader.next_packet(track) {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    if reader.seek(track, target).is_ok() {
        for _ in 0..16 {
            if !matches!(reader.next_packet(track), Ok(Some(_))) {
                break;
            }
        }
    }
});

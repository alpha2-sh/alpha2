//! Talks to `alpha2-host serve` in protocol frames.

use alpha2_host_core::host_frames::{
    FrameLimits, ReadFrame, frame_bytes, read_frame, skip_to_magic,
};

/// One request frame.
pub fn request(header: &str) -> Vec<u8> {
    frame_bytes(header.as_bytes(), &[]).expect("a request frame")
}

/// The headers `serve` wrote, after its magic.
pub fn messages(mut stdout: &[u8]) -> Vec<serde_json::Value> {
    assert!(
        matches!(skip_to_magic(&mut stdout, usize::MAX), Ok(Some(_))),
        "no magic in {}",
        String::from_utf8_lossy(stdout)
    );
    let limits = FrameLimits {
        header_bytes: u32::MAX,
        attachment_bytes: u32::MAX,
    };
    let mut messages = Vec::new();
    while let Ok(ReadFrame::Frame(frame)) = read_frame(&mut stdout, limits) {
        messages.push(serde_json::from_slice(&frame.header).expect("a header is JSON"));
    }
    messages
}

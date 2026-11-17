#![no_main]

//! Host protocol frames through `read_frame` and `split_attachments`, as `serve` reads requests.

use alpha2_host_core::host_frames::{FrameLimits, ReadFrame, read_frame, split_attachments};
use libfuzzer_sys::fuzz_target;

const LIMITS: FrameLimits = FrameLimits {
    header_bytes: 1 << 16,
    attachment_bytes: 1 << 16,
};

fuzz_target!(|bytes: &[u8]| {
    let mut input = bytes;
    loop {
        let before = input.len();
        match read_frame(&mut input, LIMITS).expect("a slice never fails to read") {
            ReadFrame::End => break,
            ReadFrame::TooLarge { .. } => {}
            ReadFrame::Frame(frame) => {
                let carried = frame.attachments.len();
                if let Ok(header) = serde_json::from_slice::<serde_json::Value>(&frame.header)
                    && let Ok(attachments) = split_attachments(&header, frame.attachments)
                {
                    let total: usize = attachments.iter().map(|part| part.bytes.len()).sum();
                    assert_eq!(total, carried);
                }
            }
        }
        assert!(input.len() < before, "every frame consumes its preamble");
    }
});

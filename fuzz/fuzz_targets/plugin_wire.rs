#![no_main]

use alpha2_host_core::plugins::wire::{
    ChildMessage, MAX_CHILD_FRAME_BYTES, parse_child, read_frame, skip_to_magic,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut reader = data;
    if skip_to_magic(&mut reader).is_err() {
        return;
    }
    while let Ok(frame) = read_frame(&mut reader, MAX_CHILD_FRAME_BYTES) {
        assert!(frame.payload.len() <= MAX_CHILD_FRAME_BYTES);
        let tag = frame.tag;
        if let Ok(message) = parse_child(frame) {
            let ends = matches!(
                message,
                ChildMessage::Loaded { .. }
                    | ChildMessage::LoadFailed { .. }
                    | ChildMessage::Described { .. }
                    | ChildMessage::Returned { .. }
            );
            assert_eq!(message.sequence().is_some(), ends, "tag {tag}");
        }
    }
});

#![no_main]

//! PNG, JPEG and WebP stills through `probe_still` and `decode_still`.

use alpha2_host_core::{decode_still, probe_still};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let probed = probe_still(bytes);
    if let Ok(decoded) = decode_still(bytes) {
        let info = decoded.info;
        assert_eq!(probed.ok(), Some(info));
        assert_eq!(
            decoded.rgba.len(),
            info.width as usize * info.height as usize * 4
        );
        let frame = decoded.into_video_frame();
        assert_eq!(
            frame.planes[0].bytes.len(),
            frame.planes[0].stride as usize * info.height as usize
        );
    }
});

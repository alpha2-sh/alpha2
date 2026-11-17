//! Runs alone in its process: it masks every SIMD flag, so swscale picks the C converters arm64
//! uses for widths off the 16-pixel grid.

use alpha2_ffmpeg_bridge::PackedScaler;
use rsmpeg::{avutil::AVFrame, ffi};

fn opaque_gray_frame(width: i32, height: i32) -> AVFrame {
    let mut frame = AVFrame::new();
    frame.set_format(ffi::AV_PIX_FMT_YUVA420P);
    frame.set_width(width);
    frame.set_height(height);
    frame.get_buffer(0).expect("frame");
    let chroma_rows = (height + 1) / 2;
    for (plane, value, rows) in [
        (0, 126, height),
        (1, 128, chroma_rows),
        (2, 128, chroma_rows),
        (3, 255, height),
    ] {
        let bytes = usize::try_from(frame.linesize[plane] * rows).expect("plane size");
        // SAFETY: the plane holds `rows` rows at its linesize inside the frame's buffer.
        unsafe { std::ptr::write_bytes(frame.data[plane], value, bytes) };
    }
    frame
}

#[test]
fn every_column_is_written_by_the_c_converters() {
    // SAFETY: plain values; this binary holds only this test, so no other scaler is running.
    unsafe { ffi::av_force_cpu_flags(0) };
    let bt709 = i32::try_from(ffi::SWS_CS_ITU709).expect("coefficient");
    for width in (1..=40).chain([251, 854]) {
        for height in [2, 16] {
            let mut scaler = PackedScaler::new(
                width,
                height,
                ffi::AV_PIX_FMT_YUVA420P,
                ffi::AV_PIX_FMT_RGBA,
            )
            .expect("scaler");
            scaler
                .configure_colorspace(bt709, false, bt709, true)
                .expect("colorspace");
            let bytes = scaler
                .convert(&opaque_gray_frame(width, height))
                .expect("convert");
            let w = usize::try_from(width).expect("width");
            assert_eq!(
                bytes.len(),
                4 * w * usize::try_from(height).expect("height")
            );
            for (index, pixel) in bytes.as_chunks::<4>().0.iter().enumerate() {
                assert_eq!(pixel[3], 255, "{width}x{height} column {}", index % w);
            }
        }
    }
}

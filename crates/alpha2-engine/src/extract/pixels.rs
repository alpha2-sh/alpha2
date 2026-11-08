//! The delivered frame size and the `luma8` conversion, computed alike on every surface.

/// The size a frame of displayed size `(width, height)` is delivered at for a request `size`:
/// contain-fit so the long edge is at most `size`, never upscaled, each edge rounded half up and
/// at least one pixel.
#[must_use]
pub fn delivered_size(displayed: (u32, u32), size: u32) -> (u32, u32) {
    let (width, height) = displayed;
    let long = width.max(height);
    if long == 0 || long <= size {
        return (width.max(1), height.max(1));
    }
    // floor(edge * size / long + 1/2) in integers: (2 * edge * size + long) / (2 * long).
    let edge = |value: u32| {
        let scaled =
            (2 * u64::from(value) * u64::from(size) + u64::from(long)) / (2 * u64::from(long));
        u32::try_from(scaled).unwrap_or(u32::MAX).max(1)
    };
    (edge(width), edge(height))
}

/// BT.709 Y' of straight-alpha sRGB-encoded RGBA8 pixels, rounded half up; alpha is ignored.
#[must_use]
pub fn luma8_from_rgba8(rgba: &[u8]) -> Vec<u8> {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .map(|pixel| {
            let weighted = 2_126 * u32::from(pixel[0])
                + 7_152 * u32::from(pixel[1])
                + 722 * u32::from(pixel[2]);
            u8::try_from((weighted + 5_000) / 10_000).unwrap_or(u8::MAX)
        })
        .collect()
}

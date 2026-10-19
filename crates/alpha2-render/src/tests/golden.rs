//! The shared PNG goldens (`alpha2-golden-support`), taking a render size.

use crate::RenderSize;

pub(crate) fn assert_rgba8_golden(name: &str, size: RenderSize, actual: &[u8], tolerance: u8) {
    alpha2_golden_support::assert_rgba8_golden(name, (size.width, size.height), actual, tolerance);
}

pub(crate) fn assert_rgba8_golden_tiles(
    names: &[&str],
    size: RenderSize,
    tiles: &[&str],
    actual: &[u8],
    tolerance: u8,
) {
    alpha2_golden_support::assert_rgba8_golden_tiles(
        names,
        (size.width, size.height),
        tiles,
        actual,
        tolerance,
    );
}

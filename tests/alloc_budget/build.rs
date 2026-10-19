//! Adds the linked `FFmpeg`'s rpath to this package's tests on macOS. They
//! reach `FFmpeg` through `alpha2-native`, whose own rpath stays with its
//! targets and does not carry over to a dependent's.

fn main() {
    alpha2_build_support::emit_ffmpeg_dev_rpath();
}

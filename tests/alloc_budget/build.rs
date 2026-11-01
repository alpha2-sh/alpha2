//! Adds the `FFmpeg` rpath on macOS; `alpha2-native`'s rpath does not carry over to dependents.

fn main() {
    alpha2_build_support::emit_ffmpeg_dev_rpath();
}

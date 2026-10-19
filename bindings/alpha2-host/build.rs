//! Adds the linked `FFmpeg`'s rpath to this package's executables on macOS.

fn main() {
    alpha2_build_support::emit_ffmpeg_dev_rpath();
}

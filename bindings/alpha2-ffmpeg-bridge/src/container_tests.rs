use super::*;

fn corpus(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/corpus")
        .join(name)
}

/// A path reaches `FFmpeg` as `file:` and the absolute path, so a relative
/// name with a colon is not taken for a protocol.
#[test]
fn a_file_url_is_absolute_and_prefixed() {
    let url = file_url(Path::new("take:1.mov")).expect("a UTF-8 path");
    let text = url.to_str().expect("UTF-8");
    let path = text.strip_prefix("file:").expect("the file protocol");
    assert!(Path::new(path).is_absolute(), "{text}");
    assert!(path.ends_with("take:1.mov"), "{text}");
    assert_eq!(file_url(Path::new("a\0b")).map(|_| ()), Err(PathError::Nul));
}

/// The callback reaches into `FFmpeg`: a read that has to go back to
/// the file stops with `AVERROR_EXIT` while the check is true, and reads
/// again once it is not.
#[test]
fn an_interrupt_stops_the_call_that_waits_on_input() {
    let backward = i32::try_from(ffi::AVSEEK_FLAG_BACKWARD).expect("flag fits");
    let mut input =
        open_input(&corpus("h264-bt709-copy.mp4"), c"mov").expect("an MP4 opens as mov");
    input.interrupt_when(|| true);
    // From the first sample on, reading the file through outgrows the I/O buffer, and the read
    // that refills it is stopped.
    input.seek(-1, 0, backward).expect("seeking reads nothing");
    let stopped = std::iter::from_fn(|| match input.read_packet() {
        Ok(Some(_)) => Some(None),
        Ok(None) => None,
        Err(error) => Some(Some(error)),
    })
    .find_map(|error| error);
    assert_eq!(
        stopped,
        Some(RsmpegError::AVError(ffi::AVERROR_EXIT)),
        "no read that went back to the file was stopped"
    );
    input.interrupt_when(|| false);
    input.seek(-1, 0, backward).expect("seek");
    assert!(input.read_packet().expect("a packet").is_some());
}

/// A JPEG that opens with an Exif segment is `image2` input to an open
/// that probes its name, the demuxer for numbered image files; its bytes
/// are the JPEG demuxer's.
#[test]
fn a_still_opens_with_the_demuxer_its_bytes_name() {
    let input = open_input(&corpus("exif-orientation-6.jpg"), c"png_pipe,jpeg_pipe")
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(input.iformat().name(), c"jpeg_pipe");
    let refused = open_input(&corpus("exif-orientation-6.jpg"), c"png_pipe")
        .err()
        .expect("a JPEG is not a PNG");
    assert!(
        matches!(&refused, OpenInputError::Refused(name) if name == "jpeg_pipe"),
        "{refused}"
    );
}

#[test]
fn a_container_the_policy_leaves_out_is_refused_by_name() {
    let refused = open_input(&corpus("h264-long-gop.mp4"), c"matroska")
        .err()
        .expect("an MP4 is not Matroska");
    assert!(
        matches!(&refused, OpenInputError::Refused(name) if name.starts_with("mov,")),
        "{refused}"
    );
}

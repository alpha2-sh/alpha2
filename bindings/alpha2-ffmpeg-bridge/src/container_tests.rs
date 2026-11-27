use super::*;

use alpha2_test_support::corpus;

/// A relative name with a colon is not taken for a protocol.
#[test]
fn a_file_url_is_absolute_and_prefixed() {
    let url = file_url(Path::new("take:1.mov")).expect("a UTF-8 path");
    let text = url.to_str().expect("UTF-8");
    let path = text.strip_prefix("file:").expect("the file protocol");
    assert!(Path::new(path).is_absolute(), "{text}");
    assert!(path.ends_with("take:1.mov"), "{text}");
    assert_eq!(file_url(Path::new("a\0b")).map(|_| ()), Err(PathError::Nul));
}

/// A read that hits the file stops with `AVERROR_EXIT` while the check is true.
#[test]
fn an_interrupt_stops_the_call_that_waits_on_input() {
    let backward = i32::try_from(ffi::AVSEEK_FLAG_BACKWARD).expect("flag fits");
    let mut input = open_input(
        InputLocation::File(&corpus("h264-bt709-copy.mp4")),
        c"mov",
        None,
    )
    .expect("an MP4 opens as mov");
    input.interrupt_when(|| true);
    // Reading through the file outgrows the I/O buffer, so a refill gets interrupted.
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

/// A JPEG with an Exif segment opens with the JPEG demuxer, not `image2`.
#[test]
fn a_still_opens_with_the_demuxer_its_bytes_name() {
    let input = open_input(
        InputLocation::File(&corpus("exif-orientation-6.jpg")),
        c"png_pipe,jpeg_pipe",
        None,
    )
    .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(input.iformat().name(), c"jpeg_pipe");
    let refused = open_input(
        InputLocation::File(&corpus("exif-orientation-6.jpg")),
        c"png_pipe",
        None,
    )
    .err()
    .expect("a JPEG is not a PNG");
    assert!(
        matches!(&refused, OpenInputError::Refused(name) if name == "jpeg_pipe"),
        "{refused}"
    );
}

#[test]
fn a_container_the_policy_leaves_out_is_refused_by_name() {
    let refused = open_input(
        InputLocation::File(&corpus("h264-long-gop.mp4")),
        c"matroska",
        None,
    )
    .err()
    .expect("an MP4 is not Matroska");
    assert!(
        matches!(&refused, OpenInputError::Refused(name) if name.starts_with("mov,")),
        "{refused}"
    );
}

/// The source containers alpha2 demuxes.
const CONTAINERS: &CStr =
    c"mov,matroska,mpegts,avi,mp3,aac,wav,flac,ogg,png_pipe,jpeg_pipe,webp_pipe,gif";

fn custom<'a>(
    path: &'a Path,
    name: &'a str,
    open: &'a dyn Fn() -> io::Result<Box<dyn InputReader>>,
) -> CustomInput<'a> {
    CustomInput {
        name,
        length: std::fs::metadata(path).expect("metadata").len(),
        open,
    }
}

/// Each packet's stream, timestamps, flags and bytes, then the first packet after seeks to a
/// quarter and to the middle of the input's duration.
fn packets_and_landings(input: &mut DemuxInput) -> Vec<(i32, i64, i64, i32, Vec<u8>)> {
    let mut seen = Vec::new();
    let mut record = |packet: &AVPacket| {
        seen.push((
            packet.stream_index,
            packet.pts,
            packet.dts,
            packet.flags,
            // SAFETY: a read packet's `data` holds `size` bytes until the packet drops.
            unsafe {
                std::slice::from_raw_parts(packet.data, usize::try_from(packet.size).unwrap_or(0))
            }
            .to_vec(),
        ));
    };
    while let Some(packet) = input.read_packet().expect("a packet or the end") {
        record(&packet);
    }
    let duration = input.duration.max(0);
    let backward = i32::try_from(ffi::AVSEEK_FLAG_BACKWARD).expect("flag fits");
    for target in [duration / 4, duration / 2] {
        if input.seek(-1, target, backward).is_ok()
            && let Ok(Some(packet)) = input.read_packet()
        {
            record(&packet);
        }
    }
    seen
}

#[test]
fn a_custom_input_reads_the_same_packets_as_the_file() {
    let directory = alpha2_test_support::tests_dir().join("corpus");
    let mut compared = 0;
    let mut entries = std::fs::read_dir(&directory)
        .expect("the corpus")
        .map(|entry| entry.expect("an entry").path())
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    entries.sort();
    for path in entries {
        let Ok(mut file) = open_input(InputLocation::File(&path), CONTAINERS, None) else {
            continue;
        };
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("a name");
        let opened = || -> io::Result<Box<dyn InputReader>> {
            Ok(Box::new(std::io::BufReader::new(File::open(&path)?)))
        };
        let source = custom(&path, name, &opened);
        let mut read = open_input(InputLocation::Custom(&source), CONTAINERS, None)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(
            read.iformat().name(),
            file.iformat().name(),
            "{}",
            path.display()
        );
        assert_eq!(read.nb_streams, file.nb_streams, "{}", path.display());
        assert_eq!(read.duration, file.duration, "{}", path.display());
        assert!(
            packets_and_landings(&mut read) == packets_and_landings(&mut file),
            "{}: the packets or seek landings differ",
            path.display()
        );
        compared += 1;
    }
    assert!(compared > 50, "only {compared} corpus files opened");
}

#[test]
fn a_custom_input_refuses_every_nested_open() {
    let path = corpus("h264-bt709-copy.mp4");
    let opened = || -> io::Result<Box<dyn InputReader>> { Ok(Box::new(File::open(&path)?)) };
    let source = custom(&path, "h264-bt709-copy.mp4", &opened);
    let mut input =
        open_input(InputLocation::Custom(&source), CONTAINERS, None).expect("the input opens");
    let context = input.as_ptr().cast_mut();
    for url in [
        c"file:/etc/hosts",
        c"h264-bt709-copy.mp4",
        c"https://example.invalid/a",
    ] {
        let mut io: *mut ffi::AVIOContext = ptr::null_mut();
        // SAFETY: the context is open and exclusively borrowed; `io_open` writes only `io`.
        let opened = unsafe {
            let open = (*context).io_open.expect("an io_open callback");
            open(context, &raw mut io, url.as_ptr(), 1, ptr::null_mut())
        };
        assert_eq!(opened, ffi::AVERROR(ffi::EPERM), "{url:?}");
        assert!(io.is_null());
    }
    assert!(input.read_packet().expect("a packet").is_some());
}

#[test]
fn a_failed_custom_read_is_kept_and_stops_the_call() {
    struct Failing(File, u64);
    impl Read for Failing {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let at = self.0.stream_position()?;
            if at >= self.1 {
                return Err(io::Error::other("the source went away"));
            }
            let length = buffer
                .len()
                .min(usize::try_from(self.1 - at).unwrap_or(usize::MAX));
            self.0.read(&mut buffer[..length])
        }
    }
    impl Seek for Failing {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.0.seek(position)
        }
    }
    let path = corpus("h264-long-gop.mp4");
    let opened =
        || -> io::Result<Box<dyn InputReader>> { Ok(Box::new(Failing(File::open(&path)?, 4096))) };
    let source = custom(&path, "h264-long-gop.mp4", &opened);
    let failed = open_input(InputLocation::Custom(&source), CONTAINERS, None)
        .err()
        .expect("a reader that fails fails the open");
    assert!(
        matches!(&failed, OpenInputError::Read(error) if error.to_string() == "the source went away"),
        "{failed}"
    );
}

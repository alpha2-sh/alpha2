use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use alpha2_host_core::remote::{
    MIN_CACHE_BYTES, RangeRequest, RangeResponse, RangeTransport, RemoteSession, ResponseHeaders,
    SourceLocation, TransportFailure,
};

use super::*;

struct Ranges {
    bytes: Vec<u8>,
    answered: AtomicUsize,
    changed: AtomicBool,
}

impl RangeTransport for Ranges {
    fn get(&self, request: &RangeRequest) -> Result<RangeResponse, TransportFailure> {
        self.answered.fetch_add(1, Ordering::AcqRel);
        let total = self.bytes.len() as u64;
        let last = request.last.min(total - 1);
        let tag = if self.changed.load(Ordering::Acquire) {
            "\"two\""
        } else {
            "\"one\""
        };
        let range = usize::try_from(request.first).expect("offset")
            ..=usize::try_from(last).expect("offset");
        Ok(RangeResponse {
            status: 206,
            headers: ResponseHeaders {
                content_range: Some(format!("bytes {}-{last}/{total}", request.first)),
                etag: Some(tag.into()),
                ..ResponseHeaders::default()
            },
            body: Box::new(std::io::Cursor::new(self.bytes[range].to_vec())),
        })
    }
}

fn remote(path: &Path) -> (Arc<Ranges>, RemoteSession, SourceLocation) {
    let transport = Arc::new(Ranges {
        bytes: fs::read(path).expect("the file"),
        answered: AtomicUsize::new(0),
        changed: AtomicBool::new(false),
    });
    let session = RemoteSession::new(
        Arc::clone(&transport) as Arc<dyn RangeTransport>,
        CancelToken::default(),
        MIN_CACHE_BYTES,
    );
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("a name");
    let object = session
        .open(&format!("https://media.example/{name}?signature=secret"))
        .expect("the probe");
    (transport, session, SourceLocation::Remote(object))
}

/// A WAV of `seconds` of stereo 16-bit PCM at 48 kHz, several blocks long.
fn long_wav(name: &str, seconds: u32) -> alpha2_test_support::ScratchDir {
    let directory = alpha2_test_support::scratch_dir(name);
    let frames = 48_000 * seconds;
    let data = frames * 4;
    let mut bytes = Vec::with_capacity(44 + data as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&48_000_u32.to_le_bytes());
    bytes.extend_from_slice(&(48_000_u32 * 4).to_le_bytes());
    bytes.extend_from_slice(&4_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data.to_le_bytes());
    for frame in 0..frames {
        let sample = i16::try_from(frame % 20_000).expect("fits").to_le_bytes();
        bytes.extend_from_slice(&sample);
        bytes.extend_from_slice(&sample);
    }
    fs::write(directory.join("long.wav"), bytes).expect("write");
    directory
}

type Blocks = Vec<(i64, Vec<Vec<f32>>)>;

fn audio_of(source: impl Into<SourceLocation>) -> Result<Blocks, DecodeError> {
    let cancel = CancelToken::default();
    let mut decoder = NativeAudioDecoder::open(source, 48_000)?;
    let mut blocks = Vec::new();
    loop {
        match decoder.decode_next(&cancel)? {
            DecodeEvent::Audio(block) => blocks.push((block.start_sample, block.channels)),
            DecodeEvent::EndOfStream => return Ok(blocks),
            _ => {}
        }
    }
}

fn frames_of(source: impl Into<SourceLocation>) -> Vec<(i64, Vec<u8>)> {
    let cancel = CancelToken::default();
    let mut decoder = NativeDecoder::open(source).expect("a decoder");
    let mut frames = Vec::new();
    loop {
        match decoder.decode_next(&cancel).expect("a frame") {
            DecodeEvent::Video(frame) => frames.push((
                frame.pts.ticks(),
                frame
                    .planes
                    .iter()
                    .flat_map(|plane| plane.bytes.iter().copied())
                    .collect(),
            )),
            DecodeEvent::EndOfStream => return frames,
            _ => {}
        }
    }
}

#[test]
fn a_remote_source_decodes_as_its_file_does() {
    crate::require_pinned_ffmpeg_for_tests();
    for name in ["h264-1080p.mp4", "vp9-alpha.webm", "h264-bt709-copy.mp4"] {
        let path = corpus(name);
        let (_transport, _session, location) = remote(&path);
        assert_eq!(
            format!("{:?}", probe(&location).expect("probe")),
            format!("{:?}", probe(&path).expect("probe")),
            "{name}"
        );
        assert!(
            frames_of(&location) == frames_of(&path),
            "{name}: the frames differ"
        );
    }
    let wav = long_wav("remote-decodes", 3);
    let path = wav.join("long.wav");
    let (_transport, session, location) = remote(&path);
    assert!(
        audio_of(&location).expect("remote audio") == audio_of(&path).expect("file audio"),
        "the audio differs"
    );
    assert!(
        session.counts().requests > 1,
        "read by range past the first block"
    );
}

#[test]
fn a_source_that_changes_while_it_is_read_fails_the_decode() {
    crate::require_pinned_ffmpeg_for_tests();
    let wav = long_wav("remote-changes", 60);
    let (transport, _session, location) = remote(&wav.join("long.wav"));
    let cancel = CancelToken::default();
    let mut decoder = NativeAudioDecoder::open(&location, 48_000).expect("the decoder opens");
    transport.changed.store(true, Ordering::Release);
    let failed = loop {
        match decoder.decode_next(&cancel) {
            Ok(DecodeEvent::EndOfStream) => panic!("the change was not seen"),
            Ok(_) => {}
            Err(error) => break error,
        }
    };
    let DecodeError::Source(failure) = failed else {
        panic!("not a source failure: {failed:?}");
    };
    assert_eq!(failure.code, "MEDIA_CHANGED");
    assert!(!failure.message.contains("secret"), "{}", failure.message);
    let (transport, _session, location) = remote(&wav.join("long.wav"));
    transport.changed.store(true, Ordering::Release);
    let refused = NativeAudioDecoder::open(&location, 48_000)
        .err()
        .expect("a change while probing fails the open");
    assert!(
        matches!(&refused, DecodeError::Source(failure) if failure.code == "MEDIA_CHANGED"),
        "{refused:?}"
    );
}

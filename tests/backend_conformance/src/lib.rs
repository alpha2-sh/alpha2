//! The decode behavior every native media backend must show on the fixture corpus.
//!
//! A backend implements [`Backend`] and instantiates the suite with [`conformance_tests!`], which
//! writes one test per case. A fixture whose container or codec the backend does not read must be
//! refused with [`DecodeError::Unsupported`]; the suite checks that refusal instead of skipping.
//!
//! The corpus is `tests/` of the repository, or `ALPHA2_TESTS_DIR` where a device runs the tests
//! from a copy.

// Cases are tests: a panic is a failure, and exact float equality pins decoded samples.
#![allow(
    clippy::missing_panics_doc,
    clippy::float_cmp,
    clippy::wildcard_imports
)]

use std::path::{Path, PathBuf};

use alpha2_media::{
    AudioBlock, CancelToken, ChromaLocation, ColorMatrix, ColorMetadata, ColorRange, DecodeError,
    DecodeEvent, DecodeRequest, DecodeRequestKind, DecodedVideoFrame, Decoder, Orientation,
    Rotation, SampleAspectRatio, SourceId, StreamInfo, StreamKind,
};
use alpha2_time::{FrameRate, MediaTime};

mod audio;
mod video;

pub use audio::*;
pub use video::*;

/// A video decoder under test.
pub trait VideoDecoder: Decoder {
    /// How far after a requested time a frame may be stored and still count as shown at it
    /// ([`MediaTime::presentation_deadline`]).
    fn presentation_slack(&self) -> MediaTime;
}

/// Which audio stream a backend opens when no stream is named.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DefaultAudioRule {
    /// `FFmpeg`'s `av_find_best_stream`: its own ranking (channels, disposition, bitrate).
    FfmpegBestStream,
    /// The first enabled audio track flagged default, else the first audio track, in container
    /// order.
    FirstDefaultFlagged,
}

/// A media backend under test.
pub trait Backend {
    /// Names the backend in failure messages.
    fn name(&self) -> &str;

    /// Opens the default video stream of `path`.
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] for a container or codec the backend does not read.
    fn open_video(&self, path: &Path) -> Result<Box<dyn VideoDecoder>, DecodeError>;

    /// Opens audio stream `stream` (an ordinal over the file's audio streams in container order;
    /// `None` is the default stream) of `path`, delivered at `sample_rate`.
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] for a container, codec or stream the backend does not read.
    fn open_audio(
        &self,
        path: &Path,
        sample_rate: u32,
        stream: Option<u32>,
    ) -> Result<Box<dyn Decoder>, DecodeError>;

    /// Why this backend refuses the fixture `name` (a file in the corpus), or `None` when it must
    /// decode it.
    fn refuses(&self, name: &str) -> Option<&'static str>;

    /// The rule behind [`Backend::open_audio`] with no stream.
    fn default_audio(&self) -> DefaultAudioRule;
}

/// One case's verdict: `Err` names what differed.
pub type Outcome = Result<(), String>;

/// The repository's `tests/`, or `ALPHA2_TESTS_DIR`.
#[must_use]
pub fn tests_dir() -> PathBuf {
    std::env::var_os("ALPHA2_TESTS_DIR").map_or_else(
        || {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("tests/")
                .to_owned()
        },
        PathBuf::from,
    )
}

/// A corpus file's path.
#[must_use]
pub fn corpus(name: &str) -> PathBuf {
    tests_dir().join("corpus").join(name)
}

/// Opens video from `name`, or checks the backend's refusal: `Ok(None)` when refused as it should
/// be.
///
/// # Errors
/// An open that failed although the backend reads the fixture, or that did not refuse as
/// unsupported.
pub fn video_or_refusal(
    backend: &dyn Backend,
    name: &str,
) -> Result<Option<Box<dyn VideoDecoder>>, String> {
    let opened = backend.open_video(&corpus(name));
    match (backend.refuses(name), opened) {
        (None, Ok(decoder)) => Ok(Some(decoder)),
        (None, Err(error)) => Err(format!("{}: {name}: {error}", backend.name())),
        (Some(_), Err(DecodeError::Unsupported(_))) => Ok(None),
        (Some(why), Err(error)) => Err(format!(
            "{}: {name} must be refused as unsupported ({why}), not {error}",
            backend.name()
        )),
        (Some(why), Ok(_)) => Err(format!(
            "{}: {name} opened though the backend says it refuses it ({why})",
            backend.name()
        )),
    }
}

/// [`video_or_refusal`] for audio.
///
/// # Errors
/// As [`video_or_refusal`].
pub fn audio_or_refusal(
    backend: &dyn Backend,
    name: &str,
    sample_rate: u32,
    stream: Option<u32>,
) -> Result<Option<Box<dyn Decoder>>, String> {
    let opened = backend.open_audio(&corpus(name), sample_rate, stream);
    match (backend.refuses(name), opened) {
        (None, Ok(decoder)) => Ok(Some(decoder)),
        (None, Err(error)) => Err(format!("{}: {name}: {error}", backend.name())),
        (Some(_), Err(DecodeError::Unsupported(_))) => Ok(None),
        (Some(why), Err(error)) => Err(format!(
            "{}: {name} must be refused as unsupported ({why}), not {error}",
            backend.name()
        )),
        (Some(why), Ok(_)) => Err(format!(
            "{}: {name} opened though the backend says it refuses it ({why})",
            backend.name()
        )),
    }
}

fn request(time: MediaTime, generation: u64) -> DecodeRequest {
    DecodeRequest {
        source: SourceId::new("conformance").expect("source ID"),
        time,
        kind: DecodeRequestKind::Seek,
        generation,
    }
}

/// Collects every failure of a case instead of stopping at the first.
#[derive(Default)]
struct Failures(Vec<String>);

impl Failures {
    fn check(&mut self, ok: bool, message: impl FnOnce() -> String) {
        if !ok {
            self.0.push(message());
        }
    }

    fn outcome(self) -> Outcome {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(self.0.join("\n"))
        }
    }
}

/// Writes one `#[test]` per conformance case for the backend `$backend` (an expression).
#[macro_export]
macro_rules! conformance_tests {
    ($backend:expr) => {
        $crate::conformance_tests!(@cases $backend;
            video_seek_shows_the_presented_frame_and_keeps_the_next,
            video_seek_to_a_frame_start_at_the_source_rate,
            video_seek_matches_a_linear_decode,
            video_corpus_decodes_with_its_metadata,
            video_still_shows_its_exif_orientation,
            video_mirroring_display_matrix,
            video_frame_color_is_the_streams,
            video_chroma_siting_is_the_streams,
            video_crop_shows_exactly_the_cropped_columns,
            video_pixel_aspect_is_the_streams,
            audio_starts_where_the_stream_starts,
            audio_surround_keeps_the_contract_speaker_order,
            audio_declared_layouts_reach_the_engine,
            audio_whole_source_meets_the_provider_contract,
            audio_seek_before_the_stream_keeps_its_initial_gap,
            audio_seek_matches_a_linear_decode,
            audio_seek_matches_a_linear_decode_in_every_container,
            audio_seek_near_a_raw_stream_start,
            audio_timestamp_gap_is_silence,
            audio_streams_open_by_ordinal,
            audio_default_stream_follows_the_backends_rule,
        );
    };
    (@cases $backend:expr; $($case:ident),* $(,)?) => {
        $(
            #[test]
            fn $case() {
                let backend = $backend;
                if let Err(failures) = $crate::$case(&backend) {
                    panic!("{failures}");
                }
            }
        )*
    };
}

fn first_frame(decoder: &mut dyn VideoDecoder) -> Result<DecodedVideoFrame, String> {
    let cancel = CancelToken::default();
    loop {
        match decoder
            .decode_next(&cancel)
            .map_err(|error| error.to_string())?
        {
            DecodeEvent::Video(frame) => return Ok(frame),
            DecodeEvent::EndOfStream => return Err("no video frame".into()),
            DecodeEvent::Retry | DecodeEvent::Audio(_) => {}
        }
    }
}

fn video_stream(decoder: &dyn VideoDecoder) -> Option<&StreamInfo> {
    decoder
        .streams()
        .iter()
        .find(|stream| stream.kind == StreamKind::Video)
}

const fn sar(numerator: u32, denominator: u32) -> SampleAspectRatio {
    SampleAspectRatio {
        num: numerator,
        den: denominator,
    }
}

fn level(value: f32) -> f32 {
    (value * 32_768.0).round() / 32_768.0
}

fn whole_audio(decoder: &mut dyn Decoder) -> Result<AudioBlock, String> {
    let cancel = CancelToken::default();
    let mut whole: Option<AudioBlock> = None;
    loop {
        match decoder
            .decode_next(&cancel)
            .map_err(|error| error.to_string())?
        {
            DecodeEvent::Audio(block) => match whole.as_mut() {
                None => whole = Some(block),
                Some(whole) => {
                    if whole.speakers != block.speakers {
                        return Err("a block's speakers changed".into());
                    }
                    for (plane, more) in whole.channels.iter_mut().zip(block.channels) {
                        plane.extend(more);
                    }
                }
            },
            DecodeEvent::EndOfStream => break,
            event => return Err(format!("unexpected {event:?}")),
        }
    }
    whole.ok_or_else(|| "no audio".into())
}

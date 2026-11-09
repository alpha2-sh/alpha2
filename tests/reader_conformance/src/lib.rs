//! What the extraction reader delivers from every native media backend on the fixture corpus.
//!
//! A backend instantiates the cases with [`reader_conformance_tests!`] over its
//! [`alpha2_backend_conformance::Backend`]. Frames are presented on the CPU renderer, so the
//! delivered bytes are the same on every machine and pinned in `tests/golden/extract-recipe.json`.

#![allow(clippy::missing_panics_doc)]

use std::sync::Arc;

use alpha2_backend_conformance::{Backend, Outcome, corpus};
use alpha2_engine::extract::fixtures::{Digest, video_input};
use alpha2_engine::extract::{
    Direction, Extractor, FrameFrom, InputFrom, Pass, RequestInput, VideoFormat,
};
use alpha2_host_core::extract::{
    ReaderSupplier, VideoReader, displayed_size, present_for_extraction,
};
use alpha2_media::{CancelToken, Decoder, SourceId};
use alpha2_model::{DataValues, Project};
use alpha2_render::CpuRenderer;
use alpha2_time::{MediaTime, TimeRange};

/// A boxed decoder as a [`Decoder`], so readers take any backend's.
pub struct Boxed(pub Box<dyn alpha2_backend_conformance::VideoDecoder>);

impl Decoder for Boxed {
    fn streams(&self) -> &[alpha2_media::StreamInfo] {
        self.0.streams()
    }

    fn flush(&mut self) -> Result<(), alpha2_media::DecodeError> {
        self.0.flush()
    }

    fn seek(
        &mut self,
        request: alpha2_media::DecodeRequest,
        cancel: &CancelToken,
    ) -> Result<alpha2_media::DecodeResult, alpha2_media::DecodeError> {
        self.0.seek(request, cancel)
    }

    fn decode_next(
        &mut self,
        cancel: &CancelToken,
    ) -> Result<alpha2_media::DecodeEvent, alpha2_media::DecodeError> {
        self.0.decode_next(cancel)
    }
}

fn reader(backend: &dyn Backend, name: &str) -> Result<VideoReader<Boxed>, String> {
    let decoder = backend
        .open_video(&corpus(name))
        .map_err(|error| format!("{name}: {error}"))?;
    Ok(VideoReader::new(
        Boxed(decoder),
        SourceId::new("v").expect("id"),
        CancelToken::default(),
    ))
}

fn project() -> Project {
    Project::from_json(
        &serde_json::json!({
            "schema_version": 1, "id": "p", "name": "P",
            "settings": { "width": 640, "height": 360, "frame_rate": { "num": 30, "den": 1 }, "sample_rate": 48_000 },
            "sources": [{ "id": "v", "kind": "video", "locator": "v.mp4" }],
            "timeline": { "tracks": [] }
        })
        .to_string(),
    )
    .expect("project")
}

/// The digest fixture's delivered times and hashes over `range` with `passes` at `size`.
fn digest(
    backend: &dyn Backend,
    name: &str,
    range: (i64, i64),
    passes: Vec<Pass>,
    size: u32,
) -> Result<Vec<(i64, u32)>, String> {
    let mut extractor = Digest::new(video_input(VideoFormat::Rgba8, 960));
    let descriptor = extractor.describe().clone();
    let request = alpha2_engine::extract::ExtractRequest {
        extractor: descriptor.id.clone(),
        version: descriptor.version,
        params: serde_json::Map::new(),
        input: RequestInput {
            from: InputFrom::Source,
            source_id: Some(SourceId::new("v").expect("id")),
            range: TimeRange::new(MediaTime(range.0), MediaTime(range.1)).expect("range"),
            passes,
            size: Some(size),
            frame_rate: None,
            stream: None,
            sample_rate: None,
            channels: None,
        },
        outputs: [(
            "digest".to_owned(),
            alpha2_model::DataSetId::new("d").expect("id"),
        )]
        .into(),
    };
    let renderer = CpuRenderer::new(1);
    let mut supplier =
        ReaderSupplier::<Boxed, Boxed>::new(&renderer, Some(reader(backend, name)?), None);
    let sets =
        alpha2_host_core::extract::run(&project(), request, &mut extractor, &mut supplier, None)
            .map_err(|error| error.to_string())?;
    let DataValues::Scalars(samples) = &sets[0].values else {
        return Err("scalars".into());
    };
    Ok(samples
        .times
        .iter()
        .zip(samples.values.chunks(2))
        .map(|(time, pair)| {
            let [hi, lo] = [pair[0].unwrap_or(0.0), pair[1].unwrap_or(0.0)];
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let hash = ((hi * 65_536.0) as u32) << 16 | ((lo * 65_536.0) as u32);
            (time.ticks(), hash)
        })
        .collect())
}

/// A forward run delivers exactly the frames shown inside the range, by their own pts.
///
/// # Errors
/// What differed.
pub fn reader_delivers_the_frames_shown_inside_the_range(backend: &dyn Backend) -> Outcome {
    // vfr.mp4 presents frames at 0, 4000, 12000, 24000 and 40000 ticks.
    let times = digest(backend, "vfr.mp4", (5_000, 30_000), vec![], 64)?
        .into_iter()
        .map(|(time, _)| time)
        .collect::<Vec<_>>();
    if times == [4_000, 12_000, 24_000] {
        Ok(())
    } else {
        Err(format!("vfr.mp4 delivered {times:?}"))
    }
}

/// Past the stream's end the reader answers that no frame exists, and the pass ends.
///
/// # Errors
/// What differed.
pub fn reader_ends_at_the_stream_end(backend: &dyn Backend) -> Outcome {
    let times = digest(backend, "vfr.mp4", (30_000, 10_000_000), vec![], 64)?
        .into_iter()
        .map(|(time, _)| time)
        .collect::<Vec<_>>();
    if times == [24_000, 40_000] {
        Ok(())
    } else {
        Err(format!("vfr.mp4 delivered {times:?}"))
    }
}

/// A backward run delivers the same frames, with the same bytes, as a forward run.
///
/// # Errors
/// What differed.
pub fn reader_backward_equals_forward_reversed(backend: &dyn Backend) -> Outcome {
    let name = "h264-bt709-copy.mp4";
    let forward = digest(backend, name, (0, 1_000_000), vec![], 48)?;
    let last = forward.last().map_or(0, |(time, _)| *time);
    let backward = digest(
        backend,
        name,
        (0, 1_000_000),
        vec![Pass {
            start: MediaTime(last),
            direction: Direction::Backward,
        }],
        48,
    )?;
    if forward.len() > 10 && forward == backward {
        Ok(())
    } else {
        Err(format!(
            "{name}: forward {} frames, backward {}",
            forward.len(),
            backward.len()
        ))
    }
}

/// Rotation and pixel aspect give the displayed size, and odd sizes round half up.
///
/// # Errors
/// What differed.
pub fn reader_delivers_the_displayed_size(backend: &dyn Backend) -> Outcome {
    let renderer = CpuRenderer::new(1);
    let mut failures = Vec::new();
    for name in [
        "rotation.mp4",
        "sar-16-15.mp4",
        "sar-16-15-rotation.mp4",
        "odd-dimensions.avi",
        "hdr-pq.mp4",
    ] {
        let mut reader = reader(backend, name)?;
        let frame = reader
            .frame(FrameFrom::Start(MediaTime::ZERO), Direction::Forward)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("{name}: no frame"))?;
        let displayed = displayed_size(&frame);
        let delivered = present_for_extraction(&renderer, Arc::clone(&frame), 100)
            .map_err(|error| format!("{name}: {error}"))?;
        let expected = alpha2_engine::extract::delivered_size(displayed, 100);
        if (delivered.width, delivered.height) != expected
            || delivered.rgba.len() != (expected.0 * expected.1 * 4) as usize
        {
            failures.push(format!(
                "{name}: delivered {}x{}, expected {expected:?}",
                delivered.width, delivered.height
            ));
        }
        let rotated = frame.orientation.rotation.swaps_axes();
        let wide = frame.visible_rect.width > frame.visible_rect.height;
        if name.contains("rotation") && rotated && (displayed.0 > displayed.1) == wide {
            failures.push(format!("{name}: displayed {displayed:?} is not turned"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// The delivered bytes are the ones pinned in `tests/golden/extract-recipe.json`
/// (`ALPHA2_UPDATE_GOLDENS=1` rewrites it).
///
/// # Errors
/// What differed.
pub fn reader_bytes_match_the_pinned_recipe(backend: &dyn Backend) -> Outcome {
    let mut found = serde_json::Map::new();
    for name in [
        "h264-bt709-copy.mp4",
        "hevc-hvc1.mp4",
        "rotation.mp4",
        "sar-16-15.mp4",
        "hdr-pq.mp4",
    ] {
        let frames = digest(backend, name, (0, 40_000), vec![], 96)?;
        found.insert(
            name.to_owned(),
            serde_json::json!(
                frames
                    .iter()
                    .map(|(time, hash)| serde_json::json!([time, format!("{hash:08x}")]))
                    .collect::<Vec<_>>()
            ),
        );
    }
    let path = alpha2_backend_conformance::tests_dir().join("golden/extract-recipe.json");
    let text = format!(
        "{}\n",
        serde_json::to_string_pretty(&serde_json::Value::Object(found)).expect("json")
    );
    if std::env::var_os("ALPHA2_UPDATE_GOLDENS").as_deref() == Some(std::ffi::OsStr::new("1")) {
        std::fs::write(&path, &text).map_err(|error| error.to_string())?;
        return Ok(());
    }
    let pinned =
        std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    if pinned == text {
        Ok(())
    } else {
        Err(format!(
            "{} differs from the delivered bytes:\n{text}",
            path.display()
        ))
    }
}

/// Audio blocks at any position hold exactly the samples a decode from the start holds there.
///
/// # Errors
/// What differed.
pub fn reader_audio_blocks_match_a_linear_decode(backend: &dyn Backend) -> Outcome {
    let name = "audio-3.0.wav";
    let rate = 48_000;
    let open = || {
        backend
            .open_audio(&corpus(name), rate, None)
            .map_err(|error| format!("{name}: {error}"))
    };
    let mut linear = open()?;
    let cancel = CancelToken::default();
    let mut planes: Vec<Vec<f32>> = Vec::new();
    while planes.first().map_or(0, Vec::len) < 20_000 {
        match linear
            .decode_next(&cancel)
            .map_err(|error| error.to_string())?
        {
            alpha2_media::DecodeEvent::Audio(block) => {
                planes.resize(block.channels.len(), Vec::new());
                for (plane, more) in planes.iter_mut().zip(block.channels) {
                    plane.extend(more);
                }
            }
            alpha2_media::DecodeEvent::EndOfStream => break,
            _ => {}
        }
    }
    let mut reader = alpha2_host_core::extract::AudioReader::new(
        DecoderBox(open()?),
        SourceId::new("a").expect("id"),
        rate,
        cancel,
    );
    // The fixture holds 4800 frames; the last block runs past them into silence.
    for (start, frames) in [
        (1_000_i64, 500_u32),
        (1_500, 256),
        (0, 777),
        (2_345, 1_000),
        (4_500, 600),
    ] {
        let block = reader
            .block(start, frames, alpha2_engine::extract::AudioChannels::Source)
            .map_err(|error| error.to_string())?;
        let length = frames as usize;
        let from = usize::try_from(start).unwrap_or(0);
        let expected = planes
            .iter()
            .flat_map(|plane| {
                (from..from + length).map(|index| plane.get(index).copied().unwrap_or(0.0))
            })
            .collect::<Vec<f32>>();
        if block.samples != expected {
            return Err(format!(
                "{name}: block {start}+{frames} differs from the linear decode"
            ));
        }
    }
    Ok(())
}

/// A boxed audio decoder as a [`Decoder`].
pub struct DecoderBox(pub Box<dyn Decoder>);

impl Decoder for DecoderBox {
    fn streams(&self) -> &[alpha2_media::StreamInfo] {
        self.0.streams()
    }

    fn flush(&mut self) -> Result<(), alpha2_media::DecodeError> {
        self.0.flush()
    }

    fn seek(
        &mut self,
        request: alpha2_media::DecodeRequest,
        cancel: &CancelToken,
    ) -> Result<alpha2_media::DecodeResult, alpha2_media::DecodeError> {
        self.0.seek(request, cancel)
    }

    fn decode_next(
        &mut self,
        cancel: &CancelToken,
    ) -> Result<alpha2_media::DecodeEvent, alpha2_media::DecodeError> {
        self.0.decode_next(cancel)
    }
}

/// Writes one test per reader case for `$backend`.
#[macro_export]
macro_rules! reader_conformance_tests {
    ($backend:expr) => {
        $crate::reader_conformance_tests!(@cases $backend;
            reader_delivers_the_frames_shown_inside_the_range,
            reader_ends_at_the_stream_end,
            reader_backward_equals_forward_reversed,
            reader_delivers_the_displayed_size,
            reader_bytes_match_the_pinned_recipe,
            reader_audio_blocks_match_a_linear_decode,
        );
    };
    (@cases $backend:expr; $($case:ident),* $(,)?) => {
        $(
            #[test]
            fn $case() {
                let backend = $backend;
                if let Err(failure) = $crate::$case(&backend) {
                    panic!("{failure}");
                }
            }
        )*
    };
}

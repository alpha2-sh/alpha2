//! Surface mode against CPU mode on the same streams.

use std::sync::Arc;

use alpha2_backend_conformance::corpus;
use alpha2_container::{TrackInfo, TrackKind};
use alpha2_host_core::video_stream::{
    BackendFrame, BackendImage, BackendKind, Poll, StreamFrame, StreamOptions, VideoDecodeBackend,
    VideoStream,
};
use alpha2_media::{CancelToken, Plane};

use super::{
    AppleSurface, AppleVideo, SURFACES_BEYOND_REORDER, UNKNOWN_REORDER_DEPTH, VideoOutput,
    VideoToolboxVideo,
};

const SURFACES: VideoOutput = VideoOutput::Surfaces { ten_bit: true };

fn frames(name: &str, output: VideoOutput) -> Vec<StreamFrame> {
    let mut stream = VideoStream::open_file(
        &corpus(name),
        Arc::new(AppleVideo { output }),
        StreamOptions::default(),
    )
    .unwrap_or_else(|error| panic!("{name}: {error}"));
    let cancel = CancelToken::default();
    std::iter::from_fn(|| stream.next(&cancel).expect(name)).collect()
}

fn surface(image: &BackendImage) -> Option<&AppleSurface> {
    match image {
        BackendImage::Surface(lease) => lease.as_any().downcast_ref::<AppleSurface>(),
        BackendImage::Cpu { .. } => None,
    }
}

fn planes(image: &BackendImage) -> Vec<Plane> {
    match image {
        BackendImage::Cpu { planes, .. } => planes.clone(),
        BackendImage::Surface(_) => {
            let surface = surface(image).expect("an Apple surface");
            surface.copy().expect("the surface copies").1
        }
    }
}

fn samples(plane: &Plane, components: usize, sample: usize) -> Vec<&[u8]> {
    let row = plane.width as usize * components * sample;
    plane
        .bytes
        .chunks(plane.stride as usize)
        .take(plane.height as usize)
        .map(|line| &line[..row])
        .collect()
}

fn compare(name: &str) -> (usize, usize) {
    let cpu = frames(name, VideoOutput::Cpu);
    assert!(
        cpu.iter().all(|frame| surface(&frame.image).is_none()),
        "{name}: CPU mode handed over a surface"
    );
    let mut stream = VideoStream::open_file(
        &corpus(name),
        Arc::new(AppleVideo { output: SURFACES }),
        StreamOptions::default(),
    )
    .expect(name);
    let cancel = CancelToken::default();
    let (mut count, mut index) = (0, 0);
    while let Some(other) = stream.next(&cancel).expect(name) {
        let cpu = cpu
            .get(index)
            .unwrap_or_else(|| panic!("{name}: more frames"));
        assert_eq!(cpu.facts, other.facts, "{name} frame {index}");
        let BackendImage::Cpu { format, .. } = &cpu.image else {
            unreachable!()
        };
        if let Some(lease) = surface(&other.image) {
            count += 1;
            assert_eq!(lease.copy().expect("copies").0, *format, "{name} {index}");
        }
        let sample = usize::from(format.bit_depth.div_ceil(8));
        let (expected, seen) = (planes(&cpu.image), planes(&other.image));
        assert_eq!(expected.len(), seen.len(), "{name} frame {index}: planes");
        for (plane, (expected, seen)) in expected.iter().zip(&seen).enumerate() {
            let components = if plane == 0 { 1 } else { 2 };
            assert_eq!(
                (expected.width, expected.height),
                (seen.width, seen.height),
                "{name} frame {index} plane {plane}"
            );
            assert!(
                samples(expected, components, sample) == samples(seen, components, sample),
                "{name} frame {index} plane {plane}: samples differ"
            );
        }
        index += 1;
    }
    assert_eq!(index, cpu.len(), "{name}: frame counts");
    eprintln!("{name}: {count} of {index} frames were surfaces");
    (count, index)
}

#[test]
fn hardware_sized_streams_hand_over_every_frame_as_a_surface_equal_to_its_copy() {
    for name in [
        "h264-padded.mp4",
        "hevc-padded.mp4",
        "hevc-main10-padded.mp4",
        "h264-bt709-crop-bottom.mp4",
        "hevc-topleft.mp4",
    ] {
        let (surfaces, frames) = compare(name);
        assert!(frames > 0, "{name}");
        assert_eq!(surfaces, frames, "{name}");
    }
}

#[test]
fn a_left_crop_is_copied_with_the_window_as_its_visible_rectangle() {
    let name = "h264-bt709-crop-left.mp4";
    let (surfaces, frames) = compare(name);
    assert!(frames > 0, "{name}");
    assert_eq!(surfaces, 0, "{name}");
}

#[test]
fn ten_bit_tracks_copy_unless_surfaces_take_them() {
    let frames = frames(
        "hevc-main10-padded.mp4",
        VideoOutput::Surfaces { ten_bit: false },
    );
    assert!(!frames.is_empty());
    assert!(frames.iter().all(|frame| surface(&frame.image).is_none()));
}

fn track(name: &str) -> TrackInfo {
    let reader = alpha2_container::open(&corpus(name)).expect(name);
    let index = alpha2_container::default_track(reader.tracks(), TrackKind::Video).expect(name);
    reader.tracks()[index].clone()
}

fn held(name: &str, track: &TrackInfo, drop_at: Option<usize>) -> Vec<bool> {
    let mut backend = VideoToolboxVideo::open(track, BackendKind::Hardware, SURFACES)
        .or_else(|_| VideoToolboxVideo::open(track, BackendKind::Software, SURFACES))
        .expect(name);
    let mut reader = alpha2_container::open(&corpus(name)).expect(name);
    let mut kept: Vec<BackendFrame> = Vec::new();
    let mut kinds = Vec::new();
    let mut take = |backend: &mut VideoToolboxVideo, kept: &mut Vec<BackendFrame>| {
        while let Poll::Frame(frame) = backend.receive(false).expect(name) {
            kinds.push(surface(&frame.image).is_some());
            kept.push(frame);
            if drop_at == Some(kinds.len()) {
                kept.clear();
            }
        }
    };
    while let Some(packet) = reader.next_packet(track.id).expect(name) {
        backend.submit(&packet).expect(name);
        take(&mut backend, &mut kept);
    }
    backend.finish_input().expect(name);
    take(&mut backend, &mut kept);
    kinds
}

#[test]
fn past_the_cap_pictures_are_copied_until_held_surfaces_go() {
    let name = "h264-bt709-copy.mp4";
    let track = track(name);
    let depth = track
        .video
        .as_ref()
        .and_then(|video| video.reorder_depth)
        .expect("the corpus stream declares its reorder depth");
    let cap = depth + SURFACES_BEYOND_REORDER;
    let kinds = held(name, &track, None);
    assert!(kinds.len() > cap + 2, "{} frames", kinds.len());
    assert!(kinds[..cap].iter().all(|surface| *surface), "{kinds:?}");
    assert!(kinds[cap..].iter().all(|surface| !surface), "{kinds:?}");
    let kinds = held(name, &track, Some(cap + 2));
    assert!(
        kinds[cap + 2..].first().is_some_and(|surface| *surface),
        "{kinds:?}"
    );
}

#[test]
fn a_track_of_unknown_reorder_depth_keeps_twenty_surfaces() {
    let name = "h264-bt709-copy.mp4";
    let mut track = track(name);
    track.video.as_mut().expect("video").reorder_depth = None;
    let kinds = held(name, &track, None);
    let cap = UNKNOWN_REORDER_DEPTH + SURFACES_BEYOND_REORDER;
    assert_eq!(cap, 20);
    assert!(kinds.len() > cap, "{} frames", kinds.len());
    assert_eq!(kinds.iter().filter(|surface| **surface).count(), cap);
    assert!(kinds[..cap].iter().all(|surface| *surface), "{kinds:?}");
}

#[test]
fn a_surface_counts_what_its_pixel_buffer_holds() {
    let frames = frames("h264-padded.mp4", SURFACES);
    let frame = frames.first().expect("a frame");
    let BackendImage::Surface(lease) = &frame.image else {
        panic!("not a surface");
    };
    let copied: u64 = planes(&frame.image)
        .iter()
        .map(|plane| u64::from(plane.stride) * u64::from(plane.height))
        .sum();
    let held = lease.retained_bytes();
    assert!(
        held > 0 && held <= copied,
        "{held} bytes held, {copied} copied"
    );
    assert!(
        !surface(&frame.image)
            .expect("apple")
            .pixel_buffer()
            .is_null()
    );
}

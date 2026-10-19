//! Source blocks: short blocks, downmixing by the speakers a block names, and blocks that are not what they say.

use super::*;

/// Renders one clip of `fixture`'s source into `channels` project
/// channels.
fn render_source(fixture: &Fixture, channels: usize) -> AudioBlock {
    let project = project(
        vec![track(
            "track",
            vec![audio_clip("clip", "source", 0, TICKS_PER_SECOND)],
        )],
        vec!["source"],
    );
    let snapshot = EngineProject::new(project).expect("valid project");
    let mut renderer = AudioRenderer::new(&snapshot, channels, 512).expect("renderer");
    let status = renderer
        .render(&snapshot, 0, 512, fixture, TransportState::Playing)
        .expect("render");
    assert!(status.is_complete());
    renderer.block(&status)
}

/// A provider holding the first `frames` samples of a source, and
/// saying whether that is where the source ends.
struct Prefix {
    block: AudioBlock,
    ends_source: bool,
}

impl AudioBlockProvider for Prefix {
    fn block(&self, _: &SourceId, _: i64, _: usize) -> Option<AudioSpan<'_>> {
        Some(AudioSpan {
            block: &self.block,
            starts_source: true,
            ends_source: self.ends_source,
        })
    }
}

#[test]
fn a_block_short_of_the_request_is_missing_unless_the_source_ends_there() {
    let project = project(
        vec![track(
            "track",
            vec![audio_clip("clip", "source", 0, TICKS_PER_SECOND)],
        )],
        vec!["source"],
    );
    let snapshot = EngineProject::new(project).expect("valid project");
    let block = AudioBlock {
        start_sample: 0,
        sample_rate: RATE,
        speakers: alpha2_media::SpeakerLayout::MONO,
        channels: vec![vec![0.5; 300]],
    };
    let render = |ends_source| {
        let mut renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
        let provider = Prefix {
            block: block.clone(),
            ends_source,
        };
        let status = renderer
            .render(&snapshot, 0, 512, &provider, TransportState::Playing)
            .expect("render");
        (status.is_complete(), renderer.block(&status))
    };
    assert!(!render(false).0);
    // The source ends there: the rest is silence, and the block is whole.
    let (complete, mixed) = render(true);
    assert!(complete);
    assert!(mixed.channels[0][..299].iter().all(|sample| *sample > 0.0));
    assert!(mixed.channels[0][301..].iter().all(|sample| *sample == 0.0));
}

#[test]
fn a_surround_source_is_folded_to_stereo_by_itu_downmix_without_its_lfe() {
    // 5.1 in speaker order: FL FR FC LFE BL BR.
    let surround = render_source(
        &Fixture::planes("source", &[0.1, 0.2, 0.3, 0.9, 0.5, 0.6]),
        2,
    );
    let half = std::f32::consts::FRAC_1_SQRT_2;
    let folded = render_source(
        &Fixture::planes(
            "source",
            &[0.1 + half * 0.3 + half * 0.5, 0.2 + half * 0.3 + half * 0.6],
        ),
        2,
    );
    assert_eq!(surround, folded);
    // Reading only planes 0 and 1 would lose the center.
    let fronts = render_source(&Fixture::planes("source", &[0.1, 0.2]), 2);
    assert_ne!(surround, fronts);
}

#[test]
fn a_source_is_folded_by_the_speakers_its_block_names() {
    let mut lcr = Fixture::planes("source", &[0.3, 0.1, 0.2]);
    lcr.blocks[0].1.speakers = SpeakerLayout::new(&[
        alpha2_media::Speaker::FrontCenter,
        alpha2_media::Speaker::FrontLeft,
        alpha2_media::Speaker::FrontRight,
    ])
    .expect("C L R");
    let half = std::f32::consts::FRAC_1_SQRT_2;
    let folded = render_source(
        &Fixture::planes("source", &[0.1 + half * 0.3, 0.2 + half * 0.3]),
        2,
    );
    assert_eq!(render_source(&lcr, 2), folded);
    // Three planes in the default layout are 2.1: the third is the LFE, left out.
    let two_one = render_source(&Fixture::planes("source", &[0.1, 0.2, 0.3]), 2);
    assert_eq!(
        two_one,
        render_source(&Fixture::planes("source", &[0.1, 0.2]), 2)
    );
}

#[test]
fn a_mono_project_hears_both_channels_of_a_stereo_source() {
    let stereo = render_source(&Fixture::planes("source", &[0.2, 0.6]), 1);
    let average = render_source(&Fixture::planes("source", &[0.4]), 1);
    assert_eq!(stereo, average);
}

#[test]
fn a_block_whose_speakers_are_not_its_planes_is_missing_media() {
    let project = project(
        vec![track(
            "track",
            vec![audio_clip("clip", "source", 0, TICKS_PER_SECOND)],
        )],
        vec!["source"],
    );
    let snapshot = EngineProject::new(project).expect("valid project");
    let mut renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    let mut mismatched = Fixture::planes("source", &[0.1; 3]);
    mismatched.blocks[0].1.speakers = SpeakerLayout::STEREO;
    let status = renderer
        .render(&snapshot, 0, 512, &mismatched, TransportState::Playing)
        .expect("render");
    assert_eq!(status.missing_sources.len(), 1);
}

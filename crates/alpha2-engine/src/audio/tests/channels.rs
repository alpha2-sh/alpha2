//! Clip channel selection (`audio.channels`) and audio stream selection (`audio.stream`).

use super::*;

use alpha2_media::Speaker;

/// Constant planes per source stream, each block starting at sample zero.
struct Streams(Vec<((SourceId, Option<u32>), AudioBlock)>);

impl Streams {
    fn new(streams: &[(Option<u32>, SpeakerLayout, &[f32])]) -> Self {
        Self(
            streams
                .iter()
                .map(|(stream, speakers, values)| {
                    let block = AudioBlock {
                        start_sample: 0,
                        sample_rate: RATE,
                        speakers: *speakers,
                        channels: values
                            .iter()
                            .map(|value| vec![*value; RATE as usize])
                            .collect(),
                    };
                    (
                        (SourceId::new("mics".to_owned()).expect("id"), *stream),
                        block,
                    )
                })
                .collect(),
        )
    }
}

impl AudioBlockProvider for Streams {
    fn block(
        &self,
        source: &SourceId,
        stream: Option<u32>,
        _start_sample: i64,
        _frames: usize,
    ) -> Option<AudioSpan<'_>> {
        self.0
            .iter()
            .find(|((id, held), _)| id == source && *held == stream)
            .map(|(_, block)| AudioSpan::whole(block))
    }
}

fn quad() -> SpeakerLayout {
    SpeakerLayout::default_for(4).expect("quad")
}

fn lane(stream: Option<u32>, channels: Option<Vec<u32>>) -> Project {
    let seconds = MediaTime::from_seconds(1).ticks();
    let mut clip = audio_clip("c1", "mics", 0, seconds);
    clip.audio.stream = stream;
    clip.audio.channels = channels;
    project(vec![track("a1", vec![clip])], vec!["mics"])
}

fn mixed(project: Project, provider: &Streams) -> (bool, [f32; 2]) {
    let snapshot = EngineProject::new(project).expect("valid project");
    let mut renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    let status = renderer
        .render(&snapshot, 0, 64, provider, TransportState::Playing)
        .expect("render");
    (
        status.is_complete(),
        [renderer.channel(0)[32], renderer.channel(1)[32]],
    )
}

#[test]
fn one_selected_channel_plays_as_mono_and_two_as_a_stereo_pair() {
    let provider = Streams::new(&[(None, quad(), &[0.1, 0.2, 0.3, 0.4])]);
    assert_eq!(
        mixed(lane(None, Some(vec![2])), &provider),
        (true, [0.3, 0.3])
    );
    assert_eq!(
        mixed(lane(None, Some(vec![3, 1])), &provider),
        (true, [0.4, 0.2])
    );
    let (complete, folded) = mixed(lane(None, None), &provider);
    assert!(complete);
    // Four channels default to 4.0 (FL, FR, FC, BC): center at -3 dB, back center at -6 dB.
    let left = 0.1 + 0.3 * std::f32::consts::FRAC_1_SQRT_2 + 0.4 * 0.5;
    assert!((folded[0] - left).abs() < 1.0e-6, "{folded:?}");
}

#[test]
fn channels_without_a_speaker_are_selectable() {
    let discrete = SpeakerLayout::new(&[Speaker::Unnamed; 3]).expect("layout");
    let provider = Streams::new(&[(None, discrete, &[0.1, 0.2, 0.3])]);
    assert_eq!(mixed(lane(None, None), &provider), (true, [0.0, 0.0]));
    assert_eq!(
        mixed(lane(None, Some(vec![1])), &provider),
        (true, [0.2, 0.2])
    );
}

#[test]
fn a_selected_channel_the_stream_lacks_leaves_the_source_missing() {
    let provider = Streams::new(&[(None, quad(), &[0.1, 0.2, 0.3, 0.4])]);
    assert_eq!(
        mixed(lane(None, Some(vec![4])), &provider),
        (false, [0.0, 0.0])
    );
}

#[test]
fn a_clip_reads_and_needs_the_stream_it_names() {
    let stereo = SpeakerLayout::default_for(2).expect("stereo");
    let provider = Streams::new(&[
        (None, stereo, &[0.5, 0.5]),
        (Some(1), quad(), &[0.1, 0.2, 0.3, 0.4]),
    ]);
    assert_eq!(
        mixed(lane(Some(1), Some(vec![0])), &provider),
        (true, [0.1, 0.1])
    );
    assert_eq!(
        mixed(lane(None, Some(vec![1])), &provider),
        (true, [0.5, 0.5])
    );
    assert!(!mixed(lane(Some(2), None), &provider).0);

    let snapshot = EngineProject::new(lane(Some(1), None)).expect("valid project");
    let renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    let needs = renderer.audio_needs(&snapshot, 0, 64).expect("needs");
    assert_ne!(needs.len(), 0);
    assert!(needs.iter().all(|need| need.stream == Some(1)));
}

#[test]
fn a_channel_selection_is_one_or_two_distinct_indices() {
    for channels in [vec![], vec![0, 0], vec![0, 1, 2], vec![24]] {
        assert!(
            EngineProject::new(lane(None, Some(channels.clone()))).is_err(),
            "{channels:?}"
        );
    }
    assert!(EngineProject::new(lane(Some(3), Some(vec![23, 0]))).is_ok());
}

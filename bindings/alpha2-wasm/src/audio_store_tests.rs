use super::*;

const RATE: u32 = 48_000;
const MONO: SpeakerLayout = SpeakerLayout::MONO;
const STEREO: SpeakerLayout = SpeakerLayout::STEREO;

fn ticks(samples: i64) -> MediaTime {
    MediaTime::from_ticks(samples * TICKS_PER_SECOND / i64::from(RATE))
}

fn source(id: &str) -> SourceId {
    SourceId::new(id.to_owned()).expect("id")
}

#[test]
fn rolling_trim_and_append_account_for_allocated_capacity() {
    let mut store = AudioStore::new(RATE);
    for index in 0..26 {
        let name = format!("s{index}");
        assert!(
            store
                .upload(&name, 0, RATE, STEREO, &[0.25; 2_000], false)
                .is_ok()
        );
        store.trim_before(&name, ticks(990));
        assert!(
            store
                .upload(&name, 1_000, RATE, STEREO, &[0.5; 60], false)
                .is_ok()
        );
        let allocated: usize = store
            .sources
            .values()
            .flatten()
            .flat_map(|w| &w.block.channels)
            .map(|p| p.capacity() * std::mem::size_of::<f32>())
            .sum();
        assert_eq!(store.retained_bytes(), allocated);
        let block = store
            .block(&source(&name), 990, 40)
            .expect("retained tail")
            .block;
        assert!(block.channels[0].capacity() <= 80);
        assert_eq!(&block.channels[0][..10], &[0.25; 10]);
        assert_eq!(&block.channels[0][10..], &[0.5; 30]);
    }
}

#[test]
fn continuation_and_replacement_obey_a_small_allocation_budget() {
    let mut store = AudioStore::new(RATE);
    store.byte_budget = 1_024;
    assert!(
        store
            .upload("old", 0, RATE, STEREO, &[0.0; 128], false)
            .is_ok()
    );
    assert!(
        store
            .upload("live", 0, RATE, STEREO, &[0.25; 128], false)
            .is_ok()
    );
    assert!(
        store
            .upload("live", 64, RATE, STEREO, &[0.5; 64], false)
            .is_ok()
    );
    assert_eq!(store.windows("old"), [] as [(i64, i64); 0]);
    assert_eq!(store.retained_bytes(), 768);
    // A continuation past what one window holds keeps the upload and the latest samples
    // before it, less a quarter window more, in the planes it had.
    assert!(
        store
            .upload("live", 96, RATE, STEREO, &[0.75; 128], false)
            .is_ok()
    );
    assert_eq!(store.windows("live"), vec![(64, 160)]);
    let block = store.block(&source("live"), 64, 96).expect("window").block;
    assert_eq!(&block.channels[0][..32], &[0.5; 32]);
    assert_eq!(&block.channels[1][32..], &[0.75; 64]);
    assert_eq!(store.retained_bytes(), 768);
    assert!(
        store
            .upload("live", 0, RATE, STEREO, &[0.75; 256], false)
            .is_ok()
    );
    assert_eq!(store.windows("live"), vec![(0, 128)]);
    assert_eq!(store.retained_bytes(), 1_024);
    assert_eq!(
        store.upload("huge", 0, RATE, STEREO, &[0.0; 258], false),
        Err(UploadRefusal::TooLarge)
    );
    assert_eq!(
        store.windows("live"),
        vec![(0, 128)],
        "a refusal keeps what was held"
    );
}

#[test]
fn every_source_together_stays_within_the_total_budget() {
    // Per-window limits alone would let 128 sources keep 600 s each (29 GB against wasm32's 4 GB);
    // the byte budget keeps only the newest uploads.
    let mut store = AudioStore::new(RATE);
    let minutes = vec![0.25_f32; 2 * 120 * RATE as usize];
    let per_upload = minutes.len() * std::mem::size_of::<f32>();
    let sources = MAX_RETAINED_BYTES / per_upload + 3;
    for index in 0..sources {
        assert!(
            store
                .upload(&format!("s{index}"), 0, RATE, STEREO, &minutes, false)
                .is_ok()
        );
        assert!(
            store.retained_bytes() <= MAX_RETAINED_BYTES,
            "after {index}"
        );
    }
    let newest = format!("s{}", sources - 1);
    assert_eq!(store.windows(&newest), vec![(0, 120 * i64::from(RATE))]);
    assert!(
        store.windows("s0").is_empty(),
        "the oldest upload is dropped first"
    );
    assert!(store.retained_bytes() + per_upload > MAX_RETAINED_BYTES);
}

#[test]
fn retaining_sources_drops_only_the_others() {
    let mut store = AudioStore::new(RATE);
    assert!(
        store
            .upload("kept", 0, RATE, MONO, &[1.0; 4], false)
            .is_ok()
    );
    assert!(
        store
            .upload("dropped", 0, RATE, MONO, &[1.0; 4], false)
            .is_ok()
    );
    store.retain_sources(|source| source != "dropped");
    assert_eq!(store.sample_rate(), RATE);
    assert_eq!(store.windows("kept"), vec![(0, 4)]);
    assert_eq!(store.windows("dropped"), [] as [(i64, i64); 0]);
}

#[test]
fn contiguous_uploads_extend_one_window() {
    let mut store = AudioStore::new(RATE);
    assert!(store.upload("a", 0, RATE, STEREO, &[1.0; 8], false).is_ok());
    assert!(store.upload("a", 4, RATE, STEREO, &[2.0; 8], false).is_ok());

    let block = store.block(&source("a"), 0, 8).expect("block").block;
    assert_eq!(block.start_sample, 0);
    assert_eq!(block.channels.len(), 2);
    assert_eq!(
        block.channels[0],
        vec![1.0, 1.0, 1.0, 1.0, 2.0, 2.0, 2.0, 2.0]
    );
}

#[test]
fn an_overlapping_upload_replaces_the_window() {
    let mut store = AudioStore::new(RATE);
    store
        .upload("a", 0, RATE, MONO, &[1.0; 8], false)
        .expect("upload");
    store
        .upload("a", 4, RATE, MONO, &[3.0; 8], false)
        .expect("upload");

    let block = store.block(&source("a"), 4, 4).expect("block").block;
    assert_eq!(block.start_sample, 4);
    assert_eq!(block.channels[0], vec![3.0; 8]);
    assert!(store.block(&source("a"), 0, 2).is_none());
    assert_eq!(store.retained_frames(), 8);
}

#[test]
fn two_clips_of_one_source_keep_a_window_each() {
    // The first clip's tail and the second clip's head, both needed by the block that
    // straddles their join.
    let mut store = AudioStore::new(RATE);
    store
        .upload("a", 96_000, RATE, MONO, &[1.0; 4], false)
        .expect("upload");
    store
        .upload("a", 0, RATE, MONO, &[2.0; 4], false)
        .expect("upload");

    let tail = store.block(&source("a"), 96_001, 2).expect("tail").block;
    assert_eq!(tail.channels[0], vec![1.0; 4]);
    let head = store.block(&source("a"), 0, 2).expect("head").block;
    assert_eq!(head.channels[0], vec![2.0; 4]);
    assert_eq!(store.retained_frames(), 8);
}

#[test]
fn windows_report_what_the_store_kept() {
    let mut store = AudioStore::new(RATE);
    assert_eq!(store.windows("a"), [] as [(i64, i64); 0]);
    store
        .upload("a", 0, RATE, MONO, &[1.0; 4], false)
        .expect("upload");
    store
        .upload("a", 4, RATE, MONO, &[1.0; 4], false)
        .expect("upload");
    store
        .upload("a", 100, RATE, MONO, &[1.0; 4], false)
        .expect("upload");
    assert_eq!(store.windows("a"), vec![(100, 104), (0, 8)]);

    // An overlapping upload replaces the window it overlaps.
    store
        .upload("a", 6, RATE, MONO, &[1.0; 4], false)
        .expect("upload");
    assert_eq!(store.windows("a"), vec![(6, 10), (100, 104)]);

    // An upload that meets a window joins it.
    store
        .upload("a", 2, RATE, MONO, &[1.0; 4], false)
        .expect("upload");
    assert_eq!(store.windows("a"), vec![(2, 10), (100, 104)]);

    // Any other upload adds a window, and none is dropped while the budget holds them.
    for index in 0..8 {
        let start = 1_000 + i64::from(index) * 100;
        store
            .upload("a", start, RATE, MONO, &[1.0; 4], false)
            .expect("upload");
    }
    let windows = store.windows("a");
    assert_eq!(windows.len(), 10);
    assert_eq!(windows[0], (1_700, 1_704), "the latest upload first");
    assert!(windows.contains(&(2, 10)) && windows.contains(&(100, 104)));

    store.trim_before("a", ticks(1_702));
    assert_eq!(store.windows("a"), vec![(1_702, 1_704)]);
    assert_eq!(store.windows("b"), [] as [(i64, i64); 0]);
}

#[test]
fn a_request_starting_just_before_a_window_reads_it() {
    let mut store = AudioStore::new(RATE);
    store
        .upload("a", 10, RATE, MONO, &[1.0; 4], false)
        .expect("upload");
    assert!(store.block(&source("a"), 9, 4).is_some());
    assert!(store.block(&source("a"), 4, 4).is_none());
}

#[test]
fn a_source_keeps_every_window_the_budget_holds() {
    // A replay after a seek reads every clip its history reaches, and clips cut from one
    // recording read as many separate parts of it.
    let mut store = AudioStore::new(RATE);
    for index in 0..64 {
        let start = i64::from(index) * 100;
        store
            .upload("a", start, RATE, MONO, &[1.0; 4], false)
            .expect("upload");
    }
    assert_eq!(store.retained_frames(), 64 * 4);
    for index in 0..64 {
        let start = i64::from(index) * 100;
        assert!(
            store
                .block(&source("a"), start, 4)
                .is_some_and(|span| span.covers(start, start + 4))
        );
    }

    // The 600 s a window holds is not a limit on the source.
    let limit = usize::try_from(RATE).expect("rate") * MAX_RETAINED_SECONDS;
    assert!(
        store
            .upload("b", 0, RATE, MONO, &vec![1.0; limit - 4], false)
            .is_ok()
    );
    assert!(
        store
            .upload("b", 1_000_000_000, RATE, MONO, &[1.0; 8], false)
            .is_ok()
    );
    assert!(store.block(&source("b"), 0, 1).is_some());
    assert!(store.block(&source("b"), 1_000_000_000, 1).is_some());
    assert_eq!(
        store.upload("b", 0, RATE, MONO, &vec![1.0; limit + 1], false),
        Err(UploadRefusal::TooLarge)
    );
}

#[test]
fn an_upload_joins_the_windows_it_meets_on_either_side() {
    let ramp = |from: i64, to: i64| -> Vec<f32> {
        (from..to)
            .map(|index| f32::from(u16::try_from(index).expect("small")))
            .collect()
    };
    let reads = |store: &AudioStore, start: i64, frames: usize| {
        let span = store.block(&source("a"), start, frames).expect("span");
        assert!(
            span.covers(start, start + i64::try_from(frames).expect("frames")),
            "{start}+{frames}"
        );
        let offset = usize::try_from(start - span.block.start_sample).expect("offset");
        assert_eq!(
            span.block.channels[0][offset..offset + frames],
            ramp(start, start + i64::try_from(frames).expect("frames"))[..]
        );
    };

    // The part just before a window, decoded after it: a backward seek.
    let mut store = AudioStore::new(RATE);
    store
        .upload("a", 1_000, RATE, MONO, &ramp(1_000, 2_000), false)
        .expect("upload");
    store
        .upload("a", 500, RATE, MONO, &ramp(500, 1_000), false)
        .expect("upload");
    assert_eq!(store.windows("a"), vec![(500, 2_000)]);
    reads(&store, 900, 200);

    // The gap between two windows.
    let mut store = AudioStore::new(RATE);
    store
        .upload("a", 0, RATE, MONO, &ramp(0, 1_000), false)
        .expect("upload");
    store
        .upload("a", 2_000, RATE, MONO, &ramp(2_000, 3_000), false)
        .expect("upload");
    store
        .upload("a", 1_000, RATE, MONO, &ramp(1_000, 2_000), false)
        .expect("upload");
    assert_eq!(store.windows("a"), vec![(0, 3_000)]);
    reads(&store, 900, 200);
    reads(&store, 1_900, 200);
    assert_eq!(store.retained_bytes(), 3_000 * std::mem::size_of::<f32>());

    // A window of another channel layout is another decode: it stays apart.
    let mut store = AudioStore::new(RATE);
    store
        .upload("a", 0, RATE, STEREO, &[0.5; 200], false)
        .expect("upload");
    store
        .upload("a", 100, RATE, MONO, &ramp(100, 200), false)
        .expect("upload");
    assert_eq!(store.windows("a"), vec![(100, 200), (0, 100)]);
}

#[test]
fn every_source_is_held_while_the_budget_holds_it() {
    // About 2 s of stereo for each of 200 sources: a long timeline's windows, well inside
    // the byte budget.
    let mut store = AudioStore::new(RATE);
    let window = vec![0.25_f32; 2 * 100_000];
    for index in 0..200 {
        assert_eq!(
            store.upload(&format!("s{index}"), 0, RATE, STEREO, &window, false),
            Ok(()),
            "source {index}"
        );
    }
    for index in 0..200 {
        assert_eq!(
            store.windows(&format!("s{index}")),
            vec![(0, 100_000)],
            "source {index}"
        );
    }
}

#[test]
fn uploads_reject_mismatched_rates_and_ragged_planes() {
    let mut store = AudioStore::new(RATE);
    let three = SpeakerLayout::default_for(3).expect("2.1");
    for (rate, channels, samples, refusal) in [
        (
            44_100,
            STEREO,
            &[1.0; 8][..],
            UploadRefusal::WrongRate { expected: RATE },
        ),
        (RATE, three, &[1.0; 8][..], UploadRefusal::Planes),
        (RATE, STEREO, &[][..], UploadRefusal::Planes),
    ] {
        assert_eq!(
            store.upload("a", 0, rate, channels, samples, false),
            Err(refusal)
        );
    }
    assert!(store.block(&source("a"), 0, 1).is_none());
}

#[test]
fn an_upload_no_window_holds_is_refused_from_its_length_alone() {
    // The binding asks before it copies the samples in; otherwise two hours of stereo would
    // grow the wasm heap by 2.76 GB before the refusal.
    let store = AudioStore::new(48_000);
    let two_hours = 2 * 48_000 * 7_200;
    assert_eq!(
        store.admits(48_000, 2, two_hours),
        Err(UploadRefusal::TooLarge)
    );
    assert_eq!(store.admits(48_000, 2, 2 * 48_000 * 600), Ok(()));
    assert_eq!(
        store.admits(48_000, 2, 2 * 48_000 * 600 + 2),
        Err(UploadRefusal::TooLarge)
    );
    // 7.1 runs into the byte budget before 600 s.
    assert_eq!(
        store.admits(48_000, 8, 8 * 48_000 * 360),
        Err(UploadRefusal::TooLarge)
    );
}

#[test]
fn a_non_finite_sample_is_kept_for_the_mixer_to_read_as_silence() {
    // A native host hands the same decoded samples to the mixer, whose reads (`interpolate`)
    // take them as silence, so one NaN or infinite sample must not refuse the whole upload.
    let mut store = AudioStore::new(RATE);
    let samples = [0.5, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.25, 0.0];
    assert_eq!(store.upload("a", 0, RATE, STEREO, &samples, false), Ok(()));
    let block = store.block(&source("a"), 0, 3).expect("kept").block;
    let bits = |plane: &[f32]| {
        plane
            .iter()
            .map(|sample| sample.to_bits())
            .collect::<Vec<_>>()
    };
    assert_eq!(bits(&block.channels[0]), bits(&samples[..3]));
    assert_eq!(bits(&block.channels[1]), bits(&samples[3..]));
}

#[test]
fn an_upload_keeps_its_speakers_and_joins_only_windows_of_the_same() {
    let mut store = AudioStore::new(RATE);
    let lcr = SpeakerLayout::new(&[
        alpha2_media::Speaker::FrontCenter,
        alpha2_media::Speaker::FrontLeft,
        alpha2_media::Speaker::FrontRight,
    ])
    .expect("C L R");
    let surround = SpeakerLayout::default_for(3).expect("2.1");
    assert!(store.upload("a", 0, RATE, lcr, &[0.5; 12], false).is_ok());
    assert!(
        store
            .upload("a", 4, RATE, surround, &[0.5; 12], false)
            .is_ok()
    );
    assert_eq!(store.windows("a"), vec![(4, 8), (0, 4)], "not joined");
    let span = store.block(&source("a"), 0, 4).expect("the first");
    assert_eq!(span.block.speakers, lcr);
    assert!(
        store
            .upload("a", 8, RATE, surround, &[0.5; 12], false)
            .is_ok()
    );
    assert_eq!(store.windows("a"), vec![(4, 12), (0, 4)], "joined");
}

#[test]
fn trimming_drops_consumed_windows_and_shortens_live_ones() {
    let mut store = AudioStore::new(RATE);
    store
        .upload("old", 0, RATE, MONO, &[1.0; 4], false)
        .expect("upload");
    store
        .upload("live", 0, RATE, MONO, &[1.0, 2.0, 3.0, 4.0], false)
        .expect("upload");
    store.trim_before("live", ticks(2));

    let live = store.block(&source("live"), 2, 2).expect("block").block;
    assert_eq!(live.channels[0], vec![3.0, 4.0]);
    assert_eq!(live.start_sample, 2);
    // Another source's samples are on another timeline and stay.
    assert_eq!(store.windows("old"), vec![(0, 4)]);

    store.trim_before("live", ticks(100));
    assert!(store.block(&source("live"), 0, 1).is_none());
    store.trim_before("old", ticks(100));
    assert_eq!(store.retained_frames(), 0);
}

#[test]
fn release_and_clear_drop_retained_audio() {
    let mut store = AudioStore::new(RATE);
    store
        .upload("a", 0, RATE, MONO, &[1.0; 4], false)
        .expect("upload");
    store
        .upload("b", 0, RATE, MONO, &[1.0; 4], false)
        .expect("upload");
    assert_eq!(store.retained_frames(), 8);
    assert!(store.release("a"));
    assert!(!store.release("a"));
    store.clear();
    assert_eq!(store.retained_frames(), 0);
}

/// A clip's timeline range and source range, in ticks.
type Cut = ((i64, i64), (i64, i64));

/// One audio track of clips cut from source `a`, each a `(timeline,
/// source)` pair of tick ranges, mixed through `master`.
fn one_source_project(
    clips: &[Cut],
    master: alpha2_model::MasterAudio,
) -> alpha2_engine::EngineProject {
    use alpha2_model::{
        Clip, ClipId, MediaKind, MediaSource, Project, ProjectId, ProjectSettings, Timeline, Track,
        TrackId, TrackKind,
    };
    use alpha2_time::{FrameRate, TimeRange};

    let range = |(start, end)| {
        TimeRange::new(MediaTime::from_ticks(start), MediaTime::from_ticks(end)).expect("range")
    };
    let mut track = Track::new(
        TrackId::new("a1".to_owned()).expect("id"),
        "a1",
        TrackKind::Audio,
    );
    track.clips = clips
        .iter()
        .enumerate()
        .map(|(index, &(timeline, source))| {
            Clip::new(
                ClipId::new(format!("c{index}")).expect("id"),
                source_id("a"),
                range(timeline),
                range(source),
            )
        })
        .collect();
    alpha2_engine::EngineProject::new(Project {
        schema_version: alpha2_model::CURRENT_SCHEMA_VERSION,
        id: ProjectId::new("one-source".to_owned()).expect("id"),
        name: "one source".into(),
        settings: ProjectSettings::new(16, 16, FrameRate::new(30, 1).expect("fps"), RATE)
            .expect("settings"),
        sources: vec![MediaSource::new(
            source_id("a"),
            "asset://a",
            MediaKind::Audio,
        )],
        effect_resources: Vec::new(),
        compositions: Vec::new(),
        timeline: Timeline {
            tracks: vec![track],
        },
        audio: master,
    })
    .expect("project")
}

/// A source whose every sample differs from its neighbours'.
fn signal(seconds: usize) -> Vec<f32> {
    (0..usize::try_from(RATE).expect("rate") * seconds)
        .map(|index| f32::from(u16::try_from(index % 997).expect("small")) / 997.0 - 0.5)
        .collect()
}

/// Uploads one need the way the supplied decoders deliver it: a
/// two-sample margin each side, then `ahead` more, zeros past the
/// source's end.
fn upload_need(
    store: &mut AudioStore,
    signal: &[f32],
    need: &alpha2_engine::AudioNeed,
    ahead: i64,
) {
    let from = (samples_at(need.source_range.start(), RATE) - 2).max(0);
    let to = samples_at(need.source_range.end(), RATE) + 2 + ahead;
    let samples: Vec<f32> = (from..to)
        .map(|index| {
            usize::try_from(index)
                .ok()
                .and_then(|index| signal.get(index))
                .copied()
                .unwrap_or(0.0)
        })
        .collect();
    assert_eq!(store.upload("a", from, RATE, MONO, &samples, false), Ok(()));
}

/// Mixes a range whose replay, after a seek, reads many separate parts of
/// one source, each part an upload of exactly what its need names.
#[test]
fn a_replay_reading_many_parts_of_one_source_mixes_from_the_uploads_its_needs_name() {
    use alpha2_audio::TransportState;
    use alpha2_engine::AudioRenderer;

    const SECOND: i64 = TICKS_PER_SECOND;
    const CUT: i64 = SECOND / 10;
    // Twelve 0.1 s cuts, each from its own place three seconds apart in one recording,
    // inside the default limiter's 1.6 s of history.
    let cuts: Vec<_> = (0..12)
        .map(|index| {
            (
                (index * CUT, (index + 1) * CUT),
                (index * 3 * SECOND, index * 3 * SECOND + CUT),
            )
        })
        .collect();
    let snapshot = one_source_project(&cuts, alpha2_model::MasterAudio::default());
    let signal = signal(36);
    let start = i64::from(RATE) * 6 / 5;
    let mut renderer = AudioRenderer::new(&snapshot, 1, 4_096).expect("renderer");
    let needs = renderer
        .audio_needs(&snapshot, start, 4_096)
        .expect("needs");
    assert!(needs.len() > 8, "{} separate parts", needs.len());
    let mut store = AudioStore::new(RATE);
    for need in &needs {
        upload_need(&mut store, &signal, need, 0);
    }
    assert_eq!(store.windows("a").len(), needs.len());
    let status = renderer
        .render(&snapshot, start, 4_096, &store, TransportState::Playing)
        .expect("render");
    assert!(
        status.missing_sources.is_empty(),
        "{:?}",
        status.missing_sources
    );
}

/// Mixes two clips cut from different parts of one source across their
/// join, uploading windows the way the timeline editor's supplier does, and
/// compares every sample with a mix read from the whole source.
#[test]
fn a_join_between_two_clips_of_one_source_mixes_like_the_whole_source() {
    use alpha2_audio::TransportState;
    use alpha2_engine::AudioRenderer;

    const BLOCK: usize = 4_096;

    const SECOND: i64 = TICKS_PER_SECOND;
    let rate = usize::try_from(RATE).expect("rate");
    let signal = signal(11);
    let snapshot = one_source_project(
        &[
            ((0, SECOND), (10 * SECOND, 11 * SECOND)),
            ((SECOND, 2 * SECOND), (0, SECOND)),
        ],
        alpha2_model::MasterAudio {
            limiter: None,
            ..alpha2_model::MasterAudio::default()
        },
    );
    let mix = |store: &mut AudioStore, supply: bool| {
        let mut renderer = AudioRenderer::new(&snapshot, 1, BLOCK).expect("renderer");
        let mut output = Vec::new();
        let mut start = 0;
        while start < rate * 2 {
            let frames = BLOCK.min(rate * 2 - start);
            let first = i64::try_from(start).expect("start");
            if supply {
                // Each need is decoded with a two-sample margin and a prefetch, zero-filled
                // past the source's end.
                for need in renderer
                    .audio_needs(&snapshot, first, frames)
                    .expect("needs")
                {
                    upload_need(store, &signal, &need, 2 * i64::from(RATE));
                }
            }
            let status = renderer
                .render(&snapshot, first, frames, store, TransportState::Playing)
                .expect("render");
            assert!(status.missing_sources.is_empty(), "block at {start}");
            output.extend_from_slice(&renderer.channel(0)[..frames]);
            start += frames;
        }
        output
    };

    // The whole source, uploaded as a host that decoded it once would: it declares that the
    // source ends where the upload does.
    let mut whole = AudioStore::new(RATE);
    assert!(whole.upload("a", 0, RATE, MONO, &signal, true).is_ok());
    let expected = mix(&mut whole, false);
    let mut windowed = AudioStore::new(RATE);
    assert_eq!(mix(&mut windowed, true), expected);
    assert!(expected.iter().any(|sample| sample.abs() > 0.1));
}

#[test]
fn a_window_reports_silence_before_sample_zero_and_after_a_declared_end() {
    let mut store = AudioStore::new(RATE);
    assert!(store.upload("a", 0, RATE, MONO, &[1.0; 8], false).is_ok());
    let span = store.block(&source("a"), 0, 8).expect("span");
    assert!(span.starts_source);
    assert!(!span.ends_source, "nothing said where the source ends");
    assert!(!span.covers(0, 10));
    assert!(store.upload("a", 8, RATE, MONO, &[1.0; 4], true).is_ok());
    let span = store.block(&source("a"), 0, 14).expect("span");
    assert!(span.ends_source);
    assert!(span.covers(0, 14));
    store.release("a");
    assert!(store.upload("a", 4, RATE, MONO, &[1.0; 4], false).is_ok());
    let span = store.block(&source("a"), 4, 4).expect("span");
    assert!(
        !span.starts_source && !span.ends_source,
        "release forgets the end"
    );
}

#[test]
fn the_store_meets_the_audio_provider_contract() {
    // The browser provider is the store fed by a host's uploads. Each request is uploaded as
    // the supplied decoders deliver it: the range with a two-sample margin, zeros where the
    // source is silent.
    let contract = alpha2_provider_contract::AudioContract::load();
    let mut store = AudioStore::new(contract.project_sample_rate);
    alpha2_provider_contract::check_audio_provider(
        &mut store,
        &source("surround"),
        &contract,
        |store, request| {
            let start = (request.start_sample - 2).max(0);
            let block = contract.decoded(start, request.end_sample() + 2);
            let planar: Vec<f32> = block.channels.concat();
            assert!(
                store
                    .upload(
                        "surround",
                        block.start_sample,
                        block.sample_rate,
                        block.speakers,
                        &planar,
                        false,
                    )
                    .is_ok()
            );
        },
    );
}

fn source_id(id: &str) -> SourceId {
    source(id)
}

#[test]
fn continuing_a_window_moves_what_it_holds_rarely() {
    // A host decoding in order uploads each piece where the last ended. Copying the window each
    // time would cost tens of ms, so planes grow by doubling and the oldest quarter goes at once.
    let value = |index: i64| f32::from(u16::try_from(index % 50_000).expect("small"));
    let mut store = AudioStore::new(RATE);
    store.byte_budget = 2 * 4_096 * std::mem::size_of::<f32>();
    let limit = store.window_frame_limit(2);
    assert_eq!(limit, 4_096);
    let chunk = 64_usize;
    let uploads = limit / chunk * 8;
    let mut moves = 0;
    let mut starts = Vec::new();
    let mut planes = None;
    for index in 0..uploads {
        let start = i64::try_from(index * chunk).expect("start");
        let end = start + i64::try_from(chunk).expect("chunk");
        let samples: Vec<f32> = (0..2).flat_map(|_| (start..end).map(value)).collect();
        store
            .upload("a", start, RATE, STEREO, &samples, false)
            .expect("upload");
        let windows = store.windows("a");
        assert_eq!(windows.len(), 1, "one window, after {index}");
        let (first, last) = windows[0];
        assert_eq!(last, end);
        assert!(
            usize::try_from(last - first).expect("frames") <= limit,
            "after {index}"
        );
        let block = store.block(&source("a"), first, 1).expect("window").block;
        for plane in &block.channels {
            assert_eq!(plane[0], value(first));
            assert_eq!(plane[plane.len() - 1], value(end - 1));
        }
        assert!(store.retained_bytes() <= store.byte_budget);
        let now: Vec<_> = block
            .channels
            .iter()
            .map(|plane| (plane.as_ptr(), plane.capacity()))
            .collect();
        if planes.as_ref() != Some(&now) {
            moves += 1;
        }
        planes = Some(now);
        if starts.last() != Some(&first) {
            starts.push(first);
        }
    }
    // 64 frames doubling to 4,096: the first allocation and six doublings.
    assert!(
        moves <= 7,
        "the planes moved {moves} times in {uploads} uploads"
    );
    let past_limit = uploads * chunk - limit;
    assert!(
        starts.len() <= 1 + past_limit / (limit / 4),
        "the window slid {} times",
        starts.len() - 1
    );
}

#[test]
fn a_growing_window_takes_no_spare_room_from_another_source() {
    // Doubling reserves room the window has not filled; it reserves only what the budget
    // leaves free, so it never evicts another source's audio for room it may not use.
    let mut store = AudioStore::new(RATE);
    store.byte_budget = 256 * std::mem::size_of::<f32>();
    store
        .upload("other", 0, RATE, MONO, &[0.5; 160], false)
        .expect("upload");
    for start in [0, 32, 64] {
        store
            .upload("a", start, RATE, MONO, &[0.25; 32], false)
            .expect("upload");
        assert_eq!(store.windows("other"), vec![(0, 160)], "after {start}");
        assert!(store.retained_bytes() <= store.byte_budget);
    }
    assert_eq!(store.windows("a"), vec![(0, 96)]);
    // Room the window needs still evicts the least recently uploaded.
    store
        .upload("a", 96, RATE, MONO, &[0.25; 32], false)
        .expect("upload");
    assert_eq!(store.windows("other"), [] as [(i64, i64); 0]);
    assert_eq!(store.windows("a"), vec![(0, 128)]);
}

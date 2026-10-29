//! Windowed audio, audio and video alignment, and loudness normalization.

use super::*;

#[test]
fn windowed_audio_meets_the_audio_provider_contract() {
    // The export's provider: each request is prepared as the export prepares a block, from the need
    // for exactly its samples.
    let contract = alpha2_provider_contract::AudioContract::load();
    let source = SourceId::new("surround").expect("id");
    let mut sources = WindowedAudioSources {
        sample_rate: contract.project_sample_rate,
        paths: HashMap::from([(source.clone(), contract.source_path())]),
        sources: HashMap::new(),
        byte_budget: crate::memory::ByteBudget::exact(16 * 1024 * 1024),
        retained_bytes: 0,
        peak_bytes: 0,
        decoding: std::sync::Arc::default(),
    };
    let cancel = CancelToken::default();
    let rate = i64::from(contract.project_sample_rate);
    let ticks = |sample: i64| MediaTime::from_ticks(sample * alpha2_time::TICKS_PER_SECOND / rate);
    alpha2_provider_contract::check_audio_provider(
        &mut sources,
        &source,
        &contract,
        |sources, request| {
            sources
                .prepare(
                    &[alpha2_engine::AudioNeed {
                        source_id: source.clone(),
                        source_range: TimeRange::new(
                            ticks(request.start_sample),
                            ticks(request.end_sample()),
                        )
                        .expect("range"),
                    }],
                    &cancel,
                )
                .expect("prepare");
        },
    );
}

/// A reversed clip reads its source backward, a block at a time, and the
/// export's windows seek a second before each read that moves back: every
/// window's first frame is placed by a seek. Each read holds the samples a
/// decode straight through gives, as one of the MP4 of the same encode does,
/// also in Matroska and `WebM`, which round each frame's timestamp to the
/// millisecond. Placed by those timestamps, a window would hold its samples up
/// to 48 samples from where the window before it held them, so a reversed clip
/// of AAC in Matroska or Opus in `WebM` would jump at every join, and an Opus
/// window would begin 2e-2 off a decode straight through (its decoder converges
/// slowly from a 100 ms pre-roll).
#[test]
fn a_source_read_backward_through_windows_gives_its_straight_samples() {
    let cancel = CancelToken::default();
    let source = SourceId::new("tones").expect("id");
    let ticks =
        |sample: i64| MediaTime::from_ticks(sample * alpha2_time::TICKS_PER_SECOND / 48_000);
    for name in [
        "audio-seek-aac.mkv",
        "audio-seek-aac.mp4",
        "audio-seek-opus.webm",
        "audio-seek-libopus.webm",
    ] {
        let path = corpus(name);
        let mut decoder = alpha2_native::NativeAudioDecoder::open(&path, 48_000).expect("open");
        let (mut first, mut straight) = (None, Vec::new());
        while let alpha2_media::DecodeEvent::Audio(block) =
            decoder.decode_next(&cancel).expect("decode")
        {
            first.get_or_insert(block.start_sample);
            straight.extend_from_slice(&block.channels[0]);
        }
        let first = first.expect("audio");
        let mut sources = WindowedAudioSources {
            sample_rate: 48_000,
            paths: HashMap::from([(source.clone(), path)]),
            sources: HashMap::new(),
            byte_budget: crate::memory::ByteBudget::exact(64 * 1024 * 1024),
            retained_bytes: 0,
            peak_bytes: 0,
            decoding: std::sync::Arc::default(),
        };
        let mut worst = (0.0_f32, 0);
        // 1.5 s down to 0.25 s in 1024-sample reads, as the engine reads a reversed clip.
        let mut end = 72_000_i64;
        while end > 12_000 {
            let start = end - 1_024;
            sources
                .prepare(
                    &[alpha2_engine::AudioNeed {
                        source_id: source.clone(),
                        source_range: TimeRange::new(ticks(start), ticks(end)).expect("range"),
                    }],
                    &cancel,
                )
                .expect("prepare");
            let span = sources
                .block(&source, start, 1_024)
                .expect("a window holds the read");
            let block = span.block;
            for sample in start..end {
                let held = block.channels[0]
                    [usize::try_from(sample - block.start_sample).expect("inside")];
                let error =
                    (held - straight[usize::try_from(sample - first).expect("inside")]).abs();
                if error > worst.0 {
                    worst = (error, sample);
                }
            }
            end = start;
        }
        assert!(
            worst.0 <= 1e-4,
            "{name}: sample {} is {} from a decode straight through",
            worst.1,
            worst.0
        );
    }
}

/// A mix whose audio fits the window budget in one channel but not in the
/// source's four is refused before the window decodes past its first block.
/// Checked against one channel first, the window would decode every channel, up
/// to the channel count times the budget, before the check against all of them.
#[test]
fn an_audio_window_is_refused_before_it_decodes_past_its_budget() {
    let source = SourceId::new("quad").expect("id");
    let budget = 40_000;
    let mut sources = WindowedAudioSources {
        sample_rate: 48_000,
        paths: HashMap::from([(source.clone(), corpus("audio-quad.wav"))]),
        sources: HashMap::new(),
        byte_budget: crate::memory::ByteBudget::exact(budget),
        retained_bytes: 0,
        peak_bytes: 0,
        decoding: std::sync::Arc::default(),
    };
    let need = alpha2_engine::AudioNeed {
        source_id: source.clone(),
        // All 0.1 s of it: about 19 kB in one channel, 77 kB in four.
        source_range: TimeRange::new(MediaTime::ZERO, MediaTime::from_ticks(12_000))
            .expect("range"),
    };
    let cancel = CancelToken::default();
    let error = sources
        .prepare(std::slice::from_ref(&need), &cancel)
        .expect_err("four channels do not fit");
    assert_eq!(HostError::classify(error.clone()).code, "RESOURCE_LIMIT");
    let held = sources
        .sources
        .values()
        .flatten()
        .filter_map(|state| state.window.as_ref())
        .map(crate::audio::audio_block_bytes)
        .sum::<u64>();
    assert!(held <= budget, "{held} bytes held after: {error}");
    sources.byte_budget = crate::memory::ByteBudget::exact(1 << 20);
    sources
        .prepare(&[need], &cancel)
        .expect("a budget that holds every channel decodes");
    assert!(sources.retained_bytes <= sources.byte_budget.bytes);
}

#[test]
fn windowed_audio_preserves_silence_before_the_first_timestamp() {
    for (name, first) in [("edit-list.mp4", 4_944), ("audio-mpegts.ts", 67_200)] {
        let source = SourceId::new("audio").expect("id");
        let mut sources = WindowedAudioSources {
            sample_rate: 48_000,
            paths: HashMap::from([(source.clone(), corpus(name))]),
            sources: HashMap::new(),
            byte_budget: crate::memory::ByteBudget::exact(128 * 1024),
            retained_bytes: 0,
            peak_bytes: 0,
            decoding: std::sync::Arc::default(),
        };
        let mut decoder =
            alpha2_native::NativeAudioDecoder::open(corpus(name), 48_000).expect("decoder");
        let cancel = CancelToken::default();
        let alpha2_media::DecodeEvent::Audio(original) =
            decoder.decode_next(&cancel).expect("decode")
        else {
            panic!("audio");
        };
        // Both a wholly silent request and one crossing the first timestamp must return known
        // media, not trigger a missing-source retry.
        for (start, end) in [
            (0, 1_200),
            (1_200, 2_400),
            (first * 5 / 2 - 600, first * 5 / 2 + 1_200),
        ] {
            sources
                .prepare(
                    &[alpha2_engine::AudioNeed {
                        source_id: source.clone(),
                        source_range: TimeRange::new(
                            MediaTime::from_ticks(start),
                            MediaTime::from_ticks(end),
                        )
                        .expect("range"),
                    }],
                    &cancel,
                )
                .expect("prepare");
            let span = sources
                .block(&source, start * 2 / 5, 1_024)
                .expect("known initial silence");
            assert!(span.starts_source, "{name}: the gap before is silence");
            let block = span.block;
            assert_eq!(block.start_sample, first, "{name}");
            assert_eq!(
                &block.channels[0][..original.frames()],
                &original.channels[0],
                "{name}"
            );
            assert!(
                sources.retained_bytes <= sources.byte_budget.bytes,
                "do not allocate the initial gap"
            );
        }
    }
}

#[test]
fn windowed_audio_merges_ranges_and_evicts_consumed_samples() {
    let source = SourceId::new("audio").expect("source ID");
    let mut paths = HashMap::new();
    paths.insert(source.clone(), corpus("audio-only-aac-priming.m4a"));
    let mut sources = WindowedAudioSources {
        sample_rate: 48_000,
        paths,
        sources: HashMap::new(),
        byte_budget: crate::memory::ByteBudget::exact(1024 * 1024),
        retained_bytes: 0,
        peak_bytes: 0,
        decoding: std::sync::Arc::default(),
    };
    let need = |start, end| alpha2_engine::AudioNeed {
        source_id: source.clone(),
        source_range: TimeRange::new(MediaTime::from_ticks(start), MediaTime::from_ticks(end))
            .expect("valid audio range"),
    };
    let cancel = CancelToken::default();

    sources
        .prepare(&[need(0, 1_200), need(2_400, 3_600)], &cancel)
        .expect("disjoint requirements for one source each get a window");
    assert!(sources.block(&source, 0, 256).is_some());
    assert!(sources.block(&source, 960, 256).is_some());
    let retained_before = sources.retained_bytes;

    sources
        .prepare(&[need(6_000, 7_200)], &cancel)
        .expect("sequential requirement reuses the decoder window");
    assert!(sources.block(&source, 2400, 256).is_some());
    assert!(sources.retained_bytes < retained_before);
    assert!(sources.peak_bytes() <= sources.byte_budget.bytes);
}

#[test]
fn windowed_audio_reading_backward_keeps_what_the_next_reads_want() {
    // A reversed clip reads a little earlier each block. After the first backward read, a window
    // reaches far enough back that the next reads are already decoded.
    let source = SourceId::new("audio").expect("source ID");
    let mut paths = HashMap::new();
    paths.insert(source.clone(), corpus("audio-only-aac-priming.m4a"));
    let mut sources = WindowedAudioSources {
        sample_rate: 48_000,
        paths,
        sources: HashMap::new(),
        byte_budget: crate::memory::ByteBudget::exact(1024 * 1024),
        retained_bytes: 0,
        peak_bytes: 0,
        decoding: std::sync::Arc::default(),
    };
    let need = |start, end| alpha2_engine::AudioNeed {
        source_id: source.clone(),
        source_range: TimeRange::new(MediaTime::from_ticks(start), MediaTime::from_ticks(end))
            .expect("valid audio range"),
    };
    let cancel = CancelToken::default();
    let window_start = |sources: &WindowedAudioSources| {
        sources.sources[&source][0]
            .window
            .as_ref()
            .expect("a window")
            .start_sample
    };
    sources
        .prepare(&[need(36_000, 48_000)], &cancel)
        .expect("first read");
    sources
        .prepare(&[need(30_000, 36_000)], &cancel)
        .expect("backward read");
    let reached = window_start(&sources);
    assert_eq!(reached, 0, "the backward read reaches the source start");
    for step in (0..5).rev() {
        let start = step * 6_000;
        sources
            .prepare(&[need(start, start + 6_000)], &cancel)
            .expect("backward read");
        assert_eq!(window_start(&sources), reached, "read at {start} continued");
        let sample = start * 48_000 / 120_000;
        assert!(sources.block(&source, sample, 256).is_some());
    }
    assert!(sources.peak_bytes() <= sources.byte_budget.bytes);
}

#[test]
fn windowed_audio_keeps_two_clips_of_one_source_apart() {
    // Two clips of one media meet on the timeline; one window over both would hold the whole 0.4 s
    // source.
    let source = SourceId::new("audio").expect("source ID");
    let mut paths = HashMap::new();
    paths.insert(source.clone(), corpus("audio-only-aac-priming.m4a"));
    let mut sources = WindowedAudioSources {
        sample_rate: 48_000,
        paths,
        sources: HashMap::new(),
        byte_budget: crate::memory::ByteBudget::exact(32 * 1024),
        retained_bytes: 0,
        peak_bytes: 0,
        decoding: std::sync::Arc::default(),
    };
    let need = |start, end| alpha2_engine::AudioNeed {
        source_id: source.clone(),
        source_range: TimeRange::new(MediaTime::from_ticks(start), MediaTime::from_ticks(end))
            .expect("valid audio range"),
    };
    let cancel = CancelToken::default();

    sources
        .prepare(&[need(42_000, 44_400)], &cancel)
        .expect("first clip's tail");
    sources
        .prepare(&[need(44_400, 45_600), need(0, 1_200)], &cancel)
        .expect("the join decodes two windows, not the range between them");
    assert_eq!(sources.sources[&source].len(), 2);
    assert!(sources.block(&source, 17760, 256).is_some());
    assert!(sources.block(&source, 0, 256).is_some());
    assert!(sources.retained_bytes <= sources.byte_budget.bytes);

    sources
        .prepare(&[need(1_200, 2_400)], &cancel)
        .expect("second clip plays on");
    assert_eq!(sources.sources[&source].len(), 1);
    assert!(sources.block(&source, 480, 256).is_some());
}

#[test]
fn windowed_audio_answers_a_read_from_the_window_that_covers_it() {
    // A clip twice as fast reads ahead of another of the same media; its reads must get their own
    // window, not the slower clip's window that only holds its start.
    let source = SourceId::new("audio").expect("source ID");
    let mut paths = HashMap::new();
    paths.insert(source.clone(), corpus("recovery-aac.m4a"));
    let mut sources = WindowedAudioSources {
        sample_rate: 48_000,
        paths,
        sources: HashMap::new(),
        byte_budget: crate::memory::ByteBudget::exact(1024 * 1024),
        retained_bytes: 0,
        peak_bytes: 0,
        decoding: std::sync::Arc::default(),
    };
    // Samples to ticks at 48 kHz.
    let need = |start: i64, end: i64| alpha2_engine::AudioNeed {
        source_id: source.clone(),
        source_range: TimeRange::new(
            MediaTime::from_ticks(start * 5 / 2),
            MediaTime::from_ticks(end * 5 / 2),
        )
        .expect("valid audio range"),
    };
    let cancel = CancelToken::default();
    for needs in [
        vec![need(0, 8_194)],
        vec![need(4_096, 16_386)],
        vec![need(8_192, 12_290), need(16_384, 24_578)],
    ] {
        sources.prepare(&needs, &cancel).expect("windows");
    }
    assert_eq!(sources.sources[&source].len(), 2);
    let span = sources
        .block(&source, 16_384, 8_194)
        .expect("a window holds the read's start");
    assert!(
        span.covers(16_384, 24_578),
        "the read got {}..{}",
        span.block.start_sample,
        span.block.end_sample()
    );
}

#[test]
fn audio_sample_ranges_merge_only_where_they_meet() {
    assert_eq!(
        merge_sample_ranges(vec![(90, 100), (0, 10), (10, 20), (15, 18)]),
        vec![(0, 20), (90, 100)]
    );
}

#[test]
fn windowed_audio_returns_a_short_block_at_source_eof() {
    let source = SourceId::new("audio").expect("source ID");
    let mut paths = HashMap::new();
    paths.insert(source.clone(), corpus("audio-only-aac-priming.m4a"));
    let mut sources = WindowedAudioSources {
        sample_rate: 48_000,
        paths,
        sources: HashMap::new(),
        byte_budget: crate::memory::ByteBudget::exact(1024 * 1024),
        retained_bytes: 0,
        peak_bytes: 0,
        decoding: std::sync::Arc::default(),
    };
    let whole_source = alpha2_engine::AudioNeed {
        source_id: source.clone(),
        source_range: TimeRange::new(MediaTime::ZERO, MediaTime::from_ticks(48_000))
            .expect("valid audio range"),
    };

    sources
        .prepare(&[whole_source], &CancelToken::default())
        .expect("decode through source EOF");

    assert!(
        sources.block(&source, 0, 1_000_000).is_some(),
        "the provider contract permits fewer frames at source EOF"
    );
}

#[test]
fn audio_eof_padding_stops_at_the_declared_container_end() {
    let mut block = AudioBlock {
        start_sample: 0,
        sample_rate: 48_000,
        speakers: alpha2_media::SpeakerLayout::MONO,
        channels: vec![vec![1.0; 4_700]],
    };

    pad_audio_window_to_declared_end(
        &mut block,
        6_000,
        Some(MediaTime::from_ticks(12_000)),
        48_000,
    );

    assert_eq!(block.channels[0].len(), 4_802);
    assert_eq!(block.channels[0][4_699].to_bits(), 1.0_f32.to_bits());
    assert!(
        block.channels[0][4_700..]
            .iter()
            .all(|sample| sample.to_bits() == 0.0_f32.to_bits())
    );
}

/// Exports a flash and a tone and measures where the audio sits against the
/// video, in audio samples.
///
/// The flash is an image clip starting on a frame boundary, so the first
/// bright output frame says which timeline time the video's zero is. The tone
/// is a low sine whose phase is zero at timeline zero, so its phase in the
/// output says which timeline time the audio's zero is. Their difference is
/// the A/V offset; a low frequency keeps it unambiguous across a whole frame.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]
fn exported_av_offset_samples(
    host: &mut Host,
    directory: &Path,
    rate: (u32, u32),
    sample_rate: u32,
    start_ticks: i64,
    (codec, width, height): (NativeVideoCodec, u32, u32),
) -> (f64, Value, PathBuf) {
    const TONE_HZ: f64 = 10.0;
    const END_TICKS: i64 = 2 * alpha2_time::TICKS_PER_SECOND;
    let frame_rate = alpha2_time::FrameRate::new(rate.0, rate.1).expect("rate");
    let flash_ticks = frame_rate.frame_start(30).expect("frame 30").ticks();
    let tone_path = directory.join(format!("tone-{sample_rate}.wav"));
    let tone = (0..sample_rate as usize * 2)
        .map(|index| {
            (std::f64::consts::TAU * TONE_HZ * index as f64 / f64::from(sample_rate)).sin() as f32
                * 0.5
        })
        .collect::<Vec<_>>();
    write_float_wav(&tone_path, sample_rate, &tone);
    let project = json!({
        "schema_version": 1,
        "id": "av-origin",
        "name": "A/V origin",
        "settings": {
            "width": width,
            "height": height,
            "frame_rate": {"num": rate.0, "den": rate.1},
            "sample_rate": sample_rate
        },
        "sources": [
            {"id": "flash", "locator": "fixture://flash", "kind": "image"},
            {"id": "tone", "locator": "fixture://tone", "kind": "audio"}
        ],
        "timeline": {"tracks": [
            {"id": "video", "name": "Video", "kind": "video", "clips": [{
                "id": "flash-clip",
                "source_id": "flash",
                "timeline_range": {"start": flash_ticks, "end": END_TICKS},
                "source_range": {"start": 0, "end": END_TICKS - flash_ticks}
            }]},
            {"id": "audio", "name": "Audio", "kind": "audio", "clips": [{
                "id": "tone-clip",
                "source_id": "tone",
                "timeline_range": {"start": 0, "end": END_TICKS},
                "source_range": {"start": 0, "end": END_TICKS}
            }]}
        ]}
    });
    let assets = HashMap::from([
        (
            "flash".to_owned(),
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/corpus/cross_topology_parity.png"),
        ),
        ("tone".to_owned(), tone_path),
    ]);
    let output = directory.join(format!(
        "av-{}-{}-{sample_rate}-{start_ticks}.mp4",
        codec.as_str(),
        rate.1
    ));
    let result = host
        .export(
            &project.to_string(),
            &assets,
            &HashMap::new(),
            Vec::new(),
            start_ticks,
            END_TICKS,
            RateRequest {
                num: rate.0,
                den: rate.1,
            },
            true,
            crate::protocol::ResolvedFormat {
                container: alpha2_media::Container::Mp4,
                video: codec,
                audio: NativeAudioCodec::Aac,
            },
            None,
            VideoQualityRequest::Level(QualityLevel::VeryHigh),
            AudioQualityRequest::Level(QualityLevel::VeryHigh),
            None,
            &output,
            &mut |_| Ok(()),
        )
        .expect("export");

    let mut video = alpha2_native::NativeDecoder::open(&output).expect("open video");
    let cancel = CancelToken::default();
    let flash_output_ticks = loop {
        match video.decode_next(&cancel).expect("decode video") {
            alpha2_media::DecodeEvent::Video(frame) => {
                // Visible samples only: a decoder's rows may carry padding.
                let luma = &frame.planes[0];
                let visible = luma
                    .bytes
                    .chunks(luma.stride as usize)
                    .flat_map(|row| &row[..luma.width as usize]);
                let mean = visible.clone().map(|&value| f64::from(value)).sum::<f64>()
                    / visible.count() as f64;
                if mean > 40.0 {
                    break frame.pts.ticks();
                }
            }
            alpha2_media::DecodeEvent::EndOfStream => panic!("no flash in {output:?}"),
            _ => {}
        }
    };
    let video_origin_ticks = flash_ticks - flash_output_ticks;

    let mut audio =
        alpha2_native::NativeAudioDecoder::open(&output, sample_rate).expect("open audio");
    let mut samples = Vec::new();
    loop {
        match audio.decode_next(&cancel).expect("decode audio") {
            alpha2_media::DecodeEvent::Audio(block) => {
                samples.extend_from_slice(&block.channels[0]);
            }
            alpha2_media::DecodeEvent::EndOfStream => break,
            _ => {}
        }
    }
    // Whole periods from the settled middle of the output, away from the encoder's first and last
    // frames.
    let omega = std::f64::consts::TAU * TONE_HZ / f64::from(sample_rate);
    let period = f64::from(sample_rate) / TONE_HZ;
    let first = sample_rate as usize / 4;
    let count = (period * 15.0).round() as usize;
    let (mut in_phase, mut quadrature) = (0.0, 0.0);
    for (offset, &sample) in samples[first..first + count].iter().enumerate() {
        let position = (first + offset) as f64 * omega;
        in_phase += f64::from(sample) * position.sin();
        quadrature += f64::from(sample) * position.cos();
    }
    // y[n] = sin(omega * (n + origin)), so its phase against sin(omega * n) is omega * origin.
    let audio_origin_samples = quadrature.atan2(in_phase) / omega;
    let video_origin_samples =
        video_origin_ticks as f64 * f64::from(sample_rate) / alpha2_time::TICKS_PER_SECOND as f64;
    let offset = (audio_origin_samples - video_origin_samples).rem_euclid(period);
    (
        if offset > period / 2.0 {
            offset - period
        } else {
            offset
        },
        result,
        output,
    )
}

/// The cases each `audio_and_video_start_together_*` test checks on its
/// export path: frame rate, sample rate, and export start in ticks. A
/// start inside the first frame snaps to zero; one past it moves the
/// output's zero, which each path maps its frames and audio from.
const AV_ORIGIN_MATRIX: [((u32, u32), u32, i64); 6] = [
    ((30, 1), 48_000, 0),
    ((30, 1), 48_000, 2_000),
    ((30, 1), 48_000, 3_999),
    ((30, 1), 48_000, 6_000),
    ((30_000, 1_001), 44_100, 2_000),
    ((30_000, 1_001), 44_100, 10_000),
];

#[test]
fn audio_and_video_start_together_from_an_unaligned_export_start() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("av-origin");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    for (rate, sample_rate, start_ticks) in AV_ORIGIN_MATRIX {
        let (offset, result, _) = exported_av_offset_samples(
            &mut host,
            &directory,
            rate,
            sample_rate,
            start_ticks,
            (NativeVideoCodec::Mpeg4Part2, 64, 48),
        );
        assert!(
            offset.abs() <= 1.0,
            "{rate:?} fps, {sample_rate} Hz, start {start_ticks}: audio is {offset:.1} samples \
             off the video"
        );
        let frame_rate = alpha2_time::FrameRate::new(rate.0, rate.1).expect("rate");
        let snapped = frame_rate
            .frame_start(
                frame_rate
                    .frame_at_or_before(MediaTime::from_ticks(start_ticks))
                    .expect("frame"),
            )
            .expect("start");
        assert_eq!(
            result["effectiveRange"]["startTicks"],
            json!(snapped.ticks()),
            "{result}"
        );
    }
    std::fs::remove_dir_all(&directory).ok();
}

/// The GPU-resident path has its own audio/video interleave and maps each
/// frame's time to the output's itself, so the same alignment is checked
/// there: a 1-sample bound from an unaligned start, at 30000/1001 and
/// 44.1 kHz too, and the audio spanning what the readback path's output of
/// the same export spans.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode"
))]
#[test]
fn audio_and_video_start_together_on_the_gpu_resident_path() {
    let _gpu = gpu_test_lock();
    let Some((mut gpu, _)) = gpu_output_host(VideoDecodePolicy::Software) else {
        return;
    };
    let mut readback = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("av-origin-gpu");
    for (rate, sample_rate, start_ticks) in AV_ORIGIN_MATRIX {
        let case = format!("{rate:?} fps, {sample_rate} Hz, start {start_ticks}");
        let (offset, result, output) = exported_av_offset_samples(
            &mut gpu,
            &directory,
            rate,
            sample_rate,
            start_ticks,
            (NativeVideoCodec::H264, 256, 144),
        );
        assert_eq!(
            result["videoPipeline"],
            json!("gpu-resident"),
            "{case}: {result}"
        );
        assert!(
            offset.abs() <= 1.0,
            "{case}: audio is {offset:.1} samples off the video"
        );
        let (_, reference, reference_output) = exported_av_offset_samples(
            &mut readback,
            &directory,
            rate,
            sample_rate,
            start_ticks,
            (NativeVideoCodec::Mpeg4Part2, 256, 144),
        );
        assert_eq!(reference["videoPipeline"], json!("readback"), "{case}");
        let (_, audio) = stream_spans(&output);
        let (_, expected) = stream_spans(&reference_output);
        assert_eq!(audio, expected, "{case}: the audio spans differ");
    }
    std::fs::remove_dir_all(&directory).ok();
}

/// A 1 kHz sine at -30 dBFS, mono, 6 s at 48 kHz, written to `directory`: the pan law is unity
/// at the centre, so both output channels carry it at -30 dBFS, -30 LUFS.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn write_quiet_tone(directory: &Path) -> PathBuf {
    let sample_rate = 48_000_u32;
    let tone_path = directory.join("tone.wav");
    let amplitude = 10_f64.powf(-30.0 / 20.0);
    let tone = (0..sample_rate as usize * 6)
        .map(|index| {
            (amplitude
                * (std::f64::consts::TAU * 1_000.0 * index as f64 / f64::from(sample_rate)).sin())
                as f32
        })
        .collect::<Vec<_>>();
    write_float_wav(&tone_path, sample_rate, &tone);
    tone_path
}

/// Exports `project` over `[0, end_ticks)` with `assets`, normalized to `target` LUFS under a
/// -1 dBTP ceiling, as `codec` to `output`. Returns the result and the file's audio, decoded and
/// measured again.
fn export_normalized(
    host: &mut Host,
    project: &Value,
    assets: &HashMap<String, PathBuf>,
    (end_ticks, target): (i64, f64),
    codec: NativeVideoCodec,
    output: &Path,
) -> (Value, alpha2_audio::LoudnessReport) {
    let result = host
        .export(
            &project.to_string(),
            assets,
            &HashMap::new(),
            Vec::new(),
            0,
            end_ticks,
            RateRequest { num: 30, den: 1 },
            true,
            crate::protocol::ResolvedFormat {
                container: alpha2_media::Container::Mp4,
                video: codec,
                audio: NativeAudioCodec::Aac,
            },
            None,
            VideoQualityRequest::Level(QualityLevel::High),
            AudioQualityRequest::Level(QualityLevel::VeryHigh),
            Some(LoudnessRequest::new(target, -1.0).expect("request")),
            output,
            &mut |_| Ok(()),
        )
        .expect("export");
    (result, measure_file_loudness(output, 48_000))
}

/// Exports `project` (an image and the `tone` clip, `[0, end_ticks)`) normalized to `target` LUFS
/// on the readback path, with the tone [`write_quiet_tone`]'s. Returns the result's `loudness` and
/// the file's audio, decoded and measured again.
fn export_normalized_tone(
    name: &str,
    project: &Value,
    end_ticks: i64,
    target: f64,
) -> (Value, alpha2_audio::LoudnessReport) {
    let directory = scratch_directory(name);
    let assets = HashMap::from([
        (
            "image".to_owned(),
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/corpus/cross_topology_parity.png"),
        ),
        ("tone".to_owned(), write_quiet_tone(&directory)),
    ]);
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let (result, report) = export_normalized(
        &mut host,
        project,
        &assets,
        (end_ticks, target),
        NativeVideoCodec::Mpeg4Part2,
        &directory.join("normalized.mp4"),
    );
    std::fs::remove_dir_all(&directory).ok();
    (result["loudness"].clone(), report)
}

/// The planned loudness of a normalized export's `loudness`, after checking that what the export
/// put out (`output`) and the file, decoded and measured again, measure it too.
fn assert_output_as_planned(loudness: &Value, file: &alpha2_audio::LoudnessReport) -> f64 {
    let applied = loudness["applied"]["integratedLufs"]
        .as_f64()
        .expect("planned");
    let output = loudness["output"]["integratedLufs"]
        .as_f64()
        .expect("the output measured");
    assert!(
        (output - applied).abs() <= 0.05,
        "the output measures {output} LUFS against the planned {applied}: {loudness}"
    );
    let exported = file.integrated_lufs.expect("loud enough");
    assert!(
        (exported - output).abs() <= 0.5,
        "the file measures {exported} LUFS, its mix {output}: {loudness}"
    );
    applied
}

/// A normalized export, decoded and measured again, sits at its target.
#[test]
fn a_normalized_export_measures_at_its_target_loudness() {
    let _gpu = gpu_test_lock();
    let end_ticks = 6 * alpha2_time::TICKS_PER_SECOND;
    let project = image_and_tone_project(end_ticks, 48_000);
    let target = -16.0;
    let (loudness, report) = export_normalized_tone("loudness", &project, end_ticks, target);
    let measured = loudness["measured"]["integratedLufs"]
        .as_f64()
        .expect("measured");
    assert!((measured + 30.0).abs() < 0.1, "{loudness}");
    assert_eq!(
        loudness["applied"]["peakLimited"],
        json!(false),
        "{loudness}"
    );
    assert!(
        (loudness["applied"]["integratedLufs"]
            .as_f64()
            .expect("planned")
            - target)
            .abs()
            < 1e-9
    );

    assert_eq!(loudness["limiterThreshold"], json!(1.0), "{loudness}");
    assert_eq!(
        loudness["applied"]["limiterLimited"],
        json!(false),
        "{loudness}"
    );
    assert_output_as_planned(&loudness, &report);

    let exported = report.integrated_lufs.expect("loud enough");
    assert!(
        (exported - target).abs() <= 0.5,
        "the exported file measures {exported} LUFS against {target}"
    );
    let peak = report.true_peak_dbtp.expect("a peak");
    assert!(peak <= -1.0 + 0.5, "{peak} dBTP");
}

/// A master limiter below where the target would put the peaks bounds the gain, and the result
/// reports what the file holds: a -30 LUFS tone under a 0.1 (-20 dBFS) limiter, asked for
/// -16 LUFS, is exported at about -20 LUFS and reported there, not at the target the limiter would
/// have cut back.
#[test]
fn a_normalized_export_under_a_low_limiter_reports_what_the_file_measures() {
    let _gpu = gpu_test_lock();
    let end_ticks = 6 * alpha2_time::TICKS_PER_SECOND;
    let mut project = image_and_tone_project(end_ticks, 48_000);
    project["audio"] = json!({"limiter": {"threshold": 0.1, "release_ms": 50}});
    let (loudness, report) = export_normalized_tone("loudness-limiter", &project, end_ticks, -16.0);
    let exported = report.integrated_lufs.expect("loud enough");
    let applied = loudness["applied"]["integratedLufs"]
        .as_f64()
        .expect("planned");
    assert!(
        (exported - applied).abs() <= 0.5,
        "the exported file measures {exported} LUFS, the result reports {applied}: {loudness}"
    );
    assert_eq!(
        loudness["applied"]["peakLimited"],
        json!(true),
        "{loudness}"
    );
    assert_eq!(
        loudness["applied"]["limiterLimited"],
        json!(true),
        "{loudness}"
    );
    assert_eq!(
        loudness["limiterThreshold"],
        json!(f64::from(0.1_f32)),
        "{loudness}"
    );
    assert_output_as_planned(&loudness, &report);
    let peak = loudness["output"]["samplePeakDbfs"]
        .as_f64()
        .expect("a peak");
    assert!(peak < -20.0, "{peak} dBFS under a -20 dBFS limiter");
}

/// The GPU-resident and packet-copy paths mix their audio with renderers of
/// their own, and report what those put out as the readback path does.
#[test]
fn a_normalized_export_reports_its_output_on_every_path() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("loudness-paths");
    let limiter = json!({"limiter": {"threshold": 0.1, "release_ms": 50}});

    #[cfg(all(
        any(target_os = "linux", target_os = "macos", target_os = "windows"),
        feature = "hwencode"
    ))]
    if let Some((mut gpu, _)) = gpu_output_host(VideoDecodePolicy::Software) {
        let end_ticks = 6 * alpha2_time::TICKS_PER_SECOND;
        let mut project = image_and_tone_project(end_ticks, 48_000);
        project["settings"]["width"] = json!(256);
        project["settings"]["height"] = json!(144);
        project["audio"] = limiter.clone();
        let assets = HashMap::from([
            (
                "image".to_owned(),
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../tests/corpus/cross_topology_parity.png"),
            ),
            ("tone".to_owned(), write_quiet_tone(&directory)),
        ]);
        let (result, file) = export_normalized(
            &mut gpu,
            &project,
            &assets,
            (end_ticks, -16.0),
            NativeVideoCodec::H264,
            &directory.join("gpu.mp4"),
        );
        assert_eq!(result["videoPipeline"], json!("gpu-resident"), "{result}");
        let loudness = &result["loudness"];
        assert_eq!(
            loudness["applied"]["limiterLimited"],
            json!(true),
            "{loudness}"
        );
        assert_output_as_planned(loudness, &file);
    }

    let second = alpha2_time::TICKS_PER_SECOND;
    let project = json!({
        "schema_version": 1, "id": "copy", "name": "Copy",
        "settings": {"width": 128, "height": 96, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
        "sources": [{"id": "video", "locator": "fixture://video", "kind": "video", "has_audio": true}],
        "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
            "id": "clip", "source_id": "video",
            "timeline_range": {"start": 0, "end": 2 * second},
            "source_range": {"start": 0, "end": 2 * second}
        }]}]},
        "audio": limiter
    });
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    host.strategy = crate::config::ExportStrategy::RequireCopy;
    let (result, file) = export_normalized(
        &mut host,
        &project,
        &HashMap::from([("video".to_owned(), corpus("h264-bt709-copy.mp4"))]),
        (2 * second, -16.0),
        NativeVideoCodec::H264,
        &directory.join("copy.mp4"),
    );
    assert_eq!(result["videoPipeline"], json!("packet-copy"), "{result}");
    assert_eq!(result["audioStrategy"], json!("encode"), "{result}");
    assert_output_as_planned(&result["loudness"], &file);
    std::fs::remove_dir_all(&directory).ok();
}

/// Decodes a file's audio at `sample_rate` and measures its loudness.
fn measure_file_loudness(path: &Path, sample_rate: u32) -> alpha2_audio::LoudnessReport {
    let mut audio = alpha2_native::NativeAudioDecoder::open(path, sample_rate).expect("open audio");
    let mut meter = alpha2_audio::LoudnessMeter::new(sample_rate, 2).expect("meter");
    let cancel = CancelToken::default();
    loop {
        match audio.decode_next(&cancel).expect("decode audio") {
            alpha2_media::DecodeEvent::Audio(block) => {
                let planes = block.channels.iter().map(Vec::as_slice).collect::<Vec<_>>();
                meter.push(&planes).expect("stereo");
            }
            alpha2_media::DecodeEvent::EndOfStream => break,
            _ => {}
        }
    }
    meter.report()
}

#[test]
fn loudness_requests_are_checked_and_need_audio() {
    let request = |loudness: Value, include_audio: bool| {
        json!({
            "protocolVersion": 1, "operation": "export", "requestId": "r",
            "project": {}, "assets": {}, "startTicks": 0, "endTicks": 1,
            "frameRate": {"num": 30, "den": 1}, "includeAudio": include_audio,
            "loudness": loudness, "output": "/tmp/x.mp4"
        })
        .to_string()
    };
    let error = |loudness: Value, include_audio: bool| match parse_request(&request(
        loudness,
        include_audio,
    )) {
        Ok(_) => panic!("the request was accepted"),
        Err(error) => error,
    };
    let good = json!({"targetLufs": -14, "truePeakCeilingDbtp": -1});
    assert!(parse_request(&request(good.clone(), true)).is_ok());
    for (loudness, field) in [
        (
            json!({"targetLufs": 3, "truePeakCeilingDbtp": -1}),
            "targetLufs",
        ),
        (
            json!({"targetLufs": -14, "truePeakCeilingDbtp": 1}),
            "truePeakCeilingDbtp",
        ),
        (json!({"targetLufs": -14}), "truePeakCeilingDbtp"),
        (
            json!({"targetLufs": -14, "truePeakCeilingDbtp": -1, "gate": 1}),
            "gate",
        ),
    ] {
        let refused = error(loudness, true);
        assert_eq!(refused.code, "INVALID_INPUT");
        assert!(refused.message.contains(field), "{}", refused.message);
    }
    assert!(error(good, false).message.contains("includeAudio"));
}

/// A `loudness` request whose project hears nothing is the caller's input on
/// every path, refused before any output exists: not failed late by the render
/// paths as `ALPHA2_ERROR` (a retryable `UNKNOWN`), nor dropped by the copy
/// path.
#[test]
fn loudness_for_a_project_that_hears_nothing_is_refused_on_every_path() {
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("loudness-silent");
    let second = alpha2_time::TICKS_PER_SECOND;
    let project = |width: u32, height: u32| {
        json!({
            "schema_version": 1, "id": "silent", "name": "Silent",
            "settings": {"width": width, "height": height, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
            "sources": [{"id": "video", "locator": "fixture://video", "kind": "video", "has_audio": false}],
            "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
                "id": "clip", "source_id": "video",
                "timeline_range": {"start": 0, "end": second},
                "source_range": {"start": 0, "end": second}
            }]}]}
        })
    };
    for (name, project, codec, strategy) in [
        ("readback.mp4", project(128, 96), "mpeg4-part2", "render"),
        ("gpu.mp4", project(640, 360), "h264", "render"),
        ("copy.mp4", project(128, 96), "h264", "prefer-copy"),
    ] {
        let output = directory.join(name);
        let request = parse_request(
            &json!({
                "protocolVersion": 1,
                "operation": "export",
                "requestId": name,
                "project": project,
                "assets": {"video": corpus("h264-bt709-copy.mp4")},
                "startTicks": 0,
                "endTicks": second,
                "frameRate": {"num": 30, "den": 1},
                "includeAudio": true,
                "videoCodec": codec,
                "strategy": strategy,
                "loudness": {"targetLufs": -14, "truePeakCeilingDbtp": -1},
                "output": output,
            })
            .to_string(),
        )
        .expect("request");
        let error = host
            .handle(request, &mut |_| Ok(()))
            .expect_err("loudness without an audible source");
        assert_eq!(error.code, "INVALID_INPUT", "{name}: {}", error.message);
        assert!(
            error.message.contains("audible"),
            "{name}: {}",
            error.message
        );
        let failure = error.failure.expect("failure report");
        assert_eq!(failure["phase"], json!("prepare"), "{name}: {failure}");
        assert!(!output.exists(), "{name}: no output was opened");
    }
    std::fs::remove_dir_all(&directory).ok();
}

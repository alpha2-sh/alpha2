//! Audio placement, layouts, seeks and stream selection.

use super::*;

/// A contract source (`corpus/<name>`) as a corpus name.
fn corpus_name(source: &str) -> &str {
    source.strip_prefix("corpus/").unwrap_or(source)
}

/// Decodes the default stream of `name` straight through at `rate`: its first sample and the
/// first channel. `None` when the backend refuses the fixture.
fn linear(backend: &dyn Backend, name: &str, rate: u32) -> Result<Option<(i64, Vec<f32>)>, String> {
    let Some(mut decoder) = audio_or_refusal(backend, name, rate, None)? else {
        return Ok(None);
    };
    let cancel = CancelToken::default();
    let mut start = None;
    let mut samples = Vec::new();
    loop {
        match decoder
            .decode_next(&cancel)
            .map_err(|error| format!("{name}: {error}"))?
        {
            DecodeEvent::Audio(block) => {
                let first = *start.get_or_insert(block.start_sample);
                let at = first + i64::try_from(samples.len()).expect("small");
                if block.start_sample != at {
                    return Err(format!(
                        "{name}: a block starts at {} after one ending at {at}",
                        block.start_sample
                    ));
                }
                samples.extend_from_slice(&block.channels[0]);
            }
            DecodeEvent::EndOfStream => break,
            DecodeEvent::Retry => {}
            event @ DecodeEvent::Video(_) => return Err(format!("{name}: unexpected {event:?}")),
        }
    }
    let start = start.ok_or_else(|| format!("{name}: no audio"))?;
    Ok(Some((start, samples)))
}

/// Seeks `name` to `ticks` and collects `frames` samples of the first channel from there.
fn after_seek(
    backend: &dyn Backend,
    name: &str,
    rate: u32,
    ticks: i64,
    frames: usize,
) -> Result<(i64, Vec<f32>), String> {
    let mut decoder = backend
        .open_audio(&corpus(name), rate, None)
        .map_err(|error| format!("{name}: {error}"))?;
    let cancel = CancelToken::default();
    let mut event = decoder
        .seek(request(MediaTime::from_ticks(ticks), 1), &cancel)
        .map_err(|error| format!("{name} seek {ticks}: {error}"))?
        .event;
    let mut start = None;
    let mut samples = Vec::new();
    while samples.len() < frames {
        let DecodeEvent::Audio(block) = event else {
            break;
        };
        start.get_or_insert(block.start_sample);
        samples.extend_from_slice(&block.channels[0]);
        event = decoder
            .decode_next(&cancel)
            .map_err(|error| format!("{name}: {error}"))?;
    }
    samples.truncate(frames);
    let start = start.ok_or_else(|| format!("{name} seek {ticks}: no audio"))?;
    Ok((start, samples))
}

fn rms_between(samples: &[f32], start: i64, from: i64, to: i64) -> f32 {
    let range = usize::try_from(from - start).expect("inside")
        ..usize::try_from(to - start).expect("inside");
    let slice = &samples[range];
    #[allow(clippy::cast_precision_loss)]
    let mean = slice.iter().map(|sample| sample * sample).sum::<f32>() / slice.len() as f32;
    mean.sqrt()
}

/// The offset within `reach` at which `seeked` best matches `linear` from `from`, by squared
/// difference after the first 1024 samples, where a resumed decoder may still differ.
fn best_lag(seeked: &[f32], linear: &[f32], from: i64, reach: i64) -> i64 {
    let mut best = (f32::MAX, 0);
    for lag in -reach..=reach {
        let Ok(at) = usize::try_from(from + lag) else {
            continue;
        };
        let Some(window) = linear.get(at + 1_024..at + seeked.len()) else {
            continue;
        };
        let error = seeked[1_024..]
            .iter()
            .zip(window)
            .map(|(a, b)| (a - b) * (a - b))
            .sum::<f32>();
        if error < best.0 {
            best = (error, lag);
        }
    }
    best.1
}

/// Audio starts at its stream's start: 0.103 s, or 0 once the edit list skips AAC priming, or
/// 1.4 s for an MPEG-TS mux delay.
///
/// # Errors
/// What differed.
pub fn audio_starts_where_the_stream_starts(backend: &dyn Backend) -> Outcome {
    let mut failures = Failures::default();
    for (name, first_sample) in [
        ("edit-list.mp4", 4_944),
        ("audio-only-aac-priming.m4a", 0),
        ("audio-mpegts.ts", 67_200),
    ] {
        let Some((start, _)) = linear(backend, name, 48_000)? else {
            continue;
        };
        failures.check(start == first_sample, || {
            format!("{name}: first sample {start}, expected {first_sample}")
        });
    }
    failures.outcome()
}

/// 5.1 arrives as FL FR FC LFE BL BR at the levels the corpus wrote.
///
/// # Errors
/// What differed.
pub fn audio_surround_keeps_the_contract_speaker_order(backend: &dyn Backend) -> Outcome {
    let name = "audio-surround-offset.mov";
    let Some(mut decoder) = audio_or_refusal(backend, name, 48_000, None)? else {
        return Ok(());
    };
    let DecodeEvent::Audio(block) = decoder
        .decode_next(&CancelToken::default())
        .map_err(|error| error.to_string())?
    else {
        return Err(format!("{name}: no audio block"));
    };
    let mut failures = Failures::default();
    failures.check(block.start_sample == 2_400, || {
        format!("{name}: starts at {}", block.start_sample)
    });
    failures.check(block.speakers.describe() == "FL+FR+FC+LFE+BL+BR", || {
        format!("{name}: speakers {}", block.speakers.describe())
    });
    for (index, (plane, value)) in block
        .channels
        .iter()
        .zip([0.1_f32, 0.2, 0.3, 0.4, 0.5, 0.6])
        .enumerate()
    {
        let expected = level(value);
        failures.check(plane.iter().all(|sample| *sample == expected), || {
            format!("{name}: plane {index} is not {expected}")
        });
    }
    failures.check(block.channels.len() == 6, || {
        format!("{name}: {} planes", block.channels.len())
    });
    failures.outcome()
}

/// Declared layouts keep their planes and name their speakers, and codec layouts (AAC's C L R)
/// arrive in the contract order.
///
/// # Errors
/// What differed.
pub fn audio_declared_layouts_reach_the_engine(backend: &dyn Backend) -> Outcome {
    let contract = alpha2_provider_contract::AudioContract::load();
    for layout in &contract.layouts {
        let Some(mut decoder) =
            audio_or_refusal(backend, corpus_name(&layout.source), 48_000, None)?
        else {
            continue;
        };
        let block =
            whole_audio(&mut *decoder).map_err(|error| format!("{}: {error}", layout.source))?;
        layout.check(&block, contract.layout_frames, contract.stream.scale);
    }
    for layout in &contract.codec_layouts {
        let Some(mut decoder) =
            audio_or_refusal(backend, corpus_name(&layout.source), 48_000, None)?
        else {
            continue;
        };
        let block =
            whole_audio(&mut *decoder).map_err(|error| format!("{}: {error}", layout.source))?;
        layout.check(&block);
    }
    Ok(())
}

/// A source decoded whole serves the engine as the audio provider contract requires.
///
/// # Errors
/// What differed.
pub fn audio_whole_source_meets_the_provider_contract(backend: &dyn Backend) -> Outcome {
    let contract = alpha2_provider_contract::AudioContract::load();
    let Some(mut decoder) = audio_or_refusal(
        backend,
        corpus_name(&contract.source),
        contract.project_sample_rate,
        None,
    )?
    else {
        return Ok(());
    };
    let whole = whole_audio(&mut *decoder)?;
    let source = SourceId::new("surround").expect("id");
    let mut sources = alpha2_host_core::DecodedAudioSources::new();
    sources.insert(source.clone(), whole);
    alpha2_provider_contract::check_audio_provider(&mut sources, &source, &contract, |_, _| {});
    Ok(())
}

/// A seek before the stream's first sample keeps its initial gap and gives the samples a linear
/// decode gives from the target.
///
/// # Errors
/// What differed.
pub fn audio_seek_before_the_stream_keeps_its_initial_gap(backend: &dyn Backend) -> Outcome {
    let mut failures = Failures::default();
    for name in [
        "edit-list.mp4",
        "audio-mpegts.ts",
        "audio-only-aac-priming.m4a",
    ] {
        let Some((start, samples)) = linear(backend, name, 48_000)? else {
            continue;
        };
        let first_ticks = start * 120_000 / 48_000;
        for ticks in [0, (first_ticks - 1).max(0), first_ticks, first_ticks + 10] {
            let mut decoder = backend
                .open_audio(&corpus(name), 48_000, None)
                .map_err(|error| error.to_string())?;
            let result = decoder
                .seek(
                    request(MediaTime::from_ticks(ticks), 1),
                    &CancelToken::default(),
                )
                .map_err(|error| format!("{name} seek {ticks}: {error}"))?;
            let DecodeEvent::Audio(block) = result.event else {
                failures.check(false, || format!("{name} seek {ticks}: no audio"));
                continue;
            };
            let expected = (ticks * 48_000 / 120_000).max(start);
            let from = usize::try_from(expected - start).expect("offset");
            // Two sessions of one decoder may differ in the last bits (`AudioToolbox`'s AAC
            // decoder does), so samples match within 1e-6 as in the other seek cases.
            let same = samples
                .get(from..from + block.frames())
                .is_some_and(|linear| {
                    linear
                        .iter()
                        .zip(&block.channels[0])
                        .all(|(a, b)| (a - b).abs() <= 1e-6)
                });
            failures.check(block.start_sample == expected && same, || {
                format!(
                    "{name} seek {ticks}: block at {}, expected {expected} with the linear samples",
                    block.start_sample
                )
            });
        }
    }
    failures.outcome()
}

/// A seek to 0.2 s gives the samples linear decoding gives from sample 9600.
///
/// # Errors
/// What differed.
pub fn audio_seek_matches_a_linear_decode(backend: &dyn Backend) -> Outcome {
    let name = "edit-list.mp4";
    let Some((start, samples)) = linear(backend, name, 48_000)? else {
        return Ok(());
    };
    let (first, seeked) = after_seek(backend, name, 48_000, 24_000, 4_096)?;
    if first != 9_600 {
        return Err(format!("{name}: the seek starts at {first}, not 9600"));
    }
    let from = usize::try_from(9_600 - start).expect("inside");
    let differs = seeked
        .iter()
        .zip(&samples[from..])
        .position(|(seek, whole)| (seek - whole).abs() > 1e-6);
    differs.map_or(Ok(()), |index| {
        Err(format!(
            "{name}: sample {} differs from the linear decode",
            9_600 + index
        ))
    })
}

/// A seek gives the samples a straight decode gives, at the same positions, in Matroska and `WebM`
/// (millisecond timestamps) as in MP4; Matroska samples sit where the MP4 of the same encode
/// puts them.
///
/// # Errors
/// What differed.
pub fn audio_seek_matches_a_linear_decode_in_every_container(backend: &dyn Backend) -> Outcome {
    let mut failures = Failures::default();
    for name in [
        "audio-seek-aac.mkv",
        "audio-seek-aac-44100.mkv",
        "audio-seek-aac.mp4",
        "audio-seek-aac-remux.mp4",
        "audio-seek-opus.webm",
        "audio-seek-opus.mp4",
        "audio-seek-libopus.webm",
    ] {
        let Some((start, samples)) = linear(backend, name, 48_000)? else {
            continue;
        };
        for ms in [150_i64, 250, 333, 500, 777, 1_000, 1_234] {
            let (first, seeked) = after_seek(backend, name, 48_000, ms * 120, 8_192)?;
            if first != ms * 48 {
                failures.check(false, || format!("{name} seek {ms} ms: starts at {first}"));
                continue;
            }
            let from = first - start;
            let lag = best_lag(&seeked, &samples, from, 96);
            failures.check(lag == 0, || {
                format!("{name} seek {ms} ms: samples {lag} off")
            });
            let at = usize::try_from(from).expect("inside");
            // A resumed decoder differs within its noise; Opus converges over its pre-roll.
            let differs = seeked
                .iter()
                .zip(&samples[at..])
                .position(|(a, b)| (a - b).abs() > 1e-4);
            failures.check(differs.is_none(), || {
                format!(
                    "{name} seek {ms} ms: sample {:?} differs",
                    differs.map(|i| first + i64::try_from(i).unwrap_or(0))
                )
            });
        }
    }
    for (rounded, exact) in [
        ("audio-seek-aac.mkv", "audio-seek-aac.mp4"),
        ("audio-seek-opus.webm", "audio-seek-opus.mp4"),
    ] {
        let (Some((rounded_start, rounded_samples)), Some((exact_start, exact_samples))) = (
            linear(backend, rounded, 48_000)?,
            linear(backend, exact, 48_000)?,
        ) else {
            continue;
        };
        let skip = usize::try_from(exact_start - rounded_start).unwrap_or(usize::MAX);
        failures.check(
            rounded_samples.get(skip..skip + 48_000) == exact_samples.get(..48_000),
            || format!("{rounded} against {exact}: the samples do not line up"),
        );
    }
    failures.outcome()
}

/// A seek near the start of an ADTS stream, which cannot seek before its first packet, decodes
/// from the start and gives the linear samples.
///
/// # Errors
/// What differed.
pub fn audio_seek_near_a_raw_stream_start(backend: &dyn Backend) -> Outcome {
    let name = "audio-adts.aac";
    let Some((start, samples)) = linear(backend, name, 48_000)? else {
        return Ok(());
    };
    let mut failures = Failures::default();
    failures.check(start == 0, || format!("{name}: starts at {start}"));
    for ticks in [0, 6_000] {
        let (first, seeked) = after_seek(backend, name, 48_000, ticks, 2_048)?;
        let sample = ticks * 48_000 / 120_000;
        let from = usize::try_from(sample).expect("inside");
        let differs = seeked
            .iter()
            .zip(&samples[from..])
            .position(|(seek, whole)| (seek - whole).abs() > 1e-6);
        failures.check(
            first == sample && !seeked.is_empty() && differs.is_none(),
            || format!("{name} seek {ticks}: starts at {first}, first difference {differs:?}"),
        );
    }
    failures.outcome()
}

/// A clean timestamp gap is silence however it is reached, and the tone keeps its phase.
///
/// # Errors
/// What differed.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub fn audio_timestamp_gap_is_silence(backend: &dyn Backend) -> Outcome {
    // audio-gap.mkv: PCM, gap from 0.98 + 0.02 s to 2.0 s. audio-gap.ts: AAC from 1.4 s, gap from
    // 2.402667 + 0.021333 s to 3.426667 s.
    let mut failures = Failures::default();
    for (name, gap, tolerance) in [
        ("audio-gap.mkv", 48_000..96_000, 1e-6),
        ("audio-gap.ts", 116_352..164_480, 1e-4),
    ] {
        let Some((start, samples)) = linear(backend, name, 48_000)? else {
            continue;
        };
        let end = start + i64::try_from(samples.len()).expect("small");
        if end < gap.end + 26_400 {
            failures.check(false, || format!("{name}: the decode ends at {end}"));
            continue;
        }
        let silence = rms_between(&samples, start, gap.start + 1_200, gap.end - 1_200);
        let after = rms_between(&samples, start, gap.end + 2_400, gap.end + 26_400);
        failures.check(silence < 1e-4 && after > 0.05, || {
            format!("{name}: RMS {silence} inside the gap, {after} after it")
        });
        if name == "audio-gap.mkv" {
            let off = (gap.end + 100..gap.end + 2_000).find(|&n| {
                let expected =
                    (2.0 * std::f64::consts::PI * 440.0 * n as f64 / 48_000.0).sin() as f32 / 8.0;
                (samples[usize::try_from(n - start).expect("inside")] - expected).abs() >= 1e-3
            });
            failures.check(off.is_none(), || {
                format!("{name}: phase lost at sample {off:?}")
            });
        }
        for ticks in [
            gap.start * 5 / 2 - 60_000,
            gap.start * 5 / 2 + 24_000,
            gap.end * 5 / 2 + 24_000,
        ] {
            let (first, seeked) = after_seek(backend, name, 48_000, ticks, 9_600)?;
            let from = usize::try_from(first - start).expect("inside");
            let differs = seeked
                .iter()
                .zip(&samples[from..])
                .position(|(a, b)| (a - b).abs() > tolerance);
            failures.check(
                first == ticks * 48_000 / 120_000 && differs.is_none(),
                || format!("{name} seek {ticks}: starts at {first}, first difference {differs:?}"),
            );
        }
    }
    failures.outcome()
}

/// Each audio stream opens by its position among the audio streams; a missing one is refused.
///
/// # Errors
/// What differed.
pub fn audio_streams_open_by_ordinal(backend: &dyn Backend) -> Outcome {
    let name = "audio-two-streams.mov";
    let mut failures = Failures::default();
    for (stream, levels) in [(0, &[0.1_f32, 0.2][..]), (1, &[0.3, 0.4, 0.5, 0.6][..])] {
        let Some(mut decoder) = audio_or_refusal(backend, name, 48_000, Some(stream))? else {
            return Ok(());
        };
        let DecodeEvent::Audio(block) = decoder
            .decode_next(&CancelToken::default())
            .map_err(|error| error.to_string())?
        else {
            return Err(format!("{name} stream {stream}: no audio"));
        };
        let firsts = block
            .channels
            .iter()
            .map(|plane| plane[0])
            .collect::<Vec<_>>();
        let expected = levels.iter().copied().map(level).collect::<Vec<_>>();
        failures.check(firsts == expected, || {
            format!("{name} stream {stream}: {firsts:?}, expected {expected:?}")
        });
    }
    let languages = backend
        .open_audio(&corpus(name), 48_000, Some(1))
        .map(|decoder| {
            decoder
                .streams()
                .iter()
                .filter(|stream| stream.kind == StreamKind::Audio)
                .map(|stream| stream.language.clone())
                .collect::<Vec<_>>()
        });
    failures.check(
        languages
            .as_ref()
            .is_ok_and(|languages| languages.get(1) == Some(&Some("eng".to_owned()))),
        || format!("{name}: stream languages {languages:?}"),
    );
    let missing = backend.open_audio(&corpus(name), 48_000, Some(2)).err();
    failures.check(
        matches!(&missing, Some(DecodeError::Unsupported(reason)) if reason.contains("no audio stream 2")),
        || format!("{name} stream 2: {missing:?}"),
    );
    failures.outcome()
}

/// With no stream named, the backend opens the stream its declared rule picks.
///
/// `audio-two-streams.mov` holds a default-flagged stereo stream, then a four-channel one. Both
/// rules pick the stereo stream: `FFmpeg` ranks the default flag before bitrate and frame count.
/// The rules differ only for files with no default-flagged stream, which the corpus does not have.
///
/// # Errors
/// What differed.
pub fn audio_default_stream_follows_the_backends_rule(backend: &dyn Backend) -> Outcome {
    let name = "audio-two-streams.mov";
    let Some(mut decoder) = audio_or_refusal(backend, name, 48_000, None)? else {
        return Ok(());
    };
    let DecodeEvent::Audio(block) = decoder
        .decode_next(&CancelToken::default())
        .map_err(|error| error.to_string())?
    else {
        return Err(format!("{name}: no audio"));
    };
    let expected = match backend.default_audio() {
        DefaultAudioRule::FfmpegBestStream | DefaultAudioRule::FirstDefaultFlagged => 2,
    };
    if block.channels.len() == expected {
        Ok(())
    } else {
        Err(format!(
            "{name}: the default stream has {} channels; {:?} picks {expected}",
            block.channels.len(),
            backend.default_audio()
        ))
    }
}

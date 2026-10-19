//! Damaged input: what each decode policy keeps and conceals.

use super::*;

#[test]
fn damaged_video_policies_preserve_prefix_suffix_pts() {
    for codec in ["h264", "vp9", "mpeg4"] {
        let decode = |name: &str, policy| {
            let mut decoder = NativeDecoder::open(corpus(name)).unwrap();
            decoder.set_decode_policy(policy);
            let mut pts = Vec::new();
            let mut error = None;
            for _ in 0..100 {
                match decoder.decode_next(&CancelToken::default()) {
                    Ok(DecodeEvent::Video(frame)) => pts.push(frame.pts),
                    Ok(DecodeEvent::EndOfStream) => break,
                    Err(failure) => {
                        error = Some(failure);
                        break;
                    }
                    other => panic!("unexpected {other:?}"),
                }
            }
            (pts, error, decoder.decode_diagnostics().clone())
        };
        let (healthy, error, _) = decode(&format!("recovery-{codec}.mp4"), DecodePolicy::Strict);
        assert!(error.is_none(), "{codec}: {error:?}");
        assert_eq!(healthy.len(), 20);
        let (prefix, error, _) = decode(
            &format!("recovery-{codec}-damaged.mp4"),
            DecodePolicy::Strict,
        );
        assert!(error.is_some(), "strict {codec} accepted damage");
        assert_eq!(prefix, healthy[..10]);
        let (recovered, error, diagnostics) = decode(
            &format!("recovery-{codec}-damaged.mp4"),
            DecodePolicy::Tolerant,
        );
        assert!(error.is_none(), "tolerant {codec}: {error:?}");
        assert!(!diagnostics.corrupt_packets.is_empty(), "{codec}");
        let expected: Vec<_> = healthy
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 10)
            .map(|(_, pts)| *pts)
            .collect();
        assert_eq!(recovered, expected, "{codec}");
        assert_eq!(diagnostics.video_gaps.len(), 1, "{codec}");
        let gap = &diagnostics.video_gaps[0];
        let base = rsmpeg::avutil::ra(gap.time_base.0, gap.time_base.1);
        assert_eq!(
            crate::convert::timestamp(gap.previous_pts.unwrap(), base),
            Some(healthy[9]),
            "{codec}: {gap:?}"
        );
        assert_eq!(
            crate::convert::timestamp(gap.recovered_pts.unwrap(), base),
            Some(healthy[11]),
            "{codec}: {gap:?}"
        );
    }
}

#[test]
fn damaged_aac_conceals_without_shifting_suffix_samples() {
    let decode = |name: &str, policy| {
        let mut decoder = NativeAudioDecoder::open(corpus(name), 48_000).unwrap();
        decoder.set_decode_policy(policy);
        let mut samples = Vec::new();
        let mut error = None;
        for _ in 0..200 {
            match decoder.decode_next(&CancelToken::default()) {
                Ok(DecodeEvent::Audio(block)) => {
                    assert_eq!(block.start_sample, i64::try_from(samples.len()).unwrap());
                    samples.extend_from_slice(&block.channels[0]);
                }
                Ok(DecodeEvent::EndOfStream) => break,
                Err(failure) => {
                    error = Some(failure);
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        (samples, error, decoder.decode_diagnostics().clone())
    };
    let (healthy, error, _) = decode("recovery-aac.m4a", DecodePolicy::Strict);
    assert!(error.is_none());
    let (_, error, _) = decode("recovery-aac-damaged.m4a", DecodePolicy::Strict);
    assert!(error.is_some());
    let (recovered, error, diagnostics) =
        decode("recovery-aac-damaged.m4a", DecodePolicy::Tolerant);
    assert!(error.is_none(), "{error:?}");
    assert_eq!(recovered.len(), healthy.len());
    assert_eq!(diagnostics.audio_gaps.len(), 1);
    let gap = &diagnostics.audio_gaps[0];
    assert_eq!((gap.start_sample, gap.end_sample), (39 * 1024, 40 * 1024));
    assert_eq!(
        recovered[..usize::try_from(gap.start_sample).unwrap()],
        healthy[..usize::try_from(gap.start_sample).unwrap()]
    );
    assert!(
        recovered
            [usize::try_from(gap.start_sample).unwrap()..usize::try_from(gap.end_sample).unwrap()]
            .iter()
            .all(|x| *x == 0.0)
    );
    // AAC overlap needs one healthy packet to recover its transform history.
    assert_eq!(recovered[41 * 1024..], healthy[41 * 1024..]);
    assert_ne!(
        diagnostics.corrupt_packets,
        [] as [recovery::CorruptPacket; 0]
    );
}

/// Decodes `path` from its start at `rate`, returning the first channel and
/// the gaps the decoder concealed.
fn decode_audio_with_gaps(path: &Path, rate: u32) -> (Vec<f32>, Vec<ConcealedAudioGap>) {
    let mut decoder = NativeAudioDecoder::open(path, rate).expect("audio decoder");
    let cancel = CancelToken::default();
    let mut samples = Vec::new();
    loop {
        match decoder.decode_next(&cancel).expect("audio block") {
            DecodeEvent::Audio(block) => samples.extend_from_slice(&block.channels[0]),
            DecodeEvent::EndOfStream => break,
            event => panic!("unexpected {event:?}"),
        }
    }
    (samples, decoder.decode_diagnostics().audio_gaps.clone())
}

/// ADTS carries no timestamps. `FFmpeg`'s demuxer makes them up from
/// the rate the stream starts at, so after a 44.1 kHz file runs into a 48 kHz
/// one each packet's timestamp runs 1024/44 100 s on while its audio lasts
/// 1024/48 000 s, and placing frames by those timestamps would leave a
/// silent gap about every 13 000 samples. Each frame's header names its own
/// rate, so the stream plays as the two files one after the other.
#[test]
fn a_raw_adts_stream_that_changes_rate_plays_without_gaps() {
    let path = corpus("audio-adts-rate-change.aac");
    let bytes = std::fs::read(&path).expect("fixture");
    let rate_index = |at: usize| (bytes[at + 2] >> 2) & 0xF;
    let mut at = 0;
    while rate_index(at) == rate_index(0) {
        assert_eq!((bytes[at], bytes[at + 1] & 0xF0), (0xFF, 0xF0), "ADTS sync");
        at += (usize::from(bytes[at + 3] & 3) << 11)
            | (usize::from(bytes[at + 4]) << 3)
            | usize::from(bytes[at + 5] >> 5);
    }
    let halves = [&bytes[..at], &bytes[at..]].map(|half| {
        let half_path = output_path(&format!("adts-half-{}", half.len())).with_extension("aac");
        std::fs::write(&half_path, half).expect("half");
        let (samples, gaps) = decode_audio_with_gaps(&half_path, 48_000);
        std::fs::remove_file(&half_path).expect("remove half");
        assert!(gaps.is_empty(), "{gaps:?}");
        samples.len()
    });
    let (whole, gaps) = decode_audio_with_gaps(&path, 48_000);
    assert!(
        gaps.is_empty(),
        "no gap in a stream that has none: {gaps:?}"
    );
    let expected = halves[0] + halves[1];
    assert!(
        whole.len().abs_diff(expected) <= 64,
        "{} samples, the two files apart {expected}",
        whole.len()
    );
    // A seek lands where linear decoding puts the audio, before the change and past it, on a
    // decoder that has not seen the change yet and on one that has.
    let cancel = CancelToken::default();
    let seek = |decoder: &mut NativeAudioDecoder, milliseconds: i64| {
        let ticks = milliseconds * alpha2_time::TICKS_PER_SECOND / 1_000;
        let request = DecodeRequest {
            source: alpha2_media::SourceId::new("adts").expect("source ID"),
            time: MediaTime::from_ticks(ticks),
            kind: alpha2_media::DecodeRequestKind::Seek,
            generation: 1,
        };
        let DecodeEvent::Audio(block) = decoder.seek(request, &cancel).expect("seek").event else {
            panic!("audio seek returned a non-audio event");
        };
        let from = usize::try_from(block.start_sample).expect("inside the stream");
        assert_eq!(
            i64::try_from(from).unwrap(),
            ticks * 48_000 / alpha2_time::TICKS_PER_SECOND
        );
        for (index, (seek, linear)) in block.channels[0].iter().zip(&whole[from..]).enumerate() {
            // AAC restarted mid-stream differs within its noise (see the resampled-seek test); a
            // seek decoded from the start does not differ at all.
            assert!(
                (seek - linear).abs() <= 1e-4,
                "{milliseconds} ms, sample {}: seek {seek}, linear {linear}",
                from + index
            );
        }
        assert_eq!(
            decoder.decode_diagnostics().audio_gaps,
            [] as [recovery::ConcealedAudioGap; 0]
        );
    };
    for milliseconds in [300, 800] {
        seek(
            &mut NativeAudioDecoder::open(&path, 48_000).expect("decoder"),
            milliseconds,
        );
    }
    let mut warm = NativeAudioDecoder::open(&path, 48_000).expect("decoder");
    for milliseconds in [900, 800, 300, 700] {
        seek(&mut warm, milliseconds);
    }
}

/// A raw ADTS stream cut off inside its last packet: Strict fails on the
/// damage. Tolerant counts the incomplete frame as damage and ends the stream
/// at the last whole one, as a damaged video tail ends at its last healthy
/// frame, so it decodes what `ffmpeg` does: the 19 whole frames, 19 456
/// samples, each the same as the uncut stream's.
#[test]
fn a_truncated_adts_tail_is_damage_tolerant_decodes_up_to() {
    let decode = |name: &str, policy| {
        let mut decoder = NativeAudioDecoder::open(corpus(name), 48_000).unwrap();
        decoder.set_decode_policy(policy);
        let mut samples = Vec::new();
        let ended = loop {
            match decoder.decode_next(&CancelToken::default()) {
                Ok(DecodeEvent::Audio(block)) => {
                    assert_eq!(block.start_sample, i64::try_from(samples.len()).unwrap());
                    samples.extend_from_slice(&block.channels[0]);
                }
                Ok(DecodeEvent::EndOfStream) => break Ok(samples),
                Ok(other) => panic!("unexpected {other:?}"),
                Err(error) => break Err(error),
            }
        };
        (ended, decoder.decode_diagnostics().clone())
    };
    let uncut = decode("audio-adts.aac", DecodePolicy::Strict)
        .0
        .expect("the uncut stream");
    let (ended, diagnostics) = decode("recovery-truncated.aac", DecodePolicy::Strict);
    let error = ended.expect_err("Strict fails on the damaged tail");
    assert!(
        error.to_string().contains("strict decode policy"),
        "{error}"
    );
    assert_eq!(diagnostics.corrupt_packets.len(), 1);
    let (ended, diagnostics) = decode("recovery-truncated.aac", DecodePolicy::Tolerant);
    let cut = ended.expect("Tolerant ends the stream at its last whole frame");
    assert_eq!(cut.len(), 19 * 1024);
    assert_eq!(cut[..], uncut[..cut.len()]);
    assert_eq!(diagnostics.corrupt_packets.len(), 1, "{diagnostics:?}");
    // Nothing follows the damage, so no silence stands in for it.
    assert_eq!(diagnostics.audio_gaps, []);
}

#[test]
fn entirely_damaged_video_cannot_succeed_with_empty_output() {
    let healthy = corpus("recovery-mpeg4.mp4");
    let mut input = crate::decode::open_input(&healthy).unwrap();
    let mut bytes = fs::read(&healthy).unwrap();
    while let Some(packet) = input.read_packet().unwrap() {
        let start = usize::try_from(packet.pos).unwrap();
        let size = usize::try_from(packet.size).unwrap();
        bytes[start..start + size].fill(0xff);
    }
    let path = output_path("fully-damaged");
    fs::write(&path, bytes).unwrap();
    let mut decoder = NativeDecoder::open(&path).unwrap();
    decoder.set_decode_policy(DecodePolicy::Tolerant);
    assert!(decoder.decode_next(&CancelToken::default()).is_err());
    drop(decoder);
    fs::remove_file(path).unwrap();
}

#[test]
fn damaged_aac_resampling_preserves_the_suffix_sample_grid() {
    for rate in [44_100, 96_000] {
        let collect = |name: &str| {
            let mut decoder = NativeAudioDecoder::open(corpus(name), rate).unwrap();
            decoder.set_block_frames(4096).unwrap();
            decoder.set_decode_policy(DecodePolicy::Tolerant);
            let mut output = Vec::new();
            loop {
                match decoder.decode_next(&CancelToken::default()).unwrap() {
                    DecodeEvent::Audio(block) => {
                        assert_eq!(block.start_sample, i64::try_from(output.len()).unwrap());
                        output.extend_from_slice(&block.channels[0]);
                    }
                    DecodeEvent::EndOfStream => break,
                    other => panic!("{other:?}"),
                }
            }
            output
        };
        let healthy = collect("recovery-aac.m4a");
        let recovered = collect("recovery-aac-damaged.m4a");
        assert_eq!(recovered.len(), healthy.len(), "rate {rate}");
        let suffix = usize::try_from(rate).unwrap();
        assert_eq!(recovered[suffix..], healthy[suffix..], "rate {rate}");
    }
}

#[test]
fn recovery_checks_cancellation_inside_video_and_audio_loops() {
    let cancel = CancelToken::default();
    let report_cancel = cancel.clone();
    let mut video = NativeDecoder::open(corpus("recovery-mpeg4-damaged.mp4")).unwrap();
    video.set_decode_policy(DecodePolicy::Tolerant);
    video.set_decode_reporter(move |_| report_cancel.cancel());
    for _ in 0..10 {
        assert!(matches!(
            video.decode_next(&cancel),
            Ok(DecodeEvent::Video(_))
        ));
    }
    assert_eq!(video.decode_next(&cancel), Err(DecodeError::Cancelled));

    let cancel = CancelToken::default();
    let report_cancel = cancel.clone();
    let mut audio = NativeAudioDecoder::open(corpus("recovery-aac-damaged.m4a"), 48_000).unwrap();
    audio.set_decode_policy(DecodePolicy::Tolerant);
    audio.set_decode_reporter(move |_| report_cancel.cancel());
    // One public call crosses the damaged packet; cancellation must interrupt fill_pending.
    audio.set_block_frames(96_000).unwrap();
    assert_eq!(audio.decode_next(&cancel), Err(DecodeError::Cancelled));
}

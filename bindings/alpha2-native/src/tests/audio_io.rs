//! Audio decode, resampling, and seeks, and the WAV and AAC sinks.

use super::*;

#[test]
fn unspecified_audio_layout_uses_the_default_channel_order() {
    let unspecified = AVChannelLayout::from_string(c"2 channels").expect("channel layout");
    assert_eq!(unspecified.order, ffi::AV_CHANNEL_ORDER_UNSPEC);
    let mut frame = AVFrame::new();
    frame.set_ch_layout(unspecified.into_inner());

    let stream = AVChannelLayout::from_nb_channels(2);
    normalize_audio_frame_layout(&mut frame, &stream).expect("normalized layout");

    assert_eq!(frame.ch_layout().nb_channels, 2);
    assert_ne!(frame.ch_layout().order, ffi::AV_CHANNEL_ORDER_UNSPEC);
    assert_eq!(
        frame
            .ch_layout()
            .describe()
            .expect("layout description")
            .to_string_lossy(),
        "stereo"
    );
}

#[test]
fn wav_sink_writes_interleaved_little_endian_samples_after_its_header() {
    let path = output_path("wav-bytes").with_extension("wav");
    let _ = fs::remove_file(&path);
    let cancel = CancelToken::default();
    let mut sink = NativeWavEncodeSink::create(&path, 48_000, 2).expect("wav sink");
    let left = [0.25_f32, -1.0, 0.5];
    let right = [0.75_f32, 2.0, -0.125];
    for (start, range) in [(0, 0..2), (2, 2..3)] {
        let block = AudioBlock {
            start_sample: start,
            sample_rate: 48_000,
            speakers: alpha2_media::SpeakerLayout::STEREO,
            channels: vec![left[range.clone()].to_vec(), right[range].to_vec()],
        };
        sink.write_audio(&block, &cancel).expect("wav block");
    }
    sink.finish_audio(&cancel).expect("wav finish");

    let mut expected = crate::encode::wav_header(48_000, 2, 24)
        .expect("header")
        .to_vec();
    for (left, right) in left.iter().zip(&right) {
        expected.extend_from_slice(&left.to_le_bytes());
        expected.extend_from_slice(&right.to_le_bytes());
    }
    assert_eq!(fs::read(&path).expect("wav bytes"), expected);
    fs::remove_file(path).expect("cleanup");
}

#[test]
fn a_wav_says_a_layout_other_than_its_counts_default_with_a_channel_mask() {
    // C L R becomes mask 0x7 with planes in mask order, and reads back as 3.0.
    let path = output_path("wav-mask").with_extension("wav");
    let _ = fs::remove_file(&path);
    let cancel = CancelToken::default();
    let lcr = alpha2_media::SpeakerLayout::new(&[
        Speaker::FrontCenter,
        Speaker::FrontLeft,
        Speaker::FrontRight,
    ])
    .expect("C L R");
    let mut sink = NativeWavEncodeSink::create_with_speakers(&path, 48_000, lcr).expect("sink");
    sink.write_audio(
        &AudioBlock {
            start_sample: 0,
            sample_rate: 48_000,
            speakers: lcr,
            channels: vec![vec![0.375; 480], vec![0.125; 480], vec![0.25; 480]],
        },
        &cancel,
    )
    .expect("write");
    sink.finish_audio(&cancel).expect("finish");
    let block = decode_whole_audio(&path);
    assert_eq!(block.speakers.describe(), "FL+FR+FC");
    assert_eq!(
        block
            .channels
            .iter()
            .map(|plane| plane[100])
            .collect::<Vec<_>>(),
        [0.125, 0.25, 0.375]
    );
    fs::remove_file(&path).expect("cleanup");
    // A mask has no bit for an unnamed speaker or one past the 18 WAVE positions.
    for speakers in [
        [Speaker::FrontLeft, Speaker::Unnamed],
        [Speaker::FrontLeft, Speaker::LowFrequency2],
    ] {
        let layout = alpha2_media::SpeakerLayout::new(&speakers).expect("layout");
        assert!(NativeWavEncodeSink::create_with_speakers(&path, 48_000, layout).is_err());
    }
}

#[test]
fn an_aborted_wav_sink_leaves_no_file() {
    let path = output_path("wav-abort").with_extension("wav");
    let _ = fs::remove_file(&path);
    let cancel = CancelToken::default();
    let mut sink = NativeWavEncodeSink::create(&path, 48_000, 1).expect("wav sink");
    let block = AudioBlock {
        start_sample: 0,
        sample_rate: 48_000,
        speakers: alpha2_media::SpeakerLayout::MONO,
        channels: vec![vec![0.5; 4_096]],
    };
    sink.write_audio(&block, &cancel).expect("wav block");
    let partial = partial_beside(&path);
    sink.abort_audio().expect("abort");
    assert!(!partial.exists());
    assert!(!path.exists());
}

#[test]
fn native_audio_decode_resamples_and_wav_sink_round_trips() {
    let mut decoder = NativeAudioDecoder::open(corpus("audio-only-aac-priming.m4a"), 48_000)
        .expect("audio decode");
    decoder.set_block_frames(257).expect("block size");
    let cancel = CancelToken::default();
    let first = match decoder.decode_next(&cancel).expect("first block") {
        DecodeEvent::Audio(block) => block,
        event => panic!("unexpected {event:?}"),
    };
    assert_eq!(first.sample_rate, 48_000);
    assert_eq!(first.channels.len(), 1);
    assert_eq!(first.channels[0].len(), 257);
    let seek_time = MediaTime::from_ticks(1_200);
    let seeked = decoder
        .seek(
            DecodeRequest {
                source: alpha2_media::SourceId::new("audio-round-trip").expect("source ID"),
                time: seek_time,
                kind: alpha2_media::DecodeRequestKind::Seek,
                generation: 1,
            },
            &cancel,
        )
        .expect("exact audio seek");
    let DecodeEvent::Audio(seeked_block) = seeked.event else {
        panic!("audio seek returned a non-audio event");
    };
    // 1200 ticks is sample 480 at 48 kHz.
    assert_eq!(seeked_block.start_sample, 480);
    decoder.flush().expect("audio reset");
    let first = match decoder.decode_next(&cancel).expect("reset first block") {
        DecodeEvent::Audio(block) => block,
        event => panic!("unexpected {event:?}"),
    };
    assert_eq!(first.start_sample, 0);
    let path = output_path("audio-round-trip").with_extension("wav");
    let _ = fs::remove_file(&path);
    let mut sink = NativeWavEncodeSink::create(&path, 48_000, 1).expect("wav sink");
    sink.write_audio(&first, &cancel).expect("wav block");
    let mut expected_start = first.channels[0].len();
    loop {
        match decoder.decode_next(&cancel).expect("audio block") {
            DecodeEvent::Audio(mut block) => {
                block.start_sample = i64::try_from(expected_start).expect("small");
                expected_start += block.channels[0].len();
                sink.write_audio(&block, &cancel).expect("wav block");
            }
            DecodeEvent::EndOfStream => break,
            event => panic!("unexpected {event:?}"),
        }
    }
    sink.finish_audio(&cancel).expect("wav finish");
    let round_trip = probe(&path).expect("wav probe");
    let audio = round_trip
        .streams
        .iter()
        .find(|stream| stream.kind == StreamKind::Audio)
        .expect("audio stream");
    assert_eq!(audio.sample_rate, Some(48_000));
    assert_eq!(audio.channels, Some(1));
    fs::remove_file(path).expect("cleanup");
}

#[test]
fn audio_video_sink_interleaves_both_streams_transactionally() {
    let path = output_path("av-round-trip");
    let _ = fs::remove_file(&path);
    let mut sink = NativeAvEncodeSink::create(
        &path,
        AvEncodeConfig {
            video: VideoEncodeConfig {
                width: 16,
                height: 16,
                frame_rate: FrameRate::new(30, 1).expect("rate"),
                color: ColorMetadata::default(),
                rate_control: VideoRateControl::bitrate(200_000, BitrateMode::Variable).unwrap(),
                codec: NativeVideoCodec::Mpeg4Part2,
                display: crate::VideoDisplay::default(),
                container: alpha2_media::Container::Mp4,
                pixels: alpha2_media::PixelLayout::YUV420P,
            },
            audio_sample_rate: 48_000,
            audio_channels: 2,
            audio_codec: NativeAudioCodec::Aac,
            audio_bit_rate: 192_000,
        },
    )
    .expect("av sink");
    let cancel = CancelToken::default();
    let mut max_queued_packets = 0;
    for index in 0..30 {
        let time = MediaTime::from_ticks(index * 4_000);
        sink.write(
            EncodeEvent::Video(rgba_frame(
                time,
                [u8::try_from(index).expect("small"), 0, 255, 255],
            )),
            &cancel,
        )
        .expect("video");
        sink.write(
            EncodeEvent::Audio(AudioBlock {
                start_sample: index * 1_600,
                sample_rate: 48_000,
                speakers: alpha2_media::SpeakerLayout::STEREO,
                channels: vec![vec![0.0; 1_600], vec![0.0; 1_600]],
            }),
            &cancel,
        )
        .expect("audio");
        max_queued_packets = max_queued_packets.max(
            sink.video_packets
                .len()
                .saturating_add(sink.queued_audio_packets()),
        );
    }
    assert!(max_queued_packets < 16, "packet queues must stay bounded");
    sink.finish(&cancel).expect("mux finish");
    let result = probe(&path).expect("mux probe");
    // The encoder leaves packet durations unset; the last frame still lasts one frame.
    assert_eq!(
        result
            .streams
            .iter()
            .find(|stream| stream.kind == StreamKind::Video)
            .and_then(|stream| stream.duration),
        Some(MediaTime::from_ticks(30 * 4_000))
    );
    assert!(result.selected_video.is_some());
    assert!(result.selected_audio.is_some());
    assert_eq!(
        result
            .streams
            .iter()
            .find(|stream| stream.kind == StreamKind::Audio)
            .and_then(|stream| stream.channels),
        Some(2)
    );
    fs::remove_file(path).expect("cleanup");
}

#[test]
fn audio_video_sink_rejects_non_interleaved_input() {
    let path = output_path("av-non-interleaved");
    let _ = fs::remove_file(&path);
    let mut sink = NativeAvEncodeSink::create(
        &path,
        AvEncodeConfig {
            video: VideoEncodeConfig {
                width: 16,
                height: 16,
                frame_rate: FrameRate::new(30, 1).expect("rate"),
                color: ColorMetadata::default(),
                rate_control: VideoRateControl::bitrate(200_000, BitrateMode::Variable).unwrap(),
                codec: NativeVideoCodec::Mpeg4Part2,
                display: crate::VideoDisplay::default(),
                container: alpha2_media::Container::Mp4,
                pixels: alpha2_media::PixelLayout::YUV420P,
            },
            audio_sample_rate: 48_000,
            audio_channels: 1,
            audio_codec: NativeAudioCodec::Aac,
            audio_bit_rate: 192_000,
        },
    )
    .expect("av sink");
    let cancel = CancelToken::default();
    sink.write(
        EncodeEvent::Video(rgba_frame(MediaTime::from_ticks(4_000), [255, 0, 0, 255])),
        &cancel,
    )
    .expect("video");
    assert_eq!(
        sink.write(
            EncodeEvent::Audio(AudioBlock {
                start_sample: 0,
                sample_rate: 48_000,
                speakers: alpha2_media::SpeakerLayout::MONO,
                channels: vec![vec![0.0; 1_024]],
            }),
            &cancel,
        ),
        Err(EncodeError::InvalidTimestamp)
    );
    sink.abort().expect("partial cleanup");
}

#[test]
fn audio_only_use_of_av_sink_keeps_packet_queues_bounded() {
    let path = output_path("av-audio-only-queue");
    let _ = fs::remove_file(&path);
    let mut sink = NativeAvEncodeSink::create(
        &path,
        AvEncodeConfig {
            video: VideoEncodeConfig {
                width: 16,
                height: 16,
                frame_rate: FrameRate::new(30, 1).expect("rate"),
                color: ColorMetadata::default(),
                rate_control: VideoRateControl::bitrate(200_000, BitrateMode::Variable).unwrap(),
                codec: NativeVideoCodec::Mpeg4Part2,
                display: crate::VideoDisplay::default(),
                container: alpha2_media::Container::Mp4,
                pixels: alpha2_media::PixelLayout::YUV420P,
            },
            audio_sample_rate: 48_000,
            audio_channels: 1,
            audio_codec: NativeAudioCodec::Aac,
            audio_bit_rate: 192_000,
        },
    )
    .expect("av sink");
    let cancel = CancelToken::default();
    for index in 0..300 {
        sink.write(
            EncodeEvent::Audio(AudioBlock {
                start_sample: index * 1_024,
                sample_rate: 48_000,
                speakers: alpha2_media::SpeakerLayout::MONO,
                channels: vec![vec![0.0; 1_024]],
            }),
            &cancel,
        )
        .expect("audio");
        assert!(sink.queued_audio_packets() < 4);
    }
    sink.abort().expect("partial cleanup");
}

#[test]
fn the_audio_thread_does_not_change_the_container_interleave() {
    // AAC encodes on its own thread, so the interleave must be byte-identical regardless of timing.
    fn encode(label: &str) -> Vec<u8> {
        let path = output_path(label);
        let _ = fs::remove_file(&path);
        let mut sink = NativeAvEncodeSink::create(
            &path,
            AvEncodeConfig {
                video: VideoEncodeConfig {
                    width: 16,
                    height: 16,
                    frame_rate: FrameRate::new(30, 1).expect("rate"),
                    color: ColorMetadata::default(),
                    rate_control: VideoRateControl::bitrate(200_000, BitrateMode::Variable)
                        .unwrap(),
                    codec: NativeVideoCodec::Mpeg4Part2,
                    display: crate::VideoDisplay::default(),
                    container: alpha2_media::Container::Mp4,
                    pixels: alpha2_media::PixelLayout::YUV420P,
                },
                audio_sample_rate: 48_000,
                audio_channels: 2,
                audio_codec: NativeAudioCodec::Aac,
                audio_bit_rate: 192_000,
            },
        )
        .expect("av sink");
        let cancel = CancelToken::default();
        for index in 0..30 {
            let time = MediaTime::from_ticks(index * 4_000);
            sink.write(
                EncodeEvent::Video(rgba_frame(
                    time,
                    [u8::try_from(index).expect("small"), 0, 255, 255],
                )),
                &cancel,
            )
            .expect("video");
            sink.write(
                EncodeEvent::Audio(AudioBlock {
                    start_sample: index * 1_600,
                    sample_rate: 48_000,
                    speakers: alpha2_media::SpeakerLayout::STEREO,
                    channels: vec![
                        (0..1_600u16)
                            .map(|n| f32::from(n % 97) / 97.0 - 0.5)
                            .collect(),
                        (0..1_600u16)
                            .map(|n| f32::from(n % 89) / 89.0 - 0.5)
                            .collect(),
                    ],
                }),
                &cancel,
            )
            .expect("audio");
        }
        sink.finish(&cancel).expect("mux finish");
        let bytes = fs::read(&path).expect("read output");
        fs::remove_file(path).expect("cleanup");
        bytes
    }

    let first = encode("av-interleave-a");
    for attempt in 0..4 {
        assert_eq!(
            first,
            encode(&format!("av-interleave-{attempt}")),
            "the muxed interleave changed between runs"
        );
    }
}

fn decode_audio_path_linearly(path: &Path, rate: u32) -> (i64, Vec<f32>) {
    let mut decoder = NativeAudioDecoder::open(path, rate).expect("audio decoder");
    let cancel = CancelToken::default();
    let mut start = None;
    let mut samples = Vec::new();
    loop {
        match decoder.decode_next(&cancel).expect("audio block") {
            DecodeEvent::Audio(block) => {
                let first = *start.get_or_insert(block.start_sample);
                assert_eq!(
                    block.start_sample,
                    first + i64::try_from(samples.len()).expect("small"),
                    "{}: blocks are contiguous",
                    path.display()
                );
                samples.extend_from_slice(&block.channels[0]);
            }
            DecodeEvent::EndOfStream => break,
            event => panic!("unexpected {event:?}"),
        }
    }
    (start.expect("some audio"), samples)
}

fn decode_whole_audio(path: &Path) -> AudioBlock {
    let mut decoder = NativeAudioDecoder::open(path, 48_000).expect("decoder");
    let cancel = CancelToken::default();
    let mut whole: Option<AudioBlock> = None;
    loop {
        match decoder.decode_next(&cancel).expect("audio block") {
            DecodeEvent::Audio(block) => match whole.as_mut() {
                None => whole = Some(block),
                Some(whole) => {
                    assert_eq!(whole.speakers, block.speakers, "every block's speakers");
                    for (plane, more) in whole.channels.iter_mut().zip(block.channels) {
                        plane.extend(more);
                    }
                }
            },
            DecodeEvent::EndOfStream => break,
            event => panic!("unexpected {event:?}"),
        }
    }
    whole.expect("some audio")
}

#[test]
fn a_declared_speaker_layout_reaches_the_engine_as_the_source_lays_it_out() {
    // These layouts keep their planes and name their speakers; nothing is rematrixed
    // into the count's default.
    let contract = alpha2_provider_contract::AudioContract::load();
    for layout in &contract.layouts {
        let decoder = NativeAudioDecoder::open(layout.source_path(), 48_000).expect("decoder");
        assert!(!decoder.speakers_assumed(), "{}", layout.source);
        let block = decode_whole_audio(&layout.source_path());
        layout.check(&block, contract.layout_frames, contract.stream.scale);
    }
    // AAC orders C L R (Ls Rs LFE); `FFmpeg`'s decoder delivers FL FR FC (LFE BL
    // BR), which the block names.
    for layout in &contract.codec_layouts {
        layout.check(&decode_whole_audio(&layout.source_path()));
    }
}

#[test]
fn a_source_that_declares_no_layout_has_ffmpegs_default_for_its_count() {
    // The engine's default layout is `FFmpeg`'s, so a count-only WAV is laid out the same way.
    for channels in 1..=alpha2_media::MAX_AUDIO_CHANNELS {
        let ffmpeg = AVChannelLayout::from_nb_channels(i32::try_from(channels).expect("small"));
        let ours = alpha2_media::SpeakerLayout::default_for(channels).expect("a default");
        if ffmpeg.order == ffi::AV_CHANNEL_ORDER_NATIVE {
            assert_eq!(
                crate::decode::layout_speakers(&ffmpeg),
                ours.speakers(),
                "{channels} channels"
            );
        } else {
            // `FFmpeg` names no layout for the count: a 7.1, the rest unnamed.
            assert_eq!(ffmpeg.order, ffi::AV_CHANNEL_ORDER_UNSPEC, "{channels}");
            assert_eq!(ours.speakers()[8..], vec![Speaker::Unnamed; channels - 8]);
        }
    }
    let path = output_path("unmasked").with_extension("wav");
    let _ = fs::remove_file(&path);
    let cancel = CancelToken::default();
    let mut sink = NativeWavEncodeSink::create(&path, 48_000, 3).expect("sink");
    sink.write_audio(
        &AudioBlock {
            start_sample: 0,
            sample_rate: 48_000,
            speakers: alpha2_media::SpeakerLayout::default_for(3).expect("2.1"),
            channels: vec![vec![0.25; 480]; 3],
        },
        &cancel,
    )
    .expect("write");
    sink.finish_audio(&cancel).expect("finish");
    let decoder = NativeAudioDecoder::open(&path, 48_000).expect("decoder");
    assert!(decoder.speakers_assumed());
    assert_eq!(decoder.speakers().describe(), "FL+FR+LFE");
    drop(decoder);
    fs::remove_file(path).expect("cleanup");
}

#[test]
fn resampled_seeks_match_linear_output_through_the_resampler_tail() {
    let path = output_path("resampled-seek").with_extension("wav");
    let _ = fs::remove_file(&path);
    let cancel = CancelToken::default();
    let mut sink = NativeWavEncodeSink::create(&path, 48_000, 1).expect("sink");
    let samples = (0..96_000)
        .map(|i| f32::from(i16::try_from((i * 97) % 16_384).expect("small")) / 32_768.0)
        .collect();
    sink.write_audio(
        &AudioBlock {
            start_sample: 0,
            sample_rate: 48_000,
            speakers: alpha2_media::SpeakerLayout::MONO,
            channels: vec![samples],
        },
        &cancel,
    )
    .expect("write");
    sink.finish_audio(&cancel).expect("finish");
    for rate in [32_000, 44_100, 96_000] {
        // PCM checks every sample through EOF. AAC noise substitution depends on decoder state
        // after a mid-stream seek, so only its first block is compared, plus timestamps and length.
        for (input, tolerance) in [(&path, 1e-6), (&corpus("edit-list.mp4"), 1e-4)] {
            let (start, linear) = decode_audio_path_linearly(input, rate);
            let mut decoder = NativeAudioDecoder::open(input, rate).expect("decoder");
            decoder.set_block_frames(37).expect("blocks");
            for ticks in [24_000, 31_111, 48_000, 54_321] {
                let tolerance = if ticks == 24_000 { 1e-6 } else { tolerance };
                let target = ticks * i64::from(rate) / 120_000;
                let mut event = decoder
                    .seek(
                        DecodeRequest {
                            source: alpha2_media::SourceId::new("audio").expect("id"),
                            time: MediaTime::from_ticks(ticks),
                            kind: alpha2_media::DecodeRequestKind::Seek,
                            generation: 1,
                        },
                        &cancel,
                    )
                    .expect("seek")
                    .event;
                let mut position = target;
                loop {
                    let DecodeEvent::Audio(block) = event else {
                        assert!(matches!(event, DecodeEvent::EndOfStream));
                        break;
                    };
                    assert_eq!(block.start_sample, position);
                    let from = usize::try_from(position - start).expect("inside");
                    let check_samples = input == &path || ticks == 24_000 || position == target;
                    for (offset, (&a, &b)) in
                        block.channels[0].iter().zip(&linear[from..]).enumerate()
                    {
                        assert!(
                            !check_samples || (a - b).abs() <= tolerance,
                            "{} rate={rate} seek={ticks} sample={} error={}",
                            input.display(),
                            position + i64::try_from(offset).expect("small"),
                            (a - b).abs()
                        );
                    }
                    position += i64::try_from(block.frames()).expect("small");
                    event = decoder.decode_next(&cancel).expect("next");
                }
                assert_eq!(
                    position,
                    start + i64::try_from(linear.len()).expect("small"),
                    "no lost flush samples"
                );
            }
        }
    }
    fs::remove_file(path).expect("remove");
}

#[allow(clippy::cast_precision_loss)]
fn rms_between(samples: &[f32], start: i64, from: i64, to: i64) -> f32 {
    let window = &samples[usize::try_from(from - start).expect("inside")
        ..usize::try_from(to - start).expect("inside")];
    (window.iter().map(|sample| sample * sample).sum::<f32>() / window.len() as f32).sqrt()
}

/// Writes `frames` silent mono frames at `rate`, each `samples` long (or the encoder's frame size),
/// with `encoder` into `path`, whose extension picks the muxer.
fn write_silent_audio(path: &Path, encoder: &std::ffi::CStr, rate: i32, samples: i32, frames: i64) {
    let codec = AVCodec::find_encoder_by_name(encoder).expect("encoder");
    let format = if encoder == c"flac" {
        ffi::AV_SAMPLE_FMT_S16
    } else {
        ffi::AV_SAMPLE_FMT_FLT
    };
    let mono = || AVChannelLayout::from_nb_channels(1).into_inner();
    let mut context = AVCodecContext::new(&codec);
    context.set_sample_rate(rate);
    context.set_sample_fmt(format);
    context.set_ch_layout(mono());
    context.set_time_base(ra(1, rate));
    context.open(None).expect("open encoder");
    let samples = if context.frame_size > 0 {
        context.frame_size
    } else {
        samples
    };
    let name = std::ffi::CString::new(path.to_str().expect("UTF-8")).expect("no NUL");
    let mut output = rsmpeg::avformat::AVFormatContextOutput::create(&name).expect("output");
    {
        let mut stream = output.new_stream();
        stream.set_codecpar(context.extract_codecpar());
        stream.set_time_base(ra(1, rate));
    }
    output.write_header(&mut None).expect("header");
    let stream_base = output.streams()[0].time_base;
    let write = |context: &mut AVCodecContext,
                 output: &mut rsmpeg::avformat::AVFormatContextOutput| {
        while let Ok(mut packet) = context.receive_packet() {
            packet.rescale_ts(ra(1, rate), stream_base);
            packet.set_stream_index(0);
            output.interleaved_write_frame(&mut packet).expect("packet");
        }
    };
    for index in 0..frames {
        let planar = alpha2_ffmpeg_bridge::audio_frame(
            &[vec![0.0; usize::try_from(samples).expect("positive")]],
            ffi::AV_SAMPLE_FMT_FLTP,
            rate,
            index * i64::from(samples),
        )
        .expect("frame");
        let frame = if format == ffi::AV_SAMPLE_FMT_FLT {
            let mut frame = planar;
            frame.set_format(ffi::AV_SAMPLE_FMT_FLT);
            frame
        } else {
            let mut resampler = rsmpeg::swresample::SwrContext::new(
                &AVChannelLayout::from_nb_channels(1),
                format,
                rate,
                &AVChannelLayout::from_nb_channels(1),
                ffi::AV_SAMPLE_FMT_FLTP,
                rate,
            )
            .expect("converter");
            resampler.init().expect("converter");
            let mut converted = AVFrame::new();
            converted.set_format(format);
            converted.set_sample_rate(rate);
            converted.set_ch_layout(mono());
            resampler
                .convert_frame(Some(&planar), &mut converted)
                .expect("convert");
            converted.set_pts(planar.pts);
            converted
        };
        context.send_frame(Some(&frame)).expect("send");
        write(&mut context, &mut output);
    }
    context.send_frame(None).expect("flush");
    write(&mut context, &mut output);
    output.write_trailer().expect("trailer");
}

/// A file declaring 1 Hz would turn each sample into 48,000 in one `FFmpeg` call. Such a rate is
/// refused at open, and in-range frames are resampled a block at a time however long.
#[test]
fn audio_whose_resampling_would_explode_is_refused_before_it_is_resampled() {
    let pcm = output_path("one-hertz-pcm").with_extension("mka");
    let flac = output_path("one-hertz-flac").with_extension("flac");
    write_silent_audio(&pcm, c"pcm_f32le", 1, 4_096, 1);
    write_silent_audio(&flac, c"flac", 1, 4_096, 2);
    for path in [&pcm, &flac] {
        let opened = NativeAudioDecoder::open(path, 48_000).map(|_| ());
        assert!(
            matches!(&opened, Err(DecodeError::Unsupported(message)) if message.contains("1 Hz")),
            "{}: {opened:?}",
            path.display()
        );
        fs::remove_file(path).expect("cleanup");
    }

    // 1 kHz is readable; one 2^20-sample block is 48 times that at the project rate, and
    // only a block's worth is made.
    let long_block = output_path("long-block").with_extension("mka");
    write_silent_audio(&long_block, c"pcm_f32le", 1_000, 1 << 20, 1);
    let mut decoder = NativeAudioDecoder::open(&long_block, 48_000).expect("1 kHz opens");
    let first_block = decoder.decode_next(&CancelToken::default());
    drop(decoder);
    fs::remove_file(&long_block).expect("cleanup");
    assert!(
        matches!(&first_block, Ok(DecodeEvent::Audio(block)) if block.frames() == 1_024),
        "{first_block:?}"
    );
}

fn decode_audio_to_end(decoder: &mut NativeAudioDecoder) -> Result<Vec<AudioBlock>, DecodeError> {
    let cancel = CancelToken::default();
    let mut blocks = Vec::new();
    loop {
        match decoder.decode_next(&cancel)? {
            DecodeEvent::Audio(block) => blocks.push(block),
            DecodeEvent::EndOfStream => return Ok(blocks),
            event => panic!("unexpected {event:?}"),
        }
    }
}

/// A timestamp 3e17 ms from zero would overflow a sample index at 44.1 kHz, so such
/// a frame is damaged input.
#[test]
fn audio_timestamps_no_sample_index_holds_are_damaged_input() {
    for name in ["audio-pts-ahead.mkv", "audio-pts-behind.mkv"] {
        let mut strict = NativeAudioDecoder::open(corpus(name), 48_000).expect("open");
        let error = decode_audio_to_end(&mut strict).expect_err("strict refuses the frame");
        assert!(
            error.to_string().contains("audio timestamp"),
            "{name}: {error}"
        );
    }
    // Tolerant decoding drops the clusters 3e17 ms before zero and plays the 0.46 s from zero.
    let mut tolerant =
        NativeAudioDecoder::open(corpus("audio-pts-behind.mkv"), 44_100).expect("open");
    tolerant.set_decode_policy(DecodePolicy::Tolerant);
    let blocks = decode_audio_to_end(&mut tolerant).expect("tolerant decode");
    assert_eq!(blocks.first().map(|block| block.start_sample), Some(0));
    let samples = blocks
        .iter()
        .map(|block| block.channels[0].len())
        .sum::<usize>();
    assert_eq!(samples, 20 * 1_024);
    assert_eq!(tolerant.decode_diagnostics().corrupt_packets.len(), 10);
    // A seek from zero never reads the damaged clusters' timestamps.
    let mut seeking =
        NativeAudioDecoder::open(corpus("audio-pts-behind.mkv"), 48_000).expect("open");
    seeking
        .seek(seek_request(36_000), &CancelToken::default())
        .expect("seek to 0.3 s");
}

/// Ambisonic layouts, which `swresample` refuses, decode as four channels in order,
/// assumed the count's default.
#[test]
fn an_ambisonic_stream_decodes_as_its_channels() {
    let mut decoder =
        NativeAudioDecoder::open(corpus("audio-ambisonic.mov"), 48_000).expect("open");
    assert_eq!(decoder.channels(), 4);
    assert!(decoder.speakers_assumed());
    let blocks = decode_audio_to_end(&mut decoder).expect("ambisonic decode");
    let frames = blocks
        .iter()
        .map(|block| block.channels[0].len())
        .sum::<usize>();
    assert_eq!(frames, 4_800);
    let middle = &blocks[0];
    for (channel, level) in [0.4_f32, 0.3, 0.2, 0.1].into_iter().enumerate() {
        let sample = middle.channels[channel][100];
        assert!((sample - level).abs() < 1e-3, "channel {channel}: {sample}");
    }
}

#[test]
fn an_audio_decoder_opens_its_container_once() {
    let opened = || crate::decode::INPUTS_OPENED.with(std::cell::Cell::get);
    let before = opened();
    let mut decoder =
        NativeAudioDecoder::open(corpus("h264-bt709-copy.mp4"), 48_000).expect("open");
    assert_eq!(opened() - before, 1, "open");
    let cancel = CancelToken::default();
    decoder
        .seek(seek_request(6_000), &cancel)
        .expect("seek near the start");
    assert_eq!(
        opened() - before,
        1,
        "a seek to the start of an unread input"
    );
    decoder.decode_next(&cancel).expect("next block");
    decoder
        .seek(seek_request(0), &cancel)
        .expect("seek back to the start");
    assert_eq!(
        opened() - before,
        2,
        "back to the start of an input already read"
    );
}

/// Exports a tone at `rate` through the A/V sink beside one video frame, returning the file.
fn export_av_audio(label: &str, rate: u32, samples: usize) -> PathBuf {
    let path = output_path(label);
    let _ = fs::remove_file(&path);
    let mut sink = NativeAvEncodeSink::create(&path, av_config(rate)).expect("av sink");
    let cancel = CancelToken::default();
    sink.write(
        EncodeEvent::Video(rgba_frame(MediaTime::ZERO, [0, 0, 0, 255])),
        &cancel,
    )
    .expect("video");
    let mut first = 0;
    while first < samples {
        let count = (samples - first).min(4_096);
        sink.write(
            EncodeEvent::Audio(tone_block(
                i64::try_from(first).expect("small"),
                count,
                rate,
            )),
            &cancel,
        )
        .expect("audio");
        first += count;
    }
    sink.finish(&cancel).expect("finish");
    path
}

#[test]
fn every_project_rate_exports_aac_at_a_rate_aac_carries() {
    for (label, path) in [
        (
            "rate-50k-av",
            export_av_audio("rate-50k-av", 50_000, 50_000),
        ),
        (
            "rate-50k-mux",
            export_muxed_audio("rate-50k-mux", 50_000, 50_000),
        ),
    ] {
        assert_eq!(exported_audio(&path), (64_000, 64_000), "{label}");
        let (_, decoded) = decode_audio_path_linearly(&path, 64_000);
        let tone = rms_between(&decoded, 0, 6_400, 57_600);
        assert!(
            (tone - 0.25 / 2_f32.sqrt()).abs() < 0.01,
            "{label}: RMS {tone}"
        );
        fs::remove_file(path).expect("cleanup");
    }
}

#[test]
fn exported_aac_ends_where_its_audio_does() {
    for samples in [48_000, 2_048, 1_600] {
        for (kind, path) in [
            (
                "av",
                export_av_audio(&format!("tail-av-{samples}"), 48_000, samples),
            ),
            (
                "mux",
                export_muxed_audio(&format!("tail-mux-{samples}"), 48_000, samples),
            ),
        ] {
            assert_eq!(
                exported_audio(&path),
                (48_000, i64::try_from(samples).expect("small")),
                "{kind}, {samples} samples"
            );
            fs::remove_file(path).expect("cleanup");
        }
    }
}

#[test]
fn a_wav_refuses_the_block_that_would_pass_the_riff_limit() {
    let path = output_path("riff-limit").with_extension("wav");
    let _ = fs::remove_file(&path);
    // Room for the header and 1 000 stereo frames.
    crate::encode::RIFF_LIMIT.with(|limit| limit.set(36 + 1_000 * 8));
    let cancel = CancelToken::default();
    let mut sink = NativeWavEncodeSink::create(&path, 48_000, 2).expect("wav");
    sink.write_audio(&tone_block(0, 1_000, 48_000), &cancel)
        .expect("up to the limit");
    let written = fs::metadata(partial_beside(&path)).map_or(0, |file| file.len());
    let refused = sink.write_audio(&tone_block(1_000, 1, 48_000), &cancel);
    crate::encode::RIFF_LIMIT.with(|limit| limit.set(u64::from(u32::MAX)));
    let error = refused.expect_err("one frame past the limit");
    assert!(error.to_string().contains("4 GiB"), "{error}");
    assert_eq!(
        fs::metadata(partial_beside(&path)).map_or(0, |file| file.len()),
        written,
        "nothing more was written"
    );
    drop(sink);
    assert!(!path.exists() && !partial_beside(&path).exists());
    assert!(crate::encode::wav_header(48_000, 2, u32::MAX - 35).is_err());
    assert!(crate::encode::wav_header(48_000, 2, u32::MAX - 36).is_ok());
}

#[test]
fn each_audio_stream_opens_by_its_position_among_the_audio_streams() {
    let path = corpus("audio-two-streams.mov");
    let probed = crate::probe(&path).expect("probe");
    let streams = probed.audio_streams().collect::<Vec<_>>();
    assert_eq!(
        streams
            .iter()
            .map(|stream| stream.channels)
            .collect::<Vec<_>>(),
        [Some(2), Some(4)]
    );
    assert_eq!(streams[1].language.as_deref(), Some("eng"));
    assert!(probed.selected_audio_ordinal().is_some());
    let cancel = CancelToken::default();
    for (stream, levels) in [(0, &[0.1_f32, 0.2][..]), (1, &[0.3, 0.4, 0.5, 0.6][..])] {
        let mut decoder =
            NativeAudioDecoder::open_stream(&path, 48_000, Some(stream)).expect("decoder");
        let DecodeEvent::Audio(block) = decoder.decode_next(&cancel).expect("block") else {
            panic!("expected audio");
        };
        for (plane, level) in block.channels.iter().zip(levels) {
            assert_eq!(
                plane[0],
                (level * 32_768.0).round() / 32_768.0,
                "stream {stream}"
            );
        }
        assert_eq!(block.channels.len(), levels.len(), "stream {stream}");
    }
    let missing = NativeAudioDecoder::open_stream(&path, 48_000, Some(2)).err();
    assert!(
        matches!(&missing, Some(DecodeError::Unsupported(reason)) if reason.contains("no audio stream 2")),
        "{:?}",
        missing.map(|error| error.to_string())
    );
}

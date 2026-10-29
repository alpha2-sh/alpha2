//! Output files: partial files, replacing writes, and file size limits.

use super::*;

#[test]
fn an_audio_encode_thread_that_cannot_start_fails_the_open_and_leaves_no_partial() {
    // At a process or cgroup thread limit the open fails rather than panicking mid-open, and no
    // header-only partial is left to block a retry.
    let path = output_path("audio-thread-refused");
    let _ = fs::remove_file(&path);
    crate::encode::REFUSE_AUDIO_ENCODE_THREAD.set(true);
    let error = NativeAvEncodeSink::create(&path, av_config(48_000))
        .err()
        .expect("the open fails");
    assert!(
        matches!(&error, EncodeError::Adapter(message) if message.contains("failed to start the audio encode thread")),
        "{error:?}"
    );
    assert!(!partial_beside(&path).exists());
    assert!(!path.exists());
    let sink = NativeAvEncodeSink::create(&path, av_config(48_000)).expect("the retry opens");
    drop(sink);
    assert!(!partial_beside(&path).exists());

    let muxed = output_path("audio-thread-refused-mux").with_extension("mp4");
    let _ = fs::remove_file(&muxed);
    let video = mp4_video_encoder();
    let audio = crate::AudioStreamConfig {
        codec: crate::NativeAudioCodec::Aac,
        sample_rate: 48_000,
        channels: 2,
        bit_rate: 128_000,
    };
    crate::encode::REFUSE_AUDIO_ENCODE_THREAD.set(true);
    let error = crate::mux::Muxer::open(
        &muxed,
        alpha2_media::Container::Mp4,
        &video,
        ra(1, 30),
        Some(audio),
    )
    .err()
    .expect("the muxer's open fails");
    assert!(
        matches!(&error, EncodeError::Adapter(message) if message.contains("failed to start the audio encode thread")),
        "{error:?}"
    );
    assert!(!partial_beside(&muxed).exists());
    drop(
        crate::mux::Muxer::open(
            &muxed,
            alpha2_media::Container::Mp4,
            &video,
            ra(1, 30),
            Some(audio),
        )
        .expect("the retry opens"),
    );
    assert!(!partial_beside(&muxed).exists());
}

#[test]
fn an_output_that_fails_to_open_removes_the_partial_it_created() {
    // The partial is created before the encoders open; a failure in between must remove it, or the
    // next export to that path is refused as stale.
    let path = output_path("av-encoder-refused");
    let _ = fs::remove_file(&path);
    let error = NativeAvEncodeSink::create_with_video_encoder(
        &path,
        av_config(48_000),
        "alpha2_no_such_encoder",
    )
    .err()
    .expect("no such encoder");
    assert!(matches!(error, EncodeError::Adapter(_)), "{error:?}");
    assert!(!partial_beside(&path).exists());

    let video = output_path("encoder-refused");
    let _ = fs::remove_file(&video);
    let error = NativeVideoEncodeSink::create_selected(
        &video,
        av_config(48_000).video,
        Some("alpha2_no_such_encoder"),
        &|| 1,
    )
    .err()
    .expect("no such encoder");
    assert!(matches!(error, EncodeError::Adapter(_)), "{error:?}");
    assert!(!partial_beside(&video).exists());

    // The muxer claims its partial before it checks the streams against the container.
    let muxed = output_path("not-offered").with_extension("mov");
    let _ = fs::remove_file(&muxed);
    let encoder = mp4_video_encoder();
    let error = crate::mux::Muxer::open(
        &muxed,
        alpha2_media::Container::Mov,
        &encoder,
        ra(1, 30),
        Some(crate::AudioStreamConfig {
            codec: crate::NativeAudioCodec::Opus,
            sample_rate: 48_000,
            channels: 2,
            bit_rate: 96_000,
        }),
    )
    .err()
    .expect("MOV offers no Opus");
    assert!(
        matches!(&error, EncodeError::Adapter(message) if message.contains("mov does not offer audio codec opus")),
        "{error:?}"
    );
    assert!(!partial_beside(&muxed).exists());
}

/// Each transactional output, opened at `path`, as the value that holds its
/// partial file until it is dropped.
fn open_each_output(
    target: impl Into<crate::OutputTarget>,
    kind: &str,
) -> Result<Box<dyn std::any::Any>, EncodeError> {
    let target = target.into();
    Ok(match kind {
        "av" => Box::new(NativeAvEncodeSink::create(&target, av_config(48_000))?),
        "video" => Box::new(NativeVideoEncodeSink::create(
            &target,
            av_config(48_000).video,
        )?),
        "wav" => Box::new(NativeWavEncodeSink::create(&target, 48_000, 2)?),
        "mux" => Box::new(crate::mux::Muxer::open(
            &target,
            alpha2_media::Container::Mp4,
            &mp4_video_encoder(),
            ra(1, 30),
            None,
        )?),
        other => unreachable!("{other}"),
    })
}

const OUTPUT_KINDS: [(&str, &str); 4] = [
    ("av", "mp4"),
    ("video", "mp4"),
    ("wav", "wav"),
    ("mux", "mp4"),
];

#[test]
fn a_partial_file_no_running_export_holds_is_taken_over() {
    // A host ended by SIGKILL, the OOM killer, or a crash removes nothing, so a later output to
    // that path takes over the partial it left rather than refusing it as stale.
    for (kind, extension) in OUTPUT_KINDS {
        let path = output_path(&format!("stale-partial-{kind}")).with_extension(extension);
        let _ = fs::remove_file(&path);
        let partial = partial_beside(&path);
        fs::write(&partial, b"what a killed export left").expect("stale partial");
        let output = open_each_output(&path, kind)
            .unwrap_or_else(|error| panic!("{kind}: a stale partial refused the output: {error}"));
        assert!(
            !fs::read(&partial)
                .expect("the output's partial")
                .starts_with(b"what a killed"),
            "{kind}: the stale bytes were not discarded"
        );
        drop(output);
        assert!(!partial.exists(), "{kind}: the partial outlived its output");
        assert!(!path.exists(), "{kind}");
    }
    // Taken over, a stale partial becomes the file a finished output is renamed from.
    let path = output_path("stale-partial-finished").with_extension("wav");
    let _ = fs::remove_file(&path);
    fs::write(partial_beside(&path), [0xAB_u8; 4096]).expect("stale partial");
    let cancel = CancelToken::default();
    let mut sink = NativeWavEncodeSink::create(&path, 48_000, 1).expect("the sink opens");
    sink.write_audio(
        &AudioBlock {
            start_sample: 0,
            sample_rate: 48_000,
            speakers: alpha2_media::SpeakerLayout::MONO,
            channels: vec![vec![0.5_f32; 4]],
        },
        &cancel,
    )
    .expect("block");
    sink.finish_audio(&cancel).expect("finish");
    let mut expected = crate::encode::wav_header(48_000, 1, 16)
        .expect("header")
        .to_vec();
    for _ in 0..4 {
        expected.extend_from_slice(&0.5_f32.to_le_bytes());
    }
    assert_eq!(fs::read(&path).expect("wav"), expected);
    assert!(!partial_beside(&path).exists());
    fs::remove_file(path).expect("cleanup");
}

#[test]
fn an_existing_output_is_refused_unless_the_output_replaces_it() {
    for (kind, extension) in OUTPUT_KINDS {
        let path = output_path(&format!("existing-{kind}")).with_extension(extension);
        fs::write(&path, b"old").expect("old file");
        let error = open_each_output(&path, kind)
            .err()
            .unwrap_or_else(|| panic!("{kind}: an existing output was opened"));
        assert!(
            error.to_string().contains("refusing to overwrite"),
            "{kind}: {error}"
        );
        assert!(!partial_beside(&path).exists(), "{kind}");
        // Replacing, the old file stays whole until the output is renamed over it: an output that
        // ends without finishing (a failure or a cancel) leaves it.
        let output = open_each_output(crate::OutputTarget::replacing(&path), kind)
            .unwrap_or_else(|error| panic!("{kind}: replacing refused: {error}"));
        assert_eq!(fs::read(&path).expect("old file"), b"old", "{kind}");
        drop(output);
        assert_eq!(fs::read(&path).expect("old file"), b"old", "{kind}");
        assert!(!partial_beside(&path).exists(), "{kind}");
        fs::remove_file(&path).expect("cleanup");
    }
}

#[test]
fn a_replacing_output_renames_over_the_old_file() {
    let path = output_path("replacing-finished").with_extension("wav");
    fs::write(&path, b"old").expect("old file");
    #[cfg(unix)]
    let old_inode = std::os::unix::fs::MetadataExt::ino(&fs::metadata(&path).expect("old"));
    let cancel = CancelToken::default();
    let mut sink = NativeWavEncodeSink::create(crate::OutputTarget::replacing(&path), 48_000, 1)
        .expect("the sink opens");
    sink.write_audio(
        &AudioBlock {
            start_sample: 0,
            sample_rate: 48_000,
            speakers: alpha2_media::SpeakerLayout::MONO,
            channels: vec![vec![0.5_f32; 4]],
        },
        &cancel,
    )
    .expect("block");
    assert_eq!(fs::read(&path).expect("old file"), b"old");
    sink.finish_audio(&cancel).expect("finish");
    let written = fs::read(&path).expect("new file");
    assert!(
        written.starts_with(b"RIFF"),
        "the old bytes are still there"
    );
    // A rename, not a truncation: whoever has the old file open keeps its bytes.
    #[cfg(unix)]
    assert_ne!(
        std::os::unix::fs::MetadataExt::ino(&fs::metadata(&path).expect("new")),
        old_inode
    );
    assert!(!partial_beside(&path).exists());
    fs::remove_file(path).expect("cleanup");
}

#[test]
fn a_replacing_output_refuses_a_directory_or_a_symbolic_link_before_writing() {
    let directory = output_path("replacing-directory-output");
    let _ = fs::remove_dir(&directory);
    fs::create_dir(&directory).expect("directory");
    for (kind, _) in OUTPUT_KINDS {
        let error = open_each_output(crate::OutputTarget::replacing(&directory), kind)
            .err()
            .unwrap_or_else(|| panic!("{kind}: a directory was opened as an output"));
        assert!(
            error.to_string().contains("not a regular file"),
            "{kind}: {error}"
        );
        assert!(!partial_beside(&directory).exists(), "{kind}");
    }
    fs::remove_dir(&directory).expect("cleanup");
    #[cfg(unix)]
    {
        let target = output_path("replacing-link-target").with_extension("wav");
        let link = output_path("replacing-link").with_extension("wav");
        fs::write(&target, b"linked").expect("target");
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(&target, &link).expect("link");
        let error = open_each_output(crate::OutputTarget::replacing(&link), "wav")
            .expect_err("a symbolic link was opened as an output");
        assert!(error.to_string().contains("not a regular file"), "{error}");
        assert!(!partial_beside(&link).exists());
        assert_eq!(fs::read(&target).expect("target"), b"linked");
        fs::remove_file(&link).expect("cleanup");
        fs::remove_file(&target).expect("cleanup");
    }
}

#[test]
fn a_partial_file_a_running_export_holds_is_refused_by_name() {
    for (kind, extension) in OUTPUT_KINDS {
        let path = output_path(&format!("held-partial-{kind}")).with_extension(extension);
        let _ = fs::remove_file(&path);
        let partial = partial_beside(&path);
        let running = open_each_output(&path, kind).expect("the first output opens");
        let error = open_each_output(&path, kind)
            .err()
            .unwrap_or_else(|| panic!("{kind}: two outputs wrote one partial file"));
        let message = error.to_string();
        assert!(
            message.contains("another export is writing this output")
                && message.contains(&partial.display().to_string()),
            "{kind}: {message}"
        );
        assert!(
            partial.exists(),
            "{kind}: the refusal removed the running output's partial"
        );
        drop(running);
        assert!(!partial.exists(), "{kind}");
        drop(
            open_each_output(&path, kind).expect("the path is free once the first output is gone"),
        );
    }
}

/// A whole-file output (the CLI's PNG) replaces only a regular file, and only
/// once it is written: a failed write leaves the old file and no partial, a
/// partial a running export holds is refused, and a directory is never
/// renamed over.
#[test]
fn a_replacing_write_keeps_the_old_file_until_it_finishes() {
    let path = output_path("replacing-write").with_extension("png");
    let partial = partial_beside(&path);
    fs::write(&path, b"old").expect("old file");
    let error = crate::write_whole(crate::OutputTarget::replacing(&path), |file| {
        std::io::Write::write_all(file, b"half").map_err(|error| error.to_string())?;
        Err("the disk is full".into())
    })
    .expect_err("a failed write fails");
    assert!(error.to_string().contains("the disk is full"), "{error}");
    assert_eq!(fs::read(&path).expect("old file"), b"old");
    assert!(!partial.exists());

    let held_final = output_path("replacing-held").with_extension("png");
    let _ = fs::remove_file(&held_final);
    let held =
        crate::partial::PartialOutput::claim(&(&held_final).into()).expect("a running output");
    let error = crate::write_whole(crate::OutputTarget::replacing(&held_final), |_| Ok(()))
        .expect_err("a held partial");
    assert!(
        error.to_string().contains(crate::PARTIAL_OUTPUT_IN_USE),
        "{error}"
    );
    drop(held);

    crate::write_whole(crate::OutputTarget::replacing(&path), |file| {
        std::io::Write::write_all(file, b"new").map_err(|error| error.to_string())
    })
    .expect("a regular file is replaced");
    assert_eq!(fs::read(&path).expect("new file"), b"new");
    assert!(!partial.exists());
    fs::remove_file(&path).expect("cleanup");

    let directory = output_path("replacing-directory").with_extension("png");
    fs::create_dir(&directory).expect("directory");
    let error = crate::write_whole(crate::OutputTarget::replacing(&directory), |_| Ok(()))
        .expect_err("a directory");
    assert!(error.to_string().contains("not a regular file"), "{error}");
    assert!(directory.is_dir() && !partial_beside(&directory).exists());
    fs::remove_dir(directory).expect("cleanup");
}

#[test]
fn removing_a_stale_partial_leaves_a_running_exports_partial() {
    let path = output_path("remove-stale-partial");
    let _ = fs::remove_file(&path);
    let partial = partial_beside(&path);
    assert!(!crate::remove_stale_partial_output(&path).expect("nothing to remove"));
    let running = NativeAvEncodeSink::create(&path, av_config(48_000)).expect("sink");
    let refused = crate::remove_stale_partial_output(&path);
    assert!(
        matches!(&refused, Err(EncodeError::OutputInUse(message)) if message.starts_with(crate::PARTIAL_OUTPUT_IN_USE)),
        "{refused:?}"
    );
    assert!(partial.exists(), "a running export's partial was removed");
    drop(running);
    fs::write(&partial, b"stale").expect("stale partial");
    assert!(crate::remove_stale_partial_output(&path).expect("stale partial"));
    assert!(!partial.exists());
}

/// HLS playlists and `ffconcat` lists name other files, which their
/// demuxers open: an asset in either form would read media its request
/// never named. Neither is a container alpha2 reads, so neither opens,
/// whatever the file is called.
#[test]
fn a_source_that_names_other_files_is_refused() {
    let named = corpus("h264-bt709-copy.mp4");
    let directory = output_path("names-other-files").with_extension("");
    fs::create_dir_all(&directory).expect("scratch directory");
    fs::copy(&named, directory.join("sibling.mp4")).expect("sibling");
    let playlist = directory.join("upload.m3u8");
    fs::write(
        &playlist,
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2.0,\n{}\n#EXT-X-ENDLIST\n",
            named.canonicalize().expect("absolute").display()
        ),
    )
    .expect("playlist");
    // Chosen by its content, under any name.
    let list = directory.join("upload.mp4");
    fs::write(&list, "ffconcat version 1.0\nfile sibling.mp4\n").expect("list");
    for (path, demuxer) in [(&playlist, "hls"), (&list, "concat")] {
        let probed = probe(path);
        assert!(
            matches!(&probed, Err(DecodeError::Unsupported(message)) if message.contains(demuxer)),
            "{demuxer}: {probed:?}"
        );
        assert!(NativeDecoder::open(path).is_err(), "{demuxer}");
        assert!(NativeAudioDecoder::open(path, 48_000).is_err(), "{demuxer}");
    }
    fs::remove_dir_all(directory).expect("cleanup");
}

/// RGBA noise: frames an encoder cannot make small.
fn noise_frame(pts: MediaTime, seed: u32, width: u32, height: u32) -> DecodedVideoFrame {
    let mut frame = rgba_frame_sized(pts, [0, 0, 0, 255], width, height);
    let mut state = seed.wrapping_mul(2_654_435_761).max(1);
    let bytes = (0..width * height * 4)
        .map(|index| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            if index % 4 == 3 {
                255
            } else {
                state.to_le_bytes()[0]
            }
        })
        .collect::<Vec<u8>>();
    frame.planes[0].bytes = bytes.into();
    frame
}

/// Writes into `output` with the sink named `sink` until the export ends.
fn export_until_it_fails(sink: &str, output: &Path) -> Result<(), EncodeError> {
    let cancel = CancelToken::default();
    let frames = (0..90).map(|index| {
        EncodeEvent::Video(noise_frame(
            MediaTime::from_ticks(i64::from(index) * alpha2_time::TICKS_PER_SECOND / 30),
            index,
            128,
            128,
        ))
    });
    let video = VideoEncodeConfig {
        width: 128,
        height: 128,
        rate_control: VideoRateControl::bitrate(20_000_000, BitrateMode::Variable).unwrap(),
        ..av_config(48_000).video
    };
    match sink {
        "video" => {
            let mut sink = NativeVideoEncodeSink::create(output, video)?;
            for frame in frames {
                sink.write(frame, &cancel)?;
            }
            sink.finish(&cancel)
        }
        "av" => {
            let mut sink = NativeAvEncodeSink::create(
                output,
                AvEncodeConfig {
                    video,
                    ..av_config(48_000)
                },
            )?;
            for (index, frame) in frames.enumerate() {
                sink.write(frame, &cancel)?;
                sink.write(
                    EncodeEvent::Audio(AudioBlock {
                        start_sample: i64::try_from(index).unwrap() * 1_600,
                        sample_rate: 48_000,
                        speakers: alpha2_media::SpeakerLayout::STEREO,
                        channels: vec![vec![0.0; 1_600]; 2],
                    }),
                    &cancel,
                )?;
            }
            sink.finish(&cancel)
        }
        "copy" => {
            let source = corpus("h264-bt709-copy.mp4");
            let frame_times = (0..60)
                .map(|frame| MediaTime::from_ticks(frame * alpha2_time::TICKS_PER_SECOND / 30))
                .collect::<Vec<_>>();
            let plan = crate::plan_packet_copy(&crate::PacketCopyRequest {
                source: &source,
                frame_times: &frame_times,
                width: 128,
                height: 96,
                codec: NativeVideoCodec::H264,
                color: alpha2_media::ColorMetadata::EXPORT,
                output_start: MediaTime::ZERO,
                audio: Some(crate::PacketCopyAudio {
                    sample_rate: 48_000,
                    channels: 2,
                }),
                container: alpha2_media::Container::Mp4,
            })
            .expect("plan");
            assert!(plan.copies_audio());
            let copy = crate::PacketCopy::join(vec![plan]).expect("one run");
            crate::copy_packets(
                &copy,
                output,
                alpha2_media::Container::Mp4,
                crate::CopyAudio::Source,
                &cancel,
                &mut |_| {},
            )
            .map(|_| ())
        }
        other => panic!("no sink {other}"),
    }
}

/// The child half of
/// `a_write_past_the_file_size_limit_fails_the_export_and_leaves_no_partial`,
/// which runs it under a file size limit.
#[test]
#[ignore = "run by a_write_past_the_file_size_limit_fails_the_export_and_leaves_no_partial in a child process whose file size is limited"]
fn export_into_a_file_size_limit() {
    let output = PathBuf::from(std::env::var_os("ALPHA2_FSIZE_OUTPUT").expect("run by its parent"));
    let sink = std::env::var("ALPHA2_FSIZE_SINK").expect("run by its parent");
    let error = export_until_it_fails(&sink, &output).expect_err("the export outgrew the limit");
    println!("{sink} failed: {error}");
    assert!(!output.exists(), "{sink} left an output");
    assert!(!partial_beside(&output).exists(), "{sink} left its partial");
}

/// A write that fails (a full disk, a quota, a file size limit) leaves an
/// error in the output's I/O context, which rsmpeg's `Drop` would unwrap when
/// the failed output is dropped, panicking the export (on the host's main
/// thread for a copy or an output that cannot start) and leaving the partial
/// file. The export fails instead and removes it. Each sink runs in a child
/// process whose file size limit fails a write at once (the header) or
/// part-way.
#[cfg(target_os = "linux")]
#[test]
fn a_write_past_the_file_size_limit_fails_the_export_and_leaves_no_partial() {
    for sink in ["video", "av", "copy"] {
        for limit in [0_u64, 16_384] {
            let output = output_path(&format!("fsize-{sink}-{limit}"));
            let _ = fs::remove_file(&output);
            let _ = fs::remove_file(partial_beside(&output));
            // An ignored SIGXFSZ stays ignored across exec, so the write past the limit fails with
            // EFBIG rather than killing the process.
            let child = std::process::Command::new("sh")
                .args(["-c", "trap '' XFSZ; exec prlimit --fsize=\"$0\" \"$@\""])
                .arg(limit.to_string())
                .arg(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "tests::outputs::export_into_a_file_size_limit",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("ALPHA2_FSIZE_OUTPUT", &output)
                .env("ALPHA2_FSIZE_SINK", sink)
                .output()
                .expect("child");
            let stdout = String::from_utf8_lossy(&child.stdout);
            let stderr = String::from_utf8_lossy(&child.stderr);
            let _ = fs::remove_file(&output);
            let _ = fs::remove_file(partial_beside(&output));
            assert!(
                child.status.success() && stdout.contains("1 passed"),
                "{sink} at {limit} bytes: {}\n{stdout}\n{stderr}",
                child.status
            );
            assert!(!stderr.contains("panicked"), "{sink} at {limit}: {stderr}");
        }
    }
}

/// A file saved at the output path during a long export is left as it is,
/// not replaced by the partial file's rename, and the export fails and
/// removes its partial file.
#[test]
fn a_file_that_appears_at_the_output_during_an_export_is_not_replaced() {
    let cancel = CancelToken::default();
    for (kind, extension) in OUTPUT_KINDS {
        let path = output_path(&format!("appeared-{kind}")).with_extension(extension);
        let _ = fs::remove_file(&path);
        let error = (|| -> Result<(), EncodeError> {
            match kind {
                "av" => {
                    let mut sink = NativeAvEncodeSink::create(&path, av_config(48_000))?;
                    sink.write(
                        EncodeEvent::Video(rgba_frame(MediaTime::ZERO, [0; 4])),
                        &cancel,
                    )?;
                    fs::write(&path, b"someone else's").expect("appear");
                    sink.finish(&cancel)
                }
                "video" => {
                    let mut sink = NativeVideoEncodeSink::create(&path, av_config(48_000).video)?;
                    sink.write(
                        EncodeEvent::Video(rgba_frame(MediaTime::ZERO, [0; 4])),
                        &cancel,
                    )?;
                    fs::write(&path, b"someone else's").expect("appear");
                    sink.finish(&cancel)
                }
                "wav" => {
                    let mut sink = NativeWavEncodeSink::create(&path, 48_000, 2)?;
                    sink.write_audio(&tone_block(0, 480, 48_000), &cancel)?;
                    fs::write(&path, b"someone else's").expect("appear");
                    sink.finish_audio(&cancel)
                }
                "mux" => {
                    let mut muxer = crate::mux::Muxer::open(
                        &path,
                        alpha2_media::Container::Mp4,
                        &mp4_video_encoder(),
                        ra(1, 30),
                        None,
                    )?;
                    fs::write(&path, b"someone else's").expect("appear");
                    muxer.close()
                }
                other => unreachable!("{other}"),
            }
        })()
        .expect_err("the export does not replace the file");
        assert!(error.to_string().contains("appeared"), "{kind}: {error}");
        assert_eq!(fs::read(&path).expect("kept"), b"someone else's", "{kind}");
        assert!(!partial_beside(&path).exists(), "{kind}: partial removed");
        fs::remove_file(&path).expect("cleanup");
    }
}

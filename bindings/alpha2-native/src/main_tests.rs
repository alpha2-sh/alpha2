use super::*;

/// A rate-control child's answer survives its line, and a child that
/// crashed or exited without one is told apart.
#[cfg(all(target_os = "windows", feature = "hwencode"))]
#[test]
fn a_rate_control_child_answers_in_one_line() {
    let opened = rate_control_answer_line(&Ok(2));
    assert_eq!(opened, "opened 2");
    assert_eq!(
        rate_control_answer(Some(0), &format!("\n{opened}\n")),
        Ok(2)
    );
    let refused = rate_control_answer_line(&Err(
        "Generic error in an external library\r\non two lines".to_owned(),
    ));
    assert_eq!(
        rate_control_answer(Some(0), &refused),
        Err("Generic error in an external library  on two lines".to_owned())
    );
    assert_eq!(
        rate_control_answer(Some(0xC000_0005_u32.cast_signed()), ""),
        Err("the open crashed its process (exit 0xC0000005)".to_owned())
    );
    assert_eq!(
        rate_control_answer(Some(1), "usage"),
        Err("the open's process exited with 1 and no answer".to_owned())
    );
    assert_eq!(
        rate_control_answer(Some(0), "opened many"),
        Err("the open's process exited with 0 and no answer".to_owned())
    );
}

#[test]
fn the_waveform_command_summarizes_a_source_at_its_own_rate() {
    let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus");
    let path = corpus.join("audio-adts.aac");
    let json = waveform_json(
        &Decoding::default(),
        path.to_str().expect("UTF-8 path"),
        20.0,
        3,
    )
    .expect("summary");
    // 48 kHz, 20 ms buckets: 960 samples, then 3840 and 15360.
    assert!(
        json.starts_with(r#"{"sampleRate":48000,"channels":1,"startSample":0,"frames":"#),
        "{json}"
    );
    for level in [960, 3_840, 15_360] {
        assert!(
            json.contains(&format!(r#"{{"bucketSamples":{level},"#)),
            "{level}"
        );
    }
    assert!(!json.contains("null"), "decoded audio is finite");
    assert!(
        waveform_json(
            &Decoding::default(),
            path.to_str().expect("UTF-8 path"),
            0.0,
            3
        )
        .is_err()
    );
}

fn first_frame(path: &std::path::Path) -> DecodedVideoFrame {
    let mut decoder = NativeDecoder::open(path).expect("open");
    loop {
        match decoder
            .decode_next(&CancelToken::default())
            .expect("decode")
        {
            DecodeEvent::Video(frame) => break frame,
            DecodeEvent::EndOfStream => panic!("no video frame"),
            DecodeEvent::Retry | DecodeEvent::Audio(_) => {}
        }
    }
}

/// The mean absolute difference of two equally sized RGB pictures.
#[allow(clippy::cast_precision_loss)]
fn mean_difference(rendered: &[u8], expected: &[u8], expected_channels: usize) -> f64 {
    let (sum, count) = rendered
        .as_chunks::<4>()
        .0
        .iter()
        .zip(expected.chunks_exact(expected_channels))
        .flat_map(|(actual, expected)| actual[..3].iter().zip(&expected[..3]))
        .fold((0_u64, 0_u64), |(sum, count), (&actual, &expected)| {
            (sum + u64::from(actual.abs_diff(expected)), count + 1)
        });
    sum as f64 / count as f64
}

/// A frame with a display matrix renders the way `FFmpeg`'s autorotate
/// shows it: turned, and mirrored too where the matrix mirrors.
#[test]
fn rotated_render_matches_ffmpeg_autorotate() {
    let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus");
    for (source, reference) in [
        ("rotation.mp4", "rotation-autorotate.png"),
        ("h264-bt709-hflip.mp4", "h264-bt709-hflip-autorotate.png"),
        ("h264-bt709-vflip.mp4", "h264-bt709-vflip-autorotate.png"),
        (
            "h264-bt709-transverse.mp4",
            "h264-bt709-transverse-autorotate.png",
        ),
    ] {
        let (width, height, rendered) =
            png_pixels(&first_frame(&corpus.join(source))).expect("render");
        let reference = png::Decoder::new(std::io::BufReader::new(
            std::fs::File::open(corpus.join(reference)).expect("reference"),
        ));
        let mut reader = reference.read_info().expect("reference header");
        let mut expected = vec![0; reader.output_buffer_size().expect("reference size")];
        let info = reader.next_frame(&mut expected).expect("reference pixels");
        assert_eq!(info.color_type, png::ColorType::Rgb);
        assert_eq!((width, height), (info.width, info.height), "{source}");
        let mean = mean_difference(&rendered, &expected, 3);
        // What remains is chroma upsampling: this CLI samples nearest, FFmpeg interpolates.
        assert!(mean < 8.0, "{source}: mean absolute difference {mean:.1}");
    }
}

/// The PNG `thumbnail` and `render` write is the picture as shown, not
/// the stored one: a 16:15 PAL frame (720x576 shown 768x576) is stretched
/// to square pixels along the stored rows, before it is turned, so a
/// turned source is stretched along its columns as shown.
#[test]
fn a_png_shows_non_square_pixels_square() {
    let (width, height, upright) =
        png_pixels(&first_frame(std::path::Path::new(&corpus("sar-16-15.mp4")))).expect("render");
    assert_eq!((width, height), (64, 48));
    let (turned_width, turned_height, turned) = png_pixels(&first_frame(std::path::Path::new(
        &corpus("sar-16-15-rotation.mp4"),
    )))
    .expect("render");
    assert_eq!((turned_width, turned_height), (48, 64));
    // Turned a quarter counter-clockwise: the shown picture's row y is the upright one's
    // column 63 - y.
    for y in 0..64_usize {
        for x in 0..48_usize {
            let shown = &turned[(y * 48 + x) * 4..][..4];
            let source = &upright[(x * 64 + 63 - y) * 4..][..4];
            assert_eq!(shown, source, "({x}, {y})");
        }
    }
    // A codec's ratio (4:3, over 128x96) and a container's (`pasp`, 4:3 over the codec's 1:1).
    for (name, size) in [
        ("h264-bt709-vui-sar.mp4", (171, 96)),
        ("h264-bt709-pasp.mp4", (171, 96)),
    ] {
        let (width, height, _) =
            png_pixels(&first_frame(std::path::Path::new(&corpus(name)))).expect("render");
        assert_eq!((width, height), size, "{name}");
    }
    let output = scratch("sar-thumbnail", "png");
    thumbnail(
        &Decoding::default(),
        &corpus("sar-16-15.mp4"),
        [output.clone(), "--at".to_owned(), "0".to_owned()].into_iter(),
    )
    .expect("thumbnail");
    let decoder = png::Decoder::new(std::io::BufReader::new(
        std::fs::File::open(&output).expect("png"),
    ));
    let info = decoder.read_info().expect("header").info().clone();
    assert_eq!((info.width, info.height), (64, 48));
    std::fs::remove_file(output).expect("cleanup");
}

/// Eight JPEGs store one picture eight ways, each with the Exif
/// `Orientation` that shows it upright: all eight render alike.
#[test]
fn every_exif_orientation_renders_the_same_upright_picture() {
    let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus");
    let (width, height, upright) =
        png_pixels(&first_frame(&corpus.join("exif-orientation-1.jpg"))).expect("render");
    assert_eq!((width, height), (64, 48));
    for value in 2..=8 {
        let name = format!("exif-orientation-{value}.jpg");
        let (shown_width, shown_height, rendered) =
            png_pixels(&first_frame(&corpus.join(&name))).expect("render");
        assert_eq!((shown_width, shown_height), (64, 48), "{name}");
        let mean = mean_difference(&rendered, &upright, 4);
        // Each stored transform codes a little differently; a wrong turn or mirror differs by
        // tens.
        assert!(mean < 2.0, "{name}: mean absolute difference {mean:.1}");
    }
}

fn corpus(name: &str) -> String {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/corpus")
        .join(name)
        .to_str()
        .expect("UTF-8 path")
        .to_owned()
}

fn scratch(label: &str, extension: &str) -> String {
    let path = std::env::temp_dir().join(format!(
        "alpha2-cli-{}-{label}.{extension}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    path.to_str().expect("UTF-8 path").to_owned()
}

fn run_transcode(source: &str, output: &str, flags: &[&str]) -> Result<(), String> {
    let args =
        std::iter::once(output.to_owned()).chain(flags.iter().map(|flag| (*flag).to_owned()));
    transcode(&Decoding::default(), &corpus(source), args)
}

fn selected_video(path: &str) -> alpha2_media::StreamInfo {
    probe(path)
        .expect("probe")
        .selected_video_stream()
        .expect("video")
        .clone()
}

/// `transcode` writes the display matrix and pixel aspect ratio the
/// source has, so a phone clip is not turned on its side and an
/// anamorphic one is not squeezed.
#[test]
fn a_transcode_shows_its_pictures_as_the_source_does() {
    for (source, flags) in [
        ("rotation.mp4", &[][..]),
        ("h264-bt709-vflip.mp4", &[][..]),
        ("h264-bt709-transverse.mp4", &[][..]),
        ("h264-bt709-pasp.mp4", &["--proxy", "half"][..]),
    ] {
        let output = scratch(&format!("shown-{source}"), "mp4");
        run_transcode(source, &output, flags).expect("transcode");
        let (expected, found) = (selected_video(&corpus(source)), selected_video(&output));
        assert_eq!(found.orientation, expected.orientation, "{source}");
        assert_eq!(
            found.sample_aspect_ratio, expected.sample_aspect_ratio,
            "{source}"
        );
        std::fs::remove_file(output).expect("cleanup");
    }
}

/// The presentation times of every video frame `path` decodes to.
fn frame_times(path: &str) -> Vec<i64> {
    let mut decoder = NativeDecoder::open(path).expect("open");
    let mut times = Vec::new();
    while let DecodeEvent::Video(frame) = decoder
        .decode_next(&CancelToken::default())
        .expect("decode")
    {
        times.push(frame.pts.ticks());
    }
    times
}

/// MPEG-4 Part 2 opens at 119.88 or 239.76 fps, and at a variable-rate
/// source's average rate, whose exact frame length needs a time base
/// denominator past its 16 bits. The output has every frame the
/// source has, in the slot of the source's rate it fills, each within the
/// encoder's unit (1/64000 s, under two ticks) of the slot's time, and
/// lasts as long as the source.
#[test]
fn a_transcode_runs_at_a_rate_mpeg4_cannot_time_exactly() {
    for source in ["rate-120-1001.mp4", "rate-240-1001.mp4", "vfr-average.mp4"] {
        let rate = selected_video(&corpus(source))
            .frame_rate
            .expect("frame rate");
        assert!(rate.numerator() > 65_535, "{source}: {rate:?}");
        let mut last = None;
        let expected = frame_times(&corpus(source))
            .into_iter()
            .filter_map(|ticks| {
                let slot = frame_slot(alpha2_time::MediaTime::from_ticks(ticks), rate);
                let fresh = last.is_none_or(|last| slot > last);
                last = Some(slot);
                fresh.then(|| slot_time(slot, rate).ticks())
            })
            .collect::<Vec<_>>();
        let output = scratch(&format!("rate-{source}"), "mp4");
        run_transcode(source, &output, &["--video-codec", "mpeg4-part2"]).expect(source);
        let written = frame_times(&output);
        assert_eq!(written.len(), expected.len(), "{source}");
        // The last frame lasts a frame, not one unit of the time base.
        let (length, source_length) = (
            selected_video(&output).duration.expect("duration").ticks(),
            selected_video(&corpus(source))
                .duration
                .expect("duration")
                .ticks(),
        );
        assert!(
            length.abs_diff(source_length) <= 2,
            "{source}: {length} ticks long, the source {source_length}"
        );
        for (index, (written, expected)) in written.iter().zip(&expected).enumerate() {
            assert!(
                written.abs_diff(*expected) <= 2,
                "{source} frame {index}: {written} ticks, the slot at {expected}"
            );
        }
        std::fs::remove_file(output).expect("cleanup");
    }
}

/// A source whose rate rises after `FFmpeg` guessed it puts two frames in
/// one slot of the guessed rate. The second is dropped, where the encoder
/// would refuse it with a bare `InvalidTimestamp` mid-file.
#[test]
fn a_variable_rate_transcode_keeps_one_frame_per_slot() {
    let output = scratch("vfr", "mp4");
    run_transcode("vfr-rising.mp4", &output, &[]).expect("transcode");
    let mut decoder = NativeDecoder::open(&output).expect("output");
    let mut frames = 0;
    while let DecodeEvent::Video(_) = decoder
        .decode_next(&CancelToken::default())
        .expect("decode")
    {
        frames += 1;
    }
    // 6 s at 30 fps, then the 1 s at 60 fps in slots 180 to 210: its last frame, at 6.983 s,
    // rounds up into slot 210.
    assert_eq!(frames, 211);
    // Windows refuses to delete a file FFmpeg still holds open.
    drop(decoder);
    std::fs::remove_file(output).expect("cleanup");
}

/// An RGB source converted to YCbCr is tagged with the matrix it was
/// converted into, not the identity matrix, which a reader shows green as
/// brown; an HDR source is refused with a message that says HDR.
#[test]
fn a_transcode_is_tagged_with_the_colour_it_writes() {
    let output = scratch("rgb", "mp4");
    run_transcode("parity-rgb.mov", &output, &[]).expect("transcode");
    let color = selected_video(&output).color.expect("tagged");
    assert_eq!(
        (color.matrix, color.range),
        (ColorMatrix::Bt709, ColorRange::Limited)
    );
    std::fs::remove_file(output).expect("cleanup");
    let refused =
        run_transcode("hdr-hlg.mp4", &scratch("hdr", "mp4"), &[]).expect_err("HDR is refused");
    assert!(refused.contains("HDR"), "{refused}");
}

/// An RGB source outside BT.709's gamut is decoded as red, green, and
/// blue planes under the identity matrix. Encoded as luma and chroma,
/// solid red would come out bright green ([48, 255, 7]) with no error;
/// `transcode` converts them with BT.709 coefficients into limited range,
/// as the output says, and keeps the source's primaries, which it does
/// not convert.
#[test]
fn a_transcode_of_red_green_and_blue_planes_keeps_their_colour() {
    for source in ["rgb-bt2020.mkv", "rgb-smpte170m.mkv", "rgb10-bt2020.mkv"] {
        let primaries = selected_video(&corpus(source))
            .color
            .expect("tagged")
            .primaries;
        for flags in [&[][..], &["--proxy", "half"][..]] {
            let output = scratch(&format!("{source}-{}", flags.len()), "mp4");
            let flags = [&["--video-codec", "mpeg4-part2"][..], flags].concat();
            run_transcode(source, &output, &flags).expect("transcode");
            let color = selected_video(&output).color.expect("tagged");
            assert_eq!(
                (color.primaries, color.matrix, color.range),
                (primaries, ColorMatrix::Bt709, ColorRange::Limited),
                "{source} {flags:?}"
            );
            let (_, _, pixels) =
                png_pixels(&first_frame(std::path::Path::new(&output))).expect("render");
            // 8-bit limited-range codes of pure red decode to within a few codes of it.
            for (channel, (&found, expected)) in pixels[..3].iter().zip([255_u8, 0, 0]).enumerate()
            {
                assert!(
                    found.abs_diff(expected) <= 8,
                    "{source} {flags:?}: first pixel {:?}, channel {channel}",
                    &pixels[..3]
                );
            }
            std::fs::remove_file(output).expect("cleanup");
        }
    }
}

/// 10-bit and other non-I420 layouts transcode through the YUV 4:2:0 converter.
#[test]
fn a_transcode_takes_a_ten_bit_source() {
    for flags in [&[][..], &["--proxy", "half"][..]] {
        let output = scratch(&format!("high10-{}", flags.len()), "mp4");
        run_transcode("h264-high10.mp4", &output, flags).expect("transcode");
        std::fs::remove_file(output).expect("cleanup");
    }
}

/// The codec can be named; an unknown one is refused with the codecs the
/// runtime encodes.
#[test]
fn a_transcode_writes_the_codec_it_is_asked_for() {
    let output = scratch("codec", "mp4");
    run_transcode("long-gop.mp4", &output, &["--video-codec", "mpeg4-part2"]).expect("transcode");
    assert_eq!(selected_video(&output).codec, "mpeg4");
    std::fs::remove_file(output).expect("cleanup");
    let refused = run_transcode(
        "long-gop.mp4",
        &scratch("no-codec", "mp4"),
        &["--video-codec", "nope"],
    )
    .expect_err("no such codec");
    assert!(
        refused.contains("encodes no nope") && refused.contains("mpeg4-part2"),
        "{refused}"
    );
}

/// A waveform's first bucket is where the stream starts on the source's
/// timeline, which the JSON says; its channels are the decoder's.
#[test]
fn a_waveform_says_where_it_starts() {
    let json =
        waveform_json(&Decoding::default(), &corpus("audio-mpegts.ts"), 10.0, 1).expect("summary");
    assert!(json.contains(r#""startSample":67200,"#), "{}", &json[..80]);
    let json =
        waveform_json(&Decoding::default(), &corpus("audio-5.1.4.wav"), 10.0, 1).expect("summary");
    assert!(
        json.starts_with(r#"{"sampleRate":48000,"channels":10,"#),
        "{}",
        &json[..80]
    );
}

/// A start that alone passes a WAV's 4 GiB is refused before anything is
/// written; an ordinary one is silence, written in pieces, not as one
/// block the size of the whole gap (a capture 12 345 s in would allocate
/// and write 4.7 GB before failing).
#[test]
fn export_audio_starts_at_the_source_zero_within_what_a_wav_holds() {
    let late = scratch("late", "wav");
    let refused = export_audio(&Decoding::default(), &corpus("audio-late-start.ts"), &late)
        .expect_err("4.7 GB of silence");
    assert!(refused.contains("starts 12346.4 s"), "{refused}");
    assert!(!std::path::Path::new(&late).exists());
    let output = scratch("ts", "wav");
    export_audio(&Decoding::default(), &corpus("audio-mpegts.ts"), &output).expect("export");
    let bytes = std::fs::read(&output).expect("wav");
    // Mono f32: 1.4 s of silence (67 200 frames) after the 44-byte header, then the tone.
    assert!(bytes[44..44 + 67_200 * 4].iter().all(|byte| *byte == 0));
    assert!(bytes[44 + 67_200 * 4..].iter().any(|byte| *byte != 0));
    std::fs::remove_file(output).expect("cleanup");
}

/// `thumbnail --at` takes a time on the source's own timeline, the
/// container's timestamps, which a project's source ranges also use: in
/// an MPEG-TS whose clock starts at 101.4 s, `--at 106.4` is frame 125
/// (5 s in), and `--at 5`, before the first frame, is that first frame.
#[test]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn thumbnail_at_is_a_time_on_the_sources_timeline() {
    let source = corpus("ts-offset-100.ts");
    let red_at = |seconds: &str| {
        let output = scratch(&format!("at-{seconds}"), "png");
        thumbnail(
            &Decoding::default(),
            &source,
            [output.clone(), "--at".to_owned(), seconds.to_owned()].into_iter(),
        )
        .expect("thumbnail");
        let decoder = png::Decoder::new(std::io::BufReader::new(
            std::fs::File::open(&output).expect("png"),
        ));
        let mut reader = decoder.read_info().expect("header");
        let mut pixels = vec![0; reader.output_buffer_size().expect("size")];
        reader.next_frame(&mut pixels).expect("pixels");
        std::fs::remove_file(output).expect("cleanup");
        pixels[0]
    };
    // Frame n's luma is 16 + n, limited range: full-range grey (n * 255 / 219).
    let grey = |frame: f64| (frame * 255.0 / 219.0).round() as u8;
    assert!(usage().contains("--at <source-seconds>"), "{}", usage());
    for (seconds, frame) in [("106.4", 125.0), ("101.4", 0.0), ("5", 0.0), ("103", 40.0)] {
        let red = red_at(seconds);
        assert!(
            red.abs_diff(grey(frame)) <= 2,
            "--at {seconds}: grey {red}, frame {frame} is {}",
            grey(frame)
        );
    }
}

/// The nine thumbnail times spread from the video's first frame over its
/// own length, not from tick 0 over the container's, which in a TS whose
/// video starts at 1.47 s looks mostly before its first frame. A
/// duration a file made up is refused rather than overflowing.
#[test]
fn thumbnail_times_spread_over_the_video_itself() {
    let ticks = alpha2_time::MediaTime::from_ticks;
    let times = thumbnail_times(ticks(176_000), ticks(240_000)).expect("times");
    assert_eq!(times.first().map(|time| time.ticks()), Some(200_000));
    assert_eq!(times.last().map(|time| time.ticks()), Some(392_000));
    assert!(video_length(Some(ticks(i64::MAX / 2)), None, ticks(0)).is_err());
    assert!(video_length(Some(ticks(-1)), None, ticks(0)).is_err());
    assert!(thumbnail_times(ticks(i64::MAX - 1), ticks(240_000)).is_err());
    assert_eq!(
        video_length(None, Some(ticks(600_000)), ticks(176_000)).map(alpha2_time::MediaTime::ticks),
        Ok(424_000)
    );
}

/// A PNG write that fails (a full disk) fails the command rather than
/// dropping the writer with its last errors, and leaves nothing half
/// written in place. The path that cannot be written sits under a regular
/// file, which fails for root too: root would replace `/dev/full` with
/// the PNG.
#[test]
fn a_png_that_cannot_be_written_fails_the_command() {
    let frame = first_frame(std::path::Path::new(&corpus("long-gop.mp4")));
    let not_a_directory = scratch("not-a-directory", "bin");
    std::fs::write(&not_a_directory, b"kept").expect("file");
    let blocked = format!("{not_a_directory}/thumb.png");
    assert!(write_png(&blocked, &frame).is_err());
    assert_eq!(std::fs::read(&not_a_directory).expect("read"), b"kept");
    std::fs::remove_file(not_a_directory).expect("cleanup");
    let output = scratch("thumb", "png");
    std::fs::write(&output, b"old").expect("old file");
    write_png(&output, &frame).expect("a regular file is replaced");
    let partial = output.replace(".png", ".a2partial.png");
    assert!(std::fs::read(&output).expect("png").starts_with(b"\x89PNG"));
    assert!(!std::path::Path::new(&partial).exists());
    std::fs::remove_file(output).expect("cleanup");
}

/// A PNG is not renamed over what is not a regular file: a FIFO stays a
/// FIFO, and nothing is left beside it.
#[cfg(unix)]
#[test]
fn a_png_does_not_replace_a_fifo() {
    use std::os::unix::fs::FileTypeExt;
    let frame = first_frame(std::path::Path::new(&corpus("long-gop.mp4")));
    let fifo = scratch("fifo", "png");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo");
    assert!(made.success());
    let error = write_png(&fifo, &frame).expect_err("a FIFO is refused");
    assert!(error.contains("not a regular file"), "{error}");
    let kind = std::fs::symlink_metadata(&fifo).expect("fifo").file_type();
    assert!(kind.is_fifo(), "{kind:?}");
    let partial = fifo.replace(".png", ".a2partial.png");
    assert!(!std::path::Path::new(&partial).exists());
    std::fs::remove_file(fifo).expect("cleanup");
}

/// A PNG is not renamed over a symbolic link: the link and the file it
/// names stay as they were.
#[cfg(unix)]
#[test]
fn a_png_does_not_replace_a_symbolic_link() {
    let frame = first_frame(std::path::Path::new(&corpus("long-gop.mp4")));
    let target = scratch("link-target", "png");
    std::fs::write(&target, b"old").expect("target");
    let link = scratch("link", "png");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let error = write_png(&link, &frame).expect_err("a symbolic link is refused");
    assert!(error.contains("not a regular file"), "{error}");
    let kind = std::fs::symlink_metadata(&link).expect("link").file_type();
    assert!(kind.is_symlink(), "{kind:?}");
    assert_eq!(std::fs::read(&target).expect("target"), b"old");
    let partial = link.replace(".png", ".a2partial.png");
    assert!(!std::path::Path::new(&partial).exists());
    std::fs::remove_file(link).expect("cleanup");
    std::fs::remove_file(target).expect("cleanup");
}

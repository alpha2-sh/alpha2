//! Export outputs: an existing file is refused unless the request overwrites
//! it, a replaced file stays whole until the export is, and an output that is
//! one of the request's inputs is refused either way.

use super::*;

/// An export request of a one-second clip of the corpus video to `output`.
fn export_request(output: &Path, overwrite: bool) -> Request {
    export_request_from(&corpus("h264-bt709-copy.mp4"), output, overwrite)
}

/// An export request of a one-second clip of the video at `input` to `output`.
fn export_request_from(input: &Path, output: &Path, overwrite: bool) -> Request {
    let second = alpha2_time::TICKS_PER_SECOND;
    parse_request(
        &json!({
            "protocolVersion": 1, "operation": "export", "requestId": "overwrite",
            "project": {
                "schema_version": 1, "id": "out", "name": "Out",
                "settings": {"width": 128, "height": 96, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
                "sources": [{"id": "video", "locator": "fixture://video", "kind": "video"}],
                "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
                    "id": "clip", "source_id": "video",
                    "timeline_range": {"start": 0, "end": second},
                    "source_range": {"start": 0, "end": second}
                }]}]}
            },
            "assets": {"video": input},
            "startTicks": 0, "endTicks": second,
            "frameRate": {"num": 30, "den": 1},
            "includeAudio": false,
            "videoCodec": "mpeg4-part2",
            "output": output,
            "overwrite": overwrite
        })
        .to_string(),
    )
    .expect("request")
}

#[test]
fn an_existing_output_is_replaced_only_when_the_request_overwrites() {
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("overwrite");
    let output = directory.join("out.mp4");
    std::fs::write(&output, b"old").expect("old file");
    let refused = host
        .handle(export_request(&output, false), &mut |_| Ok(()))
        .expect_err("an existing output is refused");
    assert_eq!(refused.code, "IO_ERROR", "{refused:?}");
    assert!(
        refused.message.contains("refusing to overwrite"),
        "{refused:?}"
    );
    assert_eq!(std::fs::read(&output).expect("old"), b"old");

    // Twice: the second replaces the first export's file.
    for _ in 0..2 {
        host.handle(export_request(&output, true), &mut |_| Ok(()))
            .expect("an overwriting export");
        assert_eq!(decoded_luma(&output).len(), 30);
    }
}

#[test]
fn the_result_names_the_file_written_by_its_absolute_path() {
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("result-output");
    std::fs::create_dir(directory.join("sub")).expect("sub");
    // A path with a `..` in it: the result names the file, not the spelling.
    let output = directory.join("sub").join("..").join("out.mp4");
    let result = host
        .handle(export_request(&output, false), &mut |_| Ok(()))
        .expect("export");
    let named = PathBuf::from(result["output"].as_str().expect("output"));
    assert!(
        names_written_file(&named, &directory.join("out.mp4")),
        "{named:?}"
    );
    assert_eq!(result["videoPipeline"], "readback", "{result}");
}

#[test]
fn the_result_says_how_the_output_was_published() {
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("publish-method");
    let output = directory.join("out.mp4");
    let result = host
        .handle(export_request(&output, false), &mut |_| Ok(()))
        .expect("export");
    assert_eq!(
        result["publish"],
        json!({"method": "rename-no-replace", "stagedLocally": false, "retries": 0}),
        "{result}"
    );
    let result = host
        .handle(export_request(&output, true), &mut |_| Ok(()))
        .expect("an overwriting export");
    assert_eq!(result["publish"]["method"], "rename-replace", "{result}");
}

#[test]
fn a_file_saved_at_the_output_during_an_export_keeps_the_render_beside_it() {
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("publish-kept");
    let output = directory.join("out.mp4");
    // Another program saves a file at the path while the frames encode.
    let error = host
        .handle(export_request(&output, false), &mut |_| {
            if !output.exists() {
                std::fs::write(&output, b"someone else's").expect("save");
            }
            Ok(())
        })
        .expect_err("the publish fails");
    assert_eq!(error.code, "OUTPUT_PUBLISH_FAILED", "{error:?}");
    assert_eq!(std::fs::read(&output).expect("saved"), b"someone else's");
    let kept = PathBuf::from(
        error.details.as_ref().expect("details")["keptPath"]
            .as_str()
            .expect("keptPath"),
    );
    assert!(kept.is_absolute(), "{}", kept.display());
    assert!(
        names_written_file(&kept, &directory.join("out.a2kept.mp4")),
        "{}",
        kept.display()
    );
    assert!(
        error.message.contains("appeared") && error.message.contains("kept at"),
        "{}",
        error.message
    );
    assert_eq!(decoded_luma(&kept).len(), 30, "the kept render is whole");
    let failure = error.failure.expect("a failure report");
    assert_eq!(failure["code"], "OUTPUT_IO_FAILED", "{failure}");
    let names: Vec<_> = std::fs::read_dir(&*directory)
        .expect("directory")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
}

#[test]
fn a_cancelled_overwrite_leaves_the_old_file_whole() {
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("overwrite-cancel");
    let output = directory.join("out.mp4");
    let old: Vec<u8> = (0..=255_u8).cycle().take(1 << 16).collect();
    std::fs::write(&output, &old).expect("old file");
    let before = sha256_hex(&output);
    // Progress is reported at most every few frames, so the first report stops it.
    let error = host
        .handle(export_request(&output, true), &mut |_| {
            Err(HostError::new("CANCELLED", "the client stopped the export"))
        })
        .expect_err("cancelled");
    assert!(error.message.contains("the client stopped"), "{error:?}");
    assert_eq!(sha256_hex(&output), before);
    let partials: Vec<_> = std::fs::read_dir(&*directory)
        .expect("directory")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(partials.len(), 1, "{partials:?}");
}

#[test]
fn an_output_that_is_an_input_is_refused_with_or_without_overwrite() {
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("output-is-input");
    let source = directory.join("source.mp4");
    std::fs::copy(corpus("h264-bt709-copy.mp4"), &source).expect("source");
    let linked = directory.join("linked.mp4");
    std::fs::hard_link(&source, &linked).expect("hard link");
    let bytes = sha256_hex(&source);
    for overwrite in [false, true] {
        let Request::Export(mut request) = export_request(&linked, overwrite) else {
            unreachable!("an export");
        };
        request.assets = HashMap::from([("video".to_owned(), source.clone())]);
        let error = host
            .handle(Request::Export(request), &mut |_| Ok(()))
            .expect_err("the output is the asset");
        assert_eq!(error.code, "INVALID_REQUEST", "{error:?}");
        assert!(error.message.contains(r#"the asset "video""#), "{error:?}");
        assert_eq!(sha256_hex(&source), bytes);
    }
    // A copy of the input is another file.
    let copy = directory.join("copy.mp4");
    std::fs::copy(&source, &copy).expect("copy");
    let Request::Export(mut request) = export_request(&copy, true) else {
        unreachable!("an export");
    };
    request.assets = HashMap::from([("video".to_owned(), source.clone())]);
    host.handle(Request::Export(request), &mut |_| Ok(()))
        .expect("a copy of an input may be replaced");
}

#[test]
fn two_overwriting_exports_to_one_path_do_not_both_write_it() {
    let _gpu = gpu_test_lock();
    let mut first = create_host(VideoDecodePolicy::Software).expect("host");
    let mut second = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("overwrite-in-use");
    let output = directory.join("out.mp4");
    std::fs::write(&output, b"old").expect("old file");
    let mut refused = None;
    first
        .handle(export_request(&output, true), &mut |_| {
            if refused.is_none() {
                refused = Some(
                    second
                        .handle(export_request(&output, true), &mut |_| Ok(()))
                        .expect_err("the output's partial is held"),
                );
            }
            Ok(())
        })
        .expect("the first export");
    let refused = refused.expect("the second export ran");
    assert_eq!(refused.code, "OUTPUT_IN_USE", "{refused:?}");
    assert_eq!(decoded_luma(&output).len(), 30);
}

/// Hangul, a word of it: the test data of the non-ASCII path tests.
const HANGUL_SCENE: &str = "\u{c7a5}\u{ba74}";

/// A video read from a folder and file named in Hangul with an emoji, which
/// is outside the Basic Multilingual Plane, exports to another such folder.
#[test]
fn an_export_reads_and_writes_paths_in_hangul_with_an_emoji() {
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("non-ascii-paths");
    let sources = directory.join("\u{c601}\u{c0c1}\u{1f600}");
    let results = directory.join("\u{acb0}\u{acfc}\u{1f600}");
    std::fs::create_dir(&sources).expect("source folder");
    std::fs::create_dir(&results).expect("output folder");
    let input = sources.join(format!("{HANGUL_SCENE}.mov"));
    std::fs::copy(corpus("h264-long-gop.mp4"), &input).expect("input");
    let output = results.join(format!("{HANGUL_SCENE}.mp4"));
    let result = host
        .handle(export_request_from(&input, &output, false), &mut |_| Ok(()))
        .expect("export");
    let named = PathBuf::from(result["output"].as_str().expect("output"));
    assert!(names_written_file(&named, &output), "{named:?}");
    assert_eq!(decoded_luma(&output).len(), 30);
}

/// A name of 81 Hangul syllables (243 bytes in UTF-8, 247 with `.mp4`, under
/// the 255 bytes a file name may take) is written whole: the partial file the
/// export writes first must not make it too long.
#[test]
fn an_output_named_by_81_hangul_syllables_is_written() {
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("long-name");
    let output = directory.join(format!("{}.mp4", "\u{d55c}".repeat(81)));
    let result = host
        .handle(export_request(&output, false), &mut |_| Ok(()))
        .expect("export");
    let named = PathBuf::from(result["output"].as_str().expect("output"));
    assert!(names_written_file(&named, &output), "{named:?}");
    assert_eq!(decoded_luma(&output).len(), 30);
}

/// A file whose name is stored decomposed (NFD, as older macOS tools wrote
/// it) is the file a request names composed: APFS looks names up either way.
#[cfg(target_os = "macos")]
#[test]
fn an_input_stored_decomposed_is_read_by_its_composed_name() {
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("nfd-input");
    // The syllable U+D55C as its three jamo, then composed.
    let decomposed = directory.join("\u{1112}\u{1161}\u{11ab}.mov");
    std::fs::copy(corpus("h264-long-gop.mp4"), &decomposed).expect("input");
    let composed = directory.join("\u{d55c}.mov");
    let output = directory.join("out.mp4");
    host.handle(export_request_from(&composed, &output, false), &mut |_| {
        Ok(())
    })
    .expect("export");
    assert_eq!(decoded_luma(&output).len(), 30);
}

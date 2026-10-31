//! Spellings of paths and text files: two names of one still output, the path
//! a missing source was looked up at, and the one rule user text files are
//! decoded by.

use super::*;
use crate::text_file::decode_user_text;

/// A still request of the conform project for `outputs`, one frame each.
fn still_request(outputs: &[&Path], overwrite: bool) -> Result<Request, HostError> {
    parse_request(
        &json!({
            "protocolVersion": 1, "operation": "still", "requestId": "spellings",
            "project": conform_project(&json!({})),
            "assets": {"video": corpus("parity-rgb.mov")},
            "stills": outputs
                .iter()
                .enumerate()
                .map(|(index, output)| json!({"time": index * 10_000, "output": output}))
                .collect::<Vec<_>>(),
            "overwrite": overwrite,
        })
        .to_string(),
    )
}

#[test]
fn two_spellings_of_one_still_output_are_refused_before_rendering() {
    let directory = Path::new("/stills");
    // Case, then e-acute composed and as e and a combining acute accent.
    for (first, second) in [("A.png", "a.png"), ("\u{e9}.png", "e\u{301}.png")] {
        let error = still_request(&[&directory.join(first), &directory.join(second)], false)
            .err()
            .unwrap_or_else(|| panic!("{first} and {second} are one output"));
        assert_eq!(error.code, "INVALID_INPUT", "{}", error.message);
        assert!(
            error.message.contains("two stills write one file"),
            "{}",
            error.message
        );
    }
    still_request(&[&directory.join("a.png"), &directory.join("b.png")], false).expect("two files");
}

/// A spelling only the file system resolves (a folder reached through a
/// link) names a file an earlier still wrote: refused before it is written
/// over, even when the request overwrites.
#[cfg(unix)]
#[test]
fn a_still_target_that_is_an_earlier_output_is_refused_before_it_is_written() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("still-link");
    let real = directory.join("real");
    std::fs::create_dir(&real).expect("folder");
    let linked = directory.join("linked");
    std::os::unix::fs::symlink(&real, &linked).expect("link");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let first = real.join("frame.png");
    let request = still_request(&[&first, &linked.join("frame.png")], true).expect("request");
    let error = host
        .handle(request, &mut |_| Ok(()))
        .expect_err("the second still is the first's file");
    assert_eq!(error.code, "INVALID_INPUT", "{}", error.message);
    assert!(
        error.message.contains("two stills write one file"),
        "{}",
        error.message
    );
    // The first still was written, and nothing wrote over it.
    let written = std::fs::read(&first).expect("the first still");
    assert!(written.starts_with(b"\x89PNG"), "a PNG");
}

#[test]
fn a_missing_source_names_its_path() {
    let directory = scratch_directory("missing-source");
    let missing = directory.join("take-07.mov");
    let resolved = resolve(
        &conform_project(&json!({})),
        &[("video", missing.clone())],
        &[],
        alpha2_assets::MediaPurpose::ExportOriginal,
    );
    let wire = serde_json::to_value(&resolved.report).expect("report");
    let issue = &wire["sources"][0]["issues"][0];
    assert_eq!(issue["kind"], "offline", "{wire}");
    assert_eq!(issue["path"], missing.display().to_string(), "{wire}");
    let summary = crate::assets::blocking_summary(&resolved.report);
    assert_eq!(
        summary,
        format!(
            "the project's media is not ready: video: offline ({} not found)",
            missing.display()
        )
    );
}

/// Linux file systems keep names byte for byte, so a name stored decomposed
/// is not found by its composed spelling; the summary names the file there.
#[cfg(target_os = "linux")]
#[test]
fn a_source_spelled_in_another_normalization_is_named() {
    let directory = scratch_directory("normalization");
    // The Hangul syllable U+D55C as its three jamo on disk, asked for composed.
    let stored = "\u{1112}\u{1161}\u{11ab}.mov";
    std::fs::copy(corpus("parity-rgb.mov"), directory.join(stored)).expect("copy");
    let resolved = resolve(
        &conform_project(&json!({})),
        &[("video", directory.join("\u{d55c}.mov"))],
        &[],
        alpha2_assets::MediaPurpose::ExportOriginal,
    );
    let summary = crate::assets::blocking_summary(&resolved.report);
    assert!(
        summary.ends_with(&format!(
            "not found) (a file named {stored} exists: the names differ in Unicode normalization)"
        )),
        "{summary}"
    );
}

#[test]
fn a_lut_with_a_legacy_title_is_invalid_input_naming_it() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("legacy-lut");
    let lut = directory.join("grade.cube");
    // A TITLE line in cp949 (the Hangul syllable U+D55C), which is no UTF-8.
    let mut cube = b"TITLE \"\xc7\xd1\"\nLUT_3D_SIZE 2\n".to_vec();
    for _ in 0..8 {
        cube.extend_from_slice(b"0 0 0\n");
    }
    std::fs::write(&lut, &cube).expect("LUT");
    let mut project = conform_project(&json!({}));
    project["effect_resources"] =
        json!([{"id": "grade", "kind": "lut3d", "locator": "asset://grade"}]);
    let request = parse_request(
        &json!({
            "protocolVersion": 1, "requestId": "lut", "operation": "export",
            "project": project,
            "assets": {"video": corpus("parity-rgb.mov")},
            "effectAssets": {"grade": lut},
            "startTicks": 0, "endTicks": 30_000,
            "frameRate": {"num": 12, "den": 1},
            "videoCodec": "mpeg4-part2",
            "output": directory.join("out.mp4")
        })
        .to_string(),
    )
    .expect("request");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let error = host
        .handle(request, &mut |_| Ok(()))
        .expect_err("the LUT is refused");
    assert_eq!(error.code, "INVALID_INPUT", "{}", error.message);
    assert!(
        error.message.contains(&format!(
            "effect resource grade {} is not UTF-8 text (invalid byte at offset 7)",
            lut.display()
        )),
        "{}",
        error.message
    );
}

#[test]
fn user_text_is_utf8_with_or_without_a_mark_or_utf16_behind_one() {
    let text = "{\"name\": \"\u{d55c}\"}";
    let utf16 = |little: bool| {
        text.encode_utf16()
            .flat_map(|unit| {
                if little {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                }
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(decode_user_text(text.as_bytes()).as_deref(), Ok(text));
    let marked = [b"\xef\xbb\xbf".as_slice(), text.as_bytes()].concat();
    assert_eq!(decode_user_text(&marked).as_deref(), Ok(text));
    let little = [b"\xff\xfe".as_slice(), &utf16(true)].concat();
    assert_eq!(decode_user_text(&little).as_deref(), Ok(text));
    let big = [b"\xfe\xff".as_slice(), &utf16(false)].concat();
    assert_eq!(decode_user_text(&big).as_deref(), Ok(text));
    // cp949, an odd UTF-16 length and an unpaired surrogate, each at its offset.
    assert_eq!(decode_user_text(b"{\"a\": \"\xc7\xd1\"}"), Err(7));
    assert_eq!(decode_user_text(b"\xff\xfe{\0\""), Err(4));
    assert_eq!(decode_user_text(b"\xff\xfe{\0\x00\xd8"), Err(4));
}

//! The command line reads the files it is pointed at as files, whatever their
//! names look like, and reads project files by the one text rule.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A one-second project showing the source `video`.
const PROJECT: &str = r#"{"schema_version": 1, "id": "out", "name": "Out",
  "settings": {"width": 128, "height": 96, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
  "sources": [{"id": "video", "locator": "fixture://video", "kind": "video"}],
  "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{"id": "clip",
    "source_id": "video", "timeline_range": {"start": 0, "end": 120000},
    "source_range": {"start": 0, "end": 120000}}]}]}}"#;

fn corpus(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/corpus")
        .join(name)
}

/// An empty directory of its own for one test.
fn scratch(name: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("alpha2-host-paths-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("scratch directory");
    directory
}

/// Runs the host in `directory` with `args`.
fn host(directory: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_alpha2-host"))
        .args(args)
        .current_dir(directory)
        .output()
        .expect("the host runs")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// `take:1.mov` in the working directory is that file, not the protocol
/// `take`.
#[cfg(unix)]
#[test]
fn a_relative_source_whose_name_looks_like_a_url_is_read_as_a_file() {
    let directory = scratch("source-colon");
    std::fs::copy(corpus("h264-bt709-copy.mp4"), directory.join("take:1.mov")).expect("copy");
    std::fs::write(directory.join("project.json"), PROJECT).expect("project");
    let output = host(
        &directory,
        &[
            "conform",
            "--project",
            "project.json",
            "--asset",
            "video=take:1.mov",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let answer: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert_eq!(answer["result"]["report"]["ready"], true, "{answer}");
    std::fs::remove_dir_all(&directory).ok();
}

/// `out:1.mp4` in the working directory is written as that file.
#[cfg(unix)]
#[test]
fn an_output_whose_name_looks_like_a_url_is_written_as_a_file() {
    let directory = scratch("output-colon");
    std::fs::copy(corpus("h264-bt709-copy.mp4"), directory.join("clip.mov")).expect("copy");
    std::fs::write(directory.join("project.json"), PROJECT).expect("project");
    let output = host(
        &directory,
        &[
            "export",
            "--project",
            "project.json",
            "--asset",
            "video=clip.mov",
            "--out",
            "out:1.mp4",
            "--renderer",
            "cpu",
            "--video-codec",
            "mpeg4-part2",
            "--no-audio",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let written = std::fs::read(directory.join("out:1.mp4")).expect("the output");
    assert_eq!(&written[4..8], b"ftyp");
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn a_project_file_with_a_bom_or_in_utf16_is_read() {
    let directory = scratch("project-encodings");
    std::fs::copy(corpus("h264-bt709-copy.mp4"), directory.join("clip.mov")).expect("copy");
    let utf16 = |little: bool| {
        PROJECT
            .encode_utf16()
            .flat_map(|unit| {
                if little {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                }
            })
            .collect::<Vec<_>>()
    };
    for (name, bytes) in [
        (
            "bom.json",
            [b"\xef\xbb\xbf".as_slice(), PROJECT.as_bytes()].concat(),
        ),
        (
            "utf16le.json",
            [b"\xff\xfe".as_slice(), &utf16(true)].concat(),
        ),
        (
            "utf16be.json",
            [b"\xfe\xff".as_slice(), &utf16(false)].concat(),
        ),
    ] {
        std::fs::write(directory.join(name), bytes).expect("project");
        let output = host(
            &directory,
            &["conform", "--project", name, "--asset", "video=clip.mov"],
        );
        assert!(output.status.success(), "{name}: {}", stderr(&output));
    }
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn a_project_file_in_a_legacy_code_page_is_invalid_input_naming_it() {
    let directory = scratch("project-cp949");
    // The project's name in cp949 (the Hangul syllable U+D55C), which is no UTF-8.
    let legacy = PROJECT.replace("\"Out\"", "\"\u{0}\"");
    let at = legacy.find('\u{0}').expect("placeholder");
    let mut bytes = legacy.into_bytes();
    bytes.splice(at..=at, *b"\xc7\xd1");
    std::fs::write(directory.join("legacy.json"), bytes).expect("project");
    for command in ["conform", "export"] {
        let output = host(
            &directory,
            &[
                command,
                "--project",
                "legacy.json",
                "--asset",
                "video=clip.mov",
                "--out",
                "out.mp4",
            ],
        );
        assert!(!output.status.success(), "{command}");
        let text = stderr(&output);
        assert!(
            text.contains(&format!(
                "project file legacy.json is not UTF-8 text (invalid byte at offset {at}); save \
                 it as UTF-8"
            )),
            "{command}: {text}"
        );
    }
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn a_missing_project_file_is_named() {
    let directory = scratch("project-missing");
    for command in ["conform", "export"] {
        let output = host(
            &directory,
            &[
                command,
                "--project",
                "nowhere.json",
                "--asset",
                "video=clip.mov",
                "--out",
                "out.mp4",
            ],
        );
        assert!(!output.status.success(), "{command}");
        let text = stderr(&output);
        assert!(
            text.contains("cannot read the project file nowhere.json: "),
            "{command}: {text}"
        );
    }
    std::fs::remove_dir_all(&directory).ok();
}

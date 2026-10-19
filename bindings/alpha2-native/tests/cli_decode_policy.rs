//! The `alpha2` CLI's damaged-input policy: `--decode-policy strict|tolerant`,
//! else `ALPHA2_DECODE_POLICY`, else Strict, the host's vocabulary, on every
//! command that decodes; and the summary of what Tolerant skipped.

use std::path::PathBuf;
use std::process::{Command, Output};

fn corpus(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/corpus")
        .join(name)
        .to_str()
        .expect("UTF-8 path")
        .to_owned()
}

fn scratch(label: &str, extension: &str) -> String {
    let path = std::env::temp_dir().join(format!(
        "alpha2-cli-policy-{}-{label}.{extension}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    path.to_str().expect("UTF-8 path").to_owned()
}

/// Runs `alpha2 <args>` with `ALPHA2_DECODE_POLICY` set to `environment`, or unset.
fn alpha2(args: &[&str], environment: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_alpha2"));
    command.args(args);
    match environment {
        Some(value) => command.env("ALPHA2_DECODE_POLICY", value),
        None => command.env_remove("ALPHA2_DECODE_POLICY"),
    };
    command.output().expect("alpha2 runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn decoded_frames(output: &Output) -> usize {
    stdout(output)
        .lines()
        .find_map(|line| line.strip_prefix("decoded_frames: "))
        .and_then(|count| count.parse().ok())
        .unwrap_or_else(|| panic!("no frame count in {}", stdout(output)))
}

/// One packet of each 20-frame all-intra source is overwritten. Strict fails
/// on it, as by default; Tolerant drops that frame, decodes the other 19,
/// and says so on stderr.
#[test]
fn tolerant_decodes_past_a_damaged_packet_and_strict_stops_at_it() {
    for codec in ["h264", "vp9", "mpeg4"] {
        let healthy = alpha2(&["decode", &corpus(&format!("recovery-{codec}.mp4"))], None);
        assert!(healthy.status.success(), "{codec}: {}", stderr(&healthy));
        assert_eq!(decoded_frames(&healthy), 20, "{codec}");
        assert!(!stderr(&healthy).contains("decode diagnostics"), "{codec}");

        let damaged = corpus(&format!("recovery-{codec}-damaged.mp4"));
        for strict in [
            alpha2(&["decode", &damaged], None),
            alpha2(&["decode", &damaged, "--decode-policy", "strict"], None),
            // The flag wins over the environment.
            alpha2(
                &["decode", "--decode-policy", "strict", &damaged],
                Some("tolerant"),
            ),
        ] {
            assert!(!strict.status.success(), "{codec}");
            assert!(
                stderr(&strict).contains("(strict decode policy)"),
                "{codec}: {}",
                stderr(&strict)
            );
        }
        for tolerant in [
            alpha2(&["decode", &damaged, "--decode-policy", "tolerant"], None),
            alpha2(&["decode", &damaged], Some("tolerant")),
        ] {
            assert!(tolerant.status.success(), "{codec}: {}", stderr(&tolerant));
            assert_eq!(decoded_frames(&tolerant), 19, "{codec}");
            assert!(
                stderr(&tolerant).contains(
                    "alpha2: decode diagnostics (tolerant decode policy): corruptPackets=1 videoGaps=1 concealedAudioSamples=0"
                ),
                "{codec}: {}",
                stderr(&tolerant)
            );
        }
    }
}

/// An ADTS stream cut off inside its last packet decodes, under Tolerant,
/// to what `ffmpeg` decodes from it: its 19 whole frames.
#[test]
fn tolerant_decodes_a_cut_adts_stream_up_to_its_last_whole_frame() {
    let cut = corpus("recovery-truncated.aac");
    let strict = alpha2(&["decode-audio", &cut], None);
    assert!(!strict.status.success());
    assert!(
        stderr(&strict).contains("(strict decode policy)"),
        "{}",
        stderr(&strict)
    );
    let tolerant = alpha2(&["decode-audio", &cut, "--decode-policy", "tolerant"], None);
    assert!(tolerant.status.success(), "{}", stderr(&tolerant));
    assert!(
        stdout(&tolerant).contains("decoded_audio_frames: 19456\n"),
        "{}",
        stdout(&tolerant)
    );
    assert!(
        stderr(&tolerant).contains("corruptPackets=1"),
        "{}",
        stderr(&tolerant)
    );
}

/// Every command that decodes takes the policy; each fails on damage under
/// Strict and finishes under Tolerant, saying what it concealed.
#[test]
fn every_decoding_command_takes_the_policy() {
    let video = corpus("recovery-h264-damaged.mp4");
    let audio = corpus("recovery-aac-damaged.m4a");
    let png = scratch("render", "png");
    let thumbnail = scratch("thumbnail", "png");
    let transcoded = scratch("transcode", "mp4");
    let wav = scratch("export", "wav");
    let commands: [(Vec<&str>, &str); 5] = [
        (vec!["thumbnail", &video, &thumbnail], "corruptPackets="),
        (
            vec![
                "transcode",
                &video,
                &transcoded,
                "--video-codec",
                "mpeg4-part2",
            ],
            "videoGaps=1",
        ),
        (vec!["decode-audio", &audio], "concealedAudioSamples=1024"),
        (
            vec!["export-audio", &audio, &wav],
            "concealedAudioSamples=1024",
        ),
        (vec!["waveform", &audio], "concealedAudioSamples=1024"),
    ];
    for (args, reported) in commands {
        let tolerant = alpha2(
            &[&args[..], &["--decode-policy", "tolerant"]].concat(),
            None,
        );
        assert!(tolerant.status.success(), "{args:?}: {}", stderr(&tolerant));
        assert!(
            stderr(&tolerant).contains("(tolerant decode policy)")
                && stderr(&tolerant).contains(reported),
            "{args:?}: {}",
            stderr(&tolerant)
        );
    }
    // A render decodes only the first frame, before the damage, under either policy.
    for policy in ["strict", "tolerant"] {
        let render = alpha2(&["render", &video, &png, "--decode-policy", policy], None);
        assert!(render.status.success(), "{policy}: {}", stderr(&render));
        assert!(
            !stderr(&render).contains("decode diagnostics"),
            "{}",
            stderr(&render)
        );
    }
    for args in [
        vec!["thumbnail", &video, &thumbnail],
        vec![
            "transcode",
            &video,
            &transcoded,
            "--video-codec",
            "mpeg4-part2",
        ],
        vec!["decode-audio", &audio],
        vec!["export-audio", &audio, &wav],
        vec!["waveform", &audio],
    ] {
        // What the Tolerant run wrote, which a command does not overwrite.
        for path in [&thumbnail, &transcoded, &wav] {
            let _ = std::fs::remove_file(path);
        }
        let strict = alpha2(&args, None);
        assert!(!strict.status.success(), "{args:?}");
        assert!(
            stderr(&strict).contains("(strict decode policy)"),
            "{args:?}: {}",
            stderr(&strict)
        );
    }
    for path in [png, thumbnail, transcoded, wav] {
        let _ = std::fs::remove_file(path);
    }
}

/// A policy that is not `strict` or `tolerant`, from the flag or the
/// environment, is refused rather than read as one; so is the flag on a
/// command that decodes nothing.
#[test]
fn an_unknown_policy_or_a_command_that_does_not_decode_is_refused() {
    let source = corpus("recovery-h264.mp4");
    for (output, message) in [
        (
            alpha2(&["decode", &source, "--decode-policy", "lenient"], None),
            "--decode-policy takes strict or tolerant",
        ),
        (
            alpha2(&["decode", &source, "--decode-policy"], None),
            "--decode-policy takes strict or tolerant",
        ),
        (
            alpha2(
                &[
                    "decode",
                    "--decode-policy",
                    "strict",
                    &source,
                    "--decode-policy",
                    "tolerant",
                ],
                None,
            ),
            "--decode-policy is given more than once",
        ),
        (
            alpha2(&["decode", &source], Some("lenient")),
            "ALPHA2_DECODE_POLICY must be strict or tolerant",
        ),
        (
            alpha2(&["probe", &source, "--decode-policy", "tolerant"], None),
            "probe does not decode, so it takes no --decode-policy",
        ),
    ] {
        assert!(!output.status.success(), "{message}");
        assert!(stderr(&output).contains(message), "{}", stderr(&output));
    }
    // A command that decodes nothing ignores the environment.
    assert!(
        alpha2(&["probe", &source], Some("lenient"))
            .status
            .success()
    );
    let usage = stderr(&alpha2(&[], None));
    assert!(usage.contains("--decode-policy strict|tolerant"), "{usage}");
}

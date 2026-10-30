//! H.264 without a hardware encoder: what `capabilities` and a refused
//! export say, and exports through `OpenH264` where Cisco's binary is
//! installed or named (`ALPHA2_OPENH264_LIBRARY`, as CI's `OpenH264` job sets).

use super::*;

#[test]
fn the_h264_status_says_which_path_encodes_it_and_what_to_do_without_one() {
    let gpu_h264 = json!({"available": true, "reason": null,
        "codecs": [{"videoCodec": "h264", "encoder": "h264_vulkan"}]});
    let no_gpu = json!({"available": false, "reason": "no video-encode queue", "codecs": []});
    let readback =
        json!([{"id": "h264", "encoder": "openh264"}, {"id": "vp9", "encoder": "libvpx-vp9"}]);
    let vp9_only = json!([{"id": "vp9", "encoder": "libvpx-vp9"}]);

    let available = crate::protocol::h264_status_json(&readback, &no_gpu);
    assert_eq!(available["status"], json!("available"));
    assert_eq!(available["encoders"], json!(["openh264"]));
    assert_eq!(available["reason"], Value::Null);
    assert_eq!(available["remedy"], Value::Null);

    let gpu_only = crate::protocol::h264_status_json(&vp9_only, &gpu_h264);
    assert_eq!(gpu_only["status"], json!("gpu-only"));
    assert_eq!(gpu_only["gpuEncoder"], json!("h264_vulkan"));
    assert!(
        gpu_only["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("cpu-readback")),
        "{gpu_only}"
    );

    let unavailable = crate::protocol::h264_status_json(&vp9_only, &no_gpu);
    assert_eq!(unavailable["status"], json!("unavailable"));
    assert!(
        unavailable["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("no video-encode queue")),
        "{unavailable}"
    );
    let remedy = unavailable["remedy"].as_str().expect("a remedy");
    assert!(
        remedy.contains("libx264") && remedy.contains("VP9"),
        "{remedy}"
    );
    if alpha2_native::openh264_status().as_str() != "disabled" {
        assert!(remedy.contains("install --openh264"), "{remedy}");
    }
}

#[test]
fn a_refused_h264_export_names_the_gpu_path_reason_and_the_remedies() {
    let message = alpha2_native::no_video_encoder_message(
        NativeVideoCodec::H264,
        &[],
        Some("inputPolicy is cpu-readback"),
    );
    assert!(message.starts_with("no usable h264 encoder: "), "{message}");
    assert!(
        message.contains("GPU output path unavailable (inputPolicy is cpu-readback)"),
        "{message}"
    );
    assert!(message.contains("offers [none]"), "{message}");
    assert!(message.contains("libx264"), "{message}");
    assert!(message.contains("export VP9 (.webm) or AV1"), "{message}");
    // Another codec has no OpenH264 to suggest.
    let hevc = alpha2_native::no_video_encoder_message(NativeVideoCodec::Hevc, &[], None);
    assert!(
        !hevc.contains("OpenH264") && !hevc.contains("GPU output"),
        "{hevc}"
    );
}

#[test]
fn capabilities_report_openh264_with_cisco_s_notice() {
    let encode = crate::protocol::codec_capabilities_json();
    let openh264 = &encode["openh264"];
    assert_eq!(openh264["encoder"], json!("openh264"));
    assert_eq!(
        openh264["notice"],
        json!("OpenH264 Video Codec provided by Cisco Systems, Inc.")
    );
    let listed = encode["video"]
        .as_array()
        .expect("video encoders")
        .iter()
        .find(|entry| entry["encoder"] == "openh264");
    match openh264["status"].as_str() {
        Some("available") => {
            let entry = listed.expect("an available OpenH264 is listed");
            assert_eq!(entry["id"], json!("h264"));
            assert_eq!(entry["encoderClass"], json!("software"));
            assert_eq!(entry["notice"], openh264["notice"]);
            assert!(entry["provider"] == "cisco-openh264" || entry["provider"] == "system");
            assert_eq!(openh264["remedy"], Value::Null);
        }
        Some(_) => {
            assert!(listed.is_none(), "{encode}");
            assert!(openh264["reason"].is_string(), "{openh264}");
        }
        None => panic!("{openh264}"),
    }
}

/// The `OpenH264` export of a 1280x720 render on the readback path, its
/// result, and the luma of every frame.
fn openh264_export(directory: &Path, name: &str) -> (Value, String, usize) {
    let end = alpha2_time::TICKS_PER_SECOND;
    let project = json!({
        "schema_version": 1, "id": "openh264", "name": "OpenH264",
        "settings": {"width": 1280, "height": 720, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
        "sources": [{"id": "video", "locator": "fixture://video", "kind": "video"}],
        "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
            "id": "clip", "source_id": "video",
            "timeline_range": {"start": 0, "end": end},
            "source_range": {"start": 0, "end": end}
        }]}]}
    });
    let assets = HashMap::from([("video".to_owned(), corpus("h264-bt709-copy.mp4"))]);
    let output = directory.join(name);
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    host.input_policy = crate::config::VideoInputPolicy::CpuReadback;
    let result = host
        .export(
            &project.to_string(),
            &assets,
            &HashMap::new(),
            Vec::new(),
            0,
            end,
            RateRequest { num: 30, den: 1 },
            false,
            crate::protocol::ResolvedFormat {
                container: alpha2_media::Container::Mp4,
                video: NativeVideoCodec::H264,
                audio: NativeAudioCodec::Aac,
                pixels: alpha2_media::PixelLayout::YUV420P,
            },
            Some(alpha2_native::OPENH264_ENCODER_NAME),
            VideoQualityRequest::Level(QualityLevel::High),
            AudioQualityRequest::Level(QualityLevel::High),
            None,
            &output,
            &mut |_| Ok(()),
        )
        .expect("an OpenH264 export");
    let frames = decoded_luma(&output).len();
    (result, sha256_hex(&output), frames)
}

#[test]
fn an_openh264_export_reports_its_encoder_and_repeats_byte_for_byte() {
    if alpha2_native::openh264_status().library().is_none() {
        eprintln!("OpenH264 is not installed: the OpenH264 export did not run");
        return;
    }
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("openh264-export");
    let (result, first, frames) = openh264_export(&directory, "first.mp4");
    assert_eq!(frames, 30);
    assert_eq!(result["videoEncoder"], json!("openh264"), "{result}");
    assert_eq!(result["videoEncoderClass"], json!("software"));
    assert_eq!(result["videoEncoderHardware"], json!(false));
    assert_eq!(result["videoPipeline"], json!("readback"));
    assert_eq!(result["videoThreadCount"], json!(2), "720 rows: two slices");
    assert_eq!(
        result["videoRateControl"]["applied"],
        json!("quantizer"),
        "{result}"
    );
    let library = &result["videoEncoderLibrary"];
    assert_eq!(
        library["notice"],
        json!("OpenH264 Video Codec provided by Cisco Systems, Inc.")
    );
    assert!(library["version"].is_string() && library["library"].is_string());
    let (_, second, _) = openh264_export(&directory, "second.mp4");
    assert_eq!(first, second, "two runs of one export differ");
}

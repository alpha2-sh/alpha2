//! Exports on the CPU renderer: the same frames the GPU host draws, what the
//! result reports, and the refusal of a runtime WGSL effect.

use super::*;

/// The complex parity project (effects, a transition, compositions with a
/// group, masks, a matte, blend modes, shapes and a fill) over a corpus
/// clip and a still, with `effects` as its runtime effects.
fn complex_export(
    renderer: crate::config::RendererPolicy,
    output: &Path,
    effects: Vec<EffectRequest>,
    project: Option<Value>,
) -> Result<Value, String> {
    let project = project.unwrap_or_else(|| {
        serde_json::from_str(include_str!(
            "../../../../tests/cases/host-parity-complex-project.json"
        ))
        .expect("complex project")
    });
    let assets = HashMap::from([
        ("video".to_owned(), corpus("h264-bt709-copy.mp4")),
        ("still".to_owned(), corpus("cross_topology_parity.png")),
    ]);
    let end = project["timeline"]["tracks"][0]["clips"][1]["timeline_range"]["end"]
        .as_i64()
        .expect("the project's end");
    let mut host = crate::create_host_with(VideoDecodePolicy::Software, renderer).expect("host");
    host.export(
        &project.to_string(),
        &assets,
        &HashMap::new(),
        effects,
        0,
        end,
        RateRequest { num: 24, den: 1 },
        false,
        crate::protocol::ResolvedFormat {
            container: alpha2_media::Container::Mp4,
            video: NativeVideoCodec::Mpeg4Part2,
            audio: NativeAudioCodec::Aac,
        },
        None,
        VideoQualityRequest::Level(QualityLevel::VeryHigh),
        AudioQualityRequest::Level(QualityLevel::High),
        None,
        output,
        &mut |_| Ok(()),
    )
}

#[test]
fn a_cpu_export_draws_the_frames_the_gpu_draws() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("cpu-renderer");
    let cpu = complex_export(
        crate::config::RendererPolicy::Cpu,
        &directory.join("cpu.mp4"),
        Vec::new(),
        None,
    )
    .expect("a CPU export");
    assert_eq!(cpu["renderer"], json!("cpu"), "{cpu}");
    assert!(
        cpu["rendererThreads"]
            .as_u64()
            .is_some_and(|threads| threads >= 1)
    );
    assert_eq!(cpu["videoOutputPath"], json!("readback"));
    assert!(
        cpu["performance"]["cpuRenderMs"]
            .as_f64()
            .is_some_and(|ms| ms > 0.0)
    );
    let gpu = complex_export(
        crate::config::RendererPolicy::Gpu,
        &directory.join("gpu.mp4"),
        Vec::new(),
        None,
    )
    .expect("a GPU export");
    assert_eq!(gpu["renderer"], json!("gpu"));
    let cpu_frames = decoded_luma(&directory.join("cpu.mp4"));
    let gpu_frames = decoded_luma(&directory.join("gpu.mp4"));
    assert_eq!(cpu_frames.len(), gpu_frames.len());
    assert_eq!(cpu["frames"], gpu["frames"]);
    // Both go through the same lossy encoder, so frames a code or two apart encode alike; a
    // frame drawn differently would be far below this.
    for (index, (cpu, gpu)) in cpu_frames.iter().zip(&gpu_frames).enumerate() {
        let squared = cpu
            .iter()
            .zip(gpu)
            .map(|(a, b)| f64::from(a.abs_diff(*b)).powi(2))
            .sum::<f64>()
            / f64::from(u32::try_from(cpu.len()).expect("a frame's samples"));
        let psnr = 10.0 * (255.0_f64 * 255.0 / squared.max(1.0e-9)).log10();
        assert!(psnr > 40.0, "frame {index}: {psnr:.1} dB");
    }
}

#[test]
fn a_cpu_export_repeats_byte_for_byte() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("cpu-renderer-repeat");
    let export = |name: &str| {
        complex_export(
            crate::config::RendererPolicy::Cpu,
            &directory.join(name),
            Vec::new(),
            None,
        )
        .expect("a CPU export");
        sha256_hex(&directory.join(name))
    };
    assert_eq!(export("first.mp4"), export("second.mp4"));
}

#[test]
fn a_runtime_effect_is_refused_on_the_cpu_and_names_the_way_out() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("cpu-renderer-runtime-effect");
    let mut project: Value = serde_json::from_str(include_str!(
        "../../../../tests/cases/host-parity-complex-project.json"
    ))
    .expect("complex project");
    project["compositions"][0]["nodes"][1]["op"]["effect"] =
        json!({"id": "test.effect.invert", "version": 1, "params": {}});
    let effect: EffectRequest = serde_json::from_value(json!({
        "id": "test.effect.invert",
        "version": 1,
        "kind": "effect",
        "wgslBody": "fn effect(uv: vec2f, src: vec4f) -> vec4f { return vec4f(src.a - src.rgb, src.a); }",
    }))
    .expect("an effect request");
    let error = complex_export(
        crate::config::RendererPolicy::Cpu,
        &directory.join("refused.mp4"),
        vec![effect],
        Some(project),
    )
    .expect_err("a runtime effect on the CPU");
    assert!(error.contains("EFFECT_ERROR"), "{error}");
    assert!(error.contains("test.effect.invert@1"), "{error}");
    assert!(error.contains("--renderer gpu"), "{error}");
    assert!(
        !directory.join("refused.mp4").exists(),
        "no output before the refusal"
    );
}

#[test]
fn capabilities_say_which_renderer_draws() {
    let _gpu = gpu_test_lock();
    let host = crate::create_host_with(
        VideoDecodePolicy::Software,
        crate::config::RendererPolicy::Cpu,
    )
    .expect("host");
    let report = serde_json::to_value(host.capability_report()).expect("report");
    assert_eq!(report["gpu"]["backend"], json!("cpu"), "{report}");
    assert_eq!(report["features"]["renderer.cpu"]["available"], json!(true));
    assert_eq!(
        report["features"]["renderer.gpu"]["available"],
        json!(false)
    );
    assert_eq!(
        report["features"]["effects.runtime"]["available"],
        json!(false)
    );
}

use super::*;
use crate::adapter_choice::AdapterPolicy;

const LAVAPIPE: &str = "llvmpipe (LLVM 15.0.7, 256 bits)";

fn record(
    index: usize,
    name: &str,
    software: bool,
    renders: Result<(), String>,
) -> alpha2_gpu::AdapterRecord {
    let mut record = alpha2_gpu::AdapterRecord::unasked(index, name.to_owned(), "vulkan", software);
    record.renders = renders;
    record
}

#[test]
fn an_explicit_gpu_renderer_refuses_a_software_adapter() {
    let records = vec![
        record(0, "GPU", false, Err("device lost".to_owned())),
        record(1, LAVAPIPE, true, Ok(())),
    ];
    let software = Some((LAVAPIPE.to_owned(), "vulkan"));
    let error =
        crate::explicit_gpu_adapter(&AdapterPolicy::Auto, software.clone(), records.clone())
            .expect_err("only a software adapter renders");
    assert_eq!(error.code, "GPU_INIT_FAILED");
    let message = &error.message;
    assert!(
        message.starts_with("--renderer gpu found no hardware adapter: "),
        "{message}"
    );
    assert!(
        message.contains("[0] GPU (vulkan, hardware): device lost"),
        "{message}"
    );
    assert!(
        message.contains(&format!("[1] {LAVAPIPE} (vulkan, software)")),
        "{message}"
    );
    assert!(
        message.contains("render on the CPU with --renderer cpu"),
        "{message}"
    );
    assert!(
        message.ends_with(&format!(
            "or pin a software adapter with ALPHA2_GPU_ADAPTER=\"{LAVAPIPE}\""
        )),
        "{message}"
    );
    let details = error.details.as_ref().expect("details");
    let choice = &details["adapterChoice"];
    assert_eq!(choice["reason"], json!("no-hardware-adapter"), "{choice}");
    assert_eq!(choice["chosen"], Value::Null, "{choice}");
    let candidates = choice["candidates"].as_array().expect("candidates");
    assert_eq!(candidates.len(), 2, "{choice}");
    assert_eq!(candidates[1]["name"], json!(LAVAPIPE));
    assert_eq!(candidates[1]["software"], json!(true));
    let fatal = crate::fatal_error(error);
    let failure = fatal.failure.expect("a failure report");
    assert_eq!(failure["code"], json!("INVALID_INPUT"), "{failure}");
    assert_eq!(failure["resource"], json!("gpu"), "{failure}");

    let unlisted = crate::explicit_gpu_adapter(&AdapterPolicy::Auto, software.clone(), Vec::new())
        .expect_err("the renderer's adapter alone");
    assert!(
        unlisted
            .message
            .contains(&format!("[0] {LAVAPIPE} (vulkan, software)")),
        "{}",
        unlisted.message
    );
    for pin in [
        AdapterPolicy::Name("llvmpipe".to_owned()),
        AdapterPolicy::Index(1),
    ] {
        crate::explicit_gpu_adapter(&pin, software.clone(), records.clone())
            .unwrap_or_else(|error| panic!("{pin:?} pins it: {}", error.message));
    }
    crate::explicit_gpu_adapter(&AdapterPolicy::Auto, None, records)
        .expect("a hardware adapter renders");
}

#[test]
fn a_gpu_renderer_without_any_adapter_offers_only_the_cpu() {
    let error = crate::no_hardware_adapter(&AdapterPolicy::Auto, Vec::new());
    assert_eq!(
        error.message,
        "--renderer gpu found no hardware adapter: no adapter; render on the CPU with --renderer \
         cpu, or pin a software adapter with ALPHA2_GPU_ADAPTER=<name>"
    );
}

#[test]
fn a_pinned_software_adapter_renders_under_gpu() {
    let _gpu = gpu_test_lock();
    let pin = if cfg!(target_os = "windows") {
        "Microsoft Basic Render"
    } else {
        "llvmpipe"
    };
    let host = crate::create_host_on(
        VideoDecodePolicy::Software,
        crate::config::RendererPolicy::Gpu,
        &AdapterPolicy::Name(pin.to_owned()),
    );
    let mut host = match host {
        Ok(host) => host,
        Err(error) => {
            alpha2_test_support::skip!(
                alpha2_test_support::Capability::SoftwareAdapter,
                "no {pin} adapter here ({})",
                error.message
            );
            return;
        }
    };
    let report = serde_json::to_value(host.capability_report()).expect("report");
    assert_eq!(report["gpu"]["softwareAdapter"], json!(true), "{report}");
    let directory = scratch_directory("pinned-software-adapter");
    let project = include_str!("../../../../tests/cases/host-parity-shapes-project.json");
    let assets = HashMap::from([("still".to_owned(), corpus("parity-image.png"))]);
    let result = host
        .export(
            project,
            &assets,
            &HashMap::new(),
            Vec::new(),
            0,
            alpha2_time::TICKS_PER_SECOND / 4,
            RateRequest { num: 24, den: 1 },
            false,
            crate::protocol::ResolvedFormat {
                container: alpha2_media::Container::Mp4,
                video: NativeVideoCodec::Mpeg4Part2,
                audio: NativeAudioCodec::Aac,
                pixels: alpha2_media::PixelLayout::YUV420P,
            },
            None,
            VideoQualityRequest::Level(QualityLevel::High),
            AudioQualityRequest::Level(QualityLevel::High),
            None,
            directory.join("pinned.mp4"),
            &mut |_| Ok(()),
        )
        .expect("an export on the pinned software adapter");
    assert_eq!(result["renderer"], json!("gpu"), "{result}");
    assert_eq!(result["softwareAdapter"], json!(true), "{result}");
    assert!(
        result["adapter"]
            .as_str()
            .is_some_and(|adapter| adapter.to_lowercase().contains(&pin.to_lowercase())),
        "{result}"
    );
}

#[test]
fn results_on_the_cpu_name_no_software_adapter() {
    let _gpu = gpu_test_lock();
    let host = crate::create_host_with(
        VideoDecodePolicy::Software,
        crate::config::RendererPolicy::Cpu,
    )
    .expect("host");
    assert_eq!(host.response_metadata(0.0)["softwareAdapter"], json!(false));
    let report = serde_json::to_value(host.capability_report()).expect("report");
    assert_eq!(report["gpu"]["softwareAdapter"], json!(false), "{report}");
}

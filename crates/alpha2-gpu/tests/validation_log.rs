//! The validation collector: what a turn reports, fails on and hands back, and that an error a
//! backend's validation reports reaches it.
//!
//! The collector is the process's one logger, so the tests take turns.

use std::sync::{Mutex, MutexGuard, PoisonError};

use alpha2_gpu::validation_log::{
    self, Checkpoint, TestTurn, TurnSettings, ValidationMode, ValidationRecord, expect_errors,
};

static TURN: Mutex<()> = Mutex::new(());

/// This binary's turn, with the collector installed and emptied.
fn turn() -> MutexGuard<'static, ()> {
    let turn = TURN.lock().unwrap_or_else(PoisonError::into_inner);
    validation_log::install().expect("the collector is the logger");
    let _ = Checkpoint::begin("emptied").finish();
    turn
}

/// Logs `message` at `level` from `target`, as `wgpu-hal` would.
fn log_from(target: &str, level: log::Level, message: &str) {
    log::logger().log(
        &log::Record::builder()
            .target(target)
            .level(level)
            .args(format_args!("{message}"))
            .build(),
    );
}

fn settings(mode: ValidationMode) -> TurnSettings {
    TurnSettings { mode, report: None }
}

fn messages(records: &[ValidationRecord]) -> Vec<&str> {
    records
        .iter()
        .map(|record| record.message.as_str())
        .collect()
}

#[test]
fn a_wgpu_hal_error_inside_a_turn_fails_the_turn_in_fail_mode() {
    let _turn = turn();
    let failed = std::panic::catch_unwind(|| {
        let _gpu = TestTurn::with_settings((), settings(ValidationMode::Fail));
        log_from(
            "wgpu_hal::vulkan::instance",
            log::Level::Error,
            "a deliberate error",
        );
    })
    .expect_err("the turn fails");
    let message = failed.downcast_ref::<String>().expect("a formatted panic");
    assert!(message.contains("1 validation error"), "{message}");
    assert!(message.contains("a deliberate error"), "{message}");
}

#[test]
fn a_warning_inside_a_turn_is_reported_without_failing_it() {
    let _turn = turn();
    let directory = scratch("warning");
    let report = directory.join("report.jsonl");
    let gpu = TestTurn::with_settings(
        (),
        TurnSettings {
            mode: ValidationMode::Fail,
            report: Some(report.clone()),
        },
    );
    log_from("wgpu_hal::gles", log::Level::Warn, "a deliberate warning");
    drop(gpu);
    let written = std::fs::read_to_string(&report).expect("the report");
    assert!(written.contains("a deliberate warning"), "{written}");
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn a_record_outside_any_turn_is_reported_by_the_next_checkpoint() {
    let _turn = turn();
    log_from(
        "wgpu_hal::vulkan::instance",
        log::Level::Error,
        "a late callback",
    );
    let records = Checkpoint::begin("the next test").finish();
    assert_eq!(
        messages(&records),
        ["before the next test: a late callback"]
    );
}

#[test]
fn expected_errors_are_returned_and_not_reported() {
    let _turn = turn();
    let checkpoint = Checkpoint::begin("a test with a deliberate error");
    let ((), expected) = expect_errors(|| {
        log_from(
            "wgpu_hal::vulkan::instance",
            log::Level::Error,
            "an expected error",
        );
    });
    assert_eq!(messages(&expected), ["an expected error"]);
    assert_eq!(expected[0].level, log::Level::Error);
    assert_eq!(expected[0].target, "wgpu_hal::vulkan::instance");
    assert_eq!(checkpoint.finish(), Vec::<ValidationRecord>::new());
}

#[test]
fn the_lines_naming_a_messages_objects_join_that_message() {
    let _turn = turn();
    let checkpoint = Checkpoint::begin("a test with a Vulkan message");
    log_from(
        "wgpu_hal::vulkan::instance",
        log::Level::Error,
        "VALIDATION [an id]",
    );
    log_from(
        "wgpu_hal::vulkan::instance",
        log::Level::Error,
        "\tobjects: (type: DEVICE)",
    );
    assert_eq!(
        messages(&checkpoint.finish()),
        ["VALIDATION [an id]\n\tobjects: (type: DEVICE)"]
    );
}

#[test]
fn records_of_other_targets_are_ignored() {
    let _turn = turn();
    let checkpoint = Checkpoint::begin("a test with other messages");
    log_from(
        "wgpu_core::device",
        log::Level::Error,
        "not a validation message",
    );
    log_from("naga::front", log::Level::Warn, "not one either");
    log_from(
        "wgpu_hal::vulkan",
        log::Level::Info,
        "below the kept levels",
    );
    assert_eq!(checkpoint.finish(), Vec::<ValidationRecord>::new());
}

#[test]
fn report_mode_writes_one_json_line_per_record() {
    let _turn = turn();
    let directory = scratch("report");
    let report = directory.join("report.jsonl");
    let gpu = TestTurn::with_settings(
        (),
        TurnSettings {
            mode: ValidationMode::Report,
            report: Some(report.clone()),
        },
    );
    log_from(
        "wgpu_hal::vulkan::instance",
        log::Level::Error,
        "first \"quoted\"\nline",
    );
    log_from("wgpu_hal::gles", log::Level::Warn, "second");
    drop(gpu);
    let written = std::fs::read_to_string(&report).expect("the report");
    let lines: Vec<serde_json::Value> = written
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .collect();
    assert_eq!(lines.len(), 2, "{written}");
    let test = std::thread::current()
        .name()
        .expect("libtest names the thread")
        .to_owned();
    assert_eq!(
        lines[0],
        serde_json::json!({
            "test": test,
            "level": "ERROR",
            "target": "wgpu_hal::vulkan::instance",
            "message": "first \"quoted\"\nline",
        })
    );
    assert_eq!(lines[1]["level"], "WARN");
    assert_eq!(lines[1]["message"], "second");
    std::fs::remove_dir_all(&directory).ok();
}

/// A new empty directory for one test's files.
fn scratch(name: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "alpha2-validation-log-{name}-{}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&directory).ok();
    std::fs::create_dir_all(&directory).expect("scratch directory");
    directory
}

/// A device on `backends` with validation on, and wgpu's debug messages routed to the log,
/// on the adapter `WGPU_ADAPTER_NAME` names or the default one; `None` without an adapter.
#[cfg(any(target_os = "linux", windows))]
fn validated_device(
    backends: wgpu::Backends,
    debug: bool,
) -> Option<(wgpu::Adapter, wgpu::Device, wgpu::Queue)> {
    let _turn = alpha2_gpu::serialize_acquisition();
    let mut descriptor = alpha2_gpu::instance_descriptor(backends);
    descriptor.flags |= wgpu::InstanceFlags::VALIDATION;
    if debug {
        descriptor.flags |= wgpu::InstanceFlags::DEBUG;
    }
    let instance = wgpu::Instance::new(descriptor);
    let adapter = pollster::block_on(wgpu::util::initialize_adapter_from_env_or_default(
        &instance, None,
    ))
    .ok()?;
    eprintln!("validation tests on {:?}", adapter.get_info());
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .expect("a device");
    Some((adapter, device, queue))
}

/// Whether a Khronos validation layer of version 1.4 or newer is installed.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn validation_layer_installed() -> bool {
    // SAFETY: loads the system's Vulkan loader, which is only asked for its layers.
    let Ok(entry) = (unsafe { ash::Entry::load() }) else {
        return false;
    };
    // SAFETY: a query with no arguments on a loaded entry.
    let layers = unsafe { entry.enumerate_instance_layer_properties() }.unwrap_or_default();
    layers.iter().any(|layer| {
        layer.layer_name_as_c_str().ok() == Some(c"VK_LAYER_KHRONOS_validation")
            && layer.spec_version >= ash::vk::make_api_version(0, 1, 4, 0)
    })
}

#[cfg(target_os = "linux")]
#[test]
#[allow(unsafe_code)]
fn an_injected_vulkan_error_reaches_the_validation_log() {
    let _turn = turn();
    if !validation_layer_installed() {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::VulkanValidation,
            "no Khronos validation layer 1.4 or newer is installed (VK_LAYER_PATH)"
        );
        return;
    }
    let Some((adapter, device, queue)) = validated_device(wgpu::Backends::VULKAN, true) else {
        alpha2_test_support::skip!(alpha2_test_support::Capability::Vulkan, "no Vulkan adapter");
        return;
    };
    let gpu = alpha2_gpu::GpuContext::from_parts(adapter, device, queue);
    // SAFETY: the raw device makes one buffer and destroys it before the guard drops.
    let hal = unsafe { gpu.device().as_hal::<wgpu::hal::api::Vulkan>() }.expect("a Vulkan device");
    let raw = hal.raw_device();
    // A buffer of no size: VUID-VkBufferCreateInfo-size-00912.
    let info = ash::vk::BufferCreateInfo::default()
        .size(0)
        .usage(ash::vk::BufferUsageFlags::TRANSFER_DST);
    let ((), records) = expect_errors(|| {
        // SAFETY: a live device; a buffer it makes is destroyed at once and never used.
        if let Ok(buffer) = unsafe { raw.create_buffer(&info, None) } {
            // SAFETY: as above.
            unsafe { raw.destroy_buffer(buffer, None) };
        }
    });
    assert!(
        records
            .iter()
            .any(|record| record.level == log::Level::Error
                && record.target.starts_with("wgpu_hal::vulkan")
                && record
                    .message
                    .contains("VUID-VkBufferCreateInfo-size-00912")),
        "the layer's error did not reach the collector: {records:#?}"
    );
}

#[cfg(windows)]
#[test]
#[allow(unsafe_code)]
fn an_injected_d3d12_error_reaches_the_validation_log() {
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_HEAP_FLAG_NONE, D3D12_HEAP_PROPERTIES, D3D12_HEAP_TYPE_DEFAULT,
        D3D12_MESSAGE_SEVERITY_ERROR, D3D12_RESOURCE_DESC, D3D12_RESOURCE_DIMENSION_BUFFER,
        D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COMMON, D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ID3D12Device, ID3D12InfoQueue, ID3D12Resource,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};
    use windows::core::Interface as _;

    /// A buffer of no width, which the debug layer reports as an error, then an error added to
    /// the queue directly.
    fn inject(raw: &ID3D12Device, queue: &ID3D12InfoQueue, probe: windows::core::PCSTR) {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let description = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Alignment: 0,
            Width: 0,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_UNKNOWN,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_NONE,
        };
        let mut resource: Option<ID3D12Resource> = None;
        // SAFETY: a live device; the descriptions outlive the call, which refuses them.
        let made = unsafe {
            raw.CreateCommittedResource(
                &raw const heap,
                D3D12_HEAP_FLAG_NONE,
                &raw const description,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &raw mut resource,
            )
        };
        assert!(made.is_err(), "a buffer of no width was made");
        // SAFETY: a live queue and a NUL-terminated message.
        unsafe { queue.AddApplicationMessage(D3D12_MESSAGE_SEVERITY_ERROR, probe) }
            .expect("an application message");
    }

    let _turn = turn();
    // Windows always has WARP.
    let (adapter, device, queue) =
        validated_device(wgpu::Backends::DX12, false).expect("a DX12 adapter");
    // SAFETY: the raw device is only queried for interfaces and asked to make a buffer it refuses.
    let hal = unsafe { device.as_hal::<wgpu::hal::api::Dx12>() }.expect("a DX12 device");
    let raw = hal.raw_device().clone();
    drop(hal);
    let Ok(info_queue) = raw.cast::<ID3D12InfoQueue>() else {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::D3d12DebugLayer,
            "the D3D12 debug layer is not installed: Add-WindowsCapability -Online -Name \
             Tools.Graphics.DirectX~~~~0.0.1.0"
        );
        return;
    };
    // First through wgpu-hal's debugger route alone: the device is not watched yet. Whether this
    // route delivers decides nothing below; it is printed for the record.
    let ((), routed) = expect_errors(|| {
        inject(
            &raw,
            &info_queue,
            windows::core::s!("alpha2 debugger route probe"),
        );
    });
    eprintln!(
        "the debugger route delivered {} record(s): {:?}",
        routed.len(),
        messages(&routed)
    );
    // Then as every test device is: adopted by a context, which watches its queue.
    let gpu = alpha2_gpu::GpuContext::from_parts(adapter, device, queue);
    let ((), records) = expect_errors(|| {
        inject(&raw, &info_queue, windows::core::s!("alpha2 sink probe"));
    });
    drop(gpu);
    let errors = records
        .iter()
        .filter(|record| record.level == log::Level::Error)
        .map(|record| record.message.as_str())
        .collect::<Vec<_>>();
    assert!(
        errors.contains(&"alpha2 sink probe"),
        "the application message did not reach the collector: {records:#?}"
    );
    assert!(
        errors.len() >= 2,
        "the refused buffer's error did not reach the collector: {records:#?}"
    );
    assert!(
        !messages(&records).contains(&"alpha2 debugger route probe"),
        "a message read before the device was watched came again: {records:#?}"
    );
}

/// `ALPHA2_VULKAN_DEVICE` is not passed on to `FFmpeg`, which reads a
/// value that starts with a digit (`4090`, `780M`) as a device index,
/// while the host matches it against adapter names. The index a probe
/// passes is that of the adapter the host chooses.
#[test]
fn a_device_selector_resolves_to_the_adapter_the_host_chooses() {
    let _turn = alpha2_gpu::serialize_acquisition();
    let instance = super::hal_instance().expect("instance");
    // SAFETY: adapter enumeration does not require a surface.
    let adapters = unsafe { instance.enumerate_adapters(None) };
    let Some(first) = adapters.first() else {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::Vulkan,
            "the Vulkan instance lists no adapter"
        );
        return;
    };
    let raw = first.adapter.shared_instance().raw_instance();
    // SAFETY: the instance is alive; enumeration has no other precondition.
    let order = unsafe { raw.enumerate_physical_devices() }.expect("devices");
    let name_at = |index: usize| {
        // SAFETY: `order[index]` was enumerated from `raw`.
        let properties = unsafe { raw.get_physical_device_properties(order[index]) };
        properties
            .device_name_as_c_str()
            .expect("name")
            .to_string_lossy()
            .into_owned()
    };
    let mut checked = 0;
    for adapter in adapters.iter().filter(|adapter| {
        adapter
            .features
            .contains(wgpu::Features::TEXTURE_FORMAT_NV12)
    }) {
        let name = &adapter.info.name;
        let index = super::shared_device_index(Some(name)).expect("by its name");
        assert!(
            name_at(index).contains(name.as_str()),
            "{name}: {}",
            name_at(index)
        );
        // A word of the name that starts with a digit names the adapter, not an index.
        for word in name
            .split(' ')
            .filter(|word| word.starts_with(|c: char| c.is_ascii_digit()))
        {
            let index = super::shared_device_index(Some(word)).expect("by a word of it");
            assert!(name_at(index).contains(word), "{word}: {}", name_at(index));
            checked += 1;
        }
    }
    let default = super::shared_device_index(None);
    if let Ok(index) = default {
        assert!(index < order.len());
    }
    eprintln!("checked {checked} digit-leading selectors");
    assert!(super::shared_device_index(Some("no adapter has this name")).is_err());
}

#[test]
fn encode_copier_timeout_retains_its_entire_ownership_graph() {
    let output = failure_tests::child_output(
        std::process::Command::new(std::env::current_exe().expect("test executable")).args([
            "--exact",
            "vulkan::tests::encode_copier_timeout_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ]),
        "encode_copier_timeout_child",
    );
    eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "run by encode_copier_timeout_retains_its_entire_ownership_graph"]
fn encode_copier_timeout_child() {
    use super::*;
    let Some(context) = failure_tests::shared_context() else {
        return;
    };
    let gpu = context.gpu();
    let (luma, chroma) = failure_tests::rendered_planes(&gpu, 64, 64);
    let mut frames = failure_tests::encode_frames(&context, 1);
    // A child isolates this short timeout from concurrently running tests.
    set_wait_timeout(std::time::Duration::from_millis(1));
    for timeout in [false, true] {
        let mut copier = VulkanEncodeCopier::new(&context).expect("copier");
        let probe = Arc::new(());
        let weak = Arc::downgrade(&probe);
        copier
            .retained
            .as_deref_mut()
            .expect("retained work")
            .lifetime_probe = Some(probe);
        // A copy whose encoder frame the GPU still waits on: its fence cannot signal within 1 ms.
        let mut blocked = None;
        if timeout {
            let mut access = alpha2_ffmpeg_bridge::VulkanEncodeFrameAccess::begin(&mut frames[0])
                .expect("frame access");
            // SAFETY: breaks the contract on purpose: the copy is to wait on a value nothing
            // signals, and the test's frames are given up without being waited on again.
            let descriptor = unsafe { access.access_mut() }.frame_mut();
            let value = descriptor.semaphore_values[0] + 1000;
            descriptor.semaphore_values[0] = value;
            blocked = Some((descriptor.semaphores[0], value));
            copier
                .submit(&mut access, &luma, &chroma, (64, 64))
                .expect("the copy submits");
        }
        let work_ptr = std::ptr::from_mut(copier.retained.as_deref_mut().expect("retained"));
        let handle = copier.copier;
        let started = std::time::Instant::now();
        let result = copier.finish();
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        let Some((semaphore, value)) = blocked else {
            result.expect("finished");
            assert!(weak.upgrade().is_none(), "completed work must be released");
            continue;
        };
        assert_eq!(
            result.as_ref().err().and_then(BridgeError::gpu_fault),
            Some(alpha2_ffmpeg_bridge::GpuFault::Timeout),
            "{result:?}"
        );
        assert!(
            weak.upgrade().is_some(),
            "timeout must retain images and device"
        );
        // Let the copy run, so the device ends with nothing outstanding, then release what the
        // timeout path deliberately forgot.
        // SAFETY: the ownership probe confirmed the graph is still live; `context` also holds
        // this device, and the semaphore is the frame's live timeline.
        unsafe {
            let device = gpu
                .device()
                .as_hal::<wgpu_hal::api::Vulkan>()
                .expect("Vulkan");
            device
                .raw_device()
                .signal_semaphore(
                    &vk::SemaphoreSignalInfo::default()
                        .semaphore(vk::Semaphore::from_raw(semaphore))
                        .value(value),
                )
                .expect("signal");
            device.raw_device().device_wait_idle().expect("idle");
            assert_eq!(
                alpha2_native_gpu_vulkan_encode_copier_destroy(handle, std::ptr::null_mut()),
                0
            );
            drop(Box::from_raw(work_ptr));
        }
        assert!(weak.upgrade().is_none(), "drained work can now be released");
    }
}

#[test]
fn a_decode_probe_that_times_out_says_so_and_frees_nothing_the_gpu_may_use() {
    // A probe whose fence wait times out must leave the fence and command pool of its batch,
    // still pending on the GPU, alive, and report a timeout rather than an FFmpeg code.
    let output = failure_tests::child_output(
        std::process::Command::new(std::env::current_exe().expect("test executable")).args([
            "--exact",
            "vulkan::tests::decode_probe_timeout_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ]),
        "decode_probe_timeout_child",
    );
    eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "run by a_decode_probe_that_times_out_says_so_and_frees_nothing_the_gpu_may_use"]
fn decode_probe_timeout_child() {
    use super::*;
    let Some(context) = alpha2_test_support::available!(
        alpha2_test_support::Capability::VulkanVideo,
        SharedVulkanContext::create().map_err(|error| format!("no Vulkan Video device ({error})"))
    ) else {
        return;
    };
    // A sampled NV12 frame of the shared device, as a decoder would hand out.
    let mut frames = context.ffmpeg_device().hwframe_ctx_alloc();
    {
        let data = frames.data();
        data.format = rsmpeg::ffi::AV_PIX_FMT_VULKAN;
        data.sw_format = rsmpeg::ffi::AV_PIX_FMT_NV12;
        data.width = 64;
        data.height = 64;
        data.initial_pool_size = 0;
    }
    alpha2_ffmpeg_bridge::vulkan_frames_add_usage(
        &mut frames,
        alpha2_ffmpeg_bridge::VULKAN_ENCODE_INPUT_USAGE,
    )
    .expect("usage");
    frames.init().expect("frames context");
    let mut frame = rsmpeg::avutil::AVFrame::new();
    frames.get_buffer(&mut frame).expect("frame");
    let lease = VulkanFrameLease::new(frame);
    // The probe waits on a timeline value nothing has signalled yet, so its batch stays pending
    // past a short wait. A child isolates the short timeout from concurrently running tests.
    let (semaphore, blocked) = {
        let mut guard = lease.lock_access().expect("frame access");
        // SAFETY: breaks the contract on purpose: the probe is to wait on a value nothing
        // signals yet, which the test signals from the host before the frame is used again.
        let descriptor = unsafe { guard.access_mut() }.frame_mut();
        let semaphore = descriptor.semaphores[0];
        let blocked = descriptor.semaphore_values[0] + 1000;
        for index in 0..descriptor.images.len() {
            if descriptor.images[index] != 0 {
                descriptor.semaphore_values[index] = blocked;
            }
        }
        guard.commit();
        (semaphore, blocked)
    };
    set_wait_timeout(std::time::Duration::from_millis(1));
    // SAFETY: a counter read.
    let retained_before = unsafe { alpha2_native_gpu_vulkan_retained_helpers() };
    let started = std::time::Instant::now();
    let error = probe_vulkan_submission(&lease)
        .expect_err("the probe cannot finish before its wait is satisfied");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(
        error.gpu_fault(),
        Some(alpha2_ffmpeg_bridge::GpuFault::Timeout),
        "{error}"
    );
    assert!(
        error
            .to_string()
            .contains("GPU work did not complete within the wait timeout"),
        "{error}"
    );
    // SAFETY: a counter read.
    let retained_after = unsafe { alpha2_native_gpu_vulkan_retained_helpers() };
    assert_eq!(
        retained_after,
        retained_before + 1,
        "the probe must leave its pending fence and command pool alive"
    );
    // The pending batch still signals when it runs, so the frame must record that value: the
    // next submission on the frame (FFmpeg's, after a probe) signals the one above it, which a
    // timeline below would make go backwards (VUID-VkSubmitInfo-pSignalSemaphores-03242).
    let recorded = lease
        .lock_access()
        .expect("frame access")
        .access()
        .frame()
        .semaphore_values[0];
    // Let the pending batch run, so the device ends with nothing outstanding (and a failure
    // below does not leave the frame's free waiting on it).
    // SAFETY: the semaphore is the frame's live timeline, and `blocked` is above its current
    // value.
    let counter = unsafe {
        let device = context
            .gpu()
            .device()
            .as_hal::<wgpu_hal::api::Vulkan>()
            .expect("Vulkan");
        let device = device.raw_device();
        device
            .signal_semaphore(
                &vk::SemaphoreSignalInfo::default()
                    .semaphore(vk::Semaphore::from_raw(semaphore))
                    .value(blocked),
            )
            .expect("signal");
        device.device_wait_idle().expect("idle");
        device
            .get_semaphore_counter_value(vk::Semaphore::from_raw(semaphore))
            .expect("counter")
    };
    assert_eq!(
        (counter, recorded),
        (blocked + 1, blocked + 1),
        "the frame must record what the probe's submission signals: (timeline, recorded)"
    );
}

use std::path::{Path, PathBuf};

use super::*;

/// Held by each test that makes a shared device in this process, for as
/// long as the device lives: side by side, one device's destruction could
/// deadlock the NVIDIA driver against another's object creation. The
/// failure tests make theirs in child processes instead.
fn in_process_device() -> std::sync::MutexGuard<'static, ()> {
    static DEVICE: Mutex<()> = Mutex::new(());
    DEVICE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn creates_one_device_for_wgpu_and_ffmpeg() {
    let _device = in_process_device();
    let Some(context) = alpha2_test_support::available!(
        alpha2_test_support::Capability::VulkanVideo,
        SharedVulkanContext::create().map_err(|error| format!("no Vulkan Video device ({error})"))
    ) else {
        return;
    };
    assert!(context.decode_codecs().contains(&"h264"));
    assert_ne!(context.identity().device, 0);
}

#[test]
fn vulkan_frames_add_usage_refuses_a_frames_context_of_another_device_type() {
    use alpha2_ffmpeg_bridge::{vulkan_frames_add_usage, vulkan_frames_usage};
    use rsmpeg::ffi;
    let device = rsmpeg::avutil::AVHWDeviceContext::alloc(ffi::AV_HWDEVICE_TYPE_VULKAN);
    let mut frames = device.hwframe_ctx_alloc();
    vulkan_frames_add_usage(&mut frames, 0x1).expect("a Vulkan frames context");
    assert_eq!(vulkan_frames_usage(&frames) & 0x1, 0x1);
    // Only Vulkan is compiled into the pinned SDK, so another device type is stood in for by
    // relabelling this one: the type is all the check reads, and it is restored before the
    // context is freed.
    let device_context = frames.data().device_ctx;
    // SAFETY: the frames context holds a reference to its live device.
    unsafe { (*device_context).type_ = ffi::AV_HWDEVICE_TYPE_CUDA };
    let refused = vulkan_frames_add_usage(&mut frames, 0x2);
    let usage = vulkan_frames_usage(&frames);
    // SAFETY: as above.
    unsafe { (*device_context).type_ = ffi::AV_HWDEVICE_TYPE_VULKAN };
    assert!(refused.is_err(), "a CUDA frames context must be refused");
    assert_eq!(usage, 0, "a CUDA frames context has no Vulkan usage");
}

/// The body [`the_validation_layer_counts_as_loaded_exactly_when_the_loader_loads_it`]
/// runs in a child process, one per environment.
#[test]
#[ignore = "run by the_validation_layer_counts_as_loaded_exactly_when_the_loader_loads_it"]
fn validation_layer_child() {
    let _turn = alpha2_gpu::serialize_acquisition();
    let instance = hal_instance().expect("Vulkan instance");
    // SAFETY: adapter enumeration does not require a surface.
    let adapters = unsafe { instance.enumerate_adapters(None) };
    let Some(exposed) = adapters.first() else {
        eprintln!("ALPHA2-LAYER none-adapter");
        return;
    };
    let shared = exposed.adapter.shared_instance();
    let layer =
        activated_validation_layer(shared.raw_instance(), exposed.adapter.raw_physical_device());
    // SAFETY: a global command with no handles.
    let loader = unsafe { shared.entry().try_enumerate_instance_version() }
        .ok()
        .flatten()
        .unwrap_or(vk::API_VERSION_1_0);
    eprintln!(
        "ALPHA2-LAYER loader={} layer={}",
        VulkanLayerVersion::from_packed(loader),
        layer.map_or_else(|| "none".to_owned(), |layer| layer.to_string())
    );
}

/// The validation layer counts as loaded when the loader loads it, by
/// whichever route, and only then. What the loader does is
/// fixed by its version: `VK_INSTANCE_LAYERS` and wgpu's own request work
/// on every loader, the filter variables only from 1.3.234, and there they
/// match case-insensitively, take four glob forms, enable nothing for
/// `~explicit~`, and can disable a layer something else enables.
#[test]
#[allow(clippy::too_many_lines)]
fn the_validation_layer_counts_as_loaded_exactly_when_the_loader_loads_it() {
    let run = |env: &[(&str, &str)]| {
        let mut command =
            std::process::Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "vulkan::tests::validation_layer_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("WGPU_VALIDATION", "0");
        for name in [
            "VK_INSTANCE_LAYERS",
            "VK_LOADER_LAYERS_ENABLE",
            "VK_LOADER_LAYERS_DISABLE",
        ] {
            command.env_remove(name);
        }
        command.envs(env.iter().copied());
        let output = failure_tests::child_output(&mut command, "validation_layer_child");
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "{env:?}:\n{stderr}");
        stderr
            .lines()
            .find_map(|line| line.strip_prefix("ALPHA2-LAYER "))
            .unwrap_or_else(|| panic!("{env:?}: no report:\n{stderr}"))
            .to_owned()
    };
    let report = |env: &[(&str, &str)]| -> Option<(VulkanLayerVersion, bool)> {
        let line = run(env);
        if line == "none-adapter" {
            return None;
        }
        let mut fields = line.split(' ').filter_map(|field| field.split_once('='));
        let (_, loader) = fields.next().expect("loader");
        let (_, layer) = fields.next().expect("layer");
        let mut parts = loader
            .split('.')
            .map(|part| part.parse::<u32>().expect("version"));
        let loader = VulkanLayerVersion {
            major: parts.next().expect("major"),
            minor: parts.next().expect("minor"),
            patch: parts.next().expect("patch"),
        };
        Some((loader, layer != "none"))
    };
    let requested = [("VK_INSTANCE_LAYERS", KHRONOS_VALIDATION)];
    let Some((loader, installed)) = report(&requested) else {
        alpha2_test_support::skip!(alpha2_test_support::Capability::Vulkan, "no Vulkan adapter");
        return;
    };
    if !installed {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::VulkanValidation,
            "no Khronos validation layer is installed"
        );
        return;
    }
    let filters = loader
        >= VulkanLayerVersion {
            major: 1,
            minor: 3,
            patch: 234,
        };
    let cases: [(&[(&str, &str)], bool); 9] = [
        (&[], false),
        (&[("WGPU_VALIDATION", "1")], true),
        (
            &[(
                "VK_INSTANCE_LAYERS",
                "VK_LAYER_MESA_overlay:VK_LAYER_KHRONOS_validation",
            )],
            true,
        ),
        (&[("VK_LOADER_LAYERS_ENABLE", "*Validation")], filters),
        (
            &[("VK_LOADER_LAYERS_ENABLE", "vk_layer_khronos_validation")],
            filters,
        ),
        (&[("VK_LOADER_LAYERS_ENABLE", "x,*khronos*")], filters),
        (&[("VK_LOADER_LAYERS_ENABLE", "~explicit~")], false),
        (
            &[("VK_LOADER_LAYERS_ENABLE", "VK_LAYER_*_validation")],
            false,
        ),
        (
            &[
                ("VK_INSTANCE_LAYERS", KHRONOS_VALIDATION),
                ("VK_LOADER_LAYERS_DISABLE", "*validation"),
            ],
            !filters,
        ),
    ];
    let wrong = cases
        .iter()
        .filter_map(|(env, loaded)| {
            let found = report(env).expect("an adapter").1;
            (found != *loaded).then(|| format!("{env:?}: loaded {found}, expected {loaded}"))
        })
        .collect::<Vec<_>>();
    assert!(wrong.is_empty(), "loader {loader}: {wrong:#?}");
}

#[test]
fn the_shared_device_takes_the_adapter_wgpu_power_pref_asks_for() {
    use wgpu::{DeviceType, PowerPreference};
    let order = |preference| {
        let mut types = [
            DeviceType::Cpu,
            DeviceType::IntegratedGpu,
            DeviceType::DiscreteGpu,
        ];
        types.sort_by_key(|device_type| adapter_rank(*device_type, preference));
        types[0]
    };
    assert_eq!(order(None), DeviceType::DiscreteGpu);
    assert_eq!(
        order(Some(PowerPreference::HighPerformance)),
        DeviceType::DiscreteGpu
    );
    assert_eq!(order(Some(PowerPreference::None)), DeviceType::DiscreteGpu);
    assert_eq!(
        order(Some(PowerPreference::LowPower)),
        DeviceType::IntegratedGpu
    );
}

#[test]
fn the_encode_extent_is_the_profile_and_granularity_h264_vulkan_opens_with() {
    use vk::native::{
        StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_BASELINE as BASELINE,
        StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH as HIGH,
        StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_MAIN as MAIN,
    };
    // FFmpeg keeps the last profile the device answers for, in its own order.
    assert_eq!(last_supported_profile(Some), Some(("high", HIGH)));
    assert_eq!(
        last_supported_profile(|idc| (idc != HIGH).then_some(idc)),
        Some(("main", MAIN))
    );
    assert_eq!(
        last_supported_profile(|idc| (idc == BASELINE).then_some(idc)),
        Some(("constrained_baseline", BASELINE))
    );
    assert_eq!(last_supported_profile(|_| None::<()>), None);
    // Mesa ANV's H.264 encoder: 16x16 picture access, 32x32 encode input, which is what FFmpeg
    // aligns to. It codes 1280x720 as 1280x736.
    let coded = (
        vk::Extent2D {
            width: 16,
            height: 16,
        },
        vk::Extent2D {
            width: 4096,
            height: 4096,
        },
    );
    let encode = vk::VideoEncodeCapabilitiesKHR {
        encode_input_picture_granularity: vk::Extent2D {
            width: 32,
            height: 32,
        },
        ..Default::default()
    };
    let extent = encode_extent(
        coded,
        &encode,
        &vk::VideoEncodeH264CapabilitiesKHR::default(),
    );
    assert_eq!(extent.granularity, (32, 32));
    assert_eq!(extent.coded(1280, 720), (1280, 736));
}

#[test]
fn this_device_reports_the_h264_profile_ffmpeg_opens() {
    let _device = in_process_device();
    let Some(context) = alpha2_test_support::available!(
        alpha2_test_support::Capability::VulkanVideo,
        SharedVulkanContext::create().map_err(|error| format!("no Vulkan Video device ({error})"))
    ) else {
        return;
    };
    let Some(extent) = context.encode_extent() else {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::VulkanVideoEncode,
            "no H.264 encode on this device"
        );
        return;
    };
    // Every device with Vulkan H.264 encode this runs on (NVIDIA, RADV) has High, which FFmpeg
    // then picks.
    assert_eq!(extent.profile, "high", "{extent:?}");
}

#[test]
fn layer_versions_order_by_major_minor_patch() {
    let packaged = VulkanLayerVersion::from_packed(vk::make_api_version(0, 1, 3, 204));
    assert_eq!(packaged.to_string(), "1.3.204");
    assert!(packaged < VulkanLayerVersion::from_packed(vk::make_api_version(0, 1, 4, 0)));
}

/// The body [`creation_never_crashes_on_any_installed_driver`] runs in a
/// child process, one per driver.
#[test]
#[ignore = "run by creation_never_crashes_on_any_installed_driver"]
fn shared_context_creation_child() {
    match SharedVulkanContext::create() {
        Ok(context) => eprintln!("created: {context:?}"),
        Err(error) => eprintln!("refused: {error}"),
    }
}

/// The directories the Vulkan loader searches for driver manifests on
/// Linux, in its order (`LoaderDriverInterface.md`): the XDG config
/// directories, `/etc`, then the XDG data directories, each with
/// `vulkan/icd.d`. `var` reads the environment.
fn driver_manifest_directories(var: impl Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let set = |name: &str| var(name).filter(|value| !value.is_empty());
    let home = set("HOME");
    let config_home =
        set("XDG_CONFIG_HOME").or_else(|| home.as_ref().map(|home| format!("{home}/.config")));
    let data_home =
        set("XDG_DATA_HOME").or_else(|| home.as_ref().map(|home| format!("{home}/.local/share")));
    let config_dirs = set("XDG_CONFIG_DIRS").unwrap_or_else(|| "/etc/xdg".to_owned());
    let data_dirs =
        set("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());
    let mut directories = Vec::new();
    for base in config_home
        .into_iter()
        .chain(config_dirs.split(':').map(str::to_owned))
        .chain(["/etc".to_owned()])
        .chain(data_home)
        .chain(data_dirs.split(':').map(str::to_owned))
        .filter(|base| !base.is_empty())
    {
        let directory = Path::new(&base).join("vulkan/icd.d");
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    }
    directories
}

/// The environment that makes the loader load `manifest` and no other
/// driver: `VK_DRIVER_FILES`, which a loader from 1.3.207 prefers, and
/// `VK_ICD_FILENAMES`, which an older one reads, both name it, and the
/// variables that add or filter drivers are cleared. Inherited, any of
/// them would send every child to the driver the parent shell pinned, and
/// each would still pass.
fn one_driver_environment(manifest: &Path) -> Vec<(&'static str, Option<&Path>)> {
    vec![
        ("VK_DRIVER_FILES", Some(manifest)),
        ("VK_ICD_FILENAMES", Some(manifest)),
        ("VK_ADD_DRIVER_FILES", None),
        ("VK_LOADER_DRIVERS_SELECT", None),
        ("VK_LOADER_DRIVERS_DISABLE", None),
        ("ALPHA2_VULKAN_DEVICE", None),
    ]
}

#[test]
fn each_driver_is_tried_where_the_loader_finds_it_and_alone() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    };
    let paths = |directories: Vec<PathBuf>| {
        directories
            .iter()
            .map(|directory| directory.display().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        paths(driver_manifest_directories(env(&[("HOME", "/home/a")]))),
        [
            "/home/a/.config/vulkan/icd.d",
            "/etc/xdg/vulkan/icd.d",
            "/etc/vulkan/icd.d",
            "/home/a/.local/share/vulkan/icd.d",
            "/usr/local/share/vulkan/icd.d",
            "/usr/share/vulkan/icd.d",
        ]
    );
    assert_eq!(
        paths(driver_manifest_directories(env(&[
            ("XDG_CONFIG_HOME", "/c"),
            ("XDG_CONFIG_DIRS", "/x:/etc"),
            ("XDG_DATA_HOME", "/d"),
            ("XDG_DATA_DIRS", "/usr/share:/opt/share"),
        ]))),
        [
            "/c/vulkan/icd.d",
            "/x/vulkan/icd.d",
            "/etc/vulkan/icd.d",
            "/d/vulkan/icd.d",
            "/usr/share/vulkan/icd.d",
            "/opt/share/vulkan/icd.d",
        ]
    );
    let manifest = Path::new("/usr/share/vulkan/icd.d/lvp_icd.x86_64.json");
    let forced = one_driver_environment(manifest);
    for name in ["VK_DRIVER_FILES", "VK_ICD_FILENAMES"] {
        assert!(forced.contains(&(name, Some(manifest))), "{name}");
    }
    for name in [
        "VK_ADD_DRIVER_FILES",
        "VK_LOADER_DRIVERS_SELECT",
        "VK_LOADER_DRIVERS_DISABLE",
    ] {
        assert!(forced.contains(&(name, None)), "{name} is cleared");
    }
}

/// Creation either succeeds or refuses on every Vulkan driver installed
/// here, including ones without Vulkan Video, which `auto` must be able
/// to fall back from. Each driver runs in its own process so that a
/// crash in one fails this test instead of taking the harness down.
#[test]
fn creation_never_crashes_on_any_installed_driver() {
    let mut manifests = driver_manifest_directories(|name| std::env::var(name).ok())
        .into_iter()
        .filter_map(|directory| std::fs::read_dir(directory).ok())
        .flatten()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    manifests.sort();
    if manifests.is_empty() {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::Vulkan,
            "the Vulkan loader finds no driver manifest here"
        );
        return;
    }
    let executable = std::env::current_exe().expect("test executable");
    for manifest in manifests {
        let mut child = std::process::Command::new(&executable);
        child.args([
            "--exact",
            "vulkan::tests::shared_context_creation_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ]);
        for (name, value) in one_driver_environment(&manifest) {
            match value {
                Some(value) => child.env(name, value),
                None => child.env_remove(name),
            };
        }
        let output = failure_tests::child_output(&mut child, "shared_context_creation_child");
        assert!(
            output.status.success(),
            "shared Vulkan context creation on {} ended with {}:\n{}",
            manifest.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

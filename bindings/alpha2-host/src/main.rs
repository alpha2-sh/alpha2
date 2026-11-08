//! Native project-rendering host and command-line entry point.
//!
//! # Process protocol v2
//!
//! `alpha2-host serve` exchanges frames over stdin and stdout, each a JSON header and binary
//! attachments ([`alpha2_host_core::host_frames`]); the headers it writes are ASCII, with other
//! characters escaped as `\uXXXX`. Every header contains `protocolVersion: 2`. `serve` owns stdout
//! and writes the frames' magic, then its `ready` state, before reading input; a client may treat
//! their absence within a bound as a start failure. The host reports
//! `{type: "state", state: "draining", reason}` when a new process would serve better
//! ([`lifecycle`]), and answers what it did not start `HOST_RETIRING`.
//!
//! Requests carry a caller-unique `requestId` and an `operation`: `capabilities`, `conform` (which
//! media each source would read, without rendering), `export`, `still` (frames to PNG, as an
//! export draws them), `compose`, `cancel` or `status`. Responses are
//! `{protocolVersion, requestId, ok: true, result, hostState}` or
//! `{protocolVersion, requestId, ok: false, error: {code, message}, hostState}`. During an export
//! the host may emit `{protocolVersion, type: "progress", requestId, phase, completed, total}`,
//! with `phase` `frames` or, once every frame is encoded and the output is closing, `finalizing`.
//! A failed export's error also carries `failure`: its class, step, frame, and diagnostics.
//!
//! Requests may be pipelined and must be correlated only by `requestId`: up to `--max-running` of
//! them run at once on worker threads against one device, each answered as it ends. A host whose
//! worker threads are stuck or whose device is lost drains and exits after its answers. End of
//! stdin means no more requests: the host answers what it has read and exits.
//!
//! `{protocolVersion, requestId, operation: "cancel", target}` is answered at once with
//! `result: {target, state}`: `running` when `target` is being handled (an export stops, removes
//! its partial output and is answered `CANCELLED`), `pending` otherwise (`target` is answered
//! `CANCELLED` without running when it arrives). `SIGTERM`, `SIGINT`, `SIGHUP`, a Windows console
//! control event, and the exit of the parent process cancel the running export the same way, then
//! the process exits; see [`termination`]. Assets and outputs are local paths.
//!
//! Runtime WGSL is executable GPU input. A remote or multi-tenant host must authenticate and
//! authorize effect definitions before forwarding them; the engine validates definitions but does
//! not decide who may register one.

// A closed stderr must not panic the process: diagnostics go through `alpha2_os::diagnostic!`.
#![cfg_attr(not(test), warn(clippy::print_stderr))]

mod adapter_choice;
mod assets;
mod audio;
mod capture;
mod cli;
mod compose_request;
mod config;
mod cpu_frames;
#[cfg(all(target_os = "windows", feature = "hwencode"))]
mod d3d12va_probe;
mod decode;
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
mod dx12_adapters;
mod effect_request;
mod encode;
mod encoder_refusal;
mod export_decode;
mod extract_request;
mod failure;
mod frames;
#[cfg(all(target_os = "windows", feature = "hwencode"))]
mod gpu_encode_probe;
#[cfg(feature = "hwencode")]
mod gpu_export;
mod gpu_output;
mod heartbeat;
mod host;
mod lifecycle;
mod media;
mod memory;
mod metrics;
mod packet_copy;
mod paths;
mod plugin_fetch;
mod protocol;
mod providers;
mod recovery;
mod registrations;
mod render_cache;
mod renderer_backend;
mod request_inputs;
mod serve_io;
mod still;
mod tag;
mod teardown;
mod termination;
#[cfg(test)]
mod tests;
mod text_file;
mod trace;
#[cfg(feature = "hwencode")]
mod yuv_transcode;

use std::collections::HashSet;
use std::env;
use std::io::Write;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::cli::{conform_cli, export_cli, usage};
use crate::config::VideoDecodePolicy;
use crate::host::{Host, HostDevice};
#[cfg(test)]
use crate::lifecycle::SoftLimits;
use crate::lifecycle::{DrainReason, Lifecycle, ServeOptions};
use crate::protocol::{HostError, operation_from_header, parse_request, request_id_from_header};
use crate::serve_io::{InputFrame, Requests, Responder};
use alpha2_gpu::GpuDiagnostic;
use alpha2_host_core::host_frames::Attachment;
use alpha2_native::ffmpeg_runtime_identity;
use alpha2_render::Renderer;
use serde_json::{Value, json};

const PROTOCOL_VERSION: u32 = 2;
const GPU_INIT_EXIT_CODE: u8 = 70;
const DEFAULT_EXPORT_IN_FLIGHT_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_IN_FLIGHT_VIDEO_FRAMES: usize = 2;
const MAX_IN_FLIGHT_VIDEO_FRAMES: usize = 3;
const MAX_PENDING_MEDIA_EVENTS: usize = 8;
/// One slot for the frame being encoded and one queued behind it keeps the encoder from waiting on
/// the coordinator between frames.
const MIN_ENCODE_CHANNEL_DEPTH: usize = 2;
/// Beyond this the channel holds more frames than any measured bubble consumes, each a full packed
/// frame of memory.
const MAX_ENCODE_CHANNEL_DEPTH: usize = 8;
const DEFAULT_AUDIO_WINDOW_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_DECODE_QUEUE_BYTES: u64 = 256 * 1024 * 1024;
/// Two sources at their own decoded-frame budget ([`alpha2_extensions::DEFAULT_FRAME_BUDGET`]).
const DEFAULT_PROVIDER_SOURCE_BYTES: u64 = 512 * 1024 * 1024;
const DEFAULT_DECODE_LOOKAHEAD: usize = 2;
const MAX_DECODE_LOOKAHEAD: usize = 3;
const DEFAULT_DECODE_CONCURRENCY: usize = 2;
const MAX_DECODE_CONCURRENCY: usize = 4;
/// The floor however few videos an export reads at once.
const DEFAULT_DECODER_SESSIONS: usize = 8;
/// Only measured with Vulkan Video on a discrete GPU (32 4K hardware sessions); `VideoToolbox` and
/// D3D12VA share the cap unmeasured, and software sessions hit memory limits first.
const MAX_DECODER_SESSIONS: usize = 32;
/// By [`config::decoder_session_bytes`]'s estimate: 32 1080p or 12 4K software sessions at the
/// default decoder threads.
const DEFAULT_DECODER_SESSION_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const DEFAULT_REVERSE_CACHE_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_REVERSE_HARDWARE_SURFACES: usize = 8;
/// All decode sessions together (`ALPHA2_DECODE_SURFACES`).
#[cfg_attr(
    not(all(
        any(target_os = "linux", target_os = "macos", target_os = "windows"),
        feature = "hwdecode"
    )),
    allow(dead_code)
)]
const DEFAULT_DECODE_SURFACES: u32 = 32;
/// The same ceiling in bytes (`ALPHA2_DECODE_SURFACE_BYTES`).
#[cfg_attr(
    not(all(
        any(target_os = "linux", target_os = "macos", target_os = "windows"),
        feature = "hwdecode"
    )),
    allow(dead_code)
)]
const DEFAULT_DECODE_SURFACE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_REVERSE_HARDWARE_SURFACES: usize = 16;
const WORKER_RETRY_INTERVAL: Duration = Duration::from_millis(1);

/// What Linux and macOS give the main thread; Windows gives it an eighth of this.
const COMMAND_STACK_BYTES: usize = 8 << 20;

fn main() -> ExitCode {
    alpha2_os::stop_std_handle_inheritance();
    match on_command_thread(|| {
        let mut host = None;
        let result = run(&mut host);
        conclude(result, host)
    }) {
        Ok(code) => ExitCode::from(code),
        // The panic hook has printed the panic.
        Err(_) => ExitCode::from(101),
    }
}

/// Runs `command` on the `alpha2-main` thread with [`COMMAND_STACK_BYTES`] of stack on every OS.
///
/// # Errors
/// The thread's panic, or the error of a thread that could not be started.
fn on_command_thread<T: Send + 'static>(
    command: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Box<dyn std::any::Any + Send>> {
    std::thread::Builder::new()
        .name("alpha2-main".to_owned())
        .stack_size(COMMAND_STACK_BYTES)
        .spawn(command)
        .map_err(|error| Box::new(error) as Box<dyn std::any::Any + Send>)?
        .join()
}

/// Reports the outcome before releasing `host`, because the release can hang on a device that
/// never goes idle; it is bounded ([`teardown`]).
fn conclude(result: Result<(), HostError>, host: Option<Host>) -> u8 {
    let code = match result {
        Ok(()) => 0,
        Err(error) => {
            alpha2_os::diagnostic!("alpha2-host: {}", error.message);
            if let Some(details) = &error.details {
                alpha2_os::diagnostic!("{details}");
            }
            if error.code == "GPU_INIT_FAILED" {
                GPU_INIT_EXIT_CODE
            } else {
                1
            }
        }
    };
    let code = termination::requested().map_or(code, alpha2_os::Termination::exit_code);
    exit_if_worker_threads_abandoned(code);
    teardown::release(host, code);
    code
}

/// Exits without exit handlers when a worker is stuck inside `FFmpeg`: such threads may be inside
/// the GPU driver, whose exit handlers would wait for them and hang after the failure was reported.
fn exit_if_worker_threads_abandoned(code: u8) {
    if !heartbeat::worker_threads_abandoned() {
        return;
    }
    exit_now(code);
}

/// The device was found unable to run work (lost, or completing work without running it), or
/// `wgpu` reported it lost.
///
/// A macOS process that hung the GPU twice has its submissions ignored for good, which the
/// renderer only learns from a missing readback canary; later exports would fail or write
/// unrendered frames. A `wgpu` loss reaches the renderer only at its next submission. Either way
/// a new process is needed.
fn gpu_unusable(host: &HostDevice) -> Option<String> {
    if let Some(lost) = host.renderer.gpu().and_then(Renderer::gpu_lost) {
        return Some(lost.to_string());
    }
    host.gpu_log
        .device_lost()
        .then(|| "the GPU device was lost".to_owned())
}

/// A host with stuck workers or an unusable device cannot take another request.
fn unusable_reason(host: Option<&HostDevice>) -> Option<DrainReason> {
    if heartbeat::worker_threads_abandoned() {
        return Some(DrainReason::ThreadsAbandoned);
    }
    let reason = gpu_unusable(host?)?;
    alpha2_os::diagnostic!(
        "alpha2-host: {reason}; retiring so that the next request gets a new process"
    );
    Some(DrainReason::DeviceLost)
}

/// Exits without handlers once a host that cannot take another request has answered.
fn exit_if_retired(served: &Result<Served, HostError>) {
    if let Ok(Served::Drained(reason)) = served
        && reason.exits_at_once()
    {
        exit_now(termination::requested().map_or(1, alpha2_os::Termination::exit_code));
    }
}

/// Exits without exit handlers, which can wait on threads inside the GPU driver.
fn exit_now(code: u8) -> ! {
    let _ = std::io::stdout().flush();
    exit_unflushed(code)
}

/// For a thread that must not wait for another's line; every line was flushed whole when written
/// ([`Responder::line`]).
fn exit_unflushed(code: u8) -> ! {
    let _ = std::io::stderr().flush();
    #[cfg(feature = "hwdevice")]
    alpha2_os::exit_without_handlers(code);
    // Without a hardware device no thread is inside a GPU driver, and the handlers can run.
    #[cfg(not(feature = "hwdevice"))]
    std::process::exit(i32::from(code));
}

/// Leaves the GPU host it made, if any, in `host` for [`conclude`] to release.
fn run(host: &mut Option<Host>) -> Result<(), HostError> {
    #[cfg(windows)]
    configure_dx12_compiler();
    let mut args = utf8_arguments(env::args_os().skip(1))?.into_iter();
    match args.next().as_deref() {
        Some("serve") => {
            let options = ServeOptions::from_args(args)
                .map_err(|error| HostError::invalid_input(format!("{error}\n{}", usage())))?;
            serve(host, options)
        }
        Some("export") => export_cli(args.collect(), host),
        Some("conform") => conform_cli(args.collect()),
        #[cfg(all(target_os = "linux", feature = "hwencode"))]
        Some("probe-gpu-encode") => probe_gpu_encode(args.collect()),
        #[cfg(all(target_os = "macos", feature = "hwencode"))]
        Some("probe-gpu-encode") => probe_metal_gpu_encode(args.collect()),
        #[cfg(all(target_os = "windows", feature = "hwencode"))]
        Some("probe-gpu-encode") => crate::gpu_encode_probe::probe(args.collect()),
        // Hidden: the host runs it as its own child, so a driver that crashes in a test encode
        // takes the child with it, not the host.
        #[cfg(all(target_os = "windows", feature = "hwencode"))]
        Some(crate::d3d12va_probe::SUBCOMMAND) => {
            crate::d3d12va_probe::child(&args.collect::<Vec<_>>())
        }
        _ => Err(HostError::invalid_input(usage())),
    }
}

/// `INVALID_INPUT` names the first argument that is not UTF-8.
fn utf8_arguments(
    arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<Vec<String>, HostError> {
    alpha2_os::utf8_arguments(arguments).map_err(HostError::invalid_input)
}

#[cfg(all(target_os = "linux", feature = "hwencode"))]
fn probe_gpu_encode(args: Vec<String>) -> Result<(), HostError> {
    let mut args = args.into_iter();
    let width = args
        .next()
        .map_or(Ok(1_920), |value| value.parse::<u32>())
        .map_err(|_| HostError::invalid_input("width must be a positive integer"))?;
    let height = args
        .next()
        .map_or(Ok(1_080), |value| value.parse::<u32>())
        .map_err(|_| HostError::invalid_input("height must be a positive integer"))?;
    let slice_path = args.next();
    if args.next().is_some() {
        return Err(HostError::invalid_input(usage()));
    }
    let shared = alpha2_native_gpu::SharedVulkanContext::create()
        .map_err(|error| HostError::new("GPU_ERROR", format!("shared Vulkan device: {error}")))?;
    alpha2_os::diagnostic!(
        "alpha2-host shared Vulkan: decode=[{}] encode=[{}]",
        shared.decode_codecs().join(", "),
        shared.encode_codecs().join(", ")
    );
    // The gate an export applies: a validation layer that crashes the encoder would crash this
    // probe too.
    if let Err(reason) = crate::config::configured_gpu_encode(shared.video_session_hazard()) {
        let error = HostError::new("GPU_OUTPUT_UNAVAILABLE", reason);
        write_stdout(&json!({ "ok": false, "error": error }))?;
        return Err(error);
    }
    let probe = alpha2_native::probe_vulkan_encode(&shared.ffmpeg_device(), width, height)
        .map_err(|error| HostError::new("ENCODE_ERROR", format!("{error}")))?;
    let rendered = slice_path
        .map(|path| {
            render_gpu_encode_slice(
                &shared.gpu(),
                &alpha2_native::HardwareEncodeDevice::vulkan(shared.clone()),
                width,
                height,
                &path,
            )
        })
        .transpose()?;
    write_stdout(&json!({
        "ok": true,
        "result": {
            "encoder": probe.encoder,
            "codedSize": [probe.coded_size.0, probe.coded_size.1],
            "framesUsage": format!("0x{:04x}", probe.frames_usage),
            "imagesPerFrame": probe.images_per_frame,
            "initialLayout": probe.initial_layout,
            "queueFamily": probe.queue_family,
            "hasTimelineSemaphore": probe.has_timeline_semaphore,
            "packets": probe.packets,
            "packetBytes": probe.packet_bytes,
            "rendered": rendered,
        },
    }))
}

/// Renders one solid frame, hands it to an encoder surface eight times through the hardware-encode
/// contract, and writes an MP4.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode"
))]
fn render_gpu_encode_slice(
    gpu: &alpha2_gpu::GpuContext,
    device: &alpha2_native::HardwareEncodeDevice,
    width: u32,
    height: u32,
    path: &str,
) -> Result<Value, HostError> {
    use alpha2_render::{Nv12PlaneTargets, RenderSize, Renderer};

    let encode_error =
        |error: alpha2_media::EncodeError| HostError::new("ENCODE_ERROR", format!("{error}"));
    let renderer = Renderer::new(gpu.clone());
    let size = RenderSize::new(width, height)
        .map_err(|error| HostError::new("INVALID_INPUT", format!("{error:?}")))?;
    let color = alpha2_render::Color::rgba(0.25, 0.5, 0.75, 1.0);
    let target = renderer
        .render_solid(size, color)
        .map_err(|error| HostError::new("GPU_ERROR", format!("{error:?}")))?;
    let rate = alpha2_time::FrameRate::new(30, 1)
        .map_err(|error| HostError::new("INVALID_INPUT", format!("{error:?}")))?;
    let codec = device
        .preferred_capability()
        .map_err(|error| HostError::new("ENCODE_ERROR", error))?
        .codec;
    let (mut session, mut input) = device
        .open(&alpha2_native::HardwareEncodeRequest {
            width,
            height,
            codec,
            frame_rate: rate,
            rate_control: alpha2_media::VideoRateControl {
                bitrate: 12_000_000,
                mode: alpha2_media::BitrateMode::Variable,
                quantizer: None,
            },
            color: alpha2_media::ColorMetadata::EXPORT,
            output: &alpha2_native::OutputTarget::from(std::path::Path::new(path)),
            container: alpha2_media::Container::Mp4,
            audio: None,
        })
        .map_err(|error| HostError::new("ENCODE_ERROR", error.to_string()))?;

    let mut frames = 0_u32;
    for index in 0..8_u32 {
        let mut surface = input.acquire_surface().map_err(encode_error)?;
        let frame = input.begin_frame(&surface).map_err(encode_error)?;
        let (luma, chroma) = input.planes(&frame);
        let mut encoder = gpu
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("alpha2 NV12 pack"),
            });
        let pack = renderer
            .encode_nv12_pack(
                &target,
                Nv12PlaneTargets { luma, chroma },
                alpha2_media::YuvEncoding::export(codec.model()),
                &mut encoder,
            )
            .map_err(|error| HostError::new("GPU_ERROR", format!("{error:?}")))?;
        // Submit through the shared context so the queue lock FFmpeg also takes is held.
        gpu.submit([encoder.finish()]);
        drop(pack);
        input.submit(frame, &mut surface).map_err(encode_error)?;
        let time = rate
            .frame_start(i64::from(index))
            .ok_or_else(|| HostError::new("INVALID_INPUT", "frame time overflowed"))?;
        input.set_time(&mut surface, time);
        session.encode(&surface).map_err(encode_error)?;
        frames += 1;
    }
    session
        .finish(&alpha2_media::CancelToken::default())
        .map_err(encode_error)?;
    input.finish().map_err(encode_error)?;
    Ok(json!({
        "path": path,
        "frames": frames,
        "solidRgba": [0.25, 0.5, 0.75, 1.0],
    }))
}

/// Whether `h264_videotoolbox` opens at a size and encodes a pool frame the renderer can draw
/// into. With an output path it also writes an eight-frame MP4.
#[cfg(all(target_os = "macos", feature = "hwencode"))]
fn probe_metal_gpu_encode(args: Vec<String>) -> Result<(), HostError> {
    let mut args = args.into_iter();
    let width = args
        .next()
        .map_or(Ok(1_920), |value| value.parse::<u32>())
        .map_err(|_| HostError::invalid_input("width must be a positive integer"))?;
    let height = args
        .next()
        .map_or(Ok(1_080), |value| value.parse::<u32>())
        .map_err(|_| HostError::invalid_input("height must be a positive integer"))?;
    let slice_path = args.next();
    if args.next().is_some() {
        return Err(HostError::invalid_input(usage()));
    }
    let gpu = pollster::block_on(alpha2_gpu::GpuContext::request_product())
        .map_err(|error| HostError::new("GPU_INIT_FAILED", error_message(error)))?;
    let shared = alpha2_native_gpu::SharedMetalContext::from_gpu(gpu)
        .map_err(|error| HostError::new("GPU_ERROR", format!("shared Metal device: {error}")))?;
    let probe = alpha2_native::probe_videotoolbox_encode(&shared, width, height)
        .map_err(|error| HostError::new("ENCODE_ERROR", format!("{error}")))?;
    let rendered = slice_path
        .map(|path| {
            render_gpu_encode_slice(
                &shared.gpu(),
                &alpha2_native::HardwareEncodeDevice::metal(shared.clone()),
                width,
                height,
                &path,
            )
        })
        .transpose()?;
    write_stdout(&json!({
        "ok": true,
        "result": {
            "encoder": probe.encoder,
            "planeSizes": probe.plane_sizes,
            "packets": probe.packets,
            "packetBytes": probe.packet_bytes,
            "rendered": rendered,
        },
    }))
}

/// DX12 compiles with DXC: `ALPHA2_DXC_PATH`, else the `dxcompiler.dll` a release ships beside the
/// executable. A DLL that does not load refuses the host; it never falls back to FXC (whose first
/// frame costs seconds) or another backend.
#[cfg(windows)]
fn configure_dx12_compiler() {
    let library = env::var_os("ALPHA2_DXC_PATH")
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|dir| dir.join("dxcompiler.dll")))
        });
    if let Some(library) = library {
        // Nothing has made an instance yet, so the settings cannot be frozen.
        let _ = alpha2_gpu::configure_instances(alpha2_gpu::InstanceSettings {
            dx12_compiler: alpha2_gpu::Dx12Compiler::Dxc { library },
        });
    }
}

fn create_host(policy: VideoDecodePolicy) -> Result<Host, HostError> {
    let renderer = crate::config::RendererPolicy::from_env().map_err(HostError::invalid_input)?;
    create_host_with(policy, renderer)
}

/// `auto` takes a hardware GPU, else the CPU renderer, never a software adapter, which renders the
/// same pictures an order of magnitude slower.
pub(crate) fn create_host_with(
    policy: VideoDecodePolicy,
    renderer: crate::config::RendererPolicy,
) -> Result<Host, HostError> {
    create_host_on(
        policy,
        renderer,
        &crate::adapter_choice::AdapterPolicy::from_env(),
    )
}

pub(crate) fn create_host_on(
    policy: VideoDecodePolicy,
    renderer: crate::config::RendererPolicy,
    adapter_policy: &crate::adapter_choice::AdapterPolicy,
) -> Result<Host, HostError> {
    use crate::config::RendererPolicy;
    use alpha2_gpu::SoftwareAdapters;
    let create_gpu_host =
        |policy, software| create_gpu_host_with_adapter(policy, adapter_policy, software);
    match renderer {
        RendererPolicy::Gpu => create_gpu_host(policy, SoftwareAdapters::Allow),
        RendererPolicy::Cpu => create_cpu_host(policy, "requested (--renderer cpu)".to_owned()),
        RendererPolicy::Auto => match create_gpu_host(policy, SoftwareAdapters::Refuse) {
            Ok(host) => {
                let software = host
                    .renderer
                    .gpu()
                    .filter(|renderer| renderer.gpu_capabilities().software_rasterizer)
                    .map(|renderer| renderer.gpu_capabilities().adapter_name.clone());
                match software {
                    None => Ok(host),
                    Some(adapter) => {
                        drop(host);
                        create_cpu_host(
                            policy,
                            format!("the GPU host started on the software adapter {adapter}"),
                        )
                    }
                }
            }
            // A missing shader compiler is a broken installation, not a machine without a
            // GPU: rendering on the CPU would hide it.
            Err(error)
                if error.code == "GPU_INIT_FAILED"
                    && error
                        .details
                        .as_ref()
                        .is_none_or(|details| details["cause"] != SHADER_COMPILER_CAUSE) =>
            {
                create_cpu_host(policy, format!("no GPU host: {}", error.message))
            }
            Err(error) => Err(error),
        },
    }
}

fn create_cpu_host(policy: VideoDecodePolicy, reason: String) -> Result<Host, HostError> {
    if policy.requires_hardware() {
        return Err(HostError::new(
            "HARDWARE_DECODE_UNAVAILABLE",
            format!("hardware decode is required, but this host renders on the CPU ({reason})"),
        ));
    }
    let renderer = alpha2_render::CpuRenderer::new(crate::config::configured_render_threads());
    alpha2_os::diagnostic!(
        "alpha2-host renderer: CPU, {} threads ({reason}), OS {}",
        renderer.threads(),
        alpha2_os::os_version()
    );
    log_runtime_codecs();
    let builtins = alpha2_render::builtin_effect_definitions()
        .into_iter()
        .chain(alpha2_render::builtin_transition_definitions())
        .filter(|definition| {
            alpha2_render::EffectCatalog::project_effect_id(
                &renderer,
                &definition.id,
                definition.version,
            )
            .is_some()
        })
        .map(|definition| (definition.id, definition.version))
        .collect::<HashSet<_>>();
    let host = Host::new_cpu(renderer, builtins, reason);
    if let Err(reason) = crate::host::host_binary_digest() {
        alpha2_os::diagnostic!("alpha2-host render cache unavailable: {reason}");
    }
    Ok(host)
}

/// A pin naming no adapter is the request's error; a DX12 backend whose shader compiler did not
/// load is an installation defect no renderer policy works around (`details.cause` is
/// `shader-compiler`); anything else is the GPU's.
fn gpu_init_error(error: alpha2_gpu::GpuError) -> HostError {
    match error.step() {
        alpha2_gpu::GpuAcquisitionStep::Pin => HostError::invalid_input(error.to_string()),
        alpha2_gpu::GpuAcquisitionStep::Instance => HostError::new(
            "GPU_INIT_FAILED",
            format!(
                "the DX12 backend did not start: shader compiler {} (set ALPHA2_DXC_PATH to the \
                 dxcompiler.dll of scripts/fetch-dxc.sh, or keep it beside alpha2-host.exe)",
                alpha2_gpu::dx12_shader_compiler()
            ),
        )
        .with_details(json!({ "cause": SHADER_COMPILER_CAUSE })),
        _ => HostError::new("GPU_INIT_FAILED", error_message(error)),
    }
}

const SHADER_COMPILER_CAUSE: &str = "shader-compiler";

/// Picks among the DX12 adapters on Windows, the Vulkan adapters that can carry the shared video
/// device on Linux, and the one Metal device on macOS ([`alpha2_gpu::rank_adapters`]). `software`
/// says whether a software adapter is acceptable.
#[allow(clippy::too_many_lines)]
pub(crate) fn create_gpu_host_with_adapter(
    policy: VideoDecodePolicy,
    adapter_policy: &crate::adapter_choice::AdapterPolicy,
    software: alpha2_gpu::SoftwareAdapters,
) -> Result<Host, HostError> {
    // The Windows scan opens a software adapter only when nothing else renders; `auto` then
    // lets that host go for the CPU renderer below.
    #[cfg(all(target_os = "windows", feature = "hwdevice"))]
    let _ = software;
    // Without hardware decode, `hardware-required` would decode every frame in software under the
    // mode's name.
    if policy.requires_hardware()
        && !cfg!(all(
            any(
                target_os = "linux",
                target_os = "macos",
                target_os = "windows"
            ),
            feature = "hwdecode"
        ))
    {
        return Err(HostError::new(
            "HARDWARE_DECODE_UNAVAILABLE",
            "hardware decode is required, but this host was built without hardware decode",
        ));
    }
    // Windows renders on the scan's DX12 device or refuses to start; it never asks for another.
    #[cfg(not(all(target_os = "windows", feature = "hwdevice")))]
    let request_default = || {
        pollster::block_on(alpha2_gpu::GpuContext::request_product_with(
            &alpha2_gpu::ProductRequest {
                software,
                ..alpha2_gpu::ProductRequest::default()
            },
        ))
        .map_err(gpu_init_error)
    };
    // The renderer goes on the shared Vulkan device, so a non-Vulkan backend skips it.
    #[cfg(all(target_os = "linux", feature = "hwdevice"))]
    let excluded = backend_excluded(wgpu::Backends::from_env(), wgpu::Backends::VULKAN, "Vulkan");
    #[cfg(all(target_os = "linux", feature = "hwdevice"))]
    let (mut adapter_choice, mut no_shared_device) = (None, excluded.clone());
    #[cfg(all(target_os = "linux", feature = "hwdevice"))]
    let (shared_vulkan, unavailable) =
        shared_device_context(policy, cfg!(feature = "hwencode"), excluded, || {
            let decode = cfg!(feature = "hwdecode") && policy.allows_hardware();
            let choice = alpha2_native_gpu::choose_shared_vulkan(adapter_policy, decode, true)
                .inspect_err(|error| no_shared_device = Some(error.to_string()))?;
            no_shared_device.clone_from(&choice.unavailable);
            adapter_choice = Some(choice.report);
            choice.context.ok_or_else(|| {
                alpha2_native_gpu::SharedVulkanError::new(
                    no_shared_device.clone().unwrap_or_else(|| {
                        "no Vulkan adapter carries the shared device".to_owned()
                    }),
                )
            })
        })?;
    // A device `FFmpeg` cannot open a video session on still renders, and its frames decode in
    // software: `auto` says why, `hardware-required` refuses to start.
    #[cfg(all(target_os = "linux", feature = "hwdevice"))]
    let unavailable = video_session_decode(
        policy,
        shared_vulkan
            .as_ref()
            .and_then(alpha2_native_gpu::SharedVulkanContext::video_session_hazard),
    )?
    .or(unavailable);
    #[cfg(all(target_os = "linux", feature = "hwdevice"))]
    let gpu = if let Some(context) = &shared_vulkan {
        context.gpu()
    } else {
        request_default()?
    };
    // D3D12VA shares the renderer's device, so the renderer must be on DX12; left to itself wgpu
    // prefers Vulkan on Windows, which would silently turn hardware decode off.
    #[cfg(all(target_os = "windows", feature = "hwencode"))]
    crate::d3d12va_probe::install();
    #[cfg(all(target_os = "windows", feature = "hwdevice"))]
    let decode_on_gpu = cfg!(feature = "hwdecode") && policy.allows_hardware();
    #[cfg(all(target_os = "windows", feature = "hwdevice"))]
    let encode_on_gpu = gpu_encode_wanted();
    #[cfg(all(target_os = "windows", feature = "hwdevice"))]
    let (mut dx12, dx12_unavailable) = {
        // Another backend would give other bytes and no hardware decode, so a DX12 that does not
        // start refuses the host.
        let scanned = pollster::block_on(crate::dx12_adapters::scan(
            adapter_policy,
            decode_on_gpu,
            encode_on_gpu,
        ));
        match scanned {
            Ok(choice) => (choice, None::<String>),
            Err(crate::dx12_adapters::ScanError::Pin(reason)) => {
                return Err(HostError::invalid_input(reason));
            }
            Err(crate::dx12_adapters::ScanError::Unavailable(error)) => {
                if policy.requires_hardware() {
                    return Err(HostError::new(
                        "HARDWARE_DECODE_UNAVAILABLE",
                        format!("DX12 unavailable: {error}"),
                    ));
                }
                return Err(gpu_init_error(error));
            }
        }
    };
    #[cfg(all(target_os = "windows", feature = "hwdevice"))]
    let gpu = dx12.gpu.clone();
    #[cfg(not(any(
        all(target_os = "linux", feature = "hwdevice"),
        all(target_os = "windows", feature = "hwdevice")
    )))]
    let gpu = request_default()?;
    #[cfg(all(target_os = "macos", feature = "hwdevice"))]
    let mut no_shared_device = None;
    #[cfg(all(target_os = "macos", feature = "hwdevice"))]
    let (shared_metal, unavailable) =
        shared_device_context(policy, cfg!(feature = "hwencode"), None, || {
            alpha2_native_gpu::SharedMetalContext::from_gpu(gpu.clone())
                .inspect_err(|error| no_shared_device = Some(error.to_string()))
        })?;
    #[cfg(all(target_os = "windows", feature = "hwdevice"))]
    let (shared_d3d12, unavailable, no_encode_device) =
        d3d12_devices(policy, dx12_unavailable, &mut dx12)?;
    // Started now, so the children open their devices while this host makes its renderer.
    #[cfg(all(target_os = "windows", feature = "hwencode"))]
    let test_encodes = start_test_encodes(shared_d3d12.as_ref(), encode_on_gpu);
    // This binary owns its stderr, so it prints device events itself and keeps the latest for a
    // failure report.
    let gpu_log = std::sync::Arc::new(alpha2_gpu::GpuDiagnosticLog::default());
    let logged = std::sync::Arc::clone(&gpu_log);
    gpu.set_diagnostic_handler(move |diagnostic: GpuDiagnostic| {
        alpha2_os::diagnostic!("alpha2-host {diagnostic}");
        logged.push(diagnostic);
    });
    // Where the graphics API reports one; idle pools are held to a quarter of it.
    #[cfg(feature = "hwdevice")]
    let gpu_memory_budget = alpha2_native_gpu::adapter_memory_budget(gpu.adapter());
    #[cfg(not(feature = "hwdevice"))]
    let gpu_memory_budget = None;
    let mut renderer = Renderer::new(gpu);
    let pools_lowered = renderer.set_gpu_memory_budget(gpu_memory_budget);
    if let Some(budget) = gpu_memory_budget {
        alpha2_os::diagnostic!(
            "alpha2-host GPU memory budget: {budget} bytes{}",
            if pools_lowered {
                "; the idle texture pools are held to a quarter of it"
            } else {
                ""
            }
        );
    }
    let wait_timeout = crate::config::configured_gpu_wait_timeout();
    renderer.set_gpu_wait_timeout(wait_timeout);
    #[cfg(feature = "hwdevice")]
    alpha2_native_gpu::set_gpu_wait_timeout(wait_timeout);
    let builtin_ids = renderer.register_builtins().map_err(|diagnostics| {
        HostError::new(
            "EFFECT_ERROR",
            format!("built-in effect registration failed: {diagnostics:?}"),
        )
    })?;
    let capabilities = renderer.gpu_capabilities();
    alpha2_os::diagnostic!(
        "alpha2-host GPU: {}, tier {:?}, OS {}",
        capabilities.summary(),
        capabilities.tier,
        alpha2_os::os_version()
    );
    if capabilities.shader_compiler.starts_with("DXC ") {
        alpha2_os::diagnostic!(
            "alpha2-host shader compiler: {}",
            crate::renderer_backend::describe_shader_compiler(&capabilities.shader_compiler)
        );
    }
    log_runtime_codecs();
    let builtins = builtin_ids
        .into_iter()
        .map(|id| (id.as_str().to_owned(), id.version()))
        .collect::<HashSet<_>>();
    #[cfg(all(target_os = "linux", feature = "hwdevice"))]
    let mut host = Host::new_with_vulkan(renderer, builtins, shared_vulkan, policy);
    #[cfg(not(any(
        all(target_os = "linux", feature = "hwdevice"),
        all(target_os = "macos", feature = "hwdevice"),
        all(target_os = "windows", feature = "hwdevice")
    )))]
    let mut host = Host::new(renderer, builtins, policy);
    #[cfg(all(target_os = "macos", feature = "hwdevice"))]
    let mut host = Host::new_with_metal(renderer, builtins, shared_metal, policy);
    #[cfg(all(target_os = "windows", feature = "hwdevice"))]
    let mut host = Host::new_with_d3d12(renderer, builtins, shared_d3d12, policy);
    #[cfg(any(
        all(target_os = "linux", feature = "hwdevice"),
        all(target_os = "macos", feature = "hwdevice"),
        all(target_os = "windows", feature = "hwdevice")
    ))]
    {
        host.device_mut().hardware_decode_unavailable = unavailable;
    }
    #[cfg(all(target_os = "linux", feature = "hwdevice"))]
    let (no_encode_device, report) = (
        missing_encode_device(host.shared_vulkan.is_none(), no_shared_device.clone()),
        product_device_report(adapter_choice, adapter_policy, &host, no_shared_device),
    );
    #[cfg(all(target_os = "macos", feature = "hwdevice"))]
    let (no_encode_device, report) = (
        missing_encode_device(host.shared_metal.is_none(), no_shared_device.clone()),
        metal_adapter_report(adapter_policy, &host, no_shared_device),
    );
    #[cfg(all(target_os = "windows", feature = "hwdevice"))]
    let report = windows_default_note(
        product_device_report(dx12.report.take(), adapter_policy, &host, None),
        decode_on_gpu,
    );
    #[cfg(not(any(
        all(target_os = "linux", feature = "hwdevice"),
        all(target_os = "macos", feature = "hwdevice"),
        all(target_os = "windows", feature = "hwdevice")
    )))]
    let (no_encode_device, report) = (
        None,
        product_device_report(None, adapter_policy, &host, None),
    );
    host.device_mut().encode_adapters =
        crate::adapter_choice::EncodeAdapterNotes::new(no_encode_device, Some(&report));
    host.device_mut().adapter_choice = Some(report);
    #[cfg(all(target_os = "windows", feature = "hwencode"))]
    if let Some(test_encodes) = test_encodes {
        test_encodes.finish();
    }
    #[cfg(all(target_os = "windows", feature = "hwdevice"))]
    report_dx12_encode(&host, encode_on_gpu);
    report_adapter_choice(&host);
    host.device_mut().gpu_log = gpu_log;
    // Read now, before an upgrade can replace the file this process runs.
    if let Err(reason) = crate::host::host_binary_digest() {
        alpha2_os::diagnostic!("alpha2-host render cache unavailable: {reason}");
    }
    Ok(host)
}

fn log_runtime_codecs() {
    // Encoders that must be opened to prove they run here (NVENC, Media Foundation,
    // VideoToolbox) are opened when an export or `capabilities` first asks.
    let (video_encoders, unprobed) = alpha2_native::video_encoders_without_probing();
    let audio_encoders = alpha2_native::audio_encoder_capabilities();
    let ffmpeg = ffmpeg_runtime_identity();
    match ffmpeg.pinned_sdk_mismatch() {
        None => alpha2_os::diagnostic!(
            "alpha2-host FFmpeg: {}, pinned SDK {}",
            ffmpeg.version,
            ffmpeg.pinned_sdk_release
        ),
        Some(reason) => alpha2_os::diagnostic!(
            "alpha2-host FFmpeg: {}, WARNING: not the pinned SDK {} ({reason}); this build \
             linked whatever FFmpeg pkg-config found, so its encoders and exports differ from \
             a release.",
            ffmpeg.version,
            ffmpeg.pinned_sdk_release
        ),
    }
    alpha2_os::diagnostic!(
        "alpha2-host MP4 encoders: video=[{}] audio=[{}]{}",
        video_encoders
            .iter()
            .map(|capability| format!(
                "{}:{}{}",
                capability.codec.as_str(),
                capability.encoder_name,
                if capability.hardware_accelerated {
                    " (hw)"
                } else {
                    ""
                }
            ))
            .collect::<Vec<_>>()
            .join(", "),
        audio_encoders
            .iter()
            .map(|capability| format!("{}:{}", capability.codec.as_str(), capability.encoder_name))
            .collect::<Vec<_>>()
            .join(", "),
        if unprobed.is_empty() {
            String::new()
        } else {
            format!(" probed when first used=[{}]", unprobed.join(", "))
        }
    );
    alpha2_os::diagnostic!(
        "alpha2-host {}",
        openh264_startup_status(alpha2_native::openh264_status())
    );
}

/// An H.264 export without a hardware encoder needs `OpenH264`, so the line says what to do when
/// it is missing.
fn openh264_startup_status(status: &alpha2_native::OpenH264Status) -> String {
    let detail = match status {
        alpha2_native::OpenH264Status::Available(library) => format!(
            "available ({}, {})",
            library.provider().as_str(),
            library.path()
        ),
        alpha2_native::OpenH264Status::Disabled => "disabled (ALPHA2_OPENH264=0)".to_owned(),
        alpha2_native::OpenH264Status::NotInstalled(_) => format!(
            "not installed ({})",
            alpha2_native::OPENH264_INSTALL_COMMAND
        ),
        alpha2_native::OpenH264Status::Unusable(reason) => format!("unusable ({reason})"),
        _ => status.as_str().to_owned(),
    };
    format!("openh264={detail}")
}

/// Only then is a device made for encode alone, and a test encode run.
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
fn gpu_encode_wanted() -> bool {
    #[cfg(feature = "hwencode")]
    return crate::config::configured_gpu_encode(None).is_ok();
    #[cfg(not(feature = "hwencode"))]
    false
}

#[cfg(all(target_os = "windows", feature = "hwdevice"))]
type D3D12Devices = (
    Option<alpha2_native_gpu::SharedD3D12Context>,
    Option<String>,
    Option<String>,
);

/// For decode, `auto` reports why the adapter does not decode and continues in software, while
/// `hardware-required` refuses to start. A device only encode wanted is best-effort. A device made
/// for encode under `software` is kept, and decode does not use it ([`Host::decode_d3d12`]).
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
fn d3d12_devices(
    policy: VideoDecodePolicy,
    dx12_unavailable: Option<String>,
    dx12: &mut crate::dx12_adapters::Dx12Choice,
) -> Result<D3D12Devices, HostError> {
    if let Some(reason) = dx12_unavailable {
        return Ok((None, Some(reason.clone()), Some(reason)));
    }
    let decode_unavailable = match dx12.decode_refused.take() {
        None => None,
        Some(reason) if policy.requires_hardware() => {
            return Err(HostError::new("HARDWARE_DECODE_UNAVAILABLE", reason));
        }
        Some(reason) => {
            alpha2_os::diagnostic!(
                "alpha2-host hardware decode unavailable, using software: {reason}"
            );
            Some(reason)
        }
    };
    let (shared, no_encode_device) = match dx12.shared.take() {
        None => (None, None),
        Some(Ok(context)) => (Some(context), None),
        Some(Err(reason)) => {
            let adapter = dx12.gpu.capabilities().adapter_name.clone();
            if decode_unavailable.is_none() {
                alpha2_os::diagnostic!(
                    "alpha2-host shared GPU device unavailable, exports take the readback path: \
                     {reason}"
                );
            }
            (
                None,
                Some(format!(
                    "{adapter} has no D3D12 device shared with a hardware encoder: {reason}"
                )),
            )
        }
    };
    Ok((shared, decode_unavailable, no_encode_device))
}

/// Runs in child processes in parallel; what an export is offered, and every later question, is
/// answered from them.
#[cfg(all(target_os = "windows", feature = "hwencode"))]
fn start_test_encodes(
    shared: Option<&alpha2_native_gpu::SharedD3D12Context>,
    encode_on_gpu: bool,
) -> Option<crate::d3d12va_probe::Prewarm> {
    let shared = shared.filter(|_| encode_on_gpu)?;
    #[cfg(test)]
    if crate::d3d12va_probe::injected() {
        crate::d3d12va_probe::prewarm_injected(
            &shared.gpu().capabilities().adapter_name,
            shared.adapter().map_or(0, |identity| identity.luid),
        );
        return None;
    }
    let codecs: Vec<_> = crate::adapter_choice::ENCODE_CODECS
        .into_iter()
        .filter(|codec| crate::d3d12va_probe::device_encodes(shared, *codec).is_ok())
        .collect();
    Some(crate::d3d12va_probe::start_prewarm(
        shared,
        &codecs,
        crate::config::configured_gpu_wait_timeout(),
    ))
}

#[cfg(all(target_os = "windows", feature = "hwdevice"))]
#[cfg_attr(not(feature = "hwencode"), allow(unused_variables))]
fn report_dx12_encode(host: &Host, encode_on_gpu: bool) {
    #[cfg(feature = "hwencode")]
    let encode = if encode_on_gpu {
        gpu_encode_diagnostic(
            &host.renderer.adapter().name,
            &host.gpu_output_codecs(),
            host.gpu_output_capability().err(),
        )
    } else {
        "off (ALPHA2_GPU_ENCODE is off)".to_owned()
    };
    #[cfg(not(feature = "hwencode"))]
    let encode = "unavailable: this host was built without hardware encode".to_owned();
    alpha2_os::diagnostic!("alpha2-host GPU encode: {encode}");
}

fn report_adapter_choice(host: &Host) {
    let Some(report) = &host.adapter_choice else {
        return;
    };
    let adapter = host.renderer.adapter();
    alpha2_os::diagnostic!(
        "alpha2-host GPU: {} ({}), {}: {}",
        adapter.name,
        adapter.backend,
        report.policy.setting(),
        report.explanation
    );
    for (at, record) in report.records.iter().enumerate() {
        if Some(at) != report.chosen {
            alpha2_os::diagnostic!("alpha2-host GPU passed over: {}", record.summary());
        }
    }
}

#[cfg(all(any(target_os = "linux", target_os = "macos"), feature = "hwdevice"))]
fn missing_encode_device(missing: bool, reason: Option<String>) -> Option<String> {
    (missing && cfg!(feature = "hwencode"))
        .then(|| reason.unwrap_or_else(|| "no shared device was made".to_owned()))
}

/// Without a shared device the renderer is the product device (`ALPHA2_GPU_ADAPTER`'s pin, or the
/// high-performance order), which the report then names, with `why`.
#[cfg(not(all(target_os = "macos", feature = "hwdevice")))]
fn product_device_report(
    report: Option<crate::adapter_choice::AdapterChoiceReport>,
    policy: &crate::adapter_choice::AdapterPolicy,
    host: &Host,
    why: Option<String>,
) -> crate::adapter_choice::AdapterChoiceReport {
    use alpha2_gpu::ChoiceReason;
    let name = host.renderer.adapter().name;
    let mut report = report.unwrap_or_else(|| crate::adapter_choice::AdapterChoiceReport {
        policy: policy.clone(),
        chosen: None,
        reason: ChoiceReason::NoSharedDevice,
        explanation: String::new(),
        records: Vec::new(),
    });
    if report
        .chosen_record()
        .is_some_and(|record| record.name == name)
    {
        return report;
    }
    report.chosen = report.records.iter().position(|record| record.name == name);
    report.reason = if policy.is_pin() {
        ChoiceReason::Pinned
    } else {
        ChoiceReason::NoSharedDevice
    };
    let why = why
        .or_else(|| (!report.explanation.is_empty()).then(|| report.explanation.clone()))
        .unwrap_or_else(|| "no shared device was wanted".to_owned());
    report.explanation = format!("{name} renders as the product device: {why}");
    report
}

/// When `ALPHA2_GPU_ADAPTER` was unset, notes that Windows used the high-performance order and
/// where `auto` would have rendered, so the user learns how to ask for it.
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
fn windows_default_note(
    mut report: crate::adapter_choice::AdapterChoiceReport,
    decode_allowed: bool,
) -> crate::adapter_choice::AdapterChoiceReport {
    use crate::adapter_choice::AdapterPolicy;
    if report.policy != AdapterPolicy::Performance || AdapterPolicy::set_in_env().is_some() {
        return report;
    }
    let auto = alpha2_gpu::rank_adapters(&report.records, &AdapterPolicy::Auto, decode_allowed)
        .ok()
        .and_then(|order| order.order.first().copied())
        .filter(|&at| Some(at) != report.chosen);
    let hint = auto.map_or_else(String::new, |at| {
        format!(
            "; ALPHA2_GPU_ADAPTER=auto would render on {} ({})",
            report.records[at].name,
            report.records[at].summary()
        )
    });
    report.explanation = format!(
        "{}; ALPHA2_GPU_ADAPTER is unset, and Windows renders by the high-performance order \
         because some Intel integrated GPUs draw leftover memory into DX12 textures with this \
         build's wgpu-hal{hint}",
        report.explanation
    );
    report
}

#[cfg(all(target_os = "macos", feature = "hwdevice"))]
fn metal_adapter_report(
    policy: &crate::adapter_choice::AdapterPolicy,
    host: &Host,
    why: Option<String>,
) -> crate::adapter_choice::AdapterChoiceReport {
    let adapter = host.renderer.adapter();
    let shared = match (&host.shared_metal, why) {
        (Some(_), _) => Ok(()),
        (None, Some(why)) => Err(why),
        (None, None) => Err("no shared device was wanted".to_owned()),
    };
    let mut record = alpha2_gpu::AdapterRecord::unasked(0, adapter.name.clone(), "metal", false);
    record.renders = Ok(());
    record.decodes = match &shared {
        Ok(()) if host.decode_policy.allows_hardware() => Ok(()),
        Ok(()) => Err("decode is in software".to_owned()),
        Err(why) => Err(why.clone()),
    };
    record.gpu_encodes = alpha2_gpu::GPU_ENCODE_CODECS
        .iter()
        .map(|&codec| {
            let answer = match &shared {
                Err(why) => Err(why.clone()),
                Ok(()) if !cfg!(feature = "hwencode") => {
                    Err("this host was built without hardware encode".to_owned())
                }
                Ok(()) if codec == "h264" => Ok(()),
                Ok(()) => Err(format!(
                    "the GPU-resident path encodes h264 on macOS, not {codec}"
                )),
            };
            (codec, answer)
        })
        .collect();
    let records = vec![record];
    let order = alpha2_gpu::rank_adapters(&records, policy, host.decode_policy.allows_hardware())
        .unwrap_or(alpha2_gpu::AdapterOrder {
            order: vec![0],
            reason: alpha2_gpu::ChoiceReason::OnlyAdapter,
        });
    crate::adapter_choice::AdapterChoiceReport::ranked(policy.clone(), records, &order)
}

#[cfg(all(any(test, target_os = "windows"), feature = "hwencode"))]
fn gpu_encode_diagnostic(
    adapter: &str,
    codecs: &[alpha2_native::HardwareEncodeCapability],
    unavailable: Option<String>,
) -> String {
    match codecs.first() {
        Some(first) => format!(
            "{} {} on {adapter}",
            first.backend.as_str(),
            codecs
                .iter()
                .map(|capability| capability.encoder)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        None => format!(
            "unavailable: {}",
            unavailable.unwrap_or_else(|| "no encoder takes GPU frames here".to_owned())
        ),
    }
}

/// The shared device is the renderer's, so making it when `WGPU_BACKEND` excludes its backend
/// would ignore the explicit choice.
#[cfg(any(test, all(target_os = "linux", feature = "hwdevice")))]
fn backend_excluded(
    chosen: Option<wgpu::Backends>,
    shared: wgpu::Backends,
    name: &str,
) -> Option<String> {
    chosen
        .filter(|chosen| !chosen.contains(shared))
        .map(|chosen| format!("WGPU_BACKEND selects {chosen:?}, not {name}"))
}

/// `None` when there is no hazard or decode is in software anyway; the reason `auto` decodes in
/// software; or the `hardware-required` refusal. The device itself is kept, since it renders.
#[cfg(any(test, all(target_os = "linux", feature = "hwdevice")))]
fn video_session_decode(
    policy: VideoDecodePolicy,
    hazard: Option<String>,
) -> Result<Option<String>, HostError> {
    let Some(hazard) = hazard.filter(|_| cfg!(feature = "hwdecode") && policy.allows_hardware())
    else {
        return Ok(None);
    };
    if policy.requires_hardware() {
        return Err(HostError::new("HARDWARE_DECODE_UNAVAILABLE", hazard));
    }
    alpha2_os::diagnostic!("alpha2-host hardware decode unavailable, using software: {hazard}");
    Ok(Some(hazard))
}

/// Hardware decode wants the device when `policy` allows GPU decode, and the GPU-resident export
/// whenever `encode` says this build has one for the backend. For decode, `auto` continues in
/// software with the reason, while `hardware-required` refuses to start. A device only encode
/// wanted is best-effort. `excluded` ([`backend_excluded`]) is reported as an unavailable device.
#[cfg(any(
    test,
    all(target_os = "linux", feature = "hwdevice"),
    all(target_os = "macos", feature = "hwdevice")
))]
fn shared_device_context<T, E: std::error::Error + 'static>(
    policy: VideoDecodePolicy,
    encode: bool,
    excluded: Option<String>,
    create: impl FnOnce() -> Result<T, E>,
) -> Result<(Option<T>, Option<String>), HostError> {
    let decode = cfg!(feature = "hwdecode") && policy.allows_hardware();
    if !decode && !encode {
        return Ok((None, None));
    }
    let created = match excluded {
        Some(reason) => Err(reason),
        None => create().map_err(error_message),
    };
    match created {
        Ok(context) => Ok((Some(context), None)),
        Err(reason) if !decode => {
            alpha2_os::diagnostic!(
                "alpha2-host shared GPU device unavailable, exports take the readback path: {reason}"
            );
            Ok((None, None))
        }
        Err(reason) if policy == VideoDecodePolicy::Auto => {
            alpha2_os::diagnostic!(
                "alpha2-host hardware decode unavailable, using software: {reason}"
            );
            Ok((None, Some(reason)))
        }
        Err(reason) => Err(HostError::new("HARDWARE_DECODE_UNAVAILABLE", reason)),
    }
}

/// Without it the process ends where it stands, leaving a partial file the next export to that
/// output takes over.
fn install_termination() {
    if let Err(error) = termination::install() {
        alpha2_os::diagnostic!(
            "alpha2-host: termination requests will end the process at once: {error}"
        );
    }
}

/// Two glibc arenas keep `serve` memory flat under mixed requests. Freed memory stays in the tops
/// of per-thread arenas, which `malloc_trim` does not shrink, and codec thread pools make dozens.
/// The CPU renderer reuses its frame and band buffers, so the cap does not slow it.
const SERVE_HEAP_ARENAS: u16 = 2;

fn serve(host: &mut Option<Host>, options: ServeOptions) -> Result<(), HostError> {
    alpha2_os::limit_heap_arenas(SERVE_HEAP_ARENAS);
    let out = Arc::new(Responder::claimed_stdout());
    let lifecycle = Arc::new(Lifecycle::new(options.limits));
    lifecycle::install(&lifecycle, &out);
    install_termination();
    if let Err(error) = alpha2_os::watch_parent() {
        alpha2_os::diagnostic!(
            "alpha2-host: the exit of the process that started this host goes unnoticed: {error}"
        );
    }
    serve_io::write_ready(&out, &lifecycle)?;
    let policy = match VideoDecodePolicy::from_env() {
        Ok(policy) => policy,
        Err(message) => {
            return Err(write_fatal(HostError::invalid_input(message), &out)?);
        }
    };
    // Read now, before an upgrade can replace the file this process runs.
    if let Err(reason) = crate::host::host_binary_digest() {
        alpha2_os::diagnostic!("alpha2-host render cache unavailable: {reason}");
    }
    let requests = serve_io::start_reader(Arc::clone(&out), Arc::clone(&lifecycle))?;
    let mut serving = Serving::with_lifecycle(policy, lifecycle, options.max_running);
    serving.extract = options.extract;
    let served = serve_requests(&mut serving, &requests, &out, &unusable_reason);
    exit_if_retired(&served);
    // Released by `main` once the outcome is out.
    *host = serving.device.take().map(Host::for_request);
    served.map(|_| ())
}

fn write_fatal(error: HostError, out: &Responder) -> Result<HostError, HostError> {
    let error = fatal_error(error);
    out.send(&json!({
        "protocolVersion": PROTOCOL_VERSION,
        "type": "fatal",
        "error": error,
    }))?;
    Ok(error)
}

/// A client classifies a start failure as it would the same code from a running host.
fn fatal_error(error: HostError) -> HostError {
    let failure = crate::failure::startup_failure_json(error.code, &error.message, error.evidence);
    error.with_failure(failure)
}

/// The GPU device is made when the first request needing one arrives, so `conform` works on a
/// machine without a usable GPU (or, under `hardware-required`, a decode device).
struct Serving {
    policy: VideoDecodePolicy,
    device: Option<Arc<HostDevice>>,
    create: fn(VideoDecodePolicy) -> Result<Host, HostError>,
    lifecycle: Arc<Lifecycle>,
    /// How many requests run at once at most.
    max_running: usize,
    /// Extraction's step timeout and plugin limits.
    extract: crate::extract_request::ExtractOptions,
}

impl Serving {
    const fn with_lifecycle(
        policy: VideoDecodePolicy,
        lifecycle: Arc<Lifecycle>,
        max_running: usize,
    ) -> Self {
        Self {
            policy,
            device: None,
            create: create_host,
            lifecycle,
            max_running,
            extract: crate::extract_request::ExtractOptions::DEFAULT,
        }
    }

    /// One request at a time with every soft limit off, so answers come in request order and other
    /// tests' memory does not retire it.
    #[cfg(test)]
    fn new(policy: VideoDecodePolicy) -> Self {
        Self::with_lifecycle(policy, Arc::new(Lifecycle::new(SoftLimits::OFF)), 1)
    }

    #[cfg(test)]
    fn started(host: Host) -> Self {
        let mut serving = Self::new(host.decode_policy);
        serving.lifecycle.set_renderer(renderer_facts(&host));
        serving.device = Some(Arc::clone(host.device()));
        // The context goes; the device stays for the requests served.
        drop(host);
        serving
    }

    fn device(&mut self) -> Result<Arc<HostDevice>, HostError> {
        if let Some(device) = &self.device {
            return Ok(Arc::clone(device));
        }
        let host = (self.create)(self.policy)?;
        self.lifecycle.set_renderer(renderer_facts(&host));
        let device = Arc::clone(host.device());
        self.device = Some(Arc::clone(&device));
        Ok(device)
    }
}

fn renderer_facts(device: &HostDevice) -> lifecycle::RendererFacts {
    lifecycle::RendererFacts {
        renderer: device.renderer.name(),
        fallback: device.renderer_reason.clone(),
        hardware_decode_fallback: device.hardware_decode_unavailable.clone(),
    }
}

/// How [`serve_requests`] stopped.
#[derive(Debug, PartialEq, Eq)]
enum Served {
    InputEnded,
    Drained(DrainReason),
}

/// Whether a request that cannot run on the device it ran on retires the host.
type Unusable<'a> = dyn Fn(Option<&HostDevice>) -> Option<DrainReason> + Sync + 'a;

/// How often the dispatcher looks at the host's state while requests run.
const DISPATCH_POLL: Duration = Duration::from_millis(100);

/// How long a host that cannot take another request waits for those it runs to answer.
const RETIRE_WAIT: Duration = Duration::from_secs(60);

/// When the GPU device cannot be made, the `fatal` notification is written and its error returned,
/// and that request and later ones go unanswered. A request cancelled before it started is
/// answered `CANCELLED` without running. Requests run on worker threads, up to
/// [`Serving::max_running`] at once, and are answered as they end. After each answer the host
/// compares itself with its soft limits and `unusable`, and once draining refuses what it has not
/// started ([`retire`]).
fn serve_requests(
    serving: &mut Serving,
    requests: &Requests,
    out: &Responder,
    unusable: &Unusable<'_>,
) -> Result<Served, HostError> {
    let slots = Slots::new(serving.max_running);
    let failed = Mutex::new(None);
    let served = std::thread::scope(|scope| {
        dispatch(scope, serving, requests, out, unusable, &slots, &failed)
    });
    match failed.into_inner().unwrap_or_else(PoisonError::into_inner) {
        Some(error) => Err(error),
        None => served,
    }
}

#[allow(clippy::too_many_lines)]
fn dispatch<'scope>(
    scope: &'scope std::thread::Scope<'scope, '_>,
    serving: &mut Serving,
    requests: &Requests,
    out: &'scope Responder,
    unusable: &'scope Unusable<'scope>,
    slots: &'scope Slots,
    failed: &'scope Mutex<Option<HostError>>,
) -> Result<Served, HostError> {
    let lifecycle = Arc::clone(&serving.lifecycle);
    loop {
        if let Some(error) = failed.lock().unwrap_or_else(PoisonError::into_inner).take() {
            return Err(error);
        }
        if let Some((reason, _)) = lifecycle.draining_since() {
            return retire(requests, out, &lifecycle, reason, slots);
        }
        let wait = match (slots.idle_for(), lifecycle.idle_timeout()) {
            (Some(idle), Some(limit)) => Some(limit.saturating_sub(idle)),
            (Some(_), None) => None,
            (None, _) => Some(DISPATCH_POLL),
        };
        let input = match wait {
            Some(wait) => requests.next_within(wait),
            None => Some(requests.next()),
        };
        let Some(input) = input else {
            if let (Some(idle), Some(limit)) = (slots.idle_for(), lifecycle.idle_timeout())
                && idle >= limit
            {
                lifecycle.drain(DrainReason::Idle, out)?;
            }
            continue;
        };
        let (request_id, envelope, refused_export) = match input {
            InputFrame::End => {
                slots.wait_idle(None);
                if let Some((reason, _)) = lifecycle.draining_since()
                    && reason.exits_at_once()
                {
                    return Ok(Served::Drained(reason));
                }
                lifecycle.drain(DrainReason::InputClosed, out)?;
                return Ok(Served::InputEnded);
            }
            input => envelope_of(input),
        };
        let is_export = match &envelope {
            Ok(request) => matches!(request, crate::protocol::Request::Export(_)),
            Err(_) => refused_export,
        };
        // Cancelled while queued: answered without making a GPU device for it.
        if envelope.is_ok() && termination::terminations().take_pending_cancel(&request_id) {
            lifecycle.begin_request();
            let response = Err(cancelled_before_start(is_export));
            answer(
                &lifecycle,
                serving.device.as_deref(),
                out,
                &request_id,
                response,
                &[],
                unusable,
            )?;
            continue;
        }
        let needs_host = match &envelope {
            Ok(request) => !matches!(
                request,
                crate::protocol::Request::Conform(_) | crate::protocol::Request::Compose(_)
            ),
            Err(_) => refused_export,
        };
        // Taken first, so a request that runs alone answers before the next one makes the device.
        slots.acquire();
        let device = if needs_host {
            match serving.device() {
                Ok(device) => Some(device),
                Err(error) => {
                    slots.release();
                    return Err(write_fatal(error, out)?);
                }
            }
        } else {
            serving.device.clone()
        };
        if lifecycle.state() == lifecycle::HostState::Draining {
            slots.release();
            let reason = lifecycle
                .draining_since()
                .map_or(DrainReason::InputClosed, |(reason, _)| reason);
            refuse_retiring(out, &request_id, is_export, reason, &lifecycle)?;
            continue;
        }
        lifecycle.begin_request();
        let work = RequestWork {
            request_id,
            envelope,
            refused_export,
            is_export,
            device,
            policy: serving.policy,
            lifecycle: Arc::clone(&lifecycle),
            extract: serving.extract,
        };
        let spawned = std::thread::Builder::new()
            .name("alpha2-request".to_owned())
            .stack_size(COMMAND_STACK_BYTES)
            .spawn_scoped(scope, move || {
                let slot = SlotGuard(slots);
                if let Err(error) = work.run(out, unusable) {
                    failed
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .get_or_insert(error);
                }
                drop(slot);
            });
        if let Err(error) = spawned {
            slots.release();
            return Err(HostError::io(error));
        }
    }
}

/// A request handed to a worker thread.
struct RequestWork {
    request_id: String,
    envelope: Result<crate::protocol::Request, HostError>,
    refused_export: bool,
    is_export: bool,
    device: Option<Arc<HostDevice>>,
    policy: VideoDecodePolicy,
    lifecycle: Arc<Lifecycle>,
    extract: crate::extract_request::ExtractOptions,
}

impl RequestWork {
    /// Runs on its own thread, holding the request's termination guard until it is answered.
    fn run(self, out: &Responder, unusable: &Unusable) -> Result<(), HostError> {
        let Self {
            request_id,
            envelope,
            refused_export,
            is_export,
            device,
            policy,
            lifecycle,
            extract,
        } = self;
        #[cfg(test)]
        if matches!(envelope, Ok(crate::protocol::Request::Export(_)))
            && let Some(slow) = crate::encode::SERVE_SLOW_ENCODE
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
        {
            crate::encode::INJECT_SLOW_ENCODE.with(|injected| injected.set(Some(slow)));
        }
        // Until the answer is written, a termination waits for this request instead of ending the
        // process.
        let guard = termination::terminations().begin_request(&request_id);
        let mut host = device.clone().map(Host::for_request);
        let missing = || HostError::new("INTERNAL", "the request has no GPU device to run on");
        let response = match envelope {
            Ok(_) if guard.cancelled_before_start() => Err(cancelled_before_start(is_export)),
            Ok(crate::protocol::Request::Conform(request)) => {
                crate::host::conform_media(request, policy)
            }
            Ok(crate::protocol::Request::Compose(request)) => {
                crate::compose_request::answer(&request)
            }
            Ok(crate::protocol::Request::Extract(request)) => match host.as_mut() {
                Some(host) => {
                    let answer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        crate::extract_request::handle(host, *request, out, extract)
                    }))
                    .unwrap_or_else(|payload| {
                        Err(HostError::new(
                            "INTERNAL",
                            format!(
                                "the host panicked handling the request: {}",
                                crate::failure::panic_message(&*payload)
                            ),
                        ))
                    });
                    host.end_extract();
                    answer
                }
                None => Err(missing()),
            },
            Ok(request) => match host.as_mut() {
                Some(host) => host.handle(request, &mut |progress| {
                    let mut event = json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "type": "progress",
                        "requestId": request_id,
                        "phase": progress.phase.as_str(),
                        "completed": progress.completed,
                        "total": progress.total,
                    });
                    if progress.restarted {
                        event["restarted"] = json!(true);
                    }
                    out.send(&event)
                }),
                None => Err(missing()),
            },
            Err(error) if refused_export => match &host {
                Some(host) => Err(host.refuse_export(error)),
                None => Err(missing()),
            },
            Err(error) => Err(error),
        };
        let attachments = host
            .as_mut()
            .map(|host| std::mem::take(&mut host.reply_attachments))
            .filter(|_| response.is_ok())
            .unwrap_or_default();
        // Its effects and resources go back before the answer, for the next request.
        drop(host);
        answer(
            &lifecycle,
            device.as_deref(),
            out,
            &request_id,
            response,
            &attachments,
            unusable,
        )?;
        // A termination ends the process here, once the last answer is out.
        drop(guard);
        Ok(())
    }
}

/// The requests running, bounded by `limit`.
struct Slots {
    limit: usize,
    state: Mutex<(usize, std::time::Instant)>,
    freed: std::sync::Condvar,
}

impl Slots {
    fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            state: Mutex::new((0, std::time::Instant::now())),
            freed: std::sync::Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, (usize, std::time::Instant)> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn acquire(&self) {
        let mut state = self.lock();
        while state.0 >= self.limit {
            state = self
                .freed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.0 += 1;
    }

    fn release(&self) {
        let mut state = self.lock();
        state.0 = state.0.saturating_sub(1);
        if state.0 == 0 {
            state.1 = std::time::Instant::now();
        }
        drop(state);
        self.freed.notify_all();
    }

    /// How long nothing has run; `None` while a request runs.
    fn idle_for(&self) -> Option<Duration> {
        let state = self.lock();
        (state.0 == 0).then(|| state.1.elapsed())
    }

    /// Waits until nothing runs, or `limit` passed. Returns whether nothing runs.
    fn wait_idle(&self, limit: Option<Duration>) -> bool {
        let deadline = limit.map(|limit| std::time::Instant::now() + limit);
        let mut state = self.lock();
        while state.0 > 0 {
            let left = match deadline {
                Some(deadline) => deadline.saturating_duration_since(std::time::Instant::now()),
                None => DISPATCH_POLL,
            };
            if left.is_zero() {
                return false;
            }
            state = self
                .freed
                .wait_timeout(state, left.min(DISPATCH_POLL))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }
}

/// Frees its slot even when the request's thread unwinds.
struct SlotGuard<'a>(&'a Slots);

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// A request read, or why the line is not one, with its id and whether a refusal answers an
/// export.
type Envelope = (String, Result<crate::protocol::Request, HostError>, bool);

fn envelope_of(input: InputFrame) -> Envelope {
    match input {
        InputFrame::Request {
            header,
            attachments,
        } => {
            let envelope = attachments.and_then(|attachments| {
                if attachments.is_empty() {
                    Ok(())
                } else {
                    Err(format!(
                        "{}; no operation takes attachments",
                        attachment_count(attachments.len())
                    ))
                }
            });
            let envelope = envelope
                .map_err(|reason| HostError::new("INVALID_REQUEST", reason))
                .and_then(|()| parse_request(&header));
            let request_id = match &envelope {
                Ok(request) => request.request_id().to_owned(),
                Err(_) => request_id_from_header(&header),
            };
            // A refused request answers an export unless it says it is another operation.
            let export = envelope.is_err()
                && operation_from_header(&header).is_none_or(|operation| operation == "export");
            (request_id, envelope, export)
        }
        InputFrame::NotUtf8 => (
            "unknown".to_owned(),
            Err(HostError::new(
                "INVALID_REQUEST",
                "a request header must be UTF-8 JSON",
            )),
            true,
        ),
        InputFrame::TooLarge(bytes) => (
            "unknown".to_owned(),
            Err(HostError::new(
                "INVALID_REQUEST",
                format!(
                    "the request frame is {bytes} bytes; a header is at most {} and its \
                     attachments at most {}",
                    crate::protocol::MAX_REQUEST_HEADER_BYTES,
                    crate::protocol::MAX_REQUEST_ATTACHMENT_BYTES
                ),
            )),
            true,
        ),
        InputFrame::End => unreachable!("the end of input is no request"),
    }
}

fn attachment_count(count: usize) -> String {
    if count == 1 {
        "the request has an attachment".to_owned()
    } else {
        format!("the request has {count} attachments")
    }
}

/// Ends the request: drains first when the host should not take another, so the answer carries
/// the state.
fn answer(
    lifecycle: &Lifecycle,
    device: Option<&HostDevice>,
    out: &Responder,
    request_id: &str,
    response: Result<Value, HostError>,
    attachments: &[Attachment],
    unusable: &Unusable<'_>,
) -> Result<(), HostError> {
    lifecycle.end_request();
    let reason = unusable(device)
        .or_else(|| termination::requested().map(|_| DrainReason::Terminating))
        .or_else(|| lifecycle.over_soft_limit());
    if let Some(reason) = reason {
        lifecycle.drain(reason, out)?;
    }
    write_answer(out, request_id, response, attachments, lifecycle)
}

/// Answers `HOST_RETIRING` to every request read but not started, and to those that arrive until
/// the input ends or [`lifecycle::DRAIN_GRACE`] after draining, once nothing runs. A host that
/// cannot take another request waits up to [`RETIRE_WAIT`] for those it runs, then exits.
fn retire(
    requests: &Requests,
    out: &Responder,
    lifecycle: &Lifecycle,
    reason: DrainReason,
    slots: &Slots,
) -> Result<Served, HostError> {
    if reason.exits_at_once() {
        slots.wait_idle(Some(RETIRE_WAIT));
        while let Some(input) = requests.queued() {
            if matches!(input, InputFrame::End) {
                break;
            }
            refuse_frame(out, &input, reason, lifecycle)?;
        }
        let served = Ok(Served::Drained(reason));
        // A request stuck past the wait would hold the worker threads' scope open.
        exit_if_retired(&served);
        return served;
    }
    let since = lifecycle
        .draining_since()
        .map_or_else(std::time::Instant::now, |(_, since)| since);
    let deadline = since + lifecycle::DRAIN_GRACE;
    let mut ended = false;
    loop {
        let now = std::time::Instant::now();
        if (ended || now >= deadline) && slots.idle_for().is_some() {
            return Ok(Served::Drained(reason));
        }
        if ended {
            slots.wait_idle(None);
            continue;
        }
        let wait = deadline.saturating_duration_since(now).min(DISPATCH_POLL);
        match requests.next_within(wait.max(Duration::from_millis(1))) {
            None => {}
            Some(InputFrame::End) => ended = true,
            Some(input) => refuse_frame(out, &input, reason, lifecycle)?,
        }
    }
}

fn refuse_frame(
    out: &Responder,
    input: &InputFrame,
    reason: DrainReason,
    lifecycle: &Lifecycle,
) -> Result<(), HostError> {
    let (request_id, export) = match input {
        InputFrame::Request { header, .. } => (
            request_id_from_header(header),
            operation_from_header(header).as_deref() == Some("export"),
        ),
        InputFrame::TooLarge(_) | InputFrame::NotUtf8 | InputFrame::End => {
            ("unknown".to_owned(), false)
        }
    };
    refuse_retiring(out, &request_id, export, reason, lifecycle)
}

fn refuse_retiring(
    out: &Responder,
    request_id: &str,
    export: bool,
    reason: DrainReason,
    lifecycle: &Lifecycle,
) -> Result<(), HostError> {
    let error = HostError::new(
        "HOST_RETIRING",
        format!(
            "the host is draining ({}) and did not start the request; send it to another host",
            reason.as_str()
        ),
    );
    let error = if export {
        let failure = crate::failure::startup_failure_json(error.code, &error.message, None);
        error.with_failure(failure)
    } else {
        error
    };
    write_answer(out, request_id, Err(error), &[], lifecycle)
}

fn write_answer(
    out: &Responder,
    request_id: &str,
    response: Result<Value, HostError>,
    attachments: &[Attachment],
    lifecycle: &Lifecycle,
) -> Result<(), HostError> {
    let host_state = lifecycle.state().as_str();
    let header = match response {
        Ok(value) => json!({
            "protocolVersion": PROTOCOL_VERSION,
            "requestId": request_id,
            "ok": true,
            "result": value,
            "hostState": host_state,
        }),
        Err(error) => json!({
            "protocolVersion": PROTOCOL_VERSION,
            "requestId": request_id,
            "ok": false,
            "error": error,
            "hostState": host_state,
        }),
    };
    out.send_with(&header, attachments)
}

/// An export's answer carries the failure report every failed export has.
fn cancelled_before_start(is_export: bool) -> HostError {
    let error = HostError::new("CANCELLED", "the request was cancelled before it started");
    if is_export {
        let failure = crate::failure::startup_failure_json(error.code, &error.message, None);
        error.with_failure(failure)
    } else {
        error
    }
}

/// One JSON line on standard output, for the commands other than `serve`.
fn write_stdout(value: &Value) -> Result<(), HostError> {
    let mut line = serve_io::json_line(value);
    line.push(b'\n');
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&line).map_err(HostError::io)?;
    stdout.flush().map_err(HostError::io)
}

enum GpuCall {
    Fault(alpha2_media::GpuFault),
    /// A native interop helper's call whose status names no fault.
    #[cfg_attr(not(feature = "hwdevice"), allow(dead_code))]
    Failed,
}

fn gpu_call(error: &(dyn std::error::Error + 'static)) -> Option<GpuCall> {
    #[cfg(feature = "hwdevice")]
    {
        let mut cause = Some(error);
        while let Some(current) = cause {
            if let Some(alpha2_ffmpeg_bridge::BridgeError::NativeGpu(failure)) =
                current.downcast_ref::<alpha2_ffmpeg_bridge::BridgeError>()
            {
                return Some(failure.fault.map_or(GpuCall::Failed, GpuCall::Fault));
            }
            cause = current.source();
        }
    }
    alpha2_engine::gpu_fault_of(error).map(GpuCall::Fault)
}

/// Tags `error`'s text with the protocol code its type decides ([`alpha2_engine::ErrorCode`], an
/// output the encoder could not create, a failed GPU call) for [`HostError::classify`], or with
/// failure-class evidence ([`failure::evidence_tag`]) for a fault with no code of its own.
fn error_message(error: impl std::error::Error + 'static) -> String {
    use alpha2_media::GpuFault;
    match gpu_call(&error) {
        Some(GpuCall::Fault(GpuFault::Timeout)) => {
            return format!("{}{}", protocol::gpu_timeout_tag(), error);
        }
        Some(GpuCall::Fault(GpuFault::DeviceLost)) => {
            return protocol::code_tag("GPU_DEVICE_LOST", error.to_string());
        }
        Some(GpuCall::Fault(GpuFault::DeviceHung)) => {
            return protocol::code_tag("GPU_DEVICE_HUNG", error.to_string());
        }
        // Exhausted memory has no protocol code; the failure class carries it.
        Some(GpuCall::Fault(GpuFault::DeviceMemory)) => {
            return failure::evidence_tag(
                "OUT_OF_MEMORY",
                protocol::code_tag("ALPHA2_ERROR", error.to_string()),
            );
        }
        Some(GpuCall::Fault(GpuFault::HostMemory)) => {
            return failure::evidence_tag(
                "ALLOCATION_FAILED",
                protocol::code_tag("ALPHA2_ERROR", error.to_string()),
            );
        }
        Some(_) => return protocol::code_tag("ALPHA2_ERROR", error.to_string()),
        None => {}
    }
    match alpha2_engine::gpu_error_of(&error) {
        Some(alpha2_gpu::GpuErrorKind::OutOfMemory) => {
            return failure::evidence_tag(
                "OUT_OF_MEMORY",
                protocol::code_tag("ALPHA2_ERROR", error.to_string()),
            );
        }
        Some(alpha2_gpu::GpuErrorKind::Validation) => {
            return failure::evidence_tag(
                "VALIDATION_FAILED",
                protocol::code_tag("ALPHA2_ERROR", error.to_string()),
            );
        }
        Some(_) => return protocol::code_tag("ALPHA2_ERROR", error.to_string()),
        None => {}
    }
    // An output the encoder could not create is the file system's answer, or another export is
    // writing it.
    match (&error as &dyn std::error::Error).downcast_ref::<alpha2_media::EncodeError>() {
        Some(alpha2_media::EncodeError::Output(_)) => {
            return protocol::code_tag("IO_ERROR", error.to_string());
        }
        Some(alpha2_media::EncodeError::OutputInUse(_)) => {
            return protocol::code_tag("OUTPUT_IN_USE", error.to_string());
        }
        Some(alpha2_media::EncodeError::Publish { kept, .. }) => {
            return protocol::output_publish_failed(kept.as_deref(), error.to_string());
        }
        Some(alpha2_media::EncodeError::EncoderRejected { frame, .. }) => {
            return protocol::encoder_rejected_tag(*frame, error.to_string());
        }
        _ => {}
    }
    match alpha2_engine::ErrorCode::of(&error) {
        Some(code) => protocol::code_tag(code.as_str(), error.to_string()),
        None => error.to_string(),
    }
}

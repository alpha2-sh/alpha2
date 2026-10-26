//! Native project-rendering host and command-line entry point.
//!
//! # Process protocol v1
//!
//! `alpha2-host serve` exchanges one UTF-8 JSON object per line over standard
//! input and output. Every message contains `protocolVersion: 1`. Requests
//! contain a caller-unique string `requestId` and an `operation`. The v1
//! operations are `capabilities`, `conform` (which media each source would
//! read, without rendering) and `export`. Browser hosts preview through the
//! wasm engine; this process renders only exports. Successful responses are
//! `{protocolVersion, requestId, ok: true, result}`. Failures are
//! `{protocolVersion, requestId, ok: false, error: {code, message}}`.
//! During export the host may emit unsolicited
//! `{protocolVersion, type: "progress", requestId, phase, completed, total}`
//! lines: `phase` is `frames` while frames are encoded and `finalizing` once
//! every frame is and the output is being flushed and closed. A failed export's
//! error also carries `failure`: its class, the step and frame it failed at,
//! and the diagnostics recorded up to then.
//!
//! Requests may be pipelined and must be correlated solely by `requestId`.
//! This v1 executable processes them serially; clients must not depend on
//! response ordering because a future resident implementation may run them
//! concurrently. A host whose worker threads are stuck or whose device is
//! lost exits after its answer, and the requests behind it go unanswered.
//! Cancellation is process termination: `SIGTERM` or `SIGINT`
//! (a console control event on Windows) cancels the running export, which
//! removes its partial output and is answered `CANCELLED` before the process
//! exits; see [`termination`]. Assets and outputs are local filesystem paths;
//! transport and staging belong to the caller.
//!
//! Runtime WGSL is executable GPU input. A remote or multi-tenant host must
//! authenticate and authorize effect definitions before forwarding them. The
//! engine validates definitions but does not decide who is allowed to register
//! one.

// A closed standard error must not panic the process: diagnostics go through
// `alpha2_os::diagnostic!`.
#![cfg_attr(not(test), warn(clippy::print_stderr))]

mod adapter_choice;
mod assets;
mod audio;
mod capture;
mod cli;
mod config;
mod cpu_frames;
#[cfg(all(target_os = "windows", feature = "hwencode"))]
mod d3d12va_probe;
mod decode;
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
mod dx12_adapters;
mod effect_request;
mod encode;
mod export_decode;
mod failure;
mod frames;
#[cfg(all(target_os = "windows", feature = "hwencode"))]
mod gpu_encode_probe;
#[cfg(feature = "hwencode")]
mod gpu_export;
mod gpu_output;
mod heartbeat;
mod host;
mod media;
mod memory;
mod metrics;
mod packet_copy;
mod protocol;
mod providers;
mod recovery;
mod render_cache;
mod renderer_backend;
mod tag;
mod teardown;
mod termination;
#[cfg(test)]
mod tests;
mod trace;
#[cfg(feature = "hwencode")]
mod yuv_transcode;

use std::collections::HashSet;
use std::env;
use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::time::Duration;

use crate::cli::{conform_cli, export_cli, usage};
use crate::config::VideoDecodePolicy;
use crate::host::Host;
use crate::protocol::{
    HostError, MAX_REQUEST_LINE_BYTES, operation_from_line, parse_request, request_id_from_line,
};
use alpha2_gpu::{GpuContext, GpuDiagnostic};
use alpha2_native::ffmpeg_runtime_identity;
use alpha2_render::Renderer;
use serde_json::{Value, json};

const PROTOCOL_VERSION: u32 = 1;
const GPU_INIT_EXIT_CODE: u8 = 70;
const DEFAULT_EXPORT_IN_FLIGHT_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_IN_FLIGHT_VIDEO_FRAMES: usize = 2;
const MAX_IN_FLIGHT_VIDEO_FRAMES: usize = 3;
const MAX_PENDING_MEDIA_EVENTS: usize = 8;
/// Smallest encode channel depth a multi-core host runs with. One slot for
/// the frame being encoded and one queued behind it is what keeps the encoder
/// from waiting on the coordinator between frames.
const MIN_ENCODE_CHANNEL_DEPTH: usize = 2;
/// Largest depth the derivation may reach. Beyond this the channel holds more
/// output frames than any measured bubble consumes, and each one is a full
/// packed frame of memory.
const MAX_ENCODE_CHANNEL_DEPTH: usize = 8;
const AUDIO_INTERPOLATION_MARGIN: i64 = 2;
const DEFAULT_AUDIO_WINDOW_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_DECODE_QUEUE_BYTES: u64 = 256 * 1024 * 1024;
/// What the open extension-provider sources of an export hold together: two
/// sources at their own decoded-frame budget
/// ([`alpha2_extensions::DEFAULT_FRAME_BUDGET`]).
const DEFAULT_PROVIDER_SOURCE_BYTES: u64 = 512 * 1024 * 1024;
const DEFAULT_DECODE_LOOKAHEAD: usize = 2;
const MAX_DECODE_LOOKAHEAD: usize = 3;
const DEFAULT_DECODE_CONCURRENCY: usize = 2;
const MAX_DECODE_CONCURRENCY: usize = 4;
const DEFAULT_DECODER_SESSIONS: usize = 8;
const MAX_DECODER_SESSIONS: usize = 32;
const DEFAULT_REVERSE_CACHE_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_REVERSE_HARDWARE_SURFACES: usize = 8;
/// Decoder surfaces all decode sessions may retain at once
/// (`ALPHA2_DECODE_SURFACES`).
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

fn main() -> ExitCode {
    let mut host = None;
    let result = run(&mut host);
    ExitCode::from(conclude(result, host))
}

/// Reports `result`, a command's outcome, then releases `host`, the GPU host
/// it made, and returns the process's exit status.
///
/// The release comes last because it can hang on a device that never goes
/// idle; it is bounded ([`teardown`]), and by then the caller has the outcome.
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

/// Ends the process now, without its exit handlers, when a decode shard or
/// the encode worker is stuck inside `FFmpeg`.
///
/// Those threads may be inside the GPU driver, and the driver's exit handlers
/// wait for them, so a normal exit would hang after the failure was reported.
/// Everything this host owes its caller has been written by now; what the
/// handlers would have released goes with the process.
fn exit_if_worker_threads_abandoned(code: u8) {
    if !heartbeat::worker_threads_abandoned() {
        return;
    }
    exit_now(code);
}

/// Ends the process after the answer is out when its device cannot run
/// another request ([`gpu_unusable`]).
fn exit_if_gpu_lost(host: &Host) {
    let Some(reason) = gpu_unusable(host) else {
        return;
    };
    alpha2_os::diagnostic!(
        "alpha2-host: {reason}; exiting so that the next request gets a new process"
    );
    exit_now(1);
}

/// Why `host`'s device cannot run another request: it was found unable to
/// run work (lost, or completing work without running it), or `wgpu`
/// reported it lost.
///
/// A macOS process that hung the GPU twice has its submissions ignored for
/// good, and the renderer finds out only from a readback's missing canary.
/// Every export after that would fail, or, where nothing reads back, write
/// frames the GPU never rendered; a new process gets a device that runs
/// its work. A lost device is the same: nothing it makes works again. A
/// loss `wgpu` reported does not reach the renderer until its next
/// submission, so without this check a host whose readback failed to map
/// would keep taking exports that can only fail.
fn gpu_unusable(host: &Host) -> Option<String> {
    if let Some(lost) = host.renderer.gpu().and_then(Renderer::gpu_lost) {
        return Some(lost.to_string());
    }
    host.gpu_log
        .device_lost()
        .then(|| "the GPU device was lost".to_owned())
}

/// What `serve` does after each answer: a host whose decode or encode
/// threads are stuck, or whose device no longer runs its work, cannot take
/// another request, so the process exits, and the requests it had not read
/// go unanswered.
fn exit_if_unusable(host: Option<&Host>) {
    exit_if_worker_threads_abandoned(
        termination::requested().map_or(1, alpha2_os::Termination::exit_code),
    );
    if let Some(host) = host {
        exit_if_gpu_lost(host);
    }
}

/// Ends the process now, without its exit handlers, which can wait on
/// threads inside the GPU driver.
fn exit_now(code: u8) -> ! {
    let _ = std::io::stdout().flush();
    exit_unflushed(code)
}

/// [`exit_now`] without flushing stdout, for another thread than the one
/// serving: that thread holds stdout's lock while it waits for a request
/// line, and every answer it wrote was flushed when written.
fn exit_unflushed(code: u8) -> ! {
    let _ = std::io::stderr().flush();
    #[cfg(feature = "hwdevice")]
    alpha2_ffmpeg_bridge::exit_without_handlers(code);
    // Without a hardware device no thread is inside a GPU driver, and the handlers can run.
    #[cfg(not(feature = "hwdevice"))]
    std::process::exit(i32::from(code));
}

/// Runs the command the arguments name, leaving the GPU host it made, if
/// any, in `host` for [`conclude`] to release.
fn run(host: &mut Option<Host>) -> Result<(), HostError> {
    let mut args = utf8_arguments(env::args_os().skip(1))?.into_iter();
    match args.next().as_deref() {
        Some("serve") => {
            if args.next().is_some() {
                return Err(HostError::invalid_input(usage()));
            }
            serve(host)
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

/// The command line as text, or `INVALID_INPUT` naming the first argument
/// that is not UTF-8 (a Linux file name in a legacy encoding).
///
/// Every value the host takes is read as UTF-8 further on, paths included;
/// `env::args` would panic on such an argument instead.
fn utf8_arguments(
    arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<Vec<String>, HostError> {
    arguments
        .map(|argument| {
            argument.into_string().map_err(|argument| {
                HostError::invalid_input(format!(
                    "argument {:?} is not valid UTF-8",
                    argument.to_string_lossy()
                ))
            })
        })
        .collect()
}

/// Reports whether this device can drive `h264_vulkan` on the shared device.
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

/// Renders one solid frame, packs it to NV12, hands it to an encoder surface
/// through the hardware-encode contract eight times, and writes an MP4.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode"
))]
fn render_gpu_encode_slice(
    gpu: &GpuContext,
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
            output: std::path::Path::new(path),
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
        // Submit through the shared context so the queue lock FFmpeg also takes is held. Submitting
        // on `queue` directly would race the decoder on this device.
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
    session.finish().map_err(encode_error)?;
    input.finish().map_err(encode_error)?;
    Ok(json!({
        "path": path,
        "frames": frames,
        "solidRgba": [0.25, 0.5, 0.75, 1.0],
    }))
}

/// Reports whether `h264_videotoolbox` opens a hardware session at a size and
/// encodes a pool frame whose planes the renderer can draw into. With an
/// output path it also renders a solid frame into eight pool surfaces through
/// the hardware-encode contract and writes an MP4.
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
    let gpu = pollster::block_on(GpuContext::request_high_performance())
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

/// Makes the host the renderer policy (`ALPHA2_RENDERER`, default `auto`)
/// asks for, decoding as `policy` says.
fn create_host(policy: VideoDecodePolicy) -> Result<Host, HostError> {
    let renderer = crate::config::RendererPolicy::from_env().map_err(HostError::invalid_input)?;
    create_host_with(policy, renderer)
}

/// Makes the host `renderer` asks for.
///
/// `auto` takes a hardware GPU when one starts, and otherwise the CPU
/// renderer: never a software adapter, which renders the same pictures an
/// order of magnitude slower. A GPU host that came up on one anyway (the
/// hardware adapters failed) is let go for the CPU renderer.
pub(crate) fn create_host_with(
    policy: VideoDecodePolicy,
    renderer: crate::config::RendererPolicy,
) -> Result<Host, HostError> {
    use crate::config::RendererPolicy;
    match renderer {
        RendererPolicy::Gpu => create_gpu_host(policy),
        RendererPolicy::Cpu => create_cpu_host(policy, "requested (--renderer cpu)".to_owned()),
        RendererPolicy::Auto => {
            if let Some(reason) = no_hardware_adapter() {
                return create_cpu_host(policy, reason);
            }
            match create_gpu_host(policy) {
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
                Err(error) if error.code == "GPU_INIT_FAILED" => {
                    create_cpu_host(policy, format!("no GPU host: {}", error.message))
                }
                Err(error) => Err(error),
            }
        }
    }
}

/// Why there is no hardware adapter to render on, or `None` when there is
/// one: every adapter the backends in use list is a software rasterizer, or
/// none is listed (no driver, or no graphics loader at all).
fn no_hardware_adapter() -> Option<String> {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let backends = wgpu::Backends::from_env().unwrap_or(wgpu::Backends::all());
    let adapters = pollster::block_on(instance.enumerate_adapters(backends));
    let mut software = Vec::new();
    for adapter in &adapters {
        let info = adapter.get_info();
        if info.device_type != wgpu::DeviceType::Cpu {
            return None;
        }
        software.push(info.name);
    }
    Some(if software.is_empty() {
        "no GPU adapter was found".to_owned()
    } else {
        format!(
            "no hardware GPU; only software adapters ({}), which auto does not use",
            software.join(", ")
        )
    })
}

/// A host that renders on the CPU, for `reason`.
fn create_cpu_host(policy: VideoDecodePolicy, reason: String) -> Result<Host, HostError> {
    if policy.requires_hardware() {
        return Err(HostError::new(
            "HARDWARE_DECODE_UNAVAILABLE",
            format!("hardware decode is required, but this host renders on the CPU ({reason})"),
        ));
    }
    let renderer = alpha2_render::CpuRenderer::new(crate::config::configured_render_threads());
    alpha2_os::diagnostic!(
        "alpha2-host renderer: CPU, {} threads ({reason})",
        renderer.threads()
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

#[allow(clippy::too_many_lines)]
fn create_gpu_host(policy: VideoDecodePolicy) -> Result<Host, HostError> {
    // Nothing in a build without hardware decode can keep `hardware-required`: it would decode
    // every frame in software under the mode's name.
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
    let request_default = || {
        pollster::block_on(GpuContext::request_high_performance())
            .map_err(|error| HostError::new("GPU_INIT_FAILED", error_message(error)))
    };
    // The renderer goes on the shared Vulkan device, so a non-Vulkan backend skips it.
    #[cfg(all(target_os = "linux", feature = "hwdevice"))]
    let (shared_vulkan, unavailable) = shared_device_context(
        policy,
        cfg!(feature = "hwencode"),
        backend_excluded(wgpu::Backends::from_env(), wgpu::Backends::VULKAN, "Vulkan"),
        alpha2_native_gpu::SharedVulkanContext::create,
    )?;
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
    // D3D12VA shares the renderer's device, so the renderer must be on DX12: left to itself wgpu
    // prefers Vulkan on Windows, which would silently turn hardware decode off. By default a
    // decoding mode takes the first adapter D3D12VA decodes on (`ALPHA2_GPU_ADAPTER` overrides).
    #[cfg(all(target_os = "windows", feature = "hwencode"))]
    crate::d3d12va_probe::install();
    #[cfg(all(target_os = "windows", feature = "hwdevice"))]
    let adapter_policy = crate::adapter_choice::AdapterPolicy::from_env();
    #[cfg(all(target_os = "windows", feature = "hwdevice"))]
    let decode_on_gpu = cfg!(feature = "hwdecode") && policy.allows_hardware();
    #[cfg(all(target_os = "windows", feature = "hwdevice"))]
    let encode_on_gpu = gpu_encode_wanted();
    #[cfg(all(target_os = "windows", feature = "hwdevice"))]
    let (mut dx12, dx12_unavailable) = {
        let scanned = pollster::block_on(crate::dx12_adapters::scan(
            &adapter_policy,
            decode_on_gpu,
            encode_on_gpu,
        ));
        // A pin that names no adapter refuses to start under every mode.
        if let Err(crate::dx12_adapters::ScanError::Pin(reason)) = &scanned {
            return Err(HostError::invalid_input(reason.clone()));
        }
        decode_backend_gpu(
            policy,
            "DX12",
            || scanned,
            || request_default().map(crate::dx12_adapters::Dx12Choice::without_scan),
        )?
    };
    #[cfg(all(target_os = "windows", feature = "hwdevice"))]
    let gpu = dx12.gpu.clone();
    #[cfg(not(any(
        all(target_os = "linux", feature = "hwdevice"),
        all(target_os = "windows", feature = "hwdevice")
    )))]
    let gpu = request_default()?;
    #[cfg(all(target_os = "macos", feature = "hwdevice"))]
    let (shared_metal, unavailable) =
        shared_device_context(policy, cfg!(feature = "hwencode"), None, || {
            alpha2_native_gpu::SharedMetalContext::from_gpu(gpu.clone())
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
    let mut renderer = Renderer::new(gpu);
    let wait_timeout = crate::config::configured_gpu_wait_timeout();
    renderer.set_gpu_wait_timeout(wait_timeout);
    #[cfg(feature = "hwdevice")]
    alpha2_native_gpu::set_gpu_wait_timeout(wait_timeout);
    let builtin_ids = pollster::block_on(renderer.register_builtins()).map_err(|diagnostics| {
        HostError::new(
            "EFFECT_ERROR",
            format!("built-in effect registration failed: {diagnostics:?}"),
        )
    })?;
    let adapter = renderer.diagnostics().adapter;
    alpha2_os::diagnostic!(
        "alpha2-host GPU: {}, {}, {} {}, tier {:?}",
        adapter.name,
        adapter.backend,
        adapter.driver,
        adapter.driver_info,
        adapter.tier
    );
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
        host.hardware_decode_unavailable = unavailable;
    }
    #[cfg(all(target_os = "windows", feature = "hwencode"))]
    {
        host.encode_adapters = crate::adapter_choice::EncodeAdapterNotes {
            no_device: no_encode_device,
            elsewhere: dx12.chosen.map_or_else(Vec::new, |chosen| {
                crate::adapter_choice::encode_elsewhere(&dx12.records, chosen, decode_on_gpu)
            }),
        };
    }
    #[cfg(all(target_os = "windows", feature = "hwencode"))]
    if let Some(test_encodes) = test_encodes {
        test_encodes.finish();
    }
    #[cfg(all(target_os = "windows", feature = "hwdevice"))]
    {
        #[cfg(not(feature = "hwencode"))]
        let _ = no_encode_device;
        report_dx12_adapters(&host, &dx12, encode_on_gpu);
    }
    host.gpu_log = gpu_log;
    // Read now, before an upgrade can replace the file this process runs.
    if let Err(reason) = crate::host::host_binary_digest() {
        alpha2_os::diagnostic!("alpha2-host render cache unavailable: {reason}");
    }
    Ok(host)
}

/// Logs the `FFmpeg` runtime this host linked and the encoders it offers.
fn log_runtime_codecs() {
    // What is known without opening an encoder: an encoder that must be opened to prove it runs
    // here (NVENC, Media Foundation, VideoToolbox) is opened when an export or `capabilities`
    // first asks, not by every host that starts.
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
}

/// Creates the renderer's GPU on the backend hardware decode shares.
///
/// `preferred` makes it on that backend, under every decode mode: the
/// renderer's backend does not change with the mode, so the backend a
/// platform's tests run on is the one every export runs on. When it
/// cannot, `software` and `auto` take the default GPU (`auto` reporting why
/// and decoding in software), and `hardware-required` refuses to start with
/// that reason instead of a generic GPU failure.
#[cfg(any(test, all(target_os = "windows", feature = "hwdevice")))]
fn decode_backend_gpu<T, E: std::fmt::Debug>(
    policy: VideoDecodePolicy,
    backend: &str,
    preferred: impl FnOnce() -> Result<T, E>,
    fallback: impl FnOnce() -> Result<T, HostError>,
) -> Result<(T, Option<String>), HostError> {
    match preferred() {
        Ok(gpu) => Ok((gpu, None)),
        Err(error) => {
            let reason = format!("{backend} unavailable: {error:?}");
            if policy.requires_hardware() {
                return Err(HostError::new("HARDWARE_DECODE_UNAVAILABLE", reason));
            }
            if !policy.allows_hardware() {
                alpha2_os::diagnostic!("alpha2-host {reason}; rendering on the default backend");
                return fallback().map(|gpu| (gpu, None));
            }
            alpha2_os::diagnostic!(
                "alpha2-host hardware decode unavailable, using software: {reason}"
            );
            fallback().map(|gpu| (gpu, Some(reason)))
        }
    }
}

/// Whether this host may encode on the GPU: a build with the GPU-resident
/// export, and `ALPHA2_GPU_ENCODE` not off. Only then is a device made for
/// encode alone, and a test encode run.
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
fn gpu_encode_wanted() -> bool {
    #[cfg(feature = "hwencode")]
    return crate::config::configured_gpu_encode_switch().is_ok();
    #[cfg(not(feature = "hwencode"))]
    false
}

/// The shared D3D12 device, why hardware decode is unavailable, and why
/// encode has no device: [`d3d12_devices`]'s answer.
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
type D3D12Devices = (
    Option<alpha2_native_gpu::SharedD3D12Context>,
    Option<String>,
    Option<String>,
);

/// The shared D3D12 device the adapter choice made, what hardware decode
/// makes of it under `policy`, and why encode has none.
///
/// For decode, `auto` reports why the renderer's adapter does not decode and
/// continues in software, while `hardware-required` refuses to start. A
/// device only encode wanted is best-effort: without it every export takes
/// the readback path and says why. A device made for encode under
/// `software` is kept, and decode does not use it ([`Host::decode_d3d12`]).
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

/// Starts the test encode of each codec `shared`'s device says it encodes,
/// all at once, in child processes, when this host may encode on the GPU:
/// what an export would be offered depends on them, and every later
/// question is answered from what they said.
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

/// Says on stderr which adapter the renderer is on, what it decodes and
/// encodes, and what every adapter passed over does. What it encodes is
/// what an export would be offered, test encode included; the last line
/// names the encoders the GPU-resident path takes, or why it takes none.
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
#[cfg_attr(not(feature = "hwencode"), allow(unused_variables))]
fn report_dx12_adapters(host: &Host, dx12: &crate::dx12_adapters::Dx12Choice, encode_on_gpu: bool) {
    #[cfg(feature = "hwencode")]
    let encodes = match (&host.shared_d3d12, encode_on_gpu) {
        (_, false) => "off (ALPHA2_GPU_ENCODE is off)".to_owned(),
        (None, true) => "none".to_owned(),
        (Some(_), true) => {
            let taken: Vec<_> = host
                .gpu_output_codecs()
                .iter()
                .map(|capability| capability.codec)
                .collect();
            if taken.is_empty() {
                "none".to_owned()
            } else {
                crate::adapter_choice::codec_list(&taken)
            }
        }
    };
    #[cfg(not(feature = "hwencode"))]
    let encodes = "not in this build".to_owned();
    alpha2_os::diagnostic!(
        "alpha2-host GPU: {} (renders, decodes: {}, encodes: {encodes})",
        host.renderer.diagnostics().adapter.name,
        if host.hardware_decode_ready() {
            "yes"
        } else {
            "no"
        }
    );
    for (at, record) in dx12.records.iter().enumerate() {
        if Some(at) != dx12.chosen {
            alpha2_os::diagnostic!("alpha2-host GPU passed over: {}", record.summary());
        }
    }
    #[cfg(feature = "hwencode")]
    let encode = gpu_encode_diagnostic(
        &host.renderer.diagnostics().adapter.name,
        &host.gpu_output_codecs(),
        host.gpu_output_capability().err(),
    );
    #[cfg(not(feature = "hwencode"))]
    let encode = "unavailable: this host was built without hardware encode".to_owned();
    alpha2_os::diagnostic!("alpha2-host GPU encode: {encode}");
}

/// The start line's account of the GPU-resident path: the backend and every
/// encoder it takes on `adapter`, or why it takes none.
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

/// Why a shared device on `shared` must not be made: `chosen`, the backends
/// `WGPU_BACKEND` names, leaves it out. An explicit backend is respected,
/// as documented; the shared device is the renderer's, so making it anyway
/// would render on Vulkan whatever was asked for.
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

/// What `hazard`, why `FFmpeg` must not open a video session on the shared
/// device, leaves of hardware decode under `policy`: nothing when there is
/// none or decode is in software anyway, the reason `auto` decodes in
/// software, or, for `hardware-required`, the refusal to start. The device
/// itself is kept: it renders, and the GPU encoder asks the same question.
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

/// Creates one backend's shared device context.
///
/// Hardware decode wants it when `policy` allows decoding on the GPU; the
/// GPU-resident export wants it whenever `encode` says this build has one
/// for the backend. For decode, `auto` reports why the device is unavailable
/// and continues in software, while `hardware-required` refuses to start a
/// run it cannot satisfy. A device only encode wanted is best-effort: without
/// it every export takes the readback path and says why. `excluded` is why
/// the device must not be made at all ([`backend_excluded`]), reported as an
/// unavailable one is.
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

/// Lets a termination request cancel the running export; without it, the
/// process ends where it stands, and the next export to the same output takes
/// over the partial file it leaves.
fn install_termination() {
    if let Err(error) = termination::install() {
        alpha2_os::diagnostic!(
            "alpha2-host: termination requests will end the process at once: {error}"
        );
    }
}

/// Serves requests on standard input until it ends, leaving the GPU host it
/// made, if any, in `host`.
fn serve(host: &mut Option<Host>) -> Result<(), HostError> {
    install_termination();
    let policy = match VideoDecodePolicy::from_env() {
        Ok(policy) => policy,
        Err(message) => {
            return Err(write_fatal(
                HostError::invalid_input(message),
                &mut std::io::stdout().lock(),
            )?);
        }
    };
    // Read now, before an upgrade can replace the file this process runs.
    if let Err(reason) = crate::host::host_binary_digest() {
        alpha2_os::diagnostic!("alpha2-host render cache unavailable: {reason}");
    }
    let mut serving = Serving::new(policy);
    let served = serve_requests(
        &mut serving,
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
        &mut exit_if_unusable,
    );
    // Released by `main` once the outcome is out: input that ended is answered by the exit.
    *host = serving.host.take();
    served
}

/// Writes `error`, a start that failed, on `stdout` as the `fatal`
/// notification, and returns it for the process's exit.
fn write_fatal(error: HostError, stdout: &mut impl Write) -> Result<HostError, HostError> {
    let error = fatal_error(error);
    let fatal = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "type": "fatal",
        "error": error,
    });
    serde_json::to_writer(&mut *stdout, &fatal).map_err(HostError::json)?;
    stdout.write_all(b"\n").map_err(HostError::io)?;
    stdout.flush().map_err(HostError::io)?;
    Ok(error)
}

/// `error`, a start that failed, carrying the failure report of the export
/// the client was waiting on: a client classifies it as it would the same
/// code from a running host.
fn fatal_error(error: HostError) -> HostError {
    let failure = crate::failure::startup_failure_json(error.code, &error.message, error.evidence);
    error.with_failure(failure)
}

/// What `serve` answers with: the decode mode, and the GPU host, made when
/// the first request that needs one arrives.
///
/// `conform` reads media alone and is answered without one, so a machine
/// without a usable GPU (or, under `hardware-required`, a decode device) can
/// still check media.
struct Serving {
    policy: VideoDecodePolicy,
    host: Option<Host>,
    create: fn(VideoDecodePolicy) -> Result<Host, HostError>,
}

impl Serving {
    const fn new(policy: VideoDecodePolicy) -> Self {
        Self {
            policy,
            host: None,
            create: create_host,
        }
    }

    /// Serving on a host made already.
    #[cfg(test)]
    const fn started(host: Host) -> Self {
        Self {
            policy: host.decode_policy,
            host: Some(host),
            create: create_host,
        }
    }

    /// The GPU host, made now if this is the first request that needs it.
    fn host(&mut self) -> Result<&mut Host, HostError> {
        match &mut self.host {
            Some(host) => Ok(host),
            slot => Ok(slot.insert((self.create)(self.policy)?)),
        }
    }
}

/// Answers each request line of `input` on `stdout`, in order, until `input`
/// ends, calling `answered` after each answer.
///
/// The GPU host is made for the first request other than `conform`; when it
/// cannot be, the `fatal` notification is written and its error returned,
/// and the request that needed it, with any after it, goes unanswered.
fn serve_requests(
    serving: &mut Serving,
    input: &mut impl BufRead,
    stdout: &mut impl Write,
    answered: &mut dyn FnMut(Option<&Host>),
) -> Result<(), HostError> {
    let mut buffer = Vec::new();
    loop {
        let envelope = match read_request_line(input, MAX_REQUEST_LINE_BYTES, &mut buffer)
            .map_err(HostError::io)?
        {
            RequestLine::End => break,
            RequestLine::Line(line) if line.trim().is_empty() => continue,
            RequestLine::Line(line) => {
                let envelope = parse_request(line);
                let request_id = match &envelope {
                    Ok(request) => request.request_id().to_owned(),
                    Err(_) => request_id_from_line(line),
                };
                // A refused line answers an export unless it says it is another operation.
                let export = envelope.is_err()
                    && operation_from_line(line).is_none_or(|operation| operation == "export");
                (request_id, envelope, export)
            }
            RequestLine::NotUtf8 => (
                "unknown".to_owned(),
                Err(HostError::new(
                    "INVALID_REQUEST",
                    "a request line must be UTF-8 JSON",
                )),
                true,
            ),
            RequestLine::TooLong(bytes) => (
                "unknown".to_owned(),
                Err(HostError::new(
                    "INVALID_REQUEST",
                    format!(
                        "the request line is {bytes} bytes; the maximum is {MAX_REQUEST_LINE_BYTES}"
                    ),
                )),
                true,
            ),
        };
        let (request_id, envelope, refused_export) = envelope;
        let needs_host = match &envelope {
            Ok(request) => !matches!(request, crate::protocol::Request::Conform(_)),
            Err(_) => refused_export,
        };
        if needs_host && let Err(error) = serving.host() {
            return Err(write_fatal(error, stdout)?);
        }
        // Until the answer is written, a termination waits for this request instead of ending the
        // process; it ends once the guard goes.
        let _request = termination::terminations().begin_request();
        let response = match envelope {
            Ok(crate::protocol::Request::Conform(request)) => {
                crate::host::conform_media(request, serving.policy)
            }
            Ok(request) => serving.host()?.handle(request, &mut |progress| {
                let event = json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "type": "progress",
                    "requestId": request_id,
                    "phase": progress.phase.as_str(),
                    "completed": progress.completed,
                    "total": progress.total,
                });
                serde_json::to_writer(&mut *stdout, &event).map_err(HostError::json)?;
                stdout.write_all(b"\n").map_err(HostError::io)?;
                stdout.flush().map_err(HostError::io)
            }),
            Err(error) if refused_export => Err(serving.host()?.refuse_export(error)),
            Err(error) => Err(error),
        };
        let body = match response {
            Ok(value) => json!({
                "protocolVersion": PROTOCOL_VERSION,
                "requestId": request_id,
                "ok": true,
                "result": value,
            }),
            Err(error) => json!({
                "protocolVersion": PROTOCOL_VERSION,
                "requestId": request_id,
                "ok": false,
                "error": error,
            }),
        };
        serde_json::to_writer(&mut *stdout, &body).map_err(HostError::json)?;
        stdout.write_all(b"\n").map_err(HostError::io)?;
        stdout.flush().map_err(HostError::io)?;
        answered(serving.host.as_ref());
    }
    Ok(())
}

/// One line `serve` read.
enum RequestLine<'a> {
    Line(&'a str),
    /// A line of this many bytes, over the limit: skipped through its end,
    /// so the next line is read whole.
    TooLong(usize),
    NotUtf8,
    End,
}

/// Reads the next line of `input`, without its line ending, into `buffer`,
/// holding at most `limit` bytes of it. A line over the limit is read
/// through and discarded, so a sender that writes one too long is answered
/// and the request after it is read whole. The last line need not end in a
/// newline.
fn read_request_line<'a>(
    input: &mut impl BufRead,
    limit: usize,
    buffer: &'a mut Vec<u8>,
) -> std::io::Result<RequestLine<'a>> {
    buffer.clear();
    let mut length = 0_usize;
    let mut ended = false;
    loop {
        let available = match input.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            break;
        }
        let (taken, newline) = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or((available.len(), false), |at| (at + 1, true));
        let content = &available[..taken - usize::from(newline)];
        length = length.saturating_add(content.len());
        if length <= limit {
            buffer.extend_from_slice(content);
        }
        input.consume(taken);
        ended = true;
        if newline {
            break;
        }
    }
    if !ended {
        return Ok(RequestLine::End);
    }
    if length > limit {
        buffer.clear();
        return Ok(RequestLine::TooLong(length));
    }
    if buffer.last() == Some(&b'\r') {
        buffer.pop();
    }
    Ok(std::str::from_utf8(buffer).map_or(RequestLine::NotUtf8, RequestLine::Line))
}

fn write_stdout(value: &Value) -> Result<(), HostError> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, value).map_err(HostError::json)?;
    stdout.write_all(b"\n").map_err(HostError::io)?;
    stdout.flush().map_err(HostError::io)
}

/// A failed GPU call found in an error's chain.
enum GpuCall {
    /// Its status named this fault.
    Fault(alpha2_media::GpuFault),
    /// A native interop helper's call whose status names no fault.
    #[cfg_attr(not(feature = "hwdevice"), allow(dead_code))]
    Failed,
}

/// The first failed GPU call in `error`'s chain: a native interop helper's
/// own failure, or the engine and media errors that keep its fault.
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

/// `error`'s text, carrying the protocol code its type decides
/// ([`alpha2_engine::ErrorCode`], an output the encoder could not create, a
/// failed GPU call and what its status said) for [`HostError::classify`],
/// and, for a fault with no code of its own, the evidence its failure class
/// is read from ([`failure::evidence_tag`]).
fn error_message(error: impl std::error::Error + 'static) -> String {
    use alpha2_media::GpuFault;
    match gpu_call(&error) {
        Some(GpuCall::Fault(GpuFault::Timeout)) => {
            return format!("{}{}", protocol::gpu_timeout_tag(), error);
        }
        Some(GpuCall::Fault(GpuFault::DeviceLost)) => {
            return protocol::code_tag("GPU_DEVICE_LOST", error.to_string());
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
    // An output the encoder could not create: the file system's answer, not the encoder's, or
    // another export's that is writing it.
    match (&error as &dyn std::error::Error).downcast_ref::<alpha2_media::EncodeError>() {
        Some(alpha2_media::EncodeError::Output(_)) => {
            return protocol::code_tag("IO_ERROR", error.to_string());
        }
        Some(alpha2_media::EncodeError::OutputInUse(_)) => {
            return protocol::code_tag("OUTPUT_IN_USE", error.to_string());
        }
        _ => {}
    }
    match alpha2_engine::ErrorCode::of(&error) {
        Some(code) => protocol::code_tag(code.as_str(), error.to_string()),
        None => error.to_string(),
    }
}

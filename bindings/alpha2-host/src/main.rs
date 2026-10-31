//! Native project-rendering host and command-line entry point.
//!
//! # Process protocol v1
//!
//! `alpha2-host serve` exchanges one UTF-8 JSON object per line over standard
//! input and output; what it writes is ASCII, every other character escaped
//! as `\uXXXX`. Every message contains `protocolVersion: 1`. `serve` writes
//! `{protocolVersion, type: "ready"}` as its first line, before reading input;
//! a client may treat its absence within a bound as a start failure. Requests
//! contain a caller-unique string `requestId` and an `operation`. The v1
//! operations are `capabilities`, `conform` (which media each source would
//! read, without rendering), `export`, `still` (frames of a project to
//! PNG, as an export draws them) and `cancel`. Browser hosts preview through the wasm
//! engine; this process renders only exports and their frames. Successful responses are
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
//! The end of standard input means no more requests: the host answers what it
//! has read and exits.
//!
//! `{protocolVersion, requestId, operation: "cancel", target}` cancels the
//! request `target`, and is answered at once with `result: {target, state}`:
//! `running` when `target` is being handled (an export stops, removes its
//! partial output and is answered `CANCELLED`; the host goes on serving),
//! `pending` otherwise (`target`, read or started later, is answered
//! `CANCELLED` without running). `SIGTERM`, `SIGINT`, `SIGHUP` (a console
//! control event on Windows) and the exit of the process that started `serve`
//! cancel the running export the same way, and the process then exits; see
//! [`termination`]. Assets and outputs are local filesystem paths; transport
//! and staging belong to the caller.
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
mod encoder_refusal;
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
mod paths;
mod protocol;
mod providers;
mod recovery;
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
use std::sync::Arc;
use std::time::Duration;

use crate::cli::{conform_cli, export_cli, usage};
use crate::config::VideoDecodePolicy;
use crate::host::Host;
use crate::protocol::{HostError, operation_from_line, parse_request, request_id_from_line};
use crate::serve_io::{InputLine, RequestLines, Responder};
#[cfg(test)]
use crate::{
    protocol::MAX_REQUEST_LINE_BYTES,
    serve_io::{RequestLine, read_request_line},
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
/// The fewest decoder sessions an export keeps open, whatever the fewest
/// videos it reads at once.
const DEFAULT_DECODER_SESSIONS: usize = 8;
/// The most decoder sessions an export keeps open. Thirty-two 4K hardware
/// sessions open on the hardware it was measured on (an RTX 4090 with Vulkan
/// video), the only measurement: `VideoToolbox` and D3D12VA run under the same
/// cap unmeasured, and software sessions this many are bounded by memory
/// first.
const MAX_DECODER_SESSIONS: usize = 32;
/// What an export's open decoder sessions may hold together, by
/// [`config::decoder_session_bytes`]'s estimate: thirty-two 1080p software
/// sessions, or twelve 4K ones, at the default decoder threads.
const DEFAULT_DECODER_SESSION_BYTES: u64 = 4 * 1024 * 1024 * 1024;
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

/// The stack of the thread every command runs on: what the main thread has on
/// Linux and macOS, which Windows gives its main thread an eighth of.
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

/// Runs `command` on the `alpha2-main` thread, with [`COMMAND_STACK_BYTES`]
/// of stack on every OS, and waits for it.
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

/// [`exit_now`] without flushing stdout, for a thread that must not wait for
/// another's line: every line written was flushed whole when written
/// ([`Responder::line`]).
fn exit_unflushed(code: u8) -> ! {
    let _ = std::io::stderr().flush();
    #[cfg(feature = "hwdevice")]
    alpha2_os::exit_without_handlers(code);
    // Without a hardware device no thread is inside a GPU driver, and the handlers can run.
    #[cfg(not(feature = "hwdevice"))]
    std::process::exit(i32::from(code));
}

/// Runs the command the arguments name, leaving the GPU host it made, if
/// any, in `host` for [`conclude`] to release.
fn run(host: &mut Option<Host>) -> Result<(), HostError> {
    #[cfg(windows)]
    configure_dx12_compiler();
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
/// that is not UTF-8 ([`alpha2_os::utf8_arguments`]).
fn utf8_arguments(
    arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<Vec<String>, HostError> {
    alpha2_os::utf8_arguments(arguments).map_err(HostError::invalid_input)
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
    let gpu = pollster::block_on(GpuContext::request_product())
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

/// Makes every DX12 instance of this process compile with DXC: the DLL
/// `ALPHA2_DXC_PATH` names, else the `dxcompiler.dll` beside the executable,
/// where a release ships it. A DLL that does not load fails the DX12 backend,
/// which the host reports and refuses to start on; it never falls back to FXC,
/// whose first frame costs seconds, or to another backend.
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

/// Makes the host that `ALPHA2_RENDERER` (default `auto`) asks for,
/// decoding as `policy` says.
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
    create_host_on(
        policy,
        renderer,
        &crate::adapter_choice::AdapterPolicy::from_env(),
    )
}

/// [`create_host_with`] on the adapter `adapter_policy` chooses.
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

/// The host error for a product device that could not be opened: a pin that
/// names no adapter is the request's error; a DX12 backend that did not
/// start names its shader compiler and is an installation defect, which no
/// renderer policy works around (`details.cause` is `shader-compiler`);
/// anything else is the GPU's.
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

/// `details.cause` of a `GPU_INIT_FAILED` whose DX12 shader compiler did not load.
const SHADER_COMPILER_CAUSE: &str = "shader-compiler";

/// A GPU host on the adapter `adapter_policy` chooses ([`alpha2_gpu::rank_adapters`]): on
/// Windows among the DX12 adapters, on Linux among the Vulkan adapters that could carry the
/// device shared with the video decoder and encoder, on macOS its one Metal device. `software`
/// says whether a software adapter is acceptable: `auto` refuses one and renders on the CPU
/// instead.
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
    // Windows renders on the scan's DX12 device or refuses to start; it never asks for another.
    #[cfg(not(all(target_os = "windows", feature = "hwdevice")))]
    let request_default = || {
        pollster::block_on(GpuContext::request_product_with(
            &alpha2_gpu::ProductRequest {
                software,
                ..alpha2_gpu::ProductRequest::default()
            },
        ))
        .map_err(gpu_init_error)
    };
    // The renderer goes on the shared Vulkan device, so a non-Vulkan backend skips it. The
    // adapter choice ranks the Vulkan adapters and makes the device on the first that carries it.
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
    // D3D12VA shares the renderer's device, so the renderer must be on DX12: left to itself wgpu
    // prefers Vulkan on Windows, which would silently turn hardware decode off. The scan ranks
    // the DX12 adapters by what each encodes and decodes (`ALPHA2_GPU_ADAPTER` overrides).
    #[cfg(all(target_os = "windows", feature = "hwencode"))]
    crate::d3d12va_probe::install();
    #[cfg(all(target_os = "windows", feature = "hwdevice"))]
    let decode_on_gpu = cfg!(feature = "hwdecode") && policy.allows_hardware();
    #[cfg(all(target_os = "windows", feature = "hwdevice"))]
    let encode_on_gpu = gpu_encode_wanted();
    #[cfg(all(target_os = "windows", feature = "hwdevice"))]
    let (mut dx12, dx12_unavailable) = {
        // The renderer is on DX12 or nowhere: rendering on another backend would give other bytes
        // and no hardware decode, so a DX12 that does not start refuses the host.
        let scanned = pollster::block_on(crate::dx12_adapters::scan(
            adapter_policy,
            decode_on_gpu,
            encode_on_gpu,
        ));
        match scanned {
            Ok(choice) => (choice, None::<String>),
            // A pin that names no adapter refuses to start under every mode.
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
    // The adapter's device memory budget, where its graphics API reports one: reported, and the
    // idle pools are held to a quarter of it.
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
        host.hardware_decode_unavailable = unavailable;
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
    host.encode_adapters =
        crate::adapter_choice::EncodeAdapterNotes::new(no_encode_device, Some(&report));
    host.adapter_choice = Some(report);
    #[cfg(all(target_os = "windows", feature = "hwencode"))]
    if let Some(test_encodes) = test_encodes {
        test_encodes.finish();
    }
    #[cfg(all(target_os = "windows", feature = "hwdevice"))]
    report_dx12_encode(&host, encode_on_gpu);
    report_adapter_choice(&host);
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
    alpha2_os::diagnostic!(
        "alpha2-host {}",
        openh264_startup_status(alpha2_native::openh264_status())
    );
}

/// `OpenH264` for the startup line: whether it is there, and if not, what to
/// do about it, since an H.264 export without a hardware encoder needs it.
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

/// Whether this host may encode on the GPU: a build with the GPU-resident
/// export, and `ALPHA2_GPU_ENCODE` not off. Only then is a device made for
/// encode alone, and a test encode run.
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
fn gpu_encode_wanted() -> bool {
    #[cfg(feature = "hwencode")]
    return crate::config::configured_gpu_encode(None).is_ok();
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

/// Says on stderr what the GPU-resident path encodes on the Windows
/// adapter the renderer is on, test encode included: the encoders it takes,
/// or why it takes none.
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

/// Says on stderr which adapter the renderer is on, under which policy and
/// why, and what every adapter passed over does.
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

/// Why a host has no device shared with a hardware encoder, when `missing`
/// says it has none and this build encodes on the GPU: `reason`, what making
/// the device answered.
#[cfg(all(any(target_os = "linux", target_os = "macos"), feature = "hwdevice"))]
fn missing_encode_device(missing: bool, reason: Option<String>) -> Option<String> {
    (missing && cfg!(feature = "hwencode"))
        .then(|| reason.unwrap_or_else(|| "no shared device was made".to_owned()))
}

/// `report`, the choice made among the adapters that could carry a shared
/// device, made to name the adapter `host` renders on. Without a shared
/// device the renderer is the product device (`ALPHA2_GPU_ADAPTER`'s pin, or
/// the high-performance order), which the report then names, with `why`
/// there is no shared device.
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

/// `report`, with the reason Windows rendered by the high-performance order when that was only
/// the Windows default (`ALPHA2_GPU_ADAPTER` unset), and where `auto` would have rendered:
/// the user learns what they gave up and how to ask for it.
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
         because an Intel Iris Xe draws leftover memory into DX12 textures with this build's \
         wgpu-hal{hint}",
        report.explanation
    );
    report
}

/// The macOS choice: the one Metal device, what `VideoToolbox` decodes into
/// it and the GPU-resident path encodes from it, or `why` neither does.
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
    if let Err(error) = alpha2_os::watch_parent() {
        alpha2_os::diagnostic!(
            "alpha2-host: the exit of the process that started this host goes unnoticed: {error}"
        );
    }
    let out = Arc::new(Responder::stdout());
    serve_io::write_ready(&out)?;
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
    let lines = serve_io::start_reader(Arc::clone(&out))?;
    let mut serving = Serving::new(policy);
    let served = serve_requests(&mut serving, &lines, &out, &mut exit_if_unusable);
    // Released by `main` once the outcome is out: input that ended is answered by the exit.
    *host = serving.host.take();
    served
}

/// Writes `error`, a start that failed, on `out` as the `fatal`
/// notification, and returns it for the process's exit.
fn write_fatal(error: HostError, out: &Responder) -> Result<HostError, HostError> {
    let error = fatal_error(error);
    out.line(&json!({
        "protocolVersion": PROTOCOL_VERSION,
        "type": "fatal",
        "error": error,
    }))?;
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

/// Answers each request line of `lines` on `out`, in order, until the input
/// ends, calling `answered` after each answer.
///
/// The GPU host is made for the first request other than `conform`; when it
/// cannot be, the `fatal` notification is written and its error returned,
/// and the request that needed it, with any after it, goes unanswered. A
/// request a `cancel` named before it started is answered `CANCELLED`
/// without running.
fn serve_requests(
    serving: &mut Serving,
    lines: &RequestLines,
    out: &Responder,
    answered: &mut dyn FnMut(Option<&Host>),
) -> Result<(), HostError> {
    loop {
        let envelope = match lines.next() {
            InputLine::End => break,
            InputLine::Line(line) if line.trim().is_empty() => continue,
            InputLine::Line(line) => {
                let envelope = parse_request(&line);
                let request_id = match &envelope {
                    Ok(request) => request.request_id().to_owned(),
                    Err(_) => request_id_from_line(&line),
                };
                // A refused line answers an export unless it says it is another operation.
                let export = envelope.is_err()
                    && operation_from_line(&line).is_none_or(|operation| operation == "export");
                (request_id, envelope, export)
            }
            InputLine::NotUtf8 => (
                "unknown".to_owned(),
                Err(HostError::new(
                    "INVALID_REQUEST",
                    "a request line must be UTF-8 JSON",
                )),
                true,
            ),
            InputLine::TooLong(bytes) => (
                "unknown".to_owned(),
                Err(HostError::new(
                    "INVALID_REQUEST",
                    format!(
                        "the request line is {bytes} bytes; the maximum is {}",
                        crate::protocol::MAX_REQUEST_LINE_BYTES
                    ),
                )),
                true,
            ),
        };
        let (request_id, envelope, refused_export) = envelope;
        let is_export = match &envelope {
            Ok(request) => matches!(request, crate::protocol::Request::Export(_)),
            Err(_) => refused_export,
        };
        // Cancelled while queued: answered without making a GPU host for it.
        if envelope.is_ok() && termination::terminations().take_pending_cancel(&request_id) {
            write_answer(out, &request_id, Err(cancelled_before_start(is_export)))?;
            answered(serving.host.as_ref());
            continue;
        }
        let needs_host = match &envelope {
            Ok(request) => !matches!(request, crate::protocol::Request::Conform(_)),
            Err(_) => refused_export,
        };
        if needs_host && let Err(error) = serving.host() {
            return Err(write_fatal(error, out)?);
        }
        // Until the answer is written, a termination waits for this request instead of ending the
        // process; it ends once the guard goes.
        let guard = termination::terminations().begin_request(&request_id);
        let response = match envelope {
            // Cancelled while its GPU host was made.
            Ok(_) if guard.cancelled_before_start() => Err(cancelled_before_start(is_export)),
            Ok(crate::protocol::Request::Conform(request)) => {
                crate::host::conform_media(request, serving.policy)
            }
            Ok(request) => serving.host()?.handle(request, &mut |progress| {
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
                out.line(&event)
            }),
            Err(error) if refused_export => Err(serving.host()?.refuse_export(error)),
            Err(error) => Err(error),
        };
        write_answer(out, &request_id, response)?;
        answered(serving.host.as_ref());
    }
    Ok(())
}

/// Writes the answer to request `request_id` on `out`.
fn write_answer(
    out: &Responder,
    request_id: &str,
    response: Result<Value, HostError>,
) -> Result<(), HostError> {
    out.line(&match response {
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
    })
}

/// The answer to a request a `cancel` named before it started; an export's
/// carries the failure report every failed export has.
fn cancelled_before_start(is_export: bool) -> HostError {
    let error = HostError::new("CANCELLED", "the request was cancelled before it started");
    if is_export {
        let failure = crate::failure::startup_failure_json(error.code, &error.message, None);
        error.with_failure(failure)
    } else {
        error
    }
}

/// Writes `value`, a command's report, as one line on standard output.
fn write_stdout(value: &Value) -> Result<(), HostError> {
    Responder::stdout().line(value)
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
    // An output the encoder could not create: the file system's answer, not the encoder's, or
    // another export's that is writing it.
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

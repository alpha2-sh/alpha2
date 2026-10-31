//! The D3D12VA test encode, run in a child process of this host.
//!
//! A D3D12 Video Encode device that says it encodes a codec is still held to
//! one encoded frame before an export is offered the GPU path
//! ([`alpha2_native::probe_d3d12va_encode`]). That encode runs inside the
//! vendor's driver, and a driver can crash there: an Intel integrated GPU on
//! driver 30.0.101.2079 hangs or crashes inside `FFmpeg`'s D3D12VA encode
//! submission about once in a few thousand frames, with plain `FFmpeg` as
//! with this host. A thread cannot contain a crash, so the encode runs in
//! `alpha2-host probe-d3d12va-encode`, on the renderer's own adapter, and
//! this process only reads its answer: one JSON line, or a crash, an exit,
//! or no answer within the GPU wait timeout, after which the child is
//! killed. Each of those is a refusal that names the adapter, the codec and
//! what happened, and the export takes the readback path.
//!
//! The answer is kept for the process by adapter LUID and codec, and asked
//! only when the GPU path could be: a build with the GPU-resident export,
//! `ALPHA2_GPU_ENCODE` not off, a shared device, and a codec the device
//! says it encodes. The host asks for every such codec as it starts, all at
//! once, so its start waits for one child, not one per codec.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use alpha2_gpu::GpuContext;
use alpha2_native::{D3D12EncodeProbe, NativeVideoCodec};
use alpha2_native_gpu::{D3D12EncodeCodec, SharedD3D12Context};
use serde_json::{Value, json};

use crate::protocol::HostError;

/// The hidden subcommand the child runs.
pub const SUBCOMMAND: &str = "probe-d3d12va-encode";

/// How long a killed child may take to go before it is left to the system:
/// one stuck inside a driver call ends only once that call returns.
const KILL_GRACE: Duration = Duration::from_secs(1);

/// How often the parent looks at whether the child has exited.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// A test encode's answer, by adapter LUID and codec.
type Answer = ((u64, NativeVideoCodec), Result<D3D12EncodeProbe, String>);

/// Every answer this process has had.
static ANSWERS: Mutex<Vec<Answer>> = Mutex::new(Vec::new());

/// What D3D12 Video Encode is asked about for `codec`.
pub const fn d3d12_codec(codec: NativeVideoCodec) -> Option<D3D12EncodeCodec> {
    match codec {
        NativeVideoCodec::H264 => Some(D3D12EncodeCodec::H264),
        NativeVideoCodec::Hevc => Some(D3D12EncodeCodec::Hevc),
        NativeVideoCodec::Av1 => Some(D3D12EncodeCodec::Av1),
        _ => None,
    }
}

/// Whether `shared`'s device reports a D3D12 video encoder for `codec` that
/// this runtime registers, or why not: the answers that cost no encode.
pub fn device_encodes(shared: &SharedD3D12Context, codec: NativeVideoCodec) -> Result<(), String> {
    alpha2_native::d3d12va_encoder(codec)?;
    let asked = d3d12_codec(codec)
        .ok_or_else(|| format!("D3D12 Video Encode does not encode {}", codec.as_str()))?;
    shared.encode_support(asked).map(|_| ())
}

/// Makes the child process what runs every test encode of this process.
pub fn install() {
    // A second host of the same process (a test's) finds the runner in place.
    let _ = alpha2_native::set_d3d12va_test_encode_runner(test_encode);
}

/// The runner [`install`] puts in place: the answer this process has for
/// `shared`'s adapter and `codec`, or the child's, asked now.
fn test_encode(
    shared: &SharedD3D12Context,
    codec: NativeVideoCodec,
) -> Result<D3D12EncodeProbe, String> {
    let luid = shared
        .adapter()
        .ok_or_else(|| "the renderer's adapter has no DXGI identity".to_owned())?
        .luid;
    // Held across the child, so one adapter and codec is only ever asked once.
    let mut answers = ANSWERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((_, answer)) = answers.iter().find(|(key, _)| *key == (luid, codec)) {
        return answer.clone();
    }
    let answer = run_child(luid, codec, alpha2_native_gpu::d3d12_wait_timeout(), None);
    answers.push(((luid, codec), answer.clone()));
    answer
}

/// The test encodes a starting host runs while it does the rest of its
/// start, one child per codec.
pub struct Prewarm(Vec<std::thread::JoinHandle<Answer>>);

/// Starts asking the child about every codec in `codecs` this process has
/// no answer for, on `shared`'s adapter, all at once and each bounded by
/// `timeout`; [`Prewarm::finish`] waits for the answers.
///
/// A child spends most of its time opening the adapter's device (`wgpu`
/// asks every DX12 adapter, which wakes a sleeping discrete GPU), so it
/// runs while the host makes its renderer and probes its encoders.
pub fn start_prewarm(
    shared: &SharedD3D12Context,
    codecs: &[NativeVideoCodec],
    timeout: Duration,
) -> Prewarm {
    let Some(identity) = shared.adapter() else {
        return Prewarm(Vec::new());
    };
    let luid = identity.luid;
    let answers = ANSWERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Prewarm(
        codecs
            .iter()
            .copied()
            .filter(|codec| !answers.iter().any(|(key, _)| *key == (luid, *codec)))
            .filter_map(|codec| {
                std::thread::Builder::new()
                    .name(format!("alpha2 {} test encode", codec.as_str()))
                    .spawn(move || ((luid, codec), run_child(luid, codec, timeout, None)))
                    .ok()
            })
            .collect(),
    )
}

impl Prewarm {
    /// Waits for every answer and keeps it for the process. Each child is
    /// bounded by its timeout, so this is too.
    pub fn finish(self) {
        let answered: Vec<Answer> = self
            .0
            .into_iter()
            .filter_map(|child| child.join().ok())
            .collect();
        let mut answers = ANSWERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (key, answer) in answered {
            if !answers.iter().any(|(known, _)| *known == key) {
                answers.push((key, answer));
            }
        }
    }
}

/// Runs the test encode of `codec` on the adapter `luid` names in a child
/// process, and returns its answer, or what became of it: a crash, an exit
/// without an answer, or no answer within `timeout`, after which it is
/// killed. `fault`, in a test, makes the child fail as it says.
fn run_child(
    luid: u64,
    codec: NativeVideoCodec,
    timeout: Duration,
    fault: Option<&str>,
) -> Result<D3D12EncodeProbe, String> {
    run_child_at(luid, codec, None, timeout, fault).0
}

/// The test encode of `codec` at `size` on the adapter `luid` names, in a
/// child, for `probe-gpu-encode`: [`run_child`]'s answer, and the last line
/// the child wrote to its standard error, which is where `FFmpeg` logs the
/// `HRESULT` behind a refusal.
pub fn probe_child(
    luid: u64,
    codec: NativeVideoCodec,
    size: (u32, u32),
    timeout: Duration,
) -> (Result<D3D12EncodeProbe, String>, Option<String>) {
    run_child_at(luid, codec, Some(size), timeout, None)
}

/// [`run_child`] at `size` (the test encode's own without one), and the
/// child's last line on standard error.
fn run_child_at(
    luid: u64,
    codec: NativeVideoCodec,
    size: Option<(u32, u32)>,
    timeout: Duration,
    fault: Option<&str>,
) -> (Result<D3D12EncodeProbe, String>, Option<String>) {
    let mut command = match child_command(luid, codec, size, fault) {
        Ok(command) => command,
        Err(error) => return (Err(error), None),
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    {
        // A host started without a console (a desktop app's) must not open one for its child.
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let started = Instant::now();
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return (
                Err(format!("the D3D12VA test encode did not start: {error}")),
                None,
            );
        }
    };
    // A child left behind when this host is killed would hold its device and its pipes.
    if let Some(job) = children_job() {
        let _ = job.assign(&child);
    }
    // Read as it comes, so a child that writes more than a pipe holds is not blocked by its parent.
    let stdout = read_in_background(child.stdout.take());
    let stderr = read_in_background(child.stderr.take());
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() >= timeout => break None,
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(error) => {
                return (
                    Err(format!("the D3D12VA test encode was lost: {error}")),
                    None,
                );
            }
        }
    };
    let Some(status) = status else {
        let _ = child.kill();
        let killed = Instant::now();
        while killed.elapsed() < KILL_GRACE && matches!(child.try_wait(), Ok(None)) {
            std::thread::sleep(POLL_INTERVAL);
        }
        return (
            Err(format!(
                "the D3D12VA test encode did not finish within {timeout:?} and was killed"
            )),
            None,
        );
    };
    let stdout = joined(stdout);
    let last_line = joined(stderr)
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| truncated(line, 240));
    let answer = stdout.lines().rev().find_map(|line| {
        serde_json::from_str::<Value>(line.trim())
            .ok()
            .filter(|value| value.get("ok").is_some())
    });
    let answer = match (status.code(), answer) {
        (Some(0), Some(answer)) => answer_from_json(&answer),
        (code, _) => {
            let last_words = last_line
                .as_ref()
                .map(|line| format!("; it last wrote: {line}"))
                .unwrap_or_default();
            Err(match code {
                // An NTSTATUS: an access violation (0xC0000005), a fast fail (0xC0000409).
                Some(code) if code.cast_unsigned() >= 0xC000_0000 => format!(
                    "the D3D12VA test encode crashed (exit {:#010X}){last_words}",
                    code.cast_unsigned()
                ),
                Some(code) => {
                    format!("the D3D12VA test encode exited with {code} and no answer{last_words}")
                }
                None => format!("the D3D12VA test encode ended without an exit code{last_words}"),
            })
        }
    };
    (answer, last_line)
}

/// The job every child this host starts is put in, which ends them with the
/// host; `None` when the system refused one.
fn children_job() -> Option<&'static alpha2_os::KillOnCloseJob> {
    static JOB: std::sync::OnceLock<Option<alpha2_os::KillOnCloseJob>> = std::sync::OnceLock::new();
    JOB.get_or_init(|| alpha2_os::KillOnCloseJob::new().ok())
        .as_ref()
}

/// The command that runs the child: this executable's subcommand, or in a
/// test this test binary's [`TEST_CHILD`] test.
#[cfg_attr(not(test), allow(clippy::unnecessary_wraps))]
fn child_command(
    luid: u64,
    codec: NativeVideoCodec,
    size: Option<(u32, u32)>,
    fault: Option<&str>,
) -> Result<Command, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("the D3D12VA test encode has no executable to run: {error}"))?;
    let arguments = child_arguments(luid, codec, size);
    let mut command = Command::new(executable);
    #[cfg(not(test))]
    {
        let _ = fault;
        command.arg(SUBCOMMAND).args(arguments);
    }
    #[cfg(test)]
    {
        command
            .args([
                "--exact",
                TEST_CHILD,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(TEST_CHILD_ARGUMENTS, arguments.join(" "));
        if let Some(fault) = fault {
            command.env(TEST_CHILD_FAULT, fault);
        }
    }
    Ok(command)
}

fn child_arguments(luid: u64, codec: NativeVideoCodec, size: Option<(u32, u32)>) -> Vec<String> {
    let mut arguments = vec![
        "--adapter".to_owned(),
        format!("{luid:016X}"),
        "--codec".to_owned(),
        codec.as_str().to_owned(),
    ];
    if let Some((width, height)) = size {
        arguments.extend(["--size".to_owned(), format!("{width}x{height}")]);
    }
    arguments
}

/// The answer this process has for the adapter `luid` names and `codec`,
/// if it has asked.
#[cfg(test)]
pub fn answer(luid: u64, codec: NativeVideoCodec) -> Option<Result<D3D12EncodeProbe, String>> {
    ANSWERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|(key, _)| *key == (luid, codec))
        .map(|(_, answer)| answer.clone())
}

/// The test that stands in for the subcommand in a test binary.
#[cfg(test)]
pub const TEST_CHILD: &str = "tests::d3d12va_encode::d3d12va_test_encode_child";

/// The subcommand's arguments, for [`TEST_CHILD`].
#[cfg(test)]
pub const TEST_CHILD_ARGUMENTS: &str = "ALPHA2_TEST_D3D12VA_CHILD";

/// How [`TEST_CHILD`] fails instead of answering: `crash`, `hang`, or
/// `refuse`.
#[cfg(test)]
pub const TEST_CHILD_FAULT: &str = "ALPHA2_TEST_D3D12VA_CHILD_FAULT";

fn read_in_background<R: Read + Send + 'static>(
    pipe: Option<R>,
) -> Option<std::thread::JoinHandle<String>> {
    let mut pipe = pipe?;
    std::thread::Builder::new()
        .name("alpha2 D3D12VA probe pipe".to_owned())
        .spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        })
        .ok()
}

/// What the pipe held, once the child that wrote it has exited.
fn joined(reader: Option<std::thread::JoinHandle<String>>) -> String {
    reader
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default()
}

fn truncated(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((at, _)) => format!("{}...", &text[..at]),
        None => text.to_owned(),
    }
}

/// The subcommand: `probe-d3d12va-encode --adapter <LUID> --codec <codec>
/// [--size <W>x<H>]`.
///
/// Opens the renderer's device on the adapter the LUID names, binds
/// `FFmpeg`'s D3D12VA context to it, encodes one frame as the parent's
/// capability would (at [`alpha2_native::D3D12VA_TEST_ENCODE_SIZE`] unless
/// `probe-gpu-encode` names a size), and writes the answer as one JSON line:
/// `{"ok": true, "result": {...}}` or `{"ok": false, "error": "..."}`. It
/// exits 0 either way: a nonzero exit is a crash, which is what the parent
/// tells apart.
///
/// # Errors
/// Arguments it cannot read, or an answer it cannot write.
pub fn child(args: &[String]) -> Result<(), HostError> {
    let (luid, codec, size) = parse_arguments(args)?;
    alpha2_native_gpu::set_gpu_wait_timeout(crate::config::configured_gpu_wait_timeout());
    let answer = encode_here(luid, codec, size);
    crate::write_stdout(&answer_json(&answer))
}

/// [`child`] in a test binary, from [`TEST_CHILD_ARGUMENTS`], failing as
/// [`TEST_CHILD_FAULT`] says.
#[cfg(test)]
pub fn child_test_main(arguments: &str) {
    let args: Vec<String> = arguments.split_whitespace().map(str::to_owned).collect();
    match std::env::var(TEST_CHILD_FAULT).ok().as_deref() {
        Some("crash") => std::process::abort(),
        Some("hang") => std::thread::sleep(Duration::from_secs(600)),
        Some("refuse") => {
            println!();
            println!(
                "{}",
                answer_json(&Err(
                    "Encoding failed: Generic error in an external library (avcodec_open2: \
                     CreateVideoEncoder returned E_INVALIDARG, injected)"
                        .to_owned()
                ))
            );
        }
        _ => {
            let answer = parse_arguments(&args)
                .map_err(|error| error.message)
                .and_then(|(luid, codec, size)| encode_here(luid, codec, size));
            println!();
            println!("{}", answer_json(&answer));
        }
    }
}

/// The adapter, the codec and the size, if one is given, the subcommand's
/// arguments name.
type ChildArguments = (u64, NativeVideoCodec, Option<(u32, u32)>);

fn parse_arguments(args: &[String]) -> Result<ChildArguments, HostError> {
    let usage = || {
        HostError::invalid_input(format!(
            "usage: alpha2-host {SUBCOMMAND} --adapter <LUID, 16 hex digits> --codec \
             <h264|hevc|av1> [--size <width>x<height>]"
        ))
    };
    let (adapter_flag, luid, codec_flag, codec, size) = match args {
        [adapter_flag, luid, codec_flag, codec] => (adapter_flag, luid, codec_flag, codec, None),
        [adapter_flag, luid, codec_flag, codec, size_flag, size] if size_flag == "--size" => {
            let size = size
                .split_once('x')
                .and_then(|(width, height)| Some((width.parse().ok()?, height.parse().ok()?)))
                .filter(|&(width, height): &(u32, u32)| width > 0 && height > 0)
                .ok_or_else(usage)?;
            (adapter_flag, luid, codec_flag, codec, Some(size))
        }
        _ => return Err(usage()),
    };
    if adapter_flag != "--adapter" || codec_flag != "--codec" {
        return Err(usage());
    }
    let luid = u64::from_str_radix(luid, 16).map_err(|_| usage())?;
    let codec = crate::adapter_choice::ENCODE_CODECS
        .into_iter()
        .find(|known| known.as_str() == codec)
        .ok_or_else(usage)?;
    Ok((luid, codec, size))
}

/// The test encode of `codec` on the adapter `luid` names, in this process,
/// at `size` or the test encode's own.
fn encode_here(
    luid: u64,
    codec: NativeVideoCodec,
    size: Option<(u32, u32)>,
) -> Result<D3D12EncodeProbe, String> {
    let adapter = pollster::block_on(GpuContext::adapters_by_preference_on(wgpu::Backends::DX12))
        .into_iter()
        .map(|(_, adapter)| adapter)
        .find(|adapter| {
            alpha2_native_gpu::d3d12_adapter_identity(adapter)
                .is_some_and(|identity| identity.luid == luid)
        })
        .ok_or_else(|| format!("no DX12 adapter has the LUID {luid:016X}"))?;
    let gpu = pollster::block_on(GpuContext::open_adapter(adapter))
        .map_err(|error| format!("the adapter's device did not open: {error}"))?;
    let shared = SharedD3D12Context::from_gpu(gpu)
        .map_err(|error| format!("the D3D12VA device did not open: {error}"))?;
    let (width, height) = size.unwrap_or(alpha2_native::D3D12VA_TEST_ENCODE_SIZE);
    alpha2_native::probe_d3d12va_encode(&shared, codec, width, height)
        .map_err(|error| error.to_string())
}

fn answer_json(answer: &Result<D3D12EncodeProbe, String>) -> Value {
    match answer {
        Ok(probe) => json!({"ok": true, "result": probe_json(probe)}),
        Err(error) => json!({"ok": false, "error": error}),
    }
}

/// What one test encode found, as the child answers it and
/// `probe-gpu-encode` reports it.
pub fn probe_json(probe: &D3D12EncodeProbe) -> Value {
    json!({
        "encoder": probe.encoder,
        "videoCodec": probe.codec.as_str(),
        "rateControl": probe.rate_control,
        "codedSize": [probe.coded_size.0, probe.coded_size.1],
        "openedProfile": probe.opened_profile,
        "extradataSize": probe.extradata_size,
        "profile": probe.profile,
        "firstPacketUnits": probe.first_packet_units,
        "packets": probe.packets,
        "packetBytes": probe.packet_bytes,
        "decodeFailure": probe.decode_failure,
    })
}

/// The probe a child's answer describes, or the error it answered with.
fn answer_from_json(answer: &Value) -> Result<D3D12EncodeProbe, String> {
    if answer["ok"] != json!(true) {
        return Err(answer["error"]
            .as_str()
            .unwrap_or("the D3D12VA test encode answered with no reason")
            .to_owned());
    }
    let result = &answer["result"];
    let malformed = || format!("the D3D12VA test encode's answer is malformed: {answer}");
    let number = |field: &str| {
        result[field]
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(malformed)
    };
    let codec = crate::adapter_choice::ENCODE_CODECS
        .into_iter()
        .find(|codec| result["videoCodec"].as_str() == Some(codec.as_str()))
        .ok_or_else(malformed)?;
    let rate_control = ["disabled", "cbr", "vbr", "qvbr"]
        .into_iter()
        .find(|mode| result["rateControl"].as_str() == Some(mode))
        .ok_or_else(malformed)?;
    let extent = result["codedSize"].as_array().ok_or_else(malformed)?;
    let side = |at: usize| {
        extent
            .get(at)
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(malformed)
    };
    Ok(D3D12EncodeProbe {
        encoder: result["encoder"].as_str().ok_or_else(malformed)?.to_owned(),
        codec,
        rate_control,
        coded_size: (side(0)?, side(1)?),
        opened_profile: result["openedProfile"]
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(malformed)?,
        extradata_size: usize::try_from(number("extradataSize")?).map_err(|_| malformed())?,
        profile: result["profile"]
            .as_u64()
            .and_then(|value| u8::try_from(value).ok()),
        first_packet_units: result["firstPacketUnits"]
            .as_array()
            .ok_or_else(malformed)?
            .iter()
            .map(|unit| {
                unit.as_u64()
                    .and_then(|value| u8::try_from(value).ok())
                    .ok_or_else(malformed)
            })
            .collect::<Result<_, _>>()?,
        packets: number("packets")?,
        packet_bytes: number("packetBytes")?,
        decode_failure: match &result["decodeFailure"] {
            Value::Null => None,
            failure => Some(failure.as_str().ok_or_else(malformed)?.to_owned()),
        },
    })
}

/// A refusal a test forces at its step, on whatever adapter the host is
/// on: what the device's own answers or the test encode would have said.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InjectedRefusal {
    /// The runtime registers no encoder for the codec.
    NoEncoder,
    /// The device reports no D3D12 video encoder for the codec.
    NoDeviceEncoder,
    /// The OS or driver has no `ID3D12VideoDevice3`.
    NoVideoDevice,
    /// The session does not open in the test encode: the child says so.
    OpenFails,
    /// The test encode never answers: the child is killed at the timeout.
    Timeout,
    /// The test encode crashes the child.
    Crash,
}

/// A forced refusal, the bound on the test encode it runs, and the reasons
/// it has given by codec.
#[cfg(test)]
type Injected = (InjectedRefusal, Duration, Vec<(NativeVideoCodec, String)>);

#[cfg(test)]
thread_local! {
    /// The refusal the hosts this thread makes report.
    static INJECTED: std::cell::RefCell<Option<Injected>> = const { std::cell::RefCell::new(None) };
}

/// Forces `refusal` for every codec, on this thread, until the guard goes;
/// a test encode it runs is bounded by `timeout`.
#[cfg(test)]
pub fn inject(refusal: InjectedRefusal, timeout: Duration) -> InjectionGuard {
    INJECTED.with(|injected| *injected.borrow_mut() = Some((refusal, timeout, Vec::new())));
    InjectionGuard
}

/// Clears the refusal [`inject`] forced.
#[cfg(test)]
pub struct InjectionGuard;

#[cfg(test)]
impl Drop for InjectionGuard {
    fn drop(&mut self) {
        INJECTED.with(|injected| *injected.borrow_mut() = None);
    }
}

/// Whether a refusal is forced on this thread.
#[cfg(test)]
pub fn injected() -> bool {
    INJECTED.with(|injected| injected.borrow().is_some())
}

/// The forced refusal of `codec` on `adapter`, from the step it is forced
/// at; `None` when none is forced. A test encode that is forced to fail is
/// run, in a child that fails that way, once per codec and thread.
#[cfg(test)]
pub fn injected_refusal(adapter: &str, luid: u64, codec: NativeVideoCodec) -> Option<String> {
    let (refusal, timeout, known) = INJECTED.with(|injected| injected.borrow().clone())?;
    if let Some((_, reason)) = known.iter().find(|(asked, _)| *asked == codec) {
        return Some(reason.clone());
    }
    let encoder = format!("{}_d3d12va", codec.as_str());
    let fault = match refusal {
        InjectedRefusal::NoEncoder => {
            return Some(format!("this FFmpeg runtime has no {encoder} encoder"));
        }
        InjectedRefusal::NoDeviceEncoder => {
            return Some(format!(
                "{adapter} reports no D3D12 video encoder for {}",
                codec.as_str()
            ));
        }
        InjectedRefusal::NoVideoDevice => {
            return Some(format!(
                "{adapter}: ID3D12VideoDevice3 unavailable (HRESULT 0x80004002, injected)"
            ));
        }
        InjectedRefusal::OpenFails => "refuse",
        InjectedRefusal::Timeout => "hang",
        InjectedRefusal::Crash => "crash",
    };
    let reason = match run_child(luid, codec, timeout, Some(fault)) {
        Ok(_) => return None,
        Err(error) => format!("{encoder} did not open and encode on {adapter}: {error}"),
    };
    INJECTED.with(|injected| {
        if let Some((_, _, known)) = injected.borrow_mut().as_mut() {
            known.push((codec, reason.clone()));
        }
    });
    Some(reason)
}

/// [`prewarm`] for a forced refusal: every codec's forced test encode, at
/// once, so a host's start waits for one.
#[cfg(test)]
pub fn prewarm_injected(adapter: &str, luid: u64) {
    let Some((refusal, timeout, _)) = INJECTED.with(|injected| injected.borrow().clone()) else {
        return;
    };
    let fault = match refusal {
        InjectedRefusal::OpenFails => "refuse",
        InjectedRefusal::Timeout => "hang",
        InjectedRefusal::Crash => "crash",
        _ => return,
    };
    let answered: Vec<(NativeVideoCodec, String)> = std::thread::scope(|scope| {
        let children: Vec<_> = crate::adapter_choice::ENCODE_CODECS
            .iter()
            .map(|&codec| {
                scope.spawn(move || {
                    let reason = run_child(luid, codec, timeout, Some(fault)).err()?;
                    Some((
                        codec,
                        format!(
                            "{}_d3d12va did not open and encode on {adapter}: {reason}",
                            codec.as_str()
                        ),
                    ))
                })
            })
            .collect();
        children
            .into_iter()
            .filter_map(|child| child.join().ok().flatten())
            .collect()
    });
    INJECTED.with(|injected| {
        if let Some((_, _, known)) = injected.borrow_mut().as_mut() {
            known.extend(answered);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child's answer survives the line it is written as.
    #[test]
    fn an_answer_reads_back_as_the_probe_it_was() {
        let probe = D3D12EncodeProbe {
            encoder: "hevc_d3d12va".to_owned(),
            codec: NativeVideoCodec::Hevc,
            rate_control: "disabled",
            coded_size: (256, 256),
            opened_profile: 1,
            extradata_size: 0,
            profile: Some(1),
            first_packet_units: vec![32, 33, 34, 19],
            packets: 1,
            packet_bytes: 4_321,
            decode_failure: Some(
                "the software hevc decoder decoded picture 0 of 1 corrupt".to_owned(),
            ),
        };
        let line = answer_json(&Ok(probe.clone())).to_string();
        let read = answer_from_json(&serde_json::from_str(&line).expect("json"));
        assert_eq!(read, Ok(probe.clone()));
        let decoded = D3D12EncodeProbe {
            decode_failure: None,
            ..probe
        };
        let line = answer_json(&Ok(decoded.clone())).to_string();
        let read = answer_from_json(&serde_json::from_str(&line).expect("json"));
        assert_eq!(read, Ok(decoded));
        let refused = answer_json(&Err("no".to_owned())).to_string();
        assert_eq!(
            answer_from_json(&serde_json::from_str(&refused).expect("json")),
            Err("no".to_owned())
        );
        assert!(
            answer_from_json(&json!({"ok": true, "result": {}}))
                .expect_err("malformed")
                .contains("malformed")
        );
    }

    #[test]
    fn the_subcommand_reads_its_arguments() {
        let args = |text: &str| text.split(' ').map(str::to_owned).collect::<Vec<_>>();
        let (luid, codec, size) =
            parse_arguments(&args("--adapter 00000000000AB12C --codec hevc")).expect("arguments");
        assert_eq!((luid, codec, size), (0xAB12C, NativeVideoCodec::Hevc, None));
        assert_eq!(
            child_arguments(luid, codec, None).join(" "),
            "--adapter 00000000000AB12C --codec hevc"
        );
        let sized = child_arguments(luid, codec, Some((1_920, 1_080)));
        assert_eq!(
            sized.join(" "),
            "--adapter 00000000000AB12C --codec hevc --size 1920x1080"
        );
        assert_eq!(
            parse_arguments(&sized).expect("arguments"),
            (luid, codec, Some((1_920, 1_080)))
        );
        for bad in [
            "--adapter 1 --codec vp9",
            "--adapter zz --codec h264",
            "--codec h264 --adapter 1",
            "--adapter 1",
            "--adapter 1 --codec h264 --size 0x2",
            "--adapter 1 --codec h264 --size 2",
            "--adapter 1 --codec h264 --width 2x2",
        ] {
            assert!(parse_arguments(&args(bad)).is_err(), "{bad}");
        }
    }
}

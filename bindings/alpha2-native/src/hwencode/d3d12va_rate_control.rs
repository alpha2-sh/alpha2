//! `probe-encoders`' view of the D3D12VA encoders: each rate-control mode
//! opened on a DX12 adapter's device and held to one encoded frame.
//!
//! An export asks the renderer's device what it encodes; this asks every DX12
//! adapter in turn, in the order the host considers them, and the first
//! whose D3D12 Video Encode reports the encoder answers for it. A mode that
//! device does not list is refused with its reason and never opened.

use std::sync::Mutex;

use alpha2_media::ColorMetadata;
use alpha2_native_gpu::SharedD3D12Context;
use rsmpeg::avcodec::AVCodecContext;
use rsmpeg::avutil::{AVFrame, ra};
use rsmpeg::error::RsmpegError;
use rsmpeg::ffi;

use super::{DeviceModes, Encoder, d3d12va_encode_frames};
use crate::NativeVideoCodec;
use crate::rate_control::RateControlMechanism;

/// The codecs D3D12 Video Encode encodes, in the order a report leads with.
const CODECS: [NativeVideoCodec; 3] = [
    NativeVideoCodec::H264,
    NativeVideoCodec::Hevc,
    NativeVideoCodec::Av1,
];

/// One open of a D3D12VA encoder with one rate-control request, on the
/// adapter its LUID names: what `probe-encoders` runs for each mode a device
/// lists ([`probe_d3d12va_rate_control`]), in whatever process a
/// [`D3D12RateControlRunner`] runs it in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct D3D12RateControlOpen {
    /// `DXGI_ADAPTER_DESC1::AdapterLuid` of the adapter the open runs on.
    pub adapter: u64,
    /// The codec whose D3D12VA encoder opens.
    pub codec: NativeVideoCodec,
    /// The output size.
    pub size: (u32, u32),
    /// `AVCodecContext::bit_rate`; zero for a quantizer mode.
    pub bit_rate: i64,
    /// The private and generic options that carry the mode, in order.
    pub options: Vec<(String, String)>,
}

impl D3D12RateControlOpen {
    /// The open as command-line arguments, which [`Self::parse`] reads back:
    /// `--adapter <LUID> --codec <codec> --size <W>x<H> --bit-rate <b>`, then
    /// one `--option <key>=<value>` per option.
    #[must_use]
    pub fn arguments(&self) -> Vec<String> {
        let mut arguments = vec![
            "--adapter".to_owned(),
            format!("{:016X}", self.adapter),
            "--codec".to_owned(),
            self.codec.as_str().to_owned(),
            "--size".to_owned(),
            format!("{}x{}", self.size.0, self.size.1),
            "--bit-rate".to_owned(),
            self.bit_rate.to_string(),
        ];
        for (key, value) in &self.options {
            arguments.push("--option".to_owned());
            arguments.push(format!("{key}={value}"));
        }
        arguments
    }

    /// The open [`Self::arguments`] wrote.
    ///
    /// # Errors
    /// Names the argument that is missing or unreadable.
    pub fn parse(arguments: &[String]) -> Result<Self, String> {
        if !arguments.len().is_multiple_of(2) {
            return Err("an argument has no value".to_owned());
        }
        let mut pairs = arguments.chunks(2);
        let mut value = |flag: &str| match pairs.next() {
            Some([given, value]) if given == flag => Ok(value.clone()),
            _ => Err(format!("expected {flag} <value>")),
        };
        let adapter = u64::from_str_radix(&value("--adapter")?, 16)
            .map_err(|_| "--adapter takes a LUID in hexadecimal".to_owned())?;
        let codec = value("--codec")?;
        let codec = CODECS
            .into_iter()
            .find(|known| known.as_str() == codec)
            .ok_or_else(|| format!("D3D12 Video Encode encodes h264, hevc or av1, not {codec}"))?;
        let size = value("--size")?;
        let size = size
            .split_once('x')
            .and_then(|(width, height)| Some((width.parse().ok()?, height.parse().ok()?)))
            .ok_or_else(|| format!("--size takes <width>x<height>, not {size}"))?;
        let bit_rate = value("--bit-rate")?
            .parse()
            .map_err(|_| "--bit-rate takes an integer".to_owned())?;
        let mut options = Vec::new();
        for _ in 0..arguments.len().saturating_sub(8) / 2 {
            let option = value("--option")?;
            let (key, value) = option
                .split_once('=')
                .ok_or_else(|| format!("--option takes <key>=<value>, not {option}"))?;
            options.push((key.to_owned(), value.to_owned()));
        }
        Ok(Self {
            adapter,
            codec,
            size,
            bit_rate,
            options,
        })
    }
}

/// Runs one [`D3D12RateControlOpen`] and returns the packets it drained, or
/// why it refused or gave no answer.
pub type D3D12RateControlRunner = fn(&D3D12RateControlOpen) -> Result<u32, String>;

/// What runs each rate-control open of `probe-encoders` instead of this
/// process.
static RUNNER: std::sync::OnceLock<D3D12RateControlRunner> = std::sync::OnceLock::new();

/// Makes `runner` what runs every open [`probe_d3d12va_rate_control`] makes,
/// from now on, in place of this process: the driver that can crash in a
/// test encode can crash in one of these, and only another process survives
/// that. The runner bounds its own wait.
///
/// # Errors
/// Returns `runner` when one is installed already: the first one stays.
pub fn set_d3d12va_rate_control_runner(
    runner: D3D12RateControlRunner,
) -> Result<(), D3D12RateControlRunner> {
    RUNNER.set(runner)
}

/// Makes every rate-control open of this test binary answer one packet
/// without encoding: the binary's frames are kept for the session tests,
/// since a driver can crash in any of them, and `alpha2 probe-encoders`'
/// own test runs the opens, in children.
#[cfg(test)]
pub(crate) fn encode_no_rate_control_frames() {
    #[allow(clippy::unnecessary_wraps)]
    fn answered(open: &D3D12RateControlOpen) -> Result<u32, String> {
        assert_ne!(open.adapter, 0, "an adapter another process can find");
        Ok(1)
    }
    let _ = RUNNER.set(answered);
}

/// One DX12 adapter as the probe found it.
struct ProbedAdapter {
    /// Its position in the backend's own enumeration, as
    /// `ALPHA2_GPU_ADAPTER` counts: `DX12 adapter <index>`. A reason that
    /// already names the adapter is prefixed with this alone.
    position: String,
    /// What a row that opened names it by: the position and its name.
    label: String,
    /// Its LUID, by which another process finds it again.
    luid: Option<u64>,
    /// The renderer's device on it, shared with `FFmpeg`, or why there is
    /// none.
    shared: Result<SharedD3D12Context, String>,
}

/// Every DX12 adapter, opened once for the process; `None` until the first
/// probe asks.
static ADAPTERS: Mutex<Option<Vec<ProbedAdapter>>> = Mutex::new(None);

/// Opens every DX12 adapter, in `wgpu`'s preference order, and shares each
/// device with `FFmpeg`. Nothing here encodes.
fn probed_adapters() -> Vec<ProbedAdapter> {
    pollster::block_on(alpha2_gpu::GpuContext::adapters_by_preference_on(
        wgpu::Backends::DX12,
    ))
    .into_iter()
    .map(|(index, adapter)| {
        let name = adapter.get_info().name;
        let position = format!("DX12 adapter {index}");
        let luid =
            alpha2_native_gpu::d3d12_adapter_identity(&adapter).map(|identity| identity.luid);
        let shared = pollster::block_on(alpha2_gpu::GpuContext::open_adapter(adapter))
            .map_err(|error| format!("{name}'s device did not open: {error}"))
            .and_then(|gpu| {
                SharedD3D12Context::from_gpu(gpu)
                    .map_err(|error| format!("{name}'s D3D12VA device did not open: {error}"))
            });
        ProbedAdapter {
            label: format!("{position} ({name})"),
            position,
            luid,
            shared,
        }
    })
    .collect()
}

/// The D3D12 Video Encode rate-control mode a mechanism asks for, as the
/// device's answers name it.
pub(super) const fn device_mode(mechanism: RateControlMechanism) -> &'static str {
    match mechanism {
        RateControlMechanism::ConstantBitrate => "cbr",
        RateControlMechanism::VariableBitrate => "vbr",
        RateControlMechanism::ConstantQuantizer => "disabled",
    }
}

/// Which of `adapters` answers for `mode` at `size`: the first whose D3D12
/// Video Encode reports the encoder. That one opens it, or refuses it with
/// the device's own reason when it does not list the mode or take the size;
/// a mode the device does not list is never opened.
///
/// # Errors
/// The answering device's refusal, or what every adapter said when none
/// reports the encoder.
pub(super) fn answering_adapter(
    adapters: &[(&str, Result<DeviceModes, String>)],
    mode: &'static str,
    size: (u32, u32),
) -> Result<usize, String> {
    let mut reasons = Vec::new();
    for (at, (label, modes)) in adapters.iter().enumerate() {
        let modes = match modes {
            Ok(modes) => modes,
            Err(reason) => {
                reasons.push(format!("{label}: {reason}"));
                continue;
            }
        };
        if let Some(reason) = modes.refusal(mode) {
            return Err(format!("on {label}: {reason}"));
        }
        if !modes.extent.accepts(size.0, size.1) {
            let coded = modes.extent.coded(size.0, size.1);
            return Err(format!(
                "on {label}: {} codes {}x{} as {}x{}, outside the device's {}x{} to {}x{}",
                modes.encoder.name,
                size.0,
                size.1,
                coded.0,
                coded.1,
                modes.extent.minimum.0,
                modes.extent.minimum.1,
                modes.extent.maximum.0,
                modes.extent.maximum.1
            ));
        }
        return Ok(at);
    }
    Err(if reasons.is_empty() {
        "no DX12 adapter".to_owned()
    } else {
        reasons.join("; ")
    })
}

/// Opens `name`, a D3D12VA encoder, with one rate-control request and
/// encodes one pool frame, on the first DX12 adapter whose D3D12 Video
/// Encode reports it, and returns the packets and the adapter that answered.
///
/// The pass condition is a software encoder's: it opened, consumed every
/// option it was given, and produced a packet. A mode the device does not
/// list is refused with the device's reason and never opened. Each open is
/// one frame, run by the [`D3D12RateControlRunner`] when one is installed,
/// and there is at most one per encoder and mechanism: a process asks a
/// driver that crashes about once in a few thousand frames for a handful.
pub(crate) fn probe_d3d12va_rate_control(
    name: &str,
    width: u32,
    height: u32,
    mechanism: RateControlMechanism,
    request: &crate::rate_control::MechanismRequest,
) -> Result<(u32, String), String> {
    let encoder = CODECS
        .into_iter()
        .filter_map(|codec| Encoder::for_codec(codec).ok())
        .find(|encoder| encoder.name == name)
        .ok_or_else(|| format!("{name} is not a D3D12VA encoder"))?;
    let mut adapters = ADAPTERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let adapters = adapters.get_or_insert_with(probed_adapters);
    let modes: Vec<_> = adapters
        .iter()
        .map(|adapter| {
            let modes = adapter
                .shared
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|shared| DeviceModes::of(shared, encoder));
            (adapter.position.as_str(), modes)
        })
        .collect();
    let adapter = &adapters[answering_adapter(&modes, device_mode(mechanism), (width, height))?];
    let shared = adapter.shared.as_ref().map_err(Clone::clone)?;
    let open = D3D12RateControlOpen {
        adapter: adapter.luid.unwrap_or(0),
        codec: encoder.codec,
        size: (width, height),
        bit_rate: request.bit_rate,
        options: request
            .options
            .iter()
            .map(|(key, value)| (key.to_string_lossy().into_owned(), value.clone()))
            .collect(),
    };
    // An adapter DXGI does not describe cannot be found again by another process.
    let packets = match (RUNNER.get(), adapter.luid) {
        (Some(runner), Some(_)) => runner(&open),
        _ => encode_one_frame(shared, &open),
    };
    packets
        .map(|packets| (packets, adapter.label.clone()))
        .map_err(|error| format!("on {}: {error}", adapter.label))
}

/// Runs `open` in this process: opens the adapter its LUID names, shares
/// its device with `FFmpeg`, opens the encoder with the open's options and
/// encodes one pool frame. What a [`D3D12RateControlRunner`]'s other
/// process runs.
///
/// # Errors
/// Why the adapter, the device or the encoder did not open, or the frame
/// did not encode.
pub fn open_d3d12va_rate_control(open: &D3D12RateControlOpen) -> Result<u32, String> {
    let adapter = pollster::block_on(alpha2_gpu::GpuContext::adapters_by_preference_on(
        wgpu::Backends::DX12,
    ))
    .into_iter()
    .map(|(_, adapter)| adapter)
    .find(|adapter| {
        alpha2_native_gpu::d3d12_adapter_identity(adapter)
            .is_some_and(|identity| identity.luid == open.adapter)
    })
    .ok_or_else(|| format!("no DX12 adapter has the LUID {:016X}", open.adapter))?;
    let gpu = pollster::block_on(alpha2_gpu::GpuContext::open_adapter(adapter))
        .map_err(|error| format!("the adapter's device did not open: {error}"))?;
    let shared = SharedD3D12Context::from_gpu(gpu)
        .map_err(|error| format!("the D3D12VA device did not open: {error}"))?;
    encode_one_frame(&shared, open)
}

/// Opens `open`'s encoder on `shared` with exactly its options, at the
/// profile an export opens, and encodes one pool frame.
fn encode_one_frame(
    shared: &SharedD3D12Context,
    open: &D3D12RateControlOpen,
) -> Result<u32, String> {
    let encoder = Encoder::for_codec(open.codec)?;
    let codec = encoder.find()?;
    let coded_width = i32::try_from(open.size.0).map_err(|_| "width is out of range")?;
    let coded_height = i32::try_from(open.size.1).map_err(|_| "height is out of range")?;
    let mut frames = d3d12va_encode_frames(&shared.ffmpeg_device(), coded_width, coded_height)
        .map_err(|error| error.to_string())?;
    let mut context = AVCodecContext::new(&codec);
    context.set_width(coded_width);
    context.set_height(coded_height);
    context.set_pix_fmt(ffi::AV_PIX_FMT_D3D12);
    context.set_time_base(ra(1, 30));
    context.set_framerate(ra(30, 1));
    context.set_bit_rate(open.bit_rate);
    context.set_gop_size(12);
    context.set_max_b_frames(0);
    context.set_hw_frames_ctx(frames.clone());
    let mut options =
        crate::convert::color_options(ColorMetadata::EXPORT).set(c"profile", encoder.profile, 0);
    for (key, value) in &open.options {
        let nul = |_| format!("the option {key} has a NUL byte");
        options = options.set(
            &std::ffi::CString::new(key.as_str()).map_err(nul)?,
            &std::ffi::CString::new(value.as_str()).map_err(nul)?,
            0,
        );
    }
    let unused = alpha2_ffmpeg_bridge::open_codec_context(&mut context, options)
        .map_err(|error| error.to_string())?;
    crate::convert::reject_unused_encode_options(unused).map_err(|error| error.to_string())?;
    let mut frame = AVFrame::new();
    frames
        .get_buffer(&mut frame)
        .map_err(|error| error.to_string())?;
    frame.set_pts(0);
    context
        .send_frame(Some(&frame))
        .map_err(|error| error.to_string())?;
    context
        .send_frame(None)
        .map_err(|error| error.to_string())?;
    let mut packets = 0;
    loop {
        match context.receive_packet() {
            Ok(_) => packets += 1,
            Err(RsmpegError::EncoderDrainError | RsmpegError::EncoderFlushedError) => break,
            Err(error) => return Err(error.to_string()),
        }
    }
    if packets == 0 {
        return Err("opened but produced no packet".to_owned());
    }
    Ok(packets)
}

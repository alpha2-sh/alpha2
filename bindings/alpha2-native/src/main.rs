//! Standalone alpha2 native media inspection CLI.

// A closed standard error must not panic the process, so diagnostics go through
// `alpha2_os::diagnostic!` instead of `eprintln!`; a closed standard output fails the command,
// so reports go through `out!` instead of `println!`.
#![cfg_attr(not(test), warn(clippy::print_stderr, clippy::print_stdout))]

use std::env;
use std::process::ExitCode;

/// `println!` that fails the command with `standard output: <error>` when the line cannot be
/// written (a reader that closed the pipe, as `| head` does), instead of panicking.
macro_rules! out {
    ($($argument:tt)*) => {{
        use std::io::Write as _;
        writeln!(std::io::stdout().lock(), $($argument)*)
            .map_err(|error| format!("standard output: {error}"))
    }};
}

use alpha2_media::{
    AudioBlock, BitrateMode, CancelToken, ColorMatrix, ColorMetadata, ColorRange, Container,
    DecodeEvent, DecodeRequest, DecodeRequestKind, DecodedVideoFrame, Decoder, EncodeEvent,
    EncodeSink, PixelFormat, Rotation, SourceId, VideoRateControl,
};
#[cfg(all(target_os = "linux", feature = "hwdecode"))]
use alpha2_native::probe_vulkan_decode;
use alpha2_native::{
    DecodeDiagnosticCounts, DecodePolicy, NativeAudioDecoder, NativeDecoder, NativeVideoCodec,
    NativeVideoEncodeSink, NativeWavEncodeSink, VideoDisplay, VideoEncodeConfig,
    codec_capabilities, probe, probe_video_encoders,
};

fn main() -> ExitCode {
    let mut decoding = Decoding::default();
    let result =
        alpha2_os::utf8_arguments(env::args_os().skip(1)).and_then(|args| run(args, &mut decoding));
    if let Some(summary) = decoding.summary() {
        alpha2_os::diagnostic!("alpha2: {summary}");
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            alpha2_os::diagnostic!("alpha2: {error}");
            ExitCode::FAILURE
        }
    }
}

/// The commands that decode a source's frames or samples, which take
/// `--decode-policy`.
const DECODING_COMMANDS: [&str; 7] = [
    "decode",
    "decode-audio",
    "render",
    "thumbnail",
    "transcode",
    "export-audio",
    "waveform",
];

/// How a command decodes: the damaged-input policy, and what its decoders
/// reported, for the summary written when it ends.
#[derive(Default)]
struct Decoding {
    policy: DecodePolicy,
    counts: std::sync::Arc<std::sync::Mutex<DecodeDiagnosticCounts>>,
}

impl Decoding {
    /// Opens `path`'s video under this policy, counting what it reports.
    fn video(&self, path: &str) -> Result<NativeDecoder, String> {
        let mut decoder = NativeDecoder::open(path).map_err(|error| format!("{error:?}"))?;
        decoder.set_decode_policy(self.policy);
        let counts = std::sync::Arc::clone(&self.counts);
        decoder.set_decode_reporter(move |diagnostic| {
            if let Ok(mut counts) = counts.lock() {
                counts.record(&diagnostic);
            }
        });
        Ok(decoder)
    }

    /// Opens `path`'s audio at `rate` under this policy, counting what it reports.
    fn audio(&self, path: &str, rate: u32) -> Result<NativeAudioDecoder, String> {
        let mut decoder =
            NativeAudioDecoder::open(path, rate).map_err(|error| format!("{error:?}"))?;
        decoder.set_decode_policy(self.policy);
        let counts = std::sync::Arc::clone(&self.counts);
        decoder.set_decode_reporter(move |diagnostic| {
            if let Ok(mut counts) = counts.lock() {
                counts.record(&diagnostic);
            }
        });
        Ok(decoder)
    }

    /// What the decoders reported, in the names a host export's
    /// `decodeDiagnostics` uses; none when they reported nothing.
    fn summary(&self) -> Option<String> {
        let counts = *self.counts.lock().ok()?;
        (!counts.is_empty()).then(|| {
            format!(
                "decode diagnostics ({} decode policy): corruptPackets={} videoGaps={} concealedAudioSamples={} timestampGapSamples={} droppedAudioSamples={}",
                self.policy.as_str(),
                counts.corrupt_packets,
                counts.video_gaps,
                counts.concealed_audio_samples,
                counts.timestamp_gap_samples,
                counts.dropped_audio_samples
            )
        })
    }
}

/// Takes `--decode-policy strict|tolerant` out of a command's arguments,
/// wherever it is: the policy it names, or none when it is absent.
fn take_decode_policy(args: &mut Vec<String>) -> Result<Option<DecodePolicy>, String> {
    let Some(at) = args.iter().position(|arg| arg == "--decode-policy") else {
        return Ok(None);
    };
    let flag = args.drain(at..(at + 2).min(args.len())).collect::<Vec<_>>();
    let policy = flag
        .get(1)
        .and_then(|value| DecodePolicy::parse(value))
        .ok_or_else(|| "--decode-policy takes strict or tolerant".to_owned())?;
    if args.iter().any(|arg| arg == "--decode-policy") {
        return Err("--decode-policy is given more than once".to_owned());
    }
    Ok(Some(policy))
}

#[allow(clippy::too_many_lines)]
fn run(mut args: Vec<String>, decoding: &mut Decoding) -> Result<(), String> {
    if args.is_empty() {
        return Err(usage());
    }
    let command = args.remove(0);
    let policy = take_decode_policy(&mut args)?;
    if DECODING_COMMANDS.contains(&command.as_str()) {
        // The flag, else the host's environment setting, else Strict.
        decoding.policy = policy.map_or_else(DecodePolicy::from_env, Ok)?;
    } else if policy.is_some() {
        return Err(format!(
            "{command} does not decode, so it takes no --decode-policy"
        ));
    }
    let decoding = &*decoding;
    let mut args = args.into_iter();
    if command == "probe-encoders" {
        return probe_encoders(args);
    }
    // Hidden: `probe-encoders` runs it as its own child, so a driver that crashes in an open takes
    // the child with it, not the probe.
    #[cfg(all(target_os = "windows", feature = "hwencode"))]
    if command == D3D12VA_RATE_CONTROL_SUBCOMMAND {
        return d3d12va_rate_control_child(&args.collect::<Vec<_>>());
    }
    let path = args.next().ok_or_else(usage)?;
    match command.as_str() {
        #[cfg(all(target_os = "linux", feature = "hwdecode"))]
        "probe-hwdecode" => {
            let frame_limit = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|_| "frame limit must be a positive integer".to_owned())?
                .unwrap_or(120);
            ensure_no_more(args)?;
            let result =
                probe_vulkan_decode(path, frame_limit).map_err(|error| format!("{error:?}"))?;
            out!("{result:#?}")?;
        }
        "probe" => {
            ensure_no_more(args)?;
            let result = probe(path).map_err(|error| format!("{error:?}"))?;
            out!("format: {}", result.format)?;
            out!(
                "duration_ticks: {}",
                result.duration.map_or(-1, alpha2_time::MediaTime::ticks)
            )?;
            out!("selected_video: {:?}", result.selected_video)?;
            out!("selected_audio: {:?}", result.selected_audio)?;
            for stream in result.streams {
                out!(
                    "stream {} {:?} codec={} duration_ticks={} dimensions={:?} rate={:?} color={:?}",
                    stream.index,
                    stream.kind,
                    stream.codec,
                    stream.duration.map_or(-1, alpha2_time::MediaTime::ticks),
                    stream.dimensions,
                    stream.frame_rate,
                    stream.color
                )?;
            }
        }
        "decode" => {
            ensure_no_more(args)?;
            let mut decoder = decoding.video(&path)?;
            let cancel = CancelToken::default();
            let mut frames = 0_u64;
            loop {
                match decoder
                    .decode_next(&cancel)
                    .map_err(|error| format!("{error:?}"))?
                {
                    DecodeEvent::Video(frame) => {
                        frames += 1;
                        out!(
                            "frame={} pts={} size={}x{} format={:?}",
                            frames,
                            frame.pts.ticks(),
                            frame.visible_rect.width,
                            frame.visible_rect.height,
                            frame.pixel_format
                        )?;
                    }
                    DecodeEvent::EndOfStream => break,
                    DecodeEvent::Retry | DecodeEvent::Audio(_) => {}
                }
            }
            out!("decoded_frames: {frames}")?;
        }
        "decode-audio" => {
            ensure_no_more(args)?;
            let mut decoder = decoding.audio(&path, 48_000)?;
            let cancel = CancelToken::default();
            let mut frames = 0_usize;
            loop {
                match decoder
                    .decode_next(&cancel)
                    .map_err(|error| format!("{error:?}"))?
                {
                    DecodeEvent::Audio(block) => {
                        frames += block.channels.first().map_or(0, Vec::len);
                    }
                    DecodeEvent::EndOfStream => break,
                    DecodeEvent::Retry | DecodeEvent::Video(_) => {}
                }
            }
            out!("decoded_audio_frames: {frames}")?;
            out!("sample_rate: 48000")?;
        }
        "render" => {
            let output = args.next().ok_or_else(usage)?;
            ensure_no_more(args)?;
            let mut decoder = decoding.video(&path)?;
            let cancel = CancelToken::default();
            let frame = loop {
                match decoder
                    .decode_next(&cancel)
                    .map_err(|error| format!("{error:?}"))?
                {
                    DecodeEvent::Video(frame) => break frame,
                    DecodeEvent::EndOfStream => return Err("source contains no video frame".into()),
                    DecodeEvent::Retry | DecodeEvent::Audio(_) => {}
                }
            };
            write_png(&output, &frame)?;
        }
        "transcode" => transcode(decoding, &path, args)?,
        "thumbnail" => thumbnail(decoding, &path, args)?,
        "export-audio" => {
            let output = args.next().ok_or_else(usage)?;
            ensure_no_more(args)?;
            export_audio(decoding, &path, &output)?;
        }
        "waveform" => {
            let bucket_ms = args
                .next()
                .map(|value| value.parse::<f64>())
                .transpose()
                .map_err(|_| "bucket milliseconds must be a number".to_owned())?
                .unwrap_or(10.0);
            let levels = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|_| "levels must be a positive integer".to_owned())?
                .unwrap_or(6);
            ensure_no_more(args)?;
            out!("{}", waveform_json(decoding, &path, bucket_ms, levels)?)?;
        }
        _ => return Err(usage()),
    }
    Ok(())
}

/// Decodes a source's audio, resampled to 48 kHz, into a WAV that starts at
/// the source's time zero: a stream that starts later is preceded by silence.
fn export_audio(decoding: &Decoding, path: &str, output: &str) -> Result<(), String> {
    let mut decoder = decoding.audio(path, 48_000)?;
    // The WAV says the source's speakers: a channel mask for any layout but its count's default.
    let mut sink = NativeWavEncodeSink::create_with_speakers(output, 48_000, decoder.speakers())
        .map_err(|error| format!("{error:?}"))?;
    let cancel = CancelToken::default();
    // The WAV starts at the source's time zero: a later start is preceded by silence, and anything
    // before zero is dropped.
    let mut written = 0_i64;
    loop {
        match decoder
            .decode_next(&cancel)
            .map_err(|error| format!("{error:?}"))?
        {
            DecodeEvent::Audio(mut block) => {
                // A late start (an MPEG-TS clock hours in) is written as silence piecewise, so the
                // sink refuses a WAV past 4 GiB before writing it; a start that alone passes it is
                // refused up front.
                if written == 0 {
                    check_wav_start(block.start_sample, block.sample_rate, block.channels.len())?;
                }
                while block.start_sample > written {
                    let gap = usize::try_from(block.start_sample - written)
                        .unwrap_or(usize::MAX)
                        .min(SILENCE_CHUNK_FRAMES);
                    sink.write(
                        EncodeEvent::Audio(AudioBlock {
                            start_sample: written,
                            sample_rate: block.sample_rate,
                            speakers: block.speakers,
                            channels: vec![vec![0.0; gap]; block.channels.len()],
                        }),
                        &cancel,
                    )
                    .map_err(|error| format!("{error:?}"))?;
                    written += i64::try_from(gap).unwrap_or(i64::MAX);
                }
                let early = written
                    .checked_sub(block.start_sample)
                    .and_then(|early| usize::try_from(early).ok())
                    .unwrap_or(0)
                    .min(block.frames());
                for channel in &mut block.channels {
                    channel.drain(..early);
                }
                block.start_sample = written;
                written += i64::try_from(block.frames()).unwrap_or(0);
                if block.frames() > 0 {
                    sink.write(EncodeEvent::Audio(block), &cancel)
                        .map_err(|error| format!("{error:?}"))?;
                }
            }
            DecodeEvent::EndOfStream => break,
            DecodeEvent::Retry | DecodeEvent::Video(_) => {}
        }
    }
    sink.finish(&cancel).map_err(|error| format!("{error:?}"))
}

/// Refuses a WAV whose leading silence alone, `start` frames of `channels`
/// 32-bit samples, passes the 4 GiB a RIFF file holds.
fn check_wav_start(start: i64, rate: u32, channels: usize) -> Result<(), String> {
    let bytes = i128::from(start) * i128::try_from(channels).unwrap_or(i128::MAX) * 4;
    if bytes + 44 > i128::from(u32::MAX) {
        #[allow(clippy::cast_precision_loss)]
        let seconds = start as f64 / f64::from(rate);
        return Err(format!(
            "the audio starts {seconds:.1} s into the source, and a WAV from the source's time zero would pass the 4 GiB a WAV file holds"
        ));
    }
    Ok(())
}

/// The most frames of leading silence `export-audio` writes in one block.
const SILENCE_CHUNK_FRAMES: usize = 1 << 16;

/// Decodes a source's audio at its own rate and returns its waveform summary
/// as JSON: `{ sampleRate, channels, startSample, frames, levels: [{
/// bucketSamples, channels: [[[min, max, rms], ...], ...] }] }`. The first
/// bucket starts at `startSample`, where the stream's audio starts on the
/// source's timeline (an MPEG-TS stream 1.4 s in starts at 67 200 at 48 kHz).
fn waveform_json(
    decoding: &Decoding,
    path: &str,
    bucket_ms: f64,
    levels: usize,
) -> Result<String, String> {
    use std::fmt::Write as _;

    let probed = probe(path).map_err(|error| format!("{error:?}"))?;
    let stream = probed
        .streams
        .iter()
        .find(|stream| Some(stream.index) == probed.selected_audio)
        .ok_or_else(|| "the source has no audio stream".to_owned())?;
    let rate = stream
        .sample_rate
        .ok_or_else(|| "missing audio sample rate".to_owned())?;
    if !(bucket_ms.is_finite() && bucket_ms > 0.0) {
        return Err("bucket milliseconds must be positive".to_owned());
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let bucket = (bucket_ms * f64::from(rate) / 1_000.0).round().max(1.0) as usize;
    let mut decoder = decoding.audio(path, rate)?;
    // The channels the decoder gives, which the stream's own count may not be.
    let channels = decoder.channels();
    let mut summary = alpha2_audio::WaveformSummary::new(channels, bucket, levels)
        .map_err(|error| error.to_string())?;
    let cancel = CancelToken::default();
    let mut start_sample = None;
    loop {
        match decoder
            .decode_next(&cancel)
            .map_err(|error| format!("{error:?}"))?
        {
            DecodeEvent::Audio(block) => {
                start_sample.get_or_insert(block.start_sample);
                let planes = block.channels.iter().map(Vec::as_slice).collect::<Vec<_>>();
                summary.push(&planes).map_err(|error| error.to_string())?;
            }
            DecodeEvent::EndOfStream => break,
            DecodeEvent::Retry | DecodeEvent::Video(_) => {}
        }
    }
    let waveform = summary.finish();
    let number = |value: f32| {
        if value.is_finite() {
            value.to_string()
        } else {
            "null".to_owned()
        }
    };
    let mut json = format!(
        "{{\"sampleRate\":{rate},\"channels\":{channels},\"startSample\":{},\"frames\":{},\"levels\":[",
        start_sample.unwrap_or(0),
        waveform.frames
    );
    for (level_index, level) in waveform.levels.iter().enumerate() {
        if level_index > 0 {
            json.push(',');
        }
        write!(
            json,
            "{{\"bucketSamples\":{},\"channels\":[",
            level.bucket_samples
        )
        .map_err(|error| error.to_string())?;
        for (channel_index, buckets) in level.channels.iter().enumerate() {
            if channel_index > 0 {
                json.push(',');
            }
            json.push('[');
            for (bucket_index, bucket) in buckets.iter().enumerate() {
                if bucket_index > 0 {
                    json.push(',');
                }
                write!(
                    json,
                    "[{},{},{}]",
                    number(bucket.min),
                    number(bucket.max),
                    number(bucket.rms)
                )
                .map_err(|error| error.to_string())?;
            }
            json.push(']');
        }
        json.push_str("]}");
    }
    json.push_str("]}");
    Ok(json)
}

fn usage() -> String {
    "usage: alpha2 probe|decode|decode-audio <media-path>\n       alpha2 probe-hwdecode <media-path> [frame-limit]\n       alpha2 probe-encoders [width] [height]\n       alpha2 render <media-path> <png-path>\n       alpha2 thumbnail <media-path> <png-path> [--auto | --at <source-seconds>]\n       alpha2 transcode <media-path> <output-media> [--proxy half|quarter] [--video-codec <id>]\n       alpha2 export-audio <media-path> <output.wav>\n       alpha2 waveform <media-path> [bucket-ms=10] [levels=6]\n\n       decode, decode-audio, render, thumbnail, transcode, export-audio and waveform take\n       --decode-policy strict|tolerant: how damaged input is handled (default: ALPHA2_DECODE_POLICY, else strict)".to_owned()
}

/// Reports what every registered video encoder declares and whether it runs.
///
/// Registration, advertisement and instantiation are three separate claims.
/// A capability list answers the first two; only opening the encoder and
/// draining a packet answers the third, and that is the one an export needs.
fn probe_encoders(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let width = args
        .next()
        .map(|value| value.parse::<u32>())
        .transpose()
        .map_err(|_| "width must be a positive integer".to_owned())?
        .unwrap_or(1_920);
    let height = args
        .next()
        .map(|value| value.parse::<u32>())
        .transpose()
        .map_err(|_| "height must be a positive integer".to_owned())?
        .unwrap_or(1_080);
    ensure_no_more(args)?;
    // Each D3D12VA mode the device lists opens in a child of this process.
    #[cfg(all(target_os = "windows", feature = "hwencode"))]
    let _ = alpha2_native::set_d3d12va_rate_control_runner(run_d3d12va_rate_control_child);
    let identity = alpha2_native::ffmpeg_runtime_identity();
    out!(
        "runtime: {}, pinned SDK {}",
        identity.version,
        identity.pinned_sdk_release
    )?;
    if let Some(reason) = identity.pinned_sdk_mismatch() {
        out!("runtime is not the pinned SDK: {reason}")?;
    }
    out!("probe size: {width}x{height}")?;
    for probe in probe_video_encoders(width, height) {
        out!()?;
        out!(
            "{}:{} class={} {} caps=0x{:08x} advertised={}",
            probe.codec.as_str(),
            probe.encoder_name,
            probe.class.as_str(),
            if probe.hardware_accelerated {
                "on-gpu"
            } else {
                "on-cpu"
            },
            probe.capability_bits,
            probe.advertised
        )?;
        out!(
            "  pixel formats: [{}]{}",
            probe.pixel_formats.join(" "),
            if probe.accepts_yuv420p {
                ""
            } else {
                " (no YUV420P)"
            }
        )?;
        for config in &probe.hardware_configs {
            out!(
                "  hw config: {} via {} methods=0x{:x}{}",
                config.pixel_format,
                config.device_type,
                config.methods,
                if config.needs_hardware_frames {
                    " needs-hw-frames-ctx"
                } else {
                    ""
                }
            )?;
        }
        out!(
            "  minimum size: {}x{}",
            probe.minimum_size.0,
            probe.minimum_size.1
        )?;
        match &probe.software_open_error {
            None => out!(
                "  software YUV420P encode: ok ({} packets)",
                probe.software_packets
            )?,
            Some(error) => out!("  software YUV420P encode: {error}")?,
        }
        if probe.requires_hardware_frames {
            out!("  verdict: hardware frames only; the YUV420P sink cannot feed it")?;
        }
        for rate in &probe.rate_control {
            let options = rate.options.as_deref().unwrap_or("-");
            match &rate.outcome {
                Ok(packets) => out!(
                    "  rate control {}: opened ({packets} packets) with [{options}]{}",
                    rate.mechanism.as_str(),
                    rate.device.as_deref().map_or_else(String::new, |device| {
                        if probe.encoder_name.ends_with("_vulkan") {
                            format!(" on Vulkan device {device}")
                        } else {
                            format!(" on {device}")
                        }
                    })
                )?,
                Err(error) => out!(
                    "  rate control {}: refused with [{options}]: {error}",
                    rate.mechanism.as_str()
                )?,
            }
        }
    }
    Ok(())
}

/// The hidden subcommand each D3D12VA rate-control open of `probe-encoders`
/// runs in: `probe-d3d12va-rate-control` and the open's arguments
/// ([`alpha2_native::D3D12RateControlOpen::arguments`]).
///
/// An Intel integrated GPU on driver 30.0.101.2079 hangs or crashes inside
/// `FFmpeg`'s D3D12VA encode submission about once in a few thousand
/// frames. The probe encodes one frame per mode the device lists, so a
/// crash is rare, but one would end the whole report; in a child it ends
/// that row only.
#[cfg(all(target_os = "windows", feature = "hwencode"))]
const D3D12VA_RATE_CONTROL_SUBCOMMAND: &str = "probe-d3d12va-rate-control";

/// How long one open's child may run before it is killed: the GPU wait
/// timeout's default, since `FFmpeg`'s own D3D12VA waits are unbounded.
#[cfg(all(target_os = "windows", feature = "hwencode"))]
const D3D12VA_RATE_CONTROL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The subcommand: runs the open in this process and writes its answer as
/// one line, `opened <packets>` or `refused <reason>`, exiting 0 either way.
#[cfg(all(target_os = "windows", feature = "hwencode"))]
fn d3d12va_rate_control_child(args: &[String]) -> Result<(), String> {
    let open = alpha2_native::D3D12RateControlOpen::parse(args)?;
    out!(
        "{}",
        rate_control_answer_line(&alpha2_native::open_d3d12va_rate_control(&open))
    )?;
    Ok(())
}

/// The line a rate-control child answers with.
#[cfg(all(target_os = "windows", feature = "hwencode"))]
fn rate_control_answer_line(answer: &Result<u32, String>) -> String {
    match answer {
        Ok(packets) => format!("opened {packets}"),
        Err(reason) => format!("refused {}", reason.replace(['\r', '\n'], " ")),
    }
}

/// What a rate-control child's exit `code` and standard output say: its
/// answer, or that it crashed or exited without one.
#[cfg(all(target_os = "windows", feature = "hwencode"))]
fn rate_control_answer(code: Option<i32>, stdout: &str) -> Result<u32, String> {
    let answer = stdout.lines().rev().map(str::trim).find_map(|line| {
        if let Some(packets) = line.strip_prefix("opened ") {
            packets.parse().ok().map(Ok)
        } else {
            line.strip_prefix("refused ")
                .map(|reason| Err(reason.to_owned()))
        }
    });
    match (code, answer) {
        (Some(0), Some(answer)) => answer,
        // An NTSTATUS: an access violation (0xC0000005), a fast fail (0xC0000409).
        (Some(code), _) if code.cast_unsigned() >= 0xC000_0000 => Err(format!(
            "the open crashed its process (exit {:#010X})",
            code.cast_unsigned()
        )),
        (Some(code), _) => Err(format!(
            "the open's process exited with {code} and no answer"
        )),
        (None, _) => Err("the open's process ended without an exit code".to_owned()),
    }
}

/// The runner `probe-encoders` installs: runs `open` in a child of this
/// executable, bounded by [`D3D12VA_RATE_CONTROL_TIMEOUT`]. The child's
/// `FFmpeg` log goes to this process's standard error, as an open in this
/// process would.
#[cfg(all(target_os = "windows", feature = "hwencode"))]
fn run_d3d12va_rate_control_child(
    open: &alpha2_native::D3D12RateControlOpen,
) -> Result<u32, String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    // A child left behind when this process is killed would hold its device. The job lives as
    // long as the process: closing it ends the children in it.
    static JOB: std::sync::OnceLock<Option<alpha2_os::KillOnCloseJob>> = std::sync::OnceLock::new();

    let executable = env::current_exe()
        .map_err(|error| format!("the open has no executable to run: {error}"))?;
    let mut child = Command::new(executable)
        .arg(D3D12VA_RATE_CONTROL_SUBCOMMAND)
        .args(open.arguments())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("the open's process did not start: {error}"))?;
    if let Some(job) = JOB.get_or_init(|| alpha2_os::KillOnCloseJob::new().ok()) {
        let _ = job.assign(&child);
    }
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= D3D12VA_RATE_CONTROL_TIMEOUT => {
                let _ = child.kill();
                return Err(format!(
                    "the open did not finish within {D3D12VA_RATE_CONTROL_TIMEOUT:?} and was killed"
                ));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(5)),
            Err(error) => return Err(format!("the open's process was lost: {error}")),
        }
    };
    // The answer is one short line, which the pipe holds until the child has exited.
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    rate_control_answer(status.code(), &stdout)
}

fn ensure_no_more(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    if args.next().is_some() {
        Err(usage())
    } else {
        Ok(())
    }
}

/// Re-encodes a file's video, at its own size or shrunk to a proxy.
///
/// The output shows its pictures as the source does: its display matrix and
/// pixel aspect ratio are written beside them. It runs at the source's
/// nominal frame rate; a frame that lands in a frame slot already written (a
/// variable-rate source running faster than its nominal rate) is dropped and
/// counted, as `ffmpeg -fps_mode cfr` drops one.
fn transcode(
    decoding: &Decoding,
    path: &str,
    mut args: impl Iterator<Item = String>,
) -> Result<(), String> {
    let output = args.next().ok_or_else(usage)?;
    let mut factor = 1_u32;
    let mut requested_codec = None;
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(usage)?;
        match (flag.as_str(), value.as_str()) {
            ("--proxy", "half") => factor = 2,
            ("--proxy", "quarter") => factor = 4,
            ("--video-codec", _) => requested_codec = Some(value),
            _ => return Err(usage()),
        }
    }
    let probed = probe(path).map_err(|error| format!("{error:?}"))?;
    let stream = probed
        .selected_video_stream()
        .ok_or_else(|| "source has no video stream".to_owned())?
        .clone();
    let (width, height) = stream
        .dimensions
        .ok_or_else(|| "missing dimensions".to_owned())?;
    // A proxy is the full size divided and rounded down to even, as each frame is shrunk.
    let (width, height) = if factor == 1 {
        (width, height)
    } else {
        ((width / factor) & !1, (height / factor) & !1)
    };
    let frame_rate = stream
        .frame_rate
        .ok_or_else(|| "missing frame rate".to_owned())?;
    let container = std::path::Path::new(&output)
        .extension()
        .and_then(|extension| extension.to_str())
        .and_then(Container::from_extension)
        .ok_or_else(|| "the output must end in .mp4, .m4v, .mov, .mkv or .webm".to_owned())?;
    let codec = transcode_codec(requested_codec.as_deref(), container, (width, height))?;
    let mut decoder = decoding.video(path)?;
    let mut sink = NativeVideoEncodeSink::create(
        &output,
        VideoEncodeConfig {
            width,
            height,
            frame_rate,
            color: transcode_color(stream.color)?,
            rate_control: VideoRateControl {
                bitrate: 8_000_000 / u64::from(factor * factor),
                mode: BitrateMode::Variable,
                quantizer: None,
            },
            codec,
            display: VideoDisplay {
                orientation: stream.orientation,
                sample_aspect_ratio: stream.sample_aspect_ratio,
            },
            container,
            pixels: alpha2_media::PixelLayout::YUV420P,
        },
    )
    .map_err(|error| format!("{error:?}"))?;
    out!(
        "video: {} ({})",
        codec.as_str(),
        sink.selection().encoder_name
    )?;
    let cancel = CancelToken::default();
    let mut last_slot = None;
    let mut dropped = 0_u64;
    loop {
        match decoder
            .decode_next(&cancel)
            .map_err(|error| format!("{error:?}"))?
        {
            DecodeEvent::Video(mut frame) => {
                let slot = frame_slot(frame.pts, frame_rate);
                if last_slot.is_some_and(|last| slot <= last) {
                    dropped += 1;
                    continue;
                }
                last_slot = Some(slot);
                frame.pts = slot_time(slot, frame_rate);
                let frame = if factor == 1 {
                    frame
                } else {
                    alpha2_native::downscale_frame(&frame, factor)?
                };
                sink.write(EncodeEvent::Video(frame), &cancel)
                    .map_err(|error| format!("{error:?}"))?;
            }
            DecodeEvent::EndOfStream => break,
            DecodeEvent::Retry | DecodeEvent::Audio(_) => {}
        }
    }
    sink.finish(&cancel).map_err(|error| format!("{error:?}"))?;
    if dropped > 0 {
        alpha2_os::diagnostic!(
            "alpha2: dropped {dropped} frames that fell in a frame slot already written at {}/{} fps",
            frame_rate.numerator(),
            frame_rate.denominator()
        );
    }
    Ok(())
}

/// The codec `transcode` writes into `container` at `size`: the one asked
/// for by its id, else the container's default, else the first this runtime
/// encodes that the container offers. An encoder whose smallest frame is
/// larger than `size` (NVENC's) does not count.
fn transcode_codec(
    requested: Option<&str>,
    container: Container,
    (width, height): (u32, u32),
) -> Result<NativeVideoCodec, String> {
    let capabilities = codec_capabilities();
    let offered = || {
        capabilities.video.iter().filter(|capability| {
            container.offers_video(capability.codec.model())
                && capability.minimum_width <= width
                && capability.minimum_height <= height
        })
    };
    let available = || {
        offered()
            .map(|capability| capability.codec.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let chosen = match requested {
        Some(id) => offered()
            .find(|capability| capability.codec.as_str() == id)
            .ok_or_else(|| {
                format!(
                    "this runtime encodes no {id} into {container}; it encodes: {}",
                    available()
                )
            })?,
        None => offered()
            .find(|capability| capability.codec.model() == container.default_video_codec())
            .or_else(|| offered().next())
            .ok_or_else(|| format!("no native video encoder writes {container} here"))?,
    };
    Ok(chosen.codec)
}

/// The colour `transcode`'s 4:2:0 output is tagged with. YCbCr frames keep
/// their codes, so their tags; RGB frames are converted with BT.709
/// coefficients into limited range, which is what the output then is.
/// HDR is refused: the encoder writes SDR, and nothing here tone-maps.
fn transcode_color(source: Option<ColorMetadata>) -> Result<ColorMetadata, String> {
    let color = source.unwrap_or_default();
    if color.is_hdr() {
        return Err(format!(
            "transcode writes SDR video, and the source is HDR (transfer {:?})",
            color.transfer
        ));
    }
    Ok(if color.matrix == ColorMatrix::Rgb {
        ColorMetadata {
            matrix: ColorMatrix::Bt709,
            range: ColorRange::Limited,
            ..color
        }
    } else {
        color
    })
}

/// The frame slot of `rate` nearest `time`, half a slot rounding up.
fn frame_slot(time: alpha2_time::MediaTime, rate: alpha2_time::FrameRate) -> i64 {
    let scaled = i128::from(time.ticks()) * i128::from(rate.numerator()) * 2;
    let unit = i128::from(rate.denominator()) * i128::from(alpha2_time::TICKS_PER_SECOND) * 2;
    i64::try_from((scaled + unit / 2).div_euclid(unit)).unwrap_or(i64::MAX)
}

/// When frame slot `slot` of `rate` starts, to the nearest tick.
fn slot_time(slot: i64, rate: alpha2_time::FrameRate) -> alpha2_time::MediaTime {
    let ticks = i128::from(slot)
        * i128::from(rate.denominator())
        * i128::from(alpha2_time::TICKS_PER_SECOND);
    let numerator = i128::from(rate.numerator());
    alpha2_time::MediaTime::from_ticks(
        i64::try_from((ticks * 2 + numerator).div_euclid(numerator * 2)).unwrap_or(i64::MAX),
    )
}

/// Writes a representative frame as PNG: by default the sharpest of nine
/// frames spread across the video that is neither black nor white, or the
/// frame at a given time on the source's timeline: its container
/// timestamps, the times a project's source ranges name, not a time from the
/// first frame.
fn thumbnail(
    decoding: &Decoding,
    path: &str,
    mut args: impl Iterator<Item = String>,
) -> Result<(), String> {
    let output = args.next().ok_or_else(usage)?;
    let at = match (args.next().as_deref(), args.next()) {
        (None | Some("--auto"), None) => None,
        (Some("--at"), Some(seconds)) => Some(
            seconds
                .parse::<f64>()
                .ok()
                .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
                .ok_or_else(|| {
                    "--at takes a time in seconds on the source's timeline".to_owned()
                })?,
        ),
        _ => return Err(usage()),
    };
    ensure_no_more(args)?;
    let probed = probe(path).map_err(|error| format!("{error:?}"))?;
    let mut decoder = decoding.video(path)?;
    let cancel = CancelToken::default();
    let span = if at.is_none() {
        let first = decoder
            .peek_video_pts(&cancel)
            .map_err(|error| format!("{error:?}"))?
            .ok_or_else(|| "source contains no video frame".to_owned())?;
        let stream = probed
            .selected_video_stream()
            .and_then(|stream| stream.duration);
        Some((first, video_length(stream, probed.duration, first)?))
    } else {
        None
    };
    let mut frame_at = |time: alpha2_time::MediaTime| -> Result<Option<DecodedVideoFrame>, String> {
        let result = decoder
            .seek(
                DecodeRequest {
                    source: SourceId::new("thumbnail".to_owned())
                        .map_err(|error| error.to_string())?,
                    time,
                    kind: DecodeRequestKind::Seek,
                    generation: 0,
                },
                &cancel,
            )
            .map_err(|error| format!("{error:?}"))?;
        Ok(match result.event {
            DecodeEvent::Video(frame) => Some(frame),
            _ => None,
        })
    };
    #[allow(clippy::cast_possible_truncation)]
    let frame = if let Some(seconds) = at {
        frame_at(alpha2_time::MediaTime::from_ticks(
            (seconds * f64::from(u32::try_from(alpha2_time::TICKS_PER_SECOND).unwrap_or(u32::MAX)))
                .round() as i64,
        ))?
        .ok_or_else(|| format!("no frame at {seconds} s"))?
    } else {
        let (first, length) = span.ok_or_else(|| "the video's span is unknown".to_owned())?;
        let mut candidates = Vec::new();
        for time in thumbnail_times(first, length)? {
            if let Some(frame) = frame_at(time)? {
                candidates.push(frame);
            }
        }
        let scores = candidates
            .iter()
            .map(alpha2_native::score_frame)
            .collect::<Result<Vec<_>, _>>()?;
        let chosen = alpha2_native::choose_thumbnail(&scores)
            .ok_or_else(|| "source contains no video frame".to_owned())?;
        candidates.swap_remove(chosen)
    };
    write_png(&output, &frame)
}

/// How long the selected video stream lasts from its first frame at
/// `first`: its own duration, else the container's less the time before
/// that frame. A length that is not positive, or past a year, is refused.
fn video_length(
    stream: Option<alpha2_time::MediaTime>,
    container: Option<alpha2_time::MediaTime>,
    first: alpha2_time::MediaTime,
) -> Result<alpha2_time::MediaTime, String> {
    const YEAR_TICKS: i64 = 365 * 24 * 3_600 * alpha2_time::TICKS_PER_SECOND;
    let length = stream
        .map(alpha2_time::MediaTime::ticks)
        .or_else(|| container.map(|duration| duration.ticks().saturating_sub(first.ticks())))
        .ok_or_else(|| "the video's duration is unknown".to_owned())?;
    if !(1..=YEAR_TICKS).contains(&length) {
        return Err(format!(
            "the video's duration ({length} ticks) is not one a file has"
        ));
    }
    Ok(alpha2_time::MediaTime::from_ticks(length))
}

/// The nine times `thumbnail --auto` looks at: a tenth to nine tenths of the
/// way from the video's first frame at `first` through `length`.
fn thumbnail_times(
    first: alpha2_time::MediaTime,
    length: alpha2_time::MediaTime,
) -> Result<Vec<alpha2_time::MediaTime>, String> {
    (1..=9_i128)
        .map(|tenth| {
            i64::try_from(i128::from(first.ticks()) + i128::from(length.ticks()) * tenth / 10)
                .map(alpha2_time::MediaTime::from_ticks)
                .map_err(|_| "the video's span is out of range".to_owned())
        })
        .collect()
}

/// Writes `frame` as a PNG at `path`, replacing a regular file there only
/// once the whole image is written: it is written to the claimed partial file
/// beside it, every write and the flush checked, then renamed into place. A
/// path that names anything but a regular file (a symbolic link, a device, a
/// FIFO) is refused.
fn write_png(path: &str, frame: &DecodedVideoFrame) -> Result<(), String> {
    let (width, height, straight) = png_pixels(frame)?;
    alpha2_native::write_png(
        alpha2_native::OutputTarget::replacing(path),
        width,
        height,
        &straight,
    )
    .map(|_| ())
    .map_err(|error| format!("writing {path}: {error}"))
}

fn png_pixels(frame: &DecodedVideoFrame) -> Result<(u32, u32, Vec<u8>), String> {
    let crop = frame.visible_rect;
    let rotated = frame.orientation.rotation.swaps_axes();
    let output_width = if rotated { crop.height } else { crop.width };
    let output_height = if rotated { crop.width } else { crop.height };
    let len = usize::try_from(output_width)
        .ok()
        .and_then(|width| {
            usize::try_from(output_height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "PNG dimensions overflow".to_owned())?;
    let mut output = vec![0_u8; len];
    for y in 0..output_height {
        for x in 0..output_width {
            // Where the mirrored picture's pixel sits, then the stored one it mirrors.
            let (source_x, source_y) = match frame.orientation.rotation {
                Rotation::Deg0 => (x, y),
                Rotation::Deg90 => (y, crop.height.saturating_sub(1).saturating_sub(x)),
                Rotation::Deg180 => (
                    crop.width.saturating_sub(1).saturating_sub(x),
                    crop.height.saturating_sub(1).saturating_sub(y),
                ),
                Rotation::Deg270 => (crop.width.saturating_sub(1).saturating_sub(y), x),
            };
            let source_x = if frame.orientation.mirror {
                crop.width.saturating_sub(1).saturating_sub(source_x)
            } else {
                source_x
            };
            let rgba = decoded_pixel(
                frame,
                crop.x.saturating_add(source_x),
                crop.y.saturating_add(source_y),
            )?;
            let offset = usize::try_from(y)
                .ok()
                .and_then(|row| row.checked_mul(usize::try_from(output_width).ok()?))
                .and_then(|row| row.checked_add(usize::try_from(x).ok()?))
                .and_then(|pixel| pixel.checked_mul(4))
                .ok_or_else(|| "PNG pixel offset overflow".to_owned())?;
            output[offset..offset + 4].copy_from_slice(&rgba);
        }
    }
    // Shown with square pixels: the stored rows stretched by the pixel aspect ratio, which a
    // quarter turn makes the shown columns.
    let sar = frame.sample_aspect_ratio;
    let shown_row = u32::try_from(
        (u64::from(crop.width) * u64::from(sar.num) * 2 + u64::from(sar.den))
            / (u64::from(sar.den) * 2),
    )
    .map_err(|_| "PNG dimensions overflow".to_owned())?
    .max(1);
    let (shown_width, shown_height) = if rotated {
        (output_width, shown_row)
    } else {
        (shown_row, output_height)
    };
    if (shown_width, shown_height) == (output_width, output_height) {
        return Ok((output_width, output_height, output));
    }
    let shown = alpha2_native::resize_rgba(
        &output,
        (output_width, output_height),
        (shown_width, shown_height),
    )?;
    Ok((shown_width, shown_height, shown))
}

fn decoded_pixel(frame: &DecodedVideoFrame, x: u32, y: u32) -> Result<[u8; 4], String> {
    match frame.pixel_format {
        PixelFormat::Rgba8 => {
            let plane = frame
                .planes
                .first()
                .ok_or_else(|| "RGBA frame has no plane".to_owned())?;
            let offset = pixel_offset(plane.stride, x, y, 4)?;
            let pixel = plane
                .bytes
                .get(offset..offset + 4)
                .ok_or_else(|| "RGBA plane is shorter than declared".to_owned())?;
            // Decoders hand over straight alpha.
            Ok([pixel[0], pixel[1], pixel[2], pixel[3]])
        }
        PixelFormat::Yuv(format) if frame.planes.len() == format.plane_count() => {
            let (chroma_x, chroma_y) = (x >> format.chroma_shift_x, y >> format.chroma_shift_y);
            let y_value = plane_sample(&frame.planes[0], format, x, y)?;
            let (u, v) = if format.interleaved_chroma {
                (
                    plane_sample(&frame.planes[1], format, chroma_x * 2, chroma_y)?,
                    plane_sample(&frame.planes[1], format, chroma_x * 2 + 1, chroma_y)?,
                )
            } else {
                (
                    plane_sample(&frame.planes[1], format, chroma_x, chroma_y)?,
                    plane_sample(&frame.planes[2], format, chroma_x, chroma_y)?,
                )
            };
            let mut pixel = yuv_to_srgb(y_value, u, v, frame.color.unwrap_or_default());
            // Straight alpha, when a plane after the chroma holds it.
            if format.alpha {
                pixel[3] = plane_sample(&frame.planes[format.color_plane_count()], format, x, y)?;
            }
            Ok(pixel)
        }
        _ => Err(format!(
            "PNG rendering does not support {:?}",
            frame.pixel_format
        )),
    }
}

/// One sample, reduced to its top eight significant bits.
fn plane_sample(
    plane: &alpha2_media::Plane,
    format: alpha2_media::YuvFormat,
    x: u32,
    y: u32,
) -> Result<u8, String> {
    let bytes = format.bytes_per_sample() as usize;
    let offset = pixel_offset(plane.stride, x, y, bytes)?;
    let sample = plane
        .bytes
        .get(offset..offset + bytes)
        .ok_or_else(|| "YUV plane is shorter than declared".to_owned())?;
    if bytes == 1 {
        return Ok(sample[0]);
    }
    let container = u16::from_le_bytes([sample[0], sample[1]]);
    let depth = u32::from(format.bit_depth);
    let code = if format.msb_aligned {
        container >> (16 - depth)
    } else {
        container
    };
    u8::try_from(code >> (depth - 8)).map_err(|_| "YUV sample exceeds its depth".to_owned())
}

fn pixel_offset(stride: u32, x: u32, y: u32, bytes_per_pixel: usize) -> Result<usize, String> {
    usize::try_from(y)
        .ok()
        .and_then(|row| row.checked_mul(usize::try_from(stride).ok()?))
        .and_then(|row| row.checked_add(usize::try_from(x).ok()?.checked_mul(bytes_per_pixel)?))
        .ok_or_else(|| "decoded pixel offset overflow".to_owned())
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn yuv_to_srgb(y: u8, u: u8, v: u8, color: alpha2_media::ColorMetadata) -> [u8; 4] {
    // Planes of red, green, and blue under the identity matrix are the signal already.
    if color.matrix == ColorMatrix::Rgb {
        return [y, u, v, 255];
    }
    // The levels and weights the renderer decodes with, from their one definition.
    let levels = color.range.levels(8);
    let (y, u, v) = (
        (f32::from(y) - levels.luma_black) / levels.luma_span,
        (f32::from(u) - levels.chroma_center) / levels.chroma_span,
        (f32::from(v) - levels.chroma_center) / levels.chroma_span,
    );
    let (kr, kb) = color
        .matrix
        .luma_weights()
        .or(ColorMatrix::Bt709.luma_weights())
        .expect("BT.709 is a YCbCr matrix");
    let kg = 1.0 - kr - kb;
    let encoded = [
        y + 2.0 * (1.0 - kr) * v,
        y - 2.0 * kb * (1.0 - kb) / kg * u - 2.0 * kr * (1.0 - kr) / kg * v,
        y + 2.0 * (1.0 - kb) * u,
    ];
    // The CLI writes the encoded RGB signal for inspection, as FFmpeg's YUV-to-RGB conversion does;
    // the render crate does transfer-aware conversion at composition.
    let encode = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    [
        encode(encoded[0]),
        encode(encoded[1]),
        encode(encoded[2]),
        255,
    ]
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;

//! Command-line export entry point and argument parsing.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::assets::{ProxyRequest, RequestMedia, resolve_media};
use crate::config::VideoDecodePolicy;
use crate::effect_request::EffectRequest;
use crate::host::Host;
use crate::protocol::{
    AudioQualityRequest, ExportMedia, ExportRequest, HostError, LoudnessRequest, ProjectJson,
    PurposeRequest, RateRequest, Request, VideoQualityRequest,
};
use crate::{PROTOCOL_VERSION, create_host_with, write_stdout};
use alpha2_media::{AudioCodec, Container, VideoCodec};
use serde_json::{Value, json};

/// The command-line synopsis, returned as the error for an unrecognized command line or `--help`.
pub fn usage() -> String {
    "usage: alpha2-host serve\n       alpha2-host conform --project <project.json> --asset <source-id>=<path> [--proxy <source-id>=<path>]... [--purpose preview|preview-proxy|thumbnail|remote-edit|export-original|export-proxy]\n       alpha2-host export --project <project.json> --asset <source-id>=<path> [--proxy <source-id>=<path>]... [--frames <source-id>=<path>,...]... [--resource <source-id> <locator> <path>]... [--media original|proxies] --effect-asset <resource-id>=<path> [--font <font-file>]... --out <output.mp4|.mov|.mkv|.webm> [--overwrite] [--container mp4|mov|mkv|webm] [--effects <effects.json>] [--start-ticks <ticks>] [--end-ticks <ticks>] [--frame-rate <num>/<den>] [--no-audio] [--video-codec <id>] [--alpha] [--bit-depth 8|10] [--video-encoder <ffmpeg-name>] [--input-policy auto|gpu-required|cpu-readback] [--strategy render|prefer-copy|require-copy] [--video-quality very-low|low|medium|high|very-high|<bits-per-second>] [--video-quantizer <n>] [--audio-codec aac|opus] [--audio-quality very-low|low|medium|high|very-high|<bits-per-second>] [--loudness-target <LUFS> [--true-peak-ceiling <dBTP>]] [--video-decode software|auto|hardware-required] [--renderer auto|gpu|cpu] [--render-cache <dir> [--render-cache-quota <bytes>] [--render-cache-backlog skip|wait]]".to_owned()
}

/// The flags shared by `conform` and `export`, as parsed. `include_audio` is true unless
/// `--no-audio` is given; every other field is unset until its flag appears.
#[derive(Default)]
pub struct ExportCliOptions {
    pub project: Option<PathBuf>,
    pub assets: HashMap<String, PathBuf>,
    pub proxies: HashMap<String, Vec<ProxyRequest>>,
    pub frames: HashMap<String, Vec<Option<PathBuf>>>,
    pub resources: HashMap<String, HashMap<String, PathBuf>>,
    pub media: ExportMedia,
    pub purpose: PurposeRequest,
    pub effect_assets: HashMap<String, PathBuf>,
    pub font_files: Vec<PathBuf>,
    pub effects: Option<PathBuf>,
    pub output: Option<PathBuf>,
    /// Replace a regular file at `output` once the export is whole.
    pub overwrite: bool,
    pub start_ticks: Option<i64>,
    pub end_ticks: Option<i64>,
    pub frame_rate: Option<RateRequest>,
    pub include_audio: bool,
    pub container: Option<Container>,
    pub video_codec: Option<VideoCodec>,
    pub alpha: bool,
    pub bit_depth: Option<u8>,
    pub video_encoder: Option<String>,
    pub input_policy: crate::config::VideoInputPolicy,
    pub strategy: crate::config::ExportStrategy,
    pub video_quality: VideoQualityRequest,
    pub video_quantizer: Option<u32>,
    pub audio_codec: Option<AudioCodec>,
    pub audio_quality: AudioQualityRequest,
    pub loudness_target: Option<f64>,
    pub true_peak_ceiling: Option<f64>,
    pub video_decode: Option<VideoDecodePolicy>,
    pub renderer: Option<crate::config::RendererPolicy>,
    pub render_cache: Option<PathBuf>,
    pub render_cache_quota: Option<u64>,
    pub render_cache_backlog: Option<crate::render_cache::Backlog>,
}

/// Prints what a purpose would read for each of a project's sources, and
/// what blocks it, without a GPU or any rendering. Exits unsuccessfully when
/// something blocks, so a script can check media before it exports.
pub fn conform_cli(args: Vec<String>) -> Result<(), HostError> {
    let options = parse_export_args(args)?;
    let project_path = options
        .project
        .as_ref()
        .ok_or_else(|| HostError::invalid_input("--project is required"))?;
    let json = fs::read_to_string(project_path).map_err(HostError::io)?;
    let project = alpha2_model::Project::from_json(&json)
        .map_err(|error| HostError::new("INVALID_INPUT", error.to_string()))?;
    let media = RequestMedia {
        originals: &options.assets,
        proxies: &options.proxies,
        frames: &options.frames,
    };
    let policy = match options.video_decode {
        Some(policy) => policy,
        None => VideoDecodePolicy::from_env().map_err(HostError::invalid_input)?,
    };
    let hardware = crate::host::conform_hardware(policy);
    let resolved = resolve_media(
        &project,
        &media,
        &media,
        options.purpose.purpose(),
        &hardware,
    );
    let ready = resolved.report.ready;
    write_stdout(&json!({
        "ok": ready,
        "result": { "catalog": resolved.catalog, "report": resolved.report },
    }))?;
    if ready {
        Ok(())
    } else {
        Err(HostError::new(
            "CONFORM_FAILED",
            crate::assets::blocking_summary(&resolved.report),
        ))
    }
}

/// Runs one export from command-line flags and prints its result as one JSON object on
/// stdout; progress goes to stderr. The GPU host it made is left in `host`, so that
/// it is released after the outcome is reported (a release can hang, [`crate::teardown`]).
///
/// # Errors
/// Returns invalid flags as `INVALID_INPUT`, and any failure to start the host or to export.
pub fn export_cli(args: Vec<String>, host: &mut Option<Host>) -> Result<(), HostError> {
    let options = parse_export_args(args)?;
    let policy = match options.video_decode {
        Some(policy) => policy,
        None => VideoDecodePolicy::from_env().map_err(HostError::invalid_input)?,
    };
    let renderer = match options.renderer {
        Some(renderer) => renderer,
        None => crate::config::RendererPolicy::from_env().map_err(HostError::invalid_input)?,
    };
    let (request, output) = export_request(options)?;
    // A termination cancels the export, which `main` then reports with the signal's status.
    crate::install_termination();
    crate::termination::terminations().hold_busy();
    let host = host.insert(create_host_with(policy, renderer)?);
    let result = host.handle(request, &mut report_progress)?;
    write_stdout(&json!({
        "ok": true,
        "output": output,
        "result": result,
    }))
}

/// The protocol request `options` ask for, and its output, checked as the
/// protocol checks a request before anything is opened.
///
/// # Errors
/// What `options` leave out or get wrong, as `INVALID_INPUT`, and a project
/// file that cannot be read.
pub fn export_request(options: ExportCliOptions) -> Result<(Request, PathBuf), HostError> {
    let project_path = options
        .project
        .as_ref()
        .ok_or_else(|| HostError::invalid_input("--project is required"))?;
    let output = options
        .output
        .clone()
        .ok_or_else(|| HostError::invalid_input("--out is required"))?;
    let project_bytes = fs::read(project_path).map_err(HostError::io)?;
    let project: Value = serde_json::from_slice(&project_bytes)
        .map_err(|error| HostError::new("INVALID_INPUT", error.to_string()))?;
    let settings = project
        .get("settings")
        .ok_or_else(|| HostError::new("INVALID_INPUT", "project settings are missing"))?;
    let frame_rate = options
        .frame_rate
        .or_else(|| serde_json::from_value(settings.get("frame_rate")?.clone()).ok())
        .ok_or_else(|| HostError::invalid_input("a valid --frame-rate is required"))?;
    let start_ticks = options.start_ticks.unwrap_or(0);
    let end_ticks = end_ticks(options.end_ticks, &project_bytes)?;
    let effects =
        match options.effects {
            Some(path) => serde_json::from_slice::<Vec<EffectRequest>>(
                &fs::read(path).map_err(HostError::io)?,
            )
            .map_err(|error| HostError::invalid_input(format!("invalid effects file: {error}")))?,
            None => Vec::new(),
        };
    let request = ExportRequest {
        protocol_version: PROTOCOL_VERSION,
        request_id: "cli-export".to_owned(),
        project: ProjectJson::from_value(&project)?,
        assets: options.assets,
        proxies: options.proxies,
        frames: options.frames,
        resources: options.resources,
        media: options.media,
        effect_assets: options.effect_assets,
        font_files: options.font_files,
        effects,
        start_ticks,
        end_ticks,
        frame_rate,
        include_audio: options.include_audio,
        container: options.container,
        video_codec: options.video_codec,
        alpha: options.alpha,
        bit_depth: options.bit_depth,
        video_encoder: options.video_encoder,
        input_policy: options.input_policy,
        strategy: options.strategy,
        video_quality: match (options.video_quality, options.video_quantizer) {
            (quality, None) => quality,
            (VideoQualityRequest::Explicit(rate), Some(quantizer)) => {
                VideoQualityRequest::Explicit(alpha2_media::VideoRateControl {
                    quantizer: Some(quantizer),
                    ..rate
                })
            }
            (VideoQualityRequest::Level(_), Some(_)) => {
                return Err(HostError::invalid_input(
                    "--video-quantizer needs --video-quality <bits-per-second> as its fallback",
                ));
            }
        },
        audio_codec: options.audio_codec,
        audio_quality: options.audio_quality,
        loudness: match (options.loudness_target, options.true_peak_ceiling) {
            // EBU R128's broadcast ceiling, and the platforms' usual one.
            (Some(target), ceiling) => Some(
                LoudnessRequest::new(target, ceiling.unwrap_or(-1.0))
                    .map_err(HostError::invalid_input)?,
            ),
            (None, Some(_)) => {
                return Err(HostError::invalid_input(
                    "--true-peak-ceiling applies to --loudness-target",
                ));
            }
            (None, None) => None,
        },
        render_cache: render_cache_request(
            options.render_cache,
            options.render_cache_quota,
            options.render_cache_backlog,
        )?,
        memory: crate::memory::MemoryRequest::default(),
        output: output.clone(),
        overwrite: options.overwrite,
    };
    request.check()?;
    Ok((Request::Export(Box::new(request)), output))
}

#[allow(clippy::unnecessary_wraps)] // The protocol's progress callback signature.
fn report_progress(progress: crate::protocol::Progress) -> Result<(), HostError> {
    alpha2_os::diagnostic!(
        "alpha2-host export: {} {}/{}",
        progress.phase.as_str(),
        progress.completed,
        progress.total
    );
    Ok(())
}

/// `--render-cache`, `--render-cache-quota`, and `--render-cache-backlog` as
/// the protocol's `renderCache`.
fn render_cache_request(
    directory: Option<PathBuf>,
    quota: Option<u64>,
    backlog: Option<crate::render_cache::Backlog>,
) -> Result<Option<crate::render_cache::RenderCacheRequest>, HostError> {
    match (directory, quota, backlog) {
        (Some(directory), quota, backlog) => crate::render_cache::RenderCacheRequest::new(
            directory,
            quota.unwrap_or(crate::render_cache::DEFAULT_QUOTA_BYTES),
            backlog.unwrap_or_default(),
        )
        .map(Some)
        .map_err(HostError::invalid_input),
        (None, None, None) => Ok(None),
        (None, _, _) => Err(HostError::invalid_input(
            "--render-cache-quota and --render-cache-backlog apply to --render-cache",
        )),
    }
}

/// Parses `conform`/`export` flags. Repeatable flags (`--asset`, `--proxy`, `--frames`,
/// `--resource`, `--effect-asset`, `--font`) accumulate.
///
/// # Errors
/// Returns `INVALID_INPUT` for an unknown flag, a missing value, or a value that does not
/// parse.
#[allow(clippy::too_many_lines)]
pub fn parse_export_args(args: Vec<String>) -> Result<ExportCliOptions, HostError> {
    let mut options = ExportCliOptions {
        include_audio: true,
        ..ExportCliOptions::default()
    };
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let value = |args: &mut std::vec::IntoIter<String>| {
            args.next()
                .ok_or_else(|| HostError::invalid_input(format!("{flag} requires a value")))
        };
        match flag.as_str() {
            "--project" => options.project = Some(PathBuf::from(value(&mut args)?)),
            "--asset" => {
                let mapping = value(&mut args)?;
                let (id, path) = mapping.split_once('=').ok_or_else(|| {
                    HostError::invalid_input("--asset must be <source-id>=<path>")
                })?;
                if id.is_empty() || path.is_empty() {
                    return Err(HostError::invalid_input(
                        "--asset must be <source-id>=<path>",
                    ));
                }
                options.assets.insert(id.to_owned(), PathBuf::from(path));
            }
            "--proxy" => {
                let mapping = value(&mut args)?;
                let (id, path) = mapping
                    .split_once('=')
                    .filter(|(id, path)| !id.is_empty() && !path.is_empty())
                    .ok_or_else(|| {
                        HostError::invalid_input("--proxy must be <source-id>=<path>")
                    })?;
                options
                    .proxies
                    .entry(id.to_owned())
                    .or_default()
                    .push(ProxyRequest::at(PathBuf::from(path)));
            }
            // Three values, so neither a locator nor a path has a separator it may not contain.
            "--resource" => {
                let id = value(&mut args)?;
                let locator = value(&mut args)?;
                let path = value(&mut args)?;
                if id.is_empty() || locator.is_empty() || path.is_empty() {
                    return Err(HostError::invalid_input(
                        "--resource takes <source-id> <locator> <path>",
                    ));
                }
                options
                    .resources
                    .entry(id)
                    .or_default()
                    .insert(locator, PathBuf::from(path));
            }
            "--frames" => {
                let mapping = value(&mut args)?;
                let (id, list) = mapping
                    .split_once('=')
                    .filter(|(id, list)| !id.is_empty() && !list.is_empty())
                    .ok_or_else(|| {
                        HostError::invalid_input(
                            "--frames must be <source-id>=<path>[,<path>...], an empty path for a gap",
                        )
                    })?;
                options.frames.insert(
                    id.to_owned(),
                    list.split(',')
                        .map(|path| (!path.is_empty()).then(|| PathBuf::from(path)))
                        .collect(),
                );
            }
            "--media" => {
                options.media = serde_json::from_value(json!(value(&mut args)?))
                    .map_err(|_| HostError::invalid_input("--media must be original or proxies"))?;
            }
            "--purpose" => {
                options.purpose = serde_json::from_value(json!(value(&mut args)?)).map_err(|_| {
                    HostError::invalid_input(
                        "--purpose must be preview, preview-proxy, thumbnail, remote-edit, export-original, or export-proxy",
                    )
                })?;
            }
            "--font" => {
                let path = value(&mut args)?;
                if path.is_empty() {
                    return Err(HostError::invalid_input("--font needs a font file"));
                }
                options.font_files.push(PathBuf::from(path));
            }
            "--effect-asset" => {
                let mapping = value(&mut args)?;
                let (id, path) = mapping.split_once('=').ok_or_else(|| {
                    HostError::invalid_input("--effect-asset must be <resource-id>=<path>")
                })?;
                if id.is_empty() || path.is_empty() {
                    return Err(HostError::invalid_input(
                        "--effect-asset must be <resource-id>=<path>",
                    ));
                }
                options
                    .effect_assets
                    .insert(id.to_owned(), PathBuf::from(path));
            }
            "--effects" => options.effects = Some(PathBuf::from(value(&mut args)?)),
            "--out" => options.output = Some(PathBuf::from(value(&mut args)?)),
            "--start-ticks" => options.start_ticks = Some(parse_arg(&flag, &value(&mut args)?)?),
            "--end-ticks" => options.end_ticks = Some(parse_arg(&flag, &value(&mut args)?)?),
            "--frame-rate" => {
                let rate = value(&mut args)?;
                let (num, den) = rate
                    .split_once('/')
                    .ok_or_else(|| HostError::invalid_input("--frame-rate must be <num>/<den>"))?;
                options.frame_rate = Some(RateRequest {
                    num: parse_arg("--frame-rate numerator", num)?,
                    den: parse_arg("--frame-rate denominator", den)?,
                });
            }
            "--overwrite" => options.overwrite = true,
            "--no-audio" => options.include_audio = false,
            "--include-audio" => options.include_audio = true,
            "--container" => {
                options.container =
                    Some(value(&mut args)?.parse().map_err(|error| {
                        HostError::invalid_input(format!("--container: {error}"))
                    })?);
            }
            "--video-codec" => {
                options.video_codec = Some(value(&mut args)?.parse().map_err(|error| {
                    HostError::invalid_input(format!("--video-codec: {error}"))
                })?);
            }
            "--alpha" => options.alpha = true,
            "--bit-depth" => {
                options.bit_depth = Some(value(&mut args)?.parse().map_err(|_| {
                    HostError::invalid_input("--bit-depth: expected 8 or 10".to_owned())
                })?);
            }
            "--video-encoder" => options.video_encoder = Some(value(&mut args)?),
            "--strategy" => {
                options.strategy = crate::config::ExportStrategy::parse(&value(&mut args)?)
                    .map_err(HostError::invalid_input)?;
            }
            "--input-policy" => {
                options.input_policy = crate::config::VideoInputPolicy::parse(&value(&mut args)?)
                    .map_err(HostError::invalid_input)?;
            }
            "--video-quality" => {
                options.video_quality = VideoQualityRequest::from_cli(&value(&mut args)?)
                    .map_err(HostError::invalid_input)?;
            }
            "--video-quantizer" => {
                let quantizer = parse_arg::<u32>(&flag, &value(&mut args)?)?;
                if quantizer > crate::protocol::MAX_QUANTIZER {
                    return Err(HostError::invalid_input(format!(
                        "--video-quantizer must be at most {}",
                        crate::protocol::MAX_QUANTIZER
                    )));
                }
                options.video_quantizer = Some(quantizer);
            }
            "--renderer" => {
                options.renderer = Some(
                    crate::config::RendererPolicy::parse(&value(&mut args)?)
                        .map_err(HostError::invalid_input)?,
                );
            }
            "--video-decode" => {
                options.video_decode = Some(
                    VideoDecodePolicy::parse(&value(&mut args)?)
                        .map_err(HostError::invalid_input)?,
                );
            }
            "--audio-quality" => {
                options.audio_quality = AudioQualityRequest::from_cli(&value(&mut args)?)
                    .map_err(HostError::invalid_input)?;
            }
            "--audio-codec" => {
                options.audio_codec = Some(value(&mut args)?.parse().map_err(|error| {
                    HostError::invalid_input(format!("--audio-codec: {error}"))
                })?);
            }
            "--loudness-target" => {
                options.loudness_target = Some(parse_arg(&flag, &value(&mut args)?)?);
            }
            "--true-peak-ceiling" => {
                options.true_peak_ceiling = Some(parse_arg(&flag, &value(&mut args)?)?);
            }
            "--render-cache" => options.render_cache = Some(PathBuf::from(value(&mut args)?)),
            "--render-cache-quota" => {
                let quota: u64 = parse_arg(&flag, &value(&mut args)?)?;
                if quota == 0 {
                    return Err(HostError::invalid_input(
                        "--render-cache-quota must be positive",
                    ));
                }
                options.render_cache_quota = Some(quota);
            }
            "--render-cache-backlog" => {
                let backlog = value(&mut args)?;
                options.render_cache_backlog = Some(
                    crate::render_cache::Backlog::parse(&backlog).ok_or_else(|| {
                        HostError::invalid_input("--render-cache-backlog must be skip or wait")
                    })?,
                );
            }
            "--help" | "-h" => return Err(HostError::invalid_input(usage())),
            _ => {
                return Err(HostError::invalid_input(format!(
                    "unknown export option {flag}\n{}",
                    usage()
                )));
            }
        }
    }
    Ok(options)
}

/// Parses `value` for flag `name`.
///
/// # Errors
/// Returns `INVALID_INPUT` naming the flag and the value.
pub fn parse_arg<T: std::str::FromStr>(name: &str, value: &str) -> Result<T, HostError> {
    value
        .parse()
        .map_err(|_| HostError::invalid_input(format!("{name} has an invalid value: {value}")))
}

/// The export's end: `requested`, or where the project ends.
fn end_ticks(requested: Option<i64>, project: &[u8]) -> Result<i64, HostError> {
    match requested {
        Some(end) => Ok(end),
        None => project_end_ticks(project)?.ok_or_else(|| {
            HostError::invalid_input("--end-ticks is required for a project that shows nothing")
        }),
    }
}

/// Where a whole-project export ends: the end of everything the timeline
/// shows, caption cues included ([`alpha2_model::Timeline::extent`]), or
/// `None` for a timeline that shows nothing.
///
/// # Errors
/// Returns `INVALID_INPUT` for a document that is not a valid project.
pub fn project_end_ticks(project: &[u8]) -> Result<Option<i64>, HostError> {
    let text = std::str::from_utf8(project)
        .map_err(|error| HostError::invalid_input(format!("the project is not UTF-8: {error}")))?;
    let project = alpha2_model::Project::from_json(text)
        .map_err(|error| HostError::invalid_input(error.to_string()))?;
    Ok(project.timeline.extent().map(|extent| extent.end().ticks()))
}

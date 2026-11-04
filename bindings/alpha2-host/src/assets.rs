//! A request's media: each source's original and proxies, which one the purpose reads, and what
//! blocks it, before anything renders.
//!
//! The engine asks only for a source's media for a purpose; an [`AssetResolver`] answers for
//! originals and a [`DerivativeProvider`] for proxies.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use alpha2_assets::{
    AssetCatalog, AssetEntry, AudioStreamFacts, ConformIssue, ConformReport, FrameRateEvidence,
    MediaPurpose, OriginalState, ProxyState, ProxyVariant, Selection, conform,
};
use alpha2_extensions::Registry;
use alpha2_media::{SourceId, StreamKind};
use alpha2_model::{
    AudioStreamShape, MediaFingerprint, MediaKind, Project, ProviderSource, StreamTopology,
    VideoStreamShape,
};
use alpha2_time::MediaTime;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// A stream only a hardware session decodes (AV1 without libdav1d) is reported `unsupported` by
/// conform on a software-decode host, rather than failing the export at its first frame.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum HardwareVideoDecode {
    #[default]
    None,
    /// By `FFmpeg` decoder name.
    // Vulkan and VideoToolbox list their codecs; Windows hosts build it in tests alone.
    #[cfg_attr(
        not(all(any(target_os = "linux", target_os = "macos"), feature = "hwdecode")),
        allow(dead_code)
    )]
    Codecs(Vec<&'static str>),
    /// D3D12 says whether it decodes a stream only when a session opens, so streams are assumed
    /// decodable until then.
    PerStream,
}

impl HardwareVideoDecode {
    fn may_decode(&self, codec: &str) -> bool {
        match self {
            Self::None => false,
            Self::Codecs(codecs) => codecs.contains(&codec),
            Self::PerStream => true,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProxyRequest {
    pub path: PathBuf,
    #[serde(default)]
    pub made_from: Option<MediaFingerprint>,
    /// Set when the proxy was tone-mapped.
    #[serde(default)]
    pub color_pipeline: Option<String>,
}

impl ProxyRequest {
    pub const fn at(path: PathBuf) -> Self {
        Self {
            path,
            made_from: None,
            color_pipeline: None,
        }
    }
}

pub trait AssetResolver {
    /// `None` when the host has no media for it.
    fn original(&self, source: &SourceId) -> Option<&Path>;

    /// Frame files in project order (`None` for a gap).
    fn sequence_frames(&self, _source: &SourceId) -> Option<&[Option<PathBuf>]> {
        None
    }
}

pub trait DerivativeProvider {
    /// In order of preference.
    fn proxies(&self, source: &SourceId) -> &[ProxyRequest];
}

pub struct RequestMedia<'a> {
    pub originals: &'a HashMap<String, PathBuf>,
    pub proxies: &'a HashMap<String, Vec<ProxyRequest>>,
    pub frames: &'a HashMap<String, Vec<Option<PathBuf>>>,
}

impl AssetResolver for RequestMedia<'_> {
    fn original(&self, source: &SourceId) -> Option<&Path> {
        self.originals.get(source.as_str()).map(PathBuf::as_path)
    }

    fn sequence_frames(&self, source: &SourceId) -> Option<&[Option<PathBuf>]> {
        self.frames.get(source.as_str()).map(Vec::as_slice)
    }
}

impl DerivativeProvider for RequestMedia<'_> {
    fn proxies(&self, source: &SourceId) -> &[ProxyRequest] {
        self.proxies.get(source.as_str()).map_or(&[], Vec::as_slice)
    }
}

pub struct ResolvedMedia {
    pub catalog: AssetCatalog,
    pub report: ConformReport,
    pub paths: HashMap<String, PathBuf>,
    /// As the probe read them, so the export sizes a decode session without opening the file.
    pub video_frames: HashMap<String, VideoFrameSize>,
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct VideoFrameSize {
    pub pixels: u64,
    /// `StreamInfo::frame_bytes`, or the 8-bit 4:2:0 size when the format is not known.
    pub bytes: u64,
}

impl VideoFrameSize {
    pub fn of(video: &alpha2_media::StreamInfo) -> Option<Self> {
        let (width, height) = video.dimensions?;
        let pixels = u64::from(width).saturating_mul(u64::from(height));
        Some(Self {
            pixels,
            bytes: video
                .frame_bytes
                .unwrap_or_else(|| pixels.saturating_mul(3).div_ceil(2)),
        })
    }
}

/// An original is hashed only when the project recorded a hash to compare, since hashing is a full
/// read of the file.
pub fn resolve_media(
    project: &Project,
    resolver: &dyn AssetResolver,
    derivatives: &dyn DerivativeProvider,
    purpose: MediaPurpose,
    hardware: &HardwareVideoDecode,
) -> ResolvedMedia {
    let mut assets = Vec::new();
    let mut proxy_paths = HashMap::new();
    let mut frames = HashMap::<PathBuf, VideoFrameSize>::new();
    for source in project
        .used_sources()
        .into_iter()
        .filter(|source| source.is_host_supplied())
    {
        let hash_recorded = source
            .fingerprint
            .as_ref()
            .is_some_and(|fingerprint| fingerprint.content_sha256.is_some());
        if let Some(provider) = &source.provider {
            if let Some(original) =
                provider_original(provider, &source.id, resolver, hash_recorded, hardware)
            {
                assets.push(AssetEntry::new(source.id.clone(), 0, original));
            }
            continue;
        }
        let original = resolver.original(&source.id);
        let proxies = derivatives.proxies(&source.id);
        if original.is_none() && proxies.is_empty() {
            continue;
        }
        let hash = hash_recorded;
        let still = source.kind == MediaKind::Image;
        let mut alpha_dropped = None;
        let mut entry = AssetEntry::new(
            source.id.clone(),
            0,
            original.map_or(OriginalState::Offline { path: None }, |path| {
                let (state, found, alpha) = original_state(path, hash, still, hardware);
                frames.extend(found.map(|found| (path.to_path_buf(), found)));
                alpha_dropped = alpha;
                state
            }),
        );
        entry.alpha_dropped = alpha_dropped.map(str::to_owned);
        if matches!(entry.original, OriginalState::Online { .. }) {
            entry.audio_streams = original.and_then(audio_streams);
        }
        for (index, proxy) in proxies.iter().enumerate() {
            let id = format!("{}#proxy{index}", source.id);
            let (variant, found) = proxy_variant(&id, proxy, hardware);
            entry.proxies.push(variant);
            frames.extend(found.map(|found| (proxy.path.clone(), found)));
            proxy_paths.insert(id, proxy.path.clone());
        }
        assets.push(entry);
    }
    let catalog = AssetCatalog::new(assets);
    let report = conform(project, &catalog, purpose);
    let paths = report
        .sources
        .iter()
        .filter_map(|source| {
            let path = match &source.selection {
                Selection::Original => resolver.original(&source.source_id)?.to_path_buf(),
                Selection::Proxy { id } => proxy_paths.get(id)?.clone(),
                _ => return None,
            };
            Some((source.source_id.as_str().to_owned(), path))
        })
        .collect::<HashMap<_, _>>();
    let video_frames = paths
        .iter()
        .filter_map(|(id, path)| Some((id.clone(), *frames.get(path)?)))
        .collect();
    ResolvedMedia {
        catalog,
        report,
        paths,
        video_frames,
    }
}

fn offline(path: &Path) -> OriginalState {
    OriginalState::Offline {
        path: Some(path.display().to_string()),
    }
}

/// One line per blocking issue, for a person: the path an offline source was looked up at (with a
/// same-name sibling in another Unicode normalization), and why a source is unsupported.
pub fn blocking_summary(report: &ConformReport) -> String {
    let issues = report
        .blocking()
        .map(|(source, issue)| match issue {
            ConformIssue::Offline { path: Some(path) } => {
                let sibling = crate::paths::normalization_sibling(Path::new(path))
                    .and_then(|other| other.file_name().map(|name| name.display().to_string()))
                    .map(|name| {
                        format!(
                            " (a file named {name} exists: the names differ in Unicode normalization)"
                        )
                    })
                    .unwrap_or_default();
                format!("{source}: offline ({path} not found){sibling}")
            }
            ConformIssue::Unsupported { reason } => format!("{source}: unsupported ({reason})"),
            ConformIssue::AudioSelection { clip, reason } => {
                format!("{source}: clip {clip} plays missing audio ({reason})")
            }
            _ => {
                let kind = serde_json::to_value(issue)
                    .ok()
                    .and_then(|value| value.get("kind")?.as_str().map(str::to_owned))
                    .unwrap_or_default();
                format!("{source}: {}", kind.replace('_', " "))
            }
        })
        .collect::<Vec<_>>();
    format!("the project's media is not ready: {}", issues.join("; "))
}

/// `None` when the request supplies nothing for it.
fn provider_original(
    provider: &ProviderSource,
    source: &SourceId,
    resolver: &dyn AssetResolver,
    hash: bool,
    hardware: &HardwareVideoDecode,
) -> Option<OriginalState> {
    let registry = Registry::with_defaults();
    if !registry.supports(provider) {
        return Some(OriginalState::Unsupported {
            reason: "this build has no provider for the source's format".into(),
        });
    }
    if let ProviderSource::ImageSequence {
        frames, frame_rate, ..
    } = provider
    {
        return sequence_original(&registry, frames, *frame_rate, source, resolver, hardware);
    }
    if matches!(provider, ProviderSource::Latex { .. }) {
        return Some(latex_original(&registry, provider));
    }
    let path = resolver.original(source)?;
    if !path.is_file() {
        return Some(offline(path));
    }
    let bytes = match crate::providers::read_source(path, provider) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Some(OriginalState::Unsupported {
                reason: error.to_string(),
            });
        }
    };
    // A Lottie source shows the animation it names, or its package's first, and a PDF source the
    // page it names, so describe that one.
    let found = match provider {
        ProviderSource::Lottie { .. } | ProviderSource::Pdf { .. } => Some(
            registry
                .open(
                    provider,
                    Some(bytes.clone().into()),
                    std::sync::Arc::new(alpha2_extensions::NoResources),
                )
                .map(|source| source.report().clone()),
        ),
        _ => registry
            .probe(&bytes)
            .map(|found| found.map(|(_, report)| report)),
    };
    Some(match found {
        Some(Ok(report)) => {
            let metadata = std::fs::metadata(path).ok();
            OriginalState::Online {
                fingerprint: MediaFingerprint {
                    content_sha256: hash.then(|| hex(&Sha256::digest(&bytes))),
                    byte_length: Some(bytes.len() as u64),
                    modified_ms: metadata
                        .and_then(|metadata| metadata.modified().ok())
                        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                        .and_then(|since| i64::try_from(since.as_millis()).ok()),
                    streams: Some(StreamTopology {
                        video: Some(VideoStreamShape::new(report.width, report.height, None)),
                        audio: None,
                    }),
                    duration_ticks: report.timing.duration().map(MediaTime::ticks),
                    color: None,
                },
            }
        }
        Some(Err(error)) => OriginalState::Unsupported {
            reason: error.to_string(),
        },
        None => OriginalState::Unsupported {
            reason: "no provider reads the file".into(),
        },
    })
}

/// A LaTeX source reads no file, so it is online when its formula typesets.
fn latex_original(registry: &Registry, provider: &ProviderSource) -> OriginalState {
    match registry.open(
        provider,
        None,
        std::sync::Arc::new(alpha2_extensions::NoResources),
    ) {
        Ok(source) => OriginalState::Online {
            fingerprint: MediaFingerprint {
                content_sha256: None,
                byte_length: None,
                modified_ms: None,
                streams: Some(StreamTopology {
                    video: Some(VideoStreamShape::new(
                        source.report().width,
                        source.report().height,
                        None,
                    )),
                    audio: None,
                }),
                duration_ticks: None,
                color: None,
            },
        },
        Err(error) => OriginalState::Unsupported {
            reason: error.to_string(),
        },
    }
}

/// Shape from the first frame; `None` when the request supplies no files.
fn sequence_original(
    registry: &Registry,
    locators: &[Option<String>],
    frame_rate: alpha2_time::FrameRate,
    source: &SourceId,
    resolver: &dyn AssetResolver,
    hardware: &HardwareVideoDecode,
) -> Option<OriginalState> {
    let paths = resolver.sequence_frames(source)?;
    if let Err(reason) = crate::providers::frame_files(locators, paths) {
        return Some(OriginalState::Unsupported { reason });
    }
    let frames = locators.len();
    if let Some(missing) = paths.iter().flatten().find(|path| !path.is_file()) {
        return Some(offline(missing));
    }
    let first = paths.iter().flatten().next()?;
    let timing = alpha2_extensions::Timing::Rate {
        rate: frame_rate,
        frames,
    };
    // OpenEXR frames are the provider's to read; PNG frames the image decoder's.
    let streams = match std::fs::read(first) {
        Ok(bytes) => match registry.probe(&bytes) {
            Some(Ok((_, report))) => Ok(Some(StreamTopology {
                video: Some(VideoStreamShape::new(report.width, report.height, None)),
                audio: None,
            })),
            Some(Err(error)) => Err(error.to_string()),
            None => fingerprint(first, false, true, hardware).map(|shape| shape.streams),
        },
        Err(error) => Err(error.to_string()),
    };
    Some(match streams {
        Ok(streams) => OriginalState::Online {
            fingerprint: MediaFingerprint {
                streams,
                duration_ticks: timing.duration().map(MediaTime::ticks),
                ..MediaFingerprint::default()
            },
        },
        Err(reason) => OriginalState::Unsupported { reason },
    })
}

fn original_state(
    path: &Path,
    hash: bool,
    still: bool,
    hardware: &HardwareVideoDecode,
) -> (OriginalState, Option<VideoFrameSize>, Option<&'static str>) {
    if !path.is_file() {
        return (offline(path), None, None);
    }
    match probe_media(path, hash, still, hardware) {
        Ok((fingerprint, frames, alpha)) => (OriginalState::Online { fingerprint }, frames, alpha),
        Err(reason) => (OriginalState::Unsupported { reason }, None, None),
    }
}

/// Every audio stream of the media at `path` in demuxer order, which `audio.stream` counts over;
/// `None` when it cannot be probed.
fn audio_streams(path: &Path) -> Option<Vec<AudioStreamFacts>> {
    let probe = alpha2_native::probe(path).ok()?;
    let default = probe.selected_audio_ordinal();
    Some(
        probe
            .audio_streams()
            .zip(0_u32..)
            .map(|(stream, ordinal)| {
                let mut facts =
                    AudioStreamFacts::new(stream.channels.unwrap_or(0), Some(ordinal) == default);
                facts.codec = Some(stream.codec.clone());
                facts.language.clone_from(&stream.language);
                facts.title.clone_from(&stream.title);
                facts
            })
            .collect(),
    )
}

fn proxy_variant(
    id: &str,
    proxy: &ProxyRequest,
    hardware: &HardwareVideoDecode,
) -> (ProxyVariant, Option<VideoFrameSize>) {
    let found = proxy
        .path
        .is_file()
        .then(|| probe_media(&proxy.path, false, false, hardware))
        .ok_or_else(|| "the proxy file is missing".to_owned())
        .and_then(|result| result);
    let (state, shape, frames) = match found {
        Ok((fingerprint, frames, _)) => (ProxyState::Ready, Some(fingerprint), frames),
        Err(reason) => (ProxyState::Failed { reason }, None, None),
    };
    let streams = shape.as_ref().and_then(|shape| shape.streams.as_ref());
    let video = streams.and_then(|streams| streams.video.as_ref());
    let variant = ProxyVariant {
        id: id.to_owned(),
        source_revision: 0,
        made_from: proxy.made_from.clone().unwrap_or_default(),
        width: video.map_or(0, |video| video.width),
        height: video.map_or(0, |video| video.height),
        frame_rate: video.and_then(|video| video.frame_rate),
        duration_ticks: shape.as_ref().and_then(|shape| shape.duration_ticks),
        audio_sample_rate: streams
            .and_then(|streams| streams.audio.as_ref())
            .map(|audio| audio.sample_rate),
        color_pipeline: proxy.color_pipeline.clone(),
        state,
    };
    (variant, frames)
}

/// Evidence of the media at `path`: length, modification time, what the demuxer finds, and with
/// `hash` a SHA-256 of every byte.
///
/// Sizes are displayed sizes, as a browser demuxer reports them: stretched by the pixel aspect
/// ratio and turned by the orientation (Exif included). A `still` records no rate or duration,
/// since an image demuxer reports only its own defaults.
///
/// # Errors
/// Returns why the file cannot be read or demuxed, or why its video cannot be decoded here.
pub fn fingerprint(
    path: &Path,
    hash: bool,
    still: bool,
    hardware: &HardwareVideoDecode,
) -> Result<MediaFingerprint, String> {
    probe_media(path, hash, still, hardware).map(|(fingerprint, _, _)| fingerprint)
}

fn probe_media(
    path: &Path,
    hash: bool,
    still: bool,
    hardware: &HardwareVideoDecode,
) -> Result<
    (
        MediaFingerprint,
        Option<VideoFrameSize>,
        Option<&'static str>,
    ),
    String,
> {
    let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
    let probe = alpha2_native::probe(path).map_err(|error| format!("{error}"))?;
    let stored_frames = probe.selected_video_stream().and_then(VideoFrameSize::of);
    let video = probe
        .selected_video_stream()
        .filter(|video| video.kind == StreamKind::Video);
    if let Some(video) = video
        && let Some(reason) = alpha2_native::software_decode_unsupported(video)
        && !hardware.may_decode(&video.codec)
    {
        return Err(reason);
    }
    let audio = probe
        .selected_audio_stream()
        .filter(|audio| audio.kind == StreamKind::Audio);
    let video_shape = video.and_then(|video| {
        let (width, height) = video.dimensions?;
        let (width, height) = video.sample_aspect_ratio.square_pixel_size(width, height);
        let (width, height) = video.orientation.displayed_size(width, height);
        let rate = (!still).then(|| measured_rate(path, video.index)).flatten();
        Some(VideoStreamShape::new(width, height, rate))
    });
    let audio_shape =
        audio.and_then(|audio| Some(AudioStreamShape::new(audio.channels?, audio.sample_rate?)));
    let fingerprint = MediaFingerprint {
        content_sha256: hash.then(|| sha256(path)).transpose()?,
        byte_length: Some(metadata.len()),
        modified_ms: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .and_then(|since| i64::try_from(since.as_millis()).ok()),
        streams: (video_shape.is_some() || audio_shape.is_some()).then_some(StreamTopology {
            video: video_shape,
            audio: audio_shape,
        }),
        // As a browser demuxer reports it: a transport stream that starts at 1.4 s and lasts 2 s
        // ends at 3.4 s.
        duration_ticks: probe.duration.filter(|_| !still).map(|duration| {
            probe
                .start
                .map_or(0, alpha2_time::MediaTime::ticks)
                .saturating_add(duration.ticks())
                .max(0)
        }),
        color: video.and_then(|video| video.color),
    };
    let alpha_dropped = video.and_then(alpha2_native::alpha_dropped_reason);
    Ok((fingerprint, stored_frames, alpha_dropped))
}

/// `None` when it cannot be measured, as a browser then records none.
fn measured_rate(path: &Path, index: usize) -> Option<alpha2_time::FrameRate> {
    let mut evidence = FrameRateEvidence::new();
    alpha2_native::visit_packet_times(path, index, |seconds| evidence.push(seconds)).ok()?;
    evidence.rate()
}

fn sha256(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

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
    MediaPurpose, OriginalState, ProxyState, ProxyVariant, Selection, conform_sources,
};
use alpha2_extensions::Registry;
use alpha2_media::{SourceId, StreamKind};
use alpha2_model::{
    AudioStreamShape, MediaFingerprint, MediaKind, Project, ProviderSource, StreamTopology,
    VideoStreamShape,
};
use alpha2_time::MediaTime;
use sha2::{Digest, Sha256};

use alpha2_host_core::remote::SourceLocation;
use alpha2_host_core::requests::MediaLocation;
pub use alpha2_host_core::requests::ProxyRequest;

use crate::protocol::HostError;
use crate::remote_media::{Located, RemoteInputs};

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

pub trait AssetResolver {
    /// `None` when the host has no media for it.
    ///
    /// # Errors
    /// A remote original whose probe failed the request.
    fn original(&self, source: &SourceId) -> Option<Result<Located, HostError>>;

    /// Whether a remote original whose project records a hash is read whole and hashed.
    fn hashes_remote(&self) -> bool {
        false
    }

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
    pub originals: &'a HashMap<String, MediaLocation>,
    pub proxies: &'a HashMap<String, Vec<ProxyRequest>>,
    pub frames: &'a HashMap<String, Vec<Option<PathBuf>>>,
    /// The request's remote sources, when it names a URL.
    pub remote: Option<&'a RemoteInputs>,
}

impl AssetResolver for RequestMedia<'_> {
    fn original(&self, source: &SourceId) -> Option<Result<Located, HostError>> {
        Some(match self.originals.get(source.as_str())? {
            MediaLocation::Path(path) => Ok(Located::Found(SourceLocation::File(path.clone()))),
            MediaLocation::Url(url) => match self.remote {
                Some(remote) => remote.locate(source.as_str(), url),
                None => Err(HostError::new(
                    "INTERNAL",
                    "a remote source was read without its request's session",
                )),
            },
        })
    }

    fn hashes_remote(&self) -> bool {
        self.remote.is_some_and(RemoteInputs::verifies)
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
    pub paths: HashMap<String, SourceLocation>,
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
/// read of the file; a remote one only when the request also asks to verify it.
///
/// # Errors
/// A remote source that fails the request: refused, changed, unreachable, or cancelled.
pub fn resolve_media(
    project: &Project,
    resolver: &dyn AssetResolver,
    derivatives: &dyn DerivativeProvider,
    purpose: MediaPurpose,
    hardware: &HardwareVideoDecode,
) -> Result<ResolvedMedia, HostError> {
    resolve_sources(
        project,
        project.used_sources(),
        resolver,
        derivatives,
        purpose,
        hardware,
    )
}

/// [`resolve_media`] over `sources` instead of the ones a render reads.
///
/// # Errors
/// As [`resolve_media`].
pub fn resolve_sources(
    project: &Project,
    sources: Vec<&alpha2_model::MediaSource>,
    resolver: &dyn AssetResolver,
    derivatives: &dyn DerivativeProvider,
    purpose: MediaPurpose,
    hardware: &HardwareVideoDecode,
) -> Result<ResolvedMedia, HostError> {
    let mut assets = Vec::new();
    let mut proxy_paths = HashMap::new();
    let mut originals = HashMap::<SourceId, SourceLocation>::new();
    let mut frames = HashMap::<SourceLocation, VideoFrameSize>::new();
    let wanted = sources
        .iter()
        .map(|source| source.id.clone())
        .collect::<Vec<_>>();
    for source in sources
        .into_iter()
        .filter(|source| source.is_host_supplied())
    {
        let hash_recorded = source
            .fingerprint
            .as_ref()
            .is_some_and(|fingerprint| fingerprint.content_sha256.is_some());
        let original = resolver.original(&source.id).transpose()?;
        if let Some(Located::Found(location)) = &original {
            originals.insert(source.id.clone(), location.clone());
        }
        if let Some(provider) = &source.provider {
            let hash = hash_recorded;
            if let Some(original) =
                provider_original(provider, &source.id, original, resolver, hash, hardware)?
            {
                assets.push(AssetEntry::new(source.id.clone(), 0, original));
            }
            continue;
        }
        let proxies = derivatives.proxies(&source.id);
        if original.is_none() && proxies.is_empty() {
            continue;
        }
        let still = source.kind == MediaKind::Image;
        let mut alpha_dropped = None;
        let state = match &original {
            None => OriginalState::Offline { path: None },
            Some(Located::Offline(shown)) => OriginalState::Offline {
                path: Some(shown.clone()),
            },
            Some(Located::Unsupported(reason)) => OriginalState::Unsupported {
                reason: reason.clone(),
            },
            Some(Located::Found(location)) => {
                let hash = hash_recorded && (location.path().is_some() || resolver.hashes_remote());
                let (state, found, alpha) = original_state(location, hash, still, hardware)?;
                frames.extend(found.map(|found| (location.clone(), found)));
                alpha_dropped = alpha;
                state
            }
        };
        let mut entry = AssetEntry::new(source.id.clone(), 0, state);
        entry.alpha_dropped = alpha_dropped.map(str::to_owned);
        if let (OriginalState::Online { .. }, Some(Located::Found(location))) =
            (&entry.original, &original)
        {
            entry.audio_streams = audio_streams(location)?;
        }
        add_proxies(&mut entry, proxies, hardware, &mut frames, &mut proxy_paths);
        assets.push(entry);
    }
    let catalog = AssetCatalog::new(assets);
    let report = conform_sources(
        project,
        project
            .sources
            .iter()
            .filter(|source| wanted.contains(&source.id))
            .collect(),
        &catalog,
        purpose,
    );
    let paths = report
        .sources
        .iter()
        .filter_map(|source| {
            let path = match &source.selection {
                Selection::Original => originals.get(&source.source_id)?.clone(),
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
    Ok(ResolvedMedia {
        catalog,
        report,
        paths,
        video_frames,
    })
}

fn add_proxies(
    entry: &mut AssetEntry,
    proxies: &[ProxyRequest],
    hardware: &HardwareVideoDecode,
    frames: &mut HashMap<SourceLocation, VideoFrameSize>,
    proxy_paths: &mut HashMap<String, SourceLocation>,
) {
    for (index, proxy) in proxies.iter().enumerate() {
        let id = format!("{}#proxy{index}", entry.source_id);
        let (variant, found) = proxy_variant(&id, proxy, hardware);
        entry.proxies.push(variant);
        let location = SourceLocation::File(proxy.path.clone());
        frames.extend(found.map(|found| (location.clone(), found)));
        proxy_paths.insert(id, location);
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
    original: Option<Located>,
    resolver: &dyn AssetResolver,
    hash: bool,
    hardware: &HardwareVideoDecode,
) -> Result<Option<OriginalState>, HostError> {
    let registry = Registry::with_defaults();
    if !registry.supports(provider) {
        return Ok(Some(OriginalState::Unsupported {
            reason: "this build has no provider for the source's format".into(),
        }));
    }
    if let ProviderSource::ImageSequence {
        frames, frame_rate, ..
    } = provider
    {
        return Ok(sequence_original(
            &registry,
            frames,
            *frame_rate,
            source,
            resolver,
            hardware,
        ));
    }
    if !provider.reads_file() {
        return Ok(Some(fileless_original(&registry, provider)));
    }
    let location = match original {
        None => return Ok(None),
        Some(Located::Offline(shown)) => {
            return Ok(Some(OriginalState::Offline { path: Some(shown) }));
        }
        Some(Located::Unsupported(reason)) => {
            return Ok(Some(OriginalState::Unsupported { reason }));
        }
        Some(Located::Found(location)) => location,
    };
    if let Some(path) = location.path()
        && !path.is_file()
    {
        return Ok(Some(offline(path)));
    }
    let bytes = match alpha2_host_core::providers::read_source(&location, provider) {
        Ok(bytes) => bytes,
        Err(alpha2_host_core::providers::SourceReadError::Remote(error)) => {
            let url = location
                .remote()
                .map_or_else(String::new, |object| object.shown_url().to_owned());
            return Err(crate::media_policy::media_error(
                source.as_str(),
                &url,
                &error,
            ));
        }
        Err(error) => {
            return Ok(Some(OriginalState::Unsupported {
                reason: error.to_string(),
            }));
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
    Ok(Some(match found {
        Some(Ok(report)) => OriginalState::Online {
            fingerprint: MediaFingerprint {
                content_sha256: hash.then(|| hex(&Sha256::digest(&bytes))),
                byte_length: Some(bytes.len() as u64),
                modified_ms: modified_ms(&location),
                streams: Some(StreamTopology {
                    video: Some(VideoStreamShape::new(report.width, report.height, None)),
                    audio: None,
                }),
                duration_ticks: report.timing.duration().map(MediaTime::ticks),
                color: None,
            },
        },
        Some(Err(error)) => OriginalState::Unsupported {
            reason: error.to_string(),
        },
        None => OriginalState::Unsupported {
            reason: "no provider reads the file".into(),
        },
    }))
}

/// A file's modification time, or a remote object's `Last-Modified`, in Unix milliseconds.
fn modified_ms(location: &SourceLocation) -> Option<i64> {
    match location {
        SourceLocation::File(path) => std::fs::metadata(path)
            .ok()?
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|since| i64::try_from(since.as_millis()).ok()),
        SourceLocation::Remote(object) => object.last_modified_ms(),
    }
}

/// A source whose provider reads no file is online when its provider opens it.
fn fileless_original(registry: &Registry, provider: &ProviderSource) -> OriginalState {
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
    if let Err(reason) = alpha2_host_core::providers::frame_files(locators, paths) {
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
    location: &SourceLocation,
    hash: bool,
    still: bool,
    hardware: &HardwareVideoDecode,
) -> Result<(OriginalState, Option<VideoFrameSize>, Option<&'static str>), HostError> {
    if let Some(path) = location.path()
        && !path.is_file()
    {
        return Ok((offline(path), None, None));
    }
    Ok(match probe_media(location, hash, still, hardware) {
        Ok((fingerprint, frames, alpha)) => (OriginalState::Online { fingerprint }, frames, alpha),
        Err(ProbeFailure::Unsupported(reason)) => {
            (OriginalState::Unsupported { reason }, None, None)
        }
        Err(ProbeFailure::Request(error)) => return Err(error),
    })
}

/// Why a probe found no media: the source is not usable, or the request failed (a remote source
/// that changed or could not be read, a cancel).
enum ProbeFailure {
    Unsupported(String),
    Request(HostError),
}

impl ProbeFailure {
    fn of(error: alpha2_media::DecodeError) -> Self {
        match error {
            alpha2_media::DecodeError::Cancelled => Self::Request(HostError::new(
                "CANCELLED",
                crate::protocol::cancelled("the request was cancelled while it read its media"),
            )),
            alpha2_media::DecodeError::Source(failure) => {
                Self::Request(crate::media_policy::source_error(&failure))
            }
            error => Self::Unsupported(error.to_string()),
        }
    }
}

/// Every audio stream of the media at `location` in demuxer order, which `audio.stream` counts
/// over; `None` when it cannot be probed.
fn audio_streams(location: &SourceLocation) -> Result<Option<Vec<AudioStreamFacts>>, HostError> {
    let probe = match alpha2_native::probe(location) {
        Ok(probe) => probe,
        Err(error) => {
            return match ProbeFailure::of(error) {
                ProbeFailure::Request(error) => Err(error),
                ProbeFailure::Unsupported(_) => Ok(None),
            };
        }
    };
    let default = probe.selected_audio_ordinal();
    Ok(Some(
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
    ))
}

fn proxy_variant(
    id: &str,
    proxy: &ProxyRequest,
    hardware: &HardwareVideoDecode,
) -> (ProxyVariant, Option<VideoFrameSize>) {
    let found = proxy
        .path
        .is_file()
        .then(|| {
            probe_media(&SourceLocation::from(&proxy.path), false, false, hardware).map_err(
                |failure| match failure {
                    ProbeFailure::Unsupported(reason) => reason,
                    ProbeFailure::Request(error) => error.message,
                },
            )
        })
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
    probe_media(&SourceLocation::from(path), hash, still, hardware)
        .map(|(fingerprint, _, _)| fingerprint)
        .map_err(|failure| match failure {
            ProbeFailure::Unsupported(reason) => reason,
            ProbeFailure::Request(error) => error.message,
        })
}

type ProbedMedia = (
    MediaFingerprint,
    Option<VideoFrameSize>,
    Option<&'static str>,
);

fn probe_media(
    location: &SourceLocation,
    hash: bool,
    still: bool,
    hardware: &HardwareVideoDecode,
) -> Result<ProbedMedia, ProbeFailure> {
    crate::heartbeat::request_step(crate::heartbeat::RequestSide::Input, "conform");
    let byte_length = location
        .byte_length()
        .map_err(|error| ProbeFailure::Unsupported(error.to_string()))?;
    let probe = alpha2_native::probe(location).map_err(ProbeFailure::of)?;
    let stored_frames = probe.selected_video_stream().and_then(VideoFrameSize::of);
    let video = probe
        .selected_video_stream()
        .filter(|video| video.kind == StreamKind::Video);
    if let Some(video) = video
        && let Some(reason) = alpha2_native::software_decode_unsupported(video)
        && !hardware.may_decode(&video.codec)
    {
        return Err(ProbeFailure::Unsupported(reason));
    }
    let audio = probe
        .selected_audio_stream()
        .filter(|audio| audio.kind == StreamKind::Audio);
    let rate = match video {
        Some(video) if !still => measured_rate(location, video.index)?,
        _ => None,
    };
    let video_shape = video.and_then(|video| {
        let (width, height) = video.dimensions?;
        let (width, height) = video.sample_aspect_ratio.square_pixel_size(width, height);
        let (width, height) = video.orientation.displayed_size(width, height);
        Some(VideoStreamShape::new(width, height, rate))
    });
    let audio_shape =
        audio.and_then(|audio| Some(AudioStreamShape::new(audio.channels?, audio.sample_rate?)));
    let fingerprint = MediaFingerprint {
        content_sha256: hash.then(|| sha256(location)).transpose()?,
        byte_length: Some(byte_length),
        modified_ms: modified_ms(location),
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
fn measured_rate(
    location: &SourceLocation,
    index: usize,
) -> Result<Option<alpha2_time::FrameRate>, ProbeFailure> {
    let mut evidence = FrameRateEvidence::new();
    let visited = alpha2_native::visit_packet_times(location, index, |seconds| {
        crate::heartbeat::beat_request();
        evidence.push(seconds)
    });
    match visited.map_err(ProbeFailure::of) {
        Ok(()) => Ok(evidence.rate()),
        Err(ProbeFailure::Unsupported(_)) => Ok(None),
        Err(failure) => Err(failure),
    }
}

/// Milliseconds each 1 MiB of hashing takes on top of its work, for a step whose length is known.
#[cfg(test)]
pub static INJECT_HASH_CHUNK_DELAY_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn sha256(location: &SourceLocation) -> Result<String, ProbeFailure> {
    let failed = |error: std::io::Error| match alpha2_host_core::remote::decode_error(&error) {
        Some(error) => ProbeFailure::of(error),
        None => ProbeFailure::Unsupported(error.to_string()),
    };
    let mut file: Box<dyn Read> = match location {
        SourceLocation::File(path) => Box::new(File::open(path).map_err(failed)?),
        SourceLocation::Remote(object) => Box::new(object.stream()),
    };
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        crate::heartbeat::beat_request();
        #[cfg(test)]
        std::thread::sleep(std::time::Duration::from_millis(
            INJECT_HASH_CHUNK_DELAY_MS.load(std::sync::atomic::Ordering::Relaxed),
        ));
        let read = file.read(&mut buffer).map_err(failed)?;
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

//! A request's media: what each source's original and proxies are, which the
//! purpose reads, and what stands in the way, before anything renders.
//!
//! The engine asks only for a source's media for a purpose. Where it lives
//! is the host's: an [`AssetResolver`] answers for originals and a
//! [`DerivativeProvider`] for proxies. The request's `assets` and `proxies`
//! maps are the implementation this host ships.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use alpha2_assets::{
    AssetCatalog, AssetEntry, ConformReport, FrameRateEvidence, MediaPurpose, OriginalState,
    ProxyState, ProxyVariant, Selection, conform,
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

/// Which video codecs this host can decode through a hardware session.
///
/// A stream only a hardware session decodes (AV1 on a runtime without libdav1d) is
/// readable exactly where one can: conform reports it `unsupported` on a host
/// that decodes in software, rather than an export failing at its first frame.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum HardwareVideoDecode {
    /// No hardware session: every video stream is decoded in software.
    #[default]
    None,
    /// A device that decodes these codecs, by `FFmpeg` decoder name.
    // Only a Vulkan device lists its codecs; macOS and Windows hosts build it in tests alone.
    #[cfg_attr(not(all(target_os = "linux", feature = "hwdecode")), allow(dead_code))]
    Codecs(Vec<&'static str>),
    /// A device that says whether it decodes a stream only when a session
    /// for that stream opens (`VideoToolbox`, D3D12), so a stream is assumed
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

/// A proxy the caller made, with what it was made from.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProxyRequest {
    /// Where it is.
    pub path: PathBuf,
    /// Evidence of the original it was made from, when the caller kept it.
    #[serde(default)]
    pub made_from: Option<MediaFingerprint>,
    /// The color pipeline baked into it, when it was tone-mapped.
    #[serde(default)]
    pub color_pipeline: Option<String>,
}

impl ProxyRequest {
    /// A proxy at `path` the caller knows nothing more about.
    pub const fn at(path: PathBuf) -> Self {
        Self {
            path,
            made_from: None,
            color_pipeline: None,
        }
    }
}

/// Where a source's original is.
pub trait AssetResolver {
    /// The original's location, or `None` when the host has no media for it.
    fn original(&self, source: &SourceId) -> Option<&Path>;

    /// An image sequence's frame files in the project's order (`None` where
    /// the sequence has a gap), or `None` when the host has none for it.
    fn sequence_frames(&self, _source: &SourceId) -> Option<&[Option<PathBuf>]> {
        None
    }
}

/// The proxies made from a source.
pub trait DerivativeProvider {
    /// Every proxy of `source`, in order of preference.
    fn proxies(&self, source: &SourceId) -> &[ProxyRequest];
}

/// The request's own maps.
pub struct RequestMedia<'a> {
    pub originals: &'a HashMap<String, PathBuf>,
    pub proxies: &'a HashMap<String, Vec<ProxyRequest>>,
    /// Image sequences' frame files, by source ID.
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

/// A project's media for one purpose.
pub struct ResolvedMedia {
    /// What the host found.
    pub catalog: AssetCatalog,
    /// What the purpose reads, and what blocks it.
    pub report: ConformReport,
    /// Each readable source's selected file, by source ID.
    pub paths: HashMap<String, PathBuf>,
    /// Luma samples in the stored frames of each selected file's video
    /// stream, by source ID, as the probe above read them: what sizes a
    /// software decode session, so the export need not open the file again.
    pub video_pixels: HashMap<String, u64>,
}

/// Finds and checks the media of every host-supplied source `project` reads.
///
/// An original is hashed only when the project recorded a hash to compare:
/// a hash nobody checks is a full read of the file for nothing.
pub fn resolve_media(
    project: &Project,
    resolver: &dyn AssetResolver,
    derivatives: &dyn DerivativeProvider,
    purpose: MediaPurpose,
    hardware: &HardwareVideoDecode,
) -> ResolvedMedia {
    let mut assets = Vec::new();
    let mut proxy_paths = HashMap::new();
    let mut pixels = HashMap::<PathBuf, u64>::new();
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
        let mut entry = AssetEntry::new(
            source.id.clone(),
            0,
            original.map_or(OriginalState::Offline, |path| {
                let (state, found) = original_state(path, hash, still, hardware);
                pixels.extend(found.map(|found| (path.to_path_buf(), found)));
                state
            }),
        );
        for (index, proxy) in proxies.iter().enumerate() {
            let id = format!("{}#proxy{index}", source.id);
            let (variant, found) = proxy_variant(&id, proxy, hardware);
            entry.proxies.push(variant);
            pixels.extend(found.map(|found| (proxy.path.clone(), found)));
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
    let video_pixels = paths
        .iter()
        .filter_map(|(id, path)| Some((id.clone(), *pixels.get(path)?)))
        .collect();
    ResolvedMedia {
        catalog,
        report,
        paths,
        video_pixels,
    }
}

/// One line per blocking issue, for a person reading an error.
pub fn blocking_summary(report: &ConformReport) -> String {
    let issues = report
        .blocking()
        .map(|(source, issue)| {
            let kind = serde_json::to_value(issue)
                .ok()
                .and_then(|value| value.get("kind")?.as_str().map(str::to_owned))
                .unwrap_or_default();
            format!("{source}: {}", kind.replace('_', " "))
        })
        .collect::<Vec<_>>();
    format!("the project's media is not ready: {}", issues.join("; "))
}

/// What an extension source's media is, found through its provider rather
/// than a demuxer: `None` when the request supplies nothing for it.
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
    let path = resolver.original(source)?;
    if !path.is_file() {
        return Some(OriginalState::Offline);
    }
    let bytes = match crate::providers::read_source(path, provider) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Some(OriginalState::Unsupported {
                reason: error.to_string(),
            });
        }
    };
    // A Lottie source shows the animation it names, or its package's first: opened, it describes
    // that one, not the first a probe finds that opens.
    let found = match provider {
        ProviderSource::Lottie { .. } => Some(
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

/// What an image sequence's frames are: their shape, from the first frame,
/// and the sequence's duration; `None` when the request supplies no files.
/// The files must be the project's frames, place for place, as the export
/// reads them.
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
    if paths.iter().flatten().any(|path| !path.is_file()) {
        return Some(OriginalState::Offline);
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

/// What an original is, and the luma samples of its stored video frames.
fn original_state(
    path: &Path,
    hash: bool,
    still: bool,
    hardware: &HardwareVideoDecode,
) -> (OriginalState, Option<u64>) {
    if !path.is_file() {
        return (OriginalState::Offline, None);
    }
    match probe_media(path, hash, still, hardware) {
        Ok((fingerprint, pixels)) => (OriginalState::Online { fingerprint }, pixels),
        Err(reason) => (OriginalState::Unsupported { reason }, None),
    }
}

/// What a proxy is, and the luma samples of its stored video frames.
fn proxy_variant(
    id: &str,
    proxy: &ProxyRequest,
    hardware: &HardwareVideoDecode,
) -> (ProxyVariant, Option<u64>) {
    let found = proxy
        .path
        .is_file()
        .then(|| probe_media(&proxy.path, false, false, hardware))
        .ok_or_else(|| "the proxy file is missing".to_owned())
        .and_then(|result| result);
    let (state, shape, pixels) = match found {
        Ok((fingerprint, pixels)) => (ProxyState::Ready, Some(fingerprint), pixels),
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
    (variant, pixels)
}

/// Evidence of the media at `path`: its length and modification time, what
/// the demuxer finds, and with `hash` a SHA-256 of every byte.
///
/// Sizes are displayed sizes, as a browser demuxer reports them: the coded
/// size in square pixels (stretched by the pixel aspect ratio the stream is
/// shown with), turned by the stream's orientation (a still's included, from
/// its Exif). A `still` image records no rate or duration: those an image
/// demuxer reports are its own defaults.
///
/// # Errors
/// Returns why the file cannot be read or demuxed, or why its video cannot
/// be decoded here: a stream only a hardware session decodes, on a host
/// whose `hardware` does not decode it.
pub fn fingerprint(
    path: &Path,
    hash: bool,
    still: bool,
    hardware: &HardwareVideoDecode,
) -> Result<MediaFingerprint, String> {
    probe_media(path, hash, still, hardware).map(|(fingerprint, _)| fingerprint)
}

/// [`fingerprint`], and the luma samples in the stored frames of the video
/// stream a decoder opens, which a software decode session is sized from.
fn probe_media(
    path: &Path,
    hash: bool,
    still: bool,
    hardware: &HardwareVideoDecode,
) -> Result<(MediaFingerprint, Option<u64>), String> {
    let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
    let probe = alpha2_native::probe(path).map_err(|error| format!("{error}"))?;
    let stored_pixels = probe.selected_video_stream().and_then(|video| {
        let (width, height) = video.dimensions?;
        Some(u64::from(width).saturating_mul(u64::from(height)))
    });
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
        // Where the media ends on its own timeline, as a browser demuxer reports it: a transport
        // stream that starts at 1.4 s and lasts 2 s ends at 3.4 s.
        duration_ticks: probe.duration.filter(|_| !still).map(|duration| {
            probe
                .start
                .map_or(0, alpha2_time::MediaTime::ticks)
                .saturating_add(duration.ticks())
                .max(0)
        }),
        color: video.and_then(|video| video.color),
    };
    Ok((fingerprint, stored_pixels))
}

/// The rate the video stream at `index` presents its first frames at, as
/// every host measures it for a fingerprint ([`FrameRateEvidence`]); `None`
/// when it cannot be measured, as a browser then records none.
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

/// Lowercase hexadecimal digits of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

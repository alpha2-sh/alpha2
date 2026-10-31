//! The host's view of each source's media: its original and the proxies made
//! from it, and which of them a purpose reads.

use alpha2_media::SourceId;
use alpha2_model::{ColorPipeline, HdrSources, MediaFingerprint};
use alpha2_time::FrameRate;
use serde::{Deserialize, Serialize};

use crate::evidence::{compare, duration_tolerance, rates_agree};

/// Every source's media as one host sees it now. It is runtime state, never
/// part of a project: a locator, a path, or a credential stays with the host.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct AssetCatalog {
    /// One entry per source the host knows.
    #[serde(default)]
    pub assets: Vec<AssetEntry>,
}

impl AssetCatalog {
    /// A catalog of `assets`.
    #[must_use]
    pub const fn new(assets: Vec<AssetEntry>) -> Self {
        Self { assets }
    }

    /// The entry for `source`, if the host knows it.
    #[must_use]
    pub fn entry(&self, source: &SourceId) -> Option<&AssetEntry> {
        self.assets.iter().find(|entry| &entry.source_id == source)
    }

    /// Checks that every fingerprint the catalog holds is well formed
    /// ([`MediaFingerprint::malformed_part`]), as a project's are: evidence
    /// that is not, such as a hash in uppercase, would otherwise be read as
    /// other media. A host checks a catalog it did not build itself.
    ///
    /// # Errors
    /// Returns the first malformed fingerprint, with its source and place.
    pub fn validate(&self) -> Result<(), CatalogError> {
        for entry in &self.assets {
            let error = |place: String, part: &'static str| CatalogError {
                source_id: entry.source_id.clone(),
                place,
                part,
            };
            if let OriginalState::Online { fingerprint } = &entry.original
                && let Some(part) = fingerprint.malformed_part()
            {
                return Err(error("the original's fingerprint".into(), part));
            }
            for proxy in &entry.proxies {
                if let Some(part) = proxy.made_from.malformed_part() {
                    return Err(error(format!("proxy {}'s made_from", proxy.id), part));
                }
            }
        }
        Ok(())
    }
}

/// A fingerprint in a catalog that is not well formed.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct CatalogError {
    /// The source whose entry holds it.
    pub source_id: SourceId,
    /// Which fingerprint of the entry, in words.
    pub place: String,
    /// The malformed part, by its field name.
    pub part: &'static str,
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "source {}: {} has a malformed {}",
            self.source_id.as_str(),
            self.place,
            self.part
        )
    }
}

impl std::error::Error for CatalogError {}

/// One source's media.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct AssetEntry {
    /// The project source.
    pub source_id: SourceId,
    /// The host's revision of the source's media; relinking it makes a new
    /// one. What was made from an earlier revision is stale.
    pub revision: u64,
    /// The original media.
    pub original: OriginalState,
    /// Proxies made from the original, in the host's order of preference.
    #[serde(default)]
    pub proxies: Vec<ProxyVariant>,
}

impl AssetEntry {
    /// An entry for `source_id` at `revision`.
    #[must_use]
    pub const fn new(source_id: SourceId, revision: u64, original: OriginalState) -> Self {
        Self {
            source_id,
            revision,
            original,
            proxies: Vec::new(),
        }
    }
}

/// Whether the original media can be read.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum OriginalState {
    /// Readable; `fingerprint` is what the host found.
    Online {
        /// Evidence of the media found.
        fingerprint: MediaFingerprint,
    },
    /// Linked once, but its bytes cannot be reached now.
    Offline {
        /// The path looked up, when the host had one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    /// Readable, but this host cannot decode it.
    Unsupported {
        /// Why, in words.
        reason: String,
    },
}

/// A lighter copy of a source's media for a purpose that does not need the
/// original's quality.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyVariant {
    /// The host's identity for the proxy.
    pub id: String,
    /// The source revision it was made from.
    pub source_revision: u64,
    /// Evidence of the original it was made from.
    pub made_from: MediaFingerprint,
    /// Displayed width in pixels.
    pub width: u32,
    /// Displayed height in pixels.
    pub height: u32,
    /// Its frame rate, which must be the original's for timestamps to map
    /// one to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<FrameRate>,
    /// Its duration in ticks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ticks: Option<i64>,
    /// Its audio sample rate, when it carries audio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_sample_rate: Option<u32>,
    /// The color pipeline baked into it ([`color_pipeline_fingerprint`]),
    /// when it was tone-mapped; absent when it carries the original's own
    /// signal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_pipeline: Option<String>,
    /// Whether it is ready.
    pub state: ProxyState,
}

/// A proxy's generation state.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum ProxyState {
    /// Made and readable.
    Ready,
    /// Being made.
    Generating,
    /// Making it failed.
    Failed {
        /// Why, in words.
        reason: String,
    },
}

/// What media is read for.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MediaPurpose {
    /// Interactive playback: the original, else a proxy.
    Preview,
    /// Interactive playback the caller asked to make lighter: a proxy, else
    /// the original.
    PreviewProxy,
    /// A small still: a proxy, else the original.
    Thumbnail,
    /// Editing away from the originals: a proxy, else the original.
    RemoteEdit,
    /// The delivered render: the original only.
    ExportOriginal,
    /// A render the caller asked to make from proxies: a proxy, else the
    /// original.
    ExportProxy,
}

impl MediaPurpose {
    /// Whether the purpose is a render someone receives.
    #[must_use]
    pub const fn is_export(self) -> bool {
        matches!(self, Self::ExportOriginal | Self::ExportProxy)
    }
}

/// Which media a purpose reads for one source.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "media", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum Selection {
    /// The original.
    Original,
    /// A proxy.
    Proxy {
        /// The proxy's identity.
        id: String,
    },
    /// Nothing usable.
    Unavailable,
}

/// Why a proxy cannot stand in for its source.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProxyProblem {
    /// Made from an earlier revision.
    StaleRevision,
    /// A different color pipeline is baked in.
    StaleColorPipeline,
    /// Not made yet.
    Pending,
    /// Making it failed.
    Failed {
        /// Why, in words.
        reason: String,
    },
    /// It does not stand in for the original.
    Invalid {
        /// What differs.
        reasons: Vec<ProxyMismatch>,
    },
}

/// A way a proxy differs from its original.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProxyMismatch {
    /// It was made from other media.
    MadeFrom,
    /// Its frame rate differs, so timestamps do not map one to one.
    FrameRate,
    /// Its duration differs by more than [`duration_tolerance`] allows: about
    /// one frame.
    Duration,
    /// Its picture's shape differs, as a wrong rotation would make it.
    Aspect,
    /// It has no audio where the original has.
    AudioMissing,
    /// Its audio sample rate differs.
    AudioSampleRate,
}

/// A color pipeline's identity, as a proxy that bakes it in records it.
#[must_use]
pub fn color_pipeline_fingerprint(pipeline: &ColorPipeline) -> String {
    let hdr = match pipeline.hdr {
        HdrSources::Reject => "reject",
        HdrSources::ToneMap => "tone_map",
    };
    format!("hdr={hdr};peak={}", pipeline.hdr_peak_nits)
}

/// Why `proxy` cannot stand in for the source of `entry`, or `None` when it
/// can. `reference` is the best evidence of the media the project recorded:
/// what the host found, unless that differs from the recording, else what
/// the project recorded.
#[must_use]
pub fn proxy_problem(
    entry: &AssetEntry,
    proxy: &ProxyVariant,
    reference: Option<&MediaFingerprint>,
    pipeline: &ColorPipeline,
) -> Option<ProxyProblem> {
    match &proxy.state {
        ProxyState::Generating => return Some(ProxyProblem::Pending),
        ProxyState::Failed { reason } => {
            return Some(ProxyProblem::Failed {
                reason: reason.clone(),
            });
        }
        ProxyState::Ready => {}
    }
    if proxy.source_revision != entry.revision {
        return Some(ProxyProblem::StaleRevision);
    }
    if proxy
        .color_pipeline
        .as_ref()
        .is_some_and(|baked| *baked != color_pipeline_fingerprint(pipeline))
    {
        return Some(ProxyProblem::StaleColorPipeline);
    }
    let reasons = reference
        .map(|reference| mismatches(proxy, reference))
        .unwrap_or_default();
    (!reasons.is_empty()).then_some(ProxyProblem::Invalid { reasons })
}

fn mismatches(proxy: &ProxyVariant, reference: &MediaFingerprint) -> Vec<ProxyMismatch> {
    let mut reasons = Vec::new();
    if !compare(reference, &proxy.made_from).is_acceptable() {
        reasons.push(ProxyMismatch::MadeFrom);
    }
    let streams = reference.streams.as_ref();
    let video = streams.and_then(|streams| streams.video.as_ref());
    if let (Some(original), Some(proxied)) =
        (video.and_then(|video| video.frame_rate), proxy.frame_rate)
        && !rates_agree(original, proxied)
    {
        reasons.push(ProxyMismatch::FrameRate);
    }
    // The proxy's own rates count where the reference records none: its
    // frame is the original's when its rate is.
    let tolerance = duration_tolerance(
        &[video.and_then(|video| video.frame_rate), proxy.frame_rate],
        &[
            streams.and_then(|streams| Some(streams.audio.as_ref()?.sample_rate)),
            proxy.audio_sample_rate,
        ],
    );
    if let (Some(original), Some(duration)) = (reference.duration_ticks, proxy.duration_ticks)
        && original.abs_diff(duration) > tolerance
    {
        reasons.push(ProxyMismatch::Duration);
    }
    if let Some(video) = video
        && !aspects_agree((video.width, video.height), (proxy.width, proxy.height))
    {
        reasons.push(ProxyMismatch::Aspect);
    }
    if let Some(audio) = streams.and_then(|streams| streams.audio.as_ref()) {
        match proxy.audio_sample_rate {
            None => reasons.push(ProxyMismatch::AudioMissing),
            Some(rate) if rate != audio.sample_rate => {
                reasons.push(ProxyMismatch::AudioSampleRate);
            }
            Some(_) => {}
        }
    }
    reasons
}

/// Whether two pictures have one shape, allowing for a scaled size rounded
/// to even dimensions: within 2 %.
fn aspects_agree(original: (u32, u32), proxy: (u32, u32)) -> bool {
    let aspect = |(width, height): (u32, u32)| f64::from(width) / f64::from(height.max(1));
    let (a, b) = (aspect(original), aspect(proxy));
    (a - b).abs() <= a.max(b) * 0.02
}

/// The media `purpose` reads for `entry`, with `reference` the best evidence
/// of the original. The original is preferred for preview and required for
/// an original export; a proxy is preferred for proxy preview, thumbnails,
/// remote editing, and a proxy export. Nothing falls back to a stale or
/// invalid proxy.
#[must_use]
pub fn select(
    entry: &AssetEntry,
    purpose: MediaPurpose,
    reference: Option<&MediaFingerprint>,
    pipeline: &ColorPipeline,
) -> Selection {
    let original = matches!(entry.original, OriginalState::Online { .. });
    let proxy = || {
        entry
            .proxies
            .iter()
            .find(|proxy| proxy_problem(entry, proxy, reference, pipeline).is_none())
            .map(|proxy| Selection::Proxy {
                id: proxy.id.clone(),
            })
    };
    let chosen = match purpose {
        MediaPurpose::Preview => original.then_some(Selection::Original).or_else(proxy),
        MediaPurpose::PreviewProxy
        | MediaPurpose::Thumbnail
        | MediaPurpose::RemoteEdit
        | MediaPurpose::ExportProxy => proxy().or_else(|| original.then_some(Selection::Original)),
        MediaPurpose::ExportOriginal => original.then_some(Selection::Original),
    };
    chosen.unwrap_or(Selection::Unavailable)
}

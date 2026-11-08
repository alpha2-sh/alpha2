//! Pre-render check that each source a project reads has usable media, reported as a list of
//! actionable issues instead of a mid-render failure.

use std::collections::HashMap;

use alpha2_media::{ColorMetadata, SourceId};
use alpha2_model::{ClipId, HdrSources, MediaFingerprint, MediaKind, MediaSource, Project};
use serde::Serialize;

use crate::catalog::{
    AssetCatalog, AssetEntry, MediaPurpose, OriginalState, ProxyProblem, Selection, proxy_problem,
    select,
};
use crate::evidence::{FingerprintField, FingerprintMatch, compare};

/// Whether a project's sources can be read for a purpose.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ConformReport {
    /// What the media is read for.
    pub purpose: MediaPurpose,
    /// Whether no issue blocks the purpose.
    pub ready: bool,
    /// Each host-supplied source a render reads, in project order.
    pub sources: Vec<SourceConform>,
}

impl ConformReport {
    /// Every blocking issue, with its source.
    pub fn blocking(&self) -> impl Iterator<Item = (&SourceId, &ConformIssue)> {
        self.sources.iter().flat_map(|source| {
            source
                .issues
                .iter()
                .filter(|entry| entry.blocking)
                .map(move |entry| (&source.source_id, &entry.issue))
        })
    }
}

/// One source's media for the purpose.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[non_exhaustive]
pub struct SourceConform {
    /// The source.
    pub source_id: SourceId,
    /// The media read.
    pub selection: Selection,
    /// A proxy that could stand in for the source, whether or not the purpose reads it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usable_proxy: Option<String>,
    /// What is wrong or worth knowing, in a fixed order.
    pub issues: Vec<ConformEntry>,
}

/// An issue and whether it blocks the purpose.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ConformEntry {
    /// The issue.
    #[serde(flatten)]
    pub issue: ConformIssue,
    /// Whether the purpose cannot go ahead with it.
    pub blocking: bool,
}

/// Something about a source's media.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ConformIssue {
    /// The host has no media for the source.
    Missing,
    /// The original was linked, but cannot be reached now.
    Offline {
        /// The path looked up, when the host had one.
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    /// The host cannot decode the original.
    Unsupported {
        /// Why, in words.
        reason: String,
    },
    /// The original the host found differs from what the project recorded.
    FingerprintMismatch {
        /// What differs.
        fields: Vec<FingerprintField>,
    },
    /// The recorded fingerprint shares no comparable field with the found media.
    FingerprintUnverified,
    /// A proxy cannot stand in for the source.
    Proxy {
        /// The proxy.
        proxy: String,
        /// Why.
        problem: ProxyProblem,
    },
    /// The purpose reads a proxy, not the original.
    UsingProxy {
        /// The proxy.
        proxy: String,
    },
    /// The source is HDR and the project's color pipeline refuses HDR.
    HdrRejected,
    /// Neither the project nor the video declares its color, so it is read as
    /// [`ColorMetadata::UNDECLARED`] (BT.709 limited-range SDR).
    UndeclaredColor,
    /// The original carries alpha the host does not decode, so its pictures are drawn opaque.
    AlphaDropped {
        /// Why, in words.
        reason: String,
    },
    /// A clip plays an audio stream or channel the original does not have
    /// (`audio.stream`, `audio.channels`).
    AudioSelection {
        /// The clip.
        clip: ClipId,
        /// What is missing, in words.
        reason: String,
    },
}

/// Checks every host-supplied source a render of `project` reads against `catalog`.
///
/// A source without media blocks every purpose. An original export also blocks on an offline,
/// undecodable or mismatched original, even when a proxy exists; a preview only warns about a
/// mismatch. An HDR source in a project that rejects HDR always blocks. Undeclared color and
/// dropped alpha are reported without blocking.
#[must_use]
pub fn conform(project: &Project, catalog: &AssetCatalog, purpose: MediaPurpose) -> ConformReport {
    conform_sources(project, project.used_sources(), catalog, purpose)
}

/// [`conform`] over `sources` instead of the ones a render reads, such as the one source an
/// extraction reads.
#[must_use]
pub fn conform_sources(
    project: &Project,
    sources: Vec<&MediaSource>,
    catalog: &AssetCatalog,
    purpose: MediaPurpose,
) -> ConformReport {
    // Index once to avoid quadratic lookups; the first entry per ID wins, as in
    // `AssetCatalog::entry`.
    let mut entries = HashMap::with_capacity(catalog.assets.len());
    for entry in &catalog.assets {
        entries.entry(&entry.source_id).or_insert(entry);
    }
    let sources = sources
        .into_iter()
        .filter(|source| source.is_host_supplied())
        .map(|source| {
            let entry = entries.get(&source.id).copied();
            conform_source(project, source, entry, purpose)
        })
        .collect::<Vec<_>>();
    ConformReport {
        purpose,
        ready: sources
            .iter()
            .all(|source| source.issues.iter().all(|entry| !entry.blocking)),
        sources,
    }
}

fn conform_source(
    project: &Project,
    source: &MediaSource,
    entry: Option<&AssetEntry>,
    purpose: MediaPurpose,
) -> SourceConform {
    let pipeline = &project.settings.color;
    let mut issues = Vec::new();
    let Some(entry) = entry else {
        issues.push(ConformEntry {
            issue: ConformIssue::Missing,
            blocking: true,
        });
        return SourceConform {
            source_id: source.id.clone(),
            selection: Selection::Unavailable,
            usable_proxy: None,
            issues,
        };
    };
    let found = match &entry.original {
        OriginalState::Online { fingerprint } => Some(fingerprint),
        OriginalState::Offline { .. } | OriginalState::Unsupported { .. } => None,
    };
    let matched = source
        .fingerprint
        .as_ref()
        .zip(found)
        .map(|(recorded, found)| compare(recorded, found));
    // Proxies are checked against the found media unless it mismatches the recording.
    let reference = match matched {
        Some(FingerprintMatch::Mismatch { .. }) => source.fingerprint.as_ref(),
        _ => found.or(source.fingerprint.as_ref()),
    };
    let selection = select(entry, purpose, reference, pipeline);
    let unavailable = selection == Selection::Unavailable;
    match &entry.original {
        OriginalState::Online { .. } => {}
        OriginalState::Offline { path } => issues.push(ConformEntry {
            issue: ConformIssue::Offline { path: path.clone() },
            blocking: unavailable,
        }),
        OriginalState::Unsupported { reason } => issues.push(ConformEntry {
            issue: ConformIssue::Unsupported {
                reason: reason.clone(),
            },
            blocking: unavailable,
        }),
    }
    if let Some(matched) = matched {
        match matched {
            FingerprintMatch::Mismatch { fields } => issues.push(ConformEntry {
                issue: ConformIssue::FingerprintMismatch { fields },
                blocking: purpose.is_export() && selection == Selection::Original,
            }),
            FingerprintMatch::Unverified => issues.push(ConformEntry {
                issue: ConformIssue::FingerprintUnverified,
                blocking: false,
            }),
            _ => {}
        }
    }
    let mut usable_proxy = None;
    for proxy in &entry.proxies {
        match proxy_problem(entry, proxy, reference, pipeline) {
            Some(problem) => issues.push(ConformEntry {
                issue: ConformIssue::Proxy {
                    proxy: proxy.id.clone(),
                    problem,
                },
                blocking: false,
            }),
            None => {
                usable_proxy.get_or_insert_with(|| proxy.id.clone());
            }
        }
    }
    if let Selection::Proxy { id } = &selection {
        issues.push(ConformEntry {
            issue: ConformIssue::UsingProxy { proxy: id.clone() },
            blocking: false,
        });
    }
    if pipeline.hdr == HdrSources::Reject && is_hdr(source, [found, reference]) {
        issues.push(ConformEntry {
            issue: ConformIssue::HdrRejected,
            blocking: true,
        });
    }
    if undeclared_color(source, found) {
        issues.push(ConformEntry {
            issue: ConformIssue::UndeclaredColor,
            blocking: false,
        });
    }
    issues.extend(alpha_dropped(entry, &selection));
    issues.extend(audio_selection(project, source, entry));
    SourceConform {
        source_id: source.id.clone(),
        selection,
        usable_proxy,
        issues,
    }
}

/// Each clip of `source` whose `audio.stream` or `audio.channels` the listed streams lack.
fn audio_selection(
    project: &Project,
    source: &MediaSource,
    entry: &AssetEntry,
) -> Vec<ConformEntry> {
    let Some(streams) = entry.audio_streams.as_deref() else {
        return Vec::new();
    };
    let clips = project
        .timeline
        .tracks
        .iter()
        .flat_map(|track| &track.clips)
        .filter(|clip| clip.source_id == source.id)
        .filter(|clip| clip.audio.stream.is_some() || clip.audio.channels.is_some());
    let mut issues = Vec::new();
    for clip in clips {
        let stream = match clip.audio.stream {
            Some(index) => streams
                .get(usize::try_from(index).unwrap_or(usize::MAX))
                .ok_or_else(|| {
                    format!(
                        "audio stream {index} does not exist; the media has {} audio streams",
                        streams.len()
                    )
                }),
            None => streams
                .iter()
                .find(|stream| stream.default)
                .or_else(|| streams.first())
                .ok_or_else(|| "the media has no audio stream".to_owned()),
        };
        let missing = stream.and_then(|stream| {
            let channels = clip.audio.channels.as_deref().unwrap_or_default();
            match channels.iter().find(|&&channel| channel >= stream.channels) {
                Some(channel) => Err(format!(
                    "channel {channel} does not exist; the audio stream has {} channels",
                    stream.channels
                )),
                None => Ok(()),
            }
        });
        if let Err(reason) = missing {
            issues.push(ConformEntry {
                issue: ConformIssue::AudioSelection {
                    clip: clip.id.clone(),
                    reason,
                },
                blocking: true,
            });
        }
    }
    issues
}

fn alpha_dropped(entry: &AssetEntry, selection: &Selection) -> Option<ConformEntry> {
    let reason = entry.alpha_dropped.as_ref()?;
    (*selection == Selection::Original).then(|| ConformEntry {
        issue: ConformIssue::AlphaDropped {
            reason: reason.clone(),
        },
        blocking: false,
    })
}

/// Whether the source or any evidence of the media a render reads is HDR, as
/// [`ColorMetadata::is_hdr`] decides.
fn is_hdr(source: &MediaSource, evidence: [Option<&MediaFingerprint>; 2]) -> bool {
    source.effective_color_metadata().is_hdr()
        || evidence
            .into_iter()
            .flatten()
            .filter_map(|fingerprint| fingerprint.color)
            .any(ColorMetadata::is_hdr)
}

/// Whether a video falls back to undeclared color. Stills and provider-drawn media are RGB, which
/// the fallback matrix and range do not affect, so they are never reported.
fn undeclared_color(source: &MediaSource, found: Option<&MediaFingerprint>) -> bool {
    source.kind == MediaKind::Video
        && source.provider.is_none()
        && source.color_metadata.is_none()
        && found.is_some_and(|fingerprint| fingerprint.color.is_none())
}

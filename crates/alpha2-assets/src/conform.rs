//! Before a render: whether each source a project reads has usable media,
//! as a list a person can act on rather than a failure part way through.

use std::collections::HashMap;

use alpha2_media::{ColorMetadata, SourceId};
use alpha2_model::{HdrSources, MediaFingerprint, MediaKind, MediaSource, Project};
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
    /// A proxy that could stand in for the source, whether or not the
    /// purpose would read it: what an original export could switch to.
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
    Offline,
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
    /// The project recorded a fingerprint, but nothing in it could be
    /// compared with what the host found.
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
    /// Neither the project nor the video says what its color is, so it is
    /// read as [`ColorMetadata::UNDECLARED`] (BT.709 limited-range SDR):
    /// declare the source's `color_metadata` if that is wrong.
    UndeclaredColor,
}

/// Checks every host-supplied source a render of `project` reads against
/// `catalog`, for `purpose`.
///
/// A source without media blocks every purpose. An original export blocks on
/// an offline or undecodable original even when a proxy exists, and on an
/// original that differs from the project's fingerprint; a preview only
/// warns about the latter. An HDR source in a project that refuses HDR
/// blocks everything, as its render would fail. A video whose color neither
/// the project nor its media declares is reported, blocking nothing.
#[must_use]
pub fn conform(project: &Project, catalog: &AssetCatalog, purpose: MediaPurpose) -> ConformReport {
    // One lookup per source, so a catalog listing every source does not make this quadratic. The
    // first entry for an ID is the one read, as `AssetCatalog::entry` reads it.
    let mut entries = HashMap::with_capacity(catalog.assets.len());
    for entry in &catalog.assets {
        entries.entry(&entry.source_id).or_insert(entry);
    }
    let sources = project
        .used_sources()
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
        OriginalState::Offline | OriginalState::Unsupported { .. } => None,
    };
    let matched = source
        .fingerprint
        .as_ref()
        .zip(found)
        .map(|(recorded, found)| compare(recorded, found));
    // A proxy stands in for the media the project recorded. What the host
    // found is better evidence of that media only when it may be it.
    let reference = match matched {
        Some(FingerprintMatch::Mismatch { .. }) => source.fingerprint.as_ref(),
        _ => found.or(source.fingerprint.as_ref()),
    };
    let selection = select(entry, purpose, reference, pipeline);
    let unavailable = selection == Selection::Unavailable;
    match &entry.original {
        OriginalState::Online { .. } => {}
        OriginalState::Offline => issues.push(ConformEntry {
            issue: ConformIssue::Offline,
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
    SourceConform {
        source_id: source.id.clone(),
        selection,
        usable_proxy,
        issues,
    }
}

/// Whether the source declares HDR, or the evidence of what a render reads
/// says HDR (the media found, and what stands in for the original when a
/// proxy is read: the recording, if the original cannot be reached): PQ or
/// HLG, or declared HDR, as the renderer decides ([`ColorMetadata::is_hdr`]).
fn is_hdr(source: &MediaSource, evidence: [Option<&MediaFingerprint>; 2]) -> bool {
    source.effective_color_metadata().is_hdr()
        || evidence
            .into_iter()
            .flatten()
            .filter_map(|fingerprint| fingerprint.color)
            .any(ColorMetadata::is_hdr)
}

/// Whether a video is read by the color fallback: the project declares no
/// color for it, and the media found carries none. Media that cannot be
/// reached says nothing either way. Neither a still image nor what an
/// extension provider draws (an animated image, a Lottie, an `OpenEXR`
/// sequence) is reported: those are RGB pictures, which the fallback's
/// matrix and range do not touch.
fn undeclared_color(source: &MediaSource, found: Option<&MediaFingerprint>) -> bool {
    source.kind == MediaKind::Video
        && source.provider.is_none()
        && source.color_metadata.is_none()
        && found.is_some_and(|fingerprint| fingerprint.color.is_none())
}

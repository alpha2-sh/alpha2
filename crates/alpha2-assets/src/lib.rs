//! Asset identity, proxies, and conform reports.
//!
//! A project records a [`MediaFingerprint`] per source, and a host keeps an [`AssetCatalog`] of
//! the originals and proxies it can read. This crate decides, identically on every host, whether
//! found media matches the record ([`compare`]), which media a purpose reads ([`select`]), and
//! what blocks a render before it starts ([`conform`]). It does no I/O; hashing, probing and
//! proxy creation belong to the host.
//!
//! [`MediaFingerprint`]: alpha2_model::MediaFingerprint

mod catalog;
mod conform;
mod evidence;
mod rate;
#[cfg(test)]
mod tests;
// The shared benchmark fixture refers to the crate by name.
#[cfg(test)]
extern crate self as alpha2_assets;

pub use catalog::{
    AssetCatalog, AssetEntry, AudioStreamFacts, CatalogError, MediaPurpose, OriginalState,
    ProxyMismatch, ProxyProblem, ProxyState, ProxyVariant, Selection, color_pipeline_fingerprint,
    proxy_problem, select,
};
pub use conform::{
    ConformEntry, ConformIssue, ConformReport, SourceConform, StaleData, conform, conform_sources,
};
pub use evidence::{FingerprintField, FingerprintMatch, compare, duration_tolerance, rates_agree};
pub use rate::{FrameRateEvidence, rational_rate};

//! Asset identity, proxies, and conform reports.
//!
//! A project records evidence of each source's media ([`MediaFingerprint`]);
//! a host keeps its own [`AssetCatalog`] of what it can read now: the
//! original, or proxies made from it. This crate decides, the same way on
//! every host, whether found media is the recorded media ([`compare`]),
//! which media a purpose reads ([`select`]), and what stands in the way of a
//! render before it starts ([`conform`]). It does no I/O: hashing, probing,
//! and making proxies are the host's.
//!
//! [`MediaFingerprint`]: alpha2_model::MediaFingerprint

mod catalog;
mod conform;
mod evidence;
mod rate;
#[cfg(test)]
mod tests;
// The benchmark's fixture, which the tests share, names the crate.
#[cfg(test)]
extern crate self as alpha2_assets;

pub use catalog::{
    AssetCatalog, AssetEntry, CatalogError, MediaPurpose, OriginalState, ProxyMismatch,
    ProxyProblem, ProxyState, ProxyVariant, Selection, color_pipeline_fingerprint, proxy_problem,
    select,
};
pub use conform::{ConformEntry, ConformIssue, ConformReport, SourceConform, conform};
pub use evidence::{FingerprintField, FingerprintMatch, compare, duration_tolerance, rates_agree};
pub use rate::{FrameRateEvidence, rational_rate};

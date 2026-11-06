//! What this process learned from encoder probe opens, so failures that cannot change are asked
//! once.

use std::collections::HashMap;
use std::sync::Mutex;

use alpha2_media::{EncoderRefusal, OpenFailureKind};

/// A probe answer that cannot change while the process runs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProbeVerdict {
    /// The encoder encoded a frame.
    Runs,
    /// The encoder failed in a way asking again would repeat, with the failure kind and reason.
    Refused(OpenFailureKind, String),
}

/// Unclassified probe failures in a row after which the last one is kept as the verdict.
///
/// Some transforms leak resources on every failed open, so a long-lived host must stop asking.
pub const UNCLASSIFIED_FAILURES_KEPT: u8 = 3;

/// What this process knows about one encoder and way it runs.
#[derive(Debug, Default)]
pub struct ProbeEntry {
    /// The kept verdict, if any.
    verdict: Option<ProbeVerdict>,
    unclassified_failures: u8,
}

/// The probe outcomes kept for this process, by encoder name and way it runs.
pub type ProbeCache = Mutex<HashMap<(String, bool), ProbeEntry>>;

/// The cached verdict for `name` and mode, else `probe`'s answer (cached when it cannot change, or
/// after [`UNCLASSIFIED_FAILURES_KEPT`] unclassified failures in a row); `None` when nothing is
/// cached and there is no `probe`.
///
/// The cache lock is held across the probe, so two exports asking at once probe once.
pub fn cached_probe(
    cache: &ProbeCache,
    name: &str,
    on_hardware: bool,
    probe: Option<impl FnOnce() -> Result<(), (OpenFailureKind, String)>>,
) -> Option<Result<(), EncoderRefusal>> {
    let refusal = |kind, reason| EncoderRefusal {
        encoder: name.to_owned(),
        hardware_accelerated: on_hardware,
        kind,
        reason,
    };
    let mut entries = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = entries.entry((name.to_owned(), on_hardware)).or_default();
    match &entry.verdict {
        Some(ProbeVerdict::Runs) => return Some(Ok(())),
        Some(ProbeVerdict::Refused(kind, reason)) => {
            return Some(Err(refusal(*kind, reason.clone())));
        }
        None => {}
    }
    let outcome = probe?();
    entry.unclassified_failures = match &outcome {
        Err((OpenFailureKind::Unknown, _)) => entry.unclassified_failures.saturating_add(1),
        _ => 0,
    };
    entry.verdict = match &outcome {
        Ok(()) => Some(ProbeVerdict::Runs),
        Err((kind @ (OpenFailureKind::Absent | OpenFailureKind::Unsuitable), reason)) => {
            Some(ProbeVerdict::Refused(*kind, reason.clone()))
        }
        Err((OpenFailureKind::Unknown, reason))
            if entry.unclassified_failures >= UNCLASSIFIED_FAILURES_KEPT =>
        {
            Some(ProbeVerdict::Refused(
                OpenFailureKind::Unknown,
                reason.clone(),
            ))
        }
        Err(_) => None,
    };
    if let Err((kind, error)) = &outcome {
        alpha2_os::diagnostic!(
            "alpha2 native: video encoder {name} {} is registered but did not encode a frame on \
             this machine, so it is not offered that way ({}{}): {error}",
            mode_label(on_hardware),
            kind.as_str(),
            if entry.verdict.is_some() {
                ""
            } else {
                "; asked again at the next export"
            }
        );
    }
    Some(outcome.map_err(|(kind, reason)| refusal(kind, reason)))
}

/// How an encoder runs, for diagnostics.
#[must_use]
pub const fn mode_label(on_hardware: bool) -> &'static str {
    if on_hardware { "on GPU" } else { "on CPU" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unclassified_probe_failures_are_kept_after_a_run_of_them() {
        let cache = ProbeCache::default();
        let probes = std::cell::Cell::new(0);
        let ask = |kind| {
            cached_probe(
                &cache,
                "hevc_mf",
                false,
                Some(|| {
                    probes.set(probes.get() + 1);
                    Err((kind, format!("failure {}", probes.get())))
                }),
            )
        };
        // A busy answer between unclassified ones restarts the run.
        ask(OpenFailureKind::Unknown);
        ask(OpenFailureKind::Busy);
        for _ in 0..UNCLASSIFIED_FAILURES_KEPT {
            ask(OpenFailureKind::Unknown);
        }
        let asked = probes.get();
        assert_eq!(asked, usize::from(UNCLASSIFIED_FAILURES_KEPT) + 2);
        let kept = ask(OpenFailureKind::Unknown);
        assert_eq!(probes.get(), asked, "probed after the run was kept");
        let Some(Err(refusal)) = kept else {
            panic!("{kept:?}");
        };
        assert_eq!(refusal.kind, OpenFailureKind::Unknown);
        assert_eq!(refusal.reason, format!("failure {asked}"));
    }
}

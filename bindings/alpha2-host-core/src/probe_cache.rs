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

/// How long a hardware encoder probe answer is kept for this process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Keep {
    /// Until the process exits. For a success, a refusal by the device, or a timed-out probe, whose
    /// thread or child may never return.
    Process,
    /// Not kept, so a busy, unstarted or lost probe is asked again next time.
    Never,
    /// Kept after [`UNCLASSIFIED_FAILURES_KEPT`] unclassified failures in a row.
    AfterRepeats,
}

impl Keep {
    /// The rule for a classified open failure, or for success when `kind` is `None`.
    #[must_use]
    pub const fn of_failure(kind: Option<OpenFailureKind>) -> Self {
        match kind {
            None | Some(OpenFailureKind::Absent | OpenFailureKind::Unsuitable) => Self::Process,
            Some(OpenFailureKind::Busy) => Self::Never,
            Some(_) => Self::AfterRepeats,
        }
    }
}

/// How a probe that runs in a child process ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildProbeEnd {
    /// The child answered; `busy` when the answer says the device was busy.
    Answered {
        /// The device was busy.
        busy: bool,
    },
    /// The child did not start.
    NotStarted,
    /// Waiting for the child failed.
    Lost,
    /// The child did not finish in time and was killed.
    TimedOut,
    /// The child crashed or exited without an answer.
    NoAnswer,
}

impl ChildProbeEnd {
    /// How long the answer of a probe that ended this way is kept.
    #[must_use]
    pub const fn keep(self) -> Keep {
        match self {
            Self::Answered { busy: false } | Self::TimedOut => Keep::Process,
            Self::Answered { busy: true } | Self::NotStarted | Self::Lost => Keep::Never,
            Self::NoAnswer => Keep::AfterRepeats,
        }
    }
}

/// Probe answers by key, kept under [`Keep`]; the first kept answer for a key stays.
#[derive(Debug)]
pub struct KeptAnswers<K, V> {
    entries: Vec<(K, Option<V>, u8)>,
}

impl<K, V> Default for KeptAnswers<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> KeptAnswers<K, V> {
    /// No answers.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<K: PartialEq, V: Clone> KeptAnswers<K, V> {
    /// The kept answer for `key`.
    #[must_use]
    pub fn get(&self, key: &K) -> Option<V> {
        self.entries
            .iter()
            .find(|(known, _, _)| known == key)
            .and_then(|(_, answer, _)| answer.clone())
    }

    /// Records `answer` for `key` as `keep` says, unless an answer is already kept.
    pub fn record(&mut self, key: K, answer: &V, keep: Keep) {
        let index = if let Some(index) = self.entries.iter().position(|(known, _, _)| *known == key)
        {
            index
        } else {
            self.entries.push((key, None, 0));
            self.entries.len() - 1
        };
        let (_, kept, repeats) = &mut self.entries[index];
        if kept.is_some() {
            return;
        }
        match keep {
            Keep::Process => *kept = Some(answer.clone()),
            Keep::Never => *repeats = 0,
            Keep::AfterRepeats => {
                *repeats = repeats.saturating_add(1);
                if *repeats >= UNCLASSIFIED_FAILURES_KEPT {
                    *kept = Some(answer.clone());
                }
            }
        }
    }
}

/// The kept answer of `answers`, else `probe` on its own thread, bounded by `timeout` and kept as
/// the [`Keep`] it returns says.
///
/// A probe still running at `timeout` is kept as a refusal for the process, so at most one probe
/// thread is ever stuck. The lock is held across the wait, so concurrent callers probe once.
///
/// # Errors
/// The probe's refusal, or why it did not answer.
pub fn bounded_kept_probe<V: Clone + Send + 'static>(
    answers: &Mutex<KeptAnswers<(), Result<V, String>>>,
    name: &str,
    timeout: std::time::Duration,
    probe: impl FnOnce() -> (Result<V, String>, Keep) + Send + 'static,
) -> Result<V, String> {
    let mut answers = answers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(answer) = answers.get(&()) {
        return answer;
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name(format!("alpha2 {name} probe"))
        .spawn(move || {
            let _ = sender.send(probe());
        });
    let (answer, keep) = match spawned {
        Err(error) => (
            Err(format!("{name}: the probe did not start: {error}")),
            Keep::Never,
        ),
        Ok(_) => match receiver.recv_timeout(timeout) {
            Ok(answered) => answered,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => (
                Err(format!(
                    "{name}'s probe session did not finish within {timeout:?}, so the path is \
                     off for this process"
                )),
                Keep::Process,
            ),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => (
                Err(format!("{name}'s probe ended without an answer")),
                Keep::AfterRepeats,
            ),
        },
    };
    answers.record((), &answer, keep);
    answer
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
    fn a_busy_or_lost_probe_is_asked_again_and_a_timeout_or_refusal_is_kept() {
        let mut answers = KeptAnswers::new();
        answers.record(
            "h264",
            &"busy",
            ChildProbeEnd::Answered { busy: true }.keep(),
        );
        assert_eq!(answers.get(&"h264"), None);
        answers.record("h264", &"not started", ChildProbeEnd::NotStarted.keep());
        answers.record("h264", &"lost", ChildProbeEnd::Lost.keep());
        assert_eq!(answers.get(&"h264"), None);
        answers.record("h264", &"timed out", ChildProbeEnd::TimedOut.keep());
        assert_eq!(answers.get(&"h264"), Some("timed out"));
        answers.record(
            "h264",
            &"later",
            ChildProbeEnd::Answered { busy: false }.keep(),
        );
        assert_eq!(
            answers.get(&"h264"),
            Some("timed out"),
            "the first kept answer stays"
        );

        answers.record(
            "hevc",
            &"refused",
            ChildProbeEnd::Answered { busy: false }.keep(),
        );
        assert_eq!(answers.get(&"hevc"), Some("refused"));

        for crash in 1..=UNCLASSIFIED_FAILURES_KEPT {
            assert_eq!(answers.get(&"av1"), None, "after {} crashes", crash - 1);
            answers.record("av1", &"crashed", ChildProbeEnd::NoAnswer.keep());
        }
        assert_eq!(answers.get(&"av1"), Some("crashed"));
        assert_eq!(Keep::of_failure(None), Keep::Process);
        assert_eq!(
            Keep::of_failure(Some(OpenFailureKind::Unsuitable)),
            Keep::Process
        );
        assert_eq!(Keep::of_failure(Some(OpenFailureKind::Busy)), Keep::Never);
        assert_eq!(
            Keep::of_failure(Some(OpenFailureKind::Unknown)),
            Keep::AfterRepeats
        );
    }

    #[test]
    fn a_probe_that_outlives_its_timeout_runs_once_for_every_later_question() {
        let answers = Mutex::new(KeptAnswers::new());
        let runs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for _ in 0..3 {
            let counted = std::sync::Arc::clone(&runs);
            let answer = bounded_kept_probe(
                &answers,
                "h264_videotoolbox",
                std::time::Duration::from_millis(20),
                move || {
                    counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    (Ok(()), Keep::Process)
                },
            );
            assert!(answer.expect_err("timed out").contains("did not finish"));
        }
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 1);

        let busy = Mutex::new(KeptAnswers::new());
        let runs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for _ in 0..2 {
            let counted = std::sync::Arc::clone(&runs);
            let _ = bounded_kept_probe(
                &busy,
                "h264_videotoolbox",
                std::time::Duration::from_secs(5),
                move || {
                    counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    (Err::<(), _>("busy".to_owned()), Keep::Never)
                },
            );
        }
        assert_eq!(
            runs.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "busy is asked again"
        );
    }

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

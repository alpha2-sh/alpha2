//! Export policy every native host applies: the H.264 profile ladder platform encoders are tried
//! with, and whether and how a failed export is retried.

/// An H.264 profile a platform encoder is asked for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AvcProfile {
    /// High (`profile_idc` 100).
    High,
    /// Main (`profile_idc` 77).
    Main,
    /// Constrained Baseline: Baseline with `constraint_set1`, decodable by every H.264 decoder.
    ConstrainedBaseline,
}

impl AvcProfile {
    /// The ladder, tried in this order.
    pub const LADDER: [Self; 3] = [Self::High, Self::Main, Self::ConstrainedBaseline];

    /// The profile's name, as reports spell it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::High => "High",
            Self::Main => "Main",
            Self::ConstrainedBaseline => "Constrained Baseline",
        }
    }

    /// `profile_idc` and the constraint flags byte, as an `avc1.PPCCLL` codec string carries them.
    #[must_use]
    pub const fn idc_and_constraints(self) -> (u8, u8) {
        match self {
            Self::High => (0x64, 0x00),
            Self::Main => (0x4d, 0x00),
            // constraint_set0, 1 and 2: decodable by Baseline, Main and Extended decoders.
            Self::ConstrainedBaseline => (0x42, 0xe0),
        }
    }
}

/// H.264 levels by frame size in macroblocks and bitrate (ITU-T H.264 Table A-1, `level_idc`).
const AVC_LEVELS: [(u32, u64, u8); 19] = [
    (99, 64_000, 10),
    (396, 192_000, 11),
    (396, 384_000, 12),
    (396, 768_000, 13),
    (396, 2_000_000, 20),
    (792, 4_000_000, 21),
    (1_620, 4_000_000, 22),
    (1_620, 10_000_000, 30),
    (3_600, 14_000_000, 31),
    (5_120, 20_000_000, 32),
    (8_192, 20_000_000, 40),
    (8_192, 50_000_000, 41),
    (8_704, 50_000_000, 42),
    (22_080, 135_000_000, 50),
    (36_864, 240_000_000, 51),
    (36_864, 240_000_000, 52),
    (139_264, 240_000_000, 60),
    (139_264, 480_000_000, 61),
    (139_264, 800_000_000, 62),
];

/// The lowest `level_idc` that holds `width` x `height` at `bitrate`, else the highest level.
#[must_use]
pub fn avc_level(width: u32, height: u32, bitrate: u64) -> u8 {
    let macroblocks = width.div_ceil(16).saturating_mul(height.div_ceil(16));
    AVC_LEVELS
        .iter()
        .find(|(most, rate, _)| macroblocks <= *most && bitrate <= *rate)
        .map_or(62, |(_, _, level)| *level)
}

/// The `avc1.PPCCLL` codec string of `profile` at `level`.
#[must_use]
pub fn avc_codec_string(profile: AvcProfile, level: u8) -> String {
    let (idc, constraints) = profile.idc_and_constraints();
    format!("avc1.{idc:02x}{constraints:02x}{level:02x}")
}

/// An in-flight budget scale for an attempt.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetryProfile {
    /// Matched by name in the history.
    pub name: &'static str,
    /// Fraction of each in-flight budget, in `(0, 1]`.
    pub memory_scale: f32,
}

/// Full, half and quarter budgets.
pub const RETRY_PROFILES: [RetryProfile; 3] = [
    RetryProfile {
        name: "default",
        memory_scale: 1.0,
    },
    RetryProfile {
        name: "reduced",
        memory_scale: 0.5,
    },
    RetryProfile {
        name: "minimal",
        memory_scale: 0.25,
    },
];

/// Which renderer an attempt used.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptRenderer {
    /// The GPU renderer.
    Gpu,
    /// The CPU renderer.
    Cpu,
}

/// Which video encoder an attempt used.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptEncoder {
    /// The platform's choice, hardware first.
    Default,
    /// A software encoder.
    Software,
}

/// A classified export failure, as the retry policy reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptFailure {
    /// The engine's error code (`DEVICE_LOST`, `OUT_OF_MEMORY`, ...).
    pub code: String,
    /// The phase it failed in (`prepare`, `decode`, `render`, `encode`, `publish`).
    pub phase: String,
    /// The export frame it failed at, when known.
    pub frame: Option<u64>,
    /// A wait the failure asks for before retrying, in milliseconds.
    pub retry_after_ms: Option<u64>,
}

/// One failed attempt.
#[derive(Clone, Debug, PartialEq)]
pub struct AttemptOutcome {
    /// Its failure.
    pub failure: AttemptFailure,
    /// Its budget profile.
    pub profile: RetryProfile,
    /// Its renderer.
    pub renderer: AttemptRenderer,
    /// Its video encoder.
    pub encoder: AttemptEncoder,
}

/// What the host can change between attempts.
#[derive(Clone, Copy, Debug)]
pub struct RetryOptions {
    /// Total attempts, the first included.
    pub max_attempts: usize,
    /// Whether an attempt can run on the CPU renderer.
    pub cpu_renderer: bool,
    /// Whether a software video encoder exists for the export's codec.
    pub software_encoder: bool,
}

impl Default for RetryOptions {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            cpu_renderer: true,
            software_encoder: true,
        }
    }
}

/// The policy's answer.
#[derive(Clone, Debug, PartialEq)]
pub enum RetryDecision {
    /// Run another attempt after `delay_ms` with these settings.
    Retry {
        /// Milliseconds to wait first.
        delay_ms: u64,
        /// The next attempt's budget profile.
        profile: RetryProfile,
        /// Its renderer.
        renderer: AttemptRenderer,
        /// Its video encoder.
        encoder: AttemptEncoder,
        /// Why, for the report.
        reason: String,
    },
    /// Stop with `code` (a GPU loss recurring at one frame stops as `DEVICE_HUNG`).
    Stop {
        /// Why, for the report.
        reason: String,
        /// The code the export fails with.
        code: Option<String>,
    },
}

const PERMANENT: [&str; 7] = [
    "CANCELLED",
    "INVALID_INPUT",
    "VALIDATION_FAILED",
    "RESOURCE_LIMIT",
    "STORAGE_QUOTA_EXCEEDED",
    "OUTPUT_IO_FAILED",
    "DEVICE_HUNG",
];
const MEMORY: [&str; 2] = ["OUT_OF_MEMORY", "ALLOCATION_FAILED"];
const GPU_LOSSES: [&str; 2] = ["DEVICE_LOST", "GPU_TIMEOUT"];

/// Whether a retry could help `failure` at all (the CPU renderer aside).
#[must_use]
pub fn retryable(failure: &AttemptFailure) -> bool {
    !PERMANENT.contains(&failure.code.as_str()) && failure.phase != "download"
}

fn backoff(failed: usize) -> u64 {
    1_000_u64.saturating_mul(1 << failed.saturating_sub(1).min(16))
}

/// Whether and how to retry after the failed attempts in `history`, oldest first.
///
/// An export is retried whole on a fresh device; attempts change the in-flight budget, the
/// renderer and the video encoder, never the output settings. A GPU that hangs on the export's
/// own work moves to the CPU renderer; memory exhaustion steps down a profile; an encode failure
/// retries once in software; a busy resource waits; anything else transient retries unchanged and
/// steps down only when it recurs at the same point.
// One rule after another, in the reference policy's order.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn decide_retry(history: &[AttemptOutcome], options: RetryOptions) -> RetryDecision {
    let Some(last) = history.last() else {
        return RetryDecision::Stop {
            reason: "nothing failed".into(),
            code: None,
        };
    };
    let failure = &last.failure;
    let code = failure.code.as_str();
    let mut note = "";
    let stop = |why: String, note: &str, final_code: &str| RetryDecision::Stop {
        reason: format!("{why}{note}"),
        code: Some(final_code.to_owned()),
    };
    let attempts_left = history.len() < options.max_attempts;
    let index = RETRY_PROFILES
        .iter()
        .position(|profile| profile.name == last.profile.name)
        .unwrap_or(0);
    let smaller = RETRY_PROFILES.get(index + 1).copied();
    let retry = |why: String,
                 note: &str,
                 profile: Option<RetryProfile>,
                 renderer: Option<AttemptRenderer>,
                 encoder: Option<AttemptEncoder>,
                 delay: Option<u64>| RetryDecision::Retry {
        delay_ms: delay.unwrap_or_else(|| backoff(history.len())),
        profile: profile.unwrap_or(RETRY_PROFILES[index]),
        renderer: renderer.unwrap_or(last.renderer),
        encoder: encoder.unwrap_or(last.encoder),
        reason: format!("{why}{note}"),
    };
    let previous = history.len().checked_sub(2).map(|at| &history[at]);
    let same_point = previous.is_some_and(|previous| {
        previous.failure.phase == failure.phase && previous.failure.frame == failure.frame
    });
    let recurred_loss = GPU_LOSSES.contains(&code)
        && previous.is_some_and(|previous| {
            previous.renderer == AttemptRenderer::Gpu
                && (GPU_LOSSES.contains(&previous.failure.code.as_str())
                    || previous.failure.code == "DEVICE_HUNG")
                && previous.failure.frame == failure.frame
        });
    if code == "DEVICE_HUNG" || recurred_loss {
        if last.renderer == AttemptRenderer::Gpu && options.cpu_renderer && attempts_left {
            return retry(
                format!(
                    "{code}: the export's own GPU work stops the GPU; retrying on the CPU renderer"
                ),
                note,
                None,
                Some(AttemptRenderer::Cpu),
                None,
                None,
            );
        }
        let why = if last.renderer == AttemptRenderer::Cpu {
            "it failed on the CPU renderer too"
        } else {
            "there is no CPU renderer to retry on"
        };
        return stop(
            format!("{code}: the export's own GPU work stops the GPU, and {why}"),
            note,
            "DEVICE_HUNG",
        );
    }
    if !retryable(failure) {
        return stop(
            format!("{code} at {} is not helped by a retry", failure.phase),
            note,
            code,
        );
    }
    if !attempts_left {
        return stop(
            format!("all {} attempts failed", options.max_attempts),
            note,
            code,
        );
    }
    if MEMORY.contains(&code) {
        return match smaller {
            Some(smaller) => retry(
                format!("{code}: retrying with less in flight ({})", smaller.name),
                note,
                Some(smaller),
                None,
                None,
                None,
            ),
            None => stop(
                format!(
                    "{code} with the smallest profile ({}): nothing left to reduce",
                    last.profile.name
                ),
                note,
                code,
            ),
        };
    }
    if failure.phase == "encode" && last.encoder == AttemptEncoder::Default {
        if options.software_encoder {
            return retry(
                format!("{code} while encoding: retrying with the software video encoder"),
                note,
                None,
                None,
                Some(AttemptEncoder::Software),
                None,
            );
        }
        note = " (no software video encoder to switch to)";
    }
    if code == "RESOURCE_BUSY" {
        let delay = failure
            .retry_after_ms
            .unwrap_or(0)
            .max(backoff(history.len()));
        return retry(
            format!("{code}: retrying when the device may be free"),
            note,
            None,
            None,
            None,
            Some(delay),
        );
    }
    let repeated = same_point && previous.is_some_and(|previous| previous.failure.code == code);
    if !repeated {
        return retry(
            format!("{code}: retrying on a fresh device"),
            note,
            None,
            None,
            None,
            None,
        );
    }
    match smaller {
        Some(smaller) => retry(
            format!(
                "{code} again at the same point: retrying with less in flight ({})",
                smaller.name
            ),
            note,
            Some(smaller),
            None,
            None,
            None,
        ),
        None => stop(
            format!("{code} again at the same point with the smallest profile: stopping early"),
            note,
            code,
        ),
    }
}

#[cfg(test)]
#[path = "export_policy_tests.rs"]
mod tests;

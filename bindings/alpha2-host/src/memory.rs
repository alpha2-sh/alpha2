//! The byte budgets an export runs within.
//!
//! Each budget resolves once per export: the request's `memory`, else `ALPHA2_*_BYTES`, else the
//! default. Budgets bound memory this host allocates (the decoder session budget bounds how many
//! decoders open, by estimate); none bounds the whole process, and encoder and driver memory is
//! outside all of them.
//!
//! Defaults are sized for a workstation. On a smaller machine or container the budgets left at
//! their defaults shrink together to half of what the process may use
//! ([`MemoryPolicy::resolve_on`]); explicitly set budgets are never shrunk.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    DEFAULT_AUDIO_WINDOW_BYTES, DEFAULT_DECODE_QUEUE_BYTES, DEFAULT_DECODER_SESSION_BYTES,
    DEFAULT_EXPORT_IN_FLIGHT_BYTES, DEFAULT_PROVIDER_SOURCE_BYTES, DEFAULT_REVERSE_CACHE_BYTES,
};

#[allow(clippy::struct_field_names)] // Named as the protocol names them.
#[derive(Copy, Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryRequest {
    /// Readback staging and packed encoder frames, or on the GPU-resident path the plane ring and
    /// encoder surfaces.
    pub in_flight_bytes: Option<u64>,
    pub decode_queue_bytes: Option<u64>,
    /// Frames a reversed clip keeps decoded.
    pub reverse_cache_bytes: Option<u64>,
    pub audio_window_bytes: Option<u64>,
    /// Kept frames and images of SVG, animated image, Lottie, OpenEXR and image sequence sources.
    pub provider_source_bytes: Option<u64>,
    /// By the host's per-session estimate; bounds how many sessions an export opens.
    pub decoder_session_bytes: Option<u64>,
    /// In (0, 1], applied to every budget after resolution, so a retry can ask for less without
    /// knowing the host's defaults.
    pub scale: Option<f64>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BudgetSource {
    Request,
    Environment,
    Default,
    /// The default, shrunk to fit the machine's memory.
    Machine,
    /// The default, lowered to a quarter of the adapter's device memory budget.
    Gpu,
}

impl BudgetSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Environment => "env",
            Self::Default => "default",
            Self::Machine => "machine",
            Self::Gpu => "gpu",
        }
    }
}

const MACHINE_BUDGET_FLOOR: u64 = 16 << 20;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MachineMemory {
    /// The lower of the cgroup or job limit and the physical total.
    pub effective: Option<u64>,
    /// `cgroup-v2`, `cgroup-v1`, `job`, or `physical`.
    pub source: &'static str,
    /// Reported available when the policy was resolved.
    pub available: Option<u64>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Budget {
    pub bytes: u64,
    pub source: BudgetSource,
    /// Before `memory.scale`: the most a working set needing more than `bytes` may still have.
    pub unscaled: u64,
}

/// What a budget may use, and how far `memory.scale` may be undone for one working set; a
/// retry's scale asks for less in flight, not for an export that cannot run.
pub use alpha2_host_core::audio_windows::ByteBudget;

impl Budget {
    fn resolve(requested: Option<u64>, variable: &str, default: u64) -> Self {
        Self::resolve_from(requested, variable, std::env::var_os(variable), default)
    }

    /// A setting that is not a positive byte count falls back to the default and is reported as
    /// the default, with a stderr line naming what was ignored.
    fn resolve_from(
        requested: Option<u64>,
        variable: &str,
        setting: Option<std::ffi::OsString>,
        default: u64,
    ) -> Self {
        if let Some(bytes) = requested {
            return Self {
                bytes,
                source: BudgetSource::Request,
                unscaled: bytes,
            };
        }
        let Some(setting) = setting else {
            return Self {
                bytes: default,
                source: BudgetSource::Default,
                unscaled: default,
            };
        };
        if let Some(bytes) = setting
            .to_str()
            .and_then(|text| text.parse::<u64>().ok())
            .filter(|bytes| *bytes > 0)
        {
            return Self {
                bytes,
                source: BudgetSource::Environment,
                unscaled: bytes,
            };
        }
        alpha2_os::diagnostic!(
            "alpha2-host: ignoring {variable}={setting:?}: not a positive byte count; \
             using the default {default}"
        );
        Self {
            bytes: default,
            source: BudgetSource::Default,
            unscaled: default,
        }
    }

    pub const fn byte_budget(self) -> ByteBudget {
        ByteBudget {
            bytes: self.bytes,
            ceiling: self.unscaled,
        }
    }

    fn to_json(self) -> Value {
        json!({ "bytes": self.bytes, "source": self.source.as_str() })
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct MemoryPolicy {
    pub in_flight: Budget,
    pub decode_queue: Budget,
    pub reverse_cache: Budget,
    pub audio_window: Budget,
    pub provider_sources: Budget,
    /// By estimate ([`crate::config::decoder_session_bytes`]).
    pub decoder_sessions: Budget,
    pub scale: f64,
    pub machine: MachineMemory,
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        Self::resolve(MemoryRequest::default()).expect("the defaults are valid")
    }
}

impl MemoryPolicy {
    /// # Errors
    /// Returns a message naming the field when a requested budget is zero.
    pub fn resolve(request: MemoryRequest) -> Result<Self, String> {
        Self::resolve_on(request, &machine_capacity(), None)
    }

    /// When the default budgets sum to more than half of what the process may use, each is scaled
    /// by the same factor (to at least 16 MiB) and reported as `machine`. The least working set may
    /// still use up to the default, as under `memory.scale`.
    ///
    /// A default in-flight budget above a quarter of `gpu_budget` is lowered to that quarter and
    /// reported as `gpu`, since on the GPU-resident path those bytes are device memory.
    ///
    /// # Errors
    /// Returns a message naming the field when a requested budget is zero.
    pub fn resolve_on(
        request: MemoryRequest,
        capacity: &alpha2_os::MemoryCapacity,
        gpu_budget: Option<u64>,
    ) -> Result<Self, String> {
        for (field, value) in [
            ("inFlightBytes", request.in_flight_bytes),
            ("decodeQueueBytes", request.decode_queue_bytes),
            ("reverseCacheBytes", request.reverse_cache_bytes),
            ("audioWindowBytes", request.audio_window_bytes),
            ("providerSourceBytes", request.provider_source_bytes),
            ("decoderSessionBytes", request.decoder_session_bytes),
        ] {
            if value == Some(0) {
                return Err(format!("memory.{field} must be a positive byte count"));
            }
        }
        let scale = request.scale.unwrap_or(1.0);
        if !(scale > 0.0 && scale <= 1.0) {
            return Err("memory.scale must be above 0 and at most 1".to_owned());
        }
        let policy = Self {
            in_flight: Budget::resolve(
                request.in_flight_bytes,
                "ALPHA2_EXPORT_IN_FLIGHT_BYTES",
                DEFAULT_EXPORT_IN_FLIGHT_BYTES,
            ),
            decode_queue: Budget::resolve(
                request.decode_queue_bytes,
                "ALPHA2_DECODE_QUEUE_BYTES",
                DEFAULT_DECODE_QUEUE_BYTES,
            ),
            reverse_cache: Budget::resolve(
                request.reverse_cache_bytes,
                "ALPHA2_REVERSE_DECODE_CACHE_BYTES",
                DEFAULT_REVERSE_CACHE_BYTES,
            ),
            audio_window: Budget::resolve(
                request.audio_window_bytes,
                "ALPHA2_AUDIO_WINDOW_BYTES",
                DEFAULT_AUDIO_WINDOW_BYTES,
            ),
            provider_sources: Budget::resolve(
                request.provider_source_bytes,
                "ALPHA2_PROVIDER_SOURCE_BYTES",
                DEFAULT_PROVIDER_SOURCE_BYTES,
            ),
            decoder_sessions: Budget::resolve(
                request.decoder_session_bytes,
                "ALPHA2_DECODER_SESSION_BYTES",
                DEFAULT_DECODER_SESSION_BYTES,
            ),
            scale,
            machine: MachineMemory {
                effective: capacity.effective_limit(),
                source: capacity.limit_source,
                available: capacity.available_bytes,
            },
        };
        Ok(policy
            .fitted_to_machine()
            .fitted_to_gpu(gpu_budget)
            .scaled())
    }

    fn fitted_to_gpu(mut self, gpu_budget: Option<u64>) -> Self {
        let Some(quarter) = gpu_budget.map(|bytes| bytes / 4) else {
            return self;
        };
        let budget = &mut self.in_flight;
        if matches!(budget.source, BudgetSource::Default | BudgetSource::Machine)
            && budget.bytes > quarter
        {
            budget.bytes = quarter.max(1);
            budget.source = BudgetSource::Gpu;
        }
        self
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )] // Byte counts times a fraction below 1, floored, stay within u64.
    fn fitted_to_machine(mut self) -> Self {
        let Some(effective) = self.machine.effective else {
            return self;
        };
        let half = effective / 2;
        let defaults = self
            .budgets_mut()
            .into_iter()
            .filter(|budget| budget.source == BudgetSource::Default)
            .map(|budget| budget.bytes)
            .fold(0_u64, u64::saturating_add);
        if defaults <= half {
            return self;
        }
        let factor = half as f64 / defaults as f64;
        for budget in self.budgets_mut() {
            if budget.source == BudgetSource::Default {
                budget.bytes = ((budget.bytes as f64 * factor).floor() as u64)
                    .max(MACHINE_BUDGET_FLOOR)
                    .min(budget.bytes);
                budget.source = BudgetSource::Machine;
            }
        }
        self
    }

    fn budgets_mut(&mut self) -> [&mut Budget; 6] {
        [
            &mut self.in_flight,
            &mut self.decode_queue,
            &mut self.reverse_cache,
            &mut self.audio_window,
            &mut self.provider_sources,
            &mut self.decoder_sessions,
        ]
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )] // A byte count times a fraction in (0, 1], floored, stays within u64.
    fn scaled(mut self) -> Self {
        let scale = self.scale;
        if scale < 1.0 {
            for budget in self.budgets_mut() {
                budget.bytes = ((budget.bytes as f64 * scale).floor() as u64).max(1);
            }
        }
        self
    }

    pub fn to_json(self) -> Value {
        json!({
            "inFlightBytes": self.in_flight.to_json(),
            "decodeQueueBytes": self.decode_queue.to_json(),
            "reverseCacheBytes": self.reverse_cache.to_json(),
            "audioWindowBytes": self.audio_window.to_json(),
            "providerSourceBytes": self.provider_sources.to_json(),
            "decoderSessionBytes": self.decoder_sessions.to_json(),
            "scale": self.scale,
            "machine": {
                "effective": self.machine.effective,
                "source": self.machine.source,
                "available": self.machine.available,
            },
            "scope": "memory this host reserves before allocating, and decoder sessions by estimate; encoder and driver memory are outside every budget",
        })
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct InFlightBudget {
    pub bytes: u64,
    pub source: BudgetSource,
    pub scale: f64,
    /// ([`ByteBudget::ceiling`]).
    pub unscaled: u64,
}

impl InFlightBudget {
    #[cfg(test)]
    pub const fn requested(bytes: u64) -> Self {
        Self {
            bytes,
            source: BudgetSource::Request,
            scale: 1.0,
            unscaled: bytes,
        }
    }

    pub fn holding(self, least: u64) -> u64 {
        ByteBudget {
            bytes: self.bytes,
            ceiling: self.unscaled,
        }
        .holding(least)
    }

    /// Names the setting that applied (`memory.inFlightBytes`, `ALPHA2_EXPORT_IN_FLIGHT_BYTES`, or
    /// the default) and any scale. Since the least is held whatever the scale, what refuses a
    /// pipeline is the unscaled budget.
    pub fn describe(self) -> String {
        let origin = match self.source {
            BudgetSource::Request => "memory.inFlightBytes",
            BudgetSource::Environment => "ALPHA2_EXPORT_IN_FLIGHT_BYTES",
            BudgetSource::Default => "the default",
            BudgetSource::Machine => "the default fitted to this machine's memory",
            BudgetSource::Gpu => "the default fitted to the GPU's memory budget",
        };
        if self.scale < 1.0 {
            format!(
                "the {}-byte in-flight budget ({origin}; memory.scale {} asked for {})",
                self.unscaled, self.scale, self.bytes
            )
        } else {
            format!("the {}-byte in-flight budget ({origin})", self.bytes)
        }
    }
}

impl MemoryPolicy {
    pub const fn in_flight_budget(&self) -> InFlightBudget {
        InFlightBudget {
            bytes: self.in_flight.bytes,
            source: self.in_flight.source,
            scale: self.scale,
            unscaled: self.in_flight.unscaled,
        }
    }
}

crate::tag::tags! {
    /// A budget cannot hold the least the export needs.
    pub fn resource_limit_tag = "RESOURCE_LIMIT] ";
}

pub fn resource_limit(message: impl std::fmt::Display) -> String {
    format!("{}{message}", resource_limit_tag())
}

crate::tag::tags! {
    /// Refused before any output opened: the least working set exceeds what the machine has
    /// available.
    fn machine_memory_tag = "STAGE=machine-memory] ";
    /// Stopped because the cgroup's memory reached its limit.
    fn memory_limit_reached_tag = "STAGE=memory-limit-reached] ";
}

pub fn machine_memory_limit(message: impl std::fmt::Display) -> String {
    format!("{}{}{message}", resource_limit_tag(), machine_memory_tag())
}

// Only the Linux watchdog stops an export this way.
#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
pub fn memory_limit_reached(message: impl std::fmt::Display) -> String {
    format!(
        "{}{}{message}",
        resource_limit_tag(),
        memory_limit_reached_tag()
    )
}

/// Tests may substitute the capacity per thread.
pub fn machine_capacity() -> alpha2_os::MemoryCapacity {
    #[cfg(test)]
    if let Some(capacity) = injected::CAPACITY.with(std::cell::Cell::get) {
        return capacity;
    }
    alpha2_os::memory_capacity()
}

#[cfg(test)]
pub mod injected {
    thread_local! {
        pub static CAPACITY: std::cell::Cell<Option<alpha2_os::MemoryCapacity>> =
            const { std::cell::Cell::new(None) };
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct LeastWorkingSet {
    /// One rendered frame read back and two packed for the encoder.
    pub in_flight: u64,
    /// One session per video read at once, at its fitted thread count
    /// ([`crate::config::decoder_session_bytes`]).
    pub decoder_sessions: u64,
    /// What the renderer keeps in system memory (the CPU renderer, or a GPU with shared memory).
    pub renderer: u64,
}

impl LeastWorkingSet {
    pub fn estimate(
        (width, height): (u32, u32),
        (videos, decoder_threads): (usize, i64),
        frame_bytes: u64,
        renderer: u64,
    ) -> Self {
        let pixels = u64::from(width).saturating_mul(u64::from(height));
        // An rgba16float readback, two 8-bit 4:2:0 packed frames.
        let in_flight = pixels
            .saturating_mul(8)
            .saturating_add(pixels.saturating_mul(3));
        let sessions = u64::try_from(videos.min(crate::MAX_DECODER_SESSIONS)).unwrap_or(1);
        Self {
            in_flight,
            decoder_sessions: crate::config::decoder_session_bytes(frame_bytes, decoder_threads)
                .saturating_mul(sessions),
            renderer,
        }
    }

    pub const fn total(self) -> u64 {
        self.in_flight
            .saturating_add(self.decoder_sessions)
            .saturating_add(self.renderer)
    }

    /// A quarter more for what no estimate counts (codec buffers, inter-stage queues); measured
    /// peaks reached about 1.2 times the total.
    pub const fn expected_peak(self) -> u64 {
        self.total().saturating_add(self.total() / 4)
    }
}

/// Refuses an export before any output opens when [`LeastWorkingSet::expected_peak`] exceeds the
/// lower of what the OS reports available and what the cgroup or job limit has left. Nothing is
/// refused where neither is known.
///
/// # Errors
/// A `RESOURCE_LIMIT` refusal with `failure.stage` `machine-memory` naming both numbers.
pub fn admit(least: LeastWorkingSet, capacity: &alpha2_os::MemoryCapacity) -> Result<(), String> {
    let Some(headroom) = capacity.headroom() else {
        return Ok(());
    };
    if least.expected_peak() <= headroom {
        return Ok(());
    }
    Err(machine_memory_limit(format!(
        "the export needs about {} bytes of memory (in flight {}, decoder sessions {}, \
         renderer {}, and a quarter more for the codecs and queues), and this machine can give \
         the host {headroom} bytes now ({}{})",
        least.expected_peak(),
        least.in_flight,
        least.decoder_sessions,
        least.renderer,
        capacity.available_bytes.map_or_else(
            || "available unknown".to_owned(),
            |bytes| format!("{bytes} available")
        ),
        capacity
            .limit_bytes
            .zip(capacity.used_in_limit_bytes)
            .map_or_else(String::new, |(limit, used)| format!(
                ", {} limit {limit} with {used} used",
                capacity.limit_source
            )),
    )))
}

/// Stops an export once the cgroup v2 working set reaches 95 % of the limit, so it fails with
/// `RESOURCE_LIMIT` (`memory-limit-reached`) instead of being silently OOM-killed. Of the exports
/// running at once, the newest stops first; the next newest stops only if the set stays over the
/// limit once it has gone. Best effort: an allocation that jumps past the limit between looks
/// still gets the process killed. Linux only.
pub struct MemoryWatchdog {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    stop: Option<(std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>)>,
    #[cfg(any(target_os = "linux", target_os = "android"))]
    watched: u64,
}

/// The watchdogs running, oldest first.
#[cfg(any(target_os = "linux", target_os = "android"))]
static WATCHED: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());

#[cfg(any(target_os = "linux", target_os = "android"))]
static NEXT_WATCHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(any(target_os = "linux", target_os = "android"))]
fn watchdogs() -> std::sync::MutexGuard<'static, Vec<u64>> {
    WATCHED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl MemoryWatchdog {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const LIMIT_PERCENT: u64 = 95;
    /// Startup working sets have grown about 100 MiB per 50 ms, which a slower look misses between
    /// 90 % and the kill. A look reads three small files.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        allow(clippy::needless_pass_by_value)
    )]
    pub fn start(failure: crate::encode::PipelineFailure) -> Self {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let watched = NEXT_WATCHED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if alpha2_os::cgroup_v2_usage().is_none() {
                return Self {
                    stop: None,
                    watched,
                };
            }
            watchdogs().push(watched);
            let request = crate::termination::current_request();
            let (sender, receiver) = std::sync::mpsc::channel::<()>();
            let spawned = std::thread::Builder::new()
                .name("alpha2 memory watchdog".to_owned())
                .spawn(move || {
                    while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                        receiver.recv_timeout(Self::INTERVAL)
                    {
                        let newest = watchdogs().last() == Some(&watched);
                        if newest && let Some(reached) = Self::reached(alpha2_os::cgroup_v2_usage())
                        {
                            failure.record(&reached);
                            crate::termination::terminations().cancel_running_request(request);
                            return;
                        }
                    }
                });
            Self {
                stop: spawned.ok().map(|handle| (sender, handle)),
                watched,
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let _ = failure;
            Self {}
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn reached(usage: Option<(u64, u64)>) -> Option<String> {
        let (used, limit) = usage?;
        (used.saturating_mul(100) >= limit.saturating_mul(Self::LIMIT_PERCENT)).then(|| {
            memory_limit_reached(format!(
                "the host's cgroup holds {used} bytes of its {limit}-byte memory limit; the \
                 export stopped before the kernel would kill the process"
            ))
        })
    }
}

impl Drop for MemoryWatchdog {
    fn drop(&mut self) {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            if let Some((sender, handle)) = self.stop.take() {
                drop(sender);
                let _ = handle.join();
            }
            watchdogs().retain(|watched| *watched != self.watched);
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RefusalStage {
    MachineMemory,
    MemoryLimitReached,
}

impl RefusalStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MachineMemory => "machine-memory",
            Self::MemoryLimitReached => "memory-limit-reached",
        }
    }
}

pub fn take_stage(message: &str) -> Option<RefusalStage> {
    if message.contains(machine_memory_tag()) {
        Some(RefusalStage::MachineMemory)
    } else if message.contains(memory_limit_reached_tag()) {
        Some(RefusalStage::MemoryLimitReached)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_requested_budget_wins_and_says_so() {
        let policy = MemoryPolicy::resolve(MemoryRequest {
            in_flight_bytes: Some(64 << 20),
            ..MemoryRequest::default()
        })
        .expect("valid");
        assert_eq!(policy.in_flight.bytes, 64 << 20);
        assert_eq!(policy.in_flight.source, BudgetSource::Request);
        assert_eq!(policy.to_json()["inFlightBytes"]["source"], "request");
    }

    #[test]
    fn a_zero_budget_is_refused_by_name() {
        let error = MemoryPolicy::resolve(MemoryRequest {
            decode_queue_bytes: Some(0),
            ..MemoryRequest::default()
        })
        .expect_err("zero");
        assert!(error.contains("memory.decodeQueueBytes"), "{error}");
    }

    #[test]
    fn a_scale_shrinks_every_resolved_budget() {
        let policy = MemoryPolicy::resolve_on(
            MemoryRequest {
                in_flight_bytes: Some(100),
                scale: Some(0.5),
                ..MemoryRequest::default()
            },
            &machine(64 << 30),
            None,
        )
        .expect("valid");
        assert_eq!(policy.in_flight.bytes, 50);
        assert_eq!(policy.decode_queue.bytes, DEFAULT_DECODE_QUEUE_BYTES / 2);
        for scale in [0.0, 1.5, f64::NAN] {
            assert!(
                MemoryPolicy::resolve(MemoryRequest {
                    scale: Some(scale),
                    ..MemoryRequest::default()
                })
                .is_err()
            );
        }
    }

    #[test]
    fn an_environment_budget_is_reported_only_when_it_applied() {
        let resolve = |setting: Option<&str>| {
            Budget::resolve_from(
                None,
                "ALPHA2_EXPORT_IN_FLIGHT_BYTES",
                setting.map(std::ffi::OsString::from),
                256,
            )
        };
        assert_eq!(
            resolve(Some("1024")),
            Budget {
                bytes: 1024,
                source: BudgetSource::Environment,
                unscaled: 1024,
            }
        );
        for ignored in [Some("1GiB"), Some("0"), Some("-5"), Some(""), None] {
            assert_eq!(
                resolve(ignored),
                Budget {
                    bytes: 256,
                    source: BudgetSource::Default,
                    unscaled: 256,
                },
                "{ignored:?}"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let not_text = Budget::resolve_from(
                None,
                "ALPHA2_EXPORT_IN_FLIGHT_BYTES",
                Some(std::ffi::OsString::from_vec(b"1\xff".to_vec())),
                256,
            );
            assert_eq!(not_text.source, BudgetSource::Default);
        }
        assert_eq!(
            Budget::resolve_from(Some(7), "X", Some("1024".into()), 256).source,
            BudgetSource::Request
        );
    }

    #[test]
    fn the_in_flight_budget_is_described_by_its_origin() {
        let policy = MemoryPolicy::resolve(MemoryRequest {
            in_flight_bytes: Some(1 << 20),
            scale: Some(0.25),
            ..MemoryRequest::default()
        })
        .expect("valid");
        assert_eq!(
            policy.in_flight_budget().describe(),
            "the 1048576-byte in-flight budget (memory.inFlightBytes; memory.scale 0.25 asked for 262144)"
        );
        let from_env = InFlightBudget {
            bytes: 64,
            source: BudgetSource::Environment,
            scale: 1.0,
            unscaled: 64,
        };
        assert_eq!(
            from_env.describe(),
            "the 64-byte in-flight budget (ALPHA2_EXPORT_IN_FLIGHT_BYTES)"
        );
        let default = InFlightBudget {
            source: BudgetSource::Default,
            ..from_env
        };
        assert_eq!(
            default.describe(),
            "the 64-byte in-flight budget (the default)"
        );
    }

    fn machine(effective: u64) -> alpha2_os::MemoryCapacity {
        alpha2_os::MemoryCapacity {
            physical_bytes: Some(64 << 30),
            available_bytes: Some(effective / 2),
            limit_bytes: Some(effective),
            limit_source: "cgroup-v2",
            used_in_limit_bytes: Some(0),
        }
    }

    #[test]
    fn default_budgets_shrink_to_half_the_machine() {
        let policy = MemoryPolicy::resolve_on(MemoryRequest::default(), &machine(1 << 30), None)
            .expect("valid");
        let budgets = [
            policy.in_flight,
            policy.decode_queue,
            policy.reverse_cache,
            policy.audio_window,
            policy.provider_sources,
            policy.decoder_sessions,
        ];
        let total = budgets.iter().map(|budget| budget.bytes).sum::<u64>();
        assert!(total <= 1 << 29, "{total}");
        assert!(total > (1 << 29) - 6, "{total}");
        for budget in budgets {
            assert_eq!(budget.source, BudgetSource::Machine);
            assert!(budget.bytes >= MACHINE_BUDGET_FLOOR);
            assert!(budget.bytes < budget.unscaled);
        }
        #[allow(clippy::cast_precision_loss)] // Budgets far below 2^52 bytes.
        let share = |budget: Budget| budget.bytes as f64 / budget.unscaled as f64;
        assert!((share(policy.in_flight) - share(policy.decoder_sessions)).abs() < 1e-6);
        let report = policy.to_json();
        assert_eq!(report["decoderSessionBytes"]["source"], "machine");
        assert_eq!(report["machine"]["effective"], 1_u64 << 30);
        assert_eq!(report["machine"]["source"], "cgroup-v2");
        assert_eq!(report["machine"]["available"], 1_u64 << 29);
        let roomy = MemoryPolicy::resolve_on(MemoryRequest::default(), &machine(64 << 30), None)
            .expect("valid");
        assert_eq!(roomy.decoder_sessions.bytes, DEFAULT_DECODER_SESSION_BYTES);
        assert_eq!(roomy.decoder_sessions.source, BudgetSource::Default);
    }

    #[test]
    fn requested_budgets_are_never_shrunk() {
        let policy = MemoryPolicy::resolve_on(
            MemoryRequest {
                decoder_session_bytes: Some(8 << 30),
                ..MemoryRequest::default()
            },
            &machine(1 << 30),
            None,
        )
        .expect("valid");
        assert_eq!(policy.decoder_sessions.bytes, 8 << 30);
        assert_eq!(policy.decoder_sessions.source, BudgetSource::Request);
        assert_eq!(policy.in_flight.source, BudgetSource::Machine);
        let shrunk = policy.in_flight.bytes
            + policy.decode_queue.bytes
            + policy.reverse_cache.bytes
            + policy.audio_window.bytes
            + policy.provider_sources.bytes;
        assert!(shrunk <= 1 << 29, "{shrunk}");
        // The scale applies after the fit.
        let fitted = MemoryPolicy::resolve_on(MemoryRequest::default(), &machine(1 << 30), None)
            .expect("valid");
        let scaled = MemoryPolicy::resolve_on(
            MemoryRequest {
                scale: Some(0.5),
                ..MemoryRequest::default()
            },
            &machine(1 << 30),
            None,
        )
        .expect("valid");
        assert_eq!(scaled.in_flight.bytes, fitted.in_flight.bytes / 2);
    }

    #[test]
    fn the_in_flight_default_is_held_to_a_quarter_of_the_gpu_budget() {
        let small = MemoryPolicy::resolve_on(
            MemoryRequest::default(),
            &machine(64 << 30),
            Some(512 << 20),
        )
        .expect("valid");
        assert_eq!(small.in_flight.bytes, 128 << 20);
        assert_eq!(small.in_flight.source, BudgetSource::Gpu);
        assert_eq!(small.in_flight.unscaled, DEFAULT_EXPORT_IN_FLIGHT_BYTES);
        assert_eq!(small.to_json()["inFlightBytes"]["source"], "gpu");
        // Only the in-flight budget is device memory.
        assert_eq!(small.decode_queue.source, BudgetSource::Default);
        let requested = MemoryPolicy::resolve_on(
            MemoryRequest {
                in_flight_bytes: Some(1 << 30),
                ..MemoryRequest::default()
            },
            &machine(64 << 30),
            Some(512 << 20),
        )
        .expect("valid");
        assert_eq!(requested.in_flight.bytes, 1 << 30);
        assert_eq!(requested.in_flight.source, BudgetSource::Request);
        let roomy =
            MemoryPolicy::resolve_on(MemoryRequest::default(), &machine(64 << 30), Some(24 << 30))
                .expect("valid");
        assert_eq!(roomy.in_flight.bytes, DEFAULT_EXPORT_IN_FLIGHT_BYTES);
        assert_eq!(roomy.in_flight.source, BudgetSource::Default);
    }

    #[test]
    fn unknown_fields_are_refused() {
        assert!(serde_json::from_value::<MemoryRequest>(json!({ "gpuBytes": 1 })).is_err());
    }
}

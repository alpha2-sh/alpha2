//! Whether `serve` takes new requests, and the soft limits after which it retires.
//!
//! The host decides when a new process would serve better than itself and reports `draining`;
//! a client only follows. Soft limits are compared between requests, never during one.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::PROTOCOL_VERSION;
use crate::serve_io::Responder;

/// How long a draining host keeps answering `HOST_RETIRING` for a client that has not closed its
/// input yet.
pub(crate) const DRAIN_GRACE: Duration = Duration::from_secs(5);

const DEFAULT_MAX_MEMORY_SHARE: f64 = 0.5;
const DEFAULT_MAX_MEMORY_GROWTH_BYTES: u64 = 1 << 30;
const DEFAULT_MAX_REQUESTS: u64 = 1000;
const DEFAULT_MAX_HANDLES: u64 = 4096;
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_MAX_RUNNING: usize = 2;

/// `serve`'s flags.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ServeOptions {
    pub limits: SoftLimits,
    /// How many requests run at once at most.
    pub max_running: usize,
    /// Extraction's step timeout and plugin limits.
    pub extract: crate::extract_request::ExtractOptions,
}

impl ServeOptions {
    /// Reads `serve`'s flags over the defaults.
    ///
    /// # Errors
    /// An unknown flag, a missing value or one out of range.
    pub(crate) fn from_args(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut max_running = DEFAULT_MAX_RUNNING;
        let mut extract = crate::extract_request::ExtractOptions::default();
        let mut rest = Vec::new();
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let mut number = |what: &str| -> Result<u64, String> {
                let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
                value
                    .parse::<u64>()
                    .map_err(|_| format!("{flag} takes {what}, got {value}"))
            };
            if flag == "--extract-step-timeout" {
                let millis = number("milliseconds")?;
                extract.step_timeout = (millis > 0).then(|| Duration::from_millis(millis));
            } else if flag == "--plugin-call-timeout" {
                let millis = number("milliseconds")?;
                if millis == 0 {
                    return Err(format!("{flag} takes milliseconds from 1"));
                }
                extract.plugin_call_deadline = Duration::from_millis(millis);
            } else if flag == "--plugin-memory-limit" {
                let bytes = number("bytes")?;
                if bytes == 0 {
                    return Err(format!("{flag} takes bytes from 1"));
                }
                extract.plugin_memory_bytes = bytes;
            } else if flag == "--max-running" {
                let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
                max_running = value
                    .parse::<usize>()
                    .ok()
                    .filter(|count| *count > 0)
                    .ok_or_else(|| format!("{flag} takes a count from 1, got {value}"))?;
            } else {
                rest.push(flag);
            }
        }
        Ok(Self {
            limits: SoftLimits::from_args(rest)?,
            max_running,
            extract,
        })
    }
}

/// `None` turns a limit off.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SoftLimits {
    pub max_memory_share: Option<f64>,
    pub max_memory_growth_bytes: Option<u64>,
    pub max_requests: Option<u64>,
    pub max_handles: Option<u64>,
    pub idle_timeout: Option<Duration>,
    /// Whether a degradation a new process would not have drains the host.
    pub drain_degraded: bool,
}

impl Default for SoftLimits {
    fn default() -> Self {
        // Half the descriptor limit leaves room for the request that crosses the limit.
        let handles = alpha2_os::open_file_limit().map_or(DEFAULT_MAX_HANDLES, |limit| {
            DEFAULT_MAX_HANDLES.min(limit / 2)
        });
        Self {
            max_memory_share: Some(DEFAULT_MAX_MEMORY_SHARE),
            max_memory_growth_bytes: Some(DEFAULT_MAX_MEMORY_GROWTH_BYTES),
            max_requests: Some(DEFAULT_MAX_REQUESTS),
            max_handles: Some(handles.max(1)),
            idle_timeout: Some(DEFAULT_IDLE_TIMEOUT),
            drain_degraded: true,
        }
    }
}

impl SoftLimits {
    #[cfg(test)]
    pub(crate) const OFF: Self = Self {
        max_memory_share: None,
        max_memory_growth_bytes: None,
        max_requests: None,
        max_handles: None,
        idle_timeout: None,
        drain_degraded: false,
    };

    /// Reads `serve`'s flags over the defaults; `0` turns a limit off.
    ///
    /// # Errors
    /// An unknown flag, a missing value or one out of range.
    pub(crate) fn from_args(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut limits = Self::default();
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            let count = || {
                value
                    .parse::<u64>()
                    .map_err(|_| format!("{flag} takes a whole number, got {value}"))
            };
            match flag.as_str() {
                "--max-memory-share" => {
                    let share = value
                        .parse::<f64>()
                        .ok()
                        .filter(|share| (0.0..=1.0).contains(share))
                        .ok_or_else(|| {
                            format!("{flag} takes a fraction from 0 to 1, got {value}")
                        })?;
                    limits.max_memory_share = (share > 0.0).then_some(share);
                }
                "--max-memory-growth" => limits.max_memory_growth_bytes = nonzero(count()?),
                "--max-requests" => limits.max_requests = nonzero(count()?),
                "--max-handles" => limits.max_handles = nonzero(count()?),
                "--idle-timeout" => {
                    limits.idle_timeout = nonzero(count()?).map(Duration::from_millis);
                }
                _ => return Err(format!("unknown serve flag {flag}")),
            }
        }
        Ok(limits)
    }

    fn to_json(self) -> Value {
        json!({
            "maxMemoryShare": self.max_memory_share,
            "maxMemoryGrowthBytes": self.max_memory_growth_bytes,
            "maxRequests": self.max_requests,
            "maxHandles": self.max_handles,
            "idleTimeoutMs": self.idle_timeout.map(|limit| limit.as_millis()),
        })
    }
}

const fn nonzero(value: u64) -> Option<u64> {
    if value == 0 { None } else { Some(value) }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HostState {
    Ready,
    Draining,
}

impl HostState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Draining => "draining",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DrainReason {
    Memory,
    MemoryGrowth,
    Requests,
    Handles,
    Degraded,
    Idle,
    DeviceLost,
    ThreadsAbandoned,
    Terminating,
    InputClosed,
}

impl DrainReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::MemoryGrowth => "memory-growth",
            Self::Requests => "requests",
            Self::Handles => "handles",
            Self::Degraded => "degraded",
            Self::Idle => "idle",
            Self::DeviceLost => "device-lost",
            Self::ThreadsAbandoned => "threads-abandoned",
            Self::Terminating => "terminating",
            Self::InputClosed => "input-closed",
        }
    }

    /// The process cannot run another request and exits once it has answered.
    pub(crate) const fn exits_at_once(self) -> bool {
        matches!(self, Self::DeviceLost | Self::ThreadsAbandoned)
    }
}

/// What the renderer the host made says about itself.
pub(crate) struct RendererFacts {
    pub renderer: &'static str,
    pub fallback: Option<String>,
    pub hardware_decode_fallback: Option<String>,
}

#[derive(Default)]
struct Life {
    draining: Option<(DrainReason, Instant)>,
    requests_served: u64,
    running: usize,
    baseline_footprint: Option<u64>,
    renderer: Option<RendererFacts>,
}

/// Shared by the request thread, which changes it, and the reader thread, which answers `status`.
pub(crate) struct Lifecycle {
    limits: SoftLimits,
    life: Mutex<Life>,
}

impl Lifecycle {
    pub(crate) fn new(limits: SoftLimits) -> Self {
        Self {
            limits,
            life: Mutex::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Life> {
        self.life.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) const fn idle_timeout(&self) -> Option<Duration> {
        self.limits.idle_timeout
    }

    pub(crate) fn state(&self) -> HostState {
        if self.lock().draining.is_some() {
            HostState::Draining
        } else {
            HostState::Ready
        }
    }

    /// When the host started draining, if it has.
    pub(crate) fn draining_since(&self) -> Option<(DrainReason, Instant)> {
        self.lock().draining
    }

    pub(crate) fn set_renderer(&self, facts: RendererFacts) {
        self.lock().renderer = Some(facts);
    }

    pub(crate) fn begin_request(&self) {
        self.lock().running += 1;
    }

    /// Counts the answer and takes the memory baseline after the first.
    pub(crate) fn end_request(&self) {
        let footprint = footprint_bytes();
        let mut life = self.lock();
        life.running = life.running.saturating_sub(1);
        life.requests_served += 1;
        if life.baseline_footprint.is_none() {
            life.baseline_footprint = footprint;
        }
    }

    /// The first soft limit the host is over now, if any.
    pub(crate) fn over_soft_limit(&self) -> Option<DrainReason> {
        self.over_limit(&Usage::now())
    }

    fn over_limit(&self, usage: &Usage) -> Option<DrainReason> {
        let (served, baseline) = {
            let life = self.lock();
            (life.requests_served, life.baseline_footprint)
        };
        let limits = self.limits;
        #[allow(clippy::cast_precision_loss)]
        if let (Some(share), Some(footprint), Some(capacity)) =
            (limits.max_memory_share, usage.footprint, usage.capacity)
            && footprint as f64 > share * capacity as f64
        {
            return Some(DrainReason::Memory);
        }
        if let (Some(growth), Some(footprint), Some(baseline)) =
            (limits.max_memory_growth_bytes, usage.footprint, baseline)
            && footprint.saturating_sub(baseline) > growth
        {
            return Some(DrainReason::MemoryGrowth);
        }
        if limits.max_requests.is_some_and(|limit| served >= limit) {
            return Some(DrainReason::Requests);
        }
        if let (Some(limit), Some(open)) = (limits.max_handles, usage.handles)
            && open > limit
        {
            return Some(DrainReason::Handles);
        }
        if limits.drain_degraded && usage.degraded {
            return Some(DrainReason::Degraded);
        }
        None
    }

    /// Moves to `draining` and writes the `state` message. A host already draining keeps its
    /// first reason and writes nothing.
    ///
    /// # Errors
    /// When the output refuses the message.
    pub(crate) fn drain(
        &self,
        reason: DrainReason,
        out: &Responder,
    ) -> Result<(), crate::protocol::HostError> {
        {
            let mut life = self.lock();
            if life.draining.is_some() {
                return Ok(());
            }
            life.draining = Some((reason, Instant::now()));
        }
        out.send(&self.state_message())
    }

    /// `{type: "state", state, reason, details}` as the host is now.
    pub(crate) fn state_message(&self) -> Value {
        let mut message = self.status();
        message["protocolVersion"] = json!(PROTOCOL_VERSION);
        message["type"] = json!("state");
        message
    }

    /// `{state, reason, details}` as the host is now.
    pub(crate) fn status(&self) -> Value {
        let life = self.lock();
        let (state, reason) = match life.draining {
            Some((reason, _)) => (HostState::Draining, reason.as_str()),
            None => (HostState::Ready, "started"),
        };
        let memory = alpha2_os::process_memory();
        let renderer = life.renderer.as_ref();
        json!({
            "state": state.as_str(),
            "reason": reason,
            "details": {
                "activity": if life.running == 0 { "idle" } else { "busy" },
                "runningRequests": life.running,
                "requestsServed": life.requests_served,
                "memory": {
                    "residentBytes": memory.resident_bytes,
                    "footprintBytes": memory.footprint_bytes.or(memory.resident_bytes),
                    "baselineBytes": life.baseline_footprint,
                    "capacityBytes": alpha2_os::memory_capacity().effective_limit(),
                },
                "openHandles": alpha2_os::open_handles(),
                "renderer": renderer.map(|facts| facts.renderer),
                "rendererFallback": renderer.and_then(|facts| facts.fallback.clone()),
                "hardwareDecodeFallback":
                    renderer.and_then(|facts| facts.hardware_decode_fallback.clone()),
                "degradations": degradations(),
                "limits": self.limits.to_json(),
            },
        })
    }
}

/// What the soft limits are compared with.
struct Usage {
    footprint: Option<u64>,
    capacity: Option<u64>,
    handles: Option<u64>,
    degraded: bool,
}

impl Usage {
    fn now() -> Self {
        Self {
            footprint: footprint_bytes(),
            capacity: alpha2_os::memory_capacity().effective_limit(),
            handles: alpha2_os::open_handles(),
            degraded: !degradations().is_empty(),
        }
    }
}

/// The footprint, else resident memory where the system reports no footprint.
fn footprint_bytes() -> Option<u64> {
    let memory = alpha2_os::process_memory();
    memory.footprint_bytes.or(memory.resident_bytes)
}

/// What this process has lost that a new process would have again.
fn degradations() -> Vec<String> {
    #[allow(unused_mut)]
    let mut lost = crate::render_cache::abandoned_directory_reasons();
    #[cfg(all(target_os = "windows", feature = "hwencode"))]
    lost.extend(alpha2_native::d3d12va_timeouts());
    lost
}

static ANNOUNCED: OnceLock<(Arc<Lifecycle>, Arc<Responder>)> = OnceLock::new();

/// Lets a termination that ends the process say so first ([`announce_termination`]).
pub(crate) fn install(lifecycle: &Arc<Lifecycle>, out: &Arc<Responder>) {
    let _ = ANNOUNCED.set((Arc::clone(lifecycle), Arc::clone(out)));
}

/// Writes the `terminating` state message, when `serve` installed one. Nothing may depend on it:
/// the client may have closed the pipe.
pub(crate) fn announce_termination() {
    if let Some((lifecycle, out)) = ANNOUNCED.get() {
        let _ = lifecycle.drain(DrainReason::Terminating, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn serve_flags_override_the_defaults_and_zero_turns_a_limit_off() {
        let limits = SoftLimits::from_args(args(
            "--max-memory-share 0.25 --max-memory-growth 0 --max-requests 3 --max-handles 0 --idle-timeout 50",
        ))
        .expect("flags");
        assert_eq!(limits.max_memory_share, Some(0.25));
        assert_eq!(limits.max_memory_growth_bytes, None);
        assert_eq!(limits.max_requests, Some(3));
        assert_eq!(limits.max_handles, None);
        assert_eq!(limits.idle_timeout, Some(Duration::from_millis(50)));
        assert_eq!(SoftLimits::from_args(args("")), Ok(SoftLimits::default()));
        let options =
            ServeOptions::from_args(args("--max-requests 3 --max-running 4")).expect("flags");
        assert_eq!(options.max_running, 4);
        assert_eq!(options.limits.max_requests, Some(3));
        assert_eq!(
            ServeOptions::from_args(args("")).expect("none").max_running,
            DEFAULT_MAX_RUNNING
        );
        assert!(ServeOptions::from_args(args("--max-running 0")).is_err());
        for refused in [
            "--max-memory-share 2",
            "--max-requests -1",
            "--max-requests",
            "--idle-timeout soon",
            "--restart-often 1",
        ] {
            assert!(SoftLimits::from_args(args(refused)).is_err(), "{refused}");
        }
    }

    #[test]
    fn each_soft_limit_drains_the_host() {
        const MIB: u64 = 1 << 20;
        let usage = |footprint: u64, handles: u64, degraded: bool| Usage {
            footprint: Some(footprint * MIB),
            capacity: Some(1000 * MIB),
            handles: Some(handles),
            degraded,
        };
        let limits = SoftLimits {
            max_memory_share: Some(0.5),
            max_memory_growth_bytes: Some(100 * MIB),
            max_requests: Some(2),
            max_handles: Some(50),
            idle_timeout: None,
            drain_degraded: true,
        };
        let lifecycle = Lifecycle::new(limits);
        lifecycle.begin_request();
        lifecycle.end_request();
        lifecycle.lock().baseline_footprint = Some(200 * MIB);
        assert_eq!(lifecycle.over_limit(&usage(300, 50, false)), None);
        assert_eq!(
            lifecycle.over_limit(&usage(501, 0, false)),
            Some(DrainReason::Memory)
        );
        assert_eq!(
            lifecycle.over_limit(&usage(301, 0, false)),
            Some(DrainReason::MemoryGrowth)
        );
        assert_eq!(
            lifecycle.over_limit(&usage(300, 51, false)),
            Some(DrainReason::Handles)
        );
        assert_eq!(
            lifecycle.over_limit(&usage(300, 0, true)),
            Some(DrainReason::Degraded)
        );
        lifecycle.begin_request();
        lifecycle.end_request();
        assert_eq!(
            lifecycle.over_limit(&usage(300, 0, false)),
            Some(DrainReason::Requests)
        );
        let off = Lifecycle::new(SoftLimits::OFF);
        assert_eq!(off.over_limit(&usage(999, 1 << 20, false)), None);
        assert_eq!(off.over_limit(&usage(999, 0, true)), None);
    }

    #[test]
    fn the_first_answer_sets_the_memory_baseline() {
        let lifecycle = Lifecycle::new(SoftLimits::OFF);
        assert_eq!(
            lifecycle.status()["details"]["memory"]["baselineBytes"],
            Value::Null
        );
        lifecycle.begin_request();
        assert_eq!(lifecycle.status()["details"]["activity"], "busy");
        lifecycle.end_request();
        let details = &lifecycle.status()["details"];
        assert_eq!(details["activity"], "idle");
        assert_eq!(details["requestsServed"], 1);
        assert!(
            details["memory"]["baselineBytes"].as_u64().is_some(),
            "{details}"
        );
    }

    #[test]
    fn a_draining_host_keeps_its_first_reason_and_reports_it() {
        let captured = crate::serve_io::Captured::default();
        let out = captured.responder();
        let lifecycle = Lifecycle::new(SoftLimits::default());
        assert_eq!(lifecycle.state(), HostState::Ready);
        assert_eq!(lifecycle.status()["reason"], "started");
        lifecycle.drain(DrainReason::Idle, &out).expect("drain");
        lifecycle.drain(DrainReason::Requests, &out).expect("drain");
        assert_eq!(lifecycle.state(), HostState::Draining);
        let messages = crate::serve_io::messages_of(&captured.bytes());
        assert_eq!(messages.len(), 1);
        let message = &messages[0].0;
        assert_eq!(message["type"], "state");
        assert_eq!(message["state"], "draining");
        assert_eq!(message["reason"], "idle");
        assert_eq!(message["details"]["activity"], "idle");
        assert_eq!(
            message["details"]["limits"]["maxRequests"],
            DEFAULT_MAX_REQUESTS
        );
    }
}

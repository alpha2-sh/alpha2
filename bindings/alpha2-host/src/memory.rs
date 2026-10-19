//! The byte budgets an export runs within.
//!
//! One policy, resolved once per export: what the request's `memory` asks
//! for, else the environment's `ALPHA2_*_BYTES` setting, else the default.
//! The environment settings are this policy's defaults, not a second set of
//! limits beside it. Each budget bounds memory this host allocates and
//! reserves before allocating; none bounds the process as a whole, and
//! decoder, encoder, and driver memory is outside all of them.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    DEFAULT_AUDIO_WINDOW_BYTES, DEFAULT_DECODE_QUEUE_BYTES, DEFAULT_EXPORT_IN_FLIGHT_BYTES,
    DEFAULT_PROVIDER_SOURCE_BYTES, DEFAULT_REVERSE_CACHE_BYTES,
};

/// The request's `memory`: any budget it names replaces the environment's.
#[allow(clippy::struct_field_names)] // Named as the protocol names them.
#[derive(Copy, Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryRequest {
    /// Rendered frames between the GPU and the encoder: readback staging and
    /// packed encoder frames, or on the GPU-resident path the plane ring and
    /// encoder surfaces.
    pub in_flight_bytes: Option<u64>,
    /// Decoded source frames waiting to be rendered.
    pub decode_queue_bytes: Option<u64>,
    /// Frames a reversed clip keeps decoded.
    pub reverse_cache_bytes: Option<u64>,
    /// Decoded audio kept around the mix window.
    pub audio_window_bytes: Option<u64>,
    /// What the open extension-provider sources (SVG, animated images,
    /// Lottie, OpenEXR, image sequences) hold together: their kept frames
    /// and images.
    pub provider_source_bytes: Option<u64>,
    /// A fraction, above 0 and at most 1, that every budget above is scaled
    /// by after it is resolved: how a retry asks for less in flight without
    /// knowing the host's defaults.
    pub scale: Option<f64>,
}

/// Where a budget's value came from.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BudgetSource {
    Request,
    Environment,
    Default,
}

impl BudgetSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Environment => "env",
            Self::Default => "default",
        }
    }
}

/// One resolved budget and where its value came from.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Budget {
    pub bytes: u64,
    pub source: BudgetSource,
    /// What `bytes` was before `memory.scale` shrank it: the most a working
    /// set that needs more than `bytes` may still have ([`ByteBudget`]).
    pub unscaled: u64,
}

/// A byte budget as the part of the pipeline it bounds holds it: what it may
/// use, and the most `memory.scale` may be undone for one working set.
///
/// A retry's scale asks for less in flight, not for an export that cannot
/// run: the least one frame or block needs is held whatever the scale, up to
/// the budget before it, as the browser exporter keeps a retry's in-flight
/// budget at its least. Only a budget that cannot hold that least
/// before any scale refuses the export (`RESOURCE_LIMIT`).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ByteBudget {
    /// The budget after any `memory.scale`.
    pub bytes: u64,
    /// The budget before the scale: the most a working set may be given.
    pub ceiling: u64,
}

impl ByteBudget {
    /// A budget no scale shrank: `bytes` is also its ceiling.
    pub const fn exact(bytes: u64) -> Self {
        Self {
            bytes,
            ceiling: bytes,
        }
    }

    /// What a working set of `least` bytes may use: the budget, or, where a
    /// scale shrank it below `least`, `least` up to the unscaled budget.
    pub fn holding(self, least: u64) -> u64 {
        self.bytes.max(least.min(self.ceiling))
    }
}

impl From<u64> for ByteBudget {
    fn from(bytes: u64) -> Self {
        Self::exact(bytes)
    }
}

impl Budget {
    fn resolve(requested: Option<u64>, variable: &str, default: u64) -> Self {
        Self::resolve_from(requested, variable, std::env::var_os(variable), default)
    }

    /// The budget `requested` asks for, else `variable`'s `setting`, else
    /// `default`.
    ///
    /// A setting that is not a positive byte count falls back to the default
    /// and is reported as the default, with a line on stderr naming what was
    /// ignored, so the report never claims a setting applied when it did not.
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

    /// The budget as the pipeline holds it ([`ByteBudget`]).
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

/// The budgets one export runs within.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct MemoryPolicy {
    pub in_flight: Budget,
    pub decode_queue: Budget,
    pub reverse_cache: Budget,
    pub audio_window: Budget,
    pub provider_sources: Budget,
    /// What the resolved budgets were scaled by.
    pub scale: f64,
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        Self::resolve(MemoryRequest::default()).expect("the defaults are valid")
    }
}

impl MemoryPolicy {
    /// The policy `request` asks for, over the environment and defaults.
    ///
    /// # Errors
    /// Returns a message naming the field when a requested budget is zero.
    pub fn resolve(request: MemoryRequest) -> Result<Self, String> {
        for (field, value) in [
            ("inFlightBytes", request.in_flight_bytes),
            ("decodeQueueBytes", request.decode_queue_bytes),
            ("reverseCacheBytes", request.reverse_cache_bytes),
            ("audioWindowBytes", request.audio_window_bytes),
            ("providerSourceBytes", request.provider_source_bytes),
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
            scale,
        };
        Ok(policy.scaled())
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )] // A byte count times a fraction in (0, 1], floored, stays within u64.
    fn scaled(mut self) -> Self {
        if self.scale < 1.0 {
            for budget in [
                &mut self.in_flight,
                &mut self.decode_queue,
                &mut self.reverse_cache,
                &mut self.audio_window,
                &mut self.provider_sources,
            ] {
                budget.bytes = ((budget.bytes as f64 * self.scale).floor() as u64).max(1);
            }
        }
        self
    }

    /// The budgets, and where each came from.
    pub fn to_json(self) -> Value {
        json!({
            "inFlightBytes": self.in_flight.to_json(),
            "decodeQueueBytes": self.decode_queue.to_json(),
            "reverseCacheBytes": self.reverse_cache.to_json(),
            "audioWindowBytes": self.audio_window.to_json(),
            "providerSourceBytes": self.provider_sources.to_json(),
            "scale": self.scale,
            "scope": "memory this host reserves before allocating; decoder, encoder, and driver memory are outside every budget",
        })
    }
}

/// The in-flight budget a pipeline is sized from, with where it came from,
/// for a refusal to name.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct InFlightBudget {
    pub bytes: u64,
    pub source: BudgetSource,
    /// What the resolved budget was scaled by (`memory.scale`).
    pub scale: f64,
    /// The budget before that scale ([`ByteBudget::ceiling`]).
    pub unscaled: u64,
}

impl InFlightBudget {
    /// `bytes`, as a request asked for them.
    #[cfg(test)]
    pub const fn requested(bytes: u64) -> Self {
        Self {
            bytes,
            source: BudgetSource::Request,
            scale: 1.0,
            unscaled: bytes,
        }
    }

    /// What a pipeline whose least is `least` bytes may use
    /// ([`ByteBudget::holding`]).
    pub fn holding(self, least: u64) -> u64 {
        ByteBudget {
            bytes: self.bytes,
            ceiling: self.unscaled,
        }
        .holding(least)
    }

    /// The budget in a refusal's words: its bytes, the setting they came
    /// from (`memory.inFlightBytes`, `ALPHA2_EXPORT_IN_FLIGHT_BYTES`, or the
    /// default), and any scale asked for after, so an operator is pointed at
    /// the setting that applied. A pipeline holds its least whatever the
    /// scale ([`Self::holding`]), so what refuses it is the budget before the
    /// scale.
    pub fn describe(self) -> String {
        let origin = match self.source {
            BudgetSource::Request => "memory.inFlightBytes",
            BudgetSource::Environment => "ALPHA2_EXPORT_IN_FLIGHT_BYTES",
            BudgetSource::Default => "the default",
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
    /// The in-flight budget, with where it came from.
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
    /// Marks a refusal to start or continue because a budget cannot hold the
    /// least the export needs. Stripped like
    /// [`crate::protocol::gpu_timeout_tag`].
    pub fn resource_limit_tag = "RESOURCE_LIMIT] ";
}

/// `message`, marked as a budget refusal.
pub fn resource_limit(message: impl std::fmt::Display) -> String {
    format!("{}{message}", resource_limit_tag())
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
        let policy = MemoryPolicy::resolve(MemoryRequest {
            in_flight_bytes: Some(100),
            scale: Some(0.5),
            ..MemoryRequest::default()
        })
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

    /// A setting that is not a positive byte count runs the default, and is
    /// reported as the default, not as the setting that was ignored.
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
        // A requested budget wins over any setting.
        assert_eq!(
            Budget::resolve_from(Some(7), "X", Some("1024".into()), 256).source,
            BudgetSource::Request
        );
    }

    /// A refusal names the budget by what set it and the scale after.
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

    #[test]
    fn unknown_fields_are_refused() {
        assert!(serde_json::from_value::<MemoryRequest>(json!({ "gpuBytes": 1 })).is_err());
    }
}

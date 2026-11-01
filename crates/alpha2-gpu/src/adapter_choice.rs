//! Which adapter a host renders on when a machine has several. The same rule applies on every
//! OS and is decided once at host start.
//!
//! Under `auto` the adapters that render are ranked by:
//! 1. how many of H.264, HEVC and AV1 the adapter encodes on the GPU,
//! 2. whether it decodes on the GPU, when the decode policy allows it,
//! 3. the high-performance order of the records.
//!
//! Capability leads because neither GPU of a hybrid machine is faster for every project: the
//! discrete GPU renders heavy compositions faster, while an integrated GPU with a video encoder
//! exports video-led projects faster at a fraction of the host CPU. `performance` takes the first
//! hardware adapter in high-performance order. A number or name pins one adapter, and a pin that
//! matches nothing is refused. Software adapters come last under every policy except a pin.
//!
//! The choice is a pure function over per-adapter records, so every policy is testable without a
//! device.

use crate::AdapterPolicy;

/// The codecs a GPU encoder is asked about, in the order a report lists them.
pub const GPU_ENCODE_CODECS: [&str; 3] = ["h264", "hevc", "av1"];

/// What one adapter was found to do. Checks that were not made are `Err`, so the choice never
/// assumes a capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterRecord {
    /// Its position in the backend's enumeration, which an index pin names.
    pub index: usize,
    /// The adapter's name, which a name pin matches.
    pub name: String,
    /// `dx12`, `vulkan` or `metal`.
    pub backend: &'static str,
    /// The DXGI adapter LUID, where the backend has one.
    pub luid: Option<u64>,
    /// WARP, lavapipe, llvmpipe or another adapter the backend flags as software.
    pub software: bool,
    /// Whether a device opens on it, or why not.
    pub renders: Result<(), String>,
    /// Whether hardware decode imports into its device, or why not.
    pub decodes: Result<(), String>,
    /// Per codec asked about, whether the device encodes it on the GPU, or why not.
    pub gpu_encodes: Vec<(&'static str, Result<(), String>)>,
}

impl AdapterRecord {
    /// A record of an adapter nothing has been asked of yet.
    #[must_use]
    pub fn unasked(index: usize, name: String, backend: &'static str, software: bool) -> Self {
        Self {
            index,
            name,
            backend,
            luid: None,
            software,
            renders: Err("not opened".to_owned()),
            decodes: Err("not asked".to_owned()),
            gpu_encodes: Vec::new(),
        }
    }

    /// Whether the device encodes `codec` on the GPU.
    #[must_use]
    pub fn encodes(&self, codec: &str) -> bool {
        self.gpu_encodes
            .iter()
            .any(|(asked, answer)| *asked == codec && answer.is_ok())
    }

    /// The codecs the device encodes on the GPU.
    #[must_use]
    pub fn encoded_codecs(&self) -> Vec<&'static str> {
        self.gpu_encodes
            .iter()
            .filter(|(_, answer)| answer.is_ok())
            .map(|(codec, _)| *codec)
            .collect()
    }

    /// Why the device encodes no codec on the GPU: the first refusal, or that none was asked.
    #[must_use]
    pub fn gpu_encode_reason(&self) -> Option<String> {
        if !self.encoded_codecs().is_empty() {
            return None;
        }
        Some(
            match self
                .gpu_encodes
                .iter()
                .find_map(|(_, answer)| answer.as_ref().err())
            {
                Some(reason) => reason.clone(),
                None => "not asked".to_owned(),
            },
        )
    }

    /// `[index] name`, with a note for a software adapter.
    #[must_use]
    pub fn label(&self) -> String {
        format!(
            "[{}] {}{}",
            self.index,
            self.name,
            if self.software {
                " (a software adapter)"
            } else {
                ""
            }
        )
    }

    /// One line describing this adapter, for the start diagnostic of a passed-over adapter.
    #[must_use]
    pub fn summary(&self) -> String {
        let label = self.label();
        if let Err(reason) = &self.renders {
            return format!("{label}: does not render ({reason})");
        }
        let decodes = match &self.decodes {
            Ok(()) => "yes".to_owned(),
            Err(reason) => format!("no ({reason})"),
        };
        let encodes = match self.gpu_encode_reason() {
            None => self.encoded_codecs().join(","),
            Some(reason) => format!("none ({reason})"),
        };
        format!("{label}: renders, decodes: {decodes}, encodes on the GPU: {encodes}")
    }

    fn renders_in_hardware(&self) -> bool {
        self.renders.is_ok() && !self.software
    }
}

/// Why the chosen adapter was taken.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChoiceReason {
    /// `ALPHA2_GPU_ADAPTER` names it.
    Pinned,
    /// It is the only hardware adapter that renders, or the only adapter at all.
    OnlyAdapter,
    /// It encodes more codecs on the GPU than the next one.
    MostGpuEncoders,
    /// It encodes as many codecs as the next one, and decodes on the GPU where that one does not.
    Decodes,
    /// Nothing else tells it from the next one: it comes first in the high-performance order.
    HighPerformanceOrder,
    /// `ALPHA2_GPU_ADAPTER=performance`: the first hardware adapter in the high-performance order.
    PerformancePolicy,
    /// No adapter could share a device with the video codecs, so decode and encode run without
    /// the GPU.
    NoSharedDevice,
}

impl ChoiceReason {
    /// The reason as a capability report names it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pinned => "pinned",
            Self::OnlyAdapter => "only-adapter",
            Self::MostGpuEncoders => "most-gpu-encoders",
            Self::Decodes => "decodes",
            Self::HighPerformanceOrder => "high-performance-order",
            Self::PerformancePolicy => "performance-policy",
            Self::NoSharedDevice => "no-shared-device",
        }
    }
}

/// The adapters a policy takes, best first, and why the first one leads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdapterOrder {
    /// Positions in the records: every adapter that renders and that the policy accepts.
    pub order: Vec<usize>,
    /// Why the first leads.
    pub reason: ChoiceReason,
}

/// The adapters `policy` accepts among `records` (in high-performance order), best first.
/// `decode_allowed` says whether the decode policy allows GPU decoding. Software adapters always
/// rank after hardware ones.
///
/// # Errors
/// Returns why no adapter can be taken: none renders, or the pin matches no rendering adapter.
pub fn rank_adapters(
    records: &[AdapterRecord],
    policy: &AdapterPolicy,
    decode_allowed: bool,
) -> Result<AdapterOrder, String> {
    // Hardware adapters in the order given, then software ones.
    let considered: Vec<usize> = (0..records.len())
        .filter(|&at| !records[at].software)
        .chain((0..records.len()).filter(|&at| records[at].software))
        .collect();
    if policy.is_pin() {
        let named: Vec<usize> = considered
            .iter()
            .copied()
            .filter(|&at| policy.names(records[at].index, &records[at].name))
            .collect();
        if named.is_empty() {
            return Err(format!(
                "{} matches no adapter; the adapters are {}",
                policy.setting(),
                adapter_list(records)
            ));
        }
        let order: Vec<usize> = named
            .iter()
            .copied()
            .filter(|&at| records[at].renders.is_ok())
            .collect();
        if order.is_empty() {
            return Err(format!(
                "{} names {}, which does not render",
                policy.setting(),
                named
                    .iter()
                    .map(|&at| records[at].summary())
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        return Ok(AdapterOrder {
            order,
            reason: ChoiceReason::Pinned,
        });
    }
    let mut order: Vec<usize> = considered
        .into_iter()
        .filter(|&at| records[at].renders.is_ok())
        .collect();
    if order.is_empty() {
        return Err(format!(
            "no adapter renders: {}",
            records
                .iter()
                .map(AdapterRecord::summary)
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    if *policy == AdapterPolicy::Performance {
        return Ok(AdapterOrder {
            order,
            reason: ChoiceReason::PerformancePolicy,
        });
    }
    let decodes = |at: usize| decode_allowed && records[at].decodes.is_ok();
    // Stable, so ties keep the high-performance order.
    order.sort_by_key(|&at| {
        let record = &records[at];
        let hardware = !record.software;
        (
            !hardware,
            std::cmp::Reverse(if hardware {
                record.encoded_codecs().len()
            } else {
                0
            }),
            !(hardware && decodes(at)),
        )
    });
    let reason = match order.get(1) {
        // A software adapter is no alternative to a hardware one.
        None => ChoiceReason::OnlyAdapter,
        Some(&next) if !records[order[0]].software && records[next].software => {
            ChoiceReason::OnlyAdapter
        }
        Some(&next) => {
            let (first, second) = (&records[order[0]], &records[next]);
            let encoders = |record: &AdapterRecord| {
                if record.software {
                    0
                } else {
                    record.encoded_codecs().len()
                }
            };
            if encoders(first) > encoders(second) {
                ChoiceReason::MostGpuEncoders
            } else if first.renders_in_hardware()
                && decodes(order[0])
                && !(second.renders_in_hardware() && decodes(next))
            {
                ChoiceReason::Decodes
            } else {
                ChoiceReason::HighPerformanceOrder
            }
        }
    };
    Ok(AdapterOrder { order, reason })
}

/// One sentence explaining why `chosen` was taken, naming the runner-up `next` where relevant.
#[must_use]
pub fn explain_choice(
    records: &[AdapterRecord],
    chosen: usize,
    next: Option<usize>,
    reason: ChoiceReason,
    policy: &AdapterPolicy,
) -> String {
    let Some(record) = records.get(chosen) else {
        return "no adapter was chosen".to_owned();
    };
    let encodes = |record: &AdapterRecord| match record.gpu_encode_reason() {
        None => format!("encodes {} on the GPU", record.encoded_codecs().join(",")),
        Some(why) => format!("encodes nothing on the GPU ({why})"),
    };
    let next = next.and_then(|at| records.get(at));
    match (reason, next) {
        (ChoiceReason::Pinned, _) => format!("{} names {}", policy.setting(), record.name),
        (ChoiceReason::OnlyAdapter, _) => format!(
            "{} is the only {}adapter that renders",
            record.name,
            if record.software { "" } else { "hardware " }
        ),
        (ChoiceReason::PerformancePolicy, _) if record.software => format!(
            "{} renders in software: no hardware adapter renders here",
            record.name
        ),
        (ChoiceReason::PerformancePolicy, _) => format!(
            "{} is the first hardware adapter in the high-performance order",
            record.name
        ),
        (ChoiceReason::NoSharedDevice, _) => format!(
            "{} renders without a device shared with the video decoder and encoder",
            record.name
        ),
        (ChoiceReason::MostGpuEncoders, Some(next)) => format!(
            "{} {}, more codecs than {}, which {}",
            record.name,
            encodes(record),
            next.name,
            encodes(next)
        ),
        (ChoiceReason::Decodes, Some(next)) => format!(
            "{} decodes on the GPU and {} does not ({}); both encode as many codecs on the GPU",
            record.name,
            next.name,
            next.decodes
                .as_ref()
                .err()
                .map_or("decode is in software", String::as_str)
        ),
        (ChoiceReason::HighPerformanceOrder, Some(next)) => format!(
            "{} comes before {} in the high-performance order, and does no less on the GPU",
            record.name, next.name
        ),
        (_, None) => format!("{} is the only adapter that renders", record.name),
    }
}

/// `[0] name, [1] name, ...`.
fn adapter_list(records: &[AdapterRecord]) -> String {
    if records.is_empty() {
        return "none".to_owned();
    }
    records
        .iter()
        .map(|record| format!("[{}] {}", record.index, record.name))
        .collect::<Vec<_>>()
        .join(", ")
}

/// For each codec `chosen` does not encode on the GPU, another hardware adapter that does and
/// the `ALPHA2_GPU_ADAPTER` setting that selects it.
#[must_use]
pub fn encode_elsewhere(records: &[AdapterRecord], chosen: usize) -> Vec<(&'static str, String)> {
    let Some(current) = records.get(chosen) else {
        return Vec::new();
    };
    GPU_ENCODE_CODECS
        .iter()
        .filter(|codec| !current.encodes(codec))
        .filter_map(|&codec| {
            let other = records
                .iter()
                .enumerate()
                .find(|(at, record)| {
                    *at != chosen && record.renders_in_hardware() && record.encodes(codec)
                })
                .map(|(_, record)| record)?;
            Some((
                codec,
                format!(
                    "{} encodes {codec} on the GPU ({} renders there)",
                    other.name,
                    AdapterPolicy::Index(other.index).setting()
                ),
            ))
        })
        .collect()
}

/// Why `chosen` cannot decode, from the records of every hardware adapter considered.
#[must_use]
pub fn decode_refusals(records: &[AdapterRecord], chosen: usize, pinned: bool) -> String {
    let considered: Vec<&AdapterRecord> = if pinned {
        records.get(chosen).into_iter().collect()
    } else {
        records
            .iter()
            .filter(|record| record.renders_in_hardware())
            .collect()
    };
    considered
        .into_iter()
        .filter_map(|record| {
            record
                .decodes
                .as_ref()
                .err()
                .map(|reason| format!("{}: {reason}", record.name))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// What a host's adapter choice was, as its capabilities and failure reports state it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterChoiceReport {
    /// The `ALPHA2_GPU_ADAPTER` policy the choice was made under.
    pub policy: AdapterPolicy,
    /// The renderer's adapter in `records`; `None` for a device from elsewhere.
    pub chosen: Option<usize>,
    /// Why the renderer is on that adapter.
    pub reason: ChoiceReason,
    /// One sentence naming the chosen adapter, and the runner-up where the reason compares them.
    pub explanation: String,
    /// Every adapter the choice considered, in the high-performance order.
    pub records: Vec<AdapterRecord>,
}

impl AdapterChoiceReport {
    /// The report for `records` ranked into `order` under `policy`, taking the first.
    #[must_use]
    pub fn ranked(
        policy: AdapterPolicy,
        records: Vec<AdapterRecord>,
        order: &AdapterOrder,
    ) -> Self {
        let chosen = order.order.first().copied();
        let explanation = chosen.map_or_else(
            || "no adapter was chosen".to_owned(),
            |chosen| {
                explain_choice(
                    &records,
                    chosen,
                    order.order.get(1).copied(),
                    order.reason,
                    &policy,
                )
            },
        );
        Self {
            policy,
            chosen,
            reason: order.reason,
            explanation,
            records,
        }
    }

    /// The record of the adapter the renderer is on.
    #[must_use]
    pub fn chosen_record(&self) -> Option<&AdapterRecord> {
        self.chosen.and_then(|at| self.records.get(at))
    }
}

#[cfg(test)]
#[path = "adapter_choice_tests.rs"]
mod tests;

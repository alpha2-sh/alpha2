//! Which adapter a host renders on when a machine has several, from what each one was found to
//! do. One rule on every OS, decided once when the host starts, before any request names a codec.
//!
//! Under `auto` the adapters that render are ranked by:
//! 1. how many codecs the adapter encodes on the GPU (the device's own answer for H.264, HEVC
//!    and AV1),
//! 2. whether it decodes on the GPU, when the decode policy allows decoding there,
//! 3. the high-performance order the records come in.
//!
//! Capability leads because speed picks no winner: on a hybrid laptop the discrete GPU renders a
//! heavy project faster on the readback path, while the integrated one, which encodes on the GPU,
//! exports a video-led one faster, at a third of the host CPU or less, and is the only one that
//! exports HEVC at all. Measured on a hybrid laptop with an Intel integrated GPU and an NVIDIA
//! discrete GPU without a video encoder (DX12, release host, software decode, H.264 1080p30,
//! three alternating rounds): the integrated GPU took about 1.4 times as long on a heavy
//! composition and was faster on a full-frame video with an overlay, at a third of the discrete
//! GPU's host CPU or less on both. `performance` takes the first hardware adapter in the
//! high-performance order whatever it encodes; a number or a name pins one adapter, and a pin that
//! matches nothing refuses rather than rendering somewhere the caller did not ask for. Software
//! adapters come last under every policy but a pin.
//!
//! The choice is a pure function over one record per adapter, so every policy and machine class
//! is tested on any platform, without a device. The backends fill the records: Windows from each
//! DX12 adapter's D3D12 video answers, Linux from each Vulkan adapter's video queues, macOS from
//! its one Metal device.

use crate::AdapterPolicy;

/// The codecs a GPU encoder is asked about, in the order a report lists them.
pub const GPU_ENCODE_CODECS: [&str; 3] = ["h264", "hevc", "av1"];

/// What one adapter was found to do. A check that was not made is an `Err` that says so, because
/// the choice must not take an adapter for something nobody asked it.
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
    /// For each codec asked about (`h264`, `hevc`, `av1`), whether the device encodes it on the
    /// GPU, or why not. A codec left out was not asked.
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

    /// Why the device encodes no codec on the GPU, when it encodes none: the first refusal, or
    /// that nobody asked.
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

    /// One line saying what this adapter does, for the start diagnostic of an adapter the
    /// renderer passed over.
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
    /// No adapter could share a device with the video decoder and encoder: the renderer took the
    /// product device, decoding and encoding without the GPU.
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

/// The adapters `policy` takes among `records`, best first: the host renders on the first.
/// `decode_allowed` says whether the decode policy allows decoding on the GPU.
///
/// `records` are in the high-performance order; a software adapter comes after every hardware
/// one whatever its place.
///
/// # Errors
/// Says why no adapter can be taken: none renders, or a pin names none, or names only adapters
/// that do not render.
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
    // A stable sort: adapters that tie keep the high-performance order. Software adapters
    // already come last and neither encode nor decode for the ranking.
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

/// One sentence saying why the adapter at `chosen` renders under `reason`, naming the runner-up
/// (`next`, the second in the policy's order) where the reason compares the two.
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

/// `[0] NVIDIA ..., [1] Intel ...`.
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

/// For each codec the adapter at `chosen` does not encode on the GPU, another hardware adapter
/// that does and the setting that renders there: what an export's reason adds, so a host pinned
/// to one adapter says how to reach the one that encodes.
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

/// Why the adapter at `chosen` cannot decode, from every hardware adapter the choice considered
/// for it (the pinned one alone under a pin), as each one's record says.
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
    /// The position in `records` of the adapter the renderer is on; `None` for a device none of
    /// them gave.
    pub chosen: Option<usize>,
    /// Why the renderer is on that adapter.
    pub reason: ChoiceReason,
    /// One sentence naming the chosen adapter, and the runner-up where the reason compares them.
    pub explanation: String,
    /// Every adapter the choice considered, in the high-performance order.
    pub records: Vec<AdapterRecord>,
}

impl AdapterChoiceReport {
    /// The report of `records` ranked into `order` under `policy`, the first in the order taken.
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

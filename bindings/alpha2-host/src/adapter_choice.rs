//! Which GPU adapter the renderer takes when a machine has several, from
//! what each one was found to do.
//!
//! Only Windows asks: there the renderer's adapter is also the one D3D12VA
//! decodes on and D3D12 Video Encode encodes on, and a hybrid laptop's
//! discrete GPU may have no encoder while its integrated one does. The
//! choice is a pure function over one record per adapter, so every policy
//! and every machine class is tested on any platform, without a device.
//!
//! `ALPHA2_GPU_ADAPTER` picks the policy:
//! - `default` (or unset): the first adapter, in `wgpu`'s high-performance
//!   order, that the decode policy accepts, whatever it encodes.
//! - `encoder`: the first that encodes the host's default codec, one that
//!   also decodes first; with none, as `default`.
//! - a number, or any other text: the adapter at that enumeration index, or
//!   the first whose name contains the text, ignoring case. A pin that
//!   matches nothing refuses to start, rather than rendering somewhere the
//!   caller did not ask for.
//!
//! Software adapters (WARP) come last under every policy but a pin, and
//! are never taken for decode or encode: they have neither.
#![cfg_attr(
    not(all(target_os = "windows", feature = "hwencode")),
    allow(dead_code)
)]

use alpha2_native::NativeVideoCodec;

/// The codecs D3D12 Video Encode is asked about, in the order a report leads
/// with them.
pub const ENCODE_CODECS: [NativeVideoCodec; 3] = [
    NativeVideoCodec::H264,
    NativeVideoCodec::Hevc,
    NativeVideoCodec::Av1,
];

/// The codec an export encodes when its request names none, which
/// `encoder` looks for.
pub const DEFAULT_ENCODE_CODEC: NativeVideoCodec = NativeVideoCodec::H264;

/// How the renderer's adapter is chosen (`ALPHA2_GPU_ADAPTER`), parsed in
/// alpha2-gpu so that tests and the host read one setting.
pub use alpha2_gpu::AdapterPin;

/// What tells one DXGI row's GPU from another's: its vendor and device ids,
/// and its PCI address when the driver reports one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RowIdentity {
    /// Its position in the backend's enumeration.
    pub index: usize,
    pub vendor_device: (u32, u32),
    pub pci_address: Option<(u32, u32, u32)>,
    pub software: bool,
}

/// The rows that are another view of a GPU an earlier-found row already is,
/// each with the enumeration index of that row.
///
/// A hybrid laptop's driver lists its integrated GPU twice, with two LUIDs:
/// one row with the GPU's PCI address and one with none. A hardware row with
/// no address whose vendor and device ids are those of a row that has one is
/// that GPU again. Two identical GPUs each report an address of their own, so
/// they are never taken for one.
#[must_use]
pub fn duplicate_rows(rows: &[RowIdentity]) -> Vec<(usize, usize)> {
    rows.iter()
        .filter(|row| !row.software && row.pci_address.is_none())
        .filter_map(|row| {
            rows.iter()
                .find(|other| {
                    !other.software
                        && other.pci_address.is_some()
                        && other.vendor_device == row.vendor_device
                })
                .map(|other| (row.index, other.index))
        })
        .collect()
}

/// What one adapter was found to do. A check that was not made is an
/// `Err` that says so, because the choice must not take an adapter for
/// something nobody asked it.
#[derive(Clone, Debug)]
pub struct AdapterRecord {
    /// Its position in the backend's enumeration, which a pin names.
    pub index: usize,
    pub name: String,
    /// WARP, or another adapter DXGI flags as software.
    pub software: bool,
    /// Whether a device opened on it, or why not.
    pub renders: Result<(), String>,
    /// Whether D3D12VA decodes into its device, or why not.
    pub decodes: Result<(), String>,
    /// For each codec asked about, whether the device reports a D3D12
    /// video encoder for it, or why not. A codec left out was not asked.
    pub encodes: Vec<(NativeVideoCodec, Result<(), String>)>,
}

impl AdapterRecord {
    /// Whether the device reports an encoder for `codec`.
    pub fn encodes(&self, codec: NativeVideoCodec) -> bool {
        self.encodes
            .iter()
            .any(|(asked, answer)| *asked == codec && answer.is_ok())
    }

    /// The codecs the device reports an encoder for.
    pub fn encoded_codecs(&self) -> Vec<NativeVideoCodec> {
        self.encodes
            .iter()
            .filter(|(_, answer)| answer.is_ok())
            .map(|(codec, _)| *codec)
            .collect()
    }

    /// One line saying what this adapter does, for the start diagnostic of
    /// an adapter the renderer passed over.
    pub fn summary(&self) -> String {
        let label = format!(
            "[{}] {}{}",
            self.index,
            self.name,
            if self.software {
                " (a software adapter)"
            } else {
                ""
            }
        );
        if let Err(reason) = &self.renders {
            return format!("{label}: does not render ({reason})");
        }
        let decodes = match &self.decodes {
            Ok(()) => "yes".to_owned(),
            Err(reason) => format!("no ({reason})"),
        };
        let reported = self.encoded_codecs();
        let encodes = if !reported.is_empty() {
            codec_list(&reported)
        } else if let Some((_, Err(reason))) = self.encodes.first() {
            format!("none ({reason})")
        } else {
            "not asked".to_owned()
        };
        format!("{label}: renders, decodes: {decodes}, reports encoders: {encodes}")
    }
}

/// `codecs` as a report lists them: `h264,hevc`.
pub fn codec_list(codecs: &[NativeVideoCodec]) -> String {
    codecs
        .iter()
        .map(|codec| codec.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// The positions of `records` in the order every policy considers them:
/// hardware adapters in the order given, then software ones.
fn ranked(records: &[AdapterRecord]) -> impl Iterator<Item = usize> + '_ {
    let hardware = (0..records.len()).filter(|&at| !records[at].software);
    let software = (0..records.len()).filter(|&at| records[at].software);
    hardware.chain(software)
}

/// The position in `records` of the adapter the renderer takes under
/// `policy`, when `decode` says the decode policy allows decoding on the
/// GPU and `codec` is the one `encoder` looks for.
///
/// `records` are in `wgpu`'s high-performance order; software adapters are
/// put last whatever their place.
///
/// # Errors
/// Says why no adapter can be taken: none renders, or a pin names none,
/// or names only adapters that do not render.
pub fn choose_adapter(
    records: &[AdapterRecord],
    policy: &AdapterPin,
    decode: bool,
    codec: NativeVideoCodec,
) -> Result<usize, String> {
    let rendering = || ranked(records).filter(|&at| records[at].renders.is_ok());
    let hardware = |at: &usize| !records[*at].software;
    let decodes = |at: &usize| records[*at].decodes.is_ok();
    let by_default = || {
        decode
            .then(|| rendering().filter(hardware).find(decodes))
            .flatten()
            .or_else(|| rendering().next())
    };
    let chosen = match policy {
        AdapterPin::Default => by_default(),
        AdapterPin::Encoder => {
            let encoding = || {
                rendering()
                    .filter(hardware)
                    .filter(|&at| records[at].encodes(codec))
            };
            decode
                .then(|| encoding().find(decodes))
                .flatten()
                .or_else(|| encoding().next())
                .or_else(by_default)
        }
        AdapterPin::Index(_) | AdapterPin::Name(_) => {
            let named: Vec<usize> = ranked(records)
                .filter(|&at| policy.names(records[at].index, &records[at].name))
                .collect();
            if named.is_empty() {
                return Err(format!(
                    "{} matches no DX12 adapter; the adapters are {}",
                    policy.setting(),
                    adapter_list(records)
                ));
            }
            let chosen = named
                .iter()
                .copied()
                .find(|&at| records[at].renders.is_ok());
            if chosen.is_none() {
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
            chosen
        }
    };
    chosen.ok_or_else(|| {
        format!(
            "no DX12 adapter renders: {}",
            records
                .iter()
                .map(AdapterRecord::summary)
                .collect::<Vec<_>>()
                .join("; ")
        )
    })
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

/// For each codec the chosen adapter has no encoder for, the other adapter
/// that reports one, and the setting that renders there: what an export's
/// reason adds, so a machine whose renderer is on the wrong adapter says
/// how to reach the right one.
///
/// The setting is `ALPHA2_GPU_ADAPTER=encoder` where that policy would take
/// the adapter named, and its index otherwise.
pub fn encode_elsewhere(
    records: &[AdapterRecord],
    chosen: usize,
    decode: bool,
) -> Vec<(NativeVideoCodec, String)> {
    let Some(current) = records.get(chosen) else {
        return Vec::new();
    };
    let by_encoder = choose_adapter(records, &AdapterPin::Encoder, decode, DEFAULT_ENCODE_CODEC)
        .ok()
        .filter(|&at| at != chosen && records[at].encodes(DEFAULT_ENCODE_CODEC));
    ENCODE_CODECS
        .iter()
        .filter(|codec| !current.encodes(**codec))
        .filter_map(|&codec| {
            let other = ranked(records).find(|&at| {
                at != chosen
                    && !records[at].software
                    && records[at].renders.is_ok()
                    && records[at].encodes(codec)
            })?;
            let setting = if codec == DEFAULT_ENCODE_CODEC && by_encoder == Some(other) {
                AdapterPin::Encoder.setting()
            } else {
                AdapterPin::Index(records[other].index).setting()
            };
            Some((
                codec,
                format!(
                    "{} reports a D3D12 video encoder for {} ({setting} renders there)",
                    records[other].name,
                    codec.as_str()
                ),
            ))
        })
        .collect()
}

/// Why the renderer's adapter cannot decode, from every adapter the choice
/// considered for it, as the scan of each adapter in turn reported it.
pub fn decode_refusals(records: &[AdapterRecord], chosen: usize, pinned: bool) -> String {
    let considered: Vec<usize> = if pinned {
        vec![chosen]
    } else {
        ranked(records)
            .filter(|&at| !records[at].software && records[at].renders.is_ok())
            .collect()
    };
    considered
        .into_iter()
        .filter_map(|at| {
            records[at]
                .decodes
                .as_ref()
                .err()
                .map(|reason| format!("{}: {reason}", records[at].name))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// What the start-up choice leaves the GPU-resident export to say.
#[derive(Clone, Debug, Default)]
pub struct EncodeAdapterNotes {
    /// Why the renderer's adapter has no device shared with a hardware
    /// encoder, when it has none.
    pub no_device: Option<String>,
    /// [`encode_elsewhere`]'s answer.
    pub elsewhere: Vec<(NativeVideoCodec, String)>,
}

impl EncodeAdapterNotes {
    /// `reason`, why `codec` is not encoded on the GPU here, with the
    /// adapter that could encode it, when one could.
    pub fn explain(&self, codec: NativeVideoCodec, reason: String) -> String {
        match self.elsewhere.iter().find(|(other, _)| *other == codec) {
            Some((_, hint)) => format!("{reason}; {hint}"),
            None => reason,
        }
    }
}

#[cfg(test)]
#[path = "adapter_choice_tests.rs"]
mod tests;

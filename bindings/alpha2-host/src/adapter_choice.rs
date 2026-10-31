//! The host's side of the adapter choice ([`alpha2_gpu::rank_adapters`]): the codecs it asks a
//! device about, what tells two DXGI rows of one GPU apart, what the choice leaves an export to
//! say, and the report capabilities and failures carry.
#![cfg_attr(
    not(all(target_os = "windows", feature = "hwdevice")),
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

/// The codec an export encodes when its request names none, which a report of another adapter
/// that could encode names first.
pub const DEFAULT_ENCODE_CODEC: NativeVideoCodec = NativeVideoCodec::H264;

/// How the renderer's adapter is chosen (`ALPHA2_GPU_ADAPTER`), parsed in alpha2-gpu so that
/// tests and the host read one setting.
pub use alpha2_gpu::{AdapterChoiceReport, AdapterPolicy};

/// The codec a GPU encode answer names.
pub fn encode_codec(name: &str) -> Option<NativeVideoCodec> {
    ENCODE_CODECS
        .into_iter()
        .find(|codec| codec.as_str() == name)
}

/// What tells one DXGI row's GPU from another's: its vendor and device ids,
/// and its PCI address when the driver reports one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RowIdentity {
    /// Its position in the backend's enumeration.
    pub index: usize,
    pub vendor_device: (u32, u32),
    pub pci_address: Option<(u32, u32, u32)>,
    /// The DXGI adapter LUID, which a report names.
    pub luid: Option<u64>,
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

/// What the start-up choice leaves the GPU-resident export to say.
#[derive(Clone, Debug, Default)]
pub struct EncodeAdapterNotes {
    /// Why the renderer's adapter has no device shared with a hardware
    /// encoder, when it has none.
    pub no_device: Option<String>,
    /// For each codec the renderer's adapter does not encode on the GPU,
    /// another adapter that does ([`alpha2_gpu::encode_elsewhere`]).
    pub elsewhere: Vec<(NativeVideoCodec, String)>,
}

impl EncodeAdapterNotes {
    /// The notes of a choice whose renderer has no shared device for
    /// `no_device`'s reason, from what `report` found every adapter does.
    pub fn new(no_device: Option<String>, report: Option<&AdapterChoiceReport>) -> Self {
        let elsewhere = report
            .and_then(|report| {
                report
                    .chosen
                    .map(|chosen| alpha2_gpu::encode_elsewhere(&report.records, chosen))
            })
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(codec, hint)| Some((encode_codec(codec)?, hint)))
            .collect();
        Self {
            no_device,
            elsewhere,
        }
    }

    /// `reason`, why `codec` is not encoded on the GPU here, with the
    /// adapter that could encode it, when one could.
    pub fn explain(&self, codec: NativeVideoCodec, reason: String) -> String {
        match self.elsewhere.iter().find(|(other, _)| *other == codec) {
            Some((_, hint)) => format!("{reason}; {hint}"),
            None => reason,
        }
    }
}

/// `report` as capabilities (`adapters`) and failure diagnostics
/// (`environment.adapterChoice`) carry it.
pub fn adapter_choice_json(report: &AdapterChoiceReport) -> serde_json::Value {
    let reason = |answer: &Result<(), String>| answer.as_ref().err().cloned();
    serde_json::json!({
        "policy": report.policy.report_name(),
        "chosen": report.chosen_record().map(|record| record.index),
        "reason": report.reason.as_str(),
        "explanation": report.explanation,
        "candidates": report
            .records
            .iter()
            .map(|record| {
                serde_json::json!({
                    "index": record.index,
                    "name": record.name,
                    "backend": record.backend,
                    "luid": record.luid.map(|luid| format!("0x{luid:016x}")),
                    "software": record.software,
                    "renders": record.renders.is_ok(),
                    "rendersReason": reason(&record.renders),
                    "decodes": record.decodes.is_ok(),
                    "decodesReason": reason(&record.decodes),
                    "gpuEncodes": record.encoded_codecs(),
                    "gpuEncodeReason": record.gpu_encode_reason(),
                })
            })
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
#[path = "adapter_choice_tests.rs"]
mod tests;

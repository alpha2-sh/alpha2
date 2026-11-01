//! The host's side of the adapter choice ([`alpha2_gpu::rank_adapters`]).
#![cfg_attr(
    not(all(target_os = "windows", feature = "hwdevice")),
    allow(dead_code)
)]

use alpha2_native::NativeVideoCodec;

/// In the order a report leads with them.
pub const ENCODE_CODECS: [NativeVideoCodec; 3] = [
    NativeVideoCodec::H264,
    NativeVideoCodec::Hevc,
    NativeVideoCodec::Av1,
];

#[cfg_attr(not(feature = "hwencode"), allow(dead_code))]
pub const DEFAULT_ENCODE_CODEC: NativeVideoCodec = NativeVideoCodec::H264;

/// Parsed in alpha2-gpu so that tests and the host read one `ALPHA2_GPU_ADAPTER` setting.
pub use alpha2_gpu::{AdapterChoiceReport, AdapterPolicy};

pub fn encode_codec(name: &str) -> Option<NativeVideoCodec> {
    ENCODE_CODECS
        .into_iter()
        .find(|codec| codec.as_str() == name)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RowIdentity {
    pub index: usize,
    pub vendor_device: (u32, u32),
    pub pci_address: Option<(u32, u32, u32)>,
    pub luid: Option<u64>,
    pub software: bool,
}

/// Rows that are another view of an earlier row's GPU, each with that row's index.
///
/// A hybrid laptop's driver lists its integrated GPU twice: once with its PCI address and once
/// without. Two identical GPUs each report their own address, so they are never merged.
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

#[derive(Clone, Debug, Default)]
#[cfg_attr(not(feature = "hwencode"), allow(dead_code))]
pub struct EncodeAdapterNotes {
    pub no_device: Option<String>,
    /// Another adapter that encodes each codec the renderer's adapter does not
    /// ([`alpha2_gpu::encode_elsewhere`]).
    pub elsewhere: Vec<(NativeVideoCodec, String)>,
}

impl EncodeAdapterNotes {
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

    #[cfg_attr(not(feature = "hwencode"), allow(dead_code))]
    pub fn explain(&self, codec: NativeVideoCodec, reason: String) -> String {
        match self.elsewhere.iter().find(|(other, _)| *other == codec) {
            Some((_, hint)) => format!("{reason}; {hint}"),
            None => reason,
        }
    }
}

/// As capabilities (`adapters`) and failure diagnostics (`environment.adapterChoice`) carry it.
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

//! Host corruption policy and structured, source-attributed recovery reports.
use alpha2_media::SourceId;
use alpha2_native::{DecodeDiagnostic, DecodeDiagnosticCounts, DecodePolicy};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[cfg(test)]
thread_local! {
    /// The policy an export a test starts on this thread decodes under, in
    /// place of `ALPHA2_DECODE_POLICY`, which tests running in parallel share.
    pub static TEST_DECODE_POLICY: std::cell::Cell<Option<DecodePolicy>> =
        const { std::cell::Cell::new(None) };
}

/// How one export decodes damaged input, and what its decoders reported.
///
/// Each decoder the export opens takes the policy and counts what it reports
/// against its source, for the result's and the failure report's
/// `decodeDiagnostics`.
#[derive(Debug, Default)]
pub struct ExportDecoding {
    policy: DecodePolicy,
    sources: Mutex<BTreeMap<String, DecodeDiagnosticCounts>>,
}

impl ExportDecoding {
    /// The policy `ALPHA2_DECODE_POLICY` selects, read as an export starts.
    ///
    /// # Errors
    /// The variable is set to something other than `strict` or `tolerant`.
    pub fn from_env() -> Result<Self, String> {
        #[cfg(test)]
        if let Some(policy) = TEST_DECODE_POLICY.with(std::cell::Cell::get) {
            return Ok(Self::new(policy));
        }
        DecodePolicy::from_env().map(Self::new)
    }

    pub fn new(policy: DecodePolicy) -> Self {
        Self {
            policy,
            sources: Mutex::default(),
        }
    }

    /// The same policy, with a report of its own: a pass that decodes again
    /// what the export's own decoders report on.
    pub fn unreported(&self) -> Self {
        Self::new(self.policy)
    }

    fn record(&self, source: &str, diagnostic: &DecodeDiagnostic) {
        if let Ok(mut sources) = self.sources.lock() {
            sources
                .entry(source.to_owned())
                .or_default()
                .record(diagnostic);
        }
    }

    /// What each source's decoders reported, in source ID order: `[]` when
    /// none reported anything.
    pub fn diagnostics_json(&self) -> Value {
        let sources = self
            .sources
            .lock()
            .map(|sources| sources.clone())
            .unwrap_or_default();
        Value::Array(
            sources
                .into_iter()
                .map(|(source, counts)| {
                    json!({
                        "source": source,
                        "corruptPackets": counts.corrupt_packets,
                        "videoGaps": counts.video_gaps,
                        "concealedAudioSamples": counts.concealed_audio_samples,
                        "timestampGapSamples": counts.timestamp_gap_samples,
                        "droppedAudioSamples": counts.dropped_audio_samples,
                    })
                })
                .collect(),
        )
    }
}

// Defines `pub fn $name(decoder, source, path, decoding)`, which applies the export's decode
// policy and installs a reporter that counts each decode diagnostic against `source` and emits it
// as a JSON line tagged with `path`.
macro_rules! configure {
    ($name:ident, $ty:ty) => {
        pub fn $name(
            mut decoder: $ty,
            source: &SourceId,
            path: &Path,
            decoding: &Arc<ExportDecoding>,
        ) -> $ty {
            decoder.set_decode_policy(decoding.policy);
            let path = path.display().to_string();
            let source = source.as_str().to_owned();
            let decoding = Arc::clone(decoding);
            decoder.set_decode_reporter(move |diagnostic| {
                decoding.record(&source, &diagnostic);
                let mut report = match diagnostic {
                    DecodeDiagnostic::Corruption(packet) => serde_json::json!({
                        "type": "decodeCorruption", "corruptPackets": 1,
                        "stage": packet.stage, "pts": packet.pts,
                        "timeBase": packet.time_base
                    }),
                    DecodeDiagnostic::VideoGap(gap) => serde_json::json!({
                        "type": "decodeVideoGap", "previousPts": gap.previous_pts,
                        "recoveredPts": gap.recovered_pts, "timeBase": gap.time_base
                    }),
                    DecodeDiagnostic::AudioGap(gap) => serde_json::json!({
                        "type": "decodeAudioConcealment", "reason": gap.reason,
                        "startSample": gap.start_sample, "endSample": gap.end_sample,
                        "samples": gap.end_sample - gap.start_sample, "sampleRate": gap.sample_rate
                    }),
                    DecodeDiagnostic::AudioOverlap(overlap) => serde_json::json!({
                        "type": "decodeAudioOverlap", "startSample": overlap.start_sample,
                        "endSample": overlap.end_sample,
                        "samples": overlap.end_sample - overlap.start_sample,
                        "sampleRate": overlap.sample_rate
                    }),
                };
                report["source"] = serde_json::json!(path);
                report["sourceId"] = serde_json::json!(source);
                alpha2_os::diagnostic!("{report}");
            });
            decoder
        }
    };
}
configure!(audio, alpha2_native::NativeAudioDecoder);
configure!(video, alpha2_native::NativeDecoder);
configure!(export, alpha2_native::NativeExportDecoder);

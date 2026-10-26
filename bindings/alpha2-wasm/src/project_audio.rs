//! The loaded project's audio: its mixer, the uploaded source audio it
//! reads, and what the last mix missed. Both browser renderers (the GPU's
//! `WebEffectRegistry` and the CPU's `WebCpuRenderer`) keep one and answer
//! their audio calls through it, so mixing never needs a GPU.

use alpha2_audio::TransportState;
use alpha2_engine::{AudioContinuity, AudioRenderer, EngineProject};
use alpha2_media::{Speaker, SpeakerLayout};
use alpha2_time::MediaTime;
use wasm_bindgen::prelude::*;

use crate::audio_store::{AudioStore, UploadRefusal, UploadSamples};
use crate::error::Code;
use crate::interop::audio_span;
use crate::web::{coded_error, internal_error, invalid_input, invalid_state, typed_error};

/// The speakers an upload's `layout` names: an array of `FFmpeg` channel
/// names, or a channel count for its default layout.
fn speaker_layout(layout: &JsValue) -> Option<SpeakerLayout> {
    if let Some(count) = layout.as_f64() {
        if count.fract() != 0.0 || !(1.0..=64.0).contains(&count) {
            return None;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        return SpeakerLayout::default_for(count as usize);
    }
    if !web_sys::js_sys::Array::is_array(layout) {
        return None;
    }
    let speakers = web_sys::js_sys::Array::from(layout)
        .iter()
        .map(|name| name.as_string().and_then(|name| Speaker::from_name(&name)))
        .collect::<Option<Vec<_>>>()?;
    SpeakerLayout::new(&speakers)
}

/// The audio half of a browser renderer.
pub(crate) struct ProjectAudio {
    audio: Option<AudioRenderer>,
    /// What `renderAudio` does with a small step outside an export
    /// (`setAudioContinuity`); an export is always exact.
    continuity: AudioContinuity,
    sources: AudioStore,
    missing: Vec<String>,
}

impl Default for ProjectAudio {
    fn default() -> Self {
        Self {
            audio: None,
            continuity: AudioContinuity::Exact,
            sources: AudioStore::default(),
            missing: Vec::new(),
        }
    }
}

impl ProjectAudio {
    /// A fresh mixer for `snapshot`, refused before anything changes.
    pub(crate) fn mixer_for(&self, snapshot: &EngineProject) -> Result<AudioRenderer, JsValue> {
        // A fresh renderer: the graph follows the new snapshot, and DSP state from the old one
        // would not match it.
        let mut audio = AudioRenderer::new(snapshot, 2, crate::AUDIO_BLOCK)
            .map_err(|error| typed_error(&error))?;
        // Loading ends any export, so the renderer mixes for preview.
        audio.set_continuity(self.continuity);
        Ok(audio)
    }

    /// Installs `audio`, made by [`Self::mixer_for`] for `snapshot`, which
    /// replaces `previous`. Audio uploaded for a source whose identity is
    /// unchanged is kept; gone or relocated sources lose theirs, and all
    /// audio goes when the project sample rate changes.
    pub(crate) fn install(
        &mut self,
        previous: Option<&EngineProject>,
        snapshot: &EngineProject,
        audio: AudioRenderer,
    ) {
        let sample_rate = snapshot.project().settings.sample_rate;
        match previous {
            Some(previous) if self.sources.sample_rate() == sample_rate => {
                let stale =
                    crate::project_cache::stale_sources(previous.project(), snapshot.project());
                self.sources
                    .retain_sources(|source| !stale.contains(source));
            }
            _ => self.sources = AudioStore::new(sample_rate),
        }
        self.audio = Some(audio);
    }

    /// `uploadAudioBlock`.
    pub(crate) fn upload_block(
        &mut self,
        source_id: &str,
        start_sample: i64,
        sample_rate: u32,
        layout: &JsValue,
        samples: &web_sys::js_sys::Float32Array,
        ends_source: Option<bool>,
    ) -> Result<(), JsValue> {
        let speakers = speaker_layout(layout).ok_or_else(|| {
            invalid_input(format!(
                "invalid audio upload for source {source_id}: its layout must be 1 to 24 FFmpeg \
                 channel names, or a channel count from 1 to 24"
            ))
        })?;
        let length = samples.length() as usize;
        let refused = |refusal| match refusal {
            UploadRefusal::WrongRate { expected } => invalid_input(format!(
                "invalid audio upload for source {source_id}: it is at {sample_rate} Hz, and \
                 audio is uploaded at the project's rate, {expected} Hz"
            )),
            UploadRefusal::Planes => invalid_input(format!(
                "invalid audio upload for source {source_id}: its {length} samples are not a \
                 full plane of one length for each of its {} speakers",
                speakers.channels()
            )),
            // Not fatal: the store holds what it did, and a smaller upload fits.
            UploadRefusal::TooLarge => coded_error(
                Code::ResourceLimit,
                format!(
                    "audio upload of {length} samples for source {source_id} is more than one \
                     window holds: at most 600 s, and at most the {} MiB every source shares",
                    crate::audio_store::MAX_RETAINED_BYTES >> 20,
                ),
            ),
        };
        // Checked from the length before copying into wasm memory, which never shrinks; an upload
        // that fits is copied once, into the window that keeps it.
        self.sources
            .admits(sample_rate, speakers.channels(), length)
            .map_err(refused)?;
        self.sources
            .upload_from(
                source_id,
                start_sample,
                sample_rate,
                speakers,
                &JsSamples(samples),
                ends_source.unwrap_or(false),
            )
            .map_err(refused)
    }

    pub(crate) fn release_source(&mut self, source_id: &str) -> bool {
        self.sources.release(source_id)
    }

    pub(crate) fn clear_sources(&mut self) {
        self.sources.clear();
    }

    pub(crate) fn retained_frames(&self) -> u32 {
        u32::try_from(self.sources.retained_frames()).unwrap_or(u32::MAX)
    }

    pub(crate) fn retained_bytes(&self) -> u32 {
        u32::try_from(self.sources.retained_bytes()).unwrap_or(u32::MAX)
    }

    /// `audioRetainedWindows`.
    pub(crate) fn retained_windows(&self, source_id: &str) -> Result<String, JsValue> {
        let windows: Vec<_> = self
            .sources
            .windows(source_id)
            .into_iter()
            .map(|(start, end)| serde_json::json!({ "startSample": start, "endSample": end }))
            .collect();
        serde_json::to_string(&windows).map_err(|error| internal_error(error.to_string()))
    }

    pub(crate) fn trim_before(&mut self, source_id: &str, ticks: i64) {
        self.sources.trim_before(source_id, MediaTime(ticks));
    }

    /// `setAudioContinuity`; an `exporting` renderer stays exact until its
    /// export ends.
    pub(crate) fn set_continuity(&mut self, policy: &str, exporting: bool) -> Result<(), JsValue> {
        let continuity = match policy {
            "exact" => AudioContinuity::Exact,
            "continue-small-gaps" => AudioContinuity::ContinueSmallGaps,
            _ => {
                return Err(invalid_input(format!(
                    "audio continuity is \"exact\" or \"continue-small-gaps\", not {policy:?}"
                )));
            }
        };
        self.continuity = continuity;
        if !exporting && let Some(audio) = self.audio.as_mut() {
            audio.set_continuity(continuity);
        }
        Ok(())
    }

    pub(crate) fn reset(&mut self) {
        if let Some(audio) = self.audio.as_mut() {
            audio.reset();
        }
    }

    /// `audioNeeds`.
    pub(crate) fn needs(
        &self,
        project: Option<&EngineProject>,
        start_sample: i64,
        frames: u32,
    ) -> Result<String, JsValue> {
        let project = project.ok_or_else(|| invalid_state("no project is loaded"))?;
        let audio = self.renderer()?;
        audio_span(audio.sample_rate(), start_sample, frames)?;
        let needs = audio
            .audio_needs(project, start_sample, frames as usize)
            .map_err(|error| typed_error(&error))?;
        let needs: Vec<_> = needs
            .iter()
            .map(|need| {
                serde_json::json!({
                    "sourceId": need.source_id.as_str(),
                    "startTicks": need.source_range.start().ticks().to_string(),
                    "endTicks": need.source_range.end().ticks().to_string(),
                })
            })
            .collect();
        serde_json::to_string(&needs).map_err(|error| internal_error(error.to_string()))
    }

    /// `renderAudio`.
    pub(crate) fn render(
        &mut self,
        project: Option<&EngineProject>,
        start_sample: i64,
        frames: u32,
        playing: bool,
    ) -> Result<Vec<f32>, JsValue> {
        if usize::try_from(frames).map_or(true, |frames| frames > crate::AUDIO_BLOCK) {
            return Err(invalid_input(format!(
                "renderAudio mixes at most {} samples per channel a call (audioMaxBlock), not {frames}",
                crate::AUDIO_BLOCK
            )));
        }
        let project = project.ok_or_else(|| invalid_state("no project is loaded"))?;
        let audio = self
            .audio
            .as_mut()
            .ok_or_else(|| invalid_state("this project has no audio renderer"))?;
        let transport = if playing {
            TransportState::Playing
        } else {
            TransportState::Stopped
        };
        let status = audio
            .render(
                project,
                start_sample,
                frames as usize,
                &self.sources,
                transport,
            )
            .map_err(|error| typed_error(&error))?;
        self.missing = status
            .missing_sources
            .iter()
            .map(ToString::to_string)
            .collect();
        let channels = audio.channels();
        let mut output = Vec::with_capacity(channels * status.frames);
        for channel in 0..channels {
            output.extend_from_slice(&audio.channel(channel)[..status.frames]);
        }
        Ok(output)
    }

    pub(crate) fn set_loudness_metering(&mut self, enabled: bool) -> Result<(), JsValue> {
        self.renderer_mut()?
            .set_loudness_metering(enabled)
            .map_err(|error| typed_error(&error))
    }

    pub(crate) fn loudness_report(&self) -> String {
        self.audio.as_ref().map_or_else(
            || "null".to_owned(),
            |renderer| {
                renderer.loudness_report().map_or_else(
                    || "null".to_owned(),
                    |report| {
                        crate::loudness::mix_report_json(&report, renderer.limiter_threshold())
                    },
                )
            },
        )
    }

    pub(crate) fn set_normalization_gain_db(&mut self, gain_db: f64) -> Result<(), JsValue> {
        self.renderer_mut()?
            .set_normalization_gain_db(gain_db)
            .map_err(|error| typed_error(&error))
    }

    pub(crate) fn missing_sources(&self) -> Result<String, JsValue> {
        serde_json::to_string(&self.missing).map_err(|error| internal_error(error.to_string()))
    }

    pub(crate) fn channels(&self) -> u32 {
        self.audio.as_ref().map_or(0, |audio| {
            u32::try_from(audio.channels()).unwrap_or(u32::MAX)
        })
    }

    pub(crate) fn sample_rate(&self) -> u32 {
        self.audio.as_ref().map_or(0, AudioRenderer::sample_rate)
    }

    /// Readies the mixer for an export: fresh DSP state, exact, and blocks a
    /// source is missing from held back.
    pub(crate) fn begin_export(&mut self) {
        // The renderer resets on any jump by itself; this covers an export starting exactly where
        // preview stopped.
        if let Some(audio) = self.audio.as_mut() {
            audio.set_continuity(AudioContinuity::Exact);
            audio.reset();
            audio.set_hold_incomplete_blocks(true);
        }
    }

    /// Returns the mixer to preview after an export.
    pub(crate) fn end_export(&mut self) {
        if let Some(audio) = self.audio.as_mut() {
            audio.set_hold_incomplete_blocks(false);
            audio.set_continuity(self.continuity);
        }
    }

    fn renderer(&self) -> Result<&AudioRenderer, JsValue> {
        self.audio
            .as_ref()
            .ok_or_else(|| invalid_state("this project has no audio renderer"))
    }

    fn renderer_mut(&mut self) -> Result<&mut AudioRenderer, JsValue> {
        self.audio
            .as_mut()
            .ok_or_else(|| invalid_state("this project has no audio renderer"))
    }
}

/// An upload's JS buffer, copied plane by plane straight into the window
/// that keeps it rather than into wasm memory first.
struct JsSamples<'a>(&'a web_sys::js_sys::Float32Array);

impl UploadSamples for JsSamples<'_> {
    fn values(&self) -> usize {
        self.0.length() as usize
    }

    fn append(&self, range: std::ops::Range<usize>, plane: &mut Vec<f32>) {
        let from = plane.len();
        plane.resize(from + range.len(), 0.0);
        let index = |at: usize| u32::try_from(at).unwrap_or(u32::MAX);
        self.0
            .subarray(index(range.start), index(range.end))
            .copy_to(&mut plane[from..]);
    }
}

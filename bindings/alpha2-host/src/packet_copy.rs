//! Exports that copy their source's compressed video packets instead of rendering.
//!
//! The video is copied only when the engine says the range is its sources shown untouched
//! ([`alpha2_engine::passthrough_plan`]) and the native side says the packets decode on their own
//! into exactly the rendered frames ([`alpha2_native::plan_packet_copy`]). Audio does not decide
//! this: untouched source audio over the whole file is copied too, anything else is mixed as a
//! render mixes it and encoded beside the copied video.
//!
//! Planning and copying run on watched worker threads ([`crate::heartbeat::run_watched`]), so a
//! read or write that never returns (a hard-mounted share) fails with `DECODE_STALLED` or
//! `ENCODE_STALLED` and a termination's cancel is still answered.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::audio::WindowedAudioSources;
use crate::config::ResolvedAudioRate;
use crate::failure::ExportPhase;
#[cfg(test)]
use crate::heartbeat::InjectedStall;
use crate::heartbeat::{Watched, WorkerHeartbeat, run_watched};
use crate::host::{Host, effective_range_json, normalize_export_audio};
use crate::metrics::{VideoPipeline, frames_per_second};
use crate::protocol::{HostError, LoudnessRequest};
use alpha2_audio::TransportState;
use alpha2_engine::{AudioRenderer, EngineProject, PassthroughAudio};
use alpha2_media::{AudioBlock, CancelToken, ColorMetadata};
use alpha2_model::Project;
use alpha2_native::{
    AudioBlockFeed, AudioStreamConfig, CopyAudio, NativeAudioCodec, NativeVideoCodec, PacketCopy,
    PacketCopyAudio, PacketCopyPlanner, PacketCopyRequest,
};
use alpha2_time::{FrameRate, MediaTime, TimeRange};
use serde_json::{Value, json};

/// Owns its parts because the plan reads the sources on its own worker thread.
pub(crate) struct CopyRequest {
    pub project: Project,
    pub assets: HashMap<String, PathBuf>,
    pub effective_range: TimeRange,
    pub rate: FrameRate,
    pub frames: u64,
    pub include_audio: bool,
    pub video_codec: NativeVideoCodec,
    pub container: alpha2_media::Container,
    pub pinned_encoder: Option<String>,
    pub loudness: bool,
    pub cancel: CancelToken,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub enum CopyStage {
    Plan,
    Copy,
}

#[cfg(test)]
thread_local! {
    /// Makes the next copy export on this thread block forever in `stage`, watched with this
    /// limit instead of the configured one.
    pub static INJECT_COPY_STALL: Cell<Option<(CopyStage, InjectedStall, Duration)>> =
        const { Cell::new(None) };
}

#[cfg_attr(not(test), allow(unused_variables))]
fn copy_worker_watch(stage: &str) -> (Arc<WorkerHeartbeat>, Duration) {
    let limit = crate::config::configured_stall_timeout();
    #[cfg(test)]
    if let Some((injected, stall, test_limit)) = INJECT_COPY_STALL.with(Cell::get)
        && matches!(
            (injected, stage),
            (CopyStage::Plan, "plan") | (CopyStage::Copy, "copy")
        )
    {
        INJECT_COPY_STALL.with(|cell| cell.set(None));
        return (Arc::new(WorkerHeartbeat::stalling(Some(stall))), test_limit);
    }
    (Arc::new(WorkerHeartbeat::default()), limit)
}

/// # Errors
/// The outer error is a failure no fallback to rendering can get past (a stalled source read, a
/// worker that panicked or could not start); the inner error is the plan's reason not to copy.
pub(crate) fn watched_packet_copy_plan(
    request: CopyRequest,
) -> Result<Result<CopyPlan, String>, String> {
    let (heartbeat, limit) = copy_worker_watch("plan");
    let cancel = request.cancel.clone();
    let beat = Arc::clone(&heartbeat);
    run_watched(
        &Watched {
            what: "packet copy plan",
            heartbeat,
            limit,
            cancel: &cancel,
        },
        move |_| {
            let _watch = alpha2_native::watch_decode_progress(move || beat.progressed());
            packet_copy_plan(&request)
        },
        &mut |_| Ok(()),
        &|stalled_for| {
            crate::failure::tag_phase(
                ExportPhase::Decode,
                None,
                format!(
                    "{}the packet copy's plan has been inside one read of its sources for {} ms, past the {} ms limit: FFmpeg is blocked, most likely reading a source that stopped answering",
                    crate::protocol::decode_stalled_tag(),
                    stalled_for.as_millis(),
                    limit.as_millis(),
                ),
            )
        },
    )
}

pub(crate) struct CopyPlan {
    pub video: PacketCopy,
    pub audio: PassthroughAudio,
}

impl CopyPlan {
    /// `None` when the audio is copied or there is none.
    fn encode_reason(&self) -> Option<String> {
        match &self.audio {
            PassthroughAudio::Absent => None,
            PassthroughAudio::Untouched if self.video.copies_audio() => None,
            PassthroughAudio::Untouched => Some(
                self.video
                    .audio_copy_refusal()
                    .unwrap_or("loudness normalization changes the audio")
                    .to_owned(),
            ),
            PassthroughAudio::Mixed(reason) => Some(reason.clone()),
        }
    }
}

/// # Errors
/// The first reason it cannot, from the request, the project, or the file.
pub(crate) fn packet_copy_plan(request: &CopyRequest) -> Result<CopyPlan, String> {
    if let Some(name) = &request.pinned_encoder {
        return Err(format!(
            "the request pins the encoder {name}, and a copy encodes no video"
        ));
    }
    if crate::config::configured_frame_capture().is_some() {
        return Err("ALPHA2_EXPORT_FRAMES_Y4M captures rendered frames".to_owned());
    }
    let plan = alpha2_engine::passthrough_plan(
        &request.project,
        request.effective_range,
        request.include_audio,
    )?;
    let first = request
        .rate
        .frame_at_or_before(request.effective_range.start())
        .ok_or_else(|| "the range does not start on the frame grid".to_owned())?;
    let timeline_times = (0..request.frames)
        .map(|offset| {
            let index = first.checked_add(i64::try_from(offset).ok()?)?;
            request.rate.frame_start(index)
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| "the range's frame times overflow".to_owned())?;
    // Loudness normalization changes the mix even when nothing in the project does.
    let offer_audio = plan.audio == PassthroughAudio::Untouched && !request.loudness;
    let mut runs = Vec::with_capacity(plan.segments.len());
    let mut planner = PacketCopyPlanner::new(request.cancel.clone());
    for segment in &plan.segments {
        // A clip shorter than a frame shows none and copies nothing.
        let shown = timeline_times
            .iter()
            .filter(|time| {
                segment.timeline_range.start() <= **time && **time < segment.timeline_range.end()
            })
            .copied()
            .collect::<Vec<_>>();
        let Some(&first_shown) = shown.first() else {
            continue;
        };
        let source = request
            .assets
            .get(segment.source_id.as_str())
            .ok_or_else(|| format!("source {} has no file", segment.source_id))?;
        let frame_times = shown
            .iter()
            .map(|time| segment.source_time(*time))
            .collect::<Vec<_>>();
        let run = planner.plan(&PacketCopyRequest {
            source,
            frame_times: &frame_times,
            width: request.project.settings.width,
            height: request.project.settings.height,
            codec: request.video_codec,
            container: request.container,
            color: ColorMetadata::EXPORT,
            output_start: MediaTime::from_ticks(
                first_shown.ticks() - request.effective_range.start().ticks(),
            ),
            audio: offer_audio.then_some(PacketCopyAudio {
                sample_rate: request.project.settings.sample_rate,
                channels: 2,
            }),
        });
        runs.push(run.map_err(|reason| {
            if plan.segments.len() == 1 {
                reason
            } else {
                format!("clip {}: {reason}", segment.clip_id)
            }
        })?);
    }
    Ok(CopyPlan {
        video: PacketCopy::join(runs)?,
        audio: plan.audio,
    })
}

/// The export's mix, rendered block by block from output sample zero. It owns its inputs because
/// the copy runs on its own worker thread.
pub(crate) struct ExportAudioFeed {
    snapshot: Arc<EngineProject>,
    renderer: AudioRenderer,
    sources: WindowedAudioSources,
    start_sample: i64,
    frames: usize,
    produced: usize,
    cancel: CancelToken,
}

impl ExportAudioFeed {
    /// Normalization measures the whole range first.
    ///
    /// # Errors
    /// Says why the sources or the renderer could not be set up.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        snapshot: Arc<EngineProject>,
        project: &Project,
        assets: &HashMap<String, PathBuf>,
        range: TimeRange,
        loudness: Option<LoudnessRequest>,
        cancel: CancelToken,
        audio_window_bytes: crate::memory::ByteBudget,
        decoding: Arc<crate::recovery::ExportDecoding>,
    ) -> Result<(Self, Option<Value>), String> {
        let sample_rate = project.settings.sample_rate;
        let window =
            alpha2_engine::export_audio_window(range, sample_rate).map_err(crate::error_message)?;
        let (start_sample, frames) = (window.start_sample, window.frames);
        let mut renderer = AudioRenderer::new(&snapshot, 2, 4_096).map_err(crate::error_message)?;
        let report = loudness
            .map(|request| {
                normalize_export_audio(
                    &mut renderer,
                    &snapshot,
                    project,
                    assets,
                    start_sample,
                    frames,
                    request,
                    &cancel,
                    audio_window_bytes,
                    &decoding,
                )
            })
            .transpose()?;
        Ok((
            Self {
                snapshot,
                renderer,
                sources: crate::audio::windowed_audio_sources(
                    project,
                    assets,
                    sample_rate,
                    audio_window_bytes,
                    decoding,
                )?,
                start_sample,
                frames,
                produced: 0,
                cancel,
            },
            report,
        ))
    }
}

impl AudioBlockFeed for ExportAudioFeed {
    fn next_block(&mut self) -> Result<Option<AudioBlock>, String> {
        if self.produced >= self.frames {
            return Ok(None);
        }
        let count = (self.frames - self.produced).min(4_096);
        let absolute = self
            .start_sample
            .saturating_add(i64::try_from(self.produced).unwrap_or(i64::MAX));
        let needs = self
            .renderer
            .audio_needs(&self.snapshot, absolute, count)
            .map_err(crate::error_message)?;
        self.sources
            .prepare(&needs, &self.cancel)
            .map_err(crate::audio::window_error_message)?;
        let status = self
            .renderer
            .render(
                &self.snapshot,
                absolute,
                count,
                &self.sources,
                TransportState::Playing,
            )
            .map_err(crate::error_message)?;
        if !status.missing_sources.is_empty() {
            return Err(crate::protocol::missing_input(format!(
                "native audio mix is missing sources: {:?}",
                status.missing_sources
            )));
        }
        let mut block = self.renderer.block(&status);
        block.start_sample = i64::try_from(self.produced).unwrap_or(i64::MAX);
        self.produced += count;
        Ok(Some(block))
    }
}

/// Returns the copy's report and the feed's renderer, which measured the mix when the export
/// normalized it.
fn copy_with_audio(
    plan: &PacketCopy,
    output: &alpha2_native::OutputTarget,
    container: alpha2_media::Container,
    mut feed: Option<ExportAudioFeed>,
    audio_config: AudioStreamConfig,
    cancel: &CancelToken,
    reached: &dyn Fn(u64),
) -> (
    Result<alpha2_native::PacketCopyReport, String>,
    Option<AudioRenderer>,
) {
    let audio = match feed.as_mut() {
        Some(feed) => CopyAudio::Encode {
            config: audio_config,
            feed,
        },
        None if plan.copies_audio() => CopyAudio::Source,
        None => CopyAudio::None,
    };
    let copied =
        alpha2_native::copy_packets(plan, output, container, audio, cancel, &mut |copied| {
            reached(copied);
        });
    (
        copied.map_err(crate::error_message),
        feed.map(|feed| feed.renderer),
    )
}

fn copy_stall(phase: ExportPhase, stalled_for: Duration, limit: Duration) -> String {
    crate::failure::tag_phase(
        phase,
        None,
        format!(
            "{}the packet copy has been inside one muxer call for {} ms, past the {} ms limit: FFmpeg is blocked, most likely writing an output that stopped answering",
            crate::protocol::encode_stalled_tag(),
            stalled_for.as_millis(),
            limit.as_millis(),
        ),
    )
}

pub(crate) struct CopyExport<'a> {
    pub plan: &'a CopyPlan,
    pub snapshot: &'a EngineProject,
    pub project: &'a Project,
    pub assets: &'a HashMap<String, PathBuf>,
    pub effective_range: TimeRange,
    pub container: alpha2_media::Container,
    pub audio_codec: NativeAudioCodec,
    pub audio_encoder: Option<String>,
    pub audio_rate: ResolvedAudioRate,
    pub loudness: Option<LoudnessRequest>,
    pub output: &'a alpha2_native::OutputTarget,
    pub started: Instant,
}

impl Host {
    /// # Errors
    /// A failed or cancelled copy, whose partial output is already removed.
    pub(crate) fn export_packet_copy(
        &self,
        export: &CopyExport<'_>,
        progress: &mut dyn FnMut(crate::protocol::Progress) -> Result<(), HostError>,
    ) -> Result<Value, String> {
        let plan = &export.plan.video;
        let total = plan.video_packets();
        let cancel = crate::termination::export_cancel();
        let encode_reason = export.plan.encode_reason();
        let mut loudness_report = None;
        let feed = match encode_reason {
            Some(_) => {
                let (feed, report) = ExportAudioFeed::new(
                    Arc::new(export.snapshot.clone()),
                    export.project,
                    export.assets,
                    export.effective_range,
                    export.loudness,
                    cancel.clone(),
                    self.memory.audio_window.byte_budget(),
                    self.failure.decoding(),
                )?;
                loudness_report = report;
                Some(feed)
            }
            None => None,
        };
        let audio_config = AudioStreamConfig {
            codec: export.audio_codec,
            sample_rate: export.project.settings.sample_rate,
            channels: 2,
            bit_rate: export.audio_rate.bitrate,
        };
        let audio_strategy = match (&feed, plan.copies_audio()) {
            (Some(_), _) => Some("encode"),
            (None, true) => Some("copy"),
            (None, false) => None,
        };
        alpha2_os::diagnostic!(
            "alpha2 native export: copying {total} video packets from the source, audio {}",
            audio_strategy.unwrap_or("none")
        );
        self.failure
            .note("videoPipeline", json!(VideoPipeline::PacketCopy.as_str()));
        self.failure.note("videoEncoder", Value::Null);
        self.failure.note(
            "audioEncoder",
            json!(feed.is_some().then_some(&export.audio_encoder)),
        );
        self.failure.note("audioStrategy", json!(audio_strategy));
        self.failure.enter(ExportPhase::Encode, Some(0));
        let phase = Cell::new(ExportPhase::Encode);
        let (heartbeat, limit) = copy_worker_watch("copy");
        let beat = Arc::clone(&heartbeat);
        let worker_plan = plan.clone();
        let output = export.output.clone();
        let container = export.container;
        let worker_cancel = cancel.clone();
        let copied = run_watched(
            &Watched {
                what: "packet copy",
                heartbeat,
                limit,
                cancel: &cancel,
            },
            move |reached| {
                // Any muxer call may never return (an output that stopped answering).
                let _watch = alpha2_native::watch_encode_calls(move || beat.beat());
                copy_with_audio(
                    &worker_plan,
                    &output,
                    container,
                    feed,
                    audio_config,
                    &worker_cancel,
                    reached,
                )
            },
            &mut |copied| {
                if copied == total {
                    phase.set(ExportPhase::Mux);
                    self.failure.enter(ExportPhase::Mux, None);
                } else {
                    self.failure.enter(ExportPhase::Encode, Some(copied));
                }
                if copied.is_multiple_of(30) || copied == total {
                    progress(crate::protocol::Progress::frames(copied, total))
                        .map_err(|error| error.message)?;
                }
                Ok(())
            },
            &|stalled_for| copy_stall(phase.get(), stalled_for, limit),
        )?;
        let (report, renderer) = copied;
        let loudness = crate::host::finish_loudness(loudness_report, renderer.as_ref());
        Ok(self.copy_result(export, &report?, audio_strategy, loudness))
    }

    fn copy_result(
        &self,
        export: &CopyExport<'_>,
        report: &alpha2_native::PacketCopyReport,
        audio_strategy: Option<&str>,
        loudness: Value,
    ) -> Value {
        let encoded = audio_strategy == Some("encode");
        let started = export.started;
        let mut result = self.response_metadata(started.elapsed().as_secs_f64() * 1_000.0);
        result["frames"] = json!(report.video_packets);
        result["audio"] = json!(audio_strategy.is_some());
        result["videoCodec"] = Value::Null;
        result["videoEncoder"] = Value::Null;
        result["videoEncoderClass"] = Value::Null;
        result["videoEncoderHardware"] = json!(false);
        result["videoEncoderLibrary"] = Value::Null;
        result["videoEncoderPreferred"] = Value::Null;
        result["videoEncoderFallbacks"] = json!([]);
        result["videoPipeline"] = json!(VideoPipeline::PacketCopy.as_str());
        result["gpuOutputPathUnavailable"] = Value::Null;
        result["gpuOutputPathFallback"] = Value::Null;
        result["publish"] = crate::host::publish_json(report.publish);
        result["videoTranscode"] = Value::Null;
        result["yuvTranscodeIneligible"] = Value::Null;
        result["strategy"] = json!("copy");
        result["copyIneligible"] = Value::Null;
        result["copiedSegments"] = json!(export.plan.video.segments());
        result["audioStrategy"] = json!(audio_strategy);
        result["audioCopyIneligible"] = json!(export.plan.encode_reason());
        result["effectiveRange"] = effective_range_json(export.effective_range);
        result["color"] = self.color_pipeline_json(export.project.settings.color);
        result["loudness"] = loudness;
        result["videoRateControl"] = Value::Null;
        // Copied audio is the source's AAC; an encoded mix is the export's codec.
        result["audioCodec"] = json!(audio_strategy.map(|_| if encoded {
            export.audio_codec.as_str()
        } else {
            "aac"
        }));
        result["audioRateControl"] = if encoded {
            export.audio_rate.to_json()
        } else {
            Value::Null
        };
        result["audioEncoder"] = json!(if encoded {
            export.audio_encoder.as_deref()
        } else {
            None
        });
        result["fps"] = json!(frames_per_second(
            report.video_packets,
            started.elapsed().as_secs_f64()
        ));
        result["performance"] = json!({
            "videoPipeline": VideoPipeline::PacketCopy.as_str(),
            "copiedVideoPackets": report.video_packets,
            "copiedAudioPackets": report.audio_packets,
            "encodedAudioFrames": report.encoded_audio_frames,
            "copiedBytes": report.bytes,
            "outputReadbackBytes": 0,
            "outputCpuPackBytes": 0,
            "encoderInputCopyBytes": 0,
        });
        result
    }
}

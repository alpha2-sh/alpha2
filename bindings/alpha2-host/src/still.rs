//! Frames of a project rendered to PNG through the same engine steps an export takes.
//!
//! The picture is read before YUV conversion, with premultiplied alpha divided out for PNG. The
//! export loop pipelines readback, encode and render cache around these steps, so stills run their
//! own loop; the host tests check both draw the same pixels.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::PoisonError;
use std::time::{Duration, Instant};

use alpha2_engine::{ExportTextureStep, Exporter, FrameProvider, FramesWithText, QualityProfile};
use alpha2_host_core::host_frames::Attachment;
use alpha2_media::CancelToken;
use alpha2_native::{ExistingOutput, OutputTarget};
use alpha2_render::RenderSize;
use alpha2_time::{MediaTime, TimeRange};
use serde_json::{Value, json};

use crate::decode::{DecodeScheduler, DecodeWorker};
use crate::effect_request::EffectRequest;
use crate::encode::PipelineFailure;
use crate::export_decode::ExportDecodePlan;
use crate::host::{Host, result_cache_budget, written_path};
use crate::metrics::ExportMetrics;
use crate::protocol::StillEntry;
use crate::renderer_backend::RendererBackend;
use crate::{WORKER_RETRY_INTERVAL, error_message};

/// What one response carries in attachments, the most a frame holds.
const MAX_RESPONSE_ATTACHMENT_BYTES: u64 = u32::MAX as u64;

impl Host {
    /// Renders in time order so every decoder only moves forward, and answers in request order.
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub(crate) fn render_stills(
        &mut self,
        project: &str,
        assets: &HashMap<String, PathBuf>,
        effect_assets: &HashMap<String, PathBuf>,
        effects: Vec<EffectRequest>,
        stills: &[StillEntry],
        alpha: bool,
        existing: ExistingOutput,
    ) -> Result<Value, String> {
        let started = Instant::now();
        let (project, snapshot) = self.prepare(project, effect_assets, effects)?;
        let size = RenderSize::new(project.settings.width, project.settings.height)
            .map_err(error_message)?;
        let rate = project.settings.frame_rate;
        let mut order = (0..stills.len()).collect::<Vec<_>>();
        order.sort_by_key(|&index| (stills[index].time, index));
        let exporters = stills
            .iter()
            .map(|still| {
                Exporter::still(
                    &snapshot,
                    MediaTime::from_ticks(still.time),
                    rate,
                    QualityProfile::export(),
                )
                .map(|exporter| exporter.with_background(!alpha))
                .map_err(error_message)
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.renderer.clear_result_cache();
        self.renderer.set_result_cache_budget(result_cache_budget(
            size,
            self.renderer.intermediate_format(),
        ));
        let cancel = crate::termination::export_cancel();
        let providers = self.export_providers(&project, size);
        let first = exporters[order[0]].effective_range().start();
        let last = exporters[order[order.len() - 1]].effective_range().end();
        let range = TimeRange::new(first, last).map_err(error_message)?;
        let plan = ExportDecodePlan::new(
            self.export_codec_split(&project, assets, range, true, (false, None)),
            providers,
        );
        let mut worker = DecodeWorker::spawn(
            &project,
            assets,
            &self.decode_worker_config(&plan),
            cancel.clone(),
            PipelineFailure::default(),
        )?;
        let mut gpu_frames = self.export_gpu_frames(size, &project);
        let mut cpu_frames = crate::cpu_frames::NativeCpuFrames::new(size, self.cache_scope);
        cpu_frames.color = gpu_frames.color;
        let mut metrics = ExportMetrics::default();
        let mut timings = StillTimings::default();
        let mut answers = vec![Value::Null; stills.len()];
        let mut written: Vec<&Path> = Vec::with_capacity(stills.len());
        let mut pngs: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut attached = 0_u64;
        for (index, mut exporter) in order.iter().map(|&index| (index, exporters[index].clone())) {
            let still = &stills[index];
            let frame_time = exporter
                .pending_time()
                .ok_or_else(|| "a still has no frame".to_owned())?;
            let decode_started = Instant::now();
            let mut scheduler = DecodeScheduler::new(1, &project);
            scheduler.refill(&exporter, &snapshot, &worker, &mut metrics)?;
            let decoded = loop {
                if let Some(decoded) = worker.result_timeout(WORKER_RETRY_INTERVAL)? {
                    break decoded;
                }
                if cancel.is_cancelled() {
                    return Err("the still was cancelled".to_owned());
                }
            };
            let bytes = decoded.bytes;
            if let Err(error) = scheduler.accept(&decoded, 0) {
                worker.release(bytes);
                return Err(error);
            }
            let installed = match &self.renderer {
                RendererBackend::Gpu(renderer) => gpu_frames.install(renderer, decoded),
                RendererBackend::Cpu(renderer) => cpu_frames.install(renderer, decoded),
            };
            worker.release(bytes);
            installed?;
            timings.decode += decode_started.elapsed();
            let render_started = Instant::now();
            let pixels = {
                let mut texts = self.texts.lock().unwrap_or_else(PoisonError::into_inner);
                texts
                    .prepare_at(self.renderer.text_upload(), &snapshot, frame_time, size)
                    .map_err(error_message)?;
                let supply: &dyn FrameProvider = if self.renderer.cpu().is_some() {
                    &cpu_frames
                } else {
                    &gpu_frames
                };
                let frames = FramesWithText::new(supply, &texts);
                match &self.renderer {
                    RendererBackend::Gpu(renderer) => {
                        let texture = rendered(exporter.render_next_texture(
                            &snapshot,
                            &**renderer,
                            &frames,
                        ))?;
                        renderer.read_rgba8(&texture).map_err(error_message)?
                    }
                    RendererBackend::Cpu(renderer) => {
                        rendered(exporter.render_next_texture(&snapshot, &**renderer, &frames))?
                            .read_rgba8()
                    }
                }
            };
            timings.render += render_started.elapsed();
            let write_started = Instant::now();
            let straight = alpha2_host_core::tools::unpremultiplied(pixels);
            let mut answer = json!({
                "frameTime": frame_time.ticks(),
                "width": size.width,
                "height": size.height,
            });
            if let Some(output) = &still.output {
                // A link or a second name of a folder can still name a file an earlier still
                // wrote.
                if let Some(earlier) = written
                    .iter()
                    .find(|earlier| alpha2_os::same_file_at(output, earlier).unwrap_or(false))
                {
                    return Err(crate::protocol::code_tag(
                        "INVALID_INPUT",
                        format!(
                            "two stills write one file: {} and {}",
                            earlier.display(),
                            output.display()
                        ),
                    ));
                }
                let published = alpha2_native::write_png(
                    OutputTarget {
                        path: output.clone(),
                        existing,
                    },
                    size.width,
                    size.height,
                    &straight,
                )
                .map_err(|error| crate::encode::encode_failure("writing a still", error))?;
                written.push(output);
                answer["output"] = json!(written_path(output));
                answer["publish"] = crate::host::publish_json(Some(published));
            } else {
                let png = alpha2_host_core::partial::png_bytes(size.width, size.height, &straight)
                    .map_err(|error| crate::protocol::code_tag("ENCODE_ERROR", error))?;
                attached = attached.saturating_add(png.len() as u64);
                if attached > MAX_RESPONSE_ATTACHMENT_BYTES {
                    return Err(crate::protocol::code_tag(
                        "RESOURCE_LIMIT",
                        format!(
                            "the stills without an output are over {MAX_RESPONSE_ATTACHMENT_BYTES} \
                             bytes of PNG, more than one response carries; give some an output"
                        ),
                    ));
                }
                answer["output"] = Value::Null;
                pngs.push((index, png));
            }
            timings.write += write_started.elapsed();
            answers[index] = answer;
            check_cancel(&cancel)?;
        }
        worker.finish()?;
        // Attached in request order, which is the order their indices were given in.
        pngs.sort_by_key(|(index, _)| *index);
        for (attachment, (index, _)) in pngs.iter().enumerate() {
            answers[*index]["attachment"] = json!(attachment);
        }
        self.reply_attachments = pngs
            .into_iter()
            .map(|(_, bytes)| Attachment {
                kind: "image/png".to_owned(),
                bytes,
            })
            .collect();
        let mut result = self.response_metadata(started.elapsed().as_secs_f64() * 1_000.0);
        result["stills"] = Value::Array(answers);
        result["timings"] = timings.to_json();
        result["unmatchedFontFamilies"] = json!(self.unmatched_font_families());
        Ok(result)
    }
}

#[derive(Default)]
struct StillTimings {
    decode: Duration,
    render: Duration,
    write: Duration,
}

impl StillTimings {
    fn to_json(&self) -> Value {
        let ms = |duration: Duration| duration.as_secs_f64() * 1_000.0;
        json!({
            "decodeMs": ms(self.decode),
            "renderMs": ms(self.render),
            "writeMs": ms(self.write),
        })
    }
}

fn rendered<I>(
    step: Result<ExportTextureStep<I>, alpha2_engine::EngineError>,
) -> Result<I, String> {
    match step.map_err(error_message)? {
        ExportTextureStep::Rendered { texture, .. } => Ok(texture),
        ExportTextureStep::Waiting {
            missing_sources, ..
        } => Err(crate::protocol::missing_input(format!(
            "the still is missing sources: {missing_sources:?}"
        ))),
        ExportTextureStep::Complete => Err("a still rendered no frame".to_owned()),
    }
}

fn check_cancel(cancel: &CancelToken) -> Result<(), String> {
    if cancel.is_cancelled() {
        Err("the still was cancelled".to_owned())
    } else {
        Ok(())
    }
}

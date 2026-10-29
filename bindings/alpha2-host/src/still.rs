//! Frames of a project rendered to PNG as an export renders them.
//!
//! Each still is the frame of the project's frame grid that holds its time,
//! drawn by the same engine step an export of that frame would take
//! ([`Exporter::still`], [`Exporter::render_next_texture`]), from sources the
//! export's decode pool decoded and its frame supply installed. The picture
//! is read before any conversion to YUV: the renderer's display-encoded RGBA,
//! with its premultiplied alpha divided out for PNG's straight alpha (every
//! frame is opaque over the project's background, so this changes nothing
//! today). The export's readback loop pipelines readback, encode and render
//! cache around the same steps, so the still runs its own loop beside it;
//! the host tests check the two draw the same pixels.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::PoisonError;
use std::time::{Duration, Instant};

use alpha2_engine::{ExportTextureStep, Exporter, FrameProvider, FramesWithText, QualityProfile};
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

impl Host {
    /// Renders each of `stills` of `project` to its PNG, in time order, so
    /// every decoder only moves forward, and answers each one's file, frame
    /// time and size in the order they were asked for.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn render_stills(
        &mut self,
        project: &str,
        assets: &HashMap<String, PathBuf>,
        effect_assets: &HashMap<String, PathBuf>,
        effects: Vec<EffectRequest>,
        stills: &[StillEntry],
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
        // The decoders are sized to what the stills read, over the span they cover.
        let first = exporters[order[0]].effective_range().start();
        let last = exporters[order[order.len() - 1]].effective_range().end();
        let range = TimeRange::new(first, last).map_err(error_message)?;
        let plan = ExportDecodePlan::new(
            self.export_codec_split(&project, assets, range, true, false),
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
        let mut cpu_frames = crate::cpu_frames::NativeCpuFrames::new(size);
        cpu_frames.color = gpu_frames.color;
        let mut metrics = ExportMetrics::default();
        let mut timings = StillTimings::default();
        let mut answers = vec![Value::Null; stills.len()];
        for (index, mut exporter) in order.iter().map(|&index| (index, exporters[index].clone())) {
            let still = &stills[index];
            let frame_time = exporter
                .pending_time()
                .ok_or_else(|| "a still has no frame".to_owned())?;
            // One frame: the job an export sends for it, with the stills the pool and the frame
            // supply both keep from the frame before.
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
            let straight = unpremultiplied(pixels);
            alpha2_native::write_png(
                OutputTarget {
                    path: still.output.clone(),
                    existing,
                },
                size.width,
                size.height,
                &straight,
            )
            .map_err(|error| crate::encode::encode_failure("writing a still", error))?;
            timings.write += write_started.elapsed();
            answers[index] = json!({
                "output": written_path(&still.output),
                "frameTime": frame_time.ticks(),
                "width": size.width,
                "height": size.height,
            });
            check_cancel(&cancel)?;
        }
        worker.finish()?;
        let mut result = self.response_metadata(started.elapsed().as_secs_f64() * 1_000.0);
        result["stills"] = Value::Array(answers);
        result["timings"] = timings.to_json();
        Ok(result)
    }
}

/// Where a still's time went.
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

/// The picture of a step that rendered: a still's sources were all
/// installed, so a step that waits names what is missing.
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

/// `pixels`, premultiplied RGBA8, with each color divided by its alpha and
/// rounded to the nearest code: PNG stores straight alpha. Opaque and fully
/// transparent pixels stay as they are.
fn unpremultiplied(mut pixels: Vec<u8>) -> Vec<u8> {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        let alpha = u16::from(pixel[3]);
        if alpha == 0 || alpha == 255 {
            continue;
        }
        for channel in &mut pixel[..3] {
            let straight = (u16::from(*channel) * 255 + alpha / 2) / alpha;
            *channel = u8::try_from(straight.min(255)).unwrap_or(u8::MAX);
        }
    }
    pixels
}

#[cfg(test)]
mod tests {
    use super::unpremultiplied;

    #[test]
    fn premultiplied_pixels_are_divided_by_their_alpha() {
        assert_eq!(
            unpremultiplied(vec![
                10, 20, 30, 255, 0, 0, 0, 0, 64, 32, 0, 128, 200, 0, 0, 100
            ]),
            vec![10, 20, 30, 255, 0, 0, 0, 0, 128, 64, 0, 128, 255, 0, 0, 100]
        );
    }
}

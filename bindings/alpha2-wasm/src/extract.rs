//! Drives the extraction session for the web, push style. The worker asks
//! [`WebExtractSession::next`] what to do, supplies frames and audio, hands each delivery to the
//! extractor and gives its emits back. Source frames arrive as `VideoFrame.copyTo` planes and are
//! presented on the CPU renderer as the native hosts present them, so deliveries match exactly.

use std::sync::Arc;

use alpha2_engine::extract::{
    AudioChannels, Delivery, Descriptor, Direction, Emit, ExtractRequest, ExtractSession,
    FrameFrom, Need, SessionError, Step, SuppliedAudio, SuppliedFrame, client_context_json,
    emit_from_json, mono_downmix, present_for_extraction,
};
use alpha2_media::{
    ColorMetadata, DecodedVideoFrame, Orientation, PixelFormat, Plane, Rect, SampleAspectRatio,
    Speaker,
};
use alpha2_model::{MediaFingerprint, Project};
use alpha2_render::CpuRenderer;
use alpha2_time::MediaTime;
use serde_json::{Value, json};
use wasm_bindgen::prelude::*;

use crate::web::{chroma_siting_named, invalid_input, rotation_from_degrees, webcodecs_yuv_format};

fn refused(error: impl std::fmt::Display) -> JsValue {
    invalid_input(error.to_string())
}

/// One extraction run.
#[wasm_bindgen(js_name = ExtractSession)]
pub struct WebExtractSession {
    session: Option<ExtractSession>,
    renderer: CpuRenderer,
    /// The delivery `next` handed out last, until the next call.
    delivered: Option<Delivery>,
    /// The need `next` handed out last, until it is answered.
    need: Option<Need>,
    /// The project's canvas, which timeline frames are delivered from.
    canvas: (u32, u32),
}

fn direction_name(direction: Direction) -> &'static str {
    match direction {
        Direction::Forward => "forward",
        Direction::Backward => "backward",
    }
}

fn need_json(need: &Need, canvas: (u32, u32)) -> Value {
    match need {
        Need::SourceFrame {
            source,
            from,
            direction,
            size,
            ..
        } => {
            let (from, time) = match from {
                FrameFrom::Start(time) => ("start", time.ticks()),
                FrameFrom::After(pts) => ("after", pts.ticks()),
            };
            json!({ "need": "source_frame", "source": source.as_str(), "from": from, "time": time,
                    "direction": direction_name(*direction), "size": size })
        }
        Need::TimelineFrame { time, size, .. } => {
            let (width, height) = alpha2_engine::extract::delivered_size(canvas, *size);
            json!({ "need": "timeline_frame", "time": time.ticks(), "size": size,
                    "width": width, "height": height })
        }
        Need::SourceAudio {
            source,
            stream,
            start_sample,
            frames,
            rate,
            channels,
        } => json!({ "need": "source_audio", "source": source.as_str(), "stream": stream,
                     "startSample": start_sample, "frames": frames, "rate": rate,
                     "channels": channels }),
        Need::TimelineAudio {
            start_sample,
            frames,
            rate,
            channels,
        } => json!({ "need": "timeline_audio", "startSample": start_sample, "frames": frames,
                     "rate": rate, "channels": channels }),
    }
}

#[wasm_bindgen(js_class = ExtractSession)]
impl WebExtractSession {
    /// A run of the extractor `descriptor` describes over `project`, as `request` asks, all JSON.
    ///
    /// # Errors
    /// `INVALID_INPUT` for a project, descriptor or request that does not parse or is refused.
    #[wasm_bindgen(constructor)]
    pub fn new(
        project: &str,
        descriptor: &str,
        request: &str,
    ) -> Result<WebExtractSession, JsValue> {
        let project = Project::from_json(project).map_err(refused)?;
        let canvas = (project.settings.width, project.settings.height);
        let descriptor: Descriptor = serde_json::from_str(descriptor).map_err(refused)?;
        let request: ExtractRequest = serde_json::from_str(request).map_err(refused)?;
        let session = ExtractSession::new(&project, &descriptor, request).map_err(refused)?;
        Ok(Self {
            session: Some(session),
            renderer: CpuRenderer::new(1),
            delivered: None,
            need: None,
            canvas,
        })
    }

    fn session(&mut self) -> Result<&mut ExtractSession, JsValue> {
        self.session
            .as_mut()
            .ok_or_else(|| invalid_input("the extraction has finished"))
    }

    /// What `begin` is told as JSON, with the range, passes, clock and delivered video or audio.
    ///
    /// # Errors
    /// After `finish`.
    pub fn context(&mut self) -> Result<String, JsValue> {
        let context = self.session()?.context();
        Ok(client_context_json(&context).to_string())
    }

    /// The next thing to do, as JSON: `{ step: "need", need: { ... } }`, `{ step: "pass", pass,
    /// direction }`, `{ step: "video", time, width, height, format }` (bytes from
    /// [`Self::video_bytes`]), `{ step: "audio", start, startSample, frames, rate, channels }`
    /// (samples from [`Self::audio_samples`]) or `{ step: "done" }`.
    ///
    /// # Errors
    /// After `finish`.
    pub fn next(&mut self) -> Result<String, JsValue> {
        let step = self.session()?.next();
        self.delivered = None;
        self.need = None;
        let json = match step {
            Step::Need(need) => {
                let json = json!({ "step": "need", "need": need_json(&need, self.canvas) });
                self.need = Some(need);
                json
            }
            Step::PassBoundary { pass, direction } => {
                json!({ "step": "pass", "pass": pass, "direction": direction_name(direction) })
            }
            Step::Deliver(delivery) => {
                let json = match &delivery {
                    Delivery::Video(frame) => json!({ "step": "video", "time": frame.time.ticks(),
                        "width": frame.width, "height": frame.height, "format": frame.format }),
                    Delivery::Audio(block) => json!({ "step": "audio",
                        "start": block.start.ticks(), "startSample": block.start_sample,
                        "frames": block.frames, "rate": block.rate, "channels": block.channels }),
                };
                self.delivered = Some(delivery);
                json
            }
            Step::Done => json!({ "step": "done" }),
        };
        Ok(json.to_string())
    }

    /// The pixels of the video delivery `next` handed out.
    #[wasm_bindgen(js_name = videoBytes)]
    #[must_use]
    pub fn video_bytes(&self) -> Vec<u8> {
        match &self.delivered {
            Some(Delivery::Video(frame)) => frame.bytes.clone(),
            _ => Vec::new(),
        }
    }

    /// The planar samples of the audio delivery `next` handed out.
    #[wasm_bindgen(js_name = audioSamples)]
    #[must_use]
    pub fn audio_samples(&self) -> Vec<f32> {
        match &self.delivered {
            Some(Delivery::Audio(block)) => block.samples.clone(),
            _ => Vec::new(),
        }
    }

    /// Answers a source-frame need with a decoded frame's planes, as `VideoFrame.copyTo` writes
    /// them (the arguments of `uploadYuvFrame`), presented at the delivered size.
    ///
    /// # Errors
    /// `INVALID_INPUT` for planes that do not fit their format, or an unasked supply.
    #[wasm_bindgen(js_name = supplyYuvFrame)]
    #[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
    pub fn supply_yuv_frame(
        &mut self,
        pts_ticks: i64,
        format: &str,
        coded_width: u32,
        coded_height: u32,
        data: Vec<u8>,
        layout: &[u32],
        visible: &[u32],
        display_width: u32,
        display_height: u32,
        color: &str,
        chroma_siting: &str,
        rotation_degrees: u32,
        mirror: bool,
    ) -> Result<(), JsValue> {
        let Some(Need::SourceFrame { size, .. }) = self.need else {
            return Err(invalid_input("the session does not want a source frame"));
        };
        let yuv = webcodecs_yuv_format(format)?;
        let color: ColorMetadata = serde_json::from_str(color)
            .map_err(|error| invalid_input(format!("invalid frame color metadata: {error}")))?;
        if layout.len() != yuv.plane_count() * 2 || visible.len() != 4 {
            return Err(invalid_input("YUV frame layout does not match its format"));
        }
        let (chroma_width, chroma_rows) = yuv.chroma_size(coded_width, coded_height);
        let colors = yuv.color_plane_count();
        let data = bytes::Bytes::from(data);
        let mut planes = Vec::with_capacity(yuv.plane_count());
        for index in 0..yuv.plane_count() {
            let luma = index == 0 || index == colors;
            let (width, rows) = if luma {
                (coded_width, coded_height)
            } else {
                (chroma_width, chroma_rows)
            };
            let offset = usize::try_from(layout[index * 2]).unwrap_or(usize::MAX);
            let stride = layout[index * 2 + 1];
            let end = usize::try_from(u64::from(stride) * u64::from(rows))
                .ok()
                .and_then(|length| offset.checked_add(length))
                .filter(|end| *end <= data.len())
                .ok_or_else(|| invalid_input("YUV plane runs past the frame data"))?;
            planes.push(Plane {
                bytes: data.slice(offset..end),
                stride,
                width,
                height: rows,
            });
        }
        let sample_aspect_ratio = SampleAspectRatio::of_display(
            (visible[2], visible[3]),
            (display_width, display_height),
        )
        .ok_or_else(|| invalid_input("the frame's display size does not fit its picture"))?;
        let frame = DecodedVideoFrame {
            pts: MediaTime(pts_ticks),
            dts: None,
            duration: None,
            visible_rect: Rect {
                x: visible[0],
                y: visible[1],
                width: visible[2],
                height: visible[3],
            },
            sample_aspect_ratio,
            orientation: Orientation {
                rotation: rotation_from_degrees(rotation_degrees)?,
                mirror,
            },
            pixel_format: PixelFormat::Yuv(yuv),
            chroma_location: chroma_siting_named(chroma_siting)?,
            color: Some(color),
            planes,
        };
        let supplied =
            present_for_extraction(&self.renderer, Arc::new(frame), size).map_err(invalid_input)?;
        self.session()?.supply_video(supplied).map_err(refused)
    }

    /// Answers a frame need with straight-alpha sRGB RGBA8 pixels already at the delivered size,
    /// such as a rendered timeline frame.
    ///
    /// # Errors
    /// `INVALID_INPUT` for a supply the session did not ask for or of the wrong size.
    #[wasm_bindgen(js_name = supplyRgbaFrame)]
    pub fn supply_rgba_frame(
        &mut self,
        pts_ticks: i64,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    ) -> Result<(), JsValue> {
        self.session()?
            .supply_video(SuppliedFrame {
                pts: MediaTime(pts_ticks),
                width,
                height,
                rgba,
            })
            .map_err(refused)
    }

    /// Answers a source-frame need when the source has no such frame.
    ///
    /// # Errors
    /// `INVALID_INPUT` when the session did not ask for a source frame.
    #[wasm_bindgen(js_name = supplyEnd)]
    pub fn supply_end(&mut self) -> Result<(), JsValue> {
        self.session()?.supply_end().map_err(refused)
    }

    /// Answers an audio need: `channels` planes of the asked frame count, channel after channel.
    /// For a mono need, several channels are mixed down by `speakers` (a JSON array of `FFmpeg`
    /// channel names, such as `["FL","FR"]`) as the native hosts mix them.
    ///
    /// # Errors
    /// `INVALID_INPUT` for a supply the session did not ask for or of the wrong length, or
    /// channels that cannot be mixed down.
    #[wasm_bindgen(js_name = supplyAudio)]
    #[allow(clippy::needless_pass_by_value)]
    pub fn supply_audio(
        &mut self,
        channels: u32,
        samples: Vec<f32>,
        speakers: Option<String>,
    ) -> Result<(), JsValue> {
        let mono = matches!(
            self.need,
            Some(Need::SourceAudio {
                channels: AudioChannels::Mono,
                ..
            })
        );
        let supplied = if mono && channels > 1 {
            let names: Vec<String> = speakers
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(refused)?
                .unwrap_or_default();
            let speakers = names
                .iter()
                .map(|name| Speaker::from_name(name))
                .collect::<Option<Vec<_>>>()
                .filter(|speakers| speakers.len() == channels as usize)
                .ok_or_else(|| invalid_input("mono audio needs the source's speakers"))?;
            let frames = samples.len() / channels as usize;
            let planes = samples
                .chunks(frames.max(1))
                .map(<[f32]>::to_vec)
                .collect::<Vec<_>>();
            let mixed = mono_downmix(&speakers, &planes)
                .ok_or_else(|| invalid_input("these channels have no mono mix"))?;
            SuppliedAudio {
                channels: 1,
                samples: mixed,
            }
        } else {
            SuppliedAudio { channels, samples }
        };
        self.session()?.supply_audio(supplied).map_err(refused)
    }

    /// Takes the extractor's emits for the last delivery or `end`, in the host protocol's JSON.
    ///
    /// # Errors
    /// `INVALID_EMIT` with `{ index, reason }` for the first emit the session refuses.
    pub fn emit(&mut self, emits: &str) -> Result<(), JsValue> {
        let values: Vec<Value> = serde_json::from_str(emits).map_err(|error| {
            invalid_emit(0, &format!("the emits are not a JSON array: {error}"))
        })?;
        let emits = values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                emit_from_json(value).map_err(|reason| invalid_emit(index, &reason))
            })
            .collect::<Result<Vec<Emit>, _>>()?;
        self.session()?.emit(&emits).map_err(|error| match error {
            SessionError::InvalidEmit { index, reason } => invalid_emit(index, &reason),
            other => invalid_emit(0, &other.to_string()),
        })
    }

    /// Each pass's progress, JSON `[{ completed, total }]`.
    ///
    /// # Errors
    /// After `finish`.
    pub fn progress(&mut self) -> Result<String, JsValue> {
        let progress = self
            .session()?
            .progress()
            .iter()
            .map(|pass| json!({ "completed": pass.completed, "total": pass.total }))
            .collect::<Vec<_>>();
        Ok(Value::Array(progress).to_string())
    }

    /// The data sets, JSON; `made_from` (a fingerprint's JSON, or empty) is recorded on every
    /// source-anchored set. Ends the session.
    ///
    /// # Errors
    /// `INVALID_EMIT` when the run is not done or its result breaks a rule.
    pub fn finish(&mut self, made_from: Option<String>) -> Result<String, JsValue> {
        let made_from: Option<MediaFingerprint> = made_from
            .filter(|text| !text.is_empty())
            .map(|text| serde_json::from_str(&text))
            .transpose()
            .map_err(|error| invalid_input(format!("invalid made_from: {error}")))?;
        let mut session = self
            .session
            .take()
            .ok_or_else(|| invalid_input("the extraction has finished"))?;
        let mut sets = session
            .finish()
            .map_err(|error| invalid_emit(0, &error.to_string()))?;
        for set in &mut sets {
            if set.anchor.source().is_some() {
                set.made_from.clone_from(&made_from);
            }
        }
        serde_json::to_string(&sets).map_err(refused)
    }
}

/// `INVALID_EMIT`, with `details` `{ index, reason }` as the native host answers it.
fn invalid_emit(index: usize, reason: &str) -> JsValue {
    let error: JsValue = crate::error::BindingError::new(
        crate::error::Code::InvalidEmit,
        format!("emit {index} is invalid: {reason}"),
    )
    .into();
    let details = web_sys::js_sys::Object::new();
    let _ = web_sys::js_sys::Reflect::set(
        &details,
        &"index".into(),
        &JsValue::from(u32::try_from(index).unwrap_or(u32::MAX)),
    );
    let _ = web_sys::js_sys::Reflect::set(&details, &"reason".into(), &JsValue::from_str(reason));
    let _ = web_sys::js_sys::Reflect::set(&error, &"details".into(), &details);
    error
}

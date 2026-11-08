//! The `extract` operation: the host runs an extraction session and its supplier, and the client
//! runs the extractor, one step at a time over protocol frames.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use alpha2_engine::extract::{
    AudioInput, Delivery, Descriptor, Direction, Emit, ExtractRequest, ExtractSession, Need,
    PassProgress, SessionError, Step, SuppliedAudio, SuppliedFrame, Supplier, VideoInput,
};
use alpha2_host_core::host_frames::Attachment;
use alpha2_media::CancelToken;
use alpha2_model::{DataInterval, DataWord, MediaFingerprint, Project};
use alpha2_time::MediaTime;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::HostError;
use crate::PROTOCOL_VERSION;
use crate::assets::ProxyRequest;
use crate::host::Host;
use crate::protocol::{ExportMedia, ProjectJson};
use crate::serve_io::Responder;

/// How long a client may take to acknowledge a `pass`, `video` or `audio` step by default.
pub const DEFAULT_STEP_TIMEOUT: Duration = Duration::from_secs(60);

/// An `extract` request.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtractHostRequest {
    pub protocol_version: u32,
    pub request_id: String,
    pub operation: String,
    pub project: ProjectJson,
    pub assets: HashMap<String, PathBuf>,
    #[serde(default)]
    pub proxies: HashMap<String, Vec<ProxyRequest>>,
    #[serde(default)]
    pub frames: HashMap<String, Vec<Option<PathBuf>>>,
    #[serde(default)]
    pub resources: HashMap<String, HashMap<String, PathBuf>>,
    #[serde(default)]
    pub media: ExportMedia,
    #[serde(default)]
    pub effect_assets: HashMap<String, PathBuf>,
    #[serde(rename = "fonts", default)]
    pub font_files: Vec<PathBuf>,
    #[serde(default)]
    pub effects: Vec<crate::effect_request::EffectRequest>,
    /// The run, as `alpha2_engine::extract::ExtractRequest` writes it.
    pub request: ExtractRequest,
    /// The client-side extractor's descriptor.
    #[serde(default)]
    pub descriptor: Option<Descriptor>,
    /// A plugin the host runs itself.
    #[serde(default)]
    pub plugin: Option<Value>,
}

impl ExtractHostRequest {
    /// # Errors
    /// `INVALID_REQUEST` for another operation, or for neither or both of `descriptor` and
    /// `plugin`.
    pub fn check(&self) -> Result<(), HostError> {
        if self.operation != "extract" {
            return Err(HostError::new(
                "INVALID_REQUEST",
                "invalid request: the operation is not `extract`",
            ));
        }
        if self.descriptor.is_some() == self.plugin.is_some() {
            return Err(HostError::new(
                "INVALID_REQUEST",
                "an extract request has exactly one of `descriptor` and `plugin`",
            ));
        }
        Ok(())
    }
}

/// The acknowledgements of running extract requests, by request id; the request reader routes
/// each `extract.ack` here as it does a `cancel`.
fn acks() -> &'static Mutex<HashMap<String, Sender<String>>> {
    static ACKS: OnceLock<Mutex<HashMap<String, Sender<String>>>> = OnceLock::new();
    ACKS.get_or_init(Mutex::default)
}

/// Hands an `extract.ack` header to its request; an ack for no running request is dropped.
pub fn route_ack(request_id: &str, header: String) {
    let routes = acks().lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(route) = routes.get(request_id) {
        let _ = route.send(header);
    }
}

/// Removes a request's route when its run ends.
struct AckRoute(String);

impl Drop for AckRoute {
    fn drop(&mut self) {
        acks()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.0);
    }
}

fn register(request_id: &str) -> (AckRoute, Receiver<String>) {
    let (sender, receiver) = std::sync::mpsc::channel();
    acks()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(request_id.to_owned(), sender);
    (AckRoute(request_id.to_owned()), receiver)
}

/// One client acknowledgement.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Ack {
    seq: u64,
    #[serde(default)]
    emits: Vec<Value>,
    #[serde(default)]
    error: Option<AckError>,
}

#[derive(Deserialize)]
struct AckError {
    message: String,
}

/// An emit as the client writes it.
fn emit_of(value: &Value) -> Result<Emit, String> {
    let output = value
        .get("output")
        .and_then(Value::as_str)
        .ok_or("an emit needs `output`")?
        .to_owned();
    let tick = |name: &str| value.get(name).and_then(Value::as_i64).map(MediaTime);
    if let Some(values) = value.get("values") {
        let time = tick("time").ok_or("a sample needs an integer `time`")?;
        let values = values
            .as_array()
            .ok_or("`values` is an array")?
            .iter()
            .map(|value| match value {
                Value::Null => Ok(None),
                #[allow(clippy::cast_possible_truncation)]
                Value::Number(number) => number
                    .as_f64()
                    .map(|number| Some(number as f32))
                    .ok_or("a value is a number or null"),
                _ => Err("a value is a number or null"),
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Emit::Sample {
            output,
            time,
            values,
        });
    }
    let start = tick("start").ok_or("an item needs an integer `start`")?;
    let end = tick("end").ok_or("an item needs an integer `end`")?;
    let text = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
    if let Some(word) = text("text") {
        return Ok(Emit::Word {
            output,
            word: DataWord {
                start,
                end,
                text: word,
                speaker: text("speaker"),
            },
        });
    }
    #[allow(clippy::cast_possible_truncation)]
    let score = value
        .get("score")
        .and_then(Value::as_f64)
        .map(|score| score as f32);
    Ok(Emit::Interval {
        output,
        interval: DataInterval {
            start,
            end,
            label: text("label"),
            score,
        },
    })
}

/// Sends steps and waits for their acknowledgements.
struct Client<'a> {
    out: &'a Responder,
    request_id: &'a str,
    acks: Receiver<String>,
    seq: u64,
    timeout: Option<Duration>,
    cancel: &'a CancelToken,
    waited: Duration,
}

impl Client<'_> {
    /// Sends `step` and returns the client's emits, refusing an error ack, a timeout or a cancel.
    fn step(
        &mut self,
        mut step: Value,
        attachments: &[Attachment],
        timed: bool,
    ) -> Result<Vec<Emit>, HostError> {
        self.seq += 1;
        step["protocolVersion"] = json!(PROTOCOL_VERSION);
        step["type"] = json!("extract.step");
        step["requestId"] = json!(self.request_id);
        step["seq"] = json!(self.seq);
        self.out.send_with(&step, attachments)?;
        let started = Instant::now();
        let deadline = timed
            .then_some(self.timeout)
            .flatten()
            .map(|timeout| started + timeout);
        loop {
            if self.cancel.is_cancelled() {
                return Err(HostError::new(
                    "CANCELLED",
                    crate::protocol::cancelled("the extraction was cancelled"),
                ));
            }
            let wait = deadline.map_or(Duration::from_millis(100), |deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(100))
            });
            match self.acks.recv_timeout(wait) {
                Ok(header) => {
                    let ack: Ack = serde_json::from_str(&header).map_err(|error| {
                        HostError::new("INVALID_REQUEST", format!("invalid extract.ack: {error}"))
                    })?;
                    if ack.seq != self.seq {
                        continue;
                    }
                    self.waited += started.elapsed();
                    if let Some(error) = ack.error {
                        return Err(HostError::new(
                            "EXTRACTOR_FAILED",
                            format!("the extractor failed: {}", error.message),
                        ));
                    }
                    return ack
                        .emits
                        .iter()
                        .enumerate()
                        .map(|(index, emit)| {
                            emit_of(emit).map_err(|reason| invalid_emit(index, &reason))
                        })
                        .collect();
                }
                Err(RecvTimeoutError::Timeout) => {
                    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        return Err(HostError::new(
                            "EXTRACTOR_TIMEOUT",
                            format!(
                                "the client did not acknowledge step {} within {} ms",
                                self.seq,
                                self.timeout.map_or(0, |timeout| timeout.as_millis())
                            ),
                        ));
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(HostError::new(
                        "INTERNAL",
                        "the acknowledgement route closed",
                    ));
                }
            }
        }
    }
}

fn invalid_emit(index: usize, reason: &str) -> HostError {
    HostError::new("INVALID_EMIT", format!("emit {index} is invalid: {reason}"))
        .with_details(json!({ "index": index, "reason": reason }))
}

fn emit_into(session: &mut ExtractSession, emits: &[Emit]) -> Result<(), HostError> {
    session.emit(emits).map_err(|error| match error {
        SessionError::InvalidEmit { index, reason } => invalid_emit(index, &reason),
        other => HostError::new("INVALID_EMIT", other.to_string()),
    })
}

fn context_json(session: &ExtractSession) -> Value {
    let context = session.context();
    json!({
        "range": context.range,
        "passes": context.passes,
        "from": context.from,
        "video": context.video.map(|(size, format)| json!({ "size": size, "format": format })),
        "audio": context.audio.map(|(rate, channels, block)| json!({ "rate": rate, "channels": channels, "blockFrames": block })),
        "frameRate": context.frame_rate,
    })
}

fn progress_message(request_id: &str, pass: usize, progress: PassProgress) -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "type": "progress",
        "requestId": request_id,
        "phase": "extract",
        "pass": pass,
        "completed": progress.completed,
        "total": progress.total,
    })
}

fn video_step(frame: &VideoInput) -> (Value, Vec<Attachment>) {
    (
        json!({ "step": "video", "time": frame.time.ticks(), "width": frame.width, "height": frame.height, "format": frame.format }),
        vec![Attachment {
            kind: "application/octet-stream".to_owned(),
            bytes: frame.bytes.clone(),
        }],
    )
}

fn audio_step(block: &AudioInput) -> (Value, Vec<Attachment>) {
    let bytes = block
        .samples
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect();
    (
        json!({ "step": "audio", "start": block.start.ticks(), "startSample": block.start_sample, "frames": block.frames, "rate": block.rate, "channels": block.channels }),
        vec![Attachment {
            kind: "application/octet-stream".to_owned(),
            bytes,
        }],
    )
}

/// Runs a client-side extractor's session over `supplier`, one step outstanding at a time.
#[allow(clippy::too_many_arguments)]
fn run_client(
    mut session: ExtractSession,
    supplier: &mut dyn Supplier,
    out: &Responder,
    request_id: &str,
    timeout: Option<Duration>,
    cancel: &CancelToken,
) -> Result<(Vec<alpha2_model::DataSet>, Duration), HostError> {
    let (_route, acks) = register(request_id);
    let mut client = Client {
        out,
        request_id,
        acks,
        seq: 0,
        timeout,
        cancel,
        waited: Duration::ZERO,
    };
    let begin = client.step(
        json!({ "step": "begin", "context": context_json(&session) }),
        &[],
        false,
    )?;
    emit_into(&mut session, &begin)?;
    let mut pass = 0;
    loop {
        if cancel.is_cancelled() {
            return Err(HostError::new(
                "CANCELLED",
                crate::protocol::cancelled("the extraction was cancelled"),
            ));
        }
        match session.next() {
            Step::Need(need) => match &need {
                Need::SourceFrame { .. } | Need::TimelineFrame { .. } => {
                    match supplier.frame(&need).map_err(HostError::classify)? {
                        Some(frame) => session
                            .supply_video(frame)
                            .map_err(|error| HostError::new("INTERNAL", error.to_string()))?,
                        None => session
                            .supply_end()
                            .map_err(|error| HostError::new("INTERNAL", error.to_string()))?,
                    }
                }
                Need::SourceAudio { .. } | Need::TimelineAudio { .. } => {
                    let block = supplier.audio(&need).map_err(HostError::classify)?;
                    session
                        .supply_audio(block)
                        .map_err(|error| HostError::new("INTERNAL", error.to_string()))?;
                }
            },
            Step::PassBoundary {
                pass: index,
                direction,
            } => {
                pass = index;
                let direction = match direction {
                    Direction::Forward => "forward",
                    Direction::Backward => "backward",
                };
                let emits = client.step(
                    json!({ "step": "pass", "pass": index, "direction": direction }),
                    &[],
                    true,
                )?;
                emit_into(&mut session, &emits)?;
            }
            Step::Deliver(delivery) => {
                let (step, attachments) = match &delivery {
                    Delivery::Video(frame) => video_step(frame),
                    Delivery::Audio(block) => audio_step(block),
                };
                let emits = client.step(step, &attachments, true)?;
                emit_into(&mut session, &emits)?;
                if let Some(progress) = session.progress().get(pass) {
                    out.send(&progress_message(request_id, pass, *progress))?;
                }
            }
            Step::Done => break,
        }
    }
    let end = client.step(json!({ "step": "end" }), &[], false)?;
    emit_into(&mut session, &end)?;
    let sets = session
        .finish()
        .map_err(|error| HostError::new("INVALID_EMIT", error.to_string()))?;
    Ok((sets, client.waited))
}

/// The supplier of source needs from native decoders, and timeline frames from stills.
struct HostSupplier<'a> {
    reader: alpha2_host_core::extract::ReaderSupplier<
        'a,
        alpha2_native::NativeDecoder,
        alpha2_native::NativeAudioDecoder,
    >,
    timeline: Option<TimelineFrames<'a>>,
}

/// Timeline frames drawn through the host's still path.
struct TimelineFrames<'a> {
    host: &'a mut Host,
    project: String,
    assets: HashMap<String, PathBuf>,
    effect_assets: HashMap<String, PathBuf>,
    effects: Vec<crate::effect_request::EffectRequest>,
    canvas: (u32, u32),
}

impl TimelineFrames<'_> {
    fn frame(&mut self, time: MediaTime, size: u32) -> Result<SuppliedFrame, String> {
        let (width, height) = alpha2_engine::extract::delivered_size(self.canvas, size);
        let project = crate::host::still_project(
            &self.project,
            Some(crate::protocol::StillSize { width, height }),
        )
        .map_err(|error| error.message)?;
        let entry = crate::protocol::StillEntry {
            time: time.ticks(),
            output: None,
        };
        self.host.render_stills(
            &project,
            &self.assets,
            &self.effect_assets,
            self.effects.clone(),
            &[entry],
            false,
            alpha2_native::ExistingOutput::Refuse,
        )?;
        let png = std::mem::take(&mut self.host.reply_attachments)
            .pop()
            .ok_or_else(|| "a still drew nothing".to_owned())?;
        let decoder = png::Decoder::new(std::io::Cursor::new(png.bytes));
        let mut reader = decoder.read_info().map_err(|error| error.to_string())?;
        let mut rgba = vec![0; reader.output_buffer_size().ok_or("an oversized still")?];
        let info = reader
            .next_frame(&mut rgba)
            .map_err(|error| error.to_string())?;
        rgba.truncate(info.buffer_size());
        Ok(SuppliedFrame {
            pts: time,
            width,
            height,
            rgba,
        })
    }
}

impl Supplier for HostSupplier<'_> {
    fn frame(&mut self, need: &Need) -> Result<Option<SuppliedFrame>, String> {
        match need {
            Need::TimelineFrame { time, size, .. } => self
                .timeline
                .as_mut()
                .ok_or_else(|| "no timeline renderer".to_owned())?
                .frame(*time, *size)
                .map(Some),
            _ => self.reader.frame(need),
        }
    }

    fn audio(&mut self, need: &Need) -> Result<SuppliedAudio, String> {
        match need {
            Need::TimelineAudio { .. } => Err(crate::protocol::code_tag(
                "INVALID_INPUT",
                "this host does not supply timeline audio to extractors yet; extract from the source",
            )),
            _ => self.reader.audio(need),
        }
    }
}

/// Answers an `extract` request with a client-side extractor.
///
/// # Errors
/// The session's refusal of the request, a supplier or protocol failure, or the extractor's.
#[allow(clippy::too_many_lines)]
pub fn handle(
    host: &mut Host,
    request: ExtractHostRequest,
    out: &Responder,
    timeout: Option<Duration>,
) -> Result<Value, HostError> {
    let started = Instant::now();
    request.check()?;
    let ExtractHostRequest {
        request_id,
        project,
        assets,
        proxies,
        frames,
        resources,
        media,
        effect_assets,
        font_files,
        effects,
        request,
        descriptor,
        ..
    } = request;
    let Some(descriptor) = descriptor else {
        return Err(HostError::new(
            "INVALID_REQUEST",
            "this host runs plugins only through the `plugin` field it supports",
        ));
    };
    let parsed = Project::from_json(project.text())
        .map_err(|error| HostError::classify(crate::error_message(error)))?;
    let session = ExtractSession::new(&parsed, &descriptor, request.clone()).map_err(|error| {
        HostError::new(
            "INVALID_REQUEST",
            format!("invalid extract request: {error}"),
        )
    })?;
    let (paths, original) = host.extract_media(
        project.text(),
        &assets,
        &proxies,
        &frames,
        &resources,
        &effect_assets,
        &font_files,
        media,
        request.input.source_id.as_ref(),
    )?;
    let cancel = crate::termination::export_cancel();
    let renderer = alpha2_render::CpuRenderer::new(1);
    let source = request.input.source_id.clone();
    let path = |id: &alpha2_media::SourceId| paths.get(id.as_str()).cloned();
    let video = match (&source, &descriptor.input) {
        (Some(id), alpha2_engine::extract::InputSpec::Video { .. }) => {
            let path = path(id).ok_or_else(|| {
                HostError::new("MISSING_INPUT", format!("no media for source {id}"))
            })?;
            let decoder = alpha2_native::NativeDecoder::open(&path)
                .map_err(|error| HostError::classify(error.to_string()))?;
            Some(alpha2_host_core::extract::VideoReader::new(
                decoder,
                id.clone(),
                (*cancel).clone(),
            ))
        }
        _ => None,
    };
    let audio = match (&source, session.context().audio) {
        (Some(id), Some((rate, _, _))) => {
            let path = path(id).ok_or_else(|| {
                HostError::new("MISSING_INPUT", format!("no media for source {id}"))
            })?;
            let decoder =
                alpha2_native::NativeAudioDecoder::open_stream(&path, rate, request.input.stream)
                    .map_err(|error| HostError::classify(error.to_string()))?;
            Some(alpha2_host_core::extract::AudioReader::new(
                decoder,
                id.clone(),
                rate,
                (*cancel).clone(),
            ))
        }
        _ => None,
    };
    let canvas = (parsed.settings.width, parsed.settings.height);
    let timeline = source.is_none().then(|| TimelineFrames {
        host: &mut *host,
        project: project.text().to_owned(),
        assets: paths.clone(),
        effect_assets: effect_assets.clone(),
        effects,
        canvas,
    });
    let mut supplier = HostSupplier {
        reader: alpha2_host_core::extract::ReaderSupplier::new(&renderer, video, audio),
        timeline,
    };
    let (mut sets, waited) =
        run_client(session, &mut supplier, out, &request_id, timeout, &cancel)?;
    for set in &mut sets {
        if set.anchor.source().is_some() {
            set.made_from.clone_from(&original);
        }
    }
    let total = started.elapsed();
    Ok(json!({
        "dataSets": sets,
        "timings": {
            "totalMs": total.as_secs_f64() * 1_000.0,
            "clientMs": waited.as_secs_f64() * 1_000.0,
        },
    }))
}

/// The fingerprint `made_from` records: the conformed original's.
pub fn original_fingerprint(
    catalog: &alpha2_assets::AssetCatalog,
    source: &alpha2_media::SourceId,
) -> Option<MediaFingerprint> {
    match &catalog.entry(source)?.original {
        alpha2_assets::OriginalState::Online { fingerprint } => Some(fingerprint.clone()),
        _ => None,
    }
}

//! How `serve` reads and writes its frames ([`alpha2_host_core::host_frames`]).
//!
//! The `alpha2-requests` thread owns stdin: it answers each `cancel` and `status` request at once
//! and queues every other frame for the request thread, so a cancel reaches a running export
//! without waiting. Every frame `serve` writes goes through one [`Responder`], which writes it
//! whole, its header as UTF-8 JSON.

use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use alpha2_host_core::host_frames::{
    Attachment, FrameLimits, MAGIC, ReadFrame, frame_bytes, read_frame, split_attachments,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::PROTOCOL_VERSION;
use crate::lifecycle::Lifecycle;
use crate::protocol::{
    HostError, MAX_CONTROL_HEADER_BYTES, MAX_REQUEST_ATTACHMENT_BYTES, MAX_REQUEST_HEADER_BYTES,
    MAX_RESPONSE_HEADER_BYTES, parse_cancel, parse_status,
};

/// Past this the reader stops reading until a request is taken, so a cancel behind that much
/// queued input is read late.
const MAX_QUEUED_BYTES: usize = 256 * 1024 * 1024;

const REQUEST_LIMITS: FrameLimits = FrameLimits {
    header_bytes: MAX_REQUEST_HEADER_BYTES,
    attachment_bytes: MAX_REQUEST_ATTACHMENT_BYTES,
};

/// JSON with every non-ASCII character escaped as `\uXXXX`.
///
/// `PowerShell` 5.1 decodes a native program's output in the console's OEM code page, where raw
/// UTF-8 would come out garbled.
pub(crate) fn json_line(value: &Value) -> Vec<u8> {
    let mut line = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut line, AsciiFormatter);
    // A `Value` always serializes: its keys are strings and its numbers finite.
    let _ = value.serialize(&mut serializer);
    line
}

struct AsciiFormatter;

impl serde_json::ser::Formatter for AsciiFormatter {
    fn write_string_fragment<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> std::io::Result<()> {
        let mut ascii_from = 0;
        for (at, character) in fragment.char_indices() {
            if character.is_ascii() {
                continue;
            }
            writer.write_all(&fragment.as_bytes()[ascii_from..at])?;
            let mut units = [0_u16; 2];
            for unit in character.encode_utf16(&mut units) {
                write!(writer, "\\u{unit:04x}")?;
            }
            ascii_from = at + character.len_utf8();
        }
        writer.write_all(&fragment.as_bytes()[ascii_from..])
    }
}

/// The protocol stream, or a test's stand-in, shared by every thread that writes a frame.
pub(crate) struct Responder {
    output: Mutex<Box<dyn Write + Send>>,
    header_limit: usize,
}

impl Responder {
    pub(crate) fn new(output: Box<dyn Write + Send>) -> Self {
        Self::with_header_limit(output, MAX_RESPONSE_HEADER_BYTES as usize)
    }

    pub(crate) const fn with_header_limit(
        output: Box<dyn Write + Send>,
        header_limit: usize,
    ) -> Self {
        Self {
            output: Mutex::new(output),
            header_limit,
        }
    }

    /// `header` as UTF-8 JSON, or `RESOURCE_LIMIT` when it is longer than a response header may
    /// be.
    fn header_bytes(&self, header: &Value) -> Result<Vec<u8>, HostError> {
        let line = serde_json::to_vec(header).map_err(HostError::json)?;
        if line.len() > self.header_limit {
            return Err(HostError::new(
                "RESOURCE_LIMIT",
                format!(
                    "the response header is {} bytes, more than the {} a response header may be",
                    line.len(),
                    self.header_limit
                ),
            )
            .with_details(json!({"headerBytes": line.len(), "limit": self.header_limit})));
        }
        Ok(line)
    }

    /// Standard output, taken over so that nothing else this process prints reaches it
    /// ([`alpha2_os::claim_stdout`]).
    pub(crate) fn claimed_stdout() -> Self {
        match alpha2_os::claim_stdout() {
            Ok(stream) => Self::new(Box::new(stream)),
            Err(error) => {
                alpha2_os::diagnostic!(
                    "alpha2-host: standard output is shared with native code: {error}"
                );
                Self::new(Box::new(std::io::stdout()))
            }
        }
    }

    /// Writes [`MAGIC`], before the first frame.
    ///
    /// # Errors
    /// When the output refuses it.
    pub(crate) fn start(&self) -> Result<(), HostError> {
        self.write(&MAGIC)
    }

    /// # Errors
    /// When the output refuses the frame (a client that closed the pipe).
    pub(crate) fn send(&self, header: &Value) -> Result<(), HostError> {
        self.send_with(header, &[])
    }

    /// Lists `attachments` in the header and writes the frame in one piece, so another thread's
    /// frame never lands inside it.
    ///
    /// # Errors
    /// When the output refuses the frame, or the attachments exceed a frame. `RESOURCE_LIMIT`,
    /// with nothing written, when the header is longer than a response header may be.
    pub(crate) fn send_with(
        &self,
        header: &Value,
        attachments: &[Attachment],
    ) -> Result<(), HostError> {
        let frame = if attachments.is_empty() {
            frame_bytes(&self.header_bytes(header)?, &[])
        } else {
            let mut header = header.clone();
            header["attachments"] = attachments
                .iter()
                .map(|attachment| json!({"type": attachment.kind, "bytes": attachment.bytes.len()}))
                .collect();
            let parts = attachments
                .iter()
                .map(|attachment| attachment.bytes.as_slice())
                .collect::<Vec<_>>();
            frame_bytes(&self.header_bytes(&header)?, &parts)
        };
        self.write(&frame.map_err(HostError::io)?)
    }

    /// [`Self::send`], or `None` without writing when another thread holds the output.
    pub(crate) fn try_send(&self, header: &Value) -> Option<Result<(), HostError>> {
        let mut output = match self.output.try_lock() {
            Ok(output) => output,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        let frame = match self.header_bytes(header) {
            Ok(line) => frame_bytes(&line, &[]).map_err(HostError::io),
            Err(error) => Err(error),
        };
        let frame = match frame {
            Ok(frame) => frame,
            Err(error) => return Some(Err(error)),
        };
        Some(
            output
                .write_all(&frame)
                .and_then(|()| output.flush())
                .map_err(HostError::io),
        )
    }

    fn write(&self, bytes: &[u8]) -> Result<(), HostError> {
        let mut output = self.output.lock().unwrap_or_else(PoisonError::into_inner);
        output.write_all(bytes).map_err(HostError::io)?;
        output.flush().map_err(HostError::io)
    }
}

/// What `serve` writes first, [`MAGIC`] and its `ready` state, before anything that can take long.
pub(crate) fn write_ready(out: &Responder, lifecycle: &Lifecycle) -> Result<(), HostError> {
    out.start()?;
    out.send(&lifecycle.state_message())
}

pub(crate) enum InputFrame {
    /// A header in UTF-8, and its attachments or why they do not match the header.
    Request {
        header: String,
        attachments: Result<Vec<Attachment>, String>,
    },
    /// A frame of this many bytes, over a limit of [`REQUEST_LIMITS`].
    TooLarge(u64),
    NotUtf8,
    /// Stdin ended.
    End,
    /// Reading stdin failed, or the reader panicked: what was written after is lost.
    Failed(HostError),
}

impl InputFrame {
    fn queued_bytes(&self) -> usize {
        match self {
            Self::Request {
                header,
                attachments,
            } => {
                header.len()
                    + attachments.as_ref().map_or(0, |attachments| {
                        attachments.iter().map(|part| part.bytes.len()).sum()
                    })
            }
            Self::TooLarge(_) | Self::NotUtf8 | Self::End | Self::Failed(_) => 0,
        }
    }
}

#[derive(Default)]
struct Queued {
    bytes: Mutex<usize>,
    taken: Condvar,
}

pub(crate) struct FrameSender {
    sender: Sender<InputFrame>,
    queued: Arc<Queued>,
}

pub(crate) struct Requests {
    receiver: Receiver<InputFrame>,
    queued: Arc<Queued>,
}

pub(crate) fn request_queue() -> (FrameSender, Requests) {
    let (sender, receiver) = std::sync::mpsc::channel();
    let queued = Arc::new(Queued::default());
    (
        FrameSender {
            sender,
            queued: Arc::clone(&queued),
        },
        Requests { receiver, queued },
    )
}

impl FrameSender {
    /// Waits while the queue holds more than [`MAX_QUEUED_BYTES`]. Returns `false` when the
    /// request thread is gone.
    pub(crate) fn send(&self, frame: InputFrame) -> bool {
        let bytes = frame.queued_bytes();
        let mut queued = self
            .queued
            .bytes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while *queued > 0 && queued.saturating_add(bytes) > MAX_QUEUED_BYTES {
            queued = self
                .queued
                .taken
                .wait(queued)
                .unwrap_or_else(PoisonError::into_inner);
        }
        *queued += bytes;
        drop(queued);
        self.sender.send(frame).is_ok()
    }
}

impl Requests {
    pub(crate) fn next(&self) -> InputFrame {
        let Ok(frame) = self.receiver.recv() else {
            return InputFrame::End;
        };
        self.taken(frame)
    }

    /// `None` when no frame arrived within `limit`.
    pub(crate) fn next_within(&self, limit: Duration) -> Option<InputFrame> {
        match self.receiver.recv_timeout(limit) {
            Ok(frame) => Some(self.taken(frame)),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Some(InputFrame::End),
        }
    }

    /// A frame already read, without waiting for one.
    pub(crate) fn queued(&self) -> Option<InputFrame> {
        self.receiver.try_recv().ok().map(|frame| self.taken(frame))
    }

    fn taken(&self, frame: InputFrame) -> InputFrame {
        let mut queued = self
            .queued
            .bytes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *queued = queued.saturating_sub(frame.queued_bytes());
        self.queued.taken.notify_all();
        frame
    }
}

/// Only the `attachments` field is read; the rest of the header is skipped, not parsed into a tree.
#[derive(serde::Deserialize)]
struct Declared {
    #[serde(default)]
    attachments: Value,
}

fn input_frame(frame: alpha2_host_core::host_frames::Frame) -> InputFrame {
    let Ok(header) = String::from_utf8(frame.header) else {
        return InputFrame::NotUtf8;
    };
    let attachments = if frame.attachments.is_empty() {
        Ok(Vec::new())
    } else {
        serde_json::from_str::<Declared>(&header)
            .map_err(|error| format!("invalid request: {error}"))
            .and_then(|declared| {
                split_attachments(
                    &json!({ "attachments": declared.attachments }),
                    frame.attachments,
                )
            })
    };
    InputFrame::Request {
        header,
        attachments,
    }
}

pub(crate) fn read_requests(
    input: &mut impl Read,
    frames: &FrameSender,
    out: &Responder,
    lifecycle: &Lifecycle,
) {
    loop {
        let frame = match read_frame(input, REQUEST_LIMITS) {
            Ok(ReadFrame::Frame(frame)) => input_frame(frame),
            Ok(ReadFrame::TooLarge {
                header_bytes,
                attachment_bytes,
            }) => InputFrame::TooLarge(u64::from(header_bytes) + u64::from(attachment_bytes)),
            Ok(ReadFrame::End) => {
                // No acknowledgement can arrive any more.
                crate::extract_request::close_ack_routes();
                frames.send(InputFrame::End);
                return;
            }
            Err(error) => {
                crate::extract_request::close_ack_routes();
                frames.send(InputFrame::Failed(HostError::new(
                    "IO_ERROR",
                    format!("reading requests failed: {error}"),
                )));
                return;
            }
        };
        // Acknowledgements go to their running extract request, never the request queue.
        if let InputFrame::Request {
            header,
            attachments: Ok(attachments),
        } = &frame
            && attachments.is_empty()
            && crate::protocol::operation_from_header(header).as_deref() == Some("extract.ack")
        {
            let request_id = crate::protocol::request_id_from_header(header);
            crate::extract_request::route_ack(&request_id, header.clone());
            continue;
        }
        if let InputFrame::Request {
            header,
            attachments: Ok(attachments),
        } = &frame
            && attachments.is_empty()
            && header.len() <= MAX_CONTROL_HEADER_BYTES
        {
            let answer = match crate::protocol::operation_from_header(header).as_deref() {
                Some("cancel") => Some(cancel_answer(header)),
                Some("status") => Some(status_answer(header, lifecycle)),
                _ => None,
            };
            if let Some(mut answer) = answer {
                answer["hostState"] = json!(lifecycle.state().as_str());
                let _ = out.send(&answer);
                continue;
            }
        }
        if !frames.send(frame) {
            return;
        }
    }
}

fn cancel_answer(header: &str) -> Value {
    match parse_cancel(header) {
        Ok(cancel) => {
            let state = crate::termination::terminations().cancel_request(&cancel.target);
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "requestId": cancel.request_id,
                "ok": true,
                "result": { "target": cancel.target, "state": state },
            })
        }
        Err(error) => json!({
            "protocolVersion": PROTOCOL_VERSION,
            "requestId": crate::protocol::request_id_from_header(header),
            "ok": false,
            "error": HostError::from(error),
        }),
    }
}

fn status_answer(header: &str, lifecycle: &Lifecycle) -> Value {
    match parse_status(header) {
        Ok(request_id) => json!({
            "protocolVersion": PROTOCOL_VERSION,
            "requestId": request_id,
            "ok": true,
            "result": lifecycle.status(),
        }),
        Err(error) => json!({
            "protocolVersion": PROTOCOL_VERSION,
            "requestId": crate::protocol::request_id_from_header(header),
            "ok": false,
            "error": HostError::from(error),
        }),
    }
}

pub(crate) fn start_reader(
    out: Arc<Responder>,
    lifecycle: Arc<Lifecycle>,
) -> Result<Requests, HostError> {
    let (sender, requests) = request_queue();
    std::thread::Builder::new()
        .name("alpha2-requests".to_owned())
        .spawn(move || {
            read_requests_guarded(&mut std::io::stdin().lock(), &sender, &out, &lifecycle);
        })
        .map_err(HostError::io)?;
    Ok(requests)
}

/// [`read_requests`], whose panic ends the input as a failure the dispatcher reports.
fn read_requests_guarded(
    input: &mut impl Read,
    frames: &FrameSender,
    out: &Responder,
    lifecycle: &Lifecycle,
) {
    let read = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        read_requests(input, frames, out, lifecycle);
    }));
    if let Err(payload) = read {
        crate::extract_request::close_ack_routes();
        frames.send(InputFrame::Failed(HostError::new(
            "INTERNAL",
            format!(
                "the request reader panicked: {}",
                crate::failure::panic_message(&*payload)
            ),
        )));
    }
}

#[cfg(test)]
pub(crate) fn requests_of(input: &[u8], out: &Responder, lifecycle: &Lifecycle) -> Requests {
    requests_from(&mut &input[..], out, lifecycle)
}

/// [`requests_of`] from any reader.
#[cfg(test)]
pub(crate) fn requests_from(
    input: &mut impl Read,
    out: &Responder,
    lifecycle: &Lifecycle,
) -> Requests {
    let (sender, requests) = request_queue();
    read_requests_guarded(input, &sender, out, lifecycle);
    requests
}

/// One request frame per line of `lines`, for tests written as JSON lines.
#[cfg(test)]
pub(crate) fn frames_of_lines(lines: &[u8]) -> Vec<u8> {
    let mut frames = Vec::new();
    for line in lines.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        frames.extend(frame_bytes(line, &[]).expect("a test frame"));
    }
    frames
}

/// Each frame's header and attachments, from bytes `serve` wrote after its [`MAGIC`].
#[cfg(test)]
pub(crate) fn messages_of(mut output: &[u8]) -> Vec<(Value, Vec<Attachment>)> {
    let limits = FrameLimits {
        header_bytes: u32::MAX,
        attachment_bytes: u32::MAX,
    };
    let mut messages = Vec::new();
    while let ReadFrame::Frame(frame) = read_frame(&mut output, limits).expect("a slice reads") {
        let header: Value = serde_json::from_slice(&frame.header).expect("a header is JSON");
        let attachments = split_attachments(&header, frame.attachments).expect("attachments");
        messages.push((header, attachments));
    }
    assert!(
        output.is_empty(),
        "{} bytes after the last frame",
        output.len()
    );
    messages
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct Captured(Arc<Mutex<Vec<u8>>>);

#[cfg(test)]
impl Captured {
    pub(crate) fn responder(&self) -> Responder {
        Responder::new(Box::new(self.clone()))
    }

    pub(crate) fn responder_with_header_limit(&self, limit: usize) -> Responder {
        Responder::with_header_limit(Box::new(self.clone()), limit)
    }

    pub(crate) fn bytes(&self) -> Vec<u8> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[cfg(test)]
impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

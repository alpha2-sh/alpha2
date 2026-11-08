//! How `serve` reads and writes its lines.
//!
//! The `alpha2-requests` thread owns stdin: it answers each `cancel` and `status` line at once and
//! queues every other line for the request thread, so a cancel reaches a running export without
//! waiting.
//! Every stdout line goes through one [`Responder`], which writes it whole.

use std::io::{BufRead, Write};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};

use crate::PROTOCOL_VERSION;
use crate::lifecycle::Lifecycle;
use crate::protocol::{
    HostError, MAX_CANCEL_LINE_BYTES, MAX_REQUEST_LINE_BYTES, parse_cancel, parse_status,
};

/// Past this the reader stops reading until a line is taken, so a cancel behind that much queued
/// input is read late.
const MAX_QUEUED_BYTES: usize = 256 * 1024 * 1024;

/// One line of JSON, newline excluded, with every non-ASCII character escaped as `\uXXXX`.
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

/// Stdout, or a test's stand-in, shared by every thread that writes a line.
pub(crate) struct Responder(Mutex<Box<dyn Write + Send>>);

impl Responder {
    pub(crate) fn new(output: Box<dyn Write + Send>) -> Self {
        Self(Mutex::new(output))
    }

    pub(crate) fn stdout() -> Self {
        Self::new(Box::new(std::io::stdout()))
    }

    /// The line is serialized first, so another thread's line never lands inside it.
    ///
    /// # Errors
    /// When the output refuses the line (a client that closed the pipe).
    pub(crate) fn line(&self, value: &Value) -> Result<(), HostError> {
        let mut line = json_line(value);
        line.push(b'\n');
        let mut output = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        output.write_all(&line).map_err(HostError::io)?;
        output.flush().map_err(HostError::io)
    }
}

/// The first line `serve` writes, its `ready` state, before anything that can take long.
pub(crate) fn write_ready(out: &Responder, lifecycle: &Lifecycle) -> Result<(), HostError> {
    out.line(&lifecycle.state_message())
}

pub(crate) enum InputLine {
    Line(String),
    /// A line of this many bytes, over [`MAX_REQUEST_LINE_BYTES`].
    TooLong(usize),
    NotUtf8,
    /// Stdin ended or failed.
    End,
}

impl InputLine {
    fn queued_bytes(&self) -> usize {
        match self {
            Self::Line(line) => line.len(),
            Self::TooLong(_) | Self::NotUtf8 | Self::End => 0,
        }
    }
}

#[derive(Default)]
struct Queued {
    bytes: Mutex<usize>,
    taken: Condvar,
}

pub(crate) struct LineSender {
    sender: Sender<InputLine>,
    queued: Arc<Queued>,
}

pub(crate) struct RequestLines {
    receiver: Receiver<InputLine>,
    queued: Arc<Queued>,
}

pub(crate) fn request_queue() -> (LineSender, RequestLines) {
    let (sender, receiver) = std::sync::mpsc::channel();
    let queued = Arc::new(Queued::default());
    (
        LineSender {
            sender,
            queued: Arc::clone(&queued),
        },
        RequestLines { receiver, queued },
    )
}

impl LineSender {
    /// Waits while the queue holds more than [`MAX_QUEUED_BYTES`]. Returns `false` when the
    /// request thread is gone.
    fn send(&self, line: InputLine) -> bool {
        let bytes = line.queued_bytes();
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
        self.sender.send(line).is_ok()
    }
}

impl RequestLines {
    pub(crate) fn next(&self) -> InputLine {
        let Ok(line) = self.receiver.recv() else {
            return InputLine::End;
        };
        self.taken(line)
    }

    /// `None` when no line arrived within `limit`.
    pub(crate) fn next_within(&self, limit: Duration) -> Option<InputLine> {
        match self.receiver.recv_timeout(limit) {
            Ok(line) => Some(self.taken(line)),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Some(InputLine::End),
        }
    }

    /// A line already read, without waiting for one.
    pub(crate) fn queued(&self) -> Option<InputLine> {
        self.receiver.try_recv().ok().map(|line| self.taken(line))
    }

    fn taken(&self, line: InputLine) -> InputLine {
        let mut queued = self
            .queued
            .bytes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *queued = queued.saturating_sub(line.queued_bytes());
        self.queued.taken.notify_all();
        line
    }
}

pub(crate) fn read_requests(
    input: &mut impl BufRead,
    lines: &LineSender,
    out: &Responder,
    lifecycle: &Lifecycle,
) {
    let mut buffer = Vec::new();
    loop {
        let line = match read_request_line(input, MAX_REQUEST_LINE_BYTES, &mut buffer) {
            Ok(RequestLine::Line(line)) => {
                if line.len() <= MAX_CANCEL_LINE_BYTES {
                    let answer = match crate::protocol::operation_from_line(line).as_deref() {
                        Some("cancel") => Some(cancel_answer(line)),
                        Some("status") => Some(status_answer(line, lifecycle)),
                        _ => None,
                    };
                    if let Some(mut answer) = answer {
                        answer["hostState"] = json!(lifecycle.state().as_str());
                        let _ = out.line(&answer);
                        continue;
                    }
                }
                InputLine::Line(line.to_owned())
            }
            Ok(RequestLine::TooLong(bytes)) => InputLine::TooLong(bytes),
            Ok(RequestLine::NotUtf8) => InputLine::NotUtf8,
            // A read error ends the input as its end would.
            Ok(RequestLine::End) | Err(_) => {
                lines.send(InputLine::End);
                return;
            }
        };
        if !lines.send(line) {
            return;
        }
    }
}

fn cancel_answer(line: &str) -> Value {
    match parse_cancel(line) {
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
            "requestId": crate::protocol::request_id_from_line(line),
            "ok": false,
            "error": error,
        }),
    }
}

fn status_answer(line: &str, lifecycle: &Lifecycle) -> Value {
    match parse_status(line) {
        Ok(request_id) => json!({
            "protocolVersion": PROTOCOL_VERSION,
            "requestId": request_id,
            "ok": true,
            "result": lifecycle.status(),
        }),
        Err(error) => json!({
            "protocolVersion": PROTOCOL_VERSION,
            "requestId": crate::protocol::request_id_from_line(line),
            "ok": false,
            "error": error,
        }),
    }
}

pub(crate) fn start_reader(
    out: Arc<Responder>,
    lifecycle: Arc<Lifecycle>,
) -> Result<RequestLines, HostError> {
    let (sender, lines) = request_queue();
    std::thread::Builder::new()
        .name("alpha2-requests".to_owned())
        .spawn(move || read_requests(&mut std::io::stdin().lock(), &sender, &out, &lifecycle))
        .map_err(HostError::io)?;
    Ok(lines)
}

#[cfg(test)]
pub(crate) fn lines_of(input: &[u8], out: &Responder, lifecycle: &Lifecycle) -> RequestLines {
    let (sender, lines) = request_queue();
    read_requests(&mut std::io::BufReader::new(input), &sender, out, lifecycle);
    lines
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct Captured(Arc<Mutex<Vec<u8>>>);

#[cfg(test)]
impl Captured {
    pub(crate) fn responder(&self) -> Responder {
        Responder::new(Box::new(self.clone()))
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

pub(crate) enum RequestLine<'a> {
    Line(&'a str),
    /// Skipped through its end, so the next line is read whole.
    TooLong(usize),
    NotUtf8,
    End,
}

/// Reads the next line without its ending, holding at most `limit` bytes. A longer line is read
/// through and discarded so the request after it is read whole. The last line need not end in a
/// newline.
pub(crate) fn read_request_line<'a>(
    input: &mut impl BufRead,
    limit: usize,
    buffer: &'a mut Vec<u8>,
) -> std::io::Result<RequestLine<'a>> {
    buffer.clear();
    let mut length = 0_usize;
    let mut ended = false;
    loop {
        let available = match input.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            break;
        }
        let (taken, newline) = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or((available.len(), false), |at| (at + 1, true));
        let content = &available[..taken - usize::from(newline)];
        length = length.saturating_add(content.len());
        if length <= limit {
            buffer.extend_from_slice(content);
        }
        input.consume(taken);
        ended = true;
        if newline {
            break;
        }
    }
    if !ended {
        return Ok(RequestLine::End);
    }
    if length > limit {
        buffer.clear();
        return Ok(RequestLine::TooLong(length));
    }
    if buffer.last() == Some(&b'\r') {
        buffer.pop();
    }
    Ok(std::str::from_utf8(buffer).map_or(RequestLine::NotUtf8, RequestLine::Line))
}

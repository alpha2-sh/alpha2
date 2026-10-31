//! How `serve` reads and writes its lines.
//!
//! One thread, `alpha2-requests`, owns standard input: it answers each
//! `cancel` line at once and queues every other line for the thread that runs
//! requests, so a cancel reaches a running export without waiting for it.
//! Every line on standard output, an answer, a progress report or the reader's
//! own, goes through one [`Responder`], which writes it whole.

use std::io::{BufRead, Write};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use serde::Serialize;
use serde_json::{Value, json};

use crate::PROTOCOL_VERSION;
use crate::protocol::{HostError, MAX_CANCEL_LINE_BYTES, MAX_REQUEST_LINE_BYTES, parse_cancel};

/// The most bytes of request lines the reader holds for the request thread.
/// Past it the reader stops reading until the request thread takes a line, so
/// a cancel written behind that much queued input is read late.
const MAX_QUEUED_BYTES: usize = 256 * 1024 * 1024;

/// `value` as one line of JSON, newline excluded, with every character past
/// ASCII written as a `\uXXXX` escape (two for one outside the Basic
/// Multilingual Plane).
///
/// A reader that decodes the host's output in a legacy code page (`PowerShell`
/// 5.1 takes the console's OEM code page for a native program's output) reads
/// an ASCII line as written, where raw UTF-8 would come out garbled.
pub(crate) fn json_line(value: &Value) -> Vec<u8> {
    let mut line = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut line, AsciiFormatter);
    // A `Value` always serializes: its keys are strings and its numbers finite.
    let _ = value.serialize(&mut serializer);
    line
}

/// Compact JSON whose strings carry only ASCII.
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

/// Standard output, or what stands in for it in a test, shared by every
/// thread that writes a line.
pub(crate) struct Responder(Mutex<Box<dyn Write + Send>>);

impl Responder {
    pub(crate) fn new(output: Box<dyn Write + Send>) -> Self {
        Self(Mutex::new(output))
    }

    /// The process's standard output.
    pub(crate) fn stdout() -> Self {
        Self::new(Box::new(std::io::stdout()))
    }

    /// Writes `value` as one line and flushes it. The line is serialized
    /// first, so another thread's line never lands inside it.
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

/// The line that tells a client the host is running and reading its input:
/// the first line `serve` writes, before anything that can take long.
pub(crate) fn write_ready(out: &Responder) -> Result<(), HostError> {
    out.line(&json!({ "protocolVersion": PROTOCOL_VERSION, "type": "ready" }))
}

/// One line the reader took from standard input, for the request thread.
pub(crate) enum InputLine {
    Line(String),
    /// A line of this many bytes, over [`MAX_REQUEST_LINE_BYTES`].
    TooLong(usize),
    NotUtf8,
    /// Standard input ended (or failed): no more requests.
    End,
}

impl InputLine {
    /// What the line counts against [`MAX_QUEUED_BYTES`].
    fn queued_bytes(&self) -> usize {
        match self {
            Self::Line(line) => line.len(),
            Self::TooLong(_) | Self::NotUtf8 | Self::End => 0,
        }
    }
}

/// The bytes of lines queued and not yet taken.
#[derive(Default)]
struct Queued {
    bytes: Mutex<usize>,
    taken: Condvar,
}

/// The reader's end of the request queue.
pub(crate) struct LineSender {
    sender: Sender<InputLine>,
    queued: Arc<Queued>,
}

/// The request thread's end of the request queue.
pub(crate) struct RequestLines {
    receiver: Receiver<InputLine>,
    queued: Arc<Queued>,
}

/// A request queue, bounded by [`MAX_QUEUED_BYTES`].
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
    /// Queues `line`, first waiting while the queue holds more than
    /// [`MAX_QUEUED_BYTES`] with it. Returns `false` when the request thread
    /// is gone.
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
    /// The next line, waiting for one; [`InputLine::End`] once the reader has
    /// ended.
    pub(crate) fn next(&self) -> InputLine {
        let Ok(line) = self.receiver.recv() else {
            return InputLine::End;
        };
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

/// Reads request lines from `input` until it ends: answers each `cancel` on
/// `out` at once ([`crate::termination::Terminations::cancel_request`]) and
/// queues the rest on `lines`, then queues [`InputLine::End`].
pub(crate) fn read_requests(input: &mut impl BufRead, lines: &LineSender, out: &Responder) {
    let mut buffer = Vec::new();
    loop {
        let line = match read_request_line(input, MAX_REQUEST_LINE_BYTES, &mut buffer) {
            Ok(RequestLine::Line(line)) => {
                if line.len() <= MAX_CANCEL_LINE_BYTES
                    && crate::protocol::operation_from_line(line).as_deref() == Some("cancel")
                {
                    // A client that has gone has nobody to read the answer.
                    let _ = out.line(&cancel_answer(line));
                    continue;
                }
                InputLine::Line(line.to_owned())
            }
            Ok(RequestLine::TooLong(bytes)) => InputLine::TooLong(bytes),
            Ok(RequestLine::NotUtf8) => InputLine::NotUtf8,
            // A read error ends the input as its end would: nothing more can come.
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

/// The answer to the cancel `line`.
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

/// Starts the `alpha2-requests` thread on standard input.
///
/// # Errors
/// When the thread cannot be started.
pub(crate) fn start_reader(out: Arc<Responder>) -> Result<RequestLines, HostError> {
    let (sender, lines) = request_queue();
    std::thread::Builder::new()
        .name("alpha2-requests".to_owned())
        .spawn(move || read_requests(&mut std::io::stdin().lock(), &sender, &out))
        .map_err(HostError::io)?;
    Ok(lines)
}

/// The lines of `input`, read on this thread as the reader thread reads
/// them: cancels answered on `out`, the rest queued.
#[cfg(test)]
pub(crate) fn lines_of(input: &[u8], out: &Responder) -> RequestLines {
    let (sender, lines) = request_queue();
    read_requests(&mut std::io::BufReader::new(input), &sender, out);
    lines
}

/// A test's [`Responder`] and the bytes written to it.
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

/// One line read from standard input.
pub(crate) enum RequestLine<'a> {
    Line(&'a str),
    /// A line of this many bytes, over the limit: skipped through its end,
    /// so the next line is read whole.
    TooLong(usize),
    NotUtf8,
    End,
}

/// Reads the next line of `input`, without its line ending, into `buffer`,
/// holding at most `limit` bytes of it. A line over the limit is read
/// through and discarded, so a sender that writes one too long is answered
/// and the request after it is read whole. The last line need not end in a
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

//! `FFmpeg`'s warning and error lines, kept for typed errors.
//!
//! `FFmpeg` explains most failures only in its log: a `VideoToolbox` `OSStatus`, an NVENC status
//! code, a D3D12 call that failed. The error code a call returns says far less (`EINVAL`,
//! `AVERROR_UNKNOWN`). [`install_log_capture`] routes every log line through a callback that keeps
//! the warnings and errors in a process-wide ring and still prints every line as `FFmpeg`'s own
//! callback does. An error path takes a [`log_mark`] before the call that may fail and reads
//! [`log_since`] after it.
//!
//! Lines are told apart by the class of the context that logged them (the codec, the hardware
//! device), not by thread: concurrent exports in one process can see each other's lines, so error
//! paths filter by the class they opened.

use std::collections::VecDeque;
use std::ffi::{CStr, c_char, c_int};
use std::sync::{Mutex, Once};

/// Lines the ring keeps; older ones are dropped.
const RING_LINES: usize = 256;
/// Lines [`log_detail`] quotes, the last ones.
const DETAIL_LINES: usize = 4;
/// Characters [`log_detail`] keeps of each line.
const DETAIL_CHARS: usize = 200;

/// One warning or error line `FFmpeg` logged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    /// Its place in the order lines were logged in.
    pub seq: u64,
    /// `FFmpeg`'s level (`AV_LOG_ERROR` is 16, `AV_LOG_WARNING` 24).
    pub level: i32,
    /// The logging context's name: a codec (`h264_nvenc`), a device context, or empty.
    pub class: String,
    /// The line, without its trailing newline.
    pub text: String,
}

/// A point in the log: [`log_since`] returns the lines logged after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogMark(u64);

struct Ring {
    next: u64,
    lines: VecDeque<LogLine>,
}

static RING: Mutex<Ring> = Mutex::new(Ring {
    next: 0,
    lines: VecDeque::new(),
});

unsafe extern "C" {
    fn alpha2_log_install();
}

/// Routes `FFmpeg`'s log through the capture. Idempotent; [`log_mark`] calls it.
pub fn install_log_capture() {
    static INSTALL: Once = Once::new();
    // SAFETY: the shim only sets FFmpeg's log callback, which may happen at any time.
    INSTALL.call_once(|| unsafe { alpha2_log_install() });
}

/// The current end of the log. Installs the capture first, so the lines after it are kept.
#[must_use]
pub fn log_mark() -> LogMark {
    install_log_capture();
    LogMark(ring().next)
}

/// The lines logged since `mark` by a context whose class is one of `classes` (any class when it
/// is empty), oldest first. Lines the ring has dropped since are missing.
#[must_use]
pub fn log_since(mark: LogMark, classes: &[&str]) -> Vec<LogLine> {
    ring()
        .lines
        .iter()
        .filter(|line| line.seq >= mark.0)
        .filter(|line| classes.is_empty() || classes.contains(&line.class.as_str()))
        .cloned()
        .collect()
}

/// `lines` as the tail of an error message: `"; FFmpeg said: [h264_nvenc] OpenEncodeSessionEx
/// failed: incompatible client key (21) | ..."`, the last four lines, each at most 200 characters
/// and ASCII (anything else escaped). Empty when there are no lines.
#[must_use]
pub fn log_detail(lines: &[LogLine]) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let quoted = lines[lines.len().saturating_sub(DETAIL_LINES)..]
        .iter()
        .map(|line| {
            let text = ascii(line.text.trim())
                .chars()
                .take(DETAIL_CHARS)
                .collect::<String>();
            if line.class.is_empty() {
                text
            } else {
                format!("[{}] {text}", ascii(&line.class))
            }
        })
        .collect::<Vec<_>>();
    format!("; FFmpeg said: {}", quoted.join(" | "))
}

fn ascii(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c == ' ' || c.is_ascii_graphic() {
                c.to_string()
            } else {
                c.escape_unicode().to_string()
            }
        })
        .collect()
}

fn ring() -> std::sync::MutexGuard<'static, Ring> {
    // A panic while holding the lock leaves whole lines behind; the ring stays usable.
    RING.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn record(level: i32, class: String, text: &str) {
    let mut ring = ring();
    let seq = ring.next;
    ring.next += 1;
    if ring.lines.len() == RING_LINES {
        ring.lines.pop_front();
    }
    ring.lines.push_back(LogLine {
        seq,
        level,
        class,
        text: text.trim_end_matches(['\n', '\r']).to_owned(),
    });
}

/// Called by the log shim, on whichever thread logged, for each warning or error line.
///
/// # Safety
/// `class_name` and `line` are null or NUL-terminated strings live for the call.
#[unsafe(no_mangle)]
unsafe extern "C" fn alpha2_log_record(
    level: c_int,
    class_name: *const c_char,
    line: *const c_char,
) {
    let text = |pointer: *const c_char| {
        if pointer.is_null() {
            String::new()
        } else {
            // SAFETY: the shim passes NUL-terminated strings that outlive this call.
            unsafe { CStr::from_ptr(pointer) }
                .to_string_lossy()
                .into_owned()
        }
    };
    // A panic must not unwind into C.
    let _ = std::panic::catch_unwind(|| record(level, text(class_name), &text(line)));
}

#[cfg(test)]
pub(crate) fn record_for_tests(level: i32, class: &str, text: &str) {
    record(level, class.to_owned(), text);
}

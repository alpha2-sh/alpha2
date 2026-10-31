//! `FFmpeg`'s warning and error lines, kept for typed errors.
//!
//! `FFmpeg` often explains a failure only in its log (an `OSStatus`, an NVENC status), while the
//! returned code says `EINVAL`. An error path takes a [`log_mark`] before the call and reads
//! [`log_since`] after it. Lines are filtered by the logging context's class, not by thread, so
//! concurrent exports can see each other's lines.

use std::collections::VecDeque;
use std::ffi::{CStr, c_char, c_int};
use std::sync::{Mutex, Once};

const RING_LINES: usize = 256;
const DETAIL_LINES: usize = 4;
const DETAIL_CHARS: usize = 200;

/// One warning or error line `FFmpeg` logged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    /// Logging order.
    pub seq: u64,
    /// `FFmpeg`'s level (`AV_LOG_ERROR` is 16, `AV_LOG_WARNING` 24).
    pub level: i32,
    /// The logging context's name (`h264_nvenc`), or empty.
    pub class: String,
    /// Without the trailing newline.
    pub text: String,
}

/// A point in the log for [`log_since`].
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

/// The current end of the log. Installs the capture first.
#[must_use]
pub fn log_mark() -> LogMark {
    install_log_capture();
    LogMark(ring().next)
}

/// Lines logged since `mark` by one of `classes` (any when empty), oldest first. Lines the ring has
/// dropped are missing.
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

/// Formats the last four lines as an error message tail (`"; FFmpeg said: [class] text | ..."`),
/// each cut to 200 characters with non-ASCII escaped. Empty when there are no lines.
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
    // Lines are pushed whole, so a poisoned ring is still consistent.
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

/// Called by the log shim, on the logging thread, for each warning or error line.
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

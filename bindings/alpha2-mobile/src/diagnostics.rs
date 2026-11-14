//! Diagnostic lines an app can read: device and driver messages, fallback reasons and panics.

use std::sync::{Arc, Once};

/// Why an app's diagnostic listener failed; the line is dropped.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ListenerFailure {
    /// The listener failed for `reason`.
    #[error("{reason}")]
    Failed {
        /// Why.
        reason: String,
    },
}

impl From<uniffi::UnexpectedUniFFICallbackError> for ListenerFailure {
    fn from(error: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Failed {
            reason: error.reason,
        }
    }
}

/// Receives the library's diagnostic lines.
#[uniffi::export(with_foreign)]
pub trait DiagnosticListener: Send + Sync {
    /// Called with one line on the thread that wrote it, which may be any library thread; keep it
    /// short.
    ///
    /// # Errors
    /// A failure, or anything the listener throws, drops the line and nothing else.
    fn on_diagnostic(&self, line: String) -> Result<(), ListenerFailure>;
}

/// Sends the library's diagnostic lines (a GPU device's errors, why an export draws on the CPU, a
/// panic and where it happened) to `listener`, or with `None` back to the default output: the
/// system log under the tag `alpha2` on Android, standard error elsewhere. One listener serves
/// the whole process.
#[uniffi::export]
pub fn set_diagnostic_listener(listener: Option<Arc<dyn DiagnosticListener>>) {
    alpha2_os::set_diagnostic_sink(listener.map(|listener| -> alpha2_os::DiagnosticSink {
        Arc::new(move |line: &str| {
            let _ = listener.on_diagnostic(line.to_owned());
        })
    }));
}

/// Sends every panic's message and location to the diagnostic output before the hook that was
/// installed, once per process.
pub(crate) fn report_panics() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let location = info.location().map_or_else(
                || "an unknown place".to_owned(),
                |at| format!("{}:{}:{}", at.file(), at.line(), at.column()),
            );
            alpha2_os::diagnostic!(
                "alpha2: panic at {location}: {}",
                alpha2_host_core::panic_text(info.payload())
            );
            previous(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Keeps the marked lines; fails on every other one it is given.
    struct Kept(Mutex<Vec<String>>);

    impl DiagnosticListener for Kept {
        fn on_diagnostic(&self, line: String) -> Result<(), ListenerFailure> {
            if !line.contains("listener-test") {
                return Err(ListenerFailure::Failed {
                    reason: "not ours".into(),
                });
            }
            self.0.lock().expect("lines").push(line);
            Ok(())
        }
    }

    #[test]
    fn a_listener_gets_diagnostic_lines_and_panics_with_their_location() {
        let kept = Arc::new(Kept(Mutex::new(Vec::new())));
        set_diagnostic_listener(Some(Arc::clone(&kept) as Arc<dyn DiagnosticListener>));
        report_panics();
        alpha2_os::diagnostic!("listener-test line");
        alpha2_os::diagnostic!("a line the listener refuses");
        let line = line!() + 1;
        let caught = std::panic::catch_unwind(|| panic!("listener-test boom"));
        set_diagnostic_listener(None);
        assert!(caught.is_err());
        let lines = kept.0.lock().expect("lines").clone();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0], "listener-test line");
        let at = format!("src/diagnostics.rs:{line}:");
        assert!(
            lines[1].starts_with("alpha2: panic at ")
                && lines[1].contains(&at)
                && lines[1].ends_with(": listener-test boom"),
            "{}",
            lines[1]
        );
    }
}

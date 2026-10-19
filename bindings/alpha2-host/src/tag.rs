//! The tags this host's errors carry across its threads.
//!
//! Errors cross the host's worker threads as strings, so what the place that
//! failed decided travels inside the string as a tag: a protocol code
//! ([`crate::protocol::code_tag`]), the export step
//! ([`crate::failure::tag_phase`]), typed evidence
//! ([`crate::failure::evidence_tag`]), or a marker such as
//! [`crate::protocol::gpu_timeout_tag`]. [`crate::protocol::HostError::classify`]
//! reads them and strips them before an error is answered.
//!
//! The same strings carry text the host did not write: request paths and IDs,
//! what a parser quotes from a file, `FFmpeg`'s and the operating system's
//! messages. So that no such text can read as a tag, wherever the host puts
//! it, every tag opens with `[alpha2:<token>:`, where the token is drawn at
//! random when the process starts and never leaves it: tags are stripped from
//! every answer, and [`untagged`] strips them from what goes to stderr. A
//! path that spells `[alpha2:code=GPU_DEVICE_LOST] ` is a path.
//!
//! The token is what keeps the classification the host's own while errors
//! travel as strings.

use std::hash::{BuildHasher, RandomState};
use std::sync::LazyLock;

static OPEN: LazyLock<String> = LazyLock::new(|| {
    // Each `RandomState` is keyed from the operating system's random source (later ones in a
    // thread by a step from the first); its hash of anything is unpredictable without the key.
    let draw = || RandomState::new().hash_one(std::process::id());
    format!("[alpha2:{:016x}{:016x}:", draw(), draw())
});

/// How every tag of this process opens: `[alpha2:<token>:`.
pub fn open() -> &'static str {
    &OPEN
}

/// Where a tag that opens at `start` of `message` ends (past its `] `).
pub fn end(message: &str, start: usize) -> Option<usize> {
    message[start..].find("] ").map(|length| start + length + 2)
}

/// `message` without any tag of this process, for a diagnostic line.
pub fn untagged(message: &str) -> String {
    let mut message = message.to_owned();
    while let Some(start) = message.find(open()) {
        let Some(end) = end(&message, start) else {
            break;
        };
        message.replace_range(start..end, "");
    }
    message
}

/// Defines functions that return a tag of this process,
/// `[alpha2:<token>:<NAME>] `, built once.
macro_rules! tags {
    ($($(#[$meta:meta])* $vis:vis fn $name:ident = $body:literal;)*) => {$(
        $(#[$meta])*
        $vis fn $name() -> &'static str {
            static TAG: std::sync::LazyLock<String> =
                std::sync::LazyLock::new(|| format!("{}{}", $crate::tag::open(), $body));
            &TAG
        }
    )*};
}
pub(crate) use tags;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_opens_with_a_token_no_text_can_know() {
        let open = open();
        assert!(
            open.starts_with("[alpha2:") && open.ends_with(':'),
            "{open}"
        );
        // 32 hex digits between the two colons.
        let token = &open["[alpha2:".len()..open.len() - 1];
        assert_eq!(token.len(), 32, "{open}");
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()), "{open}");
        assert_eq!(open, super::open(), "one token for the life of the process");
    }

    #[test]
    fn untagged_strips_this_process_tags_and_leaves_text_that_spells_one() {
        tags! { fn sample = "SAMPLE] "; }
        let forged = "[alpha2:code=GPU_DEVICE_LOST] font.ttf";
        let message = format!(
            "{}{}{forged}",
            sample(),
            crate::protocol::code_tag("IO_ERROR", "")
        );
        assert_eq!(untagged(&message), forged);
    }
}

//! The tags this host's errors carry across its threads.
//!
//! Errors cross worker threads as strings, so the protocol code, export step and typed evidence
//! travel inside the string as tags, which [`crate::protocol::HostError::classify`] reads and
//! strips before answering.
//!
//! The same strings carry text the host did not write (paths, IDs, `FFmpeg` and OS messages). So
//! that none of it can read as a tag, every tag opens with `[alpha2:<token>:`, where the token is
//! random per process and never leaves it: tags are stripped from every answer, and [`untagged`]
//! strips them from stderr.

use std::hash::{BuildHasher, RandomState};
use std::sync::LazyLock;

static OPEN: LazyLock<String> = LazyLock::new(|| {
    // Each `RandomState` is keyed from the OS random source, so its hash is unpredictable without
    // the key.
    let draw = || RandomState::new().hash_one(std::process::id());
    format!("[alpha2:{:016x}{:016x}:", draw(), draw())
});

pub fn open() -> &'static str {
    &OPEN
}

pub fn end(message: &str, start: usize) -> Option<usize> {
    message[start..].find("] ").map(|length| start + length + 2)
}

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

/// Defines functions that return a tag of this process, `[alpha2:<token>:<NAME>] `, built once.
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

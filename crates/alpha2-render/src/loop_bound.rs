//! Bounds the `for` loops in a runtime effect body.
//!
//! A fragment shader that never returns stalls the GPU until the driver resets
//! it, which loses the device for the whole process. `loop` and `while` are
//! refused outright, but `for` is how effects iterate over taps and samples,
//! often up to a parameter, so it is kept and bounded instead: every loop
//! spends one shared budget of [`MAX_LOOP_ITERATIONS`] iterations per shader
//! invocation, and stops when it is spent whatever its own condition says.
//!
//! The budget is per invocation, not per loop, because a per-loop counter
//! restarts each time its loop is entered: a nested loop, or a loop in a
//! function called from a loop, would multiply the bound by itself. It is the
//! wrapper's `var<private>` [`BUDGET`], set by the entry point. The body cannot
//! name it: identifiers starting with `alpha2_` belong to the wrapper.
//!
//! A private variable is not uniform, so a bounded loop is non-uniform control
//! flow: derivatives (`fwidth`, `textureSample`) belong before the loop.

use std::fmt::Write;

use crate::ShaderDiagnostics;

/// Iterations all `for` loops together may run in one shader invocation.
pub(crate) const MAX_LOOP_ITERATIONS: u32 = 1024;

/// The wrapper's per-invocation loop budget.
pub(crate) const BUDGET: &str = "alpha2_budget";

/// Returns `body` with comments blanked and every `for` loop spending the
/// invocation's budget.
///
/// Comments become spaces and every inserted token stays on its loop's line,
/// so compiler diagnostics keep the author's line numbers.
///
/// # Errors
/// Returns diagnostics for a `for` whose header or body this cannot delimit.
pub(crate) fn bound_for_loops(body: &str) -> Result<String, ShaderDiagnostics> {
    bound(&blank_comments(body))
}

fn bound(code: &str) -> Result<String, ShaderDiagnostics> {
    let mut output = String::with_capacity(code.len());
    let mut cursor = 0;
    while let Some(start) = find_keyword(code, cursor, "for") {
        output.push_str(&code[cursor..start]);
        let open = skip_whitespace(code, start + 3);
        if code.as_bytes().get(open) != Some(&b'(') {
            return Err(malformed());
        }
        let close = matching(code, open)?;
        let header = split_top_level(&code[open + 1..close]);
        let [init, condition, update] = header.as_slice() else {
            return Err(malformed());
        };
        let body_open = skip_whitespace(code, close + 1);
        if code.as_bytes().get(body_open) != Some(&b'{') {
            return Err(malformed());
        }
        let body_close = matching(code, body_open)?;
        let inner = bound(&code[body_open + 1..body_close])?;
        let limit = format!("{BUDGET} > 0u");
        let condition = condition.trim();
        let condition = if condition.is_empty() {
            limit
        } else {
            format!("({condition}) && {limit}")
        };
        write!(
            output,
            "for ({init}; {condition}; {update}) {{ {BUDGET} -= 1u;{inner}}}"
        )
        .expect("writing to a String cannot fail");
        cursor = body_close + 1;
    }
    output.push_str(&code[cursor..]);
    Ok(output)
}

fn malformed() -> ShaderDiagnostics {
    ShaderDiagnostics::error("WGSL for loop must be `for (init; condition; update) { ... }`")
}

pub(crate) const fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

pub(crate) fn find_keyword(code: &str, from: usize, keyword: &str) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut search = from;
    while let Some(offset) = code[search..].find(keyword) {
        let start = search + offset;
        let end = start + keyword.len();
        let before = start.checked_sub(1).map(|index| bytes[index]);
        let after = bytes.get(end).copied();
        if !before.is_some_and(is_identifier_byte) && !after.is_some_and(is_identifier_byte) {
            return Some(start);
        }
        search = end;
    }
    None
}

fn skip_whitespace(code: &str, from: usize) -> usize {
    code[from..]
        .find(|character: char| !character.is_whitespace())
        .map_or(code.len(), |offset| from + offset)
}

/// Returns the index of the delimiter closing the one at `open`.
pub(crate) fn matching(code: &str, open: usize) -> Result<usize, ShaderDiagnostics> {
    let mut depth = 0_usize;
    for (index, byte) in code.bytes().enumerate().skip(open) {
        match byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth = depth.checked_sub(1).ok_or_else(malformed)?;
                if depth == 0 {
                    return Ok(index);
                }
            }
            _ => {}
        }
    }
    Err(malformed())
}

/// Splits a `for` header at the semicolons outside any nested delimiter.
fn split_top_level(header: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0_usize;
    let mut start = 0;
    for (index, byte) in header.bytes().enumerate() {
        match byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b';' if depth == 0 => {
                parts.push(&header[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&header[start..]);
    parts
}

/// Replaces `//` and (nested) `/* */` comments with spaces, keeping newlines.
pub(crate) fn blank_comments(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut depth = 0_usize;
    let mut line_comment = false;
    while index < bytes.len() {
        let pair = (bytes[index], bytes.get(index + 1).copied());
        if line_comment {
            if bytes[index] == b'\n' {
                line_comment = false;
                output.push(b'\n');
            } else {
                output.push(b' ');
            }
            index += 1;
        } else if pair == (b'/', Some(b'*')) {
            depth += 1;
            output.extend_from_slice(b"  ");
            index += 2;
        } else if depth > 0 && pair == (b'*', Some(b'/')) {
            depth -= 1;
            output.extend_from_slice(b"  ");
            index += 2;
        } else if depth > 0 {
            output.push(if bytes[index] == b'\n' { b'\n' } else { b' ' });
            index += 1;
        } else if pair == (b'/', Some(b'/')) {
            line_comment = true;
            output.extend_from_slice(b"  ");
            index += 2;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    // Only ASCII bytes were replaced, and only by ASCII, so multi-byte characters outside
    // comments are intact; inside comments every byte of one became a space.
    String::from_utf8(output).expect("blanking comments keeps UTF-8 valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_for_loop_spends_the_one_budget() {
        let bounded =
            bound_for_loops("fn f() { for (var i = 0u; i < n; i++) { for (;;) { x += 1.0; } } }")
                .expect("well-formed loops");
        assert_eq!(
            bounded,
            "fn f() { for (var i = 0u; (i < n) && alpha2_budget > 0u;  i++) { \
             alpha2_budget -= 1u; for (; alpha2_budget > 0u; ) { alpha2_budget -= 1u; \
             x += 1.0; } } }"
        );
    }

    #[test]
    fn comments_neither_hide_nor_fake_a_loop_and_keep_line_numbers() {
        let body = "// for (;;) {\n/* a /* nested */ for */ fn f() {\nfor (;;) {} } // é";
        let bounded = bound_for_loops(body).expect("well-formed loop");
        assert_eq!(bounded.matches("alpha2_budget > 0u").count(), 1);
        assert_eq!(bounded.lines().count(), body.lines().count());
        assert!(!bounded.contains("nested"));
        // Identifiers that merely contain the keyword are not loops.
        assert_eq!(
            bound_for_loops("let format = before + forward;").expect("no loops"),
            "let format = before + forward;"
        );
    }

    #[test]
    fn a_loop_it_cannot_delimit_is_refused() {
        for body in ["for x", "for (;) {}", "for (;;) x += 1.0;", "for (;;"] {
            assert!(bound_for_loops(body).is_err(), "accepted {body}");
        }
    }
}

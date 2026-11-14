//! Bounds the `for` loops in a runtime effect body.
//!
//! A fragment shader that never returns stalls the GPU until the driver resets it, losing the
//! device for the process. `loop` and `while` are refused; `for` is kept but every loop spends
//! one shared budget of [`MAX_LOOP_ITERATIONS`] per invocation and stops when it runs out.
//!
//! The budget is per invocation, not per loop, since a per-loop counter would multiply under
//! nesting. It is the wrapper's `var<private>` [`BUDGET`], which bodies cannot name (`alpha2_`
//! identifiers are reserved).
//!
//! A private variable is not uniform, so bounded loops are non-uniform control flow: take
//! derivatives (`fwidth`, `textureSample`) before the loop.

use std::fmt::Write;

use crate::ShaderDiagnostics;

/// Iterations all `for` loops together may run in one shader invocation.
pub(crate) const MAX_LOOP_ITERATIONS: u32 = 1024;

/// The wrapper's per-invocation loop budget.
pub(crate) const BUDGET: &str = "alpha2_budget";

/// Prefix of the counter the rewrite gives each loop with a [`proven_count`].
const COUNTER: &str = "alpha2_n";

/// Returns `body` with comments blanked and every `for` loop spending the invocation budget.
///
/// Inserted tokens stay on their loop's line so diagnostics keep the author's line numbers.
///
/// # Errors
/// Returns diagnostics for a `for` whose header or body cannot be delimited.
pub(crate) fn bound_for_loops(body: &str) -> Result<String, ShaderDiagnostics> {
    bound(&blank_comments(body), &mut 0)
}

fn bound(code: &str, counters: &mut usize) -> Result<String, ShaderDiagnostics> {
    let mut output = String::with_capacity(code.len());
    let mut cursor = 0;
    while let Some(start) = find_keyword(code, cursor, "for") {
        output.push_str(&code[cursor..start]);
        let open = skip_whitespace(code, start + 3);
        if code.as_bytes().get(open) != Some(&b'(') {
            return Err(malformed());
        }
        let close = matching(code, open)?;
        let Some([init, condition, update]) = header_parts(&code[open + 1..close]) else {
            return Err(malformed());
        };
        let body_open = skip_whitespace(code, close + 1);
        if code.as_bytes().get(body_open) != Some(&b'{') {
            return Err(malformed());
        }
        let body_close = matching(code, body_open)?;
        let count = proven_count(init, condition, update, &code[body_open..=body_close]);
        let inner = bound(&code[body_open + 1..body_close], counters)?;
        let mut limit = format!("{BUDGET} > 0u");
        let mut spend = format!("{BUDGET} -= 1u;");
        let counter = count.map(|count| {
            let name = format!("{COUNTER}{counters}");
            *counters += 1;
            write!(limit, " && {name} > 0u").expect("writing to a String cannot fail");
            write!(spend, " {name} -= 1u;").expect("writing to a String cannot fail");
            (name, count)
        });
        let condition = condition.trim();
        let condition = if condition.is_empty() {
            limit
        } else {
            format!("({condition}) && {limit}")
        };
        if let Some((name, count)) = &counter {
            write!(output, "{{ var {name} = {count}u; ").expect("writing to a String cannot fail");
        }
        write!(
            output,
            "for ({init}; {condition}; {update}) {{ {spend}{inner}}}"
        )
        .expect("writing to a String cannot fail");
        if counter.is_some() {
            output.push_str(" }");
        }
        cursor = body_close + 1;
    }
    output.push_str(&code[cursor..]);
    Ok(output)
}

/// Iterations of a loop whose header steps a variable by one between non-negative integer
/// literals, if at most [`MAX_LOOP_ITERATIONS`] and `body` does not visibly write the variable.
///
/// The rewrite stops such a loop after this many iterations with a counter of its own, so the
/// count holds even where `writes` misses a write. `writes` only keeps honest loops uncut.
pub(crate) fn proven_count(init: &str, condition: &str, update: &str, body: &str) -> Option<u64> {
    let mut init = init.split_whitespace();
    if !matches!(init.next(), Some("var" | "let")) {
        return None;
    }
    let rest = init.collect::<String>();
    let (name, start) = rest.split_once('=')?;
    let name = name.split(':').next()?.trim();
    let start = integer_literal(start)?;
    let condition = condition.split_whitespace().collect::<String>();
    let (inclusive, end) = if let Some(end) = condition.strip_prefix(&format!("{name}<=")) {
        (true, end)
    } else {
        (false, condition.strip_prefix(&format!("{name}<"))?)
    };
    let end = integer_literal(end)?;
    let update = update.split_whitespace().collect::<String>();
    if ![
        format!("{name}++"),
        format!("++{name}"),
        format!("{name}+=1"),
        format!("{name}+=1u"),
    ]
    .contains(&update)
    {
        return None;
    }
    if writes(body, name) {
        return None;
    }
    let count = end
        .saturating_sub(start)
        .saturating_add(u64::from(inclusive));
    (count <= u64::from(MAX_LOOP_ITERATIONS)).then_some(count)
}

/// An integer literal such as `64`, `64u` or `64i`.
fn integer_literal(text: &str) -> Option<u64> {
    let text = text.trim();
    text.strip_suffix(['u', 'i'])
        .unwrap_or(text)
        .parse::<u64>()
        .ok()
}

/// Whether `code` may write `name`: assignment, increment, address-of, or a shadowing
/// declaration.
fn writes(code: &str, name: &str) -> bool {
    let mut from = 0;
    while let Some(start) = find_keyword(code, from, name) {
        let end = start + name.len();
        from = end;
        let after = code[end..].trim_start();
        let before = code[..start].trim_end();
        let assigned = after.starts_with('=') && !after.starts_with("==");
        let compound = [
            "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "<<=", ">>=", "++", "--",
        ]
        .iter()
        .any(|operator| after.starts_with(operator));
        let declared = before.ends_with("var") || before.ends_with("let");
        if assigned
            || compound
            || declared
            || before.ends_with("++")
            || before.ends_with("--")
            || before.ends_with('&')
        {
            return true;
        }
    }
    false
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

/// Index of the delimiter closing the one at `open`.
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

/// The init, condition and update of a `for` header, split at the semicolons outside any nested
/// delimiter.
pub(crate) fn header_parts(header: &str) -> Option<[&str; 3]> {
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
    parts.try_into().ok()
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
    // Only ASCII bytes were replaced, by ASCII, so UTF-8 outside comments is intact.
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
    fn a_counted_loop_gets_its_own_counter_whatever_its_body_hides() {
        for (header, write, count) in [
            ("var i = 0u; i < 8u; i++", "(i)--;", 8),
            ("var i = 0u; i < 8u; i++", "(i) -= 1u;", 8),
            ("var i = 0u; i < 2u; i++", "(i) = 0u;", 2),
            ("var i = 0u; i < 2u; i++", "*(&(i)) = 0u;", 2),
            ("var i = 0u; i < 2u; i++", "let p = &(i); *p = 0u;", 2),
            ("var i = 4294967295u; i <= 4294967295u; i++", "", 1),
            ("var i = 2147483647; i <= 2147483647; i++", "", 1),
        ] {
            let bounded =
                bound_for_loops(&format!("for ({header}) {{ {write} }}")).expect("a loop");
            assert!(
                bounded.starts_with(&format!("{{ var alpha2_n0 = {count}u; for (")),
                "{bounded}"
            );
            assert!(bounded.contains("&& alpha2_n0 > 0u;"), "{bounded}");
            assert!(bounded.contains("alpha2_n0 -= 1u;"), "{bounded}");
        }
        for body in [
            "for (var i = 0u; i < 8u; i++) { i = 0u; }",
            "for (var i = 0u; i < n; i++) { }",
            "for (var i = 0u; i < 2000u; i++) { }",
        ] {
            assert!(
                !bound_for_loops(body).expect("a loop").contains("alpha2_n"),
                "{body}"
            );
        }
    }

    #[test]
    fn nested_counted_loops_get_distinct_counters_on_one_line() {
        let body = "fn f() {\nfor (var i = 0u; i < 4u; i++) {\nfor (var j = 0u; j < 4u; j++) { }\n}\n\
                    for (var k = 0u; k < 4u; k++) { } }";
        let bounded = bound_for_loops(body).expect("well-formed loops");
        assert_eq!(bounded.lines().count(), body.lines().count());
        for counter in ["alpha2_n0", "alpha2_n1", "alpha2_n2"] {
            assert_eq!(bounded.matches(&format!("var {counter} = 4u;")).count(), 1);
        }
    }

    #[test]
    fn comments_neither_hide_nor_fake_a_loop_and_keep_line_numbers() {
        let body = "// for (;;) {\n/* a /* nested */ for */ fn f() {\nfor (;;) {} } // é";
        let bounded = bound_for_loops(body).expect("well-formed loop");
        assert_eq!(bounded.matches("alpha2_budget > 0u").count(), 1);
        assert_eq!(bounded.lines().count(), body.lines().count());
        assert!(!bounded.contains("nested"));
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

//! Bounds how many function bodies one runtime effect invocation runs.
//!
//! GPU compilers inline every call, so `fn f_i(x) { return f_{i-1}(f_{i-1}(x)); }` compiles to
//! `2^depth` copies of `f_0` with no loop in sight; Mesa and NVIDIA take tens of seconds and
//! gigabytes by depth 15. That compile is a synchronous driver call no timeout can interrupt,
//! so registration counts first.
//!
//! A function counts one plus, per call, its callee's count. A call inside a `for` loop counts
//! [`MAX_LOOP_ITERATIONS`] times ([`crate::loop_bound`]), since the loop condition cannot be
//! proven and drivers may unroll. Wrapper helpers and WGSL built-ins cost nothing extra.

use std::collections::HashMap;

use crate::ShaderDiagnostics;
use crate::loop_bound::{MAX_LOOP_ITERATIONS, find_keyword, is_identifier_byte, matching};

/// Function bodies one invocation may run.
///
/// The heaviest built-in (the directional transition, 4,105) has nearly twice the room. A
/// doubling call chain passes at depth 12 and is refused from depth 13.
pub(crate) const MAX_INVOCATION_CALLS: u64 = 8192;

struct Function<'a> {
    name: &'a str,
    /// Callees and how many times each call counts.
    calls: Vec<(usize, u64)>,
}

/// The most function bodies any function of `code` runs per invocation, saturating. `code` is
/// comment-blanked and accepted by [`crate::loop_bound::bound_for_loops`].
///
/// # Errors
/// Returns diagnostics for a directly or indirectly recursive function.
pub(crate) fn invocation_calls(code: &str) -> Result<(u64, Option<&str>), ShaderDiagnostics> {
    let functions = functions(code);
    // Iterative DFS so a long chain of functions does not recurse as deep.
    let mut cost: Vec<Option<u64>> = vec![None; functions.len()];
    let mut open = vec![false; functions.len()];
    for root in 0..functions.len() {
        if cost[root].is_some() {
            continue;
        }
        let mut stack = vec![(root, 0_usize)];
        open[root] = true;
        while let Some(top) = stack.last_mut() {
            let function = top.0;
            if let Some(&(callee, _)) = functions[function].calls.get(top.1) {
                top.1 += 1;
                if cost[callee].is_some() {
                    continue;
                }
                if open[callee] {
                    return Err(ShaderDiagnostics::error(format!(
                        "WGSL function {} calls itself, directly or through others; WGSL has no recursion",
                        functions[callee].name
                    )));
                }
                open[callee] = true;
                stack.push((callee, 0));
                continue;
            }
            let total = functions[function]
                .calls
                .iter()
                .fold(1_u64, |total, &(callee, times)| {
                    total.saturating_add(times.saturating_mul(cost[callee].unwrap_or(u64::MAX)))
                });
            cost[function] = Some(total);
            open[function] = false;
            stack.pop();
        }
    }
    Ok(cost
        .iter()
        .zip(&functions)
        .map(|(cost, function)| (cost.unwrap_or(u64::MAX), Some(function.name)))
        .max_by_key(|&(cost, _)| cost)
        .unwrap_or((0, None)))
}

/// Refuses a body any of whose functions runs more than [`MAX_INVOCATION_CALLS`] bodies.
///
/// # Errors
/// Returns diagnostics naming the function and its count.
pub(crate) fn check_invocation_calls(code: &str) -> Result<(), ShaderDiagnostics> {
    let (calls, name) = invocation_calls(code)?;
    if calls > MAX_INVOCATION_CALLS {
        let name = name.unwrap_or_default();
        let calls = if calls == u64::MAX {
            "at least 2^64 - 1".to_owned()
        } else {
            calls.to_string()
        };
        return Err(ShaderDiagnostics::error(format!(
            "WGSL function {name} runs {calls} function bodies per invocation once its calls are \
             inlined, counting a call in a for loop {MAX_LOOP_ITERATIONS} times; at most \
             {MAX_INVOCATION_CALLS} are allowed"
        )));
    }
    Ok(())
}

/// Module-scope functions of `code` and their calls to each other.
fn functions(code: &str) -> Vec<Function<'_>> {
    let declared = declarations(code);
    let names = declared_names(&declared);
    declared
        .iter()
        .map(|(name, body)| Function {
            name,
            calls: calls(code, body.clone(), &names),
        })
        .collect()
}

/// Module-scope function names and body ranges, braces included.
pub(crate) fn declarations(code: &str) -> Vec<(&str, std::ops::Range<usize>)> {
    let bytes = code.as_bytes();
    let mut declared = Vec::new();
    let mut depth = 0_usize;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            b'f' if depth == 0 && keyword_at(code, index, "fn") => {
                if let Some((name, body)) = declaration(code, index) {
                    index = body.end;
                    declared.push((name, body));
                    continue;
                }
            }
            _ => {}
        }
        index += 1;
    }
    declared
}

pub(crate) fn declared_names<'a>(
    declared: &[(&'a str, std::ops::Range<usize>)],
) -> HashMap<&'a str, usize> {
    declared
        .iter()
        .enumerate()
        .rev()
        .map(|(index, (name, _))| (*name, index))
        .collect()
}

/// Whether `keyword` starts at `index` as a whole identifier.
fn keyword_at(code: &str, index: usize, keyword: &str) -> bool {
    let bytes = code.as_bytes();
    code[index..].starts_with(keyword)
        && !(index > 0 && is_identifier_byte(bytes[index - 1]))
        && !bytes
            .get(index + keyword.len())
            .is_some_and(|&byte| is_identifier_byte(byte))
}

/// Name and body range (braces included) of the function declared at `start`.
fn declaration(code: &str, start: usize) -> Option<(&str, std::ops::Range<usize>)> {
    let name_start = start + 2 + code[start + 2..].find(|c: char| !c.is_whitespace())?;
    let name_end = name_start
        + code[name_start..]
            .bytes()
            .take_while(|&byte| is_identifier_byte(byte))
            .count();
    let open = name_end + code[name_end..].find('{')?;
    let close = matching(code, open).ok()?;
    (name_end > name_start).then(|| (&code[name_start..name_end], open..close + 1))
}

/// Calls in `body` to `names`, each with its count multiplier.
fn calls(
    code: &str,
    body: std::ops::Range<usize>,
    names: &HashMap<&str, usize>,
) -> Vec<(usize, u64)> {
    let bytes = code.as_bytes();
    let loops = loops(code, body.clone());
    let mut calls = Vec::new();
    let mut index = body.start;
    while index < body.end {
        if !is_identifier_byte(bytes[index]) || (index > 0 && is_identifier_byte(bytes[index - 1]))
        {
            index += 1;
            continue;
        }
        let end = index
            + code[index..body.end]
                .bytes()
                .take_while(|&byte| is_identifier_byte(byte))
                .count();
        let called = code[end..body.end].trim_start().starts_with('(');
        if let (true, Some(&callee)) = (called, names.get(&code[index..end])) {
            let in_loop = loops.iter().any(|range| range.contains(&index));
            calls.push((
                callee,
                if in_loop {
                    u64::from(MAX_LOOP_ITERATIONS)
                } else {
                    1
                },
            ));
        }
        index = end;
    }
    calls
}

/// Spans of the `for` loops in `body`, header to closing brace, nested ones included.
pub(crate) fn loops(code: &str, body: std::ops::Range<usize>) -> Vec<std::ops::Range<usize>> {
    let mut loops = Vec::new();
    let mut search = body.start;
    let within = &code[..body.end];
    while let Some(start) = find_keyword(within, search, "for") {
        search = start + 3;
        let Some(open) = code[search..body.end]
            .find('(')
            .map(|offset| search + offset)
        else {
            continue;
        };
        let Ok(close) = matching(code, open) else {
            continue;
        };
        let Some(body_open) = code[close..body.end].find('{').map(|offset| close + offset) else {
            continue;
        };
        if let Ok(body_close) = matching(code, body_open) {
            loops.push(start..body_close + 1);
        }
    }
    loops
}

/// A chain of `depth` doubling functions above `f0` plus an effect calling the last:
/// `2^(depth + 1)` bodies.
#[cfg(test)]
pub(crate) fn call_chain(depth: usize) -> String {
    use std::fmt::Write as _;
    let mut body = "fn f0(x: f32) -> f32 { return fract(x * 1.0001 + 0.37); }\n".to_owned();
    for level in 1..=depth {
        writeln!(
            body,
            "fn f{level}(x: f32) -> f32 {{ return f{0}(f{0}(x)); }}",
            level - 1
        )
        .expect("writing to a String cannot fail");
    }
    write!(
        body,
        "fn effect(uv: vec2f, src: vec4f) -> vec4f {{ return vec4f(f{depth}(uv.x), src.g, src.b, src.a); }}"
    )
    .expect("writing to a String cannot fail");
    body
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    fn chain(depth: usize) -> String {
        call_chain(depth)
    }

    #[test]
    fn a_chain_counts_every_inlined_body() {
        for depth in [0, 1, 5, 12] {
            let body = chain(depth);
            let (calls, name) = invocation_calls(&body).expect("no recursion");
            assert_eq!((calls, name), (1 << (depth + 1), Some("effect")));
        }
        assert!(check_invocation_calls(&chain(12)).is_ok());
        let refused = check_invocation_calls(&chain(13)).expect_err("depth 13");
        let message = format!("{refused:?}");
        assert!(message.contains("effect runs 16384"), "{message}");
        assert_eq!(
            invocation_calls(&chain(70)).expect("no recursion").0,
            u64::MAX
        );
        assert!(check_invocation_calls(&chain(70)).is_err());
    }

    #[test]
    fn a_call_in_a_loop_counts_the_loop_budget() {
        let body = "fn leaf(x: f32) -> f32 { return x; }\n\
                    fn twice(x: f32) -> f32 { return leaf(leaf(x)); }\n\
                    fn effect(uv: vec2f, src: vec4f) -> vec4f {\n\
                        var a = leaf(1.0);\n\
                        for (var i = 0u; i < 4u; i++) { a += twice(a); }\n\
                        for (var i = 0u; i < 4u; i++) { for (var j = 0u; j < 4u; j++) { a += leaf(a); } }\n\
                        return src * a; }";
        // 1 + leaf + 1024 * twice (3) + 1024 * leaf: a nested loop shares the one budget.
        assert_eq!(
            invocation_calls(body).expect("no recursion"),
            (1 + 1 + 3 * 1024 + 1024, Some("effect"))
        );
    }

    #[test]
    fn names_count_only_as_calls_of_declared_functions() {
        let body = "struct S { a: f32, };\n\
                    fn helper(x: f32) -> f32 { return x; }\n\
                    fn effect(uv: vec2f, src: vec4f) -> vec4f {\n\
                        let helper_value = 1.0; let s = S(helper (2.0));\n\
                        return sample_src(uv) * max(helper_value, s.a); }";
        assert_eq!(
            invocation_calls(body).expect("no recursion"),
            (2, Some("effect"))
        );
        assert_eq!(invocation_calls("").expect("empty"), (0, None));
    }

    #[test]
    fn every_built_in_fits_with_room() {
        let mut definitions = crate::builtins::internal_effect_definitions();
        definitions.extend(crate::builtin_effect_definitions());
        definitions.extend(crate::builtin_transition_definitions());
        let (most, id) = definitions
            .iter()
            .map(|definition| {
                let code = crate::loop_bound::blank_comments(&definition.wgsl_body);
                let (calls, _) = invocation_calls(&code).expect("no recursion");
                (calls, definition.id.clone())
            })
            .max()
            .expect("built-ins");
        assert_eq!(
            (most, id.as_str()),
            (4105, "alpha2.transition.directional"),
            "the costliest built-in, which MAX_INVOCATION_CALLS documents"
        );
        assert!(most + most / 2 <= MAX_INVOCATION_CALLS);
    }

    #[test]
    fn recursion_is_refused_not_followed() {
        for body in [
            "fn f(x: f32) -> f32 { return f(x); }",
            "fn a(x: f32) -> f32 { return b(x); } fn b(x: f32) -> f32 { return a(x); }",
        ] {
            let error = invocation_calls(body).expect_err(body);
            assert!(format!("{error:?}").contains("calls itself"), "{error:?}");
        }
    }

    #[test]
    fn a_long_chain_of_single_calls_does_not_recurse() {
        // A long single-call chain must be walked without a stack frame per level.
        let mut body = String::from("fn f0() -> f32 { return 1.0; }\n");
        for level in 1..3000 {
            writeln!(body, "fn f{level}() -> f32 {{ return f{}(); }}", level - 1)
                .expect("writing to a String cannot fail");
        }
        assert_eq!(invocation_calls(&body).expect("no recursion").0, 3000);
    }
}

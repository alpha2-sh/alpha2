//! Worst-case GPU cost of one runtime effect invocation.
//!
//! [`crate::loop_bound`] and [`crate::call_budget`] bound an effect's shape, not its work: 1024
//! iterations of texture reads and transcendentals fit in a few hundred bytes yet take seconds
//! per frame. This counts texture reads, transcendental calls and other operators per pixel.
//!
//! The count is an upper bound. All loops of an invocation share one budget of
//! [`MAX_LOOP_ITERATIONS`] iterations, so the cost is the costliest function's straight-line
//! code plus the budget spent on the costliest iterations. A nested loop body is its own
//! iteration; calls are inlined; both branch sides count.
//!
//! A loop is assumed to use the whole remaining budget unless its header counts a variable
//! between integer literals (`for (var i = 0u; i < 64u; i++)`), the body never writes it, and
//! the loop runs at most once per call (not nested, function not called from a loop).

use crate::ShaderDiagnostics;
use crate::call_budget::{declarations, declared_names, loops};
use crate::loop_bound::{MAX_LOOP_ITERATIONS, is_identifier_byte};

// Timed on discrete and integrated GPUs from three vendors, predictions moved by under 10% when
// these weights changed up to 8x; tap and iteration counts dominate. Arithmetic keeps a nonzero
// weight so compute-only effects are not priced at zero.
/// Units of one texture read.
pub(crate) const FETCH_UNITS: u64 = 4;
/// Units of one transcendental call.
pub(crate) const TRANSCENDENTAL_UNITS: u64 = 2;
/// Units of any other operator or call.
pub(crate) const ARITHMETIC_UNITS: u64 = 1;

/// Most units one runtime effect invocation may cost.
///
/// At the cap, an 8K frame on the slowest integrated GPU measured, split into
/// [`crate::submission_planner::MAX_BANDS`] bands, keeps each band under half the watchdog.
pub(crate) const MAX_INVOCATION_COST: u64 = 1_048_576;

/// WGSL built-ins counted as transcendental calls.
const TRANSCENDENTALS: [&str; 17] = [
    "sin",
    "cos",
    "tan",
    "asin",
    "acos",
    "atan",
    "atan2",
    "sinh",
    "cosh",
    "tanh",
    "exp",
    "exp2",
    "log",
    "log2",
    "pow",
    "sqrt",
    "inverseSqrt",
];

/// Words followed by a parenthesis that are not calls.
const KEYWORDS: [&str; 6] = ["if", "for", "while", "switch", "return", "loop"];

/// Worst-case work of one invocation (one pixel).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct EffectCost {
    /// Texture reads: wrapper `sample_*`, `from_color`, `to_color`, and `textureSample*`,
    /// `textureGather*`, `textureLoad`.
    pub(crate) fetches: u64,
    /// Transcendental built-in calls ([`TRANSCENDENTALS`]).
    pub(crate) transcendentals: u64,
    /// Every other operator and call.
    pub(crate) arithmetic: u64,
}

impl EffectCost {
    /// An engine-declared pass cost, assuming about four operators per read or call.
    pub(crate) const fn fixed(fetches: u64, transcendentals: u64) -> Self {
        Self {
            fetches,
            transcendentals,
            arithmetic: 4 * (fetches + transcendentals),
        }
    }

    /// Weighted sum used by predictions and the registration cap.
    pub(crate) const fn units(self) -> u64 {
        self.fetches
            .saturating_mul(FETCH_UNITS)
            .saturating_add(self.transcendentals.saturating_mul(TRANSCENDENTAL_UNITS))
            .saturating_add(self.arithmetic.saturating_mul(ARITHMETIC_UNITS))
    }

    const fn plus(self, other: Self) -> Self {
        Self {
            fetches: self.fetches.saturating_add(other.fetches),
            transcendentals: self.transcendentals.saturating_add(other.transcendentals),
            arithmetic: self.arithmetic.saturating_add(other.arithmetic),
        }
    }

    pub(crate) const fn times(self, count: u64) -> Self {
        Self {
            fetches: self.fetches.saturating_mul(count),
            transcendentals: self.transcendentals.saturating_mul(count),
            arithmetic: self.arithmetic.saturating_mul(count),
        }
    }

    fn costlier(self, other: Self) -> Self {
        if other.units() > self.units() {
            other
        } else {
            self
        }
    }
}

#[derive(Copy, Clone)]
enum Item {
    Cost(EffectCost),
    /// A call of the declared function with this index.
    Call(usize),
}

/// One function's tokens outside loops, and per loop the tokens of one iteration.
struct Walked {
    straight: Vec<Item>,
    loops: Vec<Loop>,
}

struct Loop {
    items: Vec<Item>,
    /// Iterations per call when provable and not nested.
    count: Option<u64>,
}

/// Worst-case cost of `code`, a comment-blanked body that passed [`crate::loop_bound`] and
/// [`crate::call_budget`]. A refused call cycle counts its callee as zero.
pub(crate) fn invocation_cost(code: &str) -> EffectCost {
    let declared = declarations(code);
    let names = declared_names(&declared);
    let walked = declared
        .iter()
        .map(|(_, body)| walk(code, body.clone(), &names))
        .collect::<Vec<_>>();
    let straight = straight_costs(&walked);
    let calls = straight_calls(&walked, &roots(&walked));
    let resolve = |items: &[Item]| {
        items.iter().fold(EffectCost::default(), |total, item| {
            total.plus(match *item {
                Item::Cost(cost) => cost,
                Item::Call(callee) => straight[callee],
            })
        })
    };
    let roots = roots(&walked);
    let entry = straight
        .iter()
        .zip(&roots)
        .filter(|(_, root)| **root)
        .fold(EffectCost::default(), |worst, (&cost, _)| {
            worst.costlier(cost)
        });
    let mut iterations = walked
        .iter()
        .zip(&calls)
        .flat_map(|(function, calls)| {
            function.loops.iter().map(move |body| {
                let most = match (body.count, calls) {
                    (Some(count), Some(calls)) => count.saturating_mul(*calls),
                    _ => u64::MAX,
                };
                (resolve(&body.items), most)
            })
        })
        .collect::<Vec<_>>();
    iterations.sort_by_key(|(cost, _)| std::cmp::Reverse(cost.units()));
    let mut budget = u64::from(MAX_LOOP_ITERATIONS);
    let mut total = entry;
    for (cost, most) in iterations {
        let spent = budget.min(most);
        total = total.plus(cost.times(spent));
        budget -= spent;
    }
    total
}

/// Functions no other function calls (all of them when calls form a cycle).
fn roots(walked: &[Walked]) -> Vec<bool> {
    let mut called = vec![false; walked.len()];
    for function in walked {
        let items = function
            .straight
            .iter()
            .chain(function.loops.iter().flat_map(|body| body.items.iter()));
        for item in items {
            if let Item::Call(callee) = item {
                called[*callee] = true;
            }
        }
    }
    if called.iter().all(|called| *called) {
        return vec![true; walked.len()];
    }
    called.into_iter().map(|called| !called).collect()
}

/// Max runs per invocation of each function (roots once); `None` if reachable from a loop.
fn straight_calls(walked: &[Walked], roots: &[bool]) -> Vec<Option<u64>> {
    let mut in_loop = vec![false; walked.len()];
    for function in walked {
        for body in &function.loops {
            for item in &body.items {
                if let Item::Call(callee) = item {
                    in_loop[*callee] = true;
                }
            }
        }
    }
    // A function called straight-line from one in a loop also runs in a loop.
    let mut changed = true;
    while changed {
        changed = false;
        for (caller, function) in walked.iter().enumerate() {
            if !in_loop[caller] {
                continue;
            }
            for item in &function.straight {
                if let Item::Call(callee) = item
                    && !in_loop[*callee]
                {
                    in_loop[*callee] = true;
                    changed = true;
                }
            }
        }
    }
    // Without cycles this settles within one round per function; a cycle stops counting, and its
    // loops still share the budget.
    let base = roots
        .iter()
        .map(|root| u64::from(*root))
        .collect::<Vec<_>>();
    let mut runs = base.clone();
    for _ in 0..walked.len() {
        let mut next = base.clone();
        for (caller, function) in walked.iter().enumerate() {
            for item in &function.straight {
                if let Item::Call(callee) = item {
                    next[*callee] = next[*callee].saturating_add(runs[caller]);
                }
            }
        }
        if next == runs {
            break;
        }
        runs = next;
    }
    runs.into_iter()
        .zip(in_loop)
        .map(|(runs, in_loop)| (!in_loop).then_some(runs))
        .collect()
}

/// Refuses a body whose invocation may cost more than [`MAX_INVOCATION_COST`].
///
/// # Errors
/// Diagnostics naming the reads, transcendental calls and units.
pub(crate) fn check_invocation_cost(code: &str) -> Result<EffectCost, ShaderDiagnostics> {
    let cost = invocation_cost(code);
    let units = cost.units();
    if units > MAX_INVOCATION_COST {
        return Err(ShaderDiagnostics::error(format!(
            "the effect may spend {} texture reads and {} transcendental calls per pixel \
             ({units} units), above the {MAX_INVOCATION_COST} a runtime effect may; read fewer \
             taps per loop iteration or loop fewer times",
            cost.fetches, cost.transcendentals
        )));
    }
    Ok(cost)
}

/// Each function's straight-line cost with its calls inlined, callees first.
fn straight_costs(walked: &[Walked]) -> Vec<EffectCost> {
    let mut cost: Vec<Option<EffectCost>> = vec![None; walked.len()];
    let mut open = vec![false; walked.len()];
    let calls = |function: usize| {
        walked[function]
            .straight
            .iter()
            .filter_map(|item| match item {
                Item::Call(callee) => Some(*callee),
                Item::Cost(_) => None,
            })
            .collect::<Vec<_>>()
    };
    // Iterative so long call chains do not recurse deeply.
    for root in 0..walked.len() {
        if cost[root].is_some() {
            continue;
        }
        let mut stack = vec![(root, calls(root), 0_usize)];
        open[root] = true;
        while let Some((function, callees, next)) = stack.last_mut() {
            if let Some(&callee) = callees.get(*next) {
                *next += 1;
                if cost[callee].is_none() && !open[callee] {
                    open[callee] = true;
                    let callee_calls = calls(callee);
                    stack.push((callee, callee_calls, 0));
                }
                continue;
            }
            let function = *function;
            let total =
                walked[function]
                    .straight
                    .iter()
                    .fold(EffectCost::default(), |total, item| {
                        total.plus(match *item {
                            Item::Cost(cost) => cost,
                            Item::Call(callee) => cost[callee].unwrap_or_default(),
                        })
                    });
            cost[function] = Some(total);
            open[function] = false;
            stack.pop();
        }
    }
    cost.into_iter().map(Option::unwrap_or_default).collect()
}

/// Sorts a function body's tokens into its straight line and their innermost loop.
fn walk(
    code: &str,
    body: std::ops::Range<usize>,
    names: &std::collections::HashMap<&str, usize>,
) -> Walked {
    let spans = loops(code, body.clone());
    let mut walked = Walked {
        straight: Vec::new(),
        loops: spans
            .iter()
            .map(|span| Loop {
                items: Vec::new(),
                count: (!spans
                    .iter()
                    .any(|outer| outer.start < span.start && outer.contains(&span.start)))
                .then(|| literal_count(code, span.clone()))
                .flatten(),
            })
            .collect(),
    };
    let bytes = code.as_bytes();
    let mut push = |at: usize, item: Item| {
        // The loop starting last among those containing `at` is the innermost.
        let innermost = spans
            .iter()
            .enumerate()
            .filter(|(_, span)| span.contains(&at))
            .max_by_key(|(_, span)| span.start)
            .map(|(index, _)| index);
        match innermost {
            Some(index) => walked.loops[index].items.push(item),
            None => walked.straight.push(item),
        }
    };
    let mut index = body.start;
    while index < body.end {
        let byte = bytes[index];
        if is_identifier_byte(byte) {
            let end = index
                + code[index..body.end]
                    .bytes()
                    .take_while(|&byte| is_identifier_byte(byte))
                    .count();
            let word = &code[index..end];
            let called = code[end..body.end].trim_start().starts_with('(');
            if called && !KEYWORDS.contains(&word) && !byte.is_ascii_digit() {
                push(
                    index,
                    names
                        .get(word)
                        .map_or_else(|| Item::Cost(call_cost(word)), |&callee| Item::Call(callee)),
                );
            }
            index = end;
        } else if is_operator(byte) {
            let end = index
                + code[index..body.end]
                    .bytes()
                    .take_while(|&byte| is_operator(byte))
                    .count();
            // A plain assignment computes nothing.
            if &code[index..end] != "=" {
                push(
                    index,
                    Item::Cost(EffectCost {
                        arithmetic: 1,
                        ..EffectCost::default()
                    }),
                );
            }
            index = end;
        } else {
            index += 1;
        }
    }
    walked
}

/// Iteration count of the loop at `span` if its header counts a variable by one between integer
/// literals and the body never writes it.
fn literal_count(code: &str, span: std::ops::Range<usize>) -> Option<u64> {
    let open = span.start + code[span.clone()].find('(')?;
    let close = crate::loop_bound::matching(code, open).ok()?;
    let header = &code[open + 1..close];
    let [init, condition, update] = header.split(';').collect::<Vec<_>>()[..] else {
        return None;
    };
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
    if writes(&code[close + 1..span.end], name) {
        return None;
    }
    Some(
        end.saturating_sub(start)
            .saturating_add(u64::from(inclusive)),
    )
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
    while let Some(start) = crate::loop_bound::find_keyword(code, from, name) {
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

/// Cost of a call to an undeclared `name` (wrapper helper or WGSL built-in).
fn call_cost(name: &str) -> EffectCost {
    let fetch = name.starts_with("sample_")
        || name.starts_with("textureSample")
        || name.starts_with("textureGather")
        || matches!(name, "from_color" | "to_color" | "textureLoad");
    if fetch {
        EffectCost {
            fetches: 1,
            ..EffectCost::default()
        }
    } else if TRANSCENDENTALS.contains(&name) {
        EffectCost {
            transcendentals: 1,
            ..EffectCost::default()
        }
    } else {
        EffectCost {
            arithmetic: 1,
            ..EffectCost::default()
        }
    }
}

const fn is_operator(byte: u8) -> bool {
    matches!(
        byte,
        b'+' | b'-' | b'*' | b'/' | b'%' | b'&' | b'|' | b'^' | b'!' | b'<' | b'>' | b'=' | b'~'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loop_bound::blank_comments;

    fn cost(body: &str) -> EffectCost {
        invocation_cost(&blank_comments(body))
    }

    #[test]
    fn loop_budget_bounds_the_costliest_iteration() {
        // The budget goes to the costlier body regardless of each loop's own bound.
        let body = "fn effect(uv: vec2f, src: vec4f) -> vec4f {\n\
                        var a = src;\n\
                        for (var i = 0u; i < n; i++) { a = a * sample_src(uv); }\n\
                        for (var j = 0u; j < n; j++) { a = sin(a); }\n\
                        return a; }";
        let worst = cost(body);
        assert_eq!(
            worst,
            EffectCost {
                fetches: 1024,
                transcendentals: 0,
                arithmetic: 3 * 1024,
            }
        );
        assert_eq!(worst.units(), 1024 * (4 + 3));
    }

    #[test]
    fn nested_loops_share_the_budget() {
        let body = "fn effect(uv: vec2f, src: vec4f) -> vec4f {\n\
                        var a = 0.0;\n\
                        for (var i = 0u; i < 32u; i++) {\n\
                            a += 1.0;\n\
                            for (var j = 0u; j < 32u; j++) { a += cos(a) + sample_src(uv).r; }\n\
                        }\n\
                        return vec4f(a); }";
        let worst = cost(body);
        assert_eq!(
            worst,
            EffectCost {
                fetches: 1024,
                transcendentals: 1024,
                arithmetic: 4 * 1024 + 1,
            }
        );
    }

    #[test]
    fn a_literal_count_caps_its_loop_and_leaves_the_budget_to_others() {
        let body = "fn effect(uv: vec2f, src: vec4f) -> vec4f {\n\
                        var a = src;\n\
                        for (var i = 0u; i < 64u; i++) { a = a * sample_src(uv); }\n\
                        for (var j = 0; j < n; j++) { a = a + 1.0; }\n\
                        return a; }";
        let worst = cost(body);
        assert_eq!(worst.fetches, 64);
        assert_eq!(worst.arithmetic, 64 * 3 + 960 * 3);
        // A written counter, a nested loop, or a loop reached from a loop uses the whole budget.
        for body in [
            "fn effect(uv: vec2f, src: vec4f) -> vec4f { var a = src; \
             for (var i = 0u; i < 64u; i++) { a = sample_src(uv); i = 0u; } return a; }",
            "fn effect(uv: vec2f, src: vec4f) -> vec4f { var a = src; \
             for (var k = 0u; k < n; k++) { \
             for (var i = 0u; i < 4u; i++) { a = sample_src(uv); } } return a; }",
            "fn taps(uv: vec2f) -> vec4f { var a = vec4f(0.0); \
             for (var i = 0u; i < 4u; i++) { a = sample_src(uv); } return a; }\n\
             fn effect(uv: vec2f, src: vec4f) -> vec4f { var a = src; \
             for (var k = 0u; k < n; k++) { a = taps(uv); } return a; }",
        ] {
            assert_eq!(cost(body).fetches, 1024, "{body}");
        }
        // A function called twice runs its counted loop twice.
        let twice = "fn taps(uv: vec2f) -> vec4f { var a = vec4f(0.0); \
                     for (var i = 0u; i <= 9u; i++) { a = sample_src(uv); } return a; }\n\
                     fn effect(uv: vec2f, src: vec4f) -> vec4f { return taps(uv) + taps(uv); }";
        assert_eq!(cost(twice).fetches, 20);
    }

    #[test]
    fn helpers_count_as_fetches() {
        let body = "fn helper(uv: vec2f) -> vec4f { return sample_src(uv) + from_color(uv); }\n\
                    fn effect(uv: vec2f, src: vec4f) -> vec4f {\n\
                        let t = to_color(uv) + sample_mask(uv) + textureLoad(x, vec2i(0), 0);\n\
                        return helper(uv) * exp(t); }";
        let worst = cost(body);
        assert_eq!(worst.fetches, 5);
        assert_eq!(worst.transcendentals, 1);
        assert_eq!(worst.arithmetic, 5);
    }

    #[test]
    fn a_call_in_a_loop_is_inlined_into_the_iteration() {
        let body = "fn tap(uv: vec2f) -> vec4f { return sample_src(uv) * 2.0; }\n\
                    fn effect(uv: vec2f, src: vec4f) -> vec4f {\n\
                        var a = vec4f(0.0);\n\
                        for (var i = 0u; i < n; i++) { a += tap(uv); }\n\
                        return a; }";
        let worst = cost(body);
        assert_eq!(worst.fetches, 1024);
        assert_eq!(worst.arithmetic, 4 * 1024 + 1);
    }

    #[test]
    fn every_built_in_fits_the_cost_cap_with_room() {
        let mut definitions = crate::builtins::internal_effect_definitions();
        definitions.extend(crate::builtin_effect_definitions());
        definitions.extend(crate::builtin_transition_definitions());
        let (units, id) = definitions
            .iter()
            .map(|definition| {
                let units = invocation_cost(&blank_comments(&definition.wgsl_body)).units();
                (units, definition.id.clone())
            })
            .max()
            .expect("built-ins");
        assert!(
            units.saturating_mul(4) <= MAX_INVOCATION_COST,
            "{id} costs {units} units"
        );
    }

    #[test]
    fn the_max_fixture_is_refused_with_a_diagnostic_naming_its_cost() {
        let fixture = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/cases/gpu-watchdog/max-effects.json"
        ))
        .expect("the max fixture");
        let effects: serde_json::Value = serde_json::from_str(&fixture).expect("JSON");
        let body = effects[0]["wgslBody"].as_str().expect("a WGSL body");
        let error = check_invocation_cost(&blank_comments(body)).expect_err("refused");
        let message = format!("{error:?}");
        let cost = invocation_cost(&blank_comments(body));
        assert!(
            message.contains(&format!("{} texture reads", cost.fetches))
                && message.contains(&format!("{} transcendental calls", cost.transcendentals))
                && message.contains(&format!("({} units)", cost.units())),
            "{message}"
        );
    }
}

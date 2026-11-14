#![no_main]

use alpha2_render::{
    EffectDef, EffectKind, ParamSpec, ParamType, ParamUnit, ParamValue, check_effect,
};
use libfuzzer_sys::fuzz_target;
use wgpu::naga;

/// Functions the wrapper declares around an effect body; they cost nothing in the analysis.
const WRAPPER_FUNCTIONS: [&str; 11] = [
    "alpha2_transition",
    "sample_src",
    "from_color",
    "to_color",
    "resolution",
    "time",
    "time_ticks",
    "progress",
    "ratio",
    "max_filter_samples",
    "load_params",
];

/// Iterations a call inside a loop counts, as the analysis counts it.
const LOOP_CALLS: u64 = 1024;

fn calls_in(block: &naga::Block, weight: u64, out: &mut Vec<(naga::Handle<naga::Function>, u64)>) {
    for statement in block.iter() {
        match statement {
            naga::Statement::Block(inner) => calls_in(inner, weight, out),
            naga::Statement::If { accept, reject, .. } => {
                calls_in(accept, weight, out);
                calls_in(reject, weight, out);
            }
            naga::Statement::Switch { cases, .. } => {
                for case in cases {
                    calls_in(&case.body, weight, out);
                }
            }
            naga::Statement::Loop {
                body, continuing, ..
            } => {
                calls_in(body, weight.saturating_mul(LOOP_CALLS), out);
                calls_in(continuing, weight.saturating_mul(LOOP_CALLS), out);
            }
            naga::Statement::Call { function, .. } => out.push((*function, weight)),
            _ => {}
        }
    }
}

// A body registration's checks accept either fails naga's own checks or is the module the
// analysis priced: no more functions than it declared, no more inlined bodies than it counted.
// A miscount here is how text-level analysis is fooled into pricing an unbounded shader cheaply.
fuzz_target!(|data: &[u8]| {
    let Ok(body) = std::str::from_utf8(data) else {
        return;
    };
    // Built-in bodies read `params.<name>`; each name read becomes a float parameter.
    let mut names = body
        .split("params.")
        .skip(1)
        .map(|rest| {
            rest.split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
                .next()
                .unwrap_or_default()
        })
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    names.sort_unstable();
    names.dedup();
    let params = names
        .into_iter()
        .take(16)
        .map(|name| ParamSpec {
            name: name.into(),
            param_type: ParamType::Float,
            default: ParamValue::Float(0.5),
            min: Some(0.0),
            max: Some(1.0),
            step: None,
            unit: ParamUnit::Normalized,
            enum_values: Vec::new(),
        })
        .collect();
    let kind = if body.contains("fn transition(") {
        EffectKind::Transition
    } else if body.contains("fn generate(") {
        EffectKind::Generator
    } else {
        EffectKind::Effect
    };
    let definition = EffectDef {
        id: "fuzz.effect".into(),
        version: 1,
        kind,
        inputs: Vec::new(),
        params,
        resources: Vec::new(),
        wgsl_body: body.into(),
        tier_min: alpha2_gpu::CapabilityTier::TierB,
        license: None,
    };
    let Ok(checked) = check_effect(&definition) else {
        return;
    };
    let Ok(module) = naga::front::wgsl::parse_str(&checked.module) else {
        return;
    };
    let validated = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    )
    .validate(&module);
    if validated.is_err() {
        return;
    }
    let wrapper = |name: &Option<String>| {
        name.as_deref()
            .is_some_and(|name| WRAPPER_FUNCTIONS.contains(&name))
    };
    let declared = module
        .functions
        .iter()
        .filter(|(_, function)| !wrapper(&function.name))
        .count();
    assert!(
        declared <= checked.functions,
        "naga sees {declared} functions, the analysis {}",
        checked.functions
    );
    // Functions come before their callers in naga's arena, so one pass in order settles them.
    let mut bodies = std::collections::HashMap::new();
    for (handle, function) in module.functions.iter() {
        if wrapper(&function.name) {
            bodies.insert(handle, 0_u64);
            continue;
        }
        let mut calls = Vec::new();
        calls_in(&function.body, 1, &mut calls);
        let total = calls.iter().fold(1_u64, |total, (callee, weight)| {
            total.saturating_add(
                bodies
                    .get(callee)
                    .copied()
                    .unwrap_or(0)
                    .saturating_mul(*weight),
            )
        });
        bodies.insert(handle, total);
    }
    let most = bodies.values().copied().max().unwrap_or(0);
    assert!(
        most <= checked.calls.max(1),
        "naga inlines {most} bodies, the analysis counted {}",
        checked.calls
    );
});

use super::*;

#[test]
fn uniform_members_receive_stable_sixteen_byte_slots() {
    let params = vec![
        ParamSpec {
            name: "radius".into(),
            param_type: ParamType::Float,
            default: ParamValue::Float(0.1),
            min: Some(0.0),
            max: Some(1.0),
            step: Some(0.01),
            unit: ParamUnit::Normalized,
            enum_values: Vec::new(),
        },
        ParamSpec {
            name: "tint".into(),
            param_type: ParamType::Color,
            default: ParamValue::Color([1.0; 4]),
            min: None,
            max: None,
            step: None,
            unit: ParamUnit::Color,
            enum_values: Vec::new(),
        },
    ];
    assert_eq!(
        uniform_layout(&params),
        UniformLayout {
            fields: vec![
                UniformField {
                    name: "radius".into(),
                    offset: 0,
                    size: 16,
                },
                UniformField {
                    name: "tint".into(),
                    offset: 16,
                    size: 16,
                },
            ],
            size: 32,
        }
    );
}

#[test]
fn a_color_parameter_must_be_a_color() {
    let color = |unit| ParamSpec {
        name: "tint".into(),
        param_type: ParamType::Color,
        default: ParamValue::Color([0.0, 0.2, 0.4, 0.0]),
        min: None,
        max: None,
        step: None,
        unit,
        enum_values: Vec::new(),
    };
    assert!(validate_param(&color(ParamUnit::Color)).is_ok());
    // Color keyframes interpolate premultiplied, which is wrong for four numbers that
    // are not a color: those are a vec4.
    assert!(validate_param(&color(ParamUnit::Normalized)).is_err());
}

#[test]
fn runtime_wgsl_rejects_unbounded_or_resource_declaring_code() {
    for body in [
        "fn effect(uv: vec2f, src: vec4f) -> vec4f { loop {} }",
        "@group(1) @binding(0) var extra: texture_2d<f32>;",
        "fn effect() { var<storage> data: array<u32>; }",
    ] {
        assert!(validate_wgsl_body(body).is_err(), "accepted {body}");
    }
    assert!(
        validate_wgsl_body("fn effect(uv: vec2f, src: vec4f) -> vec4f { return src; }").is_ok()
    );
}

#[test]
fn a_call_chain_that_inlines_exponentially_is_refused_at_registration() {
    // No loop and under a kilobyte, yet 2^(depth + 1) bodies once inlined: depth 14
    // took 20 s and 520 MB to compile on lavapipe, depth 15 28 s on NVIDIA 580.
    let chain = crate::call_budget::call_chain;
    assert!(validate_wgsl_body(&chain(12)).is_ok());
    for depth in [13, 14, 16] {
        let body = chain(depth);
        assert!(body.len() < 1024, "{} bytes", body.len());
        let error = validate_wgsl_body(&body).expect_err("an exponential chain");
        let message = format!("{error:?}");
        assert!(
            message.contains("function effect runs") && message.contains("at most 8192"),
            "{message}"
        );
    }
    // Every built-in, the generator's two gradient loops among them, still registers.
    let mut definitions = crate::builtins::internal_effect_definitions().to_vec();
    definitions.extend(crate::builtin_effect_definitions());
    definitions.extend(crate::builtin_transition_definitions());
    for definition in definitions {
        assert!(
            validate_wgsl_body(&definition.wgsl_body).is_ok(),
            "{}",
            definition.id
        );
    }
}

#[test]
fn a_body_cannot_name_what_the_wrapper_declares() {
    // A body that could assign the loop guard would reset it, and its loop would never
    // end.
    for body in [
        "fn effect(uv: vec2f, src: vec4f) -> vec4f { var acc = 0.0; for (var i = 0; true; i++) { alpha2_loop_0 = 0u; acc += 1.0; } return vec4f(acc); }",
        "fn effect(uv: vec2f, src: vec4f) -> vec4f { var acc = 0.0; for (var i = 0; true; alpha2_loop_0 = 0u) { acc += 1.0; } return vec4f(acc); }",
        "fn effect(uv: vec2f, src: vec4f) -> vec4f { var acc = 0.0; for (;;) { alpha2_budget = 1024u; acc += 1.0; } return vec4f(acc); }",
        "fn alpha2_fs() {} fn effect(uv: vec2f, src: vec4f) -> vec4f { return src; }",
    ] {
        let error = validate_wgsl_body(body).expect_err(body);
        assert!(error.messages[0].message.contains("reserved"), "{error:?}");
    }
    // Only identifiers are names: a comment may mention one, and other words may
    // contain the prefix.
    assert_eq!(
        validate_wgsl_body(
            "// alpha2_budget\nfn effect(uv: vec2f, src: vec4f) -> vec4f { let my_alpha2_x = 1.0; return src * my_alpha2_x; }"
        ),
        Ok(())
    );
    assert!(!valid_wgsl_identifier("alpha2_strength"));
}

#[test]
fn a_statement_is_limited_in_operators_not_delimiters() {
    let chain = |operands: usize| {
        format!(
            "fn effect(uv: vec2f, src: vec4f) -> vec4f {{ let x = src.r; return vec4f({}); }}",
            vec!["x"; operands].join("+")
        )
    };
    assert!(validate_wgsl_body(&chain(3_000)).is_err());
    assert_eq!(validate_wgsl_body(&chain(200)), Ok(()));
    // The same work split across statements is accepted.
    let split = format!(
        "fn effect(uv: vec2f, src: vec4f) -> vec4f {{ var x = src.r; {} return vec4f(x); }}",
        "x = x+x+x+x+x+x+x+x+x+x+x+x+x+x+x+x+x+x+x+x;".repeat(100)
    );
    assert_eq!(validate_wgsl_body(&split), Ok(()));
}

#[test]
fn delimiters_in_comments_are_not_counted() {
    // Like the forbidden tokens, balance is a property of the code: a comment may
    // write a half-open range.
    assert_eq!(
        validate_wgsl_body(
            "// hash to [0, 1)\nfn effect(uv: vec2f, src: vec4f) -> vec4f { /* ( */ return src; }"
        ),
        Ok(())
    );
    for body in [
        "fn effect(uv: vec2f, src: vec4f) -> vec4f { return src; // }",
        "fn effect(uv: vec2f, src: vec4f) -> vec4f { return src; /* } */",
    ] {
        assert!(validate_wgsl_body(body).is_err(), "accepted {body}");
    }
}

fn float_param(name: &str) -> ParamSpec {
    ParamSpec {
        name: name.into(),
        param_type: ParamType::Float,
        default: ParamValue::Float(0.5),
        min: Some(0.0),
        max: Some(1.0),
        step: Some(0.01),
        unit: ParamUnit::Normalized,
        enum_values: Vec::new(),
    }
}

fn lut(name: &str) -> ResourceSpec {
    ResourceSpec {
        name: name.into(),
        kind: EffectResourceKind::Lut3d,
    }
}

fn definition(params: Vec<ParamSpec>, resources: Vec<ResourceSpec>) -> EffectDef {
    EffectDef {
        id: "test.schema".into(),
        version: 1,
        kind: EffectKind::Effect,
        inputs: EffectInputs::One,
        params,
        resources,
        wgsl_body: "fn effect(uv: vec2f, src: vec4f) -> vec4f { return src; }".into(),
        tier_min: CapabilityTier::TierB,
        license: None,
    }
}

fn refusal(definition: &EffectDef) -> Option<String> {
    validate_definition(definition).err().map(|diagnostics| {
        diagnostics
            .messages
            .iter()
            .map(|message| message.message.clone())
            .collect::<Vec<_>>()
            .join("\n")
    })
}

#[test]
#[allow(clippy::too_many_lines)]
fn every_schema_rule_refuses_what_it_names_and_accepts_its_boundary() {
    // Parameter and resource names are pasted into module-scope WGSL past the body's token
    // checks: the identifier rule alone keeps a name from declaring code of its own.
    let with = |edit: &dyn Fn(&mut EffectDef)| {
        let mut definition = definition(vec![float_param("amount")], vec![lut("grade")]);
        edit(&mut definition);
        definition
    };
    let param = |edit: &dyn Fn(&mut ParamSpec)| with(&|definition| edit(&mut definition.params[0]));
    let named = |name: &'static str| param(&move |param| param.name = name.into());
    let resource =
        |name: &'static str| with(&move |definition| definition.resources[0].name = name.into());
    let refused: Vec<(&str, EffectDef, &str)> = vec![
        (
            "empty id",
            with(&|d| d.id = String::new()),
            "effect id must contain",
        ),
        (
            "id with a space",
            with(&|d| d.id = "bad id".into()),
            "effect id must contain",
        ),
        (
            "id with a slash",
            with(&|d| d.id = "a/b".into()),
            "effect id must contain",
        ),
        (
            "version 0",
            with(&|d| d.version = 0),
            "version must be non-zero",
        ),
        (
            "effect with two inputs",
            with(&|d| d.inputs = EffectInputs::Two),
            "effects require one input",
        ),
        (
            "transition with one input",
            with(&|d| d.kind = EffectKind::Transition),
            "effects require one input",
        ),
        (
            "generator with an input",
            with(&|d| d.kind = EffectKind::Generator),
            "effects require one input",
        ),
        (
            "name declaring code",
            named("x: f32 }; fn spin() { loop {} } struct Pad { y"),
            "invalid WGSL parameter name",
        ),
        (
            "name with a colon",
            named("a:b"),
            "invalid WGSL parameter name",
        ),
        (
            "name with a brace",
            named("a}"),
            "invalid WGSL parameter name",
        ),
        (
            "name with a semicolon",
            named("a;b"),
            "invalid WGSL parameter name",
        ),
        (
            "name with a space",
            named("a b"),
            "invalid WGSL parameter name",
        ),
        (
            "non-ASCII name",
            named("größe"),
            "invalid WGSL parameter name",
        ),
        (
            "name starting with a digit",
            named("1x"),
            "invalid WGSL parameter name",
        ),
        ("empty name", named(""), "invalid WGSL parameter name"),
        (
            "reserved prefix",
            named("alpha2_amount"),
            "invalid WGSL parameter name",
        ),
        (
            "wrapper name params",
            named("params"),
            "invalid WGSL parameter name",
        ),
        (
            "wrapper name EffectParams",
            named("EffectParams"),
            "invalid WGSL parameter name",
        ),
        (
            "wrapper name sample_src",
            named("sample_src"),
            "invalid WGSL parameter name",
        ),
        (
            "duplicate parameter",
            with(&|d| d.params.push(float_param("amount"))),
            "duplicate parameter name",
        ),
        (
            "resource name declaring code",
            resource("x; fn spin() { loop {} } //"),
            "invalid or duplicate effect resource name",
        ),
        (
            "resource name with a colon",
            resource("a:b"),
            "invalid or duplicate effect resource name",
        ),
        (
            "resource name reserved",
            resource("alpha2_grade"),
            "invalid or duplicate effect resource name",
        ),
        (
            "resource named as a parameter",
            resource("amount"),
            "invalid or duplicate effect resource name",
        ),
        (
            "resource whose helper is the wrapper's sample_src",
            resource("src"),
            "invalid or duplicate effect resource name",
        ),
        (
            "duplicate resource",
            with(&|d| d.resources.push(lut("grade"))),
            "invalid or duplicate effect resource name",
        ),
        (
            "nine resources",
            with(&|d| d.resources = (0..9).map(|index| lut(&format!("lut{index}"))).collect()),
            "declares more than 8 resources",
        ),
        (
            "float with a vec2 default",
            param(&|p| p.default = ParamValue::Vec2([0.5; 2])),
            "incompatible type, default, and unit",
        ),
        (
            "float with a color unit",
            param(&|p| p.unit = ParamUnit::Color),
            "incompatible type, default, and unit",
        ),
        (
            "color with a normalized unit",
            param(&|p| {
                p.param_type = ParamType::Color;
                p.default = ParamValue::Color([1.0; 4]);
                (p.min, p.max, p.step) = (None, None, None);
            }),
            "incompatible type, default, and unit",
        ),
        (
            "NaN default",
            param(&|p| p.default = ParamValue::Float(f32::NAN)),
            "invalid numeric range",
        ),
        (
            "infinite minimum",
            param(&|p| p.min = Some(f32::NEG_INFINITY)),
            "invalid numeric range",
        ),
        (
            "infinite maximum",
            param(&|p| p.max = Some(f32::INFINITY)),
            "invalid numeric range",
        ),
        (
            "zero step",
            param(&|p| p.step = Some(0.0)),
            "invalid numeric range",
        ),
        (
            "negative step",
            param(&|p| p.step = Some(-0.1)),
            "invalid numeric range",
        ),
        (
            "inverted range",
            param(&|p| (p.min, p.max) = (Some(1.0), Some(0.0))),
            "invalid numeric range",
        ),
        (
            "bool with a range",
            param(&|p| {
                p.param_type = ParamType::Bool;
                p.default = ParamValue::Bool(true);
                p.unit = ParamUnit::Bool;
                (p.min, p.max, p.step) = (None, Some(1.0), None);
            }),
            "may declare a numeric range",
        ),
        (
            "default above the maximum",
            param(&|p| p.default = ParamValue::Float(1.5)),
            "outside its numeric range",
        ),
        (
            "default below the minimum",
            param(&|p| p.default = ParamValue::Float(-0.5)),
            "outside its numeric range",
        ),
        (
            "color default above 1",
            param(&|p| {
                p.param_type = ParamType::Color;
                p.default = ParamValue::Color([1.0, 1.5, 0.0, 1.0]);
                p.unit = ParamUnit::Color;
                (p.min, p.max, p.step) = (None, None, None);
            }),
            "must be inside 0..=1",
        ),
        (
            "enum without values",
            param(&|p| {
                p.param_type = ParamType::Enum;
                p.default = ParamValue::Enum(0);
                p.unit = ParamUnit::Enum;
                (p.min, p.max, p.step) = (None, None, None);
            }),
            "requires named values",
        ),
        (
            "enum default past its values",
            param(&|p| {
                p.param_type = ParamType::Enum;
                p.default = ParamValue::Enum(2);
                p.unit = ParamUnit::Enum;
                p.enum_values = vec!["a".into(), "b".into()];
                (p.min, p.max, p.step) = (None, None, None);
            }),
            "default is out of range",
        ),
    ];
    for (label, definition, expected) in &refused {
        let message = refusal(definition).unwrap_or_else(|| panic!("{label}: accepted"));
        assert!(message.contains(expected), "{label}: {message}");
    }
    let accepted: Vec<(&str, EffectDef)> = vec![
        ("the base definition", with(&|_| ())),
        ("underscore-led name", named("_amount")),
        ("name with digits", named("amount2")),
        (
            "name containing the prefix later",
            named("my_alpha2_amount"),
        ),
        (
            "id with every allowed symbol",
            with(&|d| d.id = "a-b_c.D9".into()),
        ),
        (
            "transition with two inputs",
            with(&|d| (d.kind, d.inputs) = (EffectKind::Transition, EffectInputs::Two)),
        ),
        (
            "eight resources",
            with(&|d| d.resources = (0..8).map(|index| lut(&format!("lut{index}"))).collect()),
        ),
        (
            "default on the minimum",
            param(&|p| p.default = ParamValue::Float(0.0)),
        ),
        (
            "default on the maximum",
            param(&|p| p.default = ParamValue::Float(1.0)),
        ),
        (
            "an empty range",
            param(&|p| {
                (p.min, p.max, p.default) = (Some(0.5), Some(0.5), ParamValue::Float(0.5));
            }),
        ),
        (
            "no range",
            param(&|p| (p.min, p.max, p.step) = (None, None, None)),
        ),
        ("degrees", param(&|p| p.unit = ParamUnit::Degrees)),
        (
            "color at 0 and 1",
            param(&|p| {
                p.param_type = ParamType::Color;
                p.default = ParamValue::Color([0.0, 1.0, 0.0, 1.0]);
                p.unit = ParamUnit::Color;
                (p.min, p.max, p.step) = (None, None, None);
            }),
        ),
        (
            "enum default on its last value",
            param(&|p| {
                p.param_type = ParamType::Enum;
                p.default = ParamValue::Enum(1);
                p.unit = ParamUnit::Enum;
                p.enum_values = vec!["a".into(), "b".into()];
                (p.min, p.max, p.step) = (None, None, None);
            }),
        ),
    ];
    for (label, definition) in &accepted {
        assert_eq!(refusal(definition), None, "{label}");
    }
}

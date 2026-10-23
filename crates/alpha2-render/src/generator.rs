//! The internal generator behind fill sources: a color or gradient over the
//! whole picture, drawn from parameters alone. Shapes are drawn by the vector
//! pass ([`crate::vector`]), with the same paint code.

use std::fmt::Write as _;

use alpha2_gpu::CapabilityTier;

use crate::{EffectDef, EffectKind, ParamSpec, ParamType, ParamUnit, ParamValue};

/// Internal identifier of the generator pass.
pub const GENERATOR_EFFECT_ID: &str = "alpha2.internal.generator";

/// Stops one paint carries. The project model allows as many.
pub const MAX_GENERATOR_STOPS: usize = 8;

/// Enum values of a paint's `kind` parameter.
pub const PAINT_KINDS: [&str; 5] = ["none", "solid", "linear", "radial", "conic"];

/// Enum values of a paint's `spread` parameter.
pub const PAINT_SPREADS: [&str; 3] = ["pad", "repeat", "reflect"];

/// The parameter names of one of the generator's paints, as
/// [`generator_definition`] declares them.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PaintParams {
    /// `{paint}_kind`.
    pub kind: &'static str,
    /// `{paint}_a`.
    pub a: &'static str,
    /// `{paint}_b`.
    pub b: &'static str,
    /// `{paint}_spread`.
    pub spread: &'static str,
    /// `{paint}_count`.
    pub count: &'static str,
    /// `{paint}_offsets0`.
    pub offsets0: &'static str,
    /// `{paint}_offsets1`.
    pub offsets1: &'static str,
    /// `{paint}_c0` to `{paint}_c7`, by stop.
    pub colors: [&'static str; MAX_GENERATOR_STOPS],
}

macro_rules! paint_params {
    ($paint:literal) => {
        PaintParams {
            kind: concat!($paint, "_kind"),
            a: concat!($paint, "_a"),
            b: concat!($paint, "_b"),
            spread: concat!($paint, "_spread"),
            count: concat!($paint, "_count"),
            offsets0: concat!($paint, "_offsets0"),
            offsets1: concat!($paint, "_offsets1"),
            colors: [
                concat!($paint, "_c0"),
                concat!($paint, "_c1"),
                concat!($paint, "_c2"),
                concat!($paint, "_c3"),
                concat!($paint, "_c4"),
                concat!($paint, "_c5"),
                concat!($paint, "_c6"),
                concat!($paint, "_c7"),
            ],
        }
    };
}

/// The fill paint's parameter names: static, so a caller sets a generator's
/// paints without formatting a name.
pub const FILL_PAINT_PARAMS: PaintParams = paint_params!("fill");

fn param(name: String, param_type: ParamType, default: ParamValue) -> ParamSpec {
    ParamSpec {
        name,
        param_type,
        default,
        min: None,
        max: None,
        step: None,
        unit: ParamUnit::Normalized,
        enum_values: Vec::new(),
    }
}

fn enum_param(name: String, values: &[&str]) -> ParamSpec {
    ParamSpec {
        unit: ParamUnit::Enum,
        enum_values: values.iter().map(|value| (*value).to_owned()).collect(),
        ..param(name, ParamType::Enum, ParamValue::Enum(0))
    }
}

/// The generator pass: the fill paint over the whole picture.
///
/// The paint (`fill_*`) has a `kind`, two points `a` and `b` (a linear
/// gradient's start and end; a radial gradient's centre and, in `b.x`,
/// radius; a conic gradient's centre and, in `b.x`, start angle in radians
/// clockwise from up), a `spread`, a stop `count`, stop offsets in
/// `offsets0` (first four) and `offsets1`, and straight sRGB stop colors
/// `c0` to `c7`. Stops interpolate premultiplied in linear light; the output
/// is premultiplied and sRGB-encoded.
#[must_use]
pub fn generator_definition() -> EffectDef {
    let names = FILL_PAINT_PARAMS;
    let mut params = vec![
        enum_param(names.kind.into(), &PAINT_KINDS),
        param(
            names.a.into(),
            ParamType::Vec2,
            ParamValue::Vec2([0.0, 0.0]),
        ),
        param(
            names.b.into(),
            ParamType::Vec2,
            ParamValue::Vec2([1.0, 0.0]),
        ),
        enum_param(names.spread.into(), &PAINT_SPREADS),
        param(names.count.into(), ParamType::Float, ParamValue::Float(0.0)),
        param(
            names.offsets0.into(),
            ParamType::Vec4,
            ParamValue::Vec4([0.0; 4]),
        ),
        param(
            names.offsets1.into(),
            ParamType::Vec4,
            ParamValue::Vec4([0.0; 4]),
        ),
    ];
    params.extend(names.colors.map(|name| ParamSpec {
        unit: ParamUnit::Color,
        ..param(name.into(), ParamType::Color, ParamValue::Color([0.0; 4]))
    }));
    EffectDef {
        id: GENERATOR_EFFECT_ID.into(),
        version: 1,
        kind: EffectKind::Generator,
        inputs: Vec::new(),
        params,
        resources: Vec::new(),
        wgsl_body: generator_body(),
        tier_min: CapabilityTier::TierB,
        license: Some(crate::builtins::ALPHA2_EFFECT_LICENSE.into()),
    }
}

fn paint_constructor(paint: &str) -> String {
    let mut colors = String::new();
    for stop in 0..MAX_GENERATOR_STOPS {
        let separator = if stop + 1 == MAX_GENERATOR_STOPS {
            ""
        } else {
            ", "
        };
        write!(colors, "params.{paint}_c{stop}{separator}")
            .expect("writing to a String cannot fail");
    }
    format!(
        "fn builtin_{paint}_paint() -> AlphaPaint {{
    let o0 = params.{paint}_offsets0;
    let o1 = params.{paint}_offsets1;
    return AlphaPaint(
        params.{paint}_kind,
        params.{paint}_a,
        params.{paint}_b,
        params.{paint}_spread,
        u32(clamp(params.{paint}_count, 0.0, {MAX_GENERATOR_STOPS}.0)),
        array<f32, {MAX_GENERATOR_STOPS}>(o0.x, o0.y, o0.z, o0.w, o1.x, o1.y, o1.z, o1.w),
        array<vec4f, {MAX_GENERATOR_STOPS}>({colors}),
    );
}}
"
    )
}

fn generator_body() -> String {
    let paint = paint_constructor("fill");
    format!(
        r"{paint_wgsl}
{paint}
fn generate(uv: vec2f) -> vec4f {{
    return builtin_generator_store(builtin_paint(builtin_fill_paint(), uv, resolution()));
}}
",
        paint_wgsl = crate::paint_wgsl::paint_wgsl(),
    )
}

#[cfg(test)]
mod tests {
    use super::{FILL_PAINT_PARAMS, generator_definition};

    #[test]
    fn the_paints_static_names_are_the_ones_the_pass_declares() {
        let declared = generator_definition()
            .params
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        let names = FILL_PAINT_PARAMS;
        let mut expected = Vec::new();
        let fields = [
            ("kind", names.kind),
            ("a", names.a),
            ("b", names.b),
            ("spread", names.spread),
            ("count", names.count),
            ("offsets0", names.offsets0),
            ("offsets1", names.offsets1),
        ];
        let stops = names
            .colors
            .iter()
            .enumerate()
            .map(|(stop, name)| (format!("c{stop}"), *name));
        for (field, name) in fields
            .into_iter()
            .map(|(field, name)| (field.to_owned(), name))
            .chain(stops)
        {
            assert_eq!(name, format!("fill_{field}"));
            expected.push(name.to_owned());
        }
        assert_eq!(declared, expected);
    }
}

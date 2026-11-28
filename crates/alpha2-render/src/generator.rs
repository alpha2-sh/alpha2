//! The internal generator behind fill sources: a color or gradient over the whole picture.
//! Shapes use the vector pass ([`crate::vector`]) with the same paint code.

use std::fmt::Write as _;

use alpha2_gpu::CapabilityTier;

use crate::{EffectDef, EffectKind, ParamSpec, ParamType, ParamUnit, ParamValue};

/// Internal identifier of the generator pass.
pub const GENERATOR_EFFECT_ID: &str = "alpha2.internal.generator";

/// Most stops per paint.
pub const MAX_GENERATOR_STOPS: usize = 16;

/// The `vec4` parameters holding a paint's stop offsets, four each.
pub const OFFSET_VECTORS: usize = MAX_GENERATOR_STOPS / 4;

/// Enum values of a paint's `kind` parameter.
pub const PAINT_KINDS: [&str; 5] = ["none", "solid", "linear", "radial", "conic"];

/// Enum values of a paint's `spread` parameter.
pub const PAINT_SPREADS: [&str; 3] = ["pad", "repeat", "reflect"];

/// Parameter names of one paint in [`generator_definition`].
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
    /// `{paint}_offsets0` to `{paint}_offsets3`, four stops each.
    pub offsets: [&'static str; OFFSET_VECTORS],
    /// `{paint}_c0` to `{paint}_c15`, by stop.
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
            offsets: [
                concat!($paint, "_offsets0"),
                concat!($paint, "_offsets1"),
                concat!($paint, "_offsets2"),
                concat!($paint, "_offsets3"),
            ],
            colors: [
                concat!($paint, "_c0"),
                concat!($paint, "_c1"),
                concat!($paint, "_c2"),
                concat!($paint, "_c3"),
                concat!($paint, "_c4"),
                concat!($paint, "_c5"),
                concat!($paint, "_c6"),
                concat!($paint, "_c7"),
                concat!($paint, "_c8"),
                concat!($paint, "_c9"),
                concat!($paint, "_c10"),
                concat!($paint, "_c11"),
                concat!($paint, "_c12"),
                concat!($paint, "_c13"),
                concat!($paint, "_c14"),
                concat!($paint, "_c15"),
            ],
        }
    };
}

/// The fill paint's parameter names, static so callers need not format them.
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
/// `fill_*` holds `kind`; points `a` and `b` (linear start and end; radial centre and radius
/// in `b.x`; conic centre and start angle in `b.x`, radians clockwise from up); `spread`;
/// stop `count`; offsets in `offsets0` to `offsets3`, four each; and straight sRGB colors
/// `c0` to `c15`. Stops interpolate premultiplied in linear light; output is premultiplied sRGB.
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
    ];
    params.extend(
        names
            .offsets
            .map(|name| param(name.into(), ParamType::Vec4, ParamValue::Vec4([0.0; 4]))),
    );
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
    let offsets = offset_list(|vector| format!("params.{paint}_offsets{vector}"));
    format!(
        "fn builtin_{paint}_paint() -> AlphaPaint {{
    return AlphaPaint(
        params.{paint}_kind,
        params.{paint}_a,
        params.{paint}_b,
        params.{paint}_spread,
        u32(clamp(params.{paint}_count, 0.0, {MAX_GENERATOR_STOPS}.0)),
        array<f32, {MAX_GENERATOR_STOPS}>({offsets}),
        array<vec4f, {MAX_GENERATOR_STOPS}>({colors}),
    );
}}
"
    )
}

pub(crate) fn offset_list(vector: impl Fn(usize) -> String) -> String {
    (0..OFFSET_VECTORS)
        .flat_map(|index| {
            let name = vector(index);
            ["x", "y", "z", "w"].map(|lane| format!("{name}.{lane}"))
        })
        .collect::<Vec<_>>()
        .join(", ")
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
            ("offsets0", names.offsets[0]),
            ("offsets1", names.offsets[1]),
            ("offsets2", names.offsets[2]),
            ("offsets3", names.offsets[3]),
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

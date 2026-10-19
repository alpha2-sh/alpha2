//! The internal generator behind generated sources: fills, gradients, and
//! shapes drawn from parameters alone.

use std::fmt::Write as _;

use alpha2_gpu::CapabilityTier;

use crate::{EffectDef, EffectInputs, EffectKind, ParamSpec, ParamType, ParamUnit, ParamValue};

/// Internal identifier of the generator pass.
pub const GENERATOR_EFFECT_ID: &str = "alpha2.internal.generator";

/// Stops one paint carries. The project model allows as many.
pub const MAX_GENERATOR_STOPS: usize = 8;

/// Enum values of the generator's `shape` parameter.
pub const GENERATOR_SHAPES: [&str; 3] = ["fill", "rectangle", "ellipse"];

/// Enum values of a paint's `kind` parameter.
pub const PAINT_KINDS: [&str; 5] = ["none", "solid", "linear", "radial", "conic"];

/// Enum values of a paint's `spread` parameter.
pub const PAINT_SPREADS: [&str; 3] = ["pad", "repeat", "reflect"];

/// The two paints a generator draws with, in drawing order.
const PAINTS: [&str; 2] = ["fill", "stroke"];

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

/// The stroke paint's parameter names.
pub const STROKE_PAINT_PARAMS: PaintParams = paint_params!("stroke");

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

/// The generator pass.
///
/// `shape` is `fill` (the fill paint everywhere) or a `rectangle` or
/// `ellipse` at `center` with full `size` (normalized), turned `rotation`
/// radians clockwise, with corner `radius` and `stroke_width` as fractions
/// of the shorter side. Each paint (`fill_*`, `stroke_*`) has a `kind`, two
/// points `a` and `b` (a linear gradient's start and end; a radial
/// gradient's centre and, in `b.x`, radius; a conic gradient's centre and,
/// in `b.x`, start angle in radians clockwise from up), a `spread`, a stop
/// `count`, stop offsets in `offsets0` (first four) and `offsets1`, and
/// straight sRGB stop colors `c0` to `c7`. Stops interpolate premultiplied in
/// linear light; the output is premultiplied and sRGB-encoded.
#[must_use]
pub fn generator_definition() -> EffectDef {
    let mut params = vec![
        enum_param("shape".into(), &GENERATOR_SHAPES),
        param(
            "center".into(),
            ParamType::Vec2,
            ParamValue::Vec2([0.5, 0.5]),
        ),
        param("size".into(), ParamType::Vec2, ParamValue::Vec2([1.0, 1.0])),
        param("rotation".into(), ParamType::Float, ParamValue::Float(0.0)),
        param("radius".into(), ParamType::Float, ParamValue::Float(0.0)),
        param(
            "stroke_width".into(),
            ParamType::Float,
            ParamValue::Float(0.0),
        ),
    ];
    for names in [FILL_PAINT_PARAMS, STROKE_PAINT_PARAMS] {
        params.extend([
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
        ]);
        params.extend(names.colors.map(|name| ParamSpec {
            unit: ParamUnit::Color,
            ..param(name.into(), ParamType::Color, ParamValue::Color([0.0; 4]))
        }));
    }
    EffectDef {
        id: GENERATOR_EFFECT_ID.into(),
        version: 1,
        kind: EffectKind::Generator,
        inputs: EffectInputs::None,
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
    let paints = PAINTS.map(paint_constructor).concat();
    format!(
        r"struct AlphaPaint {{
    kind: u32,
    a: vec2f,
    b: vec2f,
    spread: u32,
    count: u32,
    offsets: array<f32, {MAX_GENERATOR_STOPS}>,
    // Straight sRGB, as authored.
    colors: array<vec4f, {MAX_GENERATOR_STOPS}>,
}};

{shape_wgsl}
// Straight sRGB to premultiplied linear light; 1 stays exactly 1.
fn builtin_stop_color(color: vec4f) -> vec4f {{
    let c = clamp(color.rgb, vec3f(0.0), vec3f(1.0));
    let curve = pow((c + vec3f(0.055)) / 1.055, vec3f(2.4));
    let linear = select(select(curve, c / 12.92, c <= vec3f(0.04045)), vec3f(1.0), c >= vec3f(1.0));
    let alpha = clamp(color.a, 0.0, 1.0);
    return vec4f(linear * alpha, alpha);
}}

{paints}
// Where `uv` lies along the paint's gradient, before spread.
fn builtin_paint_position(paint: AlphaPaint, uv: vec2f) -> f32 {{
    let res = resolution();
    let p = uv * res;
    if (paint.kind == 2u) {{
        let start = paint.a * res;
        let direction = paint.b * res - start;
        return dot(p - start, direction) / max(dot(direction, direction), 0.000001);
    }}
    if (paint.kind == 3u) {{
        return length(p - paint.a * res) / max(paint.b.x * min(res.x, res.y), 0.000001);
    }}
    // Conic: clockwise from straight up (-y), a whole turn from 0 to 1.
    let offset = p - paint.a * res;
    if (dot(offset, offset) <= 0.0) {{ return 0.0; }}
    let angle = atan2(offset.x, -offset.y) - paint.b.x;
    let turn = angle / 6.283185307179586;
    return turn - floor(turn);
}}

fn builtin_paint_spread(paint: AlphaPaint, t: f32) -> f32 {{
    if (paint.kind == 4u) {{ return t; }}
    if (paint.spread == 1u) {{ return t - floor(t); }}
    if (paint.spread == 2u) {{
        let m = t - 2.0 * floor(t * 0.5);
        return select(m, 2.0 - m, m > 1.0);
    }}
    return clamp(t, 0.0, 1.0);
}}

// Premultiplied linear-light color of the paint at `uv`.
fn builtin_paint(paint: AlphaPaint, uv: vec2f) -> vec4f {{
    if (paint.kind == 0u || paint.count == 0u) {{ return vec4f(0.0); }}
    var colors = paint.colors;
    var offsets = paint.offsets;
    if (paint.kind == 1u || paint.count == 1u) {{ return builtin_stop_color(colors[0]); }}
    let t = builtin_paint_spread(paint, builtin_paint_position(paint, uv));
    if (t <= offsets[0]) {{ return builtin_stop_color(colors[0]); }}
    for (var stop = 1u; stop < {MAX_GENERATOR_STOPS}u; stop++) {{
        if (stop >= paint.count) {{ break; }}
        if (t <= offsets[stop]) {{
            let span = offsets[stop] - offsets[stop - 1u];
            let f = select((t - offsets[stop - 1u]) / span, 1.0, span <= 0.0);
            return mix(builtin_stop_color(colors[stop - 1u]), builtin_stop_color(colors[stop]), f);
        }}
    }}
    return builtin_stop_color(colors[paint.count - 1u]);
}}

// Premultiplied linear light to the premultiplied, sRGB-encoded storage
// value; 1 stays exactly 1.
fn builtin_generator_store(color: vec4f) -> vec4f {{
    if (color.a <= 0.0) {{ return vec4f(0.0); }}
    let c = clamp(color.rgb / color.a, vec3f(0.0), vec3f(1.0));
    let curve = 1.055 * pow(c, vec3f(1.0 / 2.4)) - vec3f(0.055);
    let encoded = select(select(curve, c * 12.92, c <= vec3f(0.0031308)), vec3f(1.0), c >= vec3f(1.0));
    return vec4f(encoded * color.a, color.a);
}}

fn generate(uv: vec2f) -> vec4f {{
    let fill = builtin_paint(builtin_fill_paint(), uv);
    if (params.shape == 0u) {{ return builtin_generator_store(fill); }}
    let res = resolution();
    let unit = min(res.x, res.y);
    let p = builtin_shape_local(uv, params.center, params.rotation);
    let extent = abs(params.size) * res;
    let d = builtin_shape_distance(p, extent * 0.5, max(params.radius, 0.0) * unit, params.shape == 2u);
    // One-pixel antialiased edges, centred on the outline. A shape or a stroke
    // narrower than that pixel fades with its width rather than keeping the
    // ramp's half coverage down to nothing.
    let inside = clamp(0.5 - d, 0.0, 1.0) * builtin_shape_extent_weight(extent, 1.0);
    let half_width = max(params.stroke_width, 0.0) * unit * 0.5;
    let on_stroke = select(0.0, clamp(0.5 - (abs(d) - half_width), 0.0, 1.0) * min(half_width * 2.0, 1.0), half_width > 0.0);
    let stroke = builtin_paint(builtin_stroke_paint(), uv) * on_stroke;
    return builtin_generator_store(stroke + fill * inside * (1.0 - stroke.a));
}}
",
        shape_wgsl = crate::masking::SHAPE_WGSL,
    )
}

#[cfg(test)]
mod tests {
    use super::{FILL_PAINT_PARAMS, STROKE_PAINT_PARAMS, generator_definition};

    #[test]
    fn each_paints_static_names_are_the_ones_the_pass_declares() {
        let declared = generator_definition()
            .params
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        let mut expected = [
            "shape",
            "center",
            "size",
            "rotation",
            "radius",
            "stroke_width",
        ]
        .map(str::to_owned)
        .to_vec();
        for (paint, names) in [("fill", FILL_PAINT_PARAMS), ("stroke", STROKE_PAINT_PARAMS)] {
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
                assert_eq!(name, format!("{paint}_{field}"));
                expected.push(name.to_owned());
            }
        }
        assert_eq!(declared, expected);
    }
}

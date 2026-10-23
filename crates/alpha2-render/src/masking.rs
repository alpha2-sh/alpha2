//! Internal effects behind clip shape masks and track mattes.
//!
//! Neither is visible to projects as an effect: the timeline engine inserts
//! them from a clip's `masks` and `matte`.

use std::fmt::Write as _;

use alpha2_gpu::CapabilityTier;

use crate::{EffectDef, EffectKind, ParamSpec, ParamType, ParamUnit, ParamValue};

/// Internal identifier of the shape-mask pass.
pub const SHAPE_MASK_EFFECT_ID: &str = "alpha2.internal.shape-mask";

/// Internal identifier of the track-matte pass.
pub const TRACK_MATTE_EFFECT_ID: &str = "alpha2.internal.track-matte";

/// Masks one shape-mask pass evaluates. The project model allows as many.
pub const MAX_SHAPE_MASKS: usize = 8;

/// Enum values of a mask's `mode` parameter.
pub const MASK_MODES: [&str; 3] = ["add", "subtract", "intersect"];

/// The parameter names of one mask of the shape-mask pass, as
/// [`shape_mask_definition`] declares them.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ShapeMaskParams {
    /// `m{i}_center`.
    pub center: &'static str,
    /// `m{i}_size`.
    pub size: &'static str,
    /// `m{i}_rotation`.
    pub rotation: &'static str,
    /// `m{i}_radius`.
    pub radius: &'static str,
    /// `m{i}_feather`.
    pub feather: &'static str,
    /// `m{i}_ellipse`.
    pub ellipse: &'static str,
    /// `m{i}_invert`.
    pub invert: &'static str,
    /// `m{i}_mode`.
    pub mode: &'static str,
}

macro_rules! shape_mask_params {
    ($($index:literal),*) => {
        [$(ShapeMaskParams {
            center: concat!("m", $index, "_center"),
            size: concat!("m", $index, "_size"),
            rotation: concat!("m", $index, "_rotation"),
            radius: concat!("m", $index, "_radius"),
            feather: concat!("m", $index, "_feather"),
            ellipse: concat!("m", $index, "_ellipse"),
            invert: concat!("m", $index, "_invert"),
            mode: concat!("m", $index, "_mode"),
        }),*]
    };
}

/// Each mask's parameter names, by mask index: static, so a caller sets a
/// frame's masks without formatting a name.
pub const SHAPE_MASK_PARAMS: [ShapeMaskParams; MAX_SHAPE_MASKS] =
    shape_mask_params!(0, 1, 2, 3, 4, 5, 6, 7);

fn param(name: String, param_type: ParamType, default: ParamValue) -> ParamSpec {
    ParamSpec {
        name,
        param_type,
        default,
        min: None,
        max: None,
        step: None,
        unit: match param_type {
            ParamType::Bool => ParamUnit::Bool,
            ParamType::Enum => ParamUnit::Enum,
            _ => ParamUnit::Normalized,
        },
        enum_values: if param_type == ParamType::Enum {
            MASK_MODES.map(str::to_owned).to_vec()
        } else {
            Vec::new()
        },
    }
}

/// The shape-mask pass: up to [`MAX_SHAPE_MASKS`] rectangles (with rounded
/// corners) or ellipses, each with a feathered edge, combined in order.
///
/// Per mask `i` the parameters are `m{i}_center`, `m{i}_size` (full extent),
/// `m{i}_rotation` (radians, clockwise), `m{i}_radius` and `m{i}_feather` (as
/// fractions of the shorter side of the pass), `m{i}_ellipse`,
/// `m{i}_invert`, and `m{i}_mode`; `count` says how many are in use.
#[must_use]
pub fn shape_mask_definition() -> EffectDef {
    let mut params = vec![param(
        "count".into(),
        ParamType::Float,
        ParamValue::Float(0.0),
    )];
    for names in SHAPE_MASK_PARAMS {
        params.extend([
            param(
                names.center.into(),
                ParamType::Vec2,
                ParamValue::Vec2([0.5, 0.5]),
            ),
            param(
                names.size.into(),
                ParamType::Vec2,
                ParamValue::Vec2([1.0, 1.0]),
            ),
            param(
                names.rotation.into(),
                ParamType::Float,
                ParamValue::Float(0.0),
            ),
            param(
                names.radius.into(),
                ParamType::Float,
                ParamValue::Float(0.0),
            ),
            param(
                names.feather.into(),
                ParamType::Float,
                ParamValue::Float(0.0),
            ),
            param(
                names.ellipse.into(),
                ParamType::Bool,
                ParamValue::Bool(false),
            ),
            param(
                names.invert.into(),
                ParamType::Bool,
                ParamValue::Bool(false),
            ),
            param(names.mode.into(), ParamType::Enum, ParamValue::Enum(0)),
        ]);
    }
    EffectDef {
        id: SHAPE_MASK_EFFECT_ID.into(),
        version: 1,
        kind: EffectKind::Effect,
        inputs: Vec::new(),
        params,
        resources: Vec::new(),
        wgsl_body: shape_mask_body(),
        tier_min: CapabilityTier::TierB,
        license: Some(crate::builtins::ALPHA2_EFFECT_LICENSE.into()),
    }
}

/// WGSL shared by the passes that draw analytic shapes: the pixel offset
/// from a shape's centre in its unrotated frame, and the signed distance from
/// there to its outline.
pub(crate) const SHAPE_WGSL: &str = r"// Pixel offset of `uv` from `center`, in the frame of a shape turned
// `rotation` radians clockwise.
fn builtin_shape_local(uv: vec2f, center: vec2f, rotation: f32) -> vec2f {
    let offset = (uv - center) * resolution();
    let c = cos(rotation);
    let s = sin(rotation);
    return vec2f(c * offset.x + s * offset.y, -s * offset.x + c * offset.y);
}

// Signed distance, in pixels, from `p` (relative to the centre, unrotated)
// to the outline of a shape of half extent `half`: negative inside.
fn builtin_shape_distance(p: vec2f, half: vec2f, radius: f32, ellipse: bool) -> f32 {
    if (ellipse) {
        // The gradient-normalized approximation, exact on the axes and
        // within a fraction of a pixel near the outline, where it matters.
        let h = max(half, vec2f(0.0001));
        let k0 = length(p / h);
        let k1 = length(p / (h * h));
        if (k1 <= 0.0) { return -min(h.x, h.y); }
        return k0 * (k0 - 1.0) / k1;
    }
    let r = min(radius, min(half.x, half.y));
    let q = abs(p) - half + vec2f(r);
    return length(max(q, vec2f(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

// The share of its edge-ramp coverage a shape of full `extent` (pixels) keeps
// with an antialiased edge `edge` pixels wide: all of it once each side spans
// the edge, and in proportion to each side below that. The ramp, centred on
// the outline, alone keeps half coverage at the centre of a shape of no size;
// with this, a shape shrinking to nothing fades out with its area instead of
// ending on a half-covered dot. Exactly one from `edge` up.
fn builtin_shape_extent_weight(extent: vec2f, edge: f32) -> f32 {
    let w = clamp(extent / max(edge, 0.0001), vec2f(0.0), vec2f(1.0));
    return w.x * w.y;
}
";

fn shape_mask_body() -> String {
    let mut masks = String::new();
    for index in 0..MAX_SHAPE_MASKS {
        let separator = if index + 1 == MAX_SHAPE_MASKS {
            ""
        } else {
            ","
        };
        writeln!(
            masks,
            "        AlphaMask(params.m{index}_center, params.m{index}_size, \
             params.m{index}_rotation, params.m{index}_radius, params.m{index}_feather, \
             params.m{index}_ellipse, params.m{index}_invert, params.m{index}_mode){separator}"
        )
        .expect("writing to a String cannot fail");
    }
    format!(
        r"struct AlphaMask {{
    center: vec2f,
    size: vec2f,
    rotation: f32,
    radius: f32,
    feather: f32,
    ellipse: u32,
    invert: u32,
    mode: u32,
}};

{SHAPE_WGSL}
fn builtin_mask_coverage(uv: vec2f, mask: AlphaMask) -> f32 {{
    let res = resolution();
    let unit = min(res.x, res.y);
    let p = builtin_shape_local(uv, mask.center, mask.rotation);
    let extent = abs(mask.size) * res;
    let d = builtin_shape_distance(p, extent * 0.5, max(mask.radius, 0.0) * unit, mask.ellipse != 0u);
    // A one-pixel edge at least, so a sharp mask is still antialiased.
    let width = max(max(mask.feather, 0.0) * unit, 1.0);
    var coverage = clamp(0.5 - d / width, 0.0, 1.0) * builtin_shape_extent_weight(extent, width);
    if (mask.invert != 0u) {{ coverage = 1.0 - coverage; }}
    return coverage;
}}

fn effect(uv: vec2f, src: vec4f) -> vec4f {{
    var masks = array<AlphaMask, {MAX_SHAPE_MASKS}>(
{masks}    );
    let count = u32(clamp(params.count, 0.0, {MAX_SHAPE_MASKS}.0));
    // Empty when the first mask adds, full otherwise.
    var coverage = select(1.0, 0.0, masks[0].mode == 0u);
    for (var index = 0u; index < {MAX_SHAPE_MASKS}u; index++) {{
        if (index >= count) {{ break; }}
        let mask = masks[index];
        let m = builtin_mask_coverage(uv, mask);
        if (mask.mode == 1u) {{
            coverage = coverage * (1.0 - m);
        }} else if (mask.mode == 2u) {{
            coverage = coverage * m;
        }} else {{
            coverage = coverage + m - coverage * m;
        }}
    }}
    if (count == 0u) {{ return src; }}
    return src * coverage;
}}
"
    )
}

/// The track-matte pass: the first input kept where the second input's
/// alpha (or luminance over black: BT.709 weights on linear light, times
/// alpha) says, optionally inverted.
///
/// It is a two-input transition and ignores progress; pass one inside
/// `(0, 1)` so its body runs.
#[must_use]
pub fn track_matte_definition() -> EffectDef {
    EffectDef {
        id: TRACK_MATTE_EFFECT_ID.into(),
        version: 1,
        kind: EffectKind::Transition,
        inputs: Vec::new(),
        params: vec![
            ParamSpec {
                unit: ParamUnit::Bool,
                ..param("luma".into(), ParamType::Bool, ParamValue::Bool(false))
            },
            ParamSpec {
                unit: ParamUnit::Bool,
                ..param("inverted".into(), ParamType::Bool, ParamValue::Bool(false))
            },
        ],
        resources: Vec::new(),
        wgsl_body: r"fn builtin_matte_linear(value: f32) -> f32 {
    if (value <= 0.04045) { return value / 12.92; }
    return pow((value + 0.055) / 1.055, 2.4);
}

fn transition(uv: vec2f) -> vec4f {
    let picture = from_color(uv);
    let matte = to_color(uv);
    var coverage = matte.a;
    if (params.luma != 0u) {
        let straight = clamp(matte.rgb / max(matte.a, 0.000001), vec3f(0.0), vec3f(1.0));
        let linear = vec3f(
            builtin_matte_linear(straight.r),
            builtin_matte_linear(straight.g),
            builtin_matte_linear(straight.b),
        );
        coverage = dot(linear, vec3f(0.2126, 0.7152, 0.0722)) * matte.a;
    }
    if (params.inverted != 0u) { coverage = 1.0 - coverage; }
    return picture * clamp(coverage, 0.0, 1.0);
}
"
        .into(),
        tier_min: CapabilityTier::TierB,
        license: Some(crate::builtins::ALPHA2_EFFECT_LICENSE.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_SHAPE_MASKS, SHAPE_MASK_PARAMS, shape_mask_definition};

    #[test]
    fn each_masks_static_names_are_the_ones_the_pass_declares() {
        let declared = shape_mask_definition()
            .params
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        let mut expected = vec!["count".to_owned()];
        for (index, names) in SHAPE_MASK_PARAMS.iter().enumerate() {
            let fields = [
                ("center", names.center),
                ("size", names.size),
                ("rotation", names.rotation),
                ("radius", names.radius),
                ("feather", names.feather),
                ("ellipse", names.ellipse),
                ("invert", names.invert),
                ("mode", names.mode),
            ];
            for (field, name) in fields {
                assert_eq!(name, format!("m{index}_{field}"));
                expected.push(name.to_owned());
            }
        }
        assert_eq!(declared, expected);
        assert_eq!(declared.len(), 1 + 8 * MAX_SHAPE_MASKS);
    }
}

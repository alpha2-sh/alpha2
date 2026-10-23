//! Fill sources as parameters of the renderer's generator pass.

use alpha2_model::{Generator, GradientStop, Paint, Rgba, Spread, Vec2};
use alpha2_render::{
    EffectParams, FILL_PAINT_PARAMS, MAX_GENERATOR_STOPS, PaintParams, ParamValue,
};

// Every stop a valid project holds reaches the pass.
const _: () = assert!(alpha2_model::MAX_GRADIENT_STOPS <= MAX_GENERATOR_STOPS);

/// Parameters one generator sets at most: its paint's seven plus a color per
/// stop.
const MAX_GENERATOR_PARAMS: usize = 7 + MAX_GENERATOR_STOPS;

/// The generator pass's parameters for `generator`. Every name is static, so
/// the list holding them is all this allocates.
pub(crate) fn generator_params(generator: &Generator) -> EffectParams<'static> {
    let params = EffectParams::with_capacity(MAX_GENERATOR_PARAMS);
    match generator {
        Generator::Fill { paint } => with_paint(params, &FILL_PAINT_PARAMS, paint),
        // Shapes are drawn by the vector pass (`frame::shapes_node`) and text from its prepared
        // texture (`frame::PlacedText`), never by this one.
        Generator::Shapes { .. } | Generator::Text { .. } => params,
    }
}

const fn vec2(value: Vec2) -> ParamValue {
    ParamValue::Vec2([value.x, value.y])
}

const fn color(value: Rgba) -> ParamValue {
    ParamValue::Color([value.red, value.green, value.blue, value.alpha])
}

const fn spread(value: Spread) -> u32 {
    match value {
        Spread::Pad => 0,
        Spread::Repeat => 1,
        Spread::Reflect => 2,
    }
}

fn with_paint(
    params: EffectParams<'static>,
    names: &PaintParams,
    paint: &Paint,
) -> EffectParams<'static> {
    let (kind, a, b, spread, stops): (u32, Vec2, Vec2, u32, &[GradientStop]) = match paint {
        Paint::Solid { color: value } => {
            return params
                .with(names.kind, ParamValue::Enum(1))
                .with(names.count, ParamValue::Float(1.0))
                .with(names.colors[0], color(*value));
        }
        Paint::LinearGradient {
            start,
            end,
            stops,
            spread: mode,
        } => (2, *start, *end, spread(*mode), stops),
        Paint::RadialGradient {
            center,
            radius,
            stops,
            spread: mode,
        } => (3, *center, Vec2::new(*radius, 0.0), spread(*mode), stops),
        Paint::ConicGradient {
            center,
            angle_degrees,
            stops,
        } => (
            4,
            *center,
            Vec2::new(angle_degrees.to_radians(), 0.0),
            0,
            stops,
        ),
    };
    let mut offsets = [0.0_f32; MAX_GENERATOR_STOPS];
    let mut params = params
        .with(names.kind, ParamValue::Enum(kind))
        .with(names.a, vec2(a))
        .with(names.b, vec2(b))
        .with(names.spread, ParamValue::Enum(spread));
    for ((stop, name), offset) in stops.iter().zip(names.colors).zip(&mut offsets) {
        *offset = stop.offset;
        params.insert(name, color(stop.color));
    }
    #[allow(clippy::cast_precision_loss)]
    let count = stops.len().min(MAX_GENERATOR_STOPS) as f32;
    params
        .with(names.count, ParamValue::Float(count))
        .with(
            names.offsets0,
            ParamValue::Vec4([offsets[0], offsets[1], offsets[2], offsets[3]]),
        )
        .with(
            names.offsets1,
            ParamValue::Vec4([offsets[4], offsets[5], offsets[6], offsets[7]]),
        )
}

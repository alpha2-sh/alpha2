//! The generator pass: paints (solid, gradients) over the picture.

use super::{CpuKernel, KernelContext, shade_row};
use crate::cpu::image::PixelBox;
use crate::cpu::paint::{Paint, encode};
use crate::{FILL_PAINT_PARAMS, GENERATOR_EFFECT_ID, MAX_GENERATOR_STOPS};

/// The kernel for the built-in `id` (version 1), if this module has it.
pub(super) fn kernel(id: &str) -> Option<&'static dyn CpuKernel> {
    (id == GENERATOR_EFFECT_ID).then_some(&Generator as &dyn CpuKernel)
}

/// `generate(uv)`: `builtin_generator_store(builtin_paint(fill, uv, resolution()))`.
struct Generator;

/// `builtin_fill_paint()`: the paint the `fill_*` parameters carry.
fn fill_paint(context: &KernelContext<'_>) -> Paint {
    let names = FILL_PAINT_PARAMS;
    let vectors = names.offsets.map(|name| context.vec4(name));
    // `u32(clamp(count, 0.0, 16.0))`: truncated.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let count = context
        .float(names.count)
        .clamp(0.0, MAX_GENERATOR_STOPS as f32) as usize;
    Paint::new(
        context.choice(names.kind),
        context.vec2(names.a),
        context.vec2(names.b),
        context.choice(names.spread),
        count,
        std::array::from_fn(|stop| vectors[stop / 4][stop % 4]),
        names.colors.map(|name| context.vec4(name)),
    )
}

impl CpuKernel for Generator {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        if fill_paint(context).is_none() {
            return PixelBox::EMPTY;
        }
        PixelBox::whole(context.size)
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let paint = fill_paint(context);
        if let Some(color) = paint.constant() {
            out.fill(encode(color));
            return;
        }
        let res = context.resolution();
        shade_row(context, y, x0, out, |_, uv| encode(paint.at(uv, res)));
    }
}

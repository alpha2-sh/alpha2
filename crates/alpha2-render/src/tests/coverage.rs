use super::*;

/// Whether the transition of two pictures under coverage `m` is `m` over their transition.
fn commutes_with_coverage(
    renderer: &Renderer,
    id: &str,
    params: &EffectParams,
    size: RenderSize,
) -> bool {
    let pixels = 0..size.width * size.height;
    let pattern = |seed: u32| {
        pixels
            .clone()
            .flat_map(|index| {
                let value = |channel: u32| {
                    let mixed =
                        index.wrapping_mul(2_654_435_761) ^ ((seed + channel).wrapping_mul(40_503));
                    u8::try_from(mixed % 256).unwrap_or(0)
                };
                [value(0), value(1), value(2), 255]
            })
            .collect::<Vec<u8>>()
    };
    // 4-pixel checks of full and quarter coverage, so a picture read from elsewhere shows.
    let coverage = pixels
        .clone()
        .map(|index| {
            let (x, y) = (index % size.width, index / size.width);
            if (x / 4 + y / 4) % 2 == 0 { 4 } else { 1 }
        })
        .collect::<Vec<u16>>();
    let cover = |pixels: &[u8]| {
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .zip(&coverage)
            .flat_map(|(pixel, quarters)| {
                pixel.iter().map(move |value| {
                    u8::try_from((u16::from(*value) * quarters + 2) / 4).unwrap_or(u8::MAX)
                })
            })
            .collect::<Vec<u8>>()
    };
    let (from, to) = (pattern(1), pattern(7));
    let render = |from: &[u8], to: &[u8], progress: f32| {
        let from = rgba_image_texture(renderer, size, from);
        let to = rgba_image_texture(renderer, size, to);
        let target = renderer
            .render_effect(
                &crate::EffectId::new(id.into(), 1),
                &from.create_view(&wgpu::TextureViewDescriptor::default()),
                Some(&to.create_view(&wgpu::TextureViewDescriptor::default())),
                size,
                params,
                EffectRuntime {
                    progress,
                    ratio: 1.0,
                    ..EffectRuntime::default()
                },
            )
            .expect("the transition renders");
        read_rgba8(renderer, &target)
    };
    [0.35, 0.65].into_iter().all(|progress| {
        let covered = render(&cover(&from), &cover(&to), progress);
        let mixed = cover(&render(&from, &to, progress));
        covered.iter().zip(&mixed).all(|(a, b)| a.abs_diff(*b) <= 3)
    })
}

#[test]
fn transitions_marked_coverage_commuting_are_the_ones_that_commute() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    renderer
        .register_builtin_transitions()
        .expect("transitions compile");
    let size = RenderSize::new(64, 64).expect("non-zero size");
    let found = crate::builtins::builtin_transition_definitions()
        .into_iter()
        .filter(|definition| {
            variations(definition)
                .iter()
                .all(|params| commutes_with_coverage(&renderer, &definition.id, params, size))
        })
        .map(|definition| definition.id)
        .collect::<Vec<_>>();
    assert_eq!(
        found,
        crate::builtins::COVERAGE_COMMUTING_TRANSITIONS,
        "COVERAGE_COMMUTING_TRANSITIONS lists the transitions that commute with coverage"
    );
}

/// The defaults, then each parameter alone at its ends.
fn variations(definition: &crate::EffectDef) -> Vec<EffectParams<'_>> {
    let mut all = vec![EffectParams::new()];
    for spec in &definition.params {
        let ends = |default: f32| {
            [spec.min, spec.max]
                .into_iter()
                .map(|end| end.unwrap_or(default + 0.5))
                .collect::<Vec<_>>()
        };
        let values = match &spec.default {
            ParamValue::Float(default) => {
                ends(*default).into_iter().map(ParamValue::Float).collect()
            }
            ParamValue::Vec2(default) => ends(default[0])
                .into_iter()
                .map(|end| ParamValue::Vec2([end; 2]))
                .collect(),
            ParamValue::Vec4(default) => ends(default[0])
                .into_iter()
                .map(|end| ParamValue::Vec4([end; 4]))
                .collect(),
            ParamValue::Color(_) => vec![ParamValue::Color([1.0, 0.5, 0.25, 1.0])],
            ParamValue::Enum(_) => (0..u32::try_from(spec.enum_values.len()).unwrap_or(0))
                .map(ParamValue::Enum)
                .collect(),
            ParamValue::Bool(default) => vec![ParamValue::Bool(!default)],
        };
        all.extend(
            values
                .into_iter()
                .map(|value| EffectParams::new().with(&spec.name, value)),
        );
    }
    all
}

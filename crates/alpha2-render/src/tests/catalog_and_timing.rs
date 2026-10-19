//! The built-in catalogue, effect schemas, and GPU pass timing.

use super::*;

#[test]
fn builtin_transition_catalog_compiles_and_preserves_endpoints() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let ids = pollster::block_on(renderer.register_builtin_transitions())
        .expect("built-in transitions compile");
    assert_eq!(ids.len(), 30);
    renderer.warmup(&ids).expect("catalog is warm");

    let from_pixel = [240, 32, 16, 255];
    let to_pixel = [8, 64, 224, 255];
    let from = rgba_texture(&renderer, from_pixel);
    let to = rgba_texture(&renderer, to_pixel);
    let from_view = from.create_view(&wgpu::TextureViewDescriptor::default());
    let to_view = to.create_view(&wgpu::TextureViewDescriptor::default());
    let size = RenderSize::new(1, 1).expect("non-zero size");

    for id in &ids {
        let definition = renderer.effect_definition(id).expect("definition exists");
        assert!(definition.license.as_deref().is_some_and(|license| {
            license.contains("MIT") && license.contains("gl-transitions")
        }));
        for (progress, expected) in [(0.0, from_pixel), (1.0, to_pixel)] {
            let target = renderer
                .render_effect(
                    id,
                    &from_view,
                    Some(&to_view),
                    size,
                    &EffectParams::new(),
                    EffectRuntime {
                        progress,
                        ..EffectRuntime::default()
                    },
                )
                .expect("transition endpoint renders");
            assert_eq!(
                read_rgba8(&renderer, &target),
                expected,
                "{} did not preserve progress={progress}",
                id.as_str()
            );
        }
    }

    let preview_size = RenderSize::new(8, 8).expect("non-zero size");
    let from_pixels = transition_test_image(preview_size, false);
    let to_pixels = transition_test_image(preview_size, true);
    let from = rgba_image_texture(&renderer, preview_size, &from_pixels);
    let to = rgba_image_texture(&renderer, preview_size, &to_pixels);
    let from_view = from.create_view(&wgpu::TextureViewDescriptor::default());
    let to_view = to.create_view(&wgpu::TextureViewDescriptor::default());
    let frames = ids
        .iter()
        .map(|id| {
            let target = renderer
                .render_effect(
                    id,
                    &from_view,
                    Some(&to_view),
                    preview_size,
                    &EffectParams::new(),
                    EffectRuntime {
                        progress: 0.37,
                        ratio: 1.0,
                        ..EffectRuntime::default()
                    },
                )
                .expect("transition midpoint renders");
            read_rgba8(&renderer, &target)
        })
        .collect::<Vec<_>>();
    let row_bytes = usize::try_from(preview_size.width).expect("width fits usize") * 4;
    let rows = usize::try_from(preview_size.height).expect("height fits usize");
    let mut contact_sheet = Vec::with_capacity(row_bytes * frames.len() * rows);
    for row in 0..rows {
        for frame in &frames {
            contact_sheet.extend_from_slice(&frame[row * row_bytes..(row + 1) * row_bytes]);
        }
    }
    let labels = ids.iter().map(crate::EffectId::as_str).collect::<Vec<_>>();
    assert_rgba8_golden_tiles(
        // One golden for every GPU: anti-aliasing uses `fwidthFine`; plain `fwidth` differs between
        // coarse-derivative GPUs (RADV, lavapipe) and fine ones (NVIDIA, Metal).
        &["builtin_transitions"],
        RenderSize::new(
            preview_size.width * u32::try_from(ids.len()).expect("catalog size fits u32"),
            preview_size.height,
        )
        .expect("non-zero size"),
        &labels,
        &contact_sheet,
        2,
    );
}

#[test]
fn complete_builtin_catalog_compiles_and_survives_host_cleanup() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let ids = pollster::block_on(renderer.register_builtins())
        .expect("the complete built-in catalog compiles");
    assert_eq!(
        ids.len(),
        crate::builtin_effect_definitions().len() + crate::builtin_transition_definitions().len()
    );
    let internal = crate::EffectId::new("alpha2.internal.gaussian-blur-pass".into(), 1);
    assert!(renderer.is_effect_registered(&internal));
    assert!(!renderer.is_effect_project_visible(&internal));
    // An internal pass is no project's to name, however it is asked for.
    assert_eq!(
        renderer.project_effect_id("alpha2.internal.gaussian-blur-pass", 1),
        None
    );
    assert!(renderer.is_effect_registered(&crate::EffectId::from_static(
        crate::SHAPE_MASK_EFFECT_ID,
        1
    )));
    renderer.clear_effects();
    for definition in crate::builtin_effect_definitions()
        .into_iter()
        .chain(crate::builtin_transition_definitions())
    {
        // The renderer answers a project's text and version with the identity it registered.
        assert_eq!(
            renderer
                .project_effect_id(&definition.id, definition.version)
                .map(crate::EffectId::as_str),
            Some(definition.id.as_str())
        );
        assert_eq!(
            renderer.project_effect_id(&definition.id, definition.version + 1),
            None
        );
        let id = crate::EffectId::new(definition.id, definition.version);
        assert!(renderer.is_effect_registered(&id));
        assert!(renderer.is_effect_project_visible(&id));
        assert!(!renderer.unregister_effect(&id));
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_pass_timed_while_the_probe_is_busy_renders_untimed_and_leaves_the_probe_to_its_reader() {
    // The one timestamp probe is released by its read's map callback; frames that wait on a
    // readback never find it busy, so this test holds it to run the busy-skip path.
    let _lock = gpu_test_lock();
    let mut timed = Renderer::new(gpu());
    let mut untimed = Renderer::new(gpu());
    let definition = || {
        runtime_tint_effect(
            "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
        )
    };
    let id = pollster::block_on(timed.register_effect(definition())).expect("effect compiles");
    pollster::block_on(untimed.register_effect(definition())).expect("effect compiles");
    timed.set_pass_timing_enabled(true);
    let size = RenderSize::new(16, 16).expect("size");
    let input = rgba_image_texture(&timed, size, &[40, 160, 240, 255].repeat(256));
    let view = input.create_view(&wgpu::TextureViewDescriptor::default());
    let scope = timed
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    // Each frame tints differently, so no earlier picture can stand in for it.
    let render = |renderer: &Renderer, frame: u8| {
        let level = f32::from(frame + 1) / 40.0;
        renderer
            .render_effect(
                &id,
                &view,
                None,
                size,
                &EffectParams::new().with("tint", ParamValue::Color([level, level, level, 1.0])),
                EffectRuntime::default(),
            )
            .expect("the effect renders")
    };
    let expected = |frame: u8| read_rgba8(&untimed, &render(&untimed, frame));
    let timestamps = |renderer: &Renderer| {
        renderer
            .diagnostics()
            .passes
            .iter()
            .filter(|pass| pass.source == crate::TimingSource::GpuTimestamp)
            .count()
    };
    // Lets the probe's map callback run, as a host's next frame would.
    let settle = |renderer: &Renderer| {
        for _ in 0..100 {
            let _ = renderer
                .gpu
                .device()
                .poll(wgpu::PollType::wait_indefinitely());
            if renderer
                .timestamp_probe
                .get()
                .and_then(Option::as_ref)
                .is_none_or(|probe| !probe.busy.load(Ordering::Acquire))
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("the timestamp read never completed");
    };

    assert_eq!(read_rgba8(&timed, &render(&timed, 0)), expected(0));
    settle(&timed);
    let Some(probe) = timed.timestamp_probe.get().and_then(Option::as_ref) else {
        // An adapter without timestamp queries times nothing; the picture must still be the
        // same.
        assert!(!timed.gpu.capabilities().timestamp_queries);
        assert_eq!(timestamps(&timed), 0);
        eprintln!("no timestamp queries on this adapter: the probe paths do not exist here");
        assert!(pollster::block_on(scope.pop()).is_none());
        return;
    };
    let after_first = timestamps(&timed);
    assert_eq!(after_first, 1, "an idle probe times the pass");

    // An earlier read still in flight holds the probe.
    assert!(probe.acquire(), "the probe is idle between frames");
    assert_eq!(read_rgba8(&timed, &render(&timed, 1)), expected(1));
    // The callbacks it can run are run; the probe stays with whoever took it.
    let _ = timed.gpu.device().poll(wgpu::PollType::wait_indefinitely());
    assert_eq!(
        timestamps(&timed),
        after_first,
        "a busy probe times nothing"
    );
    assert!(
        probe.busy.load(Ordering::Acquire),
        "a pass that did not take the probe does not release it"
    );
    // Its reader finishes; the next pass takes it again.
    probe.busy.store(false, Ordering::Release);
    assert_eq!(read_rgba8(&timed, &render(&timed, 2)), expected(2));
    settle(&timed);
    assert_eq!(
        timestamps(&timed),
        after_first + 1,
        "the probe is taken again"
    );

    // Back to back, a read is in flight while the next pass encodes, so some frames are timed
    // and some not.
    let outputs = (3..19)
        .map(|frame| render(&timed, frame))
        .collect::<Vec<_>>();
    for (frame, output) in (3..19).zip(&outputs) {
        assert_eq!(read_rgba8(&timed, output), expected(frame), "frame {frame}");
    }
    settle(&timed);
    let timed_frames = timestamps(&timed) - after_first - 1;
    assert!(
        (1..=16).contains(&timed_frames),
        "{timed_frames} of 16 back-to-back frames were timed"
    );
    assert!(pollster::block_on(scope.pop()).is_none());
}

#[test]
fn effect_schema_rejects_invalid_defaults_before_compilation() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let mut definition = runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    );
    definition.params[0].default = ParamValue::Color([1.5, 1.0, 1.0, 1.0]);
    let diagnostics = pollster::block_on(renderer.register_effect(definition))
        .expect_err("out-of-range color default is rejected");
    assert!(diagnostics.messages[0].message.contains("0..=1"));
}

/// Polls until the renderer's timestamp probe is free, so its read's map
/// callback has run, as a host's next frame would let it; false when the
/// adapter has no timestamp queries.
fn settle_timestamp_probe(renderer: &Renderer) -> bool {
    for _ in 0..1000 {
        let _ = renderer
            .gpu
            .device()
            .poll(wgpu::PollType::wait_indefinitely());
        match renderer.timestamp_probe.get().and_then(Option::as_ref) {
            None => return false,
            Some(probe) if !probe.busy.load(Ordering::Acquire) => return true,
            Some(_) => std::thread::sleep(std::time::Duration::from_millis(1)),
        }
    }
    panic!("the timestamp read never completed");
}

#[test]
fn a_timestamp_read_that_lands_after_timing_is_turned_off_records_nothing() {
    // The late map callback must not write an "effect" entry into the history cleared by
    // turning timing off.
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let id = pollster::block_on(renderer.register_effect(runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    )))
    .expect("effect compiles");
    let size = RenderSize::new(2048, 2048).expect("size");
    let input = rgba_image_texture(&renderer, size, &[40, 160, 240, 255].repeat(2048 * 2048));
    let view = input.create_view(&wgpu::TextureViewDescriptor::default());
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let render = |renderer: &Renderer| {
        renderer
            .render_effect(
                &id,
                &view,
                None,
                size,
                &EffectParams::new(),
                EffectRuntime::default(),
            )
            .expect("the effect renders")
    };
    // On, a frame, off before its timestamp is read.
    renderer.set_pass_timing_enabled(true);
    let frame = render(&renderer);
    renderer.set_pass_timing_enabled(false);
    let timestamps = settle_timestamp_probe(&renderer);
    assert_eq!(renderer.diagnostics().passes, Vec::new(), "timing is off");
    drop(frame);

    // Off and on again before it is read: the read belongs to the timing that was turned
    // off.
    renderer.set_pass_timing_enabled(true);
    let frame = render(&renderer);
    renderer.set_pass_timing_enabled(false);
    renderer.set_pass_timing_enabled(true);
    settle_timestamp_probe(&renderer);
    assert_eq!(
        renderer.diagnostics().passes,
        Vec::new(),
        "nothing was timed since timing was turned on"
    );
    drop(frame);

    // A frame timed while timing stays on is still recorded.
    let frame = render(&renderer);
    settle_timestamp_probe(&renderer);
    let passes = renderer.diagnostics().passes;
    assert!(
        passes
            .iter()
            .any(|pass| pass.source == crate::TimingSource::CpuEncoding)
    );
    if timestamps {
        assert!(
            passes
                .iter()
                .any(|pass| pass.source == crate::TimingSource::GpuTimestamp),
            "{passes:?}"
        );
    } else {
        eprintln!("no timestamp queries on this adapter: only CPU encoding is timed");
    }
    drop(frame);
    assert!(pollster::block_on(scope.pop()).is_none());
}

#[test]
fn a_transition_or_generator_pass_records_its_gpu_time_under_its_own_kind() {
    // Effects, transitions and generators share one timestamp probe; the read records the
    // node's own kind.
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let definition = |id: &str, kind, inputs, wgsl_body: &str| EffectDef {
        id: id.into(),
        version: 1,
        kind,
        inputs,
        params: Vec::new(),
        resources: Vec::new(),
        wgsl_body: wgsl_body.into(),
        tier_min: CapabilityTier::TierB,
        license: Some("MIT".into()),
    };
    let transition = pollster::block_on(renderer.register_effect(definition(
        "test.timed-crossfade",
        EffectKind::Transition,
        EffectInputs::Two,
        "fn transition(uv: vec2f) -> vec4f { return mix(from_color(uv), to_color(uv), progress()); }",
    )))
    .expect("transition compiles");
    let generator = pollster::block_on(renderer.register_effect(definition(
        "test.timed-gradient",
        EffectKind::Generator,
        EffectInputs::None,
        "fn generate(uv: vec2f) -> vec4f { return vec4f(uv, 0.5, 1.0); }",
    )))
    .expect("generator compiles");
    let size = RenderSize::new(64, 64).expect("size");
    let from = rgba_image_texture(&renderer, size, &[255, 0, 0, 255].repeat(64 * 64));
    let to = rgba_image_texture(&renderer, size, &[0, 0, 255, 255].repeat(64 * 64));
    let from_view = from.create_view(&wgpu::TextureViewDescriptor::default());
    let to_view = to.create_view(&wgpu::TextureViewDescriptor::default());
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    renderer.set_pass_timing_enabled(true);
    let labels = |renderer: &Renderer, source| {
        renderer
            .diagnostics()
            .passes
            .iter()
            .filter(|pass| pass.source == source)
            .map(|pass| pass.label)
            .collect::<Vec<_>>()
    };
    let frame = renderer
        .render_effect(
            &transition,
            &from_view,
            Some(&to_view),
            size,
            &EffectParams::new(),
            EffectRuntime {
                progress: 0.5,
                ..EffectRuntime::default()
            },
        )
        .expect("the transition renders");
    let timestamps = settle_timestamp_probe(&renderer);
    drop(frame);
    let frame = renderer
        .render_generator(
            &generator,
            size,
            &EffectParams::new(),
            EffectRuntime::default(),
        )
        .expect("the generator renders");
    settle_timestamp_probe(&renderer);
    drop(frame);
    assert_eq!(
        labels(&renderer, crate::TimingSource::CpuEncoding),
        ["transition", "generator"]
    );
    if timestamps {
        assert_eq!(
            labels(&renderer, crate::TimingSource::GpuTimestamp),
            ["transition", "generator"]
        );
    } else {
        eprintln!("no timestamp queries on this adapter: only CPU encoding is timed");
    }
    assert!(pollster::block_on(scope.pop()).is_none());
}

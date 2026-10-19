//! What a pool lets go of it destroys. Dropping a handle frees nothing on
//! the browser's WebGPU backend until garbage collection, so every idle
//! texture or buffer a pool clears, evicts, or declines to keep is destroyed
//! there and then, except that work still being encoded keeps what it uses
//! until it is submitted.
//!
//! A destroyed texture or buffer is a validation error to write to, which is
//! how these tests tell destroyed from merely dropped on any backend.

use super::{gpu, gpu_test_lock, rgba_texture, runtime_tint_effect, visual};
use crate::readback::read_rgba8;
use crate::{
    BlendMode, CacheIntent, EffectParams, EffectRuntime, GraphLayer, ParamValue, RenderCacheKey,
    RenderGraph, RenderSize, Renderer, SourceView,
};

/// The validation error a one-texel write to `texture` raises, if any.
fn write_error_texture(renderer: &Renderer, texture: &wgpu::Texture) -> Option<String> {
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let texel = texture.format().block_copy_size(None).unwrap_or(4);
    renderer.gpu.queue().write_texture(
        texture.as_image_copy(),
        &vec![0; usize::try_from(texel).expect("texel size")],
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(texel),
            rows_per_image: Some(1),
        },
        wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
    );
    renderer.gpu.submit([]);
    pollster::block_on(scope.pop()).map(|error| error.to_string())
}

/// The validation error a four-byte write to `buffer` raises, if any.
fn write_error_buffer(renderer: &Renderer, buffer: &wgpu::Buffer) -> Option<String> {
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    renderer.gpu.queue().write_buffer(buffer, 0, &[0; 4]);
    renderer.gpu.submit([]);
    pollster::block_on(scope.pop()).map(|error| error.to_string())
}

fn assert_destroyed(error: Option<String>, what: &str) {
    let error = error.unwrap_or_else(|| panic!("{what} was dropped but not destroyed"));
    assert!(
        error.to_lowercase().contains("destroyed"),
        "{what}: unexpected error {error}"
    );
}

fn rgba8_size() -> RenderSize {
    RenderSize::new(4, 2).expect("size")
}

#[test]
fn releasing_idle_memory_destroys_every_idle_texture_and_buffer() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = rgba8_size();
    let format = wgpu::TextureFormat::Rgba8Unorm;

    let idle_target = renderer
        .textures
        .acquire(renderer.gpu.device(), size, format, "idle target");
    let idle_target_handle = idle_target.texture().clone();
    drop(idle_target);
    // Another size, so it does not take the idle texture back out.
    let held_target = renderer.textures.acquire(
        renderer.gpu.device(),
        RenderSize::new(2, 2).expect("size"),
        format,
        "held target",
    );

    let idle_upload = renderer
        .acquire_rgba8_upload_texture(size)
        .expect("upload texture");
    let idle_upload_handle = idle_upload.texture().clone();
    drop(idle_upload);

    let idle_readback =
        renderer
            .readback_buffers
            .acquire(renderer.gpu.device(), 256, "idle readback");
    let idle_readback_handle = idle_readback.buffer().clone();
    idle_readback.recycle();

    let idle_uniforms = renderer.uniform_buffers.acquire(renderer.gpu.device(), 256);
    let idle_uniform_handle = idle_uniforms.buffer().clone();
    idle_uniforms.recycle();

    let diagnostics = renderer.diagnostics();
    let idle_bytes = diagnostics.textures.available_bytes
        + diagnostics.upload_textures.available_bytes
        + diagnostics.readback_buffers.available_bytes
        + diagnostics.uniform_buffers.available_bytes;
    assert!(idle_bytes > 0);

    assert_eq!(renderer.release_idle_memory(), idle_bytes);
    assert_destroyed(
        write_error_texture(&renderer, &idle_target_handle),
        "an idle intermediate texture",
    );
    assert_destroyed(
        write_error_texture(&renderer, &idle_upload_handle),
        "an idle upload texture",
    );
    assert_destroyed(
        write_error_buffer(&renderer, &idle_readback_handle),
        "an idle readback buffer",
    );
    assert_destroyed(
        write_error_buffer(&renderer, &idle_uniform_handle),
        "an idle uniform arena",
    );
    assert_eq!(
        write_error_texture(&renderer, held_target.texture()),
        None,
        "a held texture stays alive"
    );
}

#[test]
fn what_a_pool_evicts_or_declines_to_keep_is_destroyed() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = rgba8_size();
    let format = wgpu::TextureFormat::Rgba8Unorm;

    // Evicted by a smaller budget.
    let lease = renderer
        .textures
        .acquire(renderer.gpu.device(), size, format, "evicted target");
    let evicted = lease.texture().clone();
    drop(lease);
    renderer.set_texture_budget_bytes(0);
    assert_destroyed(
        write_error_texture(&renderer, &evicted),
        "a texture evicted by the budget",
    );

    // Larger than the budget when it returns.
    let lease = renderer
        .textures
        .acquire(renderer.gpu.device(), size, format, "declined target");
    let declined = lease.texture().clone();
    drop(lease);
    assert_eq!(renderer.texture_pool_stats().available, 0);
    assert_destroyed(
        write_error_texture(&renderer, &declined),
        "a texture over the budget",
    );

    // The oldest idle upload texture, pushed out by the pool's count limit.
    let mut oldest_upload = None;
    for width in 1..=65 {
        let upload = renderer
            .acquire_rgba8_upload_texture(RenderSize::new(width, 1).expect("size"))
            .expect("upload");
        oldest_upload.get_or_insert_with(|| upload.texture().clone());
        drop(upload);
    }
    assert_eq!(renderer.upload_textures.stats().available, 64);
    assert_destroyed(
        write_error_texture(&renderer, &oldest_upload.expect("first upload")),
        "the oldest idle upload texture",
    );

    // The oldest idle readback buffer, pushed out by a fourth.
    let leases = (0..4)
        .map(|_| {
            renderer
                .readback_buffers
                .acquire(renderer.gpu.device(), 256, "readback")
        })
        .collect::<Vec<_>>();
    let oldest = leases[0].buffer().clone();
    for lease in leases {
        lease.recycle();
    }
    assert_destroyed(
        write_error_buffer(&renderer, &oldest),
        "the oldest idle readback buffer",
    );

    // A readback abandoned before it finished.
    let abandoned =
        renderer
            .readback_buffers
            .acquire(renderer.gpu.device(), 256, "abandoned readback");
    let abandoned_handle = abandoned.buffer().clone();
    drop(abandoned);
    assert_destroyed(
        write_error_buffer(&renderer, &abandoned_handle),
        "an abandoned readback buffer",
    );
}

/// A source, then three tints in a chain, then a composite: each tint's input
/// returns to the pool while the frame is still being encoded.
fn tint_chain(renderer: &mut Renderer, budget: u64) -> Vec<u8> {
    let tint = pollster::block_on(renderer.register_effect(runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    )))
    .expect("tint compiles");
    renderer.set_texture_budget_bytes(budget);
    let texture = rgba_texture(renderer, [200, 160, 120, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let size = RenderSize::new(8, 4).expect("size");
    let mut graph = RenderGraph::new(size);
    let mut node = graph.add_source(SourceView::from_wgpu(&view));
    for red in [0.9, 0.8, 0.7] {
        node = graph
            .add_effect(
                tint.clone(),
                node,
                EffectParams::new().with("tint", ParamValue::Color([red, 1.0, 1.0, 1.0])),
                EffectRuntime::default(),
            )
            .expect("effect");
    }
    let output = graph
        .add_composite(vec![GraphLayer {
            source: node,
            visual: visual([1.0, 1.0], 1.0),
            blend_mode: BlendMode::Normal,
        }])
        .expect("composite");
    graph.set_output(output).expect("output");
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let frame = renderer.execute_graph(&graph).expect("renders");
    let pixels = read_rgba8(renderer, &frame);
    if let Some(error) = pollster::block_on(scope.pop()) {
        panic!("budget {budget}: {error}");
    }
    pixels
}

#[test]
fn a_texture_let_go_while_its_frame_is_encoded_lives_until_the_frame_is_submitted() {
    let _lock = gpu_test_lock();
    let mut pooled = Renderer::new(gpu());
    let expected = tint_chain(&mut pooled, u64::MAX);
    // With no budget every intermediate is let go once the next pass has read it, before submit.
    let mut unpooled = Renderer::new(gpu());
    assert_eq!(tint_chain(&mut unpooled, 0), expected);
    let stats = unpooled.texture_pool_stats();
    assert!(stats.evicted >= 2, "evicted {}", stats.evicted);
    assert_eq!(stats.available, 0);
}

#[test]
fn releasing_idle_memory_counts_only_what_it_destroys() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let tint = pollster::block_on(renderer.register_effect(runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    )))
    .expect("tint compiles");
    renderer.set_result_cache_budget(1 << 20);
    let texture = rgba_texture(&renderer, [200, 100, 50, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let size = RenderSize::new(4, 4).expect("size");
    let mut graph = RenderGraph::new(size);
    let source = graph.add_source_with_key(SourceView::from_wgpu(&view), RenderCacheKey::source(7));
    let effect = graph
        .add_effect(
            tint,
            source,
            EffectParams::new().with("tint", ParamValue::Color([0.5, 1.0, 1.0, 1.0])),
            EffectRuntime::default(),
        )
        .expect("effect");
    let output = graph
        .add_composite(vec![GraphLayer {
            source: effect,
            visual: visual([1.0, 1.0], 1.0),
            blend_mode: BlendMode::Normal,
        }])
        .expect("composite");
    graph.set_output(output).expect("output");
    graph
        .set_cache_intent(effect, CacheIntent::Auto)
        .expect("known node");
    graph
        .set_cache_intent(output, CacheIntent::Auto)
        .expect("known node");
    // The cache keeps the tint and the output; the caller still holds the output.
    let held = renderer.execute_graph(&graph).expect("renders");
    assert_eq!(renderer.result_cache_stats().entries, 2);
    let diagnostics = renderer.diagnostics();
    let idle_bytes = diagnostics.textures.available_bytes
        + diagnostics.upload_textures.available_bytes
        + diagnostics.readback_buffers.available_bytes
        + diagnostics.uniform_buffers.available_bytes;
    let tint_bytes = renderer.result_cache_stats().bytes / 2;

    // The tint leaves the cache for the pool and is destroyed; the output is the caller's.
    assert_eq!(renderer.release_idle_memory(), idle_bytes + tint_bytes);
    assert_eq!(write_error_texture(&renderer, &held.texture), None);
}

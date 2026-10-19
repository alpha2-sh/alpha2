//! A test device fails on the call that makes an uncaptured wgpu error: tests never install a
//! diagnostic handler, so without strict diagnostics an invalid resource would pass every test that
//! does not read its pixels back.
#![cfg(not(target_arch = "wasm32"))]

use std::sync::{Arc, Mutex, PoisonError};

use alpha2_gpu::{GpuContext, GpuDiagnostic};

fn gpu() -> GpuContext {
    pollster::block_on(GpuContext::request())
        .expect("a GPU adapter for the strict-diagnostics tests")
}

/// Renders into a texture made without `RENDER_ATTACHMENT`, the invalid usage a browser's WebGPU
/// also refuses.
fn render_into_a_sampled_only_texture(gpu: &GpuContext) {
    let texture = gpu.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("sampled only"),
        size: wgpu::Extent3d {
            width: 4,
            height: 4,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = gpu
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("invalid attachment"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: &view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations::default(),
        })],
        ..wgpu::RenderPassDescriptor::default()
    }));
    gpu.submit([encoder.finish()]);
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "uncaptured GPU validation error")]
fn an_invalid_texture_usage_fails_the_test_that_made_it() {
    render_into_a_sampled_only_texture(&gpu());
}

#[test]
fn an_error_scope_still_captures_the_error() {
    let gpu = gpu();
    let scope = gpu.device().push_error_scope(wgpu::ErrorFilter::Validation);
    render_into_a_sampled_only_texture(&gpu);
    assert!(matches!(
        pollster::block_on(scope.pop()),
        Some(wgpu::Error::Validation { .. })
    ));
}

#[test]
fn an_installed_handler_receives_the_error_instead() {
    let gpu = gpu();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    gpu.set_diagnostic_handler(move |diagnostic: GpuDiagnostic| {
        recorded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(diagnostic.failure_code());
    });
    render_into_a_sampled_only_texture(&gpu);
    gpu.device()
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("the device drains");
    // The invalid pass, then the submission of its invalid command buffer.
    let seen = seen.lock().unwrap_or_else(PoisonError::into_inner);
    assert!(!seen.is_empty());
    assert!(
        seen.iter().all(|code| *code == "VALIDATION_FAILED"),
        "{seen:?}"
    );
}

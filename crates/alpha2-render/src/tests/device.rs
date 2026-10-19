//! The shared test device, and the size and colour checks a render makes before any pass.

use super::*;

/// Creates and destroys many independent devices, which no alpha2 host does.
///
/// The suite shares one device so that it does not do this, and this test
/// keeps the teardown path reachable: run it when a driver is suspected.
/// Three SIGSEGVs were once seen here on NVIDIA Vulkan 580.105.08 and never
/// reproduced, so a failure is evidence about the driver, not a regression in
/// this crate.
///
/// `cargo test -p alpha2-render -- --ignored device_creation_survives_repeated_teardown`
#[test]
#[ignore = "stresses the driver's device teardown path; not a product code path"]
fn device_creation_survives_repeated_teardown() {
    let _lock = gpu_test_lock();
    for _ in 0..64 {
        let gpu = pollster::block_on(GpuContext::request_high_performance())
            .expect("render tests require a working GPU adapter");
        let renderer = Renderer::new(gpu);
        let size = RenderSize::new(2, 2).expect("non-zero size");
        let texture = rgba_image_texture(&renderer, size, &[255, 0, 0, 255].repeat(4));
        let target = renderer
            .render_rgba_presented(
                &texture,
                size,
                SourcePresentation::identity(size),
                crate::SourceAlpha::Straight,
            )
            .expect("source presentation submits");
        assert_eq!(read_rgba8(&renderer, &target).len(), 16);
        drop(renderer);
    }
}

#[test]
fn render_size_and_color_validate_inputs() {
    assert_eq!(RenderSize::new(0, 1), Err(RenderError::ZeroDimension));
    assert!(Color::rgba(0.0, 0.5, 1.0, 1.0).is_valid());
    assert!(!Color::rgba(f64::NAN, 0.0, 0.0, 1.0).is_valid());
}

#[test]
fn a_color_is_premultiplied_so_no_channel_exceeds_its_alpha() {
    // Straight red at half alpha: not a premultiplied value, which would be 0.5, 0, 0.
    assert!(!Color::rgba(1.0, 0.5, 0.5, 0.5).is_valid());
    assert!(!Color::rgba(0.0, 0.0, 0.1, 0.0).is_valid());
    assert!(Color::rgba(0.5, 0.25, 0.0, 0.5).is_valid());
    assert!(Color::rgba(0.0, 0.0, 0.0, 0.0).is_valid());
    // The product computed in f32 may land a rounding step above alpha.
    let alpha = f64::from(0.7_f32);
    assert!(Color::rgba(alpha + 1e-7, alpha, 0.0, alpha).is_valid());
    assert!(!Color::rgba(alpha + 1e-4, alpha, 0.0, alpha).is_valid());

    let size = RenderSize::new(4, 4).expect("size");
    let mut graph = crate::RenderGraph::new(size);
    assert_eq!(
        graph.add_composite_over(Vec::new(), Color::rgba(1.0, 0.5, 0.5, 0.5)),
        Err(crate::GraphError::Render(RenderError::InvalidColor))
    );
    assert!(
        graph
            .add_composite_over(Vec::new(), Color::rgba(0.5, 0.25, 0.25, 0.5))
            .is_ok()
    );
}

#[test]
fn a_solid_render_refuses_a_color_that_is_not_premultiplied() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(1, 1).expect("size");
    assert_eq!(
        renderer
            .render_solid(size, Color::rgba(1.0, 0.5, 0.5, 0.5))
            .err(),
        Some(RenderError::InvalidColor)
    );
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let solid = renderer
        .render_solid(size, Color::rgba(0.6, 0.2, 0.0, 0.6))
        .expect("a premultiplied color renders");
    assert_eq!(read_rgba8(&renderer, &solid), [153, 51, 0, 153]);
    assert!(pollster::block_on(scope.pop()).is_none());
}

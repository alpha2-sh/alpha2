//! What a frame submits: the adapter it runs on, YUV readback in the same submission, pending source work, and source synchronization.

use super::*;

#[test]
fn the_suite_runs_on_the_adapter_a_native_host_exports_on() {
    // On a machine with an integrated and a discrete GPU, the first adapter listed is not the one
    // the host renders on.
    let host = pollster::block_on(GpuContext::request_high_performance())
        .expect("a native host's GPU adapter");
    let (suite, host) = (gpu(), host.capabilities().clone());
    assert_eq!(
        (
            &suite.capabilities().adapter_name,
            &suite.capabilities().backend
        ),
        (&host.adapter_name, &host.backend)
    );
}

#[derive(Debug, Default)]
struct ExternalLeaseCalls {
    prepared: AtomicUsize,
    flushed: AtomicUsize,
    submitted: AtomicUsize,
}

#[derive(Debug)]
struct TestExternalLease {
    submitted: Arc<AtomicBool>,
    calls: Arc<ExternalLeaseCalls>,
    flush_failure: Option<RenderError>,
}

impl TestExternalLease {
    fn new(submitted: &Arc<AtomicBool>) -> Self {
        Self {
            submitted: Arc::clone(submitted),
            calls: Arc::new(ExternalLeaseCalls::default()),
            flush_failure: None,
        }
    }
}

impl ExternalTextureLease for TestExternalLease {
    fn retained_bytes(&self) -> u64 {
        6
    }

    fn prepare_submission(&self) -> bool {
        self.calls.prepared.fetch_add(1, Ordering::AcqRel);
        true
    }

    fn flush_submission(&self) -> Result<(), RenderError> {
        self.calls.flushed.fetch_add(1, Ordering::AcqRel);
        self.flush_failure.map_or(Ok(()), Err)
    }

    fn submitted(&self) {
        self.calls.submitted.fetch_add(1, Ordering::AcqRel);
        self.submitted.store(true, Ordering::Release);
    }
}

/// Builds separate R8 luma and RG8 chroma textures for one `width`x`height`
/// NV12 image, mirroring the plane views a hardware import produces.
///
/// The renderer does not expose its device, so this uploads on the shared
/// [`gpu`] device that every renderer in this suite is created from.
fn nv12_test_planes(
    _renderer: &Renderer,
    width: u32,
    height: u32,
    luma: &[u8],
    chroma: &[u8],
) -> (wgpu::Texture, wgpu::Texture) {
    let gpu = gpu();
    let make_texture = |label, format, width: u32, height: u32, bytes: &[u8], row: u32| {
        let texture = gpu.device().create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        gpu.queue().write_texture(
            texture.as_image_copy(),
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(height),
            },
            texture.size(),
        );
        texture
    };
    let y = make_texture(
        "test NV12 Y",
        wgpu::TextureFormat::R8Unorm,
        width,
        height,
        luma,
        width,
    );
    let uv = make_texture(
        "test NV12 UV",
        wgpu::TextureFormat::Rg8Unorm,
        width / 2,
        height / 2,
        chroma,
        width,
    );
    (y, uv)
}

struct PendingFrame {
    source: SourceId,
    _texture: RenderedTexture,
    view: wgpu::TextureView,
    pending: PendingSourceWork,
}

impl FrameProvider for PendingFrame {
    fn texture_for(&self, source: &SourceId, _time: MediaTime) -> Option<SourceView<'_>> {
        (source == &self.source).then_some(SourceView::from_wgpu(&self.view))
    }

    fn pending_source_work(
        &self,
        source: &SourceId,
        _time: MediaTime,
    ) -> Option<&PendingSourceWork> {
        (source == &self.source).then_some(&self.pending)
    }
}

#[test]
fn evaluated_frame_and_yuv_readback_share_one_submission() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let source = SourceId::new("source").expect("valid ID");
    let frame = FrameDesc {
        background: alpha2_model::Rgb::BLACK,
        blend_space: alpha2_model::BlendSpace::LinearLight,
        time: MediaTime::ZERO,
        visual: vec![ActiveClip {
            clip_id: leak(ClipId::new("clip").expect("valid ID")),
            source_id: leak(source.clone()),
            track_id: leak(TrackId::new("track").expect("valid ID")),
            source_time: MediaTime::ZERO,
            visual: Some(visual(Vec2::new(1.0, 1.0), 1.0)),
            composition: None,
            blend_mode: ClipBlendMode::Normal,
            cache: alpha2_model::CacheIntent::Auto,
            masks: &[],
            matte: None,
            is_matte: false,
            generator: None,
            plan: None,
            source_composition: None,
        }],
        audio: Vec::new(),
        transitions: Vec::new(),
        captions: Vec::new(),
    };
    let size = RenderSize::new(2, 2).expect("even size");
    let input = rgba_image_texture(&renderer, size, &[255, 0, 0, 255].repeat(4));
    let view = input.create_view(&wgpu::TextureViewDescriptor::default());
    let provider = SingleFrame {
        source,
        texture: &view,
    };

    let texture = renderer
        .render_frame(size, &frame, &provider)
        .expect("standalone frame renders")
        .texture
        .expect("complete frame");
    let expected = renderer
        .begin_yuv420p_readback(&texture, centred(ColorMetadata::default()))
        .expect("standalone readback submits")
        .wait()
        .expect("standalone readback maps");
    let before = renderer.diagnostics();

    let combined = renderer
        .render_frame_yuv420p(size, &frame, &provider, centred(ColorMetadata::default()))
        .expect("combined frame submits");
    assert!(combined.report.is_complete());
    let actual = combined
        .readback
        .expect("complete frame has readback")
        .wait()
        .expect("combined readback maps");
    assert_eq!(actual, expected);

    let after = renderer.diagnostics();
    assert_eq!(after.frame_submissions - before.frame_submissions, 1);
    assert_eq!(after.graph_submissions - before.graph_submissions, 0);
    assert_eq!(after.readback_submissions - before.readback_submissions, 0);
    assert_eq!(after.graph_nodes - before.graph_nodes, 1);
    assert_eq!(after.copy_operations - before.copy_operations, 3);

    let missing_before = renderer.diagnostics();
    let missing = renderer
        .render_frame_yuv420p(
            size,
            &frame,
            &MissingFrames,
            centred(ColorMetadata::default()),
        )
        .expect("missing source is not a render error");
    assert!(!missing.is_complete());
    assert_eq!(
        renderer.diagnostics().frame_submissions,
        missing_before.frame_submissions
    );
}

#[test]
fn pending_source_presentation_joins_the_export_submission() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let source = SourceId::new("source").expect("valid ID");
    let size = RenderSize::new(2, 2).expect("even size");
    let frame = FrameDesc {
        background: alpha2_model::Rgb::BLACK,
        blend_space: alpha2_model::BlendSpace::LinearLight,
        time: MediaTime::ZERO,
        visual: vec![ActiveClip {
            clip_id: leak(ClipId::new("clip").expect("valid ID")),
            source_id: leak(source.clone()),
            track_id: leak(TrackId::new("track").expect("valid ID")),
            source_time: MediaTime::ZERO,
            visual: Some(visual(Vec2::new(1.0, 1.0), 1.0)),
            composition: None,
            blend_mode: ClipBlendMode::Normal,
            cache: alpha2_model::CacheIntent::Auto,
            masks: &[],
            matte: None,
            is_matte: false,
            generator: None,
            plan: None,
            source_composition: None,
        }],
        audio: Vec::new(),
        transitions: Vec::new(),
        captions: Vec::new(),
    };
    let (prepared, diagnostics) = renderer
        .prepare_yuv420p_presented(
            Yuv420Frame {
                y: &[16; 4],
                u: &[128],
                v: &[128],
                y_stride: 2,
                chroma_stride: 1,
                size,
                alpha: 1.0,
                color: Some(ColorMetadata::default()),
            },
            SourcePresentation::identity(size),
            ColorPolicy::RejectHdr,
        )
        .expect("YUV source prepares");
    assert_eq!(diagnostics, [] as [alpha2_render::ColorDiagnostic; 0]);
    let (texture, pending) = prepared.into_parts();
    let provider = PendingFrame {
        source,
        view: texture
            .texture()
            .create_view(&wgpu::TextureViewDescriptor::default()),
        _texture: texture,
        pending: pending.expect("SDR conversion is pending"),
    };
    assert!(provider.pending.retained_bytes() > 0);

    let before = renderer.diagnostics();
    let planes = renderer
        .render_frame_yuv420p(size, &frame, &provider, centred(ColorMetadata::default()))
        .expect("source, graph, and readback encode")
        .readback
        .expect("complete frame has readback")
        .wait()
        .expect("combined readback maps");
    assert!(planes.y.iter().all(|value| value.abs_diff(16) <= 1));
    assert!(planes.u.iter().all(|value| value.abs_diff(128) <= 1));
    assert!(planes.v.iter().all(|value| value.abs_diff(128) <= 1));

    let after = renderer.diagnostics();
    assert_eq!(after.frame_submissions - before.frame_submissions, 1);
    assert_eq!(after.source_submissions - before.source_submissions, 0);
    assert_eq!(after.graph_submissions - before.graph_submissions, 0);
    assert_eq!(after.readback_submissions - before.readback_submissions, 0);
    // Presentation, the layer composited straight over the background (no clear pass), and the
    // two pack passes.
    assert_eq!(after.render_passes - before.render_passes, 4);
    assert_eq!(provider.pending.retained_bytes(), 0);
    assert!(!renderer.submit_pending_source(&provider.pending));
}

#[test]
fn export_submission_stages_external_source_synchronization() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let source = SourceId::new("source").expect("valid ID");
    let size = RenderSize::new(2, 2).expect("even size");
    let (y, uv) = nv12_test_planes(&renderer, 2, 2, &[16; 4], &[128, 128]);
    let y_view = y.create_view(&wgpu::TextureViewDescriptor::default());
    let uv_view = uv.create_view(&wgpu::TextureViewDescriptor::default());
    let submitted = Arc::new(AtomicBool::new(false));
    let lease = Arc::new(TestExternalLease::new(&submitted));
    let calls = Arc::clone(&lease.calls);
    let (prepared, diagnostics) = renderer
        .prepare_gpu_yuv_presented(
            GpuVideoPlanes {
                y: &y_view,
                chroma: &uv_view,
                v: None,
                format: alpha2_media::YuvFormat::NV12,
                size,
                alpha: 1.0,
                color: Some(ColorMetadata::default()),
            },
            SourcePresentation::identity(size),
            ColorPolicy::RejectHdr,
            lease,
        )
        .expect("NV12 source prepares");
    assert_eq!(diagnostics, [] as [alpha2_render::ColorDiagnostic; 0]);
    let (texture, pending) = prepared.into_parts();
    let provider = PendingFrame {
        source: source.clone(),
        view: texture
            .texture()
            .create_view(&wgpu::TextureViewDescriptor::default()),
        _texture: texture,
        pending: pending.expect("conversion remains pending"),
    };
    let frame = FrameDesc {
        background: alpha2_model::Rgb::BLACK,
        blend_space: alpha2_model::BlendSpace::LinearLight,
        time: MediaTime::ZERO,
        visual: vec![ActiveClip {
            clip_id: leak(ClipId::new("clip").expect("valid ID")),
            source_id: leak(source.clone()),
            track_id: leak(TrackId::new("track").expect("valid ID")),
            source_time: MediaTime::ZERO,
            visual: Some(visual(Vec2::new(1.0, 1.0), 1.0)),
            composition: None,
            blend_mode: ClipBlendMode::Normal,
            cache: alpha2_model::CacheIntent::Auto,
            masks: &[],
            matte: None,
            is_matte: false,
            generator: None,
            plan: None,
            source_composition: None,
        }],
        audio: Vec::new(),
        transitions: Vec::new(),
        captions: Vec::new(),
    };

    // The export path must stage native waits and layout transitions exactly like the preview
    // path; a decode surface sampled without them is only accidentally correct.
    drop(
        renderer
            .render_frame_yuv420p(size, &frame, &provider, centred(ColorMetadata::default()))
            .expect("source, graph, and readback encode")
            .readback
            .expect("complete frame has readback")
            .wait()
            .expect("combined readback maps"),
    );
    assert_eq!(calls.prepared.load(Ordering::Acquire), 1);
    assert_eq!(calls.flushed.load(Ordering::Acquire), 1);
    assert_eq!(calls.submitted.load(Ordering::Acquire), 1);
    assert!(submitted.load(Ordering::Acquire));
}

#[test]
fn a_failed_source_synchronization_fails_the_frame_with_what_the_device_said() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(2, 2).expect("even size");
    let (y, uv) = nv12_test_planes(&renderer, 2, 2, &[16; 4], &[128, 128]);
    let y_view = y.create_view(&wgpu::TextureViewDescriptor::default());
    let uv_view = uv.create_view(&wgpu::TextureViewDescriptor::default());
    for fault in [
        alpha2_media::GpuFault::DeviceLost,
        alpha2_media::GpuFault::DeviceMemory,
        alpha2_media::GpuFault::Timeout,
    ] {
        let source = SourceId::new("source").expect("valid ID");
        let submitted = Arc::new(AtomicBool::new(false));
        let mut lease = TestExternalLease::new(&submitted);
        lease.flush_failure = Some(RenderError::ExternalTextureSync(Some(fault)));
        let (prepared, _) = renderer
            .prepare_gpu_yuv_presented(
                GpuVideoPlanes {
                    y: &y_view,
                    chroma: &uv_view,
                    v: None,
                    format: alpha2_media::YuvFormat::NV12,
                    size,
                    alpha: 1.0,
                    color: Some(ColorMetadata::default()),
                },
                SourcePresentation::identity(size),
                ColorPolicy::RejectHdr,
                Arc::new(lease),
            )
            .expect("NV12 source prepares");
        let (texture, pending) = prepared.into_parts();
        let provider = PendingFrame {
            source: source.clone(),
            view: texture
                .texture()
                .create_view(&wgpu::TextureViewDescriptor::default()),
            _texture: texture,
            pending: pending.expect("conversion remains pending"),
        };
        let frame = FrameDesc {
            background: alpha2_model::Rgb::BLACK,
            blend_space: alpha2_model::BlendSpace::LinearLight,
            time: MediaTime::ZERO,
            visual: vec![ActiveClip {
                clip_id: leak(ClipId::new("clip").expect("valid ID")),
                source_id: leak(source),
                track_id: leak(TrackId::new("track").expect("valid ID")),
                source_time: MediaTime::ZERO,
                visual: Some(visual(Vec2::new(1.0, 1.0), 1.0)),
                composition: None,
                blend_mode: ClipBlendMode::Normal,
                cache: alpha2_model::CacheIntent::Auto,
                masks: &[],
                matte: None,
                is_matte: false,
                generator: None,
                plan: None,
                source_composition: None,
            }],
            audio: Vec::new(),
            transitions: Vec::new(),
            captions: Vec::new(),
        };
        let error = renderer
            .render_frame_yuv420p(size, &frame, &provider, centred(ColorMetadata::default()))
            .expect_err("the frame fails at its source's synchronization");
        assert_eq!(error, RenderError::ExternalTextureSync(Some(fault)));
        assert!(!submitted.load(Ordering::Acquire), "nothing was submitted");
        let engine = crate::EngineError::Render(error);
        assert_eq!(crate::gpu_fault_of(&engine), Some(fault));
        let expected = match fault {
            alpha2_media::GpuFault::DeviceLost => crate::ErrorCode::GpuDeviceLost,
            alpha2_media::GpuFault::Timeout => crate::ErrorCode::GpuTimeout,
            _ => crate::ErrorCode::Alpha2Error,
        };
        assert_eq!(crate::ErrorCode::of(&engine), Some(expected), "{fault:?}");
    }
}

#[test]
fn failed_frame_encoding_restores_pending_source_work() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let source = SourceId::new("source").expect("valid ID");
    let size = RenderSize::new(3, 3).expect("odd size");
    let frame = FrameDesc {
        background: alpha2_model::Rgb::BLACK,
        blend_space: alpha2_model::BlendSpace::LinearLight,
        time: MediaTime::ZERO,
        visual: vec![ActiveClip {
            clip_id: leak(ClipId::new("clip").expect("valid ID")),
            source_id: leak(source.clone()),
            track_id: leak(TrackId::new("track").expect("valid ID")),
            source_time: MediaTime::ZERO,
            visual: Some(visual(Vec2::new(1.0, 1.0), 1.0)),
            composition: None,
            blend_mode: ClipBlendMode::Normal,
            cache: alpha2_model::CacheIntent::Auto,
            masks: &[],
            matte: None,
            is_matte: false,
            generator: None,
            plan: None,
            source_composition: None,
        }],
        audio: Vec::new(),
        transitions: Vec::new(),
        captions: Vec::new(),
    };
    let (prepared, _) = renderer
        .prepare_yuv420p_presented(
            Yuv420Frame {
                y: &[16; 9],
                u: &[128; 4],
                v: &[128; 4],
                y_stride: 3,
                chroma_stride: 2,
                size,
                alpha: 1.0,
                color: Some(ColorMetadata::default()),
            },
            SourcePresentation::identity(size),
            ColorPolicy::RejectHdr,
        )
        .expect("YUV source prepares");
    let (texture, pending) = prepared.into_parts();
    let provider = PendingFrame {
        source,
        view: texture
            .texture()
            .create_view(&wgpu::TextureViewDescriptor::default()),
        _texture: texture,
        pending: pending.expect("conversion is pending"),
    };

    assert!(matches!(
        renderer.render_frame_yuv420p(size, &frame, &provider, centred(ColorMetadata::default())),
        Err(RenderError::TextureReadback(_))
    ));
    assert!(renderer.submit_pending_source(&provider.pending));
    assert_eq!(renderer.diagnostics().source_submissions, 1);
}

#[test]
fn pending_source_presentation_joins_the_preview_graph_submission() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let source = SourceId::new("source").expect("valid ID");
    let size = RenderSize::new(2, 2).expect("size");
    let frame = FrameDesc {
        background: alpha2_model::Rgb::BLACK,
        blend_space: alpha2_model::BlendSpace::LinearLight,
        time: MediaTime::ZERO,
        visual: vec![ActiveClip {
            clip_id: leak(ClipId::new("clip").expect("valid ID")),
            source_id: leak(source.clone()),
            track_id: leak(TrackId::new("track").expect("valid ID")),
            source_time: MediaTime::ZERO,
            visual: Some(visual(Vec2::new(1.0, 1.0), 1.0)),
            composition: None,
            blend_mode: ClipBlendMode::Normal,
            cache: alpha2_model::CacheIntent::Auto,
            masks: &[],
            matte: None,
            is_matte: false,
            generator: None,
            plan: None,
            source_composition: None,
        }],
        audio: Vec::new(),
        transitions: Vec::new(),
        captions: Vec::new(),
    };
    let (prepared, _) = renderer
        .prepare_yuv420p_presented(
            Yuv420Frame {
                y: &[16; 4],
                u: &[128],
                v: &[128],
                y_stride: 2,
                chroma_stride: 1,
                size,
                alpha: 1.0,
                color: Some(ColorMetadata::default()),
            },
            SourcePresentation::identity(size),
            ColorPolicy::RejectHdr,
        )
        .expect("YUV source prepares");
    let (texture, pending) = prepared.into_parts();
    let provider = PendingFrame {
        source,
        view: texture
            .texture()
            .create_view(&wgpu::TextureViewDescriptor::default()),
        _texture: texture,
        pending: pending.expect("conversion is pending"),
    };
    let before = renderer.diagnostics();

    let output = renderer
        .render_frame(size, &frame, &provider)
        .expect("preview frame renders")
        .texture
        .expect("complete frame");
    assert!(
        read_rgba8(&renderer, &output)
            .as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| pixel[0] <= 1 && pixel[1] <= 1 && pixel[2] <= 1 && pixel[3] == 255)
    );
    let after = renderer.diagnostics();
    assert_eq!(after.frame_submissions - before.frame_submissions, 1);
    assert_eq!(after.source_submissions - before.source_submissions, 0);
    assert_eq!(after.graph_submissions - before.graph_submissions, 0);
}

//! Renders on several threads sharing intermediates and source conversions.

use super::*;

/// A source lease that stops its frame between encoding and submission: it
/// says so on `encoded`, then waits for `release` (at most five seconds).
#[derive(Debug)]
struct PausingLease {
    encoded: Mutex<std::sync::mpsc::Sender<()>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl ExternalTextureLease for PausingLease {
    fn retained_bytes(&self) -> u64 {
        0
    }

    fn prepare_submission(&self) -> bool {
        let _ = self
            .encoded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .send(());
        let _ = self
            .release
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv_timeout(std::time::Duration::from_secs(5));
        true
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_render_on_another_thread_never_gets_an_intermediate_a_frame_still_to_be_submitted_writes() {
    // Intermediates go back to the pool once their last reader is encoded, before submission;
    // without the frame lock another thread could take one, and this frame's submission would
    // then overwrite that thread's kept result.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(4, 4).expect("size");
    let white = rgba_texture(&renderer, [255; 4]);
    let white_view = white.create_view(&wgpu::TextureViewDescriptor::default());
    // A frame with a pending source whose lease pauses it after encoding, at another size
    // so the source's texture is not the one at stake.
    let (y, uv) = nv12_test_planes(&renderer, 2, 2, &[16; 4], &[128, 128]);
    let y_view = y.create_view(&wgpu::TextureViewDescriptor::default());
    let uv_view = uv.create_view(&wgpu::TextureViewDescriptor::default());
    let (encoded_sender, encoded) = std::sync::mpsc::channel();
    let (release, release_receiver) = std::sync::mpsc::channel();
    let (prepared, _) = renderer
        .prepare_gpu_yuv_presented(
            GpuVideoPlanes {
                y: &y_view,
                chroma: &uv_view,
                v: None,
                format: alpha2_media::YuvFormat::NV12,
                size: RenderSize::new(2, 2).expect("size"),
                alpha: 1.0,
                color: Some(ColorMetadata::default()),
            },
            SourcePresentation::identity(RenderSize::new(2, 2).expect("size")),
            ColorPolicy::RejectHdr,
            Arc::new(PausingLease {
                encoded: Mutex::new(encoded_sender),
                release: Mutex::new(release_receiver),
            }),
        )
        .expect("NV12 source prepares");
    let (_source, pending) = prepared.into_parts();
    let pending = pending.expect("conversion remains pending");
    let mut graph = RenderGraph::new(size);
    let first = GraphLayer {
        source: graph.add_source(SourceView::from_wgpu(&white_view)),
        visual: visual([1.0, 1.0], 1.0),
        blend_mode: BlendMode::Normal,
    };
    let first = graph.add_composite(vec![first]).expect("composite");
    let second = GraphLayer {
        source: first,
        visual: visual([1.0, 1.0], 1.0),
        blend_mode: BlendMode::Normal,
    };
    let output = graph.add_composite(vec![second]).expect("composite");
    graph.set_output(output).expect("output");
    let blue = Color::rgba(0.0, 0.0, 1.0, 1.0);
    let (shared, graph, pending) = (&renderer, &graph, &pending);
    let other = std::thread::scope(|scope| {
        let frame = scope.spawn(move || shared.render_graph(graph, &[pending]));
        encoded
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the frame encodes");
        // Encoded, not submitted: another thread renders a picture of the same size.
        let (done_sender, done) = std::sync::mpsc::channel();
        let other = scope.spawn(move || {
            let solid = shared.render_solid(size, blue).expect("solid");
            let _ = done_sender.send(());
            solid
        });
        // Give it time to finish if nothing holds it back, then let the frame submit.
        let _ = done.recv_timeout(std::time::Duration::from_millis(500));
        release.send(()).expect("the frame waits");
        frame
            .join()
            .expect("frame thread")
            .expect("the frame renders");
        other.join().expect("other thread")
    });
    assert_eq!(
        read_rgba8(&renderer, &other),
        [0, 0, 255, 255].repeat(16),
        "the other thread's picture is what it drew"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_render_on_another_thread_waits_for_a_source_conversion_another_frame_is_encoding() {
    // A frame holds a pending source's conversion until submission, so another thread cannot
    // read it as submitted and draw the source texture before anything was written to it.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(2, 2).expect("size");
    // Studio white NV12: converted, it is opaque white; unconverted, the texture is still
    // empty.
    let (y, uv) = nv12_test_planes(&renderer, 2, 2, &[235; 4], &[128, 128]);
    let y_view = y.create_view(&wgpu::TextureViewDescriptor::default());
    let uv_view = uv.create_view(&wgpu::TextureViewDescriptor::default());
    let (encoded_sender, encoded) = std::sync::mpsc::channel();
    let (release, release_receiver) = std::sync::mpsc::channel();
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
            Arc::new(PausingLease {
                encoded: Mutex::new(encoded_sender),
                release: Mutex::new(release_receiver),
            }),
        )
        .expect("NV12 source prepares");
    let (source, pending) = prepared.into_parts();
    let pending = pending.expect("conversion remains pending");
    let source_view = source
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());
    let picture = || {
        let mut graph = RenderGraph::new(size);
        let layer = GraphLayer {
            source: graph.add_source(SourceView::from_wgpu(&source_view)),
            visual: visual([1.0, 1.0], 1.0),
            blend_mode: BlendMode::Normal,
        };
        let output = graph.add_composite(vec![layer]).expect("composite");
        graph.set_output(output).expect("output");
        graph
    };
    let (first, second) = (picture(), picture());
    let (shared, pending) = (&renderer, &pending);
    let (first, second) = std::thread::scope(|scope| {
        let frame = scope.spawn(|| shared.render_graph(&first, &[pending]));
        encoded
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the first frame encodes");
        let (done_sender, done) = std::sync::mpsc::channel();
        let other = scope.spawn(move || {
            let drawn = shared.render_graph(&second, &[pending]);
            let _ = done_sender.send(());
            drawn
        });
        let _ = done.recv_timeout(std::time::Duration::from_millis(500));
        release.send(()).expect("the first frame waits");
        (
            frame
                .join()
                .expect("first thread")
                .expect("the first frame renders"),
            other
                .join()
                .expect("second thread")
                .expect("the second frame renders"),
        )
    });
    assert_eq!(read_rgba8(&renderer, &first), [255; 16], "first");
    assert_eq!(
        read_rgba8(&renderer, &second),
        [255; 16],
        "the second frame draws the converted source"
    );
}

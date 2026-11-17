//! What one CPU frame allocates on the calling thread, which draws every band with one thread.

// The counting allocator is the only unsafe code, and it only forwards.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use alpha2_media::{
    BitDepth, Chroma, ChromaLocation, ColorMetadata, PixelLayout, PlaneLayout, VideoCodec,
    YuvEncoding, YuvFormat,
};
use alpha2_render::{
    BlendMode, Color, ColorPolicy, CpuImage, CpuRenderer, EffectId, EffectParams, EffectRuntime,
    GAUSSIAN_BLUR_ID, GraphLayer, LayerTransform, ParamValue, PlaneTarget, RenderGraph,
    RenderNodeId, RenderSize, SourcePlane, SourcePresentation, SourceView, Yuv420pTargets,
    YuvFrame, YuvPlanes,
};

struct Counting;

thread_local! {
    static MEASURING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: forwards every call to the system allocator; the const-initialized thread-local
// counters never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: the caller's contract is the system allocator's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: as above.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(new_size);
        // SAFETY: as above.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as above.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn record(size: usize) {
    // Allocations also happen while thread locals are torn down.
    let _ = MEASURING.try_with(|measuring| {
        if measuring.get() {
            ALLOCATIONS.with(|count| count.set(count.get() + 1));
            BYTES.with(|bytes| bytes.set(bytes.get() + size as u64));
        }
    });
}

fn counted(work: impl FnOnce()) -> (u64, u64) {
    ALLOCATIONS.with(|count| count.set(0));
    BYTES.with(|bytes| bytes.set(0));
    MEASURING.with(|measuring| measuring.set(true));
    work();
    MEASURING.with(|measuring| measuring.set(false));
    (ALLOCATIONS.with(Cell::get), BYTES.with(Cell::get))
}

#[derive(Debug)]
struct Planes {
    format: YuvFormat,
    data: [Vec<u8>; 3],
    alpha: Option<Vec<u8>>,
    size: RenderSize,
}

impl YuvPlanes for Planes {
    fn frame(&self) -> YuvFrame<'_> {
        let (width, chroma) = (self.size.width, self.size.width / 2);
        YuvFrame {
            format: self.format,
            planes: [
                SourcePlane {
                    bytes: &self.data[0],
                    stride: width,
                },
                SourcePlane {
                    bytes: &self.data[1],
                    stride: chroma,
                },
                SourcePlane {
                    bytes: &self.data[2],
                    stride: chroma,
                },
            ],
            alpha_plane: self.alpha.as_ref().map(|bytes| SourcePlane {
                bytes,
                stride: width,
            }),
            size: self.size,
            alpha: 1.0,
            color: Some(ColorMetadata::EXPORT),
        }
    }
}

/// Ramps; with `alpha`, a plane opaque in the middle fifth of the rows and clear elsewhere.
fn planes(size: RenderSize, alpha: bool) -> Arc<Planes> {
    let (width, height) = (size.width as usize, size.height as usize);
    #[allow(clippy::cast_possible_truncation)]
    let ramp = |count: usize, step: usize| (0..count).map(|index| (index * step) as u8).collect();
    let format = if alpha {
        YuvFormat::I420.with_alpha()
    } else {
        YuvFormat::I420
    };
    Arc::new(Planes {
        format,
        data: [
            ramp(width * height, 3),
            ramp(width * height / 4, 5),
            ramp(width * height / 4, 7),
        ],
        alpha: alpha.then(|| {
            (0..width * height)
                .map(|index| {
                    let row = index / width;
                    if row * 5 / height == 2 { 255 } else { 0 }
                })
                .collect()
        }),
        size,
    })
}

fn whole(source: RenderNodeId) -> GraphLayer {
    GraphLayer {
        source,
        visual: LayerTransform {
            position: [0.5, 0.5],
            anchor: [0.5, 0.5],
            scale: [1.0, 1.0],
            rotation_degrees: 0.0,
            opacity: 1.0,
        },
        blend_mode: BlendMode::Normal,
    }
}

/// Allocations and bytes of one frame of `case`, after a warm-up frame.
fn frame(case: &str) -> (u64, u64) {
    let size = RenderSize::new(640, 360).expect("size");
    let renderer = CpuRenderer::new(1);
    let clip = planes(size, false);
    let overlay = planes(size, true);
    let (width, height) = (size.width as usize, size.height as usize);
    let mut y = vec![0_u8; width * height];
    let mut u = vec![0_u8; width * height / 4];
    let mut v = vec![0_u8; width * height / 4];
    let mut draw = || {
        let present = |planes: &Arc<Planes>| {
            renderer
                .present_yuv(
                    Arc::clone(planes) as Arc<dyn YuvPlanes>,
                    SourcePresentation::identity(size),
                    ColorPolicy::RejectHdr,
                )
                .expect("present")
                .0
        };
        let sources = [present(&clip), present(&overlay)];
        let mut graph = RenderGraph::new(size);
        let clip = graph.add_source(SourceView::cpu(&sources[0]));
        let output = match case {
            "clip" => graph
                .add_composite_over(vec![whole(clip)], Color::rgba(0.0, 0.0, 0.0, 1.0))
                .expect("composite"),
            "blur" => graph
                .add_effect(
                    EffectId::from_static(GAUSSIAN_BLUR_ID, 1),
                    clip,
                    EffectParams::new().with("radius", ParamValue::Float(0.04)),
                    EffectRuntime::default(),
                )
                .expect("blur"),
            "yuva" => {
                let overlay = graph.add_source(SourceView::cpu(&sources[1]));
                graph
                    .add_composite_over(
                        vec![whole(clip), whole(overlay)],
                        Color::rgba(0.0, 0.0, 0.0, 1.0),
                    )
                    .expect("composite")
            }
            _ => unreachable!("known cases"),
        };
        graph.set_output(output).expect("output");
        renderer
            .render_graph_into_yuv420p(
                &graph,
                YuvEncoding::new(ColorMetadata::EXPORT, ChromaLocation::Left),
                &mut Yuv420pTargets {
                    y: PlaneTarget::new(&mut y, width),
                    u: PlaneTarget::new(&mut u, width / 2),
                    v: PlaneTarget::new(&mut v, width / 2),
                },
            )
            .expect("render");
    };
    draw();
    counted(draw)
}

/// Allocations of a YUVA frame for bands, scratch and node lists; none are per pixel.
const YUVA_FRAME: u64 = 400;

#[test]
fn cpu_frames_report_what_they_allocate() {
    for case in ["clip", "blur", "yuva"] {
        let (allocations, bytes) = frame(case);
        println!("cpu frame {case} 640x360: allocations={allocations} bytes={bytes}");
        if case == "yuva" {
            assert!(allocations <= YUVA_FRAME, "{allocations} allocations");
        }
    }
}

/// Per band of a 10-bit 4:2:2 pack: its plane rows, two decoded lines and their bookkeeping.
const PACK_PER_BAND: u64 = 16;

#[test]
fn a_cpu_pack_allocates_per_band_not_per_sample() {
    let (width, height) = (1_920_usize, 1_080_usize);
    let size = RenderSize::new(1_920, 1_080).expect("size");
    #[allow(clippy::cast_possible_truncation)]
    let rgba = (0..width * height * 4)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<u8>>();
    let picture = CpuImage::from_premultiplied_rgba8(size, &rgba).expect("picture");
    let renderer = CpuRenderer::new(1);
    let layout = PixelLayout {
        chroma: Chroma::Yuv422,
        bit_depth: BitDepth::Ten,
        alpha: false,
        planes: PlaneLayout::Planar,
    };
    let mut planes = [
        vec![0_u8; width * height * 2],
        vec![0_u8; width * height],
        vec![0_u8; width * height],
    ];
    let mut pack = || {
        let [y, u, v] = &mut planes;
        renderer
            .pack_image(
                &picture,
                YuvEncoding::export(VideoCodec::ProRes),
                layout,
                &mut [
                    PlaneTarget::new(y, width * 2),
                    PlaneTarget::new(u, width),
                    PlaneTarget::new(v, width),
                ],
            )
            .expect("pack");
    };
    pack();
    let (allocations, bytes) = counted(pack);
    // Bands of about 16,384 pixels: eight rows of 1920.
    let bands = (height / (16_384 / width)) as u64;
    println!("cpu pack 1920x1080 10-bit 4:2:2: allocations={allocations} bytes={bytes}");
    assert!(
        allocations <= PACK_PER_BAND * bands,
        "{allocations} allocations for {bands} bands"
    );
}

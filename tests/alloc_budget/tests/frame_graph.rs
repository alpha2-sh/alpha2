//! What building one frame's render graph allocates.
//!
//! A frame of a generated source and a chain of four effects compiles into a
//! graph whose effect identities and parameter names the engine and the
//! renderer already hold: the built-in passes' names are static, a project's
//! effect is named by the identity it is registered under, and its parameter
//! names are borrowed from the project. What the graph still allocates is its
//! node lists, one parameter list per pass, and the frame's own bookkeeping.
//! A frame that draws shapes and shape masks also builds a vector scene for
//! each, within a budget of its own. Submitting the graph is the renderer's
//! and the driver's, and is not counted.

// The counting allocator is the only unsafe code, and it only forwards.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fmt::Write as _;

use alpha2_engine::{FrameGraph, FrameProvider, FrameRenderer};
use alpha2_eval::{ActiveIndex, evaluate};
use alpha2_gpu::GpuContext;
use alpha2_media::SourceId;
use alpha2_model::Project;
use alpha2_render::{RenderSize, Renderer, SourceView};
use alpha2_time::MediaTime;

/// Counts allocations made on a thread while it is measuring.
struct Counting;

thread_local! {
    static MEASURING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: forwards every call to the system allocator unchanged; the thread-local counters are
// const-initialized, so touching them never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record();
        // SAFETY: the caller's contract is the system allocator's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record();
        // SAFETY: as above.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record();
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

fn record() {
    // `try_with` because allocations also happen while a thread's locals are torn down.
    let _ = MEASURING.try_with(|measuring| {
        if measuring.get() {
            ALLOCATIONS.with(|count| count.set(count.get() + 1));
        }
    });
}

/// The allocations `work` makes on this thread.
fn allocations<T>(work: impl FnOnce() -> T) -> (T, u64) {
    ALLOCATIONS.with(|count| count.set(0));
    MEASURING.with(|measuring| measuring.set(true));
    let result = work();
    MEASURING.with(|measuring| measuring.set(false));
    (result, ALLOCATIONS.with(Cell::get))
}

/// Supplies no source frames: every clip draws a generated source.
struct NoSources;

impl FrameProvider for NoSources {
    fn texture_for(&self, _source: &SourceId, _time: MediaTime) -> Option<SourceView<'_>> {
        None
    }
}

/// Three tracks over one generated gradient and one generated shape: the
/// gradient under eight shape masks, the stroked shape alone, and the
/// gradient through a composition of four effects, the last a blur the
/// graph draws as several passes. Without `vectors`, only the last: no
/// vector scene is drawn.
fn project(vectors: bool) -> Project {
    let constant = |value: &str| format!(r#"{{"mode":"constant","value":{value}}}"#);
    let mut masks = String::new();
    for index in 0..8_u8 {
        let offset = f32::from(index) / 16.0;
        let (kind, radius) = if index % 2 == 0 {
            ("ellipse", String::new())
        } else {
            (
                "rectangle",
                format!(r#","corner_radius":{}"#, constant("0.02")),
            )
        };
        let mode = ["add", "subtract", "intersect"][usize::from(index % 3)];
        if index > 0 {
            masks.push(',');
        }
        write!(
            masks,
            r#"{{"shape":{{"type":"{kind}","center":{},"size":{},"rotation_degrees":{}{radius}}},"feather":{},"mode":"{mode}"}}"#,
            constant(&format!(r#"{{"x":{},"y":0.5}}"#, 0.25 + offset)),
            constant(r#"{"x":0.4,"y":0.3}"#),
            constant(&format!("{}", f32::from(index) * 5.0)),
            constant("0.01"),
        )
        .expect("write");
    }
    let float = |value: f32| constant(&format!(r#"{{"type":"float","value":{value}}}"#));
    let effect = |id: &str, node: &str, input: &str, params: &str| {
        format!(
            r#"{{"id":"{node}","op":{{"type":"effect","input":"{input}","effect":{{"id":"alpha2.effect.{id}","version":1,"params":{{{params}}}}}}}}}"#
        )
    };
    let look = [
        effect(
            "color-controls",
            "e0",
            "clip",
            &format!(r#""exposure":{},"saturation":{}"#, float(0.2), float(1.3)),
        ),
        effect(
            "vignette",
            "e1",
            "e0",
            &format!(r#""amount":{}"#, float(0.5)),
        ),
        effect(
            "sharpen",
            "e2",
            "e1",
            &format!(r#""amount":{}"#, float(0.8)),
        ),
        effect(
            "gaussian-blur",
            "e3",
            "e2",
            &format!(r#""radius":{}"#, float(0.05)),
        ),
    ]
    .join(",");
    let stops = (0..8_u8)
        .map(|stop| {
            format!(
                r#"{{"offset":{},"color":{{"red":{},"green":0.4,"blue":0.8,"alpha":1}}}}"#,
                f32::from(stop) / 7.0,
                f32::from(stop) / 8.0
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let range =
        r#""timeline_range":{"start":0,"end":120000},"source_range":{"start":0,"end":120000}"#;
    Project::from_json(&format!(
        r#"{{"schema_version":1,"id":"frame-graph","name":"Frame graph","settings":{{"width":64,"height":36,"frame_rate":{{"num":24,"den":1}},"sample_rate":48000}},
"sources":[
 {{"id":"wash","kind":"generated","generator":{{"type":"fill","paint":{{"type":"linear_gradient","start":{{"x":0,"y":0}},"end":{{"x":1,"y":1}},"stops":[{stops}]}}}}}},
 {{"id":"badge","kind":"generated","generator":{{"type":"shapes","items":[{{"type":"rectangle","center":{center},"size":{size},"corner_radius":{radius}}},{{"type":"stroke","width":{width},"paint":{{"mode":"constant","value":{{"type":"solid","color":{{"red":1,"green":1,"blue":1,"alpha":1}}}}}}}},{{"type":"fill","paint":{{"mode":"constant","value":{{"type":"radial_gradient","center":{{"x":0.5,"y":0.5}},"radius":0.5,"stops":[{stops}]}}}}}}]}}}}
],
"compositions":[{{"id":"look","nodes":[{{"id":"clip","op":{{"type":"clip"}}}},{look}],"output":"e3"}}],
"timeline":{{"tracks":[{vector_tracks}
 {{"id":"looked","name":"Looked","kind":"video","clips":[{{"id":"looked","source_id":"wash",{range},"composition":{{"id":"look"}}}}]}}
]}}}}"#,
        center = constant(r#"{"x":0.5,"y":0.5}"#),
        size = constant(r#"{"x":0.4,"y":0.4}"#),
        radius = constant("0.05"),
        width = constant("0.01"),
        vector_tracks = if vectors {
            format!(
                r#"
 {{"id":"masked","name":"Masked","kind":"video","clips":[{{"id":"masked","source_id":"wash",{range},"masks":[{masks}]}}]}},
 {{"id":"shape","name":"Shape","kind":"video","clips":[{{"id":"shape","source_id":"badge",{range}}}]}},"#
            )
        } else {
            String::new()
        },
    ))
    .expect("valid project")
}

/// Allocations building the graph of one frame of a generated picture and
/// four effects may make: the node, key, stability, and intent lists the
/// graph grows, one parameter list per pass, and the frame's clip,
/// transition, layer, and source-work lists. Drawing vector scenes adds
/// nothing to a frame that has none.
const FRAME_GRAPH: u64 = 27;

/// What a frame that also draws a shapes generator and eight shape masks
/// may allocate: each scene's operations, paths, and paints (a mask's
/// outline is one scene, the shapes another), the named inputs of the pass
/// combining the masks, and the parameter lists of the feather blurs.
const VECTOR_FRAME_GRAPH: u64 = 240;

fn measure(vectors: bool) -> Vec<u64> {
    let gpu = pollster::block_on(GpuContext::request_high_performance())
        .expect("these budgets need a GPU adapter");
    let mut renderer = Renderer::new(gpu);
    pollster::block_on(renderer.register_builtins()).expect("built-ins");
    let project = project(vectors);
    let index = ActiveIndex::build(&project).expect("valid project");
    let frame = evaluate(&project, &index, MediaTime::from_ticks(60_000)).expect("evaluation");
    let size = RenderSize::new(64, 36).expect("size");
    // The graph draws: every name it holds reaches a pass.
    let render = renderer
        .render_frame(size, &frame, &NoSources)
        .expect("renders");
    assert!(render.is_complete());
    let mut counts = Vec::new();
    for _ in 0..5 {
        let (graph, count) = allocations(|| renderer.build_frame_graph(size, &frame, &NoSources));
        let FrameGraph { graph, .. } = graph.expect("builds");
        assert!(graph.is_some(), "every picture is generated");
        counts.push(count);
    }
    counts
}

#[test]
fn building_a_frames_graph_allocates_its_lists_not_its_names() {
    let counts = measure(false);
    println!("build_frame_graph generated=1 effects=4: allocations per frame {counts:?}");
    assert!(
        counts.iter().all(|&count| count <= FRAME_GRAPH),
        "building a frame's graph allocated {counts:?}, budget {FRAME_GRAPH}"
    );
}

#[test]
fn building_a_frame_of_shapes_and_masks_allocates_within_its_budget() {
    let counts = measure(true);
    println!(
        "build_frame_graph masks=8 shapes=1 generated=3 effects=4: allocations per frame {counts:?}"
    );
    assert!(
        counts.iter().all(|&count| count <= VECTOR_FRAME_GRAPH),
        "building a frame's graph allocated {counts:?}, budget {VECTOR_FRAME_GRAPH}"
    );
}

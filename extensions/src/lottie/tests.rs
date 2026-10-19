use super::*;
use crate::api::NoResources;

fn corpus(name: &str) -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tests/corpus/extensions")
            .join(name),
    )
    .expect("corpus file")
}

fn moving() -> Value {
    serde_json::from_slice(&corpus("moving.json")).expect("moving.json")
}

/// Frame `index` of an animation at `fit`, composited bounded or whole:
/// its pixels, the pixels composited, and the layers allocated.
fn rasterize(
    document: &Value,
    fit: (u32, u32),
    index: usize,
    whole: bool,
) -> (Vec<u8>, u64, usize) {
    let mut source = LottieSource::open(
        &serde_json::to_vec(document).expect("json"),
        &ProviderSource::Lottie { animation: None },
        &NoResources,
    )
    .expect("open");
    let (width, height) = fitted(source.report.width, source.report.height, fit);
    let mut sink = SkiaSink::new(width, height, &source.images).expect("sink");
    sink.whole = whole;
    #[allow(clippy::cast_precision_loss)]
    let frame = source.composition.frames.start + index as f64;
    let transform = Affine::scale_non_uniform(
        f64::from(width) / f64::from(source.report.width),
        f64::from(height) / f64::from(source.report.height),
    );
    source
        .renderer
        .append(&source.composition, frame, transform, 1.0, &mut sink);
    let (composited, allocated) = (sink.composited, sink.allocated);
    (sink.finish(), composited, allocated)
}

/// The moving box, through a repeater of `copies` copies that fade to
/// half, each a little to the right.
fn fading(copies: u32) -> Value {
    let mut json = moving();
    let items = json["layers"][0]["shapes"][0]["it"]
        .as_array_mut()
        .expect("items");
    let transform = items.pop().expect("transform");
    items.push(serde_json::json!({
        "ty": "rp", "c": {"a": 0, "k": copies}, "o": {"a": 0, "k": 0}, "m": 1,
        "tr": {"ty": "tr", "p": {"a": 0, "k": [0.1, 0]}, "a": {"a": 0, "k": [0, 0]},
               "s": {"a": 0, "k": [100, 100]}, "r": {"a": 0, "k": 0},
               "so": {"a": 0, "k": 100}, "eo": {"a": 0, "k": 50}}
    }));
    items.push(transform);
    json
}

/// Masks, mattes, a stroke with miter joins, opacity, and an image.
fn layered() -> Vec<Value> {
    let mut json = moving();
    let mut layer = json["layers"][0].clone();
    layer["masksProperties"] = serde_json::json!([{
        "mode": "a", "inv": false, "o": {"a": 0, "k": 100}, "x": {"a": 0, "k": 0},
        "pt": {"a": 0, "k": {"c": true, "v": [[-5, -7], [9, -3], [6, 8], [-8, 5]],
               "i": [[0, 0], [2, 1], [0, 0], [0, 0]], "o": [[0, 0], [0, 0], [1, 3], [0, 0]]}}
    }]);
    layer["ks"]["o"] = serde_json::json!({"a": 0, "k": 63});
    layer["shapes"][0]["it"]
        .as_array_mut()
        .expect("items")
        .insert(
            1,
            serde_json::json!({
                "ty": "st", "c": {"a": 0, "k": [0, 1, 0, 1]}, "o": {"a": 0, "k": 100},
                "w": {"a": 0, "k": 3}, "lc": 2, "lj": 1, "ml": 10
            }),
        );
    let mut matte = layer.clone();
    matte["td"] = serde_json::json!(1);
    matte["ind"] = serde_json::json!(7);
    matte["ks"]["r"] = serde_json::json!({"a": 0, "k": 33});
    let mut matted = json["layers"][0].clone();
    matted["tt"] = serde_json::json!(2);
    matted["ks"]["s"] = serde_json::json!({"a": 0, "k": [170, 130, 100]});
    json["layers"] = serde_json::json!([layer, matte, matted]);
    let mut cases: Vec<Value> =
        serde_json::from_slice(&corpus("normalize-cases.json")).expect("cases");
    cases.retain(|case| case["outcome"] == "opens");
    let mut documents = cases
        .into_iter()
        .map(|case| case["animation"].clone())
        .collect::<Vec<_>>();
    documents.extend([
        moving(),
        serde_json::from_slice(&corpus("unsupported.json")).expect("unsupported.json"),
        json,
        fading(12),
    ]);
    documents
}

#[test]
fn bounded_compositing_draws_what_whole_compositing_draws() {
    for (number, document) in layered().iter().enumerate() {
        for fit in [(0, 0), (256, 256), (37, 29), (333, 251)] {
            for index in [0, 9] {
                let (bounded, ..) = rasterize(document, fit, index, false);
                let (whole, ..) = rasterize(document, fit, index, true);
                assert!(
                    bounded == whole,
                    "document {number} at {fit:?}, frame {index}"
                );
            }
        }
    }
}

#[test]
fn a_layer_costs_what_it_covers_not_the_frame() {
    let canvas = 640_u64 * 480;
    // The clip to the animation's own canvas, every frame, is no layer.
    let (_, composited, allocated) = rasterize(&moving(), (640, 480), 0, false);
    assert_eq!((composited, allocated), (0, 0));
    let (_, composited, allocated) = rasterize(&moving(), (640, 480), 0, true);
    assert_eq!((composited, allocated), (canvas, 1));
    // 60 copies of a 160-pixel box fading to half: the 59 less than
    // opaque are layers, each composites its box, and they share one
    // allocation.
    let (_, composited, allocated) = rasterize(&fading(60), (640, 480), 0, false);
    assert!(composited < 59 * 170 * 170, "{composited}");
    assert_eq!(allocated, 1);
    let (_, composited, allocated) = rasterize(&fading(60), (640, 480), 0, true);
    assert_eq!((composited, allocated), (60 * canvas, 60));
}

/// A clip that covers the canvas, rasterized, is the mask the sink
/// shares for one: 255 everywhere.
#[test]
#[allow(clippy::cast_precision_loss)]
fn a_covering_clip_rasterizes_to_full_coverage() {
    for (width, height, rect, transform) in [
        (64, 48, (0.0, 0.0, 64.0, 48.0), Transform::identity()),
        (
            37,
            29,
            (0.0, 0.0, 64.0, 48.0),
            Transform::from_scale(37.0 / 64.0, 29.0 / 48.0),
        ),
        (
            1920,
            1080,
            (0.0, 0.0, 512.0, 288.0),
            Transform::from_scale(3.75, 3.75),
        ),
        (
            333,
            251,
            (-10.0, -3.5, 400.0, 300.0),
            Transform::from_row(1.0, 0.0, 0.0, 1.0, 0.25, -0.75),
        ),
    ] {
        let path = PathBuilder::from_rect(
            tiny_skia::Rect::from_ltrb(rect.0, rect.1, rect.2, rect.3).expect("rect"),
        );
        let bounds = path.clone().transform(transform).expect("path").bounds();
        assert!(bounds.left() <= 0.0 && bounds.top() <= 0.0);
        assert!(bounds.right() >= width as f32 && bounds.bottom() >= height as f32);
        let mut mask = Mask::new(width, height).expect("mask");
        mask.fill_path(&path, FillRule::Winding, true, transform);
        assert!(
            mask.data().iter().all(|coverage| *coverage == 255),
            "{width}x{height}"
        );
    }
}

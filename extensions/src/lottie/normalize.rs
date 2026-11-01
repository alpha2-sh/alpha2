//! Rewrites an animation's JSON so velato can neither panic nor run away on it.
//!
//! velato 0.12 panics on some spec-valid input, rejects whole documents for one unknown layer
//! type or modifier, and draws precompositions, mattes, repeaters, and polystars as deep and as
//! often as the file says. Panics and unbounded recursion cannot be caught downstream, so they
//! are handled here before velato reads anything.
//!
//! - Skippable features are rewritten and reported: unknown layer types become null layers,
//!   zig-zag and no-style items are removed, a missing layer rotation gets the spec default, and
//!   a multi-value scalar easing handle keeps its first value.
//! - Unreadable input is refused with its location, as are documents past [`MAX_DRAWN`],
//!   [`MAX_OFFSCREEN`], [`MAX_DRAWING`], [`MAX_NESTING`], [`MAX_REPEATER_COPIES`], or
//!   [`MAX_POLYSTAR_POINTS`]. Counts follow what velato draws, including every precomposition
//!   instance, matte, and repeater copy.
//!
//! velato's serde also accepts a shape `ty` given as velato's own variant number. Shapes are
//! identified here only by name, so numbered types are refused rather than renamed.

use std::collections::{BTreeMap, HashMap};

use serde_json::{Map, Value};

use crate::api::{Diagnostic, ExtensionError};
use crate::budget::MAX_DRAWING;

/// Most copies a repeater makes at any frame.
pub(super) const MAX_REPEATER_COPIES: f64 = 1_000.0;
/// Most points a polystar has at any frame.
pub(super) const MAX_POLYSTAR_POINTS: f64 = 1_000.0;
/// Most layers, masks, shapes, and styles one frame draws, all repeats counted.
pub(super) const MAX_DRAWN: u64 = 100_000;
/// Most offscreen layers one frame composites (masks, two per track matte, fading repeater
/// copies). A full-frame one costs about 9 ms at 1080p.
pub(super) const MAX_OFFSCREEN: u64 = 512;
/// Most nested offscreen layers at once (precomposition levels, masks, repeaters, two per track
/// matte). Each is a frame-sized layer and a level of recursion.
pub(super) const MAX_NESTING: u64 = 64;

/// Rewrites the animation in place for velato and returns what it leaves out. In place because a
/// parsed document is many times its file size.
pub(super) fn normalize(mut json: Value) -> Result<(Vec<Diagnostic>, Value), ExtensionError> {
    let number = |key: &str| json.get(key).and_then(Value::as_f64).unwrap_or(0.0);
    let canvas = number("w") * number("h");
    let images = image_sizes(&json);
    let mut scan = Scan::default();
    scan.value(&mut json, "");
    let sets = Structure {
        scan: &mut scan,
        canvas,
        images,
    }
    .animation(&mut json);
    if let Some(error) = scan.refused {
        return Err(error);
    }
    check_drawing(&sets)?;
    let diagnostics = scan
        .found
        .into_iter()
        .map(|(feature, (path, count))| Diagnostic::UnsupportedFeature {
            feature: feature.to_owned(),
            path,
            count,
        })
        .collect();
    Ok((diagnostics, json))
}

#[derive(Default)]
struct Scan {
    /// Unsupported features with their first location and count.
    found: BTreeMap<&'static str, (String, usize)>,
    refused: Option<ExtensionError>,
}

impl Scan {
    fn found(&mut self, feature: &'static str, path: &str) {
        self.found
            .entry(feature)
            .and_modify(|(_, count)| *count += 1)
            .or_insert_with(|| (path.to_owned(), 1));
    }

    fn refuse(&mut self, feature: &str, path: &str) {
        self.fail(ExtensionError::Unsupported(format!(
            "the Lottie backend cannot read {feature} (at {path})"
        )));
    }

    fn fail(&mut self, error: ExtensionError) {
        self.refused.get_or_insert(error);
    }

    fn value(&mut self, value: &mut Value, path: &str) {
        match value {
            Value::Array(items) => {
                for (index, item) in items.iter_mut().enumerate() {
                    self.value(item, &format!("{path}/{index}"));
                }
            }
            Value::Object(object) => {
                self.object(object, path);
                for (key, child) in object.iter_mut() {
                    self.value(child, &format!("{path}/{key}"));
                }
            }
            _ => {}
        }
    }

    fn object(&mut self, object: &mut Map<String, Value>, path: &str) {
        let kind = object.get("ty").and_then(Value::as_str).map(str::to_owned);
        let is_layer =
            object.contains_key("ty") && object.contains_key("ip") && object.contains_key("op");
        if is_layer {
            if object.contains_key("tm") {
                self.found("time remapping", path);
            }
            if object
                .get("ef")
                .and_then(Value::as_array)
                .is_some_and(|effects| !effects.is_empty())
            {
                self.found("layer effects", path);
            }
            if object.get("mb").and_then(Value::as_bool) == Some(true) {
                self.found("motion blur", path);
            }
            if object.get("ddd").and_then(Value::as_i64) == Some(1) {
                self.found("3D layers", path);
            }
            // Add and Hard Mix stop the backend: they are drawn as Normal, and reported.
            if let Some(mode @ (16 | 17)) = object.get("bm").and_then(Value::as_i64) {
                self.found(
                    if mode == 16 {
                        "the Add blend mode"
                    } else {
                        "the Hard Mix blend mode"
                    },
                    path,
                );
                object.insert("bm".into(), Value::from(0));
            }
            // velato reads a layer's blend mode and never applies it.
            if object
                .get("bm")
                .and_then(Value::as_i64)
                .is_some_and(|mode| (1..=15).contains(&mode))
            {
                self.found("layer blend modes", path);
            }
            // velato mattes by alpha whatever the mode says.
            if let Some(3 | 4) = object.get("tt").and_then(Value::as_i64) {
                self.found("luma mattes", path);
            }
            if let Some(masks) = object.get("masksProperties").and_then(Value::as_array) {
                for (index, mask) in masks.iter().enumerate() {
                    if mask
                        .get("mode")
                        .and_then(Value::as_str)
                        .is_some_and(|mode| mode != "a")
                    {
                        self.found(
                            "mask modes other than add",
                            &format!("{path}/masksProperties/{index}"),
                        );
                    }
                    if mask.get("inv").and_then(Value::as_bool) == Some(true) {
                        self.found("inverted masks", &format!("{path}/masksProperties/{index}"));
                    }
                }
            }
        }
        match kind.as_deref() {
            Some("mm") => self.found("merge paths", path),
            Some("op") => self.found("offset paths", path),
            Some("pb") => self.found("pucker and bloat", path),
            Some("rd") => self.found("rounded corners", path),
            Some("tw") => self.found("twist", path),
            // velato fills nonzero whatever the rule says.
            Some("fl" | "gf") if object.get("r").and_then(Value::as_i64) == Some(2) => {
                self.found("even-odd fills", path);
            }
            Some("st" | "gs")
                if object
                    .get("d")
                    .is_some_and(|dashes| dashes.as_array().is_some_and(|d| !d.is_empty())) =>
            {
                self.found("stroke dashes", path);
            }
            _ => {}
        }
        if object.get("x").is_some_and(Value::is_string) && object.contains_key("k") {
            self.found("expressions", path);
        }
        if object.contains_key("ti") && object.contains_key("to") && object.contains_key("t") {
            let nonzero = |key: &str| {
                object
                    .get(key)
                    .and_then(Value::as_array)
                    .is_some_and(|values| {
                        values.iter().any(|v| v.as_f64().is_some_and(|v| v != 0.0))
                    })
            };
            if nonzero("ti") || nonzero("to") {
                self.found("curved position paths", path);
            }
        }
    }
}

/// The layers of the root composition or of one precomposition asset.
struct LayerSet {
    /// JSON pointer.
    path: String,
    layers: Vec<LayerInfo>,
    /// Layers velato draws, in order (not track mattes).
    drawn: Vec<usize>,
}

/// Per-frame drawing cost.
#[derive(Clone, Copy, Default)]
struct Cost {
    /// Layers, masks, shapes, and styles drawn.
    drawn: u64,
    offscreen: u64,
    /// Most offscreen layers (recursion levels) held at once.
    depth: u64,
    paint: Paint,
}

impl Cost {
    fn then(self, other: Self) -> Self {
        Self {
            drawn: self.drawn.saturating_add(other.drawn),
            offscreen: self.offscreen.saturating_add(other.offscreen),
            depth: self.depth.max(other.depth),
            paint: self.paint.then(other.paint),
        }
    }
}

/// Fill and stroke coverage, in root-composition canvases.
///
/// Each draw covers at most a canvas and at most its bounds, which are only known once all
/// enclosing transforms are. Draws are summed and capped at one canvas each. Draws already
/// covering a canvas are kept apart so one large shape does not inflate the small ones. Bounds
/// are carried through every transform, so the count never undercounts.
#[derive(Clone, Copy, Default)]
struct Paint {
    /// Draws whose bounds covered at least a canvas where drawn.
    large: Draws,
    /// The others.
    small: Draws,
}

/// A draw count and their summed bounds, in canvases.
#[derive(Clone, Copy, Default)]
struct Draws {
    count: f64,
    area: f64,
}

impl Draws {
    fn then(self, other: Self) -> Self {
        Self {
            count: self.count + other.count,
            area: self.area + other.area,
        }
    }

    fn grown(self, factor: f64) -> Self {
        Self {
            area: grown(self.area, factor),
            ..self
        }
    }

    fn times(self, copies: f64) -> Self {
        Self {
            count: self.count * copies,
            area: grown(self.area, copies),
        }
    }

    /// Each draw covers a canvas at most.
    fn canvases(self) -> f64 {
        self.count.min(self.area)
    }
}

impl Paint {
    fn draw(area: f64) -> Self {
        let draw = Draws {
            count: 1.0,
            area: if area.is_nan() { f64::INFINITY } else { area },
        };
        if draw.area < 1.0 {
            Self {
                small: draw,
                ..Self::default()
            }
        } else {
            Self {
                large: draw,
                ..Self::default()
            }
        }
    }

    fn then(self, other: Self) -> Self {
        Self {
            large: self.large.then(other.large),
            small: self.small.then(other.small),
        }
    }

    /// Through transforms that grow areas at most `factor` times.
    fn grown(self, factor: f64) -> Self {
        Self {
            large: self.large.grown(factor),
            small: self.small.grown(factor),
        }
    }

    fn times(self, copies: f64) -> Self {
        Self {
            large: self.large.times(copies),
            small: self.small.times(copies),
        }
    }

    fn canvases(self) -> f64 {
        self.large.canvases() + self.small.canvases()
    }
}

/// An upper bound on `value * factor`, treating infinity times zero as unbounded.
fn grown(value: f64, factor: f64) -> f64 {
    let product = value * factor;
    if product.is_nan() {
        f64::INFINITY
    } else {
        product
    }
}

/// Summed bounding boxes of the shapes a fill or stroke would paint.
#[derive(Clone, Copy, Default)]
struct Outline {
    area: f64,
    /// Summed width plus height.
    sides: f64,
    count: f64,
}

impl Outline {
    fn of(width: f64, height: f64) -> Self {
        Self {
            area: grown(width, height),
            sides: width + height,
            count: 1.0,
        }
    }

    /// Unknown bounds, never smaller than the canvas.
    fn unknown() -> Self {
        Self::of(f64::INFINITY, f64::INFINITY)
    }

    fn then(self, other: Self) -> Self {
        Self {
            area: self.area + other.area,
            sides: self.sides + other.sides,
            count: self.count + other.count,
        }
    }

    fn scaled(self, scale: Scale) -> Self {
        Self {
            area: grown(self.area, scale.area),
            sides: grown(self.sides, scale.stretch),
            count: self.count,
        }
    }

    fn repeated(self, scale: Scale, copies: f64) -> Self {
        let each = self.scaled(scale);
        Self {
            area: grown(each.area, copies),
            sides: grown(each.sides, copies),
            count: each.count * copies,
        }
    }

    /// Upper bound on a stroke's area, growing each box by `reach` on every side.
    fn stroked(self, reach: f64) -> f64 {
        self.area + grown(2.0 * reach, self.sides) + grown(4.0 * reach * reach, self.count)
    }
}

/// The most a transform grows what it draws at any frame.
#[derive(Clone, Copy)]
struct Scale {
    area: f64,
    /// Length factor.
    stretch: f64,
}

impl Scale {
    const NONE: Self = Self {
        area: 1.0,
        stretch: 1.0,
    };

    /// From a layer or group transform. Scale is in percent; skew stretches lengths by up to one
    /// plus its tangent but keeps area; rotation changes neither.
    fn of(transform: &Map<String, Value>) -> Self {
        let percent = |component| {
            transform
                .get("s")
                .map_or(100.0, |scale| magnitude(scale, component))
        };
        let (x, y) = (percent(0) / 100.0, percent(1) / 100.0);
        let skew = transform.get("sk").map_or(0.0, |skew| magnitude(skew, 0));
        let shear = if skew < 90.0 {
            1.0 + skew.to_radians().tan()
        } else {
            f64::INFINITY
        };
        Self {
            area: grown(x, y),
            stretch: grown(x.max(y), shear),
        }
    }
}

#[derive(Default)]
struct LayerInfo {
    path: String,
    /// The layer, masks, shapes, and styles; depth is the most repeaters any shape is copied by.
    own: Cost,
    /// Each an offscreen layer held while the content draws.
    masks: u64,
    /// A track matte (`td`), drawn only for the layer it mattes.
    is_matte: bool,
    /// The matte layer, resolved from `tt` and `tp` as velato does.
    matte: Option<usize>,
    /// The precomposition asset (`refId`).
    instance: Option<String>,
    scale: Scale,
    /// Area growth of its transform with its parents'.
    growth: f64,
}

impl Default for Scale {
    fn default() -> Self {
        Self::NONE
    }
}

/// Walks the parts of the document velato reads.
struct Structure<'a> {
    scan: &'a mut Scan,
    /// Root composition area, the unit paint is counted in.
    canvas: f64,
    images: HashMap<String, (f64, f64)>,
}

impl Structure<'_> {
    /// Layer sets: the root composition first, then each precomposition asset.
    fn animation(&mut self, root: &mut Value) -> Vec<(Option<String>, LayerSet)> {
        let mut sets = Vec::new();
        if let Some(layers) = root.get_mut("layers") {
            sets.push((None, self.layers(layers, "/layers")));
        }
        if let Some(assets) = root.get_mut("assets").and_then(Value::as_array_mut) {
            for (index, asset) in assets.iter_mut().enumerate() {
                // velato reads an asset with layers as a precomposition.
                let id = asset.get("id").and_then(Value::as_str).map(str::to_owned);
                if let Some(layers) = asset.get_mut("layers") {
                    let path = format!("/assets/{index}/layers");
                    let set = self.layers(layers, &path);
                    sets.push((Some(id.unwrap_or_default()), set));
                }
            }
        }
        sets
    }

    fn layers(&mut self, layers: &mut Value, path: &str) -> LayerSet {
        let mut set = LayerSet {
            path: path.to_owned(),
            layers: Vec::new(),
            drawn: Vec::new(),
        };
        let Some(layers) = layers.as_array_mut() else {
            return set;
        };
        // velato resolves `tp` by layer index (`ind`, default 0, last wins), and an implicit
        // matte as the latest track matte above not yet taken.
        let mut indices = HashMap::new();
        let mut explicit = Vec::with_capacity(layers.len());
        let mut parents = Vec::with_capacity(layers.len());
        for (position, layer) in layers.iter_mut().enumerate() {
            let path = format!("{path}/{position}");
            let info = self.layer(layer, &path);
            let index = layer.get("ind").and_then(Value::as_u64).unwrap_or(0);
            indices.insert(index, position);
            parents.push(layer.get("parent").and_then(Value::as_u64));
            // velato casts `tp` to usize, which saturates at u32::MAX in wasm; clamping there
            // finds every layer either build reads.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let matte_index = layer
                .get("tp")
                .and_then(Value::as_f64)
                .map(|index| index.min(f64::from(u32::MAX)) as u64);
            let matted = layer.get("tt").is_some_and(|mode| !mode.is_null());
            explicit.push((matted, matte_index));
            set.layers.push(info);
        }
        let mut latest_matte = None;
        for (position, (matted, matte_index)) in explicit.into_iter().enumerate() {
            if matted {
                set.layers[position].matte = match matte_index {
                    Some(index) => indices.get(&index).copied(),
                    None => latest_matte,
                };
            }
            if set.layers[position].is_matte {
                latest_matte = Some(position);
            } else if matted {
                latest_matte = None;
            }
        }
        set.drawn.extend(
            set.layers
                .iter()
                .enumerate()
                .filter(|(_, layer)| !layer.is_matte)
                .map(|(position, _)| position),
        );
        // velato resolves a parent (`parent`) by layer index, as a matte.
        let parents = parents
            .into_iter()
            .map(|parent| parent.and_then(|index| indices.get(&index).copied()))
            .collect::<Vec<_>>();
        let own = set
            .layers
            .iter()
            .map(|layer| layer.scale.area)
            .collect::<Vec<_>>();
        for (layer, growth) in set.layers.iter_mut().zip(chain_growth(&own, &parents)) {
            layer.growth = growth;
            layer.own.paint = layer.own.paint.grown(growth);
        }
        set
    }

    fn layer(&mut self, layer: &mut Value, path: &str) -> LayerInfo {
        let mut info = LayerInfo {
            path: path.to_owned(),
            ..LayerInfo::default()
        };
        let Some(layer) = layer.as_object_mut() else {
            return info;
        };
        let kind = layer.get("ty").and_then(Value::as_u64);
        if let Some(kind @ 5..) = kind {
            // velato refuses layer types past 4. A null layer draws nothing but keeps the index,
            // parent, transform, timing, and matte role other layers use.
            self.scan.found(undrawn_layer(kind), path);
            layer.insert("ty".into(), Value::from(3));
        }
        if kind == Some(1) {
            solid_as_shapes(layer, path, self.scan);
        }
        let kind = layer.get("ty").and_then(Value::as_u64);
        let hidden = layer.get("hd").and_then(Value::as_bool) == Some(true);
        info.is_matte = layer.get("td").and_then(Value::as_u64) == Some(1) && !hidden;
        if let Some(transform) = layer.get_mut("ks").and_then(Value::as_object_mut) {
            self.transform(transform, &format!("{path}/ks"), true);
            info.scale = Scale::of(transform);
        }
        if let Some(masks) = layer
            .get_mut("masksProperties")
            .and_then(Value::as_array_mut)
        {
            info.masks = masks.len() as u64;
            for (index, mask) in masks.iter_mut().enumerate() {
                let path = format!("{path}/masksProperties/{index}");
                if let Some(mask) = mask.as_object_mut() {
                    self.easings(mask.get_mut("pt"), &format!("{path}/pt"));
                    self.scalars(mask, &["o", "x"], &path);
                }
            }
        }
        // A hidden layer draws no content, and is no track matte.
        let mut content = Cost::default();
        match kind {
            Some(0) => {
                // Clip a precomposition to its own size with a mask, as lottie-web does.
                if let Some(clip) = precomposition_clip(layer)
                    && let Some(masks) = layer
                        .entry("masksProperties")
                        .or_insert_with(|| Value::Array(Vec::new()))
                        .as_array_mut()
                {
                    masks.push(clip);
                    info.masks += 1;
                }
                self.scalars(layer, &["tm"], path);
                if !hidden {
                    info.instance = layer
                        .get("refId")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
            }
            Some(2) if !hidden => {
                // An image is drawn at its asset's size.
                let (width, height) = layer
                    .get("refId")
                    .and_then(Value::as_str)
                    .and_then(|id| self.images.get(id))
                    .copied()
                    .unwrap_or((f64::INFINITY, f64::INFINITY));
                content.paint = Paint::draw(grown(width, height) / self.canvas);
            }
            Some(4) => {
                if let Some(shapes) = layer.get_mut("shapes") {
                    let (shapes, _) = self.shapes(shapes, &format!("{path}/shapes"), false);
                    if !hidden {
                        content = shapes;
                    }
                }
            }
            _ => {}
        }
        info.own = Cost {
            drawn: 1_u64.saturating_add(info.masks),
            offscreen: info.masks,
            ..Cost::default()
        }
        .then(content);
        info
    }

    /// Cost of a shape list (a layer's or group's) with repeats multiplied, and the bounds of its
    /// shapes, which a later style in an enclosing list also paints. Group bounds are transformed.
    fn shapes(&mut self, shapes: &mut Value, path: &str, in_group: bool) -> (Cost, Outline) {
        let Some(items) = shapes.as_array_mut() else {
            return (Cost::default(), Outline::default());
        };
        // Only named shape types are recognized here; numbered ones would bypass the budget.
        if let Some(position) = items
            .iter()
            .position(|item| !item.get("ty").is_some_and(Value::is_string))
        {
            self.scan.fail(ExtensionError::Malformed(format!(
                "a Lottie shape's type is not a string (at {path}/{position})"
            )));
            return (Cost::default(), Outline::default());
        }
        // velato refuses zig-zag and no-style items; removing them changes nothing else drawn.
        // Paths keep pointing into the original file.
        let mut kept = Vec::with_capacity(items.len());
        let mut position = 0;
        items.retain(|item| {
            let kind = item.get("ty").and_then(Value::as_str);
            if kind == Some("zz") {
                self.scan.found("zig zag", &format!("{path}/{position}"));
            }
            let keep = !matches!(kind, Some("zz" | "no"));
            if keep {
                kept.push(position);
            }
            position += 1;
            keep
        });
        let mut cost = Cost::default();
        // Bounds of the shapes so far, which a style paints.
        let mut outline = Outline::default();
        let mut transform = None;
        for (item, position) in items.iter_mut().zip(kept) {
            let path = format!("{path}/{position}");
            let Some(item) = item.as_object_mut() else {
                continue;
            };
            let kind = item.get("ty").and_then(Value::as_str).map(str::to_owned);
            match kind.as_deref() {
                Some("gr") => {
                    if let Some(group) = item.get_mut("it") {
                        let (group, shapes) = self.shapes(group, &format!("{path}/it"), true);
                        cost = cost.then(group);
                        outline = outline.then(shapes);
                    }
                }
                // velato reads a group's transform; one in a layer's own list it skips.
                Some("tr") if in_group => {
                    self.transform(item, &path, false);
                    // velato keeps a group's last.
                    transform = Some(Scale::of(item));
                }
                Some("rp") => (cost, outline) = self.repeater(item, &path, cost, outline),
                Some("sr") => {
                    self.scalars(item, &["or", "os", "r", "pt", "ir", "is"], &path);
                    if item.get("pt").map_or(0.0, reach) > MAX_POLYSTAR_POINTS {
                        self.scan.fail(ExtensionError::Budget(format!(
                            "a Lottie polystar has more than {MAX_POLYSTAR_POINTS} points (at {path})"
                        )));
                    }
                    outline = outline.then(polystar_bounds(item));
                    cost.drawn = cost.drawn.saturating_add(1);
                }
                Some("sh") => {
                    self.easings(item.get_mut("ks"), &format!("{path}/ks"));
                    outline =
                        outline.then(item.get("ks").map_or_else(Outline::unknown, path_bounds));
                    cost.drawn = cost.drawn.saturating_add(1);
                }
                Some("rc") => {
                    self.scalars(item, &["r"], &path);
                    outline = outline.then(size_bounds(item));
                    cost.drawn = cost.drawn.saturating_add(1);
                }
                Some("el") => {
                    outline = outline.then(size_bounds(item));
                    cost.drawn = cost.drawn.saturating_add(1);
                }
                Some(style @ ("fl" | "st" | "gf" | "gs")) => {
                    let stroke = matches!(style, "st" | "gs");
                    let paint = self.style(item, &path, stroke, outline);
                    cost.paint = cost.paint.then(paint);
                    cost.drawn = cost.drawn.saturating_add(1);
                }
                Some("tm") => self.scalars(item, &["s", "e", "o"], &path),
                Some("rd") => self.scalars(item, &["r"], &path),
                Some("pb" | "tw") => self.scalars(item, &["a"], &path),
                Some("op") => self.scalars(item, &["a", "ml"], &path),
                _ => {}
            }
        }
        let scale = transform.unwrap_or(Scale::NONE);
        cost.paint = cost.paint.grown(scale.area);
        (cost, outline.scaled(scale))
    }

    /// A fill or stroke, which paints every shape above it in its list (`outline`).
    fn style(
        &mut self,
        item: &mut Map<String, Value>,
        path: &str,
        stroke: bool,
        outline: Outline,
    ) -> Paint {
        self.scalars(item, &["o", "w", "ml2", "h", "a"], path);
        if let Some(colors) = item.get_mut("g").and_then(Value::as_object_mut) {
            // velato reads gradient keyframes' easing as scalar handles.
            self.easings(colors.get_mut("k"), &format!("{path}/g/k"));
        }
        if stroke && let Some(dashes) = item.get_mut("d").and_then(Value::as_array_mut) {
            for (index, dash) in dashes.iter_mut().enumerate() {
                if let Some(dash) = dash.as_object_mut() {
                    self.scalars(dash, &["v"], &format!("{path}/d/{index}"));
                }
            }
        }
        let area = if stroke {
            outline.stroked(stroke_reach(item))
        } else {
            outline.area
        };
        Paint::draw(area / self.canvas)
    }

    /// A repeater multiplies what it copies, adding an offscreen layer per shape and copy when
    /// copies are translucent.
    fn repeater(
        &mut self,
        item: &mut Map<String, Value>,
        path: &str,
        mut cost: Cost,
        outline: Outline,
    ) -> (Cost, Outline) {
        self.scalars(item, &["c", "o"], path);
        if let Some(transform) = item.get_mut("tr").and_then(Value::as_object_mut) {
            let path = format!("{path}/tr");
            // A repeater draws a split rotation by its z.
            match transform.get_mut("r").and_then(Value::as_object_mut) {
                Some(split) if !split.contains_key("k") => {
                    self.scalars(split, &["x", "y", "z"], &format!("{path}/r"));
                }
                _ => self.scalars(transform, &["r"], &path),
            }
            self.scalars(transform, &["o", "sk", "sa", "so", "eo"], &path);
            self.split_position(transform, &path);
        }
        let copies = item.get("c").map_or(0.0, reach);
        if copies > MAX_REPEATER_COPIES {
            self.scan.fail(ExtensionError::Budget(format!(
                "a Lottie repeater makes more than {MAX_REPEATER_COPIES} copies (at {path})"
            )));
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let copies = copies.round().clamp(1.0, MAX_REPEATER_COPIES) as u64;
        cost.drawn = cost.drawn.saturating_mul(copies);
        // A translucent copy draws through its own layer, in the worst case per shape.
        let opaque = |key: &str| {
            item.get("tr")
                .and_then(|transform| transform.get(key))
                .is_none_or(|opacity| opacity.get("k").and_then(Value::as_f64) == Some(100.0))
        };
        cost.offscreen = if opaque("so") && opaque("eo") {
            cost.offscreen.saturating_mul(copies)
        } else {
            cost.drawn
        };
        cost.depth += 1;
        let scale = copy_scale(item, copies);
        #[allow(clippy::cast_precision_loss)]
        let copies = copies as f64;
        cost.paint = cost.paint.grown(scale.area).times(copies);
        (cost, outline.repeated(scale, copies))
    }

    /// A position with separate x and y; velato reads each as a scalar.
    fn split_position(&mut self, transform: &mut Map<String, Value>, path: &str) {
        if let Some(position) = transform.get_mut("p").and_then(Value::as_object_mut)
            && position.get("s").and_then(Value::as_bool) == Some(true)
        {
            self.scalars(position, &["x", "y"], &format!("{path}/p"));
        }
    }

    /// A layer transform (`ks`) or a group's (`tr`).
    fn transform(&mut self, transform: &mut Map<String, Value>, path: &str, layer: bool) {
        // velato reads only `r`. 3D rotations would be dropped silently, and a split rotation or
        // a layer transform without `r` panics.
        if ["rx", "ry", "rz", "or"]
            .iter()
            .any(|key| transform.contains_key(*key))
        {
            self.scan.refuse("3D rotations", path);
            return;
        }
        match transform.get("r") {
            Some(rotation) if rotation.get("k").is_none() => {
                self.scan.refuse("split rotations", &format!("{path}/r"));
                return;
            }
            None if layer => {
                // The spec's default: no rotation.
                transform.insert("r".into(), serde_json::json!({ "a": 0, "k": 0 }));
            }
            _ => {}
        }
        // velato reads a split position on a layer but fails on one in a group.
        let split = transform
            .get("p")
            .and_then(|position| position.get("s"))
            .and_then(Value::as_bool)
            == Some(true);
        if split && !layer {
            self.scan.refuse("split positions", path);
            return;
        }
        self.split_position(transform, path);
        self.scalars(transform, &["r", "o", "sk", "sa"], path);
    }

    /// Scalar properties by key; velato reads each keyframe's first value and one easing value.
    fn scalars(&mut self, object: &mut Map<String, Value>, keys: &[&str], path: &str) {
        for key in keys {
            let Some(property) = object.get_mut(*key) else {
                continue;
            };
            let path = format!("{path}/{key}");
            if let Some(keyframes) = property.get("k").and_then(Value::as_array) {
                for (index, keyframe) in keyframes.iter().enumerate() {
                    if keyframe
                        .get("s")
                        .and_then(Value::as_array)
                        .is_some_and(Vec::is_empty)
                    {
                        self.scan
                            .refuse("keyframes without a value", &format!("{path}/k/{index}"));
                    }
                }
            }
            self.easings(Some(property), &path);
        }
    }

    /// Keeps the first value of each easing handle of a one-dimensional property, as lottie-web
    /// does.
    fn easings(&mut self, property: Option<&mut Value>, path: &str) {
        let Some(keyframes) = property
            .and_then(|property| property.get_mut("k"))
            .and_then(Value::as_array_mut)
        else {
            return;
        };
        for (index, keyframe) in keyframes.iter_mut().enumerate() {
            for handle in ["i", "o"] {
                let Some(handle) = keyframe.get_mut(handle).and_then(Value::as_object_mut) else {
                    continue;
                };
                for axis in ["x", "y"] {
                    if let Some(values) = handle.get_mut(axis).and_then(Value::as_array_mut) {
                        if values.is_empty() {
                            self.scan.refuse(
                                "easing handles without a value",
                                &format!("{path}/k/{index}"),
                            );
                        }
                        values.truncate(1);
                    }
                }
            }
        }
    }
}

/// Rewrites a solid layer (`ty` 1) as a shape layer with a filled rectangle, since velato draws
/// nothing for solids. Unreadable color or size is reported and left as is.
fn solid_as_shapes(layer: &mut Map<String, Value>, path: &str, scan: &mut Scan) {
    let number = |key: &str| layer.get(key).and_then(Value::as_f64);
    let color = layer.get("sc").and_then(Value::as_str).and_then(hex_color);
    let (Some(width), Some(height), Some(color)) = (number("sw"), number("sh"), color) else {
        scan.found("solid layers", path);
        return;
    };
    let static_value = |value: Value| serde_json::json!({ "a": 0, "k": value });
    layer.insert("ty".into(), Value::from(4));
    layer.insert(
        "shapes".into(),
        serde_json::json!([{
            "ty": "gr",
            "it": [
                {
                    "ty": "rc",
                    "d": 1,
                    "p": static_value(serde_json::json!([width / 2.0, height / 2.0])),
                    "s": static_value(serde_json::json!([width, height])),
                    "r": static_value(Value::from(0)),
                },
                {
                    "ty": "fl",
                    "c": static_value(serde_json::json!(color)),
                    "o": static_value(Value::from(100)),
                    "r": 1,
                },
                {
                    "ty": "tr",
                    "p": static_value(serde_json::json!([0, 0])),
                    "a": static_value(serde_json::json!([0, 0])),
                    "s": static_value(serde_json::json!([100, 100])),
                    "r": static_value(Value::from(0)),
                    "o": static_value(Value::from(100)),
                },
            ],
        }]),
    );
}

/// A `#rrggbb` color as Lottie's 0 to 1 RGBA.
fn hex_color(text: &str) -> Option<[f64; 4]> {
    let hex = text.strip_prefix('#')?;
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    let channel = |at: usize| {
        u8::from_str_radix(&hex[at..at + 2], 16)
            .ok()
            .map(|value| f64::from(value) / 255.0)
    };
    Some([channel(0)?, channel(2)?, channel(4)?, 1.0])
}

/// A mask of a precomposition layer's own rectangle, (0, 0) to its `w` and
/// `h`, when it has them.
fn precomposition_clip(layer: &Map<String, Value>) -> Option<Value> {
    let number = |key: &str| {
        layer
            .get(key)
            .and_then(Value::as_f64)
            .filter(|value| *value > 0.0)
    };
    let (width, height) = (number("w")?, number("h")?);
    Some(serde_json::json!({
        "mode": "a",
        "inv": false,
        "o": { "a": 0, "k": 100 },
        "pt": { "a": 0, "k": {
            "c": true,
            "v": [[0, 0], [width, 0], [width, height], [0, height]],
            "i": [[0, 0], [0, 0], [0, 0], [0, 0]],
            "o": [[0, 0], [0, 0], [0, 0], [0, 0]],
        } },
    }))
}

fn undrawn_layer(kind: u64) -> &'static str {
    match kind {
        5 => "text layers",
        6 => "audio layers",
        7 => "video placeholder layers",
        8 => "image sequence layers",
        9 => "video layers",
        10 => "image placeholder layers",
        11 => "guide layers",
        12 => "adjustment layers",
        13 => "camera layers",
        14 => "light layers",
        15 => "data layers",
        _ => "layers of an unknown type",
    }
}

/// The largest value a scalar property reaches at any frame. Easing stays within the handles'
/// heights and 0 to 1, so a handle beyond 1 overshoots by as much.
fn reach(property: &Value) -> f64 {
    let first = |value: &Value| match value {
        Value::Array(values) => values.first().and_then(Value::as_f64),
        value => value.as_f64(),
    };
    match property.get("k") {
        Some(Value::Array(keyframes)) if keyframes.iter().any(Value::is_object) => {
            let values = keyframes
                .iter()
                .map(|keyframe| keyframe.get("s").and_then(first).unwrap_or(0.0))
                .collect::<Vec<_>>();
            let mut most = values.iter().copied().fold(0.0, f64::max);
            for (pair, keyframe) in values.windows(2).zip(keyframes) {
                let (mut low, mut high) = (0.0_f64, 1.0_f64);
                for y in ["i", "o"]
                    .iter()
                    .filter_map(|handle| keyframe.get(*handle)?.get("y"))
                {
                    let heights = match y {
                        Value::Array(heights) => heights.as_slice(),
                        height => std::slice::from_ref(height),
                    };
                    for height in heights.iter().filter_map(Value::as_f64) {
                        (low, high) = (low.min(height), high.max(height));
                    }
                }
                let span = pair[1] - pair[0];
                most = most.max(pair[0] + span * low).max(pair[0] + span * high);
            }
            most
        }
        Some(value) => first(value).unwrap_or(0.0),
        None => 0.0,
    }
}

/// The range of a property's `component` at any frame, including easing overshoot (see
/// [`reach`]).
fn span(property: &Value, component: usize) -> Option<(f64, f64)> {
    let at = |value: &Value| match value {
        Value::Array(values) => values
            .get(component)
            .or_else(|| values.first())
            .and_then(Value::as_f64),
        value => value.as_f64(),
    };
    match property.get("k")? {
        Value::Array(keyframes) if keyframes.iter().any(Value::is_object) => {
            // A keyframe holds its value in `s`; old files end a segment's in `e`.
            let values = keyframes
                .iter()
                .map(|keyframe| keyframe.get("s").and_then(at))
                .collect::<Vec<_>>();
            let mut known = values.iter().flatten().copied().chain(
                keyframes
                    .iter()
                    .filter_map(|keyframe| keyframe.get("e").and_then(at)),
            );
            let first = known.next()?;
            let (mut low, mut high) = known.fold((first, first), |(low, high), value| {
                (low.min(value), high.max(value))
            });
            for (pair, keyframe) in values.windows(2).zip(keyframes) {
                let (Some(from), Some(to)) = (pair[0], pair[1]) else {
                    continue;
                };
                let (least, most) = easing_heights(keyframe);
                let span = to - from;
                for value in [from + span * least, from + span * most] {
                    (low, high) = (low.min(value), high.max(value));
                }
            }
            Some((low, high))
        }
        value => at(value).map(|value| (value, value)),
    }
}

/// The range an eased segment covers as a fraction from one keyframe to the next, including
/// handle overshoot.
fn easing_heights(keyframe: &Value) -> (f64, f64) {
    let (mut low, mut high) = (0.0_f64, 1.0_f64);
    for y in ["i", "o"]
        .iter()
        .filter_map(|handle| keyframe.get(*handle)?.get("y"))
    {
        let heights = match y {
            Value::Array(heights) => heights.as_slice(),
            height => std::slice::from_ref(height),
        };
        for height in heights.iter().filter_map(Value::as_f64) {
            (low, high) = (low.min(height), high.max(height));
        }
    }
    (low, high)
}

/// The largest magnitude of `component` at any frame; infinite when unknown.
fn magnitude(property: &Value, component: usize) -> f64 {
    span(property, component).map_or(f64::INFINITY, |(low, high)| low.abs().max(high.abs()))
}

fn size_bounds(item: &Map<String, Value>) -> Outline {
    item.get("s").map_or_else(Outline::unknown, |size| {
        Outline::of(magnitude(size, 0), magnitude(size, 1))
    })
}

/// A polystar's bounds: the square of its larger radius, grown by its roundness handles.
fn polystar_bounds(item: &Map<String, Value>) -> Outline {
    let of = |key: &str| item.get(key).map_or(0.0, |value| magnitude(value, 0));
    let roundness = 1.0 + of("os").max(of("is")) / 100.0;
    let side = 2.0 * of("or").max(of("ir")) * roundness;
    Outline::of(side, side)
}

/// A path's bounds over all keyframes: vertices and handles, grown by easing overshoot.
fn path_bounds(property: &Value) -> Outline {
    let mut boxed = Bounds::default();
    let mut shape = |shape: &Value| {
        let points = |key: &str| shape.get(key).and_then(Value::as_array);
        let point = |value: &Value| {
            let value = value.as_array()?;
            Some((value.first()?.as_f64()?, value.get(1)?.as_f64()?))
        };
        let Some(vertices) = points("v") else {
            return false;
        };
        for (index, vertex) in vertices.iter().enumerate() {
            let Some((x, y)) = point(vertex) else {
                return false;
            };
            boxed.add(x, y);
            for handles in [points("i"), points("o")].into_iter().flatten() {
                if let Some((dx, dy)) = handles.get(index).and_then(point) {
                    boxed.add(x + dx, y + dy);
                }
            }
        }
        true
    };
    let known = match property.get("k") {
        Some(Value::Array(keyframes)) => {
            let mut known = true;
            let mut overshoot = 0.0_f64;
            for keyframe in keyframes {
                for key in ["s", "e"] {
                    match keyframe.get(key) {
                        Some(Value::Array(shapes)) => known &= shapes.iter().all(&mut shape),
                        Some(other) => known &= shape(other),
                        None => {}
                    }
                }
                let (least, most) = easing_heights(keyframe);
                overshoot = overshoot.max(-least).max(most - 1.0);
            }
            boxed.grow(overshoot);
            known
        }
        Some(shape_value) => shape(shape_value),
        None => false,
    };
    match boxed.size() {
        Some((width, height)) if known => Outline::of(width, height),
        _ => Outline::unknown(),
    }
}

#[derive(Default)]
struct Bounds(Option<(f64, f64, f64, f64)>);

impl Bounds {
    fn add(&mut self, x: f64, y: f64) {
        let (left, top, right, bottom) = self.0.unwrap_or((x, y, x, y));
        self.0 = Some((left.min(x), top.min(y), right.max(x), bottom.max(y)));
    }

    fn grow(&mut self, times: f64) {
        if let Some((left, top, right, bottom)) = self.0 {
            let (dx, dy) = ((right - left) * times, (bottom - top) * times);
            self.0 = Some((left - dx, top - dy, right + dx, bottom + dy));
        }
    }

    fn size(&self) -> Option<(f64, f64)> {
        self.0
            .map(|(left, top, right, bottom)| (right - left, bottom - top))
    }
}

/// How far a stroke reaches beyond its path at most, including miter joins (default limit 4)
/// and square caps.
fn stroke_reach(item: &Map<String, Value>) -> f64 {
    let width = item.get("w").map_or(0.0, |width| magnitude(width, 0));
    let join = if item.get("lj").and_then(Value::as_u64) == Some(1) {
        item.get("ml").and_then(Value::as_f64).unwrap_or(4.0)
    } else {
        1.0
    };
    let cap = if item.get("lc").and_then(Value::as_u64) == Some(3) {
        std::f64::consts::SQRT_2
    } else {
        1.0
    };
    width / 2.0 * join.max(cap).max(1.0)
}

/// The most one repeater copy's transform grows what it copies. Copy `t` scales by the scale to
/// the power `t`, which peaks at the first or last copy.
fn copy_scale(item: &Map<String, Value>, copies: u64) -> Scale {
    let transform = item.get("tr");
    let offset = item.get("o").and_then(|offset| span(offset, 0));
    let (first, last_offset) = offset.unwrap_or((0.0, 0.0));
    #[allow(clippy::cast_precision_loss)]
    let last = last_offset + copies.saturating_sub(1) as f64;
    // velato draws a negative scale between two whole powers.
    let powers = [first.floor(), last.ceil()];
    let axis = |component| {
        let Some((low, high)) = transform
            .and_then(|transform| transform.get("s"))
            .map_or(Some((100.0, 100.0)), |scale| span(scale, component))
        else {
            return f64::INFINITY;
        };
        let smallest = if low <= 0.0 && high >= 0.0 {
            0.0
        } else {
            low.abs().min(high.abs())
        };
        let largest = low.abs().max(high.abs());
        [smallest / 100.0, largest / 100.0]
            .into_iter()
            .flat_map(|base| powers.map(|power| base.powf(power)))
            .fold(0.0, |most: f64, value| {
                if value.is_nan() {
                    f64::INFINITY
                } else {
                    most.max(value)
                }
            })
    };
    let (x, y) = (axis(0), axis(1));
    Scale {
        area: grown(x, y),
        stretch: x.max(y),
    }
}

/// Image asset sizes (`w`, `h`) by ID; velato draws images at their asset size.
fn image_sizes(json: &Value) -> HashMap<String, (f64, f64)> {
    json.get("assets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|asset| asset.get("layers").is_none())
        .filter_map(|asset| {
            let number = |key: &str| asset.get(key).and_then(Value::as_f64).map(f64::abs);
            Some((
                asset.get("id")?.as_str()?.to_owned(),
                (
                    number("w").unwrap_or(f64::INFINITY),
                    number("h").unwrap_or(f64::INFINITY),
                ),
            ))
        })
        .collect()
}

#[derive(Clone, Copy)]
enum Node {
    Set(usize),
    Layer(usize, usize),
}

#[derive(Clone, Copy)]
enum Mark {
    New,
    /// On the walk's stack; reaching it again is a cycle.
    Open,
    Done(Cost),
}

/// The drawing graph from the root composition: sets draw their non-matte layers, and a layer
/// draws its matte and the asset it instances.
struct Drawing<'a> {
    sets: &'a [(Option<String>, LayerSet)],
    /// Set by precomposition ID; velato keeps the last of an ID.
    assets: HashMap<&'a str, usize>,
    set_marks: Vec<Mark>,
    layer_marks: Vec<Vec<Mark>>,
}

impl<'a> Drawing<'a> {
    fn new(sets: &'a [(Option<String>, LayerSet)]) -> Self {
        Self {
            sets,
            assets: sets
                .iter()
                .enumerate()
                .filter_map(|(index, (id, _))| Some((id.as_deref()?, index)))
                .collect(),
            set_marks: vec![Mark::New; sets.len()],
            layer_marks: sets
                .iter()
                .map(|(_, set)| vec![Mark::New; set.layers.len()])
                .collect(),
        }
    }

    fn layer(&self, set: usize, layer: usize) -> &'a LayerInfo {
        &self.sets[set].1.layers[layer]
    }

    fn instance(&self, set: usize, layer: usize) -> Option<usize> {
        self.layer(set, layer)
            .instance
            .as_deref()
            .and_then(|id| self.assets.get(id).copied())
    }

    fn child(&self, node: Node, next: usize) -> Option<Node> {
        match node {
            Node::Set(set) => self.sets[set]
                .1
                .drawn
                .get(next)
                .map(|&layer| Node::Layer(set, layer)),
            Node::Layer(set, layer) => self
                .layer(set, layer)
                .matte
                .map(|matte| Node::Layer(set, matte))
                .into_iter()
                .chain(self.instance(set, layer).map(Node::Set))
                .nth(next),
        }
    }

    fn mark(&mut self, node: Node) -> &mut Mark {
        match node {
            Node::Set(set) => &mut self.set_marks[set],
            Node::Layer(set, layer) => &mut self.layer_marks[set][layer],
        }
    }

    fn done(&self, node: Node) -> Cost {
        let mark = match node {
            Node::Set(set) => self.set_marks[set],
            Node::Layer(set, layer) => self.layer_marks[set][layer],
        };
        match mark {
            Mark::Done(cost) => cost,
            Mark::New | Mark::Open => Cost::default(),
        }
    }

    fn finish(&self, node: Node) -> Cost {
        match node {
            Node::Set(set) => self.sets[set]
                .1
                .drawn
                .iter()
                .map(|&layer| self.done(Node::Layer(set, layer)))
                .fold(Cost::default(), Cost::then),
            Node::Layer(set, layer) => {
                let info = self.layer(set, layer);
                let matte = info.matte.map(|matte| self.done(Node::Layer(set, matte)));
                let inner = self
                    .instance(set, layer)
                    .map(|inner| self.done(Node::Set(inner)));
                // A matte uses two offscreen layers, masks are held while the content draws, and
                // a precomposition's layers draw one level down.
                let held = if matte.is_some() { 2 } else { 0 };
                let mut cost = Cost {
                    offscreen: held,
                    ..Cost::default()
                }
                .then(info.own)
                .then(inner.map_or_else(Cost::default, |inner| Cost {
                    depth: inner.depth.saturating_add(1),
                    paint: inner.paint.grown(info.growth),
                    ..inner
                }));
                cost.depth = (held + info.masks).saturating_add(cost.depth);
                if let Some(matte) = matte {
                    cost = cost.then(Cost {
                        depth: matte.depth.saturating_add(1),
                        ..matte
                    });
                }
                cost
            }
        }
    }

    fn cycle(&self, node: Node) -> ExtensionError {
        ExtensionError::Malformed(match node {
            Node::Set(set) => format!(
                "a Lottie precomposition draws itself (at {})",
                self.sets[set].1.path
            ),
            Node::Layer(set, layer) => format!(
                "a Lottie track matte mattes itself (at {})",
                self.layer(set, layer).path
            ),
        })
    }
}

/// Refuses self-drawing layers and documents over the drawing or nesting budget. The walk uses an
/// explicit stack because precomposition and matte chains can be arbitrarily long.
fn check_drawing(sets: &[(Option<String>, LayerSet)]) -> Result<(), ExtensionError> {
    if sets.first().is_none_or(|(id, _)| id.is_some()) {
        return Ok(());
    }
    let mut drawing = Drawing::new(sets);
    let mut stack = vec![(Node::Set(0), 0_usize)];
    *drawing.mark(Node::Set(0)) = Mark::Open;
    while let Some(&(node, next)) = stack.last() {
        if let Some(child) = drawing.child(node, next) {
            if let Some(top) = stack.last_mut() {
                top.1 += 1;
            }
            match *drawing.mark(child) {
                Mark::New => {
                    *drawing.mark(child) = Mark::Open;
                    stack.push((child, 0));
                }
                Mark::Open => return Err(drawing.cycle(child)),
                Mark::Done(..) => {}
            }
            continue;
        }
        let cost = drawing.finish(node);
        *drawing.mark(node) = Mark::Done(cost);
        stack.pop();
    }
    let Cost {
        drawn,
        offscreen,
        depth,
        paint,
    } = drawing.done(Node::Set(0));
    if drawn > MAX_DRAWN {
        return Err(ExtensionError::Budget(format!(
            "a frame of the Lottie animation draws more than {MAX_DRAWN} layers, masks, and shapes"
        )));
    }
    if offscreen > MAX_OFFSCREEN {
        return Err(ExtensionError::Budget(format!(
            "a frame of the Lottie animation composites more than {MAX_OFFSCREEN} offscreen layers"
        )));
    }
    if depth > MAX_NESTING {
        return Err(ExtensionError::Budget(format!(
            "the Lottie animation's layers nest more than {MAX_NESTING} deep"
        )));
    }
    if paint.canvases() > MAX_DRAWING {
        // Name the root layer that paints the most.
        let (_, heaviest) = &sets[0];
        let most = heaviest
            .drawn
            .iter()
            .map(|&layer| (drawing.done(Node::Layer(0, layer)).paint.canvases(), layer))
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .map_or(heaviest.path.as_str(), |(_, layer)| {
                heaviest.layers[layer].path.as_str()
            });
        return Err(ExtensionError::Budget(format!(
            "a frame of the Lottie animation paints more than {MAX_DRAWING} times its canvas (the most at {most})"
        )));
    }
    Ok(())
}

/// The most each layer's transform chain grows an area. velato follows parent loops too, so a
/// loop that grows areas is unbounded, and one that does not grows at most by its growing layers.
fn chain_growth(own: &[f64], parents: &[Option<usize>]) -> Vec<f64> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        New,
        OnPath,
        Done,
    }
    let mut growth = vec![1.0; own.len()];
    let mut state = vec![State::New; own.len()];
    for start in 0..own.len() {
        let mut path: Vec<usize> = Vec::new();
        let mut at = Some(start);
        while let Some(layer) = at {
            match state[layer] {
                State::Done => break,
                State::OnPath => {
                    let from = path.iter().position(|&on| on == layer).unwrap_or(0);
                    let cycle = path.split_off(from);
                    let around = cycle.iter().map(|&layer| own[layer]).fold(1.0_f64, grown);
                    let most = if around > 1.0 || around.is_nan() {
                        f64::INFINITY
                    } else {
                        cycle
                            .iter()
                            .map(|&layer| own[layer].max(1.0_f64))
                            .fold(1.0_f64, grown)
                    };
                    for layer in cycle {
                        growth[layer] = most;
                        state[layer] = State::Done;
                    }
                    break;
                }
                State::New => {
                    state[layer] = State::OnPath;
                    path.push(layer);
                    at = parents[layer];
                }
            }
        }
        while let Some(layer) = path.pop() {
            let above = parents[layer].map_or(1.0, |parent| growth[parent]);
            growth[layer] = grown(own[layer], above);
            state[layer] = State::Done;
        }
    }
    growth
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape walk refuses numbered `ty` tags. Any other internally tagged enum in a velato
    /// update would accept numbers too, so this test fails until the walk handles it.
    #[test]
    fn the_only_tagged_enum_velato_reads_is_its_shapes() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
        let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        let output = std::process::Command::new(cargo)
            .args([
                "metadata",
                "--format-version",
                "1",
                "--offline",
                "--locked",
                // Other platforms' packages may not be downloaded, and `--offline` fails on them.
                "--filter-platform",
                "host-tuple",
                "--manifest-path",
                manifest,
            ])
            .output()
            .expect("cargo metadata");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata: Value = serde_json::from_slice(&output.stdout).expect("metadata");
        let velato = metadata["packages"]
            .as_array()
            .expect("packages")
            .iter()
            .find(|package| package["name"] == "velato")
            .expect("velato");
        let source = std::path::Path::new(velato["manifest_path"].as_str().expect("path"))
            .parent()
            .expect("crate")
            .join("src");
        let mut tagged = Vec::new();
        let mut directories = vec![source];
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(&directory).expect("directory") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    directories.push(path);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    let text = std::fs::read_to_string(&path).expect("source");
                    let mut lines = text.lines();
                    while let Some(line) = lines.next() {
                        // `tag = "..."`, alone or with `content`; not `untagged`.
                        let tagged_here = line.contains("#[serde(")
                            && line
                                .split(['(', ',', ')'])
                                .any(|part| part.trim_start().starts_with("tag"));
                        if !tagged_here {
                            continue;
                        }
                        let item = lines
                            .by_ref()
                            .find(|line| line.contains("enum ") || line.contains("struct "))
                            .expect("an item after its attribute");
                        tagged.push(
                            item.split_whitespace()
                                .skip_while(|word| *word != "enum" && *word != "struct")
                                .nth(1)
                                .expect("a name")
                                .trim_end_matches(['{', '<'])
                                .to_owned(),
                        );
                    }
                }
            }
        }
        assert_eq!(tagged, ["AnyShape"]);
    }

    #[test]
    fn a_layer_grows_as_its_parents_do_round_a_loop_too() {
        // 0 is inside 1 inside 2; 3 and 4 parent each other, growing; 5 and 6
        // too, one growing and one shrinking as much.
        let own = [2.0, 3.0, 0.5, 2.0, 1.0, 4.0, 0.25];
        let parents = [Some(1), Some(2), None, Some(4), Some(3), Some(6), Some(5)];
        let growth = chain_growth(&own, &parents);
        assert_eq!(growth[..3], [3.0, 1.5, 0.5]);
        assert!(growth[3].is_infinite() && growth[4].is_infinite());
        assert_eq!(growth[5..], [4.0, 4.0]);
    }

    #[test]
    fn a_repeater_copy_grows_as_its_largest_power_of_the_scale() {
        let repeater = |scale: [f64; 2], offset: f64| {
            serde_json::json!({
                "ty": "rp", "c": {"a": 0, "k": 3}, "o": {"a": 0, "k": offset},
                "tr": {"s": {"a": 0, "k": scale}}
            })
        };
        let scale = |item: Value, copies| {
            let scale = copy_scale(item.as_object().expect("object"), copies);
            (scale.area, scale.stretch)
        };
        // Copies 0 to 2: the last is scaled twice.
        assert_eq!(scale(repeater([200.0, 100.0], 0.0), 3), (4.0, 4.0));
        // Shrinking, the first, unscaled, is the largest; offset by -1, the one before it.
        assert_eq!(scale(repeater([50.0, 50.0], 0.0), 3), (1.0, 1.0));
        assert_eq!(scale(repeater([50.0, 50.0], -1.0), 3), (4.0, 2.0));
        // Scaled to nothing, a copy before the first has no bound.
        assert!(scale(repeater([0.0, 100.0], -1.0), 3).0.is_infinite());
    }

    #[test]
    fn a_draw_covers_a_canvas_at_most_whatever_its_bounds() {
        let large = Paint::draw(4.0);
        let small = Paint::draw(0.25).times(8.0);
        assert_eq!(large.then(small).canvases(), 3.0);
        // Grown four times, each small draw covers a canvas; shrunk, the large one a quarter.
        assert_eq!(large.then(small).grown(4.0).canvases(), 9.0);
        assert_eq!(large.then(small).grown(1.0 / 16.0).canvases(), 0.375);
        // A bound past what a float holds, made nothing, is still no bound.
        assert_eq!(Paint::draw(f64::INFINITY).grown(0.0).canvases(), 1.0);
    }
}

//! Estimates usvg's conversion cost on the parsed XML tree before usvg runs.
//!
//! usvg 0.47 expands `<use>` copies, matches every style rule against every element (descendant
//! combinators backtrack, so cost grows as a power of depth), resolves properties through
//! ancestors, splits arcs into cubics ([`super::path_data`]), copies marker content at every
//! vertex after its own node limit check, and converts the patterns, masks, clip paths, and
//! filters an element names by recursing into them. Its own limits do not bound this: a 400-byte
//! selector over 100 nested groups does not finish in a minute, a large marker on a long path can
//! use tens of GB, and three patterns that fill each other overflow any stack. Documents over
//! their share of [`super::MAX_READING`] or [`super::MAX_NESTING`], or whose references form a
//! ring, are refused.

use std::collections::{HashMap, HashSet};

use resvg::usvg::roxmltree::{Document, Node, NodeId};

use super::path_data::{PathData, path_data, points};
use crate::api::ExtensionError;

const SVG_NS: &str = "http://www.w3.org/2000/svg";
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";

/// Cost of one ancestry level per element for property inheritance (about 18 ns natively, three
/// selector steps).
const DEPTH_STEP: u64 = 3;

/// Most elements a selector match may visit; simplecss recurses per hop, so more could overflow
/// the stack.
const MAX_SELECTOR_HOPS: u32 = 512;

/// Most elements in the converted tree with `<use>` copies and markers in place. Matches usvg's
/// own limit, which markers bypass.
pub(super) const MAX_ELEMENTS: u64 = 1_000_000;

/// Nesting levels one paint, mask, clip path, filter, or template reference counts as. usvg
/// recurses through several frames per reference (about 5.7 kB of stack for a pattern), so
/// [`super::MAX_NESTING`] references of one level each would overflow a 1 MiB stack.
pub(crate) const REFERENCE_LEVELS: usize = 4;

/// Most vertices of a basic shape (a rounded rectangle's lines and corner cubics).
const SHAPE_VERTICES: u64 = 16;

/// Steps per character of a `<text>` element: shaping and one outline per glyph take about
/// 1.4 us a character natively.
const TEXT_CHAR_STEPS: u64 = 190;

/// Bytes of a `data:` URL per step. usvg decodes the URL again for every element that draws it,
/// about 1 ns a byte natively.
const DATA_URL_BYTES_PER_STEP: u64 = 8;

/// Character pairs of one `<text>` element per step. usvg's horizontal layout walks a chunk from
/// its start for every cluster, about 0.5 ns per pair of characters natively.
const TEXT_PAIRS_PER_STEP: u64 = 15;

/// A property whose value can name another element.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Property {
    Marker,
    Fill,
    Stroke,
    Mask,
    ClipPath,
    Filter,
}

const PROPERTIES: [(&str, Property); 9] = [
    ("marker", Property::Marker),
    ("marker-start", Property::Marker),
    ("marker-mid", Property::Marker),
    ("marker-end", Property::Marker),
    ("fill", Property::Fill),
    ("stroke", Property::Stroke),
    ("mask", Property::Mask),
    ("clip-path", Property::ClipPath),
    ("filter", Property::Filter),
];

fn property(name: &str) -> Option<Property> {
    PROPERTIES
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, property)| *property)
}

/// Elements usvg draws with a fill or stroke paint.
fn paints(name: &str) -> bool {
    matches!(
        name,
        "path"
            | "rect"
            | "circle"
            | "ellipse"
            | "line"
            | "polyline"
            | "polygon"
            | "text"
            | "tspan"
            | "textPath"
            | "tref"
    )
}

fn paint_server(name: &str) -> bool {
    matches!(name, "pattern" | "linearGradient" | "radialGradient")
}

/// The ids every `url(#id)` in a property value names.
fn urls(value: &str) -> impl Iterator<Item = &str> {
    value.split("url(").skip(1).filter_map(|rest| {
        rest[..rest.find(')')?]
            .trim()
            .trim_matches(['"', '\''])
            .strip_prefix('#')
    })
}

pub(super) struct Conversion {
    pub(super) steps: u64,
    pub(super) depth: usize,
}

/// Counts the conversion cost of `doc`, refusing it past `steps` or `depth`.
pub(super) fn conversion(
    doc: &Document,
    steps: u64,
    depth: usize,
) -> Result<Conversion, ExtensionError> {
    let mut count = Count::new(steps, depth);
    count.match_styles(doc)?;
    count.index(doc);
    let root = count.subtree(doc.root(), 0, 0)?;
    let steps = root.cost().saturating_add(count.resolving);
    if steps > count.budget {
        return Err(too_costly(count.budget));
    }
    if root.elements > MAX_ELEMENTS {
        return Err(too_many());
    }
    Ok(Conversion {
        steps,
        depth: root.height,
    })
}

fn too_many() -> ExtensionError {
    ExtensionError::Budget(format!(
        "the SVG converts to more than {MAX_ELEMENTS} elements with its `use` copies, markers, \
         and references in place"
    ))
}

fn too_costly(budget: u64) -> ExtensionError {
    ExtensionError::Budget(format!(
        "reading the SVG takes more than {budget} steps (its elements, each as deep as it nests, \
         its `use` copies, markers, and references, its paths' arcs, and its style rules matched \
         against each element)"
    ))
}

fn ring() -> ExtensionError {
    ExtensionError::Malformed(
        "a paint, mask, clip path or filter of the SVG refers to itself".to_owned(),
    )
}

/// One element's share of the converted tree, wherever it is copied.
#[derive(Clone, Copy, Default)]
struct Subtree {
    /// Sum of each element's depth below this one.
    depths: u64,
    elements: u64,
    /// Selector matching, declarations, and `<use>` loop checks.
    work: u64,
    /// Levels reached, itself included.
    height: usize,
}

/// A subtree being counted, with the references on the path when it started, or counted.
#[derive(Clone, Copy)]
enum Counted {
    Counting(usize),
    Done(Subtree),
}

/// What an element and its ancestors name that its content uses: markers, and the paints a
/// fill or stroke inherits.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
struct Context {
    markers: Vec<NodeId>,
    fills: Vec<NodeId>,
    strokes: Vec<NodeId>,
}

struct Count<'a> {
    budget: u64,
    depth_limit: usize,
    /// Style matching cost per element.
    styles: HashMap<NodeId, u64>,
    /// Declarations of [`PROPERTIES`] that matching style rules make, per element. A fill or
    /// stroke value that names nothing is kept empty.
    styled: HashMap<NodeId, Vec<(Property, String)>>,
    /// Counted subtrees per (element, context). usvg leaves out a `<use>` of itself, so a subtree
    /// reached again while counting counts as empty, unless a reference lies on the way back.
    subtrees: HashMap<(NodeId, usize), Counted>,
    /// The first element carrying each id, as usvg resolves `<use>`.
    ids: HashMap<&'a str, NodeId>,
    /// Every element carrying each id; a reference may resolve to any of them.
    all_ids: HashMap<&'a str, Vec<NodeId>>,
    contexts: Vec<Context>,
    context_ids: HashMap<Context, usize>,
    /// The context each referenced element's ancestors give its content.
    outside: HashMap<NodeId, usize>,
    /// References on the path being counted.
    references: usize,
    /// Paint servers and templates usvg converts once and shares.
    shared: HashSet<NodeId>,
    /// Steps spent resolving the paints contexts name, one per candidate element.
    resolving: u64,
    /// Elements under and including each `<use>` target, which usvg checks per copy.
    descendants: HashMap<NodeId, u64>,
    paths: HashMap<NodeId, PathData>,
}

impl<'a> Count<'a> {
    fn new(budget: u64, depth_limit: usize) -> Self {
        Count {
            budget,
            depth_limit,
            styles: HashMap::new(),
            styled: HashMap::new(),
            subtrees: HashMap::new(),
            ids: HashMap::new(),
            all_ids: HashMap::new(),
            contexts: vec![Context::default()],
            context_ids: HashMap::from([(Context::default(), 0)]),
            outside: HashMap::new(),
            references: 0,
            shared: HashSet::new(),
            resolving: 0,
            descendants: HashMap::new(),
            paths: HashMap::new(),
        }
    }

    fn index(&mut self, doc: &'a Document<'_>) {
        for node in doc.descendants() {
            if let Some(id) = node.attribute("id") {
                // As in usvg, a `<use>` links to the first element carrying it.
                self.ids.entry(id).or_insert(node.id());
                self.all_ids.entry(id).or_default().push(node.id());
            }
        }
    }

    /// Matches every style rule against every element as usvg will, stopping once the budget is
    /// spent.
    fn match_styles(&mut self, doc: &'a Document<'_>) -> Result<(), ExtensionError> {
        let mut sheet = simplecss::StyleSheet::new();
        for node in doc.descendants().filter(|node| node.has_tag_name("style")) {
            if !matches!(node.attribute("type"), None | Some("text/css")) {
                continue;
            }
            if let Some(text) = node.text() {
                sheet.parse_more(text);
            }
        }
        if sheet.rules.is_empty() {
            return Ok(());
        }
        let meter = Meter {
            left: std::cell::Cell::new(self.budget),
            deep: std::cell::Cell::new(false),
        };
        for node in doc.descendants().filter(Node::is_element) {
            let before = meter.left.get();
            for rule in &sheet.rules {
                if !meter.spend(1) {
                    return Err(too_costly(self.budget));
                }
                let element = Styled {
                    node,
                    hops: 0,
                    meter: &meter,
                };
                if rule.selector.matches(&element) {
                    if !meter.spend(u64::try_from(rule.declarations.len()).unwrap_or(u64::MAX)) {
                        return Err(too_costly(self.budget));
                    }
                    for declaration in &rule.declarations {
                        let Some(property) = property(declaration.name) else {
                            continue;
                        };
                        let value = declaration.value;
                        let kept = if value.contains("url(") || value.trim() == "inherit" {
                            value.to_owned()
                        } else {
                            String::new()
                        };
                        self.styled
                            .entry(node.id())
                            .or_default()
                            .push((property, kept));
                    }
                }
                if meter.deep.get() {
                    return Err(ExtensionError::Budget(format!(
                        "a style rule of the SVG looks more than {MAX_SELECTOR_HOPS} elements away from the one it styles"
                    )));
                }
                if meter.left.get() == 0 {
                    return Err(too_costly(self.budget));
                }
            }
            self.styles.insert(node.id(), before - meter.left.get());
        }
        // Matching cost is added per converted copy in the subtree walk.
        Ok(())
    }

    /// The converted subtree of `node` at `level`, under context `inherited`.
    fn subtree(
        &mut self,
        node: Node<'a, '_>,
        level: usize,
        inherited: usize,
    ) -> Result<Subtree, ExtensionError> {
        if node.is_element() && level >= self.depth_limit {
            return Err(too_deep(self.depth_limit));
        }
        let key = (node.id(), inherited);
        match self.subtrees.get(&key) {
            Some(Counted::Counting(references)) if self.references > *references => {
                return Err(ring());
            }
            Some(Counted::Counting(_)) => return Ok(Subtree::default()),
            Some(Counted::Done(known)) => {
                if level + known.height > self.depth_limit {
                    return Err(too_deep(self.depth_limit));
                }
                return Ok(*known);
            }
            None => {}
        }
        self.subtrees
            .insert(key, Counted::Counting(self.references));
        let own = if node.is_element() {
            // usvg keeps only SVG or unnamespaced elements, and drops `<style>`.
            if !matches!(node.tag_name().namespace(), None | Some(SVG_NS))
                || node.tag_name().name() == "style"
            {
                Subtree::default()
            } else {
                self.element(node, level, inherited)?
            }
        } else {
            // The document root's children are at its own level.
            let mut subtree = Subtree::default();
            for child in node.children().filter(Node::is_element) {
                let child = self.subtree(child, level, inherited)?;
                subtree.depths = subtree.depths.saturating_add(child.depths);
                subtree.elements = subtree.elements.saturating_add(child.elements);
                subtree.work = subtree.work.saturating_add(child.work);
                subtree.height = subtree.height.max(child.height);
                self.within_budget(&subtree)?;
            }
            subtree
        };
        self.within_budget(&own)?;
        self.subtrees.insert(key, Counted::Done(own));
        Ok(own)
    }

    /// The converted subtree of an SVG element.
    fn element(
        &mut self,
        node: Node<'a, '_>,
        level: usize,
        inherited: usize,
    ) -> Result<Subtree, ExtensionError> {
        let here = self.context(inherited, node);
        if self.resolving > self.budget {
            return Err(too_costly(self.budget));
        }
        let mut subtree = Subtree {
            elements: 1,
            work: self.styles.get(&node.id()).copied().unwrap_or(0),
            height: 1,
            ..Subtree::default()
        };
        let name = node.tag_name().name();
        if name == "use" {
            if let Some(link) = self.link(node) {
                // usvg checks the copy's descendants for a `<use>` naming it.
                let checked = *self.descendants.entry(link.id()).or_insert_with(|| {
                    u64::try_from(link.descendants().count()).unwrap_or(u64::MAX)
                });
                subtree.work = subtree.work.saturating_add(checked);
                let copy = self.subtree(link, level + 1, here)?;
                subtree.add(copy);
            }
        } else {
            for child in node.children().filter(Node::is_element) {
                let child = self.subtree(child, level + 1, here)?;
                subtree.add(child);
                self.within_budget(&subtree)?;
            }
        }
        if matches!(name, "image" | "feImage")
            && let Some(href) = node
                .attribute((XLINK_NS, "href"))
                .or_else(|| node.attribute("href"))
                .filter(|href| href.trim_start().starts_with("data:"))
        {
            let bytes = u64::try_from(href.len()).unwrap_or(u64::MAX);
            subtree.work = subtree.work.saturating_add(bytes / DATA_URL_BYTES_PER_STEP);
        }
        if name == "text" {
            // Every glyph becomes an outline, whether or not a font is registered here.
            let chars: u64 = node
                .descendants()
                .filter(Node::is_text)
                .map(|text| text.text().map_or(0, |text| text.chars().count() as u64))
                .sum();
            let pairs = chars.saturating_mul(chars) / 2;
            subtree.elements = subtree.elements.saturating_add(chars);
            subtree.work = subtree
                .work
                .saturating_add(chars.saturating_mul(TEXT_CHAR_STEPS))
                .saturating_add(pairs / TEXT_PAIRS_PER_STEP);
            self.within_budget(&subtree)?;
        }
        let steps = self.path(node).steps;
        subtree.work = subtree.work.saturating_add(steps);
        // A text path reads the path it follows again.
        if name == "textPath"
            && let Some(link) = self.link(node)
        {
            let steps = self.path(link).steps;
            subtree.work = subtree.work.saturating_add(steps);
        }
        if let Some(vertices) = self.vertices(node) {
            for marker in self.contexts[here].markers.clone() {
                let Some(marker) = node.document().get_node(marker) else {
                    continue;
                };
                let outside = self.outside_context(marker);
                let instance = self.subtree(marker, level + 1, outside)?;
                subtree.add_copies(instance, vertices);
                self.within_budget(&subtree)?;
            }
        }
        for (target, shared) in self.referenced(node, here, &mut subtree) {
            let Some(target) = node.document().get_node(target) else {
                continue;
            };
            let outside = self.outside_context(target);
            self.references += 1;
            let counted = self.subtree(target, level + REFERENCE_LEVELS, outside);
            self.references -= 1;
            let counted = counted?;
            if shared && !self.shared.insert(target.id()) {
                subtree.height = subtree.height.max(counted.height + REFERENCE_LEVELS);
            } else {
                subtree.add(counted);
                subtree.height = subtree.height.max(counted.height + REFERENCE_LEVELS);
            }
            self.within_budget(&subtree)?;
        }
        Ok(subtree)
    }

    /// Refuses a subtree being counted as soon as it is past the step or element budget.
    fn within_budget(&self, subtree: &Subtree) -> Result<(), ExtensionError> {
        if subtree.cost() > self.budget {
            return Err(too_costly(self.budget));
        }
        if subtree.elements > MAX_ELEMENTS {
            return Err(too_many());
        }
        Ok(())
    }

    /// The elements usvg converts in place of `node`'s references, and whether each is converted
    /// once and shared. Every candidate an id could resolve to counts, at one step each.
    fn referenced(
        &self,
        node: Node<'_, '_>,
        here: usize,
        subtree: &mut Subtree,
    ) -> Vec<(NodeId, bool)> {
        let name = node.tag_name().name();
        let mut found = Vec::new();
        let mut add = |id: &str, accept: fn(&str) -> bool, shared: bool, found: &mut Vec<_>| {
            for &target in self.all_ids.get(id).into_iter().flatten() {
                subtree.work = subtree.work.saturating_add(1);
                let Some(element) = node.document().get_node(target) else {
                    continue;
                };
                if accept(element.tag_name().name()) {
                    found.push((target, shared));
                }
            }
        };
        for (property, value) in self.declarations(node) {
            let accept: fn(&str) -> bool = match property {
                Property::Mask => |name| name == "mask",
                Property::ClipPath => |name| name == "clipPath",
                Property::Filter => |name| name == "filter",
                Property::Marker | Property::Fill | Property::Stroke => continue,
            };
            for id in urls(value) {
                add(id, accept, false, &mut found);
            }
        }
        if paints(name) {
            let context = &self.contexts[here];
            for &target in context.fills.iter().chain(&context.strokes) {
                found.push((target, true));
            }
        }
        let href = node
            .attribute((XLINK_NS, "href"))
            .or_else(|| node.attribute("href"))
            .and_then(|href| href.trim().strip_prefix('#'));
        if let Some(id) = href {
            if paint_server(name) {
                // A pattern or gradient takes what it lacks from the one it names.
                add(id, paint_server, true, &mut found);
            } else if name == "feImage" {
                add(id, |_| true, false, &mut found);
            }
        }
        found
    }

    fn path(&mut self, node: Node<'_, '_>) -> PathData {
        if node.tag_name().name() != "path" {
            return PathData::default();
        }
        *self
            .paths
            .entry(node.id())
            .or_insert_with(|| node.attribute("d").map(path_data).unwrap_or_default())
    }

    /// Marker vertices of a shape element; `None` for other elements.
    fn vertices(&mut self, node: Node<'_, '_>) -> Option<u64> {
        match node.tag_name().name() {
            "path" => Some(self.path(node).vertices),
            "polyline" | "polygon" => Some(node.attribute("points").map_or(0, points)),
            "line" => Some(2),
            "rect" | "circle" | "ellipse" => Some(SHAPE_VERTICES),
            _ => None,
        }
    }

    /// Declarations of [`PROPERTIES`] on `node` by attribute, `style`, or the style sheet. Every
    /// value counts, whichever usvg would apply.
    fn declarations<'n>(&'n self, node: Node<'n, '_>) -> Vec<(Property, &'n str)> {
        let mut values: Vec<(Property, &str)> = PROPERTIES
            .iter()
            .filter_map(|(name, property)| Some((*property, node.attribute(*name)?)))
            .collect();
        if let Some(style) = node.attribute("style") {
            values.extend(
                simplecss::DeclarationTokenizer::from(style).filter_map(|declaration| {
                    Some((property(declaration.name)?, declaration.value))
                }),
            );
        }
        if let Some(styled) = self.styled.get(&node.id()) {
            values.extend(
                styled
                    .iter()
                    .map(|(property, value)| (*property, value.as_str())),
            );
        }
        values
    }

    /// The context below `node`: `inherited` plus the markers it names, with the fill and stroke
    /// paints it names in place of the inherited ones.
    fn context(&mut self, inherited: usize, node: Node<'_, '_>) -> usize {
        let declarations = self.declarations(node);
        if declarations.is_empty() {
            return inherited;
        }
        let mut next = self.contexts[inherited].clone();
        let paint = |wanted: Property, inherited: &mut Vec<NodeId>| {
            let mut declared = false;
            let mut inherits = false;
            let mut named = Vec::new();
            for (property, value) in &declarations {
                if *property != wanted {
                    continue;
                }
                declared = true;
                inherits |= value.trim() == "inherit";
                named.extend(urls(value));
            }
            if declared && !inherits {
                inherited.clear();
            }
            named
        };
        let fills = paint(Property::Fill, &mut next.fills);
        let strokes = paint(Property::Stroke, &mut next.strokes);
        let markers: Vec<NodeId> = declarations
            .iter()
            .filter(|(property, _)| *property == Property::Marker)
            .flat_map(|(_, value)| urls(value))
            .filter_map(|id| {
                let marker = node.document().get_node(*self.ids.get(id)?)?;
                (marker.tag_name().name() == "marker").then_some(marker.id())
            })
            .collect();
        let resolve = |ids: Vec<&str>, resolving: &mut u64| -> Vec<NodeId> {
            ids.into_iter()
                .flat_map(|id| self.all_ids.get(id).into_iter().flatten().copied())
                .inspect(|_| *resolving = resolving.saturating_add(1))
                .filter(|target| {
                    node.document()
                        .get_node(*target)
                        .is_some_and(|target| paint_server(target.tag_name().name()))
                })
                .collect()
        };
        let mut resolving = self.resolving;
        next.fills.extend(resolve(fills, &mut resolving));
        next.strokes.extend(resolve(strokes, &mut resolving));
        self.resolving = resolving;
        next.markers.extend(markers);
        for set in [&mut next.markers, &mut next.fills, &mut next.strokes] {
            set.sort_unstable_by_key(|node| node.get_usize());
            set.dedup();
        }
        if next == self.contexts[inherited] {
            return inherited;
        }
        let index = self.contexts.len();
        let index = *self.context_ids.entry(next.clone()).or_insert(index);
        if index == self.contexts.len() {
            self.contexts.push(next);
        }
        index
    }

    /// A marker's or referenced element's content uses its own ancestors' context, not that of
    /// the element that names it.
    fn outside_context(&mut self, element: Node<'_, '_>) -> usize {
        if let Some(&known) = self.outside.get(&element.id()) {
            return known;
        }
        let ancestors: Vec<_> = element
            .ancestors()
            .skip(1)
            .filter(Node::is_element)
            .collect();
        let context = ancestors
            .into_iter()
            .rev()
            .fold(0, |context, ancestor| self.context(context, ancestor));
        self.outside.insert(element.id(), context);
        context
    }

    /// The element a `<use>` copies, resolved as usvg does (`xlink:href` or `href`).
    fn link<'input>(&self, node: Node<'a, 'input>) -> Option<Node<'a, 'input>> {
        let href = node
            .attribute((XLINK_NS, "href"))
            .or_else(|| node.attribute("href"))?;
        let id = href.trim().strip_prefix('#')?.trim_end();
        let link = node.document().get_node(*self.ids.get(id)?)?;
        // A `<use>` of itself or an ancestor is left out.
        if node.ancestors().any(|ancestor| ancestor == link) {
            return None;
        }
        Some(link)
    }
}

impl Subtree {
    fn cost(&self) -> u64 {
        self.depths
            .saturating_mul(DEPTH_STEP)
            .saturating_add(self.work)
    }

    fn add(&mut self, child: Self) {
        self.add_copies(child, 1);
    }

    fn add_copies(&mut self, child: Self, copies: u64) {
        if copies == 0 {
            return;
        }
        self.depths = self.depths.saturating_add(
            child
                .depths
                .saturating_add(child.elements)
                .saturating_mul(copies),
        );
        self.elements = self
            .elements
            .saturating_add(child.elements.saturating_mul(copies));
        self.work = self.work.saturating_add(child.work.saturating_mul(copies));
        self.height = self.height.max(child.height + 1);
    }
}

fn too_deep(limit: usize) -> ExtensionError {
    ExtensionError::Budget(format!(
        "the SVG's elements nest more than {limit} deep with its `use` copies and references in place"
    ))
}

/// Remaining style matching steps.
struct Meter {
    left: std::cell::Cell<u64>,
    /// Whether a selector looked further than [`MAX_SELECTOR_HOPS`].
    deep: std::cell::Cell<bool>,
}

impl Meter {
    /// Returns false once the steps run out.
    fn spend(&self, steps: u64) -> bool {
        let left = self.left.get();
        self.left.set(left.saturating_sub(steps));
        left >= steps
    }
}

/// An element as usvg presents it to simplecss, counting each look. Once out of steps or hops,
/// it reports nothing further so the match ends.
struct Styled<'m, 'a, 'input> {
    node: Node<'a, 'input>,
    /// Distance from the styled element.
    hops: u32,
    meter: &'m Meter,
}

impl<'a, 'input> Styled<'_, 'a, 'input> {
    fn look(&self) -> bool {
        self.meter.spend(1)
    }

    fn next(&self, to: Option<Node<'a, 'input>>) -> Option<Self> {
        let node = to.filter(|_| self.look())?;
        if self.hops >= MAX_SELECTOR_HOPS {
            self.meter.deep.set(true);
            return None;
        }
        Some(Styled {
            node,
            hops: self.hops + 1,
            meter: self.meter,
        })
    }
}

impl simplecss::Element for Styled<'_, '_, '_> {
    fn parent_element(&self) -> Option<Self> {
        self.next(self.node.parent_element())
    }

    fn prev_sibling_element(&self) -> Option<Self> {
        self.next(self.node.prev_sibling_element())
    }

    fn has_local_name(&self, local_name: &str) -> bool {
        self.look() && self.node.tag_name().name() == local_name
    }

    fn attribute_matches(&self, local_name: &str, operator: simplecss::AttributeOperator) -> bool {
        self.look()
            && self
                .node
                .attribute(local_name)
                .is_some_and(|value| operator.matches(value))
    }

    fn pseudo_class_matches(&self, class: simplecss::PseudoClass) -> bool {
        // Only `:first-child` matches a static document, as in usvg.
        self.look()
            && matches!(class, simplecss::PseudoClass::FirstChild)
            && self.node.prev_sibling_element().is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Document<'_> {
        Document::parse(text).expect("parses")
    }

    #[test]
    fn a_use_copy_counts_where_it_is_placed() {
        let doc = parse(
            r##"<svg xmlns="http://www.w3.org/2000/svg"><defs><g id="a"><g><rect/></g></g></defs><use href="#a"/><use href="#a"/></svg>"##,
        );
        let counted = conversion(&doc, u64::MAX, usize::MAX).expect("counted");
        // svg 1, defs 2, g 3, g 4, rect 5; each use 2, its copy 3, 4, 5.
        assert_eq!(counted.depth, 5);
        // Depths below svg: defs 1, g 2, g 3, rect 4; use 1, 2, 3, 4 twice.
        let depths = (1 + 2 + 3 + 4) * 3;
        // Each use checks the 3 elements below and including what it copies.
        assert_eq!(counted.steps, depths * DEPTH_STEP + 3 * 2);
    }

    #[test]
    fn a_use_of_itself_or_an_ancestor_is_left_out() {
        let doc = parse(
            r##"<svg xmlns="http://www.w3.org/2000/svg"><g id="a"><use href="#a"/></g><use id="b" href="#b"/></svg>"##,
        );
        let counted = conversion(&doc, u64::MAX, usize::MAX).expect("counted");
        assert_eq!(counted.depth, 3);
    }

    #[test]
    fn nesting_through_use_copies_past_the_limit_is_refused() {
        let mut text =
            String::from(r#"<svg xmlns="http://www.w3.org/2000/svg"><defs><g id="a0"/>"#);
        text.extend((1..=10).map(|level| {
            format!(
                r##"<g id="a{level}">{}<use href="#a{}"/>{}</g>"##,
                "<g>".repeat(9),
                level - 1,
                "</g>".repeat(9)
            )
        }));
        text.push_str(r##"</defs><use href="#a10"/></svg>"##);
        let doc = parse(&text);
        let depth = conversion(&doc, u64::MAX, usize::MAX)
            .expect("counted")
            .depth;
        assert!(depth > 100, "{depth}");
        assert!(conversion(&doc, u64::MAX, depth).is_ok());
        assert!(conversion(&doc, u64::MAX, depth - 1).is_err());
    }

    #[test]
    fn style_matching_is_counted_as_it_backtracks() {
        let deep = |rule: &str| {
            format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg"><style>{rule}</style>{}<rect/>{}</svg>"#,
                "<g>".repeat(30),
                "</g>".repeat(30)
            )
        };
        let cost = |rule: &str| {
            let text = deep(rule);
            conversion(&parse(&text), u64::MAX, usize::MAX)
                .expect("counted")
                .steps
        };
        let none = cost("");
        let simple = cost("rect{fill:red}");
        let two = cost("x g rect{fill:red}");
        let three = cost("x g g rect{fill:red}");
        assert!(
            none < simple && simple < two && two < three,
            "{none} {simple} {two} {three}"
        );
        // Each added descendant combinator multiplies the backtracking by about the depth.
        assert!(
            three - simple > 10 * (two - simple),
            "{simple} {two} {three}"
        );
        let text = deep("x g g g g g rect{fill:red}");
        assert!(conversion(&parse(&text), 1 << 16, usize::MAX).is_err());
    }

    #[test]
    fn a_marker_counts_at_each_vertex_of_each_shape_that_names_it() {
        let elements = |text: &str| {
            let doc = parse(text);
            let mut count = Count::new(u64::MAX, usize::MAX);
            count.match_styles(&doc).expect("matched");
            count.index(&doc);
            count.subtree(doc.root(), 0, 0).expect("counted").elements
        };
        let svg = |body: &str| {
            format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg"><marker id="m"><rect/><rect/></marker>{body}</svg>"#
            )
        };
        // svg, marker, and its two rects.
        let base = 4;
        // A path of four vertices: the path, and at each vertex the marker and its two rects.
        let path = r#"d="M 0 0 L 1 1 L 2 2 Z""#;
        assert_eq!(elements(&svg(&format!("<path {path}/>"))), base + 1);
        // Each with the elements it adds besides the path: a group, or the `<defs>`, the path in
        // it, the group, and the `<use>` (a `<style>` is not converted).
        for (named, besides) in [
            (format!(r#"<path {path} marker-mid="url(#m)"/>"#), 0),
            (
                format!(r#"<path {path} style="fill:red; marker-end: url( '#m' )"/>"#),
                0,
            ),
            (
                format!("<style>path{{marker:url(#m)}}</style><path {path}/>"),
                0,
            ),
            (
                format!(r#"<g marker-start="url(#m)"><path {path}/></g>"#),
                1,
            ),
            // A copy takes the markers of where it is placed.
            (
                format!(
                    r##"<defs><path id="p" {path}/></defs><g marker-mid="url(#m)"><use href="#p"/></g>"##
                ),
                4,
            ),
        ] {
            assert_eq!(
                elements(&svg(&named)),
                base + 1 + 4 * 3 + besides,
                "{named}"
            );
        }
        // A polyline's points, a line's ends.
        assert_eq!(
            elements(&svg(
                r#"<polyline points="0,0 1,1 2,2" marker-mid="url(#m)"/>"#
            )),
            base + 1 + 3 * 3
        );
        assert_eq!(
            elements(&svg(r#"<line x2="1" marker-mid="url(#m)"/>"#)),
            base + 1 + 2 * 3
        );
        // A marker whose content names itself is not drawn inside itself; another marker is.
        assert_eq!(
            elements(
                r#"<svg xmlns="http://www.w3.org/2000/svg"><marker id="a"><line x2="1" marker-end="url(#a)"/></marker><line x2="1" marker-end="url(#a)"/></svg>"#
            ),
            // svg, marker a, its line; the line, and at its two ends a and its line.
            3 + 1 + 2 * 2
        );
    }

    #[test]
    fn markers_past_the_elements_a_tree_may_hold_are_refused() {
        let text = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg"><marker id="m">{}</marker><path d="M 0 0 {}" marker-mid="url(#m)"/></svg>"#,
            "<rect/>".repeat(100),
            "L 1 1 ".repeat(20_000)
        );
        let error = conversion(&parse(&text), u64::MAX, usize::MAX)
            .err()
            .expect("refused");
        assert!(
            error.to_string().contains("more than 1000000 elements"),
            "{error}"
        );
    }

    #[test]
    fn a_paths_arcs_are_counted_as_the_cubics_they_become() {
        let arc = |end: &str| {
            format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg"><path d="M 0 0 A 1 1 0 0 0 {end} 0"/></svg>"#
            )
        };
        let near = conversion(&parse(&arc("2")), u64::MAX, usize::MAX)
            .expect("counted")
            .steps;
        let far = conversion(&parse(&arc("2e29")), u64::MAX, usize::MAX)
            .expect("counted")
            .steps;
        assert!(near < 100 && far > 1 << 27, "{near} {far}");
    }

    #[test]
    fn a_selector_that_looks_too_far_is_refused() {
        let rule = format!("{}rect{{fill:red}}", "rect + ".repeat(600));
        let text = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg"><style>{rule}</style>{}</svg>"#,
            "<rect/>".repeat(700)
        );
        let error = conversion(&parse(&text), u64::MAX, usize::MAX)
            .err()
            .expect("refused");
        assert!(error.to_string().contains("looks more than 512"), "{error}");
    }

    /// Counts `text` with the full budgets, answering the result and the counter.
    fn counted(text: &str) -> (Result<Subtree, ExtensionError>, usize, usize, usize) {
        let doc = parse(text);
        let mut count = Count::new(super::super::MAX_READING, usize::MAX);
        count.match_styles(&doc).expect("matched");
        count.index(&doc);
        let result = count.subtree(doc.root(), 0, 0);
        (
            result,
            count.subtrees.len(),
            count.descendants.len(),
            count.outside.len(),
        )
    }

    #[test]
    fn many_use_copies_of_a_large_target_are_refused_after_few_visits() {
        let text = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg"><defs><g id="t">{}</g></defs>{}</svg>"#,
            "<rect/>".repeat(100_000),
            r##"<use href="#t"/>"##.repeat(200_000)
        );
        let (result, subtrees, descendants, _) = counted(&text);
        let error = result.err().expect("refused");
        assert!(error.to_string().contains("elements"), "{error}");
        // The target's elements, and the first few copies before the running count is past the
        // budget; its descendants are counted once.
        assert_eq!(descendants, 1);
        assert!(subtrees < 100_100, "{subtrees} subtrees counted");
    }

    #[test]
    fn a_shape_naming_many_markers_is_refused_before_it_counts_them_all() {
        use std::fmt::Write as _;
        let mut text = String::from(r#"<svg xmlns="http://www.w3.org/2000/svg"><style>"#);
        for at in 0..1000 {
            let _ = write!(text, "path{{marker-mid:url(#m{at})}}");
        }
        text.push_str("</style>");
        for at in 0..1000 {
            let _ = write!(text, r#"<marker id="m{at}"><rect/></marker>"#);
        }
        let _ = write!(
            text,
            r#"<defs><path id="p" d="M 0 0 {}"/></defs>{}</svg>"#,
            "L 1 1 ".repeat(1000),
            r##"<use href="#p"/>"##.repeat(1000)
        );
        let (result, _, _, outside) = counted(&text);
        let error = result.err().expect("refused");
        assert!(error.to_string().contains("elements"), "{error}");
        // Each marker costs its two elements at each of the path's 1001 vertices.
        assert!(outside < 1000, "{outside} markers placed");
    }
}

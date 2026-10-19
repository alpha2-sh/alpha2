//! What usvg's conversion of a parsed XML document costs, measured on the
//! XML tree before usvg converts it.
//!
//! usvg 0.47 copies the element each `<use>` names in its place (checking
//! the copy's descendants for a loop first), matches every element with every
//! style sheet rule (backtracking over its ancestors at each descendant
//! combinator: a selector of a few combinators costs a power of the depth),
//! and then resolves each element's properties through its ancestors. It
//! turns each path's arcs into cubics ([`super::path_data`]), and copies a
//! marker's content once at each vertex of every shape that names it, after
//! its own node limit is checked: a 700 kB document of a thousand-element
//! marker on a path of 100,000 vertices took 49 GB before it was stopped.
//! None of that is bounded by its own limits (a million elements, 1,024 deep):
//! measured here, 300,000 small elements 1,000 groups deep take 5.5 s, and a
//! 400-byte document whose one rule has five descendant combinators over 100
//! nested groups does not finish in a minute. So the conversion is counted
//! here, as usvg will do it, and a document that costs more than its share of
//! [`super::MAX_READING`] steps, or nests deeper than its share of
//! [`super::MAX_NESTING`] once its `<use>` copies are in place, is refused
//! before usvg converts it.

use std::collections::HashMap;

use resvg::usvg::roxmltree::{Document, Node, NodeId};

use super::path_data::{PathData, path_data, points};
use crate::api::ExtensionError;

const SVG_NS: &str = "http://www.w3.org/2000/svg";
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";

/// Steps one level of ancestry costs each element usvg converts: it looks up
/// its inherited properties through its ancestors. About 18 ns natively,
/// three times a step of matching a selector.
const DEPTH_STEP: u64 = 3;

/// The most steps a selector's match may move from the element it starts
/// at (each parent and each previous sibling it looks at): simplecss
/// recurses once for each, so a longer chain could overflow the stack. No
/// selector a document meant takes more than a few.
const MAX_SELECTOR_HOPS: u32 = 512;

/// The most elements the converted tree may hold, each `<use>` copy and
/// each marker in place: usvg's own limit on the elements it reads, which
/// markers, copied after it, are not held to.
pub(super) const MAX_ELEMENTS: u64 = 1_000_000;

/// The properties that name a marker.
const MARKERS: [&str; 4] = ["marker", "marker-start", "marker-mid", "marker-end"];

/// The most vertices a shape other than a path, polyline, or polygon has
/// (a rounded rectangle's lines and corner cubics).
const SHAPE_VERTICES: u64 = 16;

/// What converting the document costs, and how deep it nests.
pub(super) struct Conversion {
    /// Steps: each selector test and declaration applied, each element's
    /// ancestry ([`DEPTH_STEP`] a level), and each `<use>`'s loop check.
    pub(super) steps: u64,
    /// The deepest an element of the converted tree is.
    pub(super) depth: usize,
}

/// Counts what usvg's conversion of `doc` costs, refusing a document that
/// costs more than `steps` or nests more than `depth` deep with its `<use>`
/// copies in place.
pub(super) fn conversion(
    doc: &Document,
    steps: u64,
    depth: usize,
) -> Result<Conversion, ExtensionError> {
    let mut count = Count {
        budget: steps,
        depth_limit: depth,
        styles: HashMap::new(),
        styled_markers: HashMap::new(),
        subtrees: HashMap::new(),
        ids: HashMap::new(),
        contexts: vec![Vec::new()],
        context_ids: HashMap::from([(Vec::new(), 0)]),
        paths: HashMap::new(),
    };
    count.match_styles(doc)?;
    // Ids name the first element that carries them, as usvg links them.
    for node in doc.descendants() {
        if let Some(id) = node.attribute("id") {
            count.ids.entry(id).or_insert(node.id());
        }
    }
    let root = count.subtree(doc.root(), 0, 0)?;
    let steps = root.cost();
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
        "the SVG converts to more than {MAX_ELEMENTS} elements with its `use` copies and markers in place"
    ))
}

fn too_costly(budget: u64) -> ExtensionError {
    ExtensionError::Budget(format!(
        "reading the SVG takes more than {budget} steps (its elements, each as deep as it nests, \
         its `use` copies and markers, its paths' arcs, and its style rules matched against each \
         element)"
    ))
}

/// One element's share of the converted tree, wherever it is copied.
#[derive(Clone, Copy, Default)]
struct Subtree {
    /// Its depth sum: each element of it, as deep below it as it is.
    depths: u64,
    /// Its elements, with every `<use>` copy in place.
    elements: u64,
    /// Selector matching, declarations, and `<use>` loop checks for all of
    /// them.
    work: u64,
    /// How many levels deep it reaches, itself included.
    height: usize,
}

struct Count<'a> {
    budget: u64,
    depth_limit: usize,
    /// What matching the style sheet against each element costs.
    styles: HashMap<NodeId, u64>,
    /// The marker properties the style sheet gives each element it sets any
    /// on: every value any matching rule declares.
    styled_markers: HashMap<NodeId, Vec<String>>,
    /// Each element's subtree, below ancestors that name the markers of a
    /// context, once counted; `None` while it is being counted (a `<use>` or
    /// a marker in itself, which usvg leaves out).
    subtrees: HashMap<(NodeId, usize), Option<Subtree>>,
    ids: HashMap<&'a str, NodeId>,
    /// The sets of markers an element and its ancestors name, by index:
    /// what it may draw at its vertices.
    contexts: Vec<Vec<NodeId>>,
    context_ids: HashMap<Vec<NodeId>, usize>,
    /// Each path element's data, once counted.
    paths: HashMap<NodeId, PathData>,
}

impl<'a> Count<'a> {
    /// Matches every rule of the document's style sheets against every
    /// element, as usvg will, counting each selector test, each step a
    /// selector moves, and each declaration a match applies; stops as soon
    /// as the budget is spent.
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
                        if MARKERS.contains(&declaration.name) {
                            self.styled_markers
                                .entry(node.id())
                                .or_default()
                                .push(declaration.value.to_owned());
                        }
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
        // An element's matching counts once for each place usvg converts it,
        // in the subtree walk.
        Ok(())
    }

    /// The share of the converted tree that `node`, `level` levels deep
    /// below ancestors that name the markers of context `inherited`, stands
    /// for.
    fn subtree(
        &mut self,
        node: Node<'a, '_>,
        level: usize,
        inherited: usize,
    ) -> Result<Subtree, ExtensionError> {
        // An element `level` levels below the root element is that many plus one deep.
        if node.is_element() && level >= self.depth_limit {
            return Err(too_deep(self.depth_limit));
        }
        let key = (node.id(), inherited);
        if let Some(known) = self.subtrees.get(&key) {
            let Some(known) = *known else {
                return Ok(Subtree::default());
            };
            if level + known.height > self.depth_limit {
                return Err(too_deep(self.depth_limit));
            }
            return Ok(known);
        }
        self.subtrees.insert(key, None);
        let own = if node.is_element() {
            // usvg keeps SVG elements, or those without a namespace, and none
            // of their style sheets.
            if !matches!(node.tag_name().namespace(), None | Some(SVG_NS))
                || node.tag_name().name() == "style"
            {
                Subtree::default()
            } else {
                let here = self.context(inherited, node);
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
                        let checked = u64::try_from(link.descendants().count()).unwrap_or(u64::MAX);
                        subtree.work = subtree.work.saturating_add(checked);
                        let copy = self.subtree(link, level + 1, here)?;
                        subtree.add(copy);
                    }
                } else {
                    for child in node.children().filter(Node::is_element) {
                        let child = self.subtree(child, level + 1, here)?;
                        subtree.add(child);
                    }
                }
                // A path's arcs, made into cubics.
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
                    // Each marker its ancestors or it names, at each vertex.
                    for marker in self.contexts[here].clone() {
                        let Some(marker) = node.document().get_node(marker) else {
                            continue;
                        };
                        let outside = self.outside_context(marker);
                        let instance = self.subtree(marker, level + 1, outside)?;
                        subtree.add_copies(instance, vertices);
                    }
                }
                subtree
            }
        } else {
            // The document's root: its children are at its own level.
            let mut subtree = Subtree::default();
            for child in node.children().filter(Node::is_element) {
                let child = self.subtree(child, level, inherited)?;
                subtree.depths = subtree.depths.saturating_add(child.depths);
                subtree.elements = subtree.elements.saturating_add(child.elements);
                subtree.work = subtree.work.saturating_add(child.work);
                subtree.height = subtree.height.max(child.height);
            }
            subtree
        };
        if own.cost() > self.budget {
            return Err(too_costly(self.budget));
        }
        if own.elements > MAX_ELEMENTS {
            return Err(too_many());
        }
        self.subtrees.insert(key, Some(own));
        Ok(own)
    }

    /// The data of a path element, once counted (none for another element).
    fn path(&mut self, node: Node<'_, '_>) -> PathData {
        if node.tag_name().name() != "path" {
            return PathData::default();
        }
        *self
            .paths
            .entry(node.id())
            .or_insert_with(|| node.attribute("d").map(path_data).unwrap_or_default())
    }

    /// The vertices of a shape element, where markers are drawn; `None` for
    /// an element that is not a shape.
    fn vertices(&mut self, node: Node<'_, '_>) -> Option<u64> {
        match node.tag_name().name() {
            "path" => Some(self.path(node).vertices),
            "polyline" | "polygon" => Some(node.attribute("points").map_or(0, points)),
            "line" => Some(2),
            "rect" | "circle" | "ellipse" => Some(SHAPE_VERTICES),
            _ => None,
        }
    }

    /// The markers `node` names itself, by any of its marker properties: as
    /// attributes, in its `style`, or through the style sheet. Every value
    /// any of them gives counts, whichever usvg would apply.
    fn own_markers(&self, node: Node<'_, '_>) -> Vec<NodeId> {
        let mut values: Vec<&str> = MARKERS
            .iter()
            .filter_map(|name| node.attribute(*name))
            .collect();
        if let Some(style) = node.attribute("style") {
            values.extend(
                simplecss::DeclarationTokenizer::from(style)
                    .filter(|declaration| MARKERS.contains(&declaration.name))
                    .map(|declaration| declaration.value),
            );
        }
        if let Some(styled) = self.styled_markers.get(&node.id()) {
            values.extend(styled.iter().map(String::as_str));
        }
        values
            .into_iter()
            .filter_map(|value| {
                let id = value
                    .trim()
                    .strip_prefix("url(")?
                    .strip_suffix(')')?
                    .trim()
                    .trim_matches(['"', '\''])
                    .strip_prefix('#')?;
                let marker = node.document().get_node(*self.ids.get(id)?)?;
                (marker.tag_name().name() == "marker").then_some(marker.id())
            })
            .collect()
    }

    /// The context below `node`: the markers of `inherited` and those it
    /// names itself.
    fn context(&mut self, inherited: usize, node: Node<'_, '_>) -> usize {
        let own = self.own_markers(node);
        if own
            .iter()
            .all(|marker| self.contexts[inherited].contains(marker))
        {
            return inherited;
        }
        let mut markers = self.contexts[inherited].clone();
        markers.extend(own);
        markers.sort_unstable_by_key(|marker| marker.get_usize());
        markers.dedup();
        let next = self.contexts.len();
        let index = *self.context_ids.entry(markers.clone()).or_insert(next);
        if index == next {
            self.contexts.push(markers);
        }
        index
    }

    /// The context a marker's content is converted in: the markers its own
    /// ancestors name, not the shape's it is drawn at.
    fn outside_context(&mut self, marker: Node<'_, '_>) -> usize {
        let ancestors: Vec<_> = marker
            .ancestors()
            .skip(1)
            .filter(Node::is_element)
            .collect();
        ancestors
            .into_iter()
            .rev()
            .fold(0, |context, ancestor| self.context(context, ancestor))
    }

    /// The element a `<use>` copies, as usvg resolves its link: by
    /// `xlink:href` or `href`, to the first element with that id.
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
    /// Its steps: its elements' ancestry and its work.
    fn cost(&self) -> u64 {
        self.depths
            .saturating_mul(DEPTH_STEP)
            .saturating_add(self.work)
    }

    /// Takes in a child one level below.
    fn add(&mut self, child: Self) {
        self.add_copies(child, 1);
    }

    /// Takes in `copies` of a child one level below.
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
        "the SVG's elements nest more than {limit} deep with its `use` copies in place"
    ))
}

/// The steps style matching may still take.
struct Meter {
    left: std::cell::Cell<u64>,
    /// Whether a selector looked further than [`MAX_SELECTOR_HOPS`].
    deep: std::cell::Cell<bool>,
}

impl Meter {
    /// Spends `steps`; false once there are none left.
    fn spend(&self, steps: u64) -> bool {
        let left = self.left.get();
        self.left.set(left.saturating_sub(steps));
        left >= steps
    }
}

/// An element as simplecss matches it, the way usvg presents it, with each
/// look at it counted. Once the steps run out or a selector looks too far,
/// it answers as if there were nothing more to see, so the match ends.
struct Styled<'m, 'a, 'input> {
    node: Node<'a, 'input>,
    /// How many elements away from the styled one this is.
    hops: u32,
    meter: &'m Meter,
}

impl<'a, 'input> Styled<'_, 'a, 'input> {
    fn look(&self) -> bool {
        self.meter.spend(1)
    }

    /// `to`, one more element away, unless the match must end.
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
            let mut count = Count {
                budget: u64::MAX,
                depth_limit: usize::MAX,
                styles: HashMap::new(),
                styled_markers: HashMap::new(),
                subtrees: HashMap::new(),
                ids: HashMap::new(),
                contexts: vec![Vec::new()],
                context_ids: HashMap::from([(Vec::new(), 0)]),
                paths: HashMap::new(),
            };
            count.match_styles(&doc).expect("matched");
            for node in doc.descendants() {
                if let Some(id) = node.attribute("id") {
                    count.ids.entry(id).or_insert(node.id());
                }
            }
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
}

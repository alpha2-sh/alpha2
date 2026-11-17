//! Bounds what drawing one page decodes and draws before hayro, which decodes every stream it
//! reads in full and interprets a form or glyph procedure every time it is invoked.

use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::rc::Rc;

use alpha2_model::MAX_SVG_DOCUMENT_BYTES;
use hayro_svg::hayro_syntax::Filter;
use hayro_svg::hayro_syntax::content::TypedIter;
use hayro_svg::hayro_syntax::content::ops::TypedInstruction;
use hayro_svg::hayro_syntax::object::{
    Array, Dict, MaybeRef, Name, Object, ObjectIdentifier, Stream,
};
use hayro_svg::hayro_syntax::page::{Page, Resources};
use hayro_svg::hayro_syntax::xref::XRef;

use super::MAX_IMAGE_DATA_BYTES;
use crate::api::ExtensionError;
use crate::budget::MAX_SOURCE_IMAGE_PIXELS;

/// Most bytes the streams one page reaches may decode to together.
pub(super) const MAX_DECODED_BYTES: u64 = 256 * 1024 * 1024;

/// Most bytes the content streams one page reaches (contents, forms, patterns, glyph procedures,
/// appearances) may decode to together, as much as an SVG document may be.
const MAX_CONTENT_BYTES: u64 = MAX_SVG_DOCUMENT_BYTES as u64;

/// Most elements of markup one page may draw. A draw is about 137 bytes of SVG, so this is what
/// the markup limit would refuse after the conversion.
const MAX_DRAWS: u64 = MAX_SVG_DOCUMENT_BYTES as u64 / 128;

/// Deepest chain of forms, patterns, and glyph procedures counted; hayro stops at 50.
const MAX_INVOCATION_DEPTH: usize = 256;

/// Most objects, direct or not, the walk from one page visits.
const MAX_VISITS: usize = 1 << 21;

/// Keys that lead to the page tree, other pages, actions, structure, or metadata, which drawing
/// the page does not read.
const SKIPPED: [&[u8]; 12] = [
    b"Parent",
    b"P",
    b"Dest",
    b"Dests",
    b"A",
    b"AA",
    b"B",
    b"Popup",
    b"IRT",
    b"StructParent",
    b"Metadata",
    b"Thumb",
];

/// The fonts every PDF reader has, which hayro draws with its bundled substitutes.
const STANDARD_FONTS: [&str; 14] = [
    "Helvetica",
    "Helvetica-Bold",
    "Helvetica-Oblique",
    "Helvetica-BoldOblique",
    "Courier",
    "Courier-Bold",
    "Courier-Oblique",
    "Courier-BoldOblique",
    "Times-Roman",
    "Times-Bold",
    "Times-Italic",
    "Times-BoldItalic",
    "Symbol",
    "ZapfDingbats",
];

/// Keys of a stream's own dictionary that describe its bytes rather than lead elsewhere.
const STREAM_KEYS: [&[u8]; 4] = [b"Length", b"Filter", b"DecodeParms", b"DL"];

/// What a stream is drawn as.
#[derive(Clone, Copy, PartialEq)]
enum Role {
    /// Read but not drawn as content, such as a font program or a function.
    Data,
    /// Operators, which may hold inline images.
    Content,
}

/// What one draw or a content stream's drawing costs the SVG hayro-svg writes.
#[derive(Clone, Copy, Default)]
struct Cost {
    /// Elements of markup: painted paths, clips, glyphs, shadings, images, and form invocations.
    draws: u64,
    /// Image pixels decoded, per draw.
    pixels: u64,
    /// Bytes of `data:` URLs written, per image draw.
    data: u64,
}

impl Cost {
    fn add(self, other: Self) -> Self {
        Self {
            draws: self.draws.saturating_add(other.draws),
            pixels: self.pixels.saturating_add(other.pixels),
            data: self.data.saturating_add(other.data),
        }
    }

    fn times(self, count: u64) -> Self {
        Self {
            draws: self.draws.saturating_mul(count),
            pixels: self.pixels.saturating_mul(count),
            data: self.data.saturating_mul(count),
        }
    }

    fn max(self, other: Self) -> Self {
        Self {
            draws: self.draws.max(other.draws),
            pixels: self.pixels.max(other.pixels),
            data: self.data.max(other.data),
        }
    }

    /// One image draw of `pixels` pixels, written as a PNG of `channels` in base64.
    fn image(pixels: u64, channels: u64) -> Self {
        Self {
            draws: 1,
            pixels,
            data: pixels.saturating_mul(channels).saturating_mul(4) / 3,
        }
    }
}

/// The objects a resource dictionary names, by name. A name may stand for several objects when
/// it comes from every dictionary that defines it.
#[derive(Default)]
struct Names {
    xobjects: HashMap<Vec<u8>, Vec<ObjectIdentifier>>,
    patterns: HashMap<Vec<u8>, Vec<ObjectIdentifier>>,
    /// Per font, the glyph procedures of a Type 3 font; empty for any other font.
    fonts: HashMap<Vec<u8>, Vec<Vec<ObjectIdentifier>>>,
    /// The soft mask groups graphics states set.
    states: HashMap<Vec<u8>, Vec<ObjectIdentifier>>,
}

impl Names {
    fn merge(&mut self, other: &Self) {
        for (name, ids) in &other.xobjects {
            self.xobjects.entry(name.clone()).or_default().extend(ids);
        }
        for (name, ids) in &other.patterns {
            self.patterns.entry(name.clone()).or_default().extend(ids);
        }
        for (name, procedures) in &other.fonts {
            self.fonts
                .entry(name.clone())
                .or_default()
                .extend(procedures.iter().cloned());
        }
        for (name, ids) in &other.states {
            self.states.entry(name.clone()).or_default().extend(ids);
        }
    }
}

/// What a content stream draws, recorded while it is tokenized.
#[derive(Default)]
struct Content {
    /// Its own drawing operators and inline images.
    own: Cost,
    /// `Do` names, with how often each is invoked.
    invoked: HashMap<Vec<u8>, u64>,
    /// Pattern names set as a fill or stroke color.
    patterns: HashMap<Vec<u8>, u64>,
    /// Graphics state names set.
    states: HashMap<Vec<u8>, u64>,
    /// Bytes shown per font name; `None` for text shown before the stream sets a font.
    shown: HashMap<Option<Vec<u8>>, u64>,
    /// Its own resources; without them hayro uses the caller's, which may be any.
    names: Option<Rc<Names>>,
}

/// A content stream, by object, or by its place in the walk when it is direct.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Key {
    Object(ObjectIdentifier),
    Direct(usize),
}

struct Walk<'a> {
    xref: &'a XRef,
    seen: HashSet<ObjectIdentifier>,
    visits: usize,
    decoded: u64,
    /// Bytes the content streams decode to.
    content: u64,
    pixels: u64,
    substituted: Vec<String>,
    contents: HashMap<Key, Content>,
    /// One draw of each image `XObject`.
    images: HashMap<ObjectIdentifier, Cost>,
    /// Names of indirect resource dictionaries, read once.
    resources: HashMap<ObjectIdentifier, Rc<Names>>,
    /// Every name a resource dictionary defines, which a stream without its own may reach.
    all: Names,
    direct: usize,
}

/// Refuses a page whose streams decode past [`MAX_DECODED_BYTES`], whose content streams decode
/// past [`MAX_CONTENT_BYTES`], or whose images decode past [`MAX_SOURCE_IMAGE_PIXELS`], and one
/// that draws more than [`MAX_DRAWS`] elements, image pixels, or [`MAX_IMAGE_DATA_BYTES`] of image
/// data with every invocation of its forms, patterns, and glyph procedures counted, before hayro
/// reads it. Answers the simple fonts it uses that are neither embedded nor one of the standard
/// 14, which hayro substitutes.
///
/// # Errors
/// [`ExtensionError::Budget`].
pub(super) fn check_page(page: &Page<'_>) -> Result<Vec<String>, ExtensionError> {
    let mut walk = Walk {
        xref: page.xref(),
        seen: HashSet::new(),
        visits: 0,
        decoded: 0,
        content: 0,
        pixels: 0,
        substituted: Vec::new(),
        contents: HashMap::new(),
        images: HashMap::new(),
        resources: HashMap::new(),
        all: Names::default(),
        direct: 0,
    };
    let mut stack = Vec::new();
    for (key, value) in page.raw().entries() {
        let role = if key.as_ref() == b"Contents" {
            Role::Content
        } else {
            Role::Data
        };
        if !skipped(&key) {
            stack.push((value, role));
        }
    }
    // A page may inherit its resources, which its own dictionary then does not hold.
    let resources = page.resources();
    for dict in [
        &resources.x_objects,
        &resources.patterns,
        &resources.fonts,
        &resources.ext_g_states,
        &resources.shadings,
        &resources.color_spaces,
    ] {
        stack.push((MaybeRef::NotRef(Object::Dict(dict.clone())), Role::Data));
    }
    while let Some((value, role)) = stack.pop() {
        walk.visits += 1;
        if walk.visits > MAX_VISITS {
            return Err(ExtensionError::Budget(format!(
                "the page reaches more than {MAX_VISITS} objects"
            )));
        }
        let (object, id) = match value {
            MaybeRef::Ref(reference) => {
                let id = ObjectIdentifier::from(reference);
                if !walk.seen.insert(id) {
                    continue;
                }
                match walk.xref.get::<Object<'_>>(id) {
                    Some(object) => (object, Some(id)),
                    None => continue,
                }
            }
            MaybeRef::NotRef(object) => (object, None),
        };
        match object {
            Object::Dict(dict) => {
                if is_named(&dict, b"Type", b"Page") {
                    continue;
                }
                walk.font(&dict);
                walk.own_names(&dict);
                Walk::children(&dict, role, &mut stack, &[]);
            }
            Object::Array(array) => {
                for item in array.raw_iter() {
                    stack.push((item, role));
                }
            }
            Object::Stream(stream) => {
                let dict = stream.dict().clone();
                let role = if role == Role::Content
                    || is_named(&dict, b"Subtype", b"Form")
                    || dict.get::<i32>(b"PatternType") == Some(1)
                {
                    Role::Content
                } else {
                    Role::Data
                };
                walk.stream(&stream, role, id)?;
                Walk::children(&dict, Role::Data, &mut stack, &STREAM_KEYS);
            }
            _ => {}
        }
    }
    walk.page_contents(page);
    walk.check_drawing()?;
    Ok(walk.substituted)
}

fn skipped(key: &Name<'_>) -> bool {
    SKIPPED.contains(&key.as_ref())
}

fn is_named(dict: &Dict<'_>, key: &[u8], name: &[u8]) -> bool {
    dict.get::<Name<'_>>(key)
        .is_some_and(|value| value.as_ref() == name)
}

impl Walk<'_> {
    /// Queues `dict`'s values. Glyph procedures and appearance streams are content.
    fn children<'o>(
        dict: &Dict<'o>,
        role: Role,
        stack: &mut Vec<(MaybeRef<Object<'o>>, Role)>,
        own: &[&[u8]],
    ) {
        for (key, value) in dict.entries() {
            if skipped(&key) || own.contains(&key.as_ref()) {
                continue;
            }
            let role = if matches!(key.as_ref(), b"CharProcs" | b"AP") {
                Role::Content
            } else {
                role
            };
            stack.push((value, role));
        }
    }

    /// Records a simple font that is neither embedded nor standard. Composite fonts are
    /// recorded when hayro asks for their fallback.
    fn font(&mut self, dict: &Dict<'_>) {
        let simple = [b"Type1".as_slice(), b"MMType1", b"TrueType"]
            .iter()
            .any(|subtype| is_named(dict, b"Subtype", subtype));
        if !is_named(dict, b"Type", b"Font") || !simple {
            return;
        }
        let embedded = dict
            .get::<Dict<'_>>(b"FontDescriptor")
            .is_some_and(|descriptor| {
                [b"FontFile".as_slice(), b"FontFile2", b"FontFile3"]
                    .iter()
                    .any(|key| descriptor.contains_key(key))
            });
        let Some(name) = dict.get::<Name<'_>>(b"BaseFont") else {
            return;
        };
        // A subset tag is six capitals and a plus sign.
        let name = name.as_str();
        let name = match name.split_once('+') {
            Some((tag, rest)) if tag.len() == 6 && tag.bytes().all(|b| b.is_ascii_uppercase()) => {
                rest
            }
            _ => name,
        };
        if !embedded
            && !STANDARD_FONTS.contains(&name)
            && !self.substituted.iter().any(|seen| seen == name)
        {
            self.substituted.push(name.to_owned());
        }
    }

    /// Gives the page's content streams the page's resources.
    fn page_contents(&mut self, page: &Page<'_>) {
        let names = Rc::new(self.names_of(page.resources()));
        self.all.merge(&names);
        let contents = match page.raw().get_raw::<Object<'_>>(b"Contents") {
            Some(MaybeRef::Ref(reference)) => vec![ObjectIdentifier::from(reference)],
            Some(MaybeRef::NotRef(Object::Array(array))) => array
                .raw_iter()
                .filter_map(|item| match item {
                    MaybeRef::Ref(reference) => Some(ObjectIdentifier::from(reference)),
                    MaybeRef::NotRef(_) => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        for id in contents {
            if let Some(content) = self.contents.get_mut(&Key::Object(id)) {
                content.names = Some(Rc::clone(&names));
            }
        }
    }

    /// Records the names a dictionary's own resources define, for streams that have none.
    fn own_names(&mut self, dict: &Dict<'_>) {
        self.resources_of(dict);
    }

    /// The names `dict`'s `Resources` define, read once per indirect dictionary and added to
    /// [`Walk::all`] when first read.
    fn resources_of(&mut self, dict: &Dict<'_>) -> Option<Rc<Names>> {
        let resources = dict.get::<Dict<'_>>(b"Resources")?;
        let id = dict.get_ref(b"Resources").map(ObjectIdentifier::from);
        if let Some(known) = id.and_then(|id| self.resources.get(&id)) {
            return Some(Rc::clone(known));
        }
        let names = Rc::new(self.names_of(&Resources::new(resources)));
        self.all.merge(&names);
        if let Some(id) = id {
            self.resources.insert(id, Rc::clone(&names));
        }
        Some(names)
    }

    /// What `read` finds in the dictionary `value` is or refers to.
    fn resolved<T>(&self, value: MaybeRef<Object<'_>>, read: impl Fn(&Dict<'_>) -> T) -> Option<T> {
        match value {
            MaybeRef::Ref(reference) => self
                .xref
                .get::<Dict<'_>>(ObjectIdentifier::from(reference))
                .map(|dict| read(&dict)),
            MaybeRef::NotRef(Object::Dict(dict)) => Some(read(&dict)),
            MaybeRef::NotRef(_) => None,
        }
    }

    /// The objects `resources` names: forms and images, tiling patterns, the glyph procedures of
    /// Type 3 fonts, and the soft mask groups of graphics states.
    fn names_of(&self, resources: &Resources<'_>) -> Names {
        let reference = |value: MaybeRef<Object<'_>>| match value {
            MaybeRef::Ref(reference) => Some(ObjectIdentifier::from(reference)),
            MaybeRef::NotRef(_) => None,
        };
        let mut names = Names::default();
        for (name, value) in resources.x_objects.entries() {
            names
                .xobjects
                .entry(name.as_ref().to_vec())
                .or_default()
                .extend(reference(value));
        }
        for (name, value) in resources.patterns.entries() {
            names
                .patterns
                .entry(name.as_ref().to_vec())
                .or_default()
                .extend(reference(value));
        }
        for (name, value) in resources.fonts.entries() {
            let procedures = self
                .resolved(value, |font| {
                    if !is_named(font, b"Subtype", b"Type3") {
                        return Vec::new();
                    }
                    font.get::<Dict<'_>>(b"CharProcs")
                        .map(|procedures| {
                            procedures
                                .entries()
                                .filter_map(|(_, value)| reference(value))
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            names
                .fonts
                .entry(name.as_ref().to_vec())
                .or_default()
                .push(procedures);
        }
        for (name, value) in resources.ext_g_states.entries() {
            let group = self
                .resolved(value, |state| {
                    state
                        .get::<Dict<'_>>(b"SMask")
                        .and_then(|mask| mask.get_ref(b"G"))
                        .map(ObjectIdentifier::from)
                })
                .flatten();
            names
                .states
                .entry(name.as_ref().to_vec())
                .or_default()
                .extend(group);
        }
        names
    }

    fn stream(
        &mut self,
        stream: &Stream<'_>,
        role: Role,
        id: Option<ObjectIdentifier>,
    ) -> Result<(), ExtensionError> {
        let dict = stream.dict();
        let image = is_named(dict, b"Subtype", b"Image");
        if image {
            let draw = self.image(dict, stream)?;
            if let Some(id) = id {
                self.images.insert(id, draw);
            }
        }
        let keep = role == Role::Content && !image;
        let decoded_left = MAX_DECODED_BYTES - self.decoded;
        let content_left = MAX_CONTENT_BYTES - self.content;
        let left = if keep {
            decoded_left.min(content_left)
        } else {
            decoded_left
        };
        let decoded = decode(stream, left, keep).map_err(|error| {
            if keep && content_left < decoded_left {
                too_much_content()
            } else {
                error
            }
        })?;
        match decoded {
            Decoded::Bytes(bytes) => {
                self.decoded += bytes.len() as u64;
                self.content += bytes.len() as u64;
                let mut content = self.record(&bytes)?;
                content.names = self.resources_of(dict);
                let key = id.map_or_else(
                    || {
                        self.direct += 1;
                        Key::Direct(self.direct)
                    },
                    Key::Object,
                );
                self.contents.insert(key, content);
            }
            Decoded::Length(length) => self.decoded += length,
            Decoded::Image => {}
        }
        Ok(())
    }

    /// Counts an image's pixels and decode memory as [`image_size`] sizes it, and answers what one
    /// draw of it costs.
    fn image(&mut self, dict: &Dict<'_>, stream: &Stream<'_>) -> Result<Cost, ExtensionError> {
        let Sized {
            pixels,
            codec_bytes,
        } = image_size(dict, stream)?;
        self.pixels = self.pixels.saturating_add(pixels);
        if self.pixels > MAX_SOURCE_IMAGE_PIXELS {
            return Err(ExtensionError::Budget(format!(
                "the page's images decode to more than {MAX_SOURCE_IMAGE_PIXELS} pixels"
            )));
        }
        // Decoded samples take at most 8 bytes a pixel (four 16-bit components).
        self.add_decoded(pixels.saturating_mul(8).saturating_add(codec_bytes))?;
        // hayro-svg writes an RGB PNG, with alpha when the image is masked, and a soft mask of
        // another size as a gray image of its own.
        let masked = dict.contains_key(b"SMask")
            || dict.contains_key(b"Mask")
            || dict
                .get::<i32>(b"SMaskInData")
                .is_some_and(|value| value != 0);
        let mut draw = Cost::image(pixels, if masked { 4 } else { 3 });
        if let Some(mask) = dict.get::<Stream<'_>>(b"SMask") {
            draw = draw.add(Cost::image(image_size(mask.dict(), &mask)?.pixels, 1));
        }
        Ok(draw)
    }

    /// What a content stream draws, counting the inline images it holds.
    fn record(&mut self, bytes: &[u8]) -> Result<Content, ExtensionError> {
        fn count(counts: &mut HashMap<Vec<u8>, u64>, name: &Name<'_>) {
            let counted = counts.entry(name.as_ref().to_vec()).or_default();
            *counted = counted.saturating_add(1);
        }
        let mut content = Content::default();
        let mut font: Option<Vec<u8>> = None;
        let mut saved = Vec::new();
        let mut operations = TypedIter::new(bytes);
        while let Some(operation) = operations.next() {
            let shown = match operation {
                TypedInstruction::StrokePath(_)
                | TypedInstruction::CloseAndStrokePath(_)
                | TypedInstruction::FillPathNonZero(_)
                | TypedInstruction::FillPathNonZeroCompatibility(_)
                | TypedInstruction::FillPathEvenOdd(_)
                | TypedInstruction::FillAndStrokeNonZero(_)
                | TypedInstruction::FillAndStrokeEvenOdd(_)
                | TypedInstruction::CloseFillAndStrokeNonZero(_)
                | TypedInstruction::CloseFillAndStrokeEvenOdd(_)
                | TypedInstruction::ClipNonZero(_)
                | TypedInstruction::ClipEvenOdd(_)
                | TypedInstruction::Shading(_) => {
                    content.own.draws = content.own.draws.saturating_add(1);
                    0
                }
                TypedInstruction::InlineImage(inline) => {
                    let stream = inline.0.clone();
                    let draw = self.image(stream.dict(), &stream)?;
                    content.own = content.own.add(draw);
                    let left = MAX_DECODED_BYTES - self.decoded;
                    if let Decoded::Length(length) = decode(&stream, left, false)? {
                        self.add_decoded(length)?;
                    }
                    0
                }
                TypedInstruction::XObject(invoked) => {
                    content.own.draws = content.own.draws.saturating_add(1);
                    count(&mut content.invoked, invoked.0);
                    0
                }
                TypedInstruction::NonStrokeColorNamed(color) => {
                    if let Some(name) = color.1 {
                        count(&mut content.patterns, name);
                    }
                    0
                }
                TypedInstruction::StrokeColorNamed(color) => {
                    if let Some(name) = color.1 {
                        count(&mut content.patterns, name);
                    }
                    0
                }
                TypedInstruction::SetGraphicsState(state) => {
                    count(&mut content.states, state.0);
                    0
                }
                TypedInstruction::TextFont(set) => {
                    font = Some(set.0.as_ref().to_vec());
                    0
                }
                TypedInstruction::SaveState(_) => {
                    saved.push(font.clone());
                    0
                }
                TypedInstruction::RestoreState(_) => {
                    if let Some(previous) = saved.pop() {
                        font = previous;
                    }
                    0
                }
                TypedInstruction::ShowText(text) => text.0.as_bytes().len(),
                TypedInstruction::NextLineAndShowText(text) => text.0.as_bytes().len(),
                TypedInstruction::ShowTextWithParameters(text) => text.2.as_bytes().len(),
                TypedInstruction::ShowTexts(texts) => texts
                    .0
                    .raw_iter()
                    .map(|item| match item {
                        MaybeRef::NotRef(Object::String(text)) => text.as_bytes().len(),
                        _ => 0,
                    })
                    .sum(),
                _ => 0,
            };
            if shown > 0 {
                let counted = content.shown.entry(font.clone()).or_default();
                *counted = counted.saturating_add(shown as u64);
            }
        }
        Ok(content)
    }

    fn add_decoded(&mut self, bytes: u64) -> Result<(), ExtensionError> {
        self.decoded = self.decoded.saturating_add(bytes);
        if self.decoded > MAX_DECODED_BYTES {
            return Err(too_much_decoded());
        }
        Ok(())
    }

    /// Refuses a page that draws past [`MAX_DRAWS`], [`MAX_SOURCE_IMAGE_PIXELS`], or
    /// [`MAX_IMAGE_DATA_BYTES`]. A stream no other stream names (the page's contents, an
    /// annotation's appearance) is drawn once; every other one each time it is named.
    fn check_drawing(&self) -> Result<(), ExtensionError> {
        let mut reached = HashSet::<ObjectIdentifier>::new();
        for content in self.contents.values() {
            let names = content.names.as_deref().unwrap_or(&self.all);
            for name in content.invoked.keys() {
                reached.extend(lookup(&names.xobjects, name).iter().copied());
            }
            for name in content.patterns.keys() {
                reached.extend(lookup(&names.patterns, name).iter().copied());
            }
            for name in content.states.keys() {
                reached.extend(lookup(&names.states, name).iter().copied());
            }
            for font in content.shown.keys() {
                for procedures in fonts(names, font.as_deref()) {
                    reached.extend(procedures.iter().copied());
                }
            }
        }
        let mut counted = HashMap::new();
        let mut total = Cost::default();
        for &key in self.contents.keys() {
            if matches!(key, Key::Object(id) if reached.contains(&id)) {
                continue;
            }
            total = total.add(self.cost(key, 0, &mut counted)?);
            if total.draws > MAX_DRAWS {
                return Err(ExtensionError::Budget(format!(
                    "the page draws more than {MAX_DRAWS} paths, glyphs, images, and form \
                     invocations, each form, pattern, and glyph procedure counted every time it is \
                     drawn"
                )));
            }
        }
        if total.pixels > MAX_SOURCE_IMAGE_PIXELS {
            return Err(ExtensionError::Budget(format!(
                "the page's image draws decode more than {MAX_SOURCE_IMAGE_PIXELS} pixels"
            )));
        }
        if total.data > MAX_IMAGE_DATA_BYTES as u64 {
            return Err(ExtensionError::Budget(format!(
                "the page's image draws write more than {MAX_IMAGE_DATA_BYTES} bytes of image data"
            )));
        }
        Ok(())
    }

    /// What drawing `key` once costs, memoized; a stream that reaches itself is refused.
    fn cost(
        &self,
        key: Key,
        depth: usize,
        counted: &mut HashMap<Key, Option<Cost>>,
    ) -> Result<Cost, ExtensionError> {
        match counted.get(&key) {
            Some(Some(known)) => return Ok(*known),
            Some(None) => {
                return Err(ExtensionError::Budget(
                    "a form, pattern, or glyph procedure of the page draws itself".into(),
                ));
            }
            None => {}
        }
        if depth > MAX_INVOCATION_DEPTH {
            return Err(ExtensionError::Budget(format!(
                "the page nests forms, patterns, and glyph procedures more than \
                 {MAX_INVOCATION_DEPTH} deep"
            )));
        }
        let Some(content) = self.contents.get(&key) else {
            return Ok(Cost::default());
        };
        counted.insert(key, None);
        let names = content.names.as_deref().unwrap_or(&self.all);
        let mut total = content.own;
        let most = |ids: &[ObjectIdentifier],
                    alone: Cost,
                    counted: &mut HashMap<Key, Option<Cost>>|
         -> Result<Cost, ExtensionError> {
            let mut most = Cost::default();
            for id in ids {
                let one = match self.images.get(id) {
                    Some(image) => *image,
                    None if self.contents.contains_key(&Key::Object(*id)) => {
                        self.cost(Key::Object(*id), depth + 1, counted)?
                    }
                    None => alone,
                };
                most = most.max(one);
            }
            Ok(most)
        };
        for (name, times) in &content.invoked {
            let each = most(lookup(&names.xobjects, name), Cost::default(), counted)?;
            total = total.add(each.times(*times));
        }
        // A shading pattern fills like one shading.
        let shading = Cost {
            draws: 1,
            ..Cost::default()
        };
        for (name, times) in &content.patterns {
            let each = most(lookup(&names.patterns, name), shading, counted)?;
            total = total.add(each.times(*times));
        }
        for (name, times) in &content.states {
            let each = most(lookup(&names.states, name), Cost::default(), counted)?;
            total = total.add(each.times(*times));
        }
        for (font, bytes) in &content.shown {
            // A glyph of outlines is one path; a Type 3 glyph draws its costliest procedure.
            let mut glyph = shading;
            for procedures in fonts(names, font.as_deref()) {
                glyph = glyph.max(most(procedures, Cost::default(), counted)?);
            }
            total = total.add(glyph.times(*bytes));
        }
        counted.insert(key, Some(total));
        Ok(total)
    }
}

/// The glyph procedures of each font `font` may name, an empty list for a font of outlines;
/// every font for text shown before a font is set.
fn fonts<'n>(names: &'n Names, font: Option<&[u8]>) -> Vec<&'n [ObjectIdentifier]> {
    match font {
        Some(font) => names
            .fonts
            .get(font)
            .into_iter()
            .flatten()
            .map(Vec::as_slice)
            .collect(),
        None => names.fonts.values().flatten().map(Vec::as_slice).collect(),
    }
}

fn lookup<'n>(
    map: &'n HashMap<Vec<u8>, Vec<ObjectIdentifier>>,
    name: &[u8],
) -> &'n [ObjectIdentifier] {
    map.get(name).map_or(&[], Vec::as_slice)
}

/// What decoding an image allocates: its pixels by the larger of its dictionary and its own coded
/// size, and the bytes its codec allocates for them.
struct Sized {
    pixels: u64,
    /// Codec buffers besides the 8 bytes a pixel of decoded samples.
    codec_bytes: u64,
}

/// Sizes an image the way hayro's decoders do: JPEG and JPEG 2000 from the codestream, JBIG2 from
/// the embedded page, CCITT from `/Columns` and the height or `/Rows`, others from the
/// dictionary. A coded size the codec's own parser cannot read fails decoding the same way, so
/// the dictionary size stands.
///
/// # Errors
/// [`ExtensionError::Budget`] for a CCITT image without a row count, which hayro grows per row.
fn image_size(dict: &Dict<'_>, stream: &Stream<'_>) -> Result<Sized, ExtensionError> {
    let side = |short: &[u8], long: &[u8]| {
        dict.get::<u32>(short)
            .or_else(|| dict.get::<u32>(long))
            .unwrap_or(0)
    };
    let (width, height) = (side(b"W", b"Width"), side(b"H", b"Height"));
    let declared = u64::from(width) * u64::from(height);
    let filters = stream.filters();
    let Some((position, &last)) = filters.iter().enumerate().next_back() else {
        return Ok(Sized {
            pixels: declared,
            codec_bytes: 0,
        });
    };
    let data = || stream.raw_data();
    let area = |width: u32, height: u32| u64::from(width) * u64::from(height);
    let (coded, per_pixel) = match last {
        // A codec after another filter reads decoded bytes, which the scan does not hold here.
        _ if position > 0 => (0, 0),
        Filter::DctDecode => (
            imagesize::blob_size(&data()).map_or(0, |coded| {
                u64::try_from(coded.width.saturating_mul(coded.height)).unwrap_or(u64::MAX)
            }),
            0,
        ),
        Filter::JpxDecode => {
            let settings = hayro_jpeg2000::DecodeSettings {
                resolve_palette_indices: false,
                strict: false,
                target_resolution: None,
            };
            let data = data();
            match hayro_jpeg2000::Image::new(&data, &settings) {
                Ok(image) => {
                    let components = u64::from(image.color_space().num_channels())
                        + u64::from(image.has_alpha());
                    // One 32-bit sample per pixel and component.
                    (area(image.width(), image.height()), 4 * components)
                }
                Err(_) => (0, 0),
            }
        }
        Filter::Jbig2Decode => {
            let data = data();
            match hayro_jbig2::Page::new_embedded(
                &data,
                None,
                &hayro_jbig2::DecodeSettings::default(),
            ) {
                Ok(page) => (area(page.width(), page.height()), 1),
                Err(_) => (0, 0),
            }
        }
        Filter::CcittFaxDecode => {
            let params = decode_params(dict, position);
            let columns = params
                .as_ref()
                .and_then(|params| params.get::<u32>(b"Columns"))
                .unwrap_or(1728);
            let rows = if height > 0 {
                height
            } else {
                params
                    .as_ref()
                    .and_then(|params| params.get::<u32>(b"Rows"))
                    .unwrap_or(0)
            };
            if rows == 0 {
                return Err(ExtensionError::Budget(
                    "the page holds a CCITT image whose row count is not given".into(),
                ));
            }
            (area(columns, rows), 1)
        }
        _ => (0, 0),
    };
    let pixels = declared.max(coded);
    Ok(Sized {
        pixels,
        codec_bytes: coded.saturating_mul(per_pixel),
    })
}

/// The decode parameters of the filter at `position`, from a dictionary or an array of them.
fn decode_params<'o>(dict: &Dict<'o>, position: usize) -> Option<Dict<'o>> {
    for key in [b"DecodeParms".as_slice(), b"DP"] {
        if let Some(params) = dict.get::<Dict<'o>>(key) {
            return Some(params);
        }
        if let Some(params) = dict.get::<Array<'o>>(key) {
            return params.iter::<Dict<'o>>().nth(position);
        }
    }
    None
}

fn too_much_content() -> ExtensionError {
    ExtensionError::Budget(format!(
        "the page's content streams decode to more than {MAX_CONTENT_BYTES} bytes"
    ))
}

fn too_much_decoded() -> ExtensionError {
    ExtensionError::Budget(format!(
        "the page's streams decode to more than {MAX_DECODED_BYTES} bytes"
    ))
}

enum Decoded {
    /// The decoded bytes, kept for content streams.
    Bytes(Vec<u8>),
    /// How many bytes it decodes to.
    Length(u64),
    /// Ends in an image codec, counted by its pixels.
    Image,
}

/// Decodes `stream` through its filters, never past `left` bytes at any step. Predictors only
/// shrink data, so they are not applied. A filter hayro does not read decodes to nothing.
fn decode(stream: &Stream<'_>, left: u64, keep: bool) -> Result<Decoded, ExtensionError> {
    let filters = stream.filters();
    let mut data = stream.raw_data().into_owned();
    let mut filters = filters.as_slice();
    if filters.first() == Some(&Filter::Crypt) {
        filters = &filters[1..];
    }
    let limit = usize::try_from(left).unwrap_or(usize::MAX);
    for (index, filter) in filters.iter().enumerate() {
        let last = index + 1 == filters.len();
        data = match filter {
            Filter::FlateDecode if last && !keep => return inflated_length(&data, left),
            Filter::FlateDecode => inflate(&data, limit)?,
            Filter::AsciiHexDecode => ascii_hex(&data),
            Filter::Ascii85Decode => ascii_85(&data, limit)?,
            Filter::RunLengthDecode => run_length(&data, limit)?,
            Filter::LzwDecode => {
                let early = stream
                    .dict()
                    .get::<Dict<'_>>(b"DecodeParms")
                    .and_then(|params| params.get::<i32>(b"EarlyChange"))
                    .unwrap_or(1);
                lzw(&data, early != 0, limit)?
            }
            Filter::DctDecode
            | Filter::JpxDecode
            | Filter::Jbig2Decode
            | Filter::CcittFaxDecode => return Ok(Decoded::Image),
            Filter::Crypt => Vec::new(),
        };
    }
    if data.len() > limit {
        return Err(too_much_decoded());
    }
    Ok(if keep {
        Decoded::Bytes(data)
    } else {
        Decoded::Length(data.len() as u64)
    })
}

/// Inflates zlib data, or raw deflate when zlib fails as hayro does, and answers the length and,
/// when `keep`, the bytes. Output before a corrupt end counts, since hayro keeps it too, and so
/// does a zlib stream whose checksum is wrong, which hayro's last decoder does not check.
fn inflated(data: &[u8], left: u64, keep: bool) -> Result<(u64, Vec<u8>), ExtensionError> {
    let mut best = (0, Vec::new());
    let after_header = data.get(2..).unwrap_or_default();
    for attempt in 0..3 {
        let mut reader: Box<dyn std::io::Read> = match attempt {
            0 => Box::new(flate2::read::ZlibDecoder::new(data)),
            1 => Box::new(flate2::read::DeflateDecoder::new(data)),
            _ => Box::new(flate2::read::DeflateDecoder::new(after_header)),
        };
        let mut buffer = vec![0; 64 * 1024];
        let mut total = 0_u64;
        let mut kept = Vec::new();
        let finished = loop {
            match reader.read(&mut buffer) {
                Ok(0) => break true,
                Ok(read) => {
                    total += read as u64;
                    if total > left {
                        return Err(too_much_decoded());
                    }
                    if keep {
                        kept.extend_from_slice(&buffer[..read]);
                    }
                }
                Err(_) => break false,
            }
        };
        if total >= best.0 {
            best = (total, kept);
        }
        if finished {
            break;
        }
    }
    Ok(best)
}

fn inflated_length(data: &[u8], left: u64) -> Result<Decoded, ExtensionError> {
    inflated(data, left, false).map(|(length, _)| Decoded::Length(length))
}

fn inflate(data: &[u8], limit: usize) -> Result<Vec<u8>, ExtensionError> {
    inflated(data, limit as u64, true).map(|(_, bytes)| bytes)
}

fn ascii_hex(data: &[u8]) -> Vec<u8> {
    let digits = data
        .iter()
        .take_while(|&&byte| byte != b'>')
        .filter_map(|&byte| char::from(byte).to_digit(16))
        .collect::<Vec<_>>();
    digits
        .chunks(2)
        .map(|pair| u8::try_from(pair[0] * 16 + pair.get(1).copied().unwrap_or(0)).unwrap_or(0))
        .collect()
}

fn ascii_85(data: &[u8], limit: usize) -> Result<Vec<u8>, ExtensionError> {
    let mut output = Vec::new();
    let mut group = Vec::with_capacity(5);
    let flush = |group: &[u32], output: &mut Vec<u8>| {
        let value = (0..5).fold(0_u32, |value, index| {
            value
                .wrapping_mul(85)
                .wrapping_add(group.get(index).copied().unwrap_or(84))
        });
        output.extend_from_slice(&value.to_be_bytes()[..group.len().saturating_sub(1)]);
    };
    for &byte in data {
        match byte {
            b'~' => break,
            b'z' if group.is_empty() => output.extend_from_slice(&[0; 4]),
            b'!'..=b'u' => {
                group.push(u32::from(byte - b'!'));
                if group.len() == 5 {
                    flush(&group, &mut output);
                    group.clear();
                }
            }
            _ => {}
        }
        if output.len() > limit {
            return Err(too_much_decoded());
        }
    }
    if group.len() > 1 {
        flush(&group, &mut output);
    }
    Ok(output)
}

fn run_length(data: &[u8], limit: usize) -> Result<Vec<u8>, ExtensionError> {
    let mut output = Vec::new();
    let mut at = 0;
    while let Some(&length) = data.get(at) {
        match length {
            0..=127 => {
                let end = (at + 2 + usize::from(length)).min(data.len());
                output.extend_from_slice(&data[at + 1..end]);
                at = end;
            }
            128 => break,
            _ => {
                let Some(&byte) = data.get(at + 1) else { break };
                output.resize(output.len() + 257 - usize::from(length), byte);
                at += 2;
            }
        }
        if output.len() > limit {
            return Err(too_much_decoded());
        }
    }
    Ok(output)
}

/// A code past 257 is the previous code's string plus one byte, so each is an output range.
fn lzw(data: &[u8], early_change: bool, limit: usize) -> Result<Vec<u8>, ExtensionError> {
    const CLEAR: usize = 256;
    const END: usize = 257;
    let mut output = Vec::<u8>::new();
    let mut table = Vec::<(usize, usize)>::new();
    let mut previous: Option<(usize, usize)> = None;
    let mut width = 9;
    let (mut bits, mut held) = (0_u32, 0_u32);
    for &byte in data {
        bits = (bits << 8) | u32::from(byte);
        held += 8;
        while held >= width {
            held -= width;
            let code = ((bits >> held) & ((1 << width) - 1)) as usize;
            bits &= (1 << held) - 1;
            if code == CLEAR {
                table.clear();
                previous = None;
                width = 9;
                continue;
            }
            if code == END {
                return Ok(output);
            }
            let start = output.len();
            let entry = if code < CLEAR {
                output.push(u8::try_from(code).unwrap_or(0));
                (start, 1)
            } else if let Some(&(at, length)) = table.get(code - 258) {
                output.extend_from_within(at..at + length);
                (start, length)
            } else if let (Some((at, length)), true) = (previous, code - 258 == table.len()) {
                output.extend_from_within(at..at + length);
                output.push(output[at]);
                (start, length + 1)
            } else {
                return Ok(output);
            };
            if let Some((at, length)) = previous
                && table.len() + 258 < 4096
            {
                table.push((at, length + 1));
            }
            previous = Some(entry);
            let next = table.len() + 258 + usize::from(early_change);
            width = match next {
                ..512 => 9,
                512..1024 => 10,
                1024..2048 => 11,
                _ => 12,
            };
            if output.len() > limit {
                return Err(too_much_decoded());
            }
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::{ascii_85, ascii_hex, lzw, run_length};

    #[test]
    fn the_text_and_run_length_filters_decode_as_the_spec_shows() {
        assert_eq!(ascii_hex(b"48 65 6c6C6\n>99"), b"Hell\x60");
        assert_eq!(
            ascii_85(b"87cURD]i,\"Ebo80~>", 64).expect("decoded"),
            b"Hello World!"
        );
        assert_eq!(ascii_85(b"z!!~>", 64).expect("decoded"), [0, 0, 0, 0, 0]);
        assert_eq!(
            run_length(&[2, b'a', b'b', b'c', 253, b'x', 128], 64).expect("decoded"),
            b"abcxxxx"
        );
        assert!(run_length(&[129, 0, 129, 0], 200).is_err());
    }

    #[test]
    fn lzw_decodes_the_spec_example_within_its_limit() {
        // PDF 32000-1:2008 7.4.4.2: "-----A---B" with EarlyChange 1.
        let coded = [0x80, 0x0b, 0x60, 0x50, 0x22, 0x0c, 0x0c, 0x85, 0x01];
        assert_eq!(lzw(&coded, true, 64).expect("decoded"), b"-----A---B");
        assert!(lzw(&coded, true, 4).is_err());
    }
}

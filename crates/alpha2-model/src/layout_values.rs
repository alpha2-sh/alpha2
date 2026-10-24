//! The CSS value forms a layout reads: lengths, the four-side shorthand,
//! grid track lists and grid lines.
//!
//! A JSON number is a length, a fraction of the picture's shorter side. A
//! string is CSS: `"12px"` is pixels of the project canvas, `"50%"` a
//! percent of the parent's content box where a percent is allowed. Inside
//! these strings keywords use CSS spelling (`min-content`, `auto-fill`);
//! everywhere else a layout's keywords are `snake_case`. Whitespace is any run
//! of ASCII spaces. A string that does not parse is refused by the parser,
//! naming the token and, where one is likely, the spelling meant.
//!
//! Each form serializes canonically (a number, `"<n>px"`, `"<n>%"`, the
//! sides object, a track list as CSS), and a serialized value parses to the
//! same value.

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use std::fmt::{self, Write as _};

/// A CSS number without an exponent: `-?(\d+\.?\d*|\.\d+)`, finite. Read as
/// a JSON number is, through 64 bits, so `"0.1px"` holds the `f32` that
/// `0.1` does.
pub(crate) fn parse_number(text: &str) -> Option<f32> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    let all_digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    if !(all_digits(whole) && all_digits(fraction)) || (whole.is_empty() && fraction.is_empty()) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    let value = text.parse::<f64>().ok()? as f32;
    value.is_finite().then_some(value)
}

/// The tokens of a CSS value: runs of characters between ASCII spaces.
fn tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(' ').filter(|token| !token.is_empty())
}

/// A length's text, as it is written back.
fn write_length(out: &mut impl fmt::Write, value: f32, unit: &str) -> fmt::Result {
    write!(out, "{value}{unit}")
}

/// A layout length that may be a percent: a size or an inset side.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum LayoutLength {
    /// A fraction of the layout picture's shorter side, written as a number.
    Fraction(f32),
    /// Pixels of the project canvas, written `"<n>px"`. A plan turns it into
    /// a fraction of the canvas's shorter side.
    Px(f32),
    /// Percent (0 to 100 is 0% to 100%) of the parent's content box along
    /// the field's axis, written `"<n>%"`.
    Percent(f32),
}

impl Default for LayoutLength {
    fn default() -> Self {
        Self::Fraction(0.0)
    }
}

impl LayoutLength {
    /// The number, whatever its unit.
    #[must_use]
    pub const fn value(self) -> f32 {
        match self {
            Self::Fraction(value) | Self::Px(value) | Self::Percent(value) => value,
        }
    }

    /// Whether it is a percent.
    #[must_use]
    pub const fn is_percent(self) -> bool {
        matches!(self, Self::Percent(_))
    }

    /// The same length with pixels turned into a fraction of a picture
    /// `shorter_side` pixels on its shorter side.
    #[must_use]
    pub fn without_px(self, shorter_side: f32) -> Self {
        match self {
            Self::Px(value) => Self::Fraction(px_fraction(value, shorter_side)),
            other => other,
        }
    }

    /// The same kind of length with `value`.
    #[must_use]
    pub const fn with_value(self, value: f32) -> Self {
        match self {
            Self::Fraction(_) => Self::Fraction(value),
            Self::Px(_) => Self::Px(value),
            Self::Percent(_) => Self::Percent(value),
        }
    }

    fn parse(text: &str) -> Result<Self, String> {
        let size = |value: LayoutSize| match value {
            LayoutSize::Fraction(value) => Self::Fraction(value),
            LayoutSize::Px(value) => Self::Px(value),
        };
        match text.trim_matches(' ').strip_suffix('%') {
            Some(number) => parse_number(number)
                .map(Self::Percent)
                .ok_or_else(|| invalid_length(text, true)),
            None => LayoutSize::parse_token(text.trim_matches(' '), true)
                .map(size)
                .map_err(|_| invalid_length(text, true)),
        }
    }
}

/// `n` canvas pixels as a fraction of a shorter side of `shorter_side`
/// pixels.
#[allow(clippy::cast_possible_truncation)]
fn px_fraction(value: f32, shorter_side: f32) -> f32 {
    (f64::from(value) / f64::from(shorter_side)) as f32
}

impl fmt::Display for LayoutLength {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Fraction(value) => write!(formatter, "{value}"),
            Self::Px(value) => write_length(formatter, value, "px"),
            Self::Percent(value) => write_length(formatter, value, "%"),
        }
    }
}

/// A layout length that may not be a percent: a gap, a radius, a border
/// width, a font size, a padding or margin side, a translation.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum LayoutSize {
    /// A fraction of the layout picture's shorter side, written as a number.
    Fraction(f32),
    /// Pixels of the project canvas, written `"<n>px"`.
    Px(f32),
}

impl Default for LayoutSize {
    fn default() -> Self {
        Self::Fraction(0.0)
    }
}

impl LayoutSize {
    /// The number, whatever its unit.
    #[must_use]
    pub const fn value(self) -> f32 {
        match self {
            Self::Fraction(value) | Self::Px(value) => value,
        }
    }

    /// The same size with pixels turned into a fraction of a picture
    /// `shorter_side` pixels on its shorter side.
    #[must_use]
    pub fn without_px(self, shorter_side: f32) -> Self {
        match self {
            Self::Px(value) => Self::Fraction(px_fraction(value, shorter_side)),
            other @ Self::Fraction(_) => other,
        }
    }

    /// Its fraction of a picture `shorter_side` pixels on its shorter side.
    #[must_use]
    pub fn fraction(self, shorter_side: f32) -> f32 {
        self.without_px(shorter_side).value()
    }

    /// The same kind of size with `value`.
    #[must_use]
    pub const fn with_value(self, value: f32) -> Self {
        match self {
            Self::Fraction(_) => Self::Fraction(value),
            Self::Px(_) => Self::Px(value),
        }
    }

    /// One token: a number or `<n>px`. `percent` says whether the field takes
    /// a percent, for the message.
    fn parse_token(token: &str, percent: bool) -> Result<Self, String> {
        if let Some(number) = token.strip_suffix("px") {
            return parse_number(number)
                .map(Self::Px)
                .ok_or_else(|| invalid_length(token, percent));
        }
        if !percent && token.ends_with('%') && parse_number(&token[..token.len() - 1]).is_some() {
            return Err(format!("percent is not allowed here: \"{token}\""));
        }
        parse_number(token)
            .map(Self::Fraction)
            .ok_or_else(|| invalid_length(token, percent))
    }

    fn parse(text: &str) -> Result<Self, String> {
        Self::parse_token(text.trim_matches(' '), false)
    }
}

impl fmt::Display for LayoutSize {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Fraction(value) => write!(formatter, "{value}"),
            Self::Px(value) => write_length(formatter, value, "px"),
        }
    }
}

fn invalid_length(token: &str, percent: bool) -> String {
    let hint = if token.contains(' ') {
        format!(" (write {})", token.replace(' ', ""))
    } else if token.ends_with("em") {
        " (em and rem are not supported: write px or a number)".to_owned()
    } else {
        String::new()
    };
    let forms = if percent {
        "a number, \"<n>px\", or \"<n>%\""
    } else {
        "a number or \"<n>px\""
    };
    format!("invalid length \"{token}\"{hint}: write {forms}")
}

/// A visitor for a value written as a number or a string.
struct NumberOrString<T> {
    expecting: &'static str,
    number: fn(f32) -> T,
    string: fn(&str) -> Result<T, String>,
}

impl<T> Visitor<'_> for NumberOrString<T> {
    type Value = T;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.expecting)
    }

    #[allow(clippy::cast_possible_truncation)]
    fn visit_f64<E: de::Error>(self, value: f64) -> Result<T, E> {
        Ok((self.number)(value as f32))
    }

    #[allow(clippy::cast_precision_loss)]
    fn visit_u64<E: de::Error>(self, value: u64) -> Result<T, E> {
        Ok((self.number)(value as f32))
    }

    #[allow(clippy::cast_precision_loss)]
    fn visit_i64<E: de::Error>(self, value: i64) -> Result<T, E> {
        Ok((self.number)(value as f32))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<T, E> {
        (self.string)(value).map_err(E::custom)
    }
}

impl<'de> Deserialize<'de> for LayoutLength {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(NumberOrString {
            expecting: "a length: a number, \"<n>px\", or \"<n>%\"",
            number: Self::Fraction,
            string: Self::parse,
        })
    }
}

impl Serialize for LayoutLength {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Fraction(value) => serializer.serialize_f32(*value),
            other => serializer.collect_str(other),
        }
    }
}

impl<'de> Deserialize<'de> for LayoutSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(NumberOrString {
            expecting: "a length: a number or \"<n>px\"",
            number: Self::Fraction,
            string: Self::parse,
        })
    }
}

impl Serialize for LayoutSize {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Fraction(value) => serializer.serialize_f32(*value),
            other @ Self::Px(_) => serializer.collect_str(other),
        }
    }
}

/// A length on each side of a box (a padding or a margin). Missing sides
/// are 0.
///
/// Written as one length for every side, as the CSS shorthand (`"8px 16px"`:
/// one to four lengths, top first, clockwise, the missing ones repeating the
/// opposite side), or as the sides object.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Insets {
    /// Top side.
    pub top: LayoutSize,
    /// Right side.
    pub right: LayoutSize,
    /// Bottom side.
    pub bottom: LayoutSize,
    /// Left side.
    pub left: LayoutSize,
}

impl Insets {
    /// The sides, top first, clockwise.
    #[must_use]
    pub const fn sides(self) -> [LayoutSize; 4] {
        [self.top, self.right, self.bottom, self.left]
    }

    /// Insets from sides, top first, clockwise.
    #[must_use]
    pub const fn from_sides([top, right, bottom, left]: [LayoutSize; 4]) -> Self {
        Self {
            top,
            right,
            bottom,
            left,
        }
    }

    /// The same insets with pixels turned into fractions.
    #[must_use]
    pub fn without_px(self, shorter_side: f32) -> Self {
        Self::from_sides(self.sides().map(|side| side.without_px(shorter_side)))
    }

    fn parse(text: &str) -> Result<Self, String> {
        let sides = tokens(text)
            .map(|token| LayoutSize::parse_token(token, false))
            .collect::<Result<Vec<_>, _>>()?;
        let [top, right, bottom, left] = shorthand(&sides)?;
        Ok(Self {
            top,
            right,
            bottom,
            left,
        })
    }
}

/// The four sides of a CSS shorthand of one to four values.
fn shorthand<T: Copy>(values: &[T]) -> Result<[T; 4], String> {
    match *values {
        [all] => Ok([all; 4]),
        [vertical, horizontal] => Ok([vertical, horizontal, vertical, horizontal]),
        [top, horizontal, bottom] => Ok([top, horizontal, bottom, horizontal]),
        [top, right, bottom, left] => Ok([top, right, bottom, left]),
        _ => Err(format!("expected 1 to 4 lengths, got {}", values.len())),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InsetsObject {
    #[serde(default)]
    top: LayoutSize,
    #[serde(default)]
    right: LayoutSize,
    #[serde(default)]
    bottom: LayoutSize,
    #[serde(default)]
    left: LayoutSize,
}

struct InsetsVisitor;

impl<'de> Visitor<'de> for InsetsVisitor {
    type Value = Insets;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("insets: a length, one to four lengths, or the sides object")
    }

    #[allow(clippy::cast_possible_truncation)]
    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Insets, E> {
        Ok(Insets::from_sides([LayoutSize::Fraction(value as f32); 4]))
    }

    #[allow(clippy::cast_precision_loss)]
    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Insets, E> {
        Ok(Insets::from_sides([LayoutSize::Fraction(value as f32); 4]))
    }

    #[allow(clippy::cast_precision_loss)]
    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Insets, E> {
        Ok(Insets::from_sides([LayoutSize::Fraction(value as f32); 4]))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Insets, E> {
        Insets::parse(value).map_err(E::custom)
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Insets, A::Error> {
        let sides = InsetsObject::deserialize(de::value::MapAccessDeserializer::new(map))?;
        Ok(Insets {
            top: sides.top,
            right: sides.right,
            bottom: sides.bottom,
            left: sides.left,
        })
    }
}

impl<'de> Deserialize<'de> for Insets {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(InsetsVisitor)
    }
}

impl Serialize for Insets {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sides = serializer.serialize_struct("Insets", 4)?;
        sides.serialize_field("top", &self.top)?;
        sides.serialize_field("right", &self.right)?;
        sides.serialize_field("bottom", &self.bottom)?;
        sides.serialize_field("left", &self.left)?;
        sides.end()
    }
}

/// A box's offset after layout: `x` right, `y` down.
///
/// Written as the object, or as the CSS `translate` value: one or two
/// lengths, `y` 0 when missing.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct LayoutOffset {
    /// Rightward.
    pub x: LayoutSize,
    /// Downward.
    pub y: LayoutSize,
}

impl LayoutOffset {
    /// The same offset with pixels turned into fractions.
    #[must_use]
    pub fn without_px(self, shorter_side: f32) -> Self {
        Self {
            x: self.x.without_px(shorter_side),
            y: self.y.without_px(shorter_side),
        }
    }

    fn parse(text: &str) -> Result<Self, String> {
        let values = tokens(text)
            .map(|token| LayoutSize::parse_token(token, false))
            .collect::<Result<Vec<_>, _>>()?;
        match *values {
            [x] => Ok(Self {
                x,
                y: LayoutSize::default(),
            }),
            [x, y] => Ok(Self { x, y }),
            _ => Err(format!("expected 1 or 2 lengths, got {}", values.len())),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OffsetObject {
    x: LayoutSize,
    y: LayoutSize,
}

struct OffsetVisitor;

impl<'de> Visitor<'de> for OffsetVisitor {
    type Value = LayoutOffset;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an offset: one or two lengths, or {x, y}")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<LayoutOffset, E> {
        LayoutOffset::parse(value).map_err(E::custom)
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<LayoutOffset, A::Error> {
        let OffsetObject { x, y } =
            OffsetObject::deserialize(de::value::MapAccessDeserializer::new(map))?;
        Ok(LayoutOffset { x, y })
    }
}

impl<'de> Deserialize<'de> for LayoutOffset {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(OffsetVisitor)
    }
}

impl Serialize for LayoutOffset {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut offset = serializer.serialize_struct("LayoutOffset", 2)?;
        offset.serialize_field("x", &self.x)?;
        offset.serialize_field("y", &self.y)?;
        offset.end()
    }
}

/// How large a grid track may be at least or at most: one side of
/// `minmax()`.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum TrackBreadth {
    /// A length or a percent of the grid's content box.
    Length(LayoutLength),
    /// A share of the free space, `<n>fr`; only as the largest size.
    Fr(f32),
    /// `auto`.
    Auto,
    /// `min-content`.
    MinContent,
    /// `max-content`.
    MaxContent,
}

impl TrackBreadth {
    const fn is_fixed(self) -> bool {
        matches!(self, Self::Length(_))
    }

    fn without_px(self, shorter_side: f32) -> Self {
        match self {
            Self::Length(length) => Self::Length(length.without_px(shorter_side)),
            other => other,
        }
    }

    fn parse(token: &str, max: bool) -> Result<Self, String> {
        match token {
            "auto" => Ok(Self::Auto),
            "min-content" => Ok(Self::MinContent),
            "max-content" => Ok(Self::MaxContent),
            _ => {
                if let Some(share) = token.strip_suffix("fr").and_then(parse_number) {
                    return if max {
                        Ok(Self::Fr(share))
                    } else {
                        Err(format!("a minimum size cannot be fr: \"{token}\""))
                    };
                }
                LayoutLength::parse(token)
                    .map(Self::Length)
                    .map_err(|_| unknown_track(token))
            }
        }
    }
}

impl fmt::Display for TrackBreadth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length(length) => write!(formatter, "{length}"),
            Self::Fr(share) => write_length(formatter, *share, "fr"),
            Self::Auto => formatter.write_str("auto"),
            Self::MinContent => formatter.write_str("min-content"),
            Self::MaxContent => formatter.write_str("max-content"),
        }
    }
}

/// The size of one grid track.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum TrackSize {
    /// One breadth, as CSS writes it: a length is fixed, `<n>fr` is
    /// `minmax(auto, <n>fr)`, a keyword sizes by content.
    Breadth(TrackBreadth),
    /// `minmax(min, max)`.
    MinMax(TrackBreadth, TrackBreadth),
    /// `fit-content(limit)`.
    FitContent(LayoutLength),
}

impl TrackSize {
    /// Whether it is a CSS fixed size, as `auto-fill` and `auto-fit` need:
    /// a length, or `minmax()` with a length on one side and no flexible
    /// or content minimum.
    #[must_use]
    pub const fn is_fixed(self) -> bool {
        match self {
            Self::Breadth(breadth) => breadth.is_fixed(),
            Self::MinMax(min, max) => {
                min.is_fixed() || (max.is_fixed() && !matches!(min, TrackBreadth::Fr(_)))
            }
            Self::FitContent(_) => false,
        }
    }

    fn without_px(self, shorter_side: f32) -> Self {
        match self {
            Self::Breadth(breadth) => Self::Breadth(breadth.without_px(shorter_side)),
            Self::MinMax(min, max) => {
                Self::MinMax(min.without_px(shorter_side), max.without_px(shorter_side))
            }
            Self::FitContent(limit) => Self::FitContent(limit.without_px(shorter_side)),
        }
    }

    fn lengths(self) -> impl Iterator<Item = LayoutLength> {
        let breadth = |breadth: TrackBreadth| match breadth {
            TrackBreadth::Length(length) => Some(length),
            _ => None,
        };
        let (first, second) = match self {
            Self::Breadth(single) => (breadth(single), None),
            Self::MinMax(min, max) => (breadth(min), breadth(max)),
            Self::FitContent(limit) => (Some(limit), None),
        };
        first.into_iter().chain(second)
    }

    fn fr(self) -> Option<f32> {
        match self {
            Self::Breadth(TrackBreadth::Fr(share)) | Self::MinMax(_, TrackBreadth::Fr(share)) => {
                Some(share)
            }
            _ => None,
        }
    }
}

impl fmt::Display for TrackSize {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Breadth(breadth) => write!(formatter, "{breadth}"),
            Self::MinMax(min, max) => write!(formatter, "minmax({min}, {max})"),
            Self::FitContent(limit) => write!(formatter, "fit-content({limit})"),
        }
    }
}

/// How often `repeat()` repeats its tracks.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RepeatCount {
    /// That many times.
    Count(u16),
    /// `auto-fill`: as many as fit.
    AutoFill,
    /// `auto-fit`: as many as fit, empty ones collapsed.
    AutoFit,
}

impl fmt::Display for RepeatCount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Count(count) => write!(formatter, "{count}"),
            Self::AutoFill => formatter.write_str("auto-fill"),
            Self::AutoFit => formatter.write_str("auto-fit"),
        }
    }
}

/// One entry of a track list: a track, or `repeat()` of some.
#[derive(Clone, Debug, PartialEq)]
pub enum TrackComponent {
    /// One track.
    Single(TrackSize),
    /// `repeat(count, tracks)`.
    Repeat(RepeatCount, Vec<TrackSize>),
}

/// A grid track list, as `grid_template_columns` writes it in CSS:
/// `"1fr 2fr 120px"`, `"repeat(3, minmax(100px, 1fr))"`,
/// `"repeat(auto-fill, 160px)"`. Named lines are not supported.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackList(pub Vec<TrackComponent>);

impl TrackList {
    /// How many explicit tracks it has with each count repeated (an
    /// automatic repeat counts once).
    #[must_use]
    pub fn track_count(&self) -> usize {
        self.0
            .iter()
            .map(|component| match component {
                TrackComponent::Single(_) => 1,
                TrackComponent::Repeat(RepeatCount::Count(count), tracks) => {
                    usize::from(*count) * tracks.len()
                }
                TrackComponent::Repeat(_, tracks) => tracks.len(),
            })
            .sum()
    }

    /// Every track size, once each, repeats included once.
    pub fn sizes(&self) -> impl Iterator<Item = TrackSize> + '_ {
        self.0.iter().flat_map(|component| match component {
            TrackComponent::Single(size) => std::slice::from_ref(size).iter().copied(),
            TrackComponent::Repeat(_, tracks) => tracks.iter().copied(),
        })
    }

    /// Every length in it, for range checks.
    pub fn lengths(&self) -> impl Iterator<Item = LayoutLength> + '_ {
        self.sizes().flat_map(TrackSize::lengths)
    }

    /// Every `fr` share in it.
    pub fn shares(&self) -> impl Iterator<Item = f32> + '_ {
        self.sizes().filter_map(TrackSize::fr)
    }

    /// The same list with pixels turned into fractions.
    #[must_use]
    pub fn without_px(&self, shorter_side: f32) -> Self {
        Self(
            self.0
                .iter()
                .map(|component| match component {
                    TrackComponent::Single(size) => {
                        TrackComponent::Single(size.without_px(shorter_side))
                    }
                    TrackComponent::Repeat(count, tracks) => TrackComponent::Repeat(
                        *count,
                        tracks
                            .iter()
                            .map(|size| size.without_px(shorter_side))
                            .collect(),
                    ),
                })
                .collect(),
        )
    }

    fn parse(text: &str) -> Result<Self, String> {
        let mut parser = TrackParser { text, at: 0 };
        let mut components = Vec::new();
        while parser.skip_spaces() {
            if parser.rest().starts_with("repeat(") {
                parser.at += "repeat(".len();
                components.push(parser.repeat()?);
            } else {
                components.push(TrackComponent::Single(parser.track()?));
            }
        }
        if components.is_empty() {
            return Err("expected at least one track".to_owned());
        }
        Ok(Self(components))
    }
}

impl fmt::Display for TrackList {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, component) in self.0.iter().enumerate() {
            if index > 0 {
                formatter.write_char(' ')?;
            }
            match component {
                TrackComponent::Single(size) => write!(formatter, "{size}")?,
                TrackComponent::Repeat(count, tracks) => {
                    write!(formatter, "repeat({count},")?;
                    for size in tracks {
                        write!(formatter, " {size}")?;
                    }
                    formatter.write_char(')')?;
                }
            }
        }
        Ok(())
    }
}

/// The message for a track that is not one, with the spelling likely meant.
fn unknown_track(token: &str) -> String {
    let hint = match token {
        "min_content" => Some("min-content".to_owned()),
        "max_content" => Some("max-content".to_owned()),
        "auto_fill" => Some("auto-fill".to_owned()),
        "auto_fit" => Some("auto-fit".to_owned()),
        _ if token.starts_with('[') => {
            return format!("named grid lines are not supported: \"{token}\"");
        }
        _ if token.contains(' ') => Some(token.replace(' ', "")),
        _ => None,
    };
    match hint {
        Some(hint) => format!("unknown track \"{token}\" (write {hint})"),
        None => format!("unknown track \"{token}\""),
    }
}

/// Reads a track list left to right.
struct TrackParser<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> TrackParser<'a> {
    fn rest(&self) -> &'a str {
        &self.text[self.at..]
    }

    /// Skips spaces; whether anything is left.
    fn skip_spaces(&mut self) -> bool {
        while self.rest().starts_with(' ') {
            self.at += 1;
        }
        !self.rest().is_empty()
    }

    /// The next token: up to a space, a comma or a parenthesis.
    fn token(&mut self) -> &'a str {
        let rest = &self.text[self.at..];
        let end = rest.find([' ', ',', '(', ')']).unwrap_or(rest.len());
        self.at += end;
        &rest[..end]
    }

    /// `expected` after optional spaces.
    fn expect(&mut self, expected: char) -> Result<(), String> {
        self.skip_spaces();
        if self.rest().starts_with(expected) {
            self.at += 1;
            Ok(())
        } else {
            Err(format!("expected \"{expected}\" in \"{}\"", self.text))
        }
    }

    fn track(&mut self) -> Result<TrackSize, String> {
        let start = self.at;
        let token = self.token();
        match token {
            "minmax" | "fit-content" if self.rest().starts_with('(') => {
                self.at += 1;
                if token == "fit-content" {
                    let limit = self.breadth(false)?;
                    self.expect(')')?;
                    return match limit {
                        TrackBreadth::Length(limit) => Ok(TrackSize::FitContent(limit)),
                        _ => Err(format!(
                            "fit-content takes a length: \"{}\"",
                            &self.text[start..self.at]
                        )),
                    };
                }
                let min = self.breadth(false)?;
                self.expect(',')?;
                let max = self.breadth(true)?;
                self.expect(')')?;
                Ok(TrackSize::MinMax(min, max))
            }
            "" => Err(format!("expected a track in \"{}\"", self.text)),
            "fit_content" | "min_max" => Err(format!(
                "unknown track \"{token}\" (write {})",
                token.replace('_', "-").replace("min-max", "minmax")
            )),
            _ => {
                // A unit standing apart from its number: "1 fr".
                let after = self.at;
                if parse_number(token).is_some() && self.skip_spaces() {
                    let mark = self.at;
                    let unit = self.token();
                    if matches!(unit, "fr" | "px" | "%") {
                        return Err(unknown_track(&self.text[start..self.at]));
                    }
                    self.at = mark;
                }
                self.at = after;
                TrackBreadth::parse(token, true).map(TrackSize::Breadth)
            }
        }
    }

    fn breadth(&mut self, max: bool) -> Result<TrackBreadth, String> {
        self.skip_spaces();
        let token = self.token();
        if token.is_empty() {
            return Err(format!("expected a track size in \"{}\"", self.text));
        }
        TrackBreadth::parse(token, max)
    }

    fn repeat(&mut self) -> Result<TrackComponent, String> {
        self.skip_spaces();
        let token = self.token();
        let count = match token {
            "auto-fill" => RepeatCount::AutoFill,
            "auto-fit" => RepeatCount::AutoFit,
            "auto_fill" | "auto_fit" => {
                return Err(format!(
                    "unknown repeat count \"{token}\" (write {})",
                    token.replace('_', "-")
                ));
            }
            _ => token
                .parse::<u16>()
                .ok()
                .filter(|_| token.bytes().all(|byte| byte.is_ascii_digit()))
                .map(RepeatCount::Count)
                .ok_or_else(|| format!("unknown repeat count \"{token}\""))?,
        };
        self.expect(',')?;
        let mut tracks = Vec::new();
        loop {
            if !self.skip_spaces() {
                return Err(format!("expected \")\" in \"{}\"", self.text));
            }
            if self.rest().starts_with(')') {
                self.at += 1;
                break;
            }
            if self.rest().starts_with("repeat(") {
                return Err("repeat() cannot hold repeat()".to_owned());
            }
            tracks.push(self.track()?);
        }
        if tracks.is_empty() {
            return Err("repeat() needs at least one track".to_owned());
        }
        Ok(TrackComponent::Repeat(count, tracks))
    }
}

impl<'de> Deserialize<'de> for TrackList {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(de::Error::custom)
    }
}

impl Serialize for TrackList {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// One edge of a grid item's placement.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum GridEdge {
    /// Placed automatically.
    #[default]
    Auto,
    /// At a line: 1 is the first, -1 the last explicit line.
    Line(i16),
    /// So many tracks from the other edge.
    Span(u16),
}

impl GridEdge {
    fn parse(text: &str) -> Result<Self, String> {
        let words = tokens(text).collect::<Vec<_>>();
        let integer = |word: &str| {
            let digits = word.strip_prefix('-').unwrap_or(word);
            (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| word.parse::<i16>().ok())
                .flatten()
        };
        match *words {
            ["auto"] => Ok(Self::Auto),
            ["span", count] => integer(count)
                .and_then(|count| u16::try_from(count).ok())
                .map(Self::Span)
                .ok_or_else(|| invalid_line(text)),
            [line] => integer(line)
                .map(Self::Line)
                .ok_or_else(|| invalid_line(text)),
            _ => Err(invalid_line(text)),
        }
    }
}

fn invalid_line(text: &str) -> String {
    let text = text.trim_matches(' ');
    let named = text
        .split([' ', '/'])
        .filter(|word| !word.is_empty())
        .any(|word| {
            word != "span"
                && word != "auto"
                && word
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_alphabetic())
        });
    if named {
        format!("named grid lines are not supported: \"{text}\"")
    } else {
        format!("invalid grid line \"{text}\": write a line number, \"span <n>\", or \"auto\"")
    }
}

impl fmt::Display for GridEdge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => formatter.write_str("auto"),
            Self::Line(line) => write!(formatter, "{line}"),
            Self::Span(count) => write!(formatter, "span {count}"),
        }
    }
}

/// Where a grid item lies on one axis, as `grid_column` writes it in CSS:
/// `2`, `"1 / 3"`, `"span 2"`, `"2 / span 3"`, `"auto"`. A missing end is
/// `auto`.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct GridPlacement {
    /// The start edge.
    pub start: GridEdge,
    /// The end edge.
    pub end: GridEdge,
}

impl GridPlacement {
    /// Every edge of it.
    #[must_use]
    pub const fn edges(self) -> [GridEdge; 2] {
        [self.start, self.end]
    }

    fn parse(text: &str) -> Result<Self, String> {
        let (start, end) = match text.split_once('/') {
            Some((start, end)) => (start, Some(end)),
            None => (text, None),
        };
        if end.is_some_and(|end| end.contains('/')) {
            return Err(invalid_line(text));
        }
        let edge = |text| GridEdge::parse(text).map_err(|_| invalid_line(text));
        Ok(Self {
            start: edge(start)?,
            end: end.map_or(Ok(GridEdge::Auto), edge)?,
        })
    }
}

impl fmt::Display for GridPlacement {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.end {
            GridEdge::Auto => write!(formatter, "{}", self.start),
            end => write!(formatter, "{} / {end}", self.start),
        }
    }
}

struct PlacementVisitor;

impl Visitor<'_> for PlacementVisitor {
    type Value = GridPlacement;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a grid line: a line number, \"1 / 3\", \"span 2\", or \"auto\"")
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<GridPlacement, E> {
        i16::try_from(value)
            .map(|line| GridPlacement {
                start: GridEdge::Line(line),
                end: GridEdge::Auto,
            })
            .map_err(|_| E::custom(invalid_line(&value.to_string())))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<GridPlacement, E> {
        i16::try_from(value)
            .map(|line| GridPlacement {
                start: GridEdge::Line(line),
                end: GridEdge::Auto,
            })
            .map_err(|_| E::custom(invalid_line(&value.to_string())))
    }

    /// A JSON number written with a fraction is a line when it is whole, as
    /// a JavaScript number is.
    #[allow(clippy::cast_possible_truncation)]
    fn visit_f64<E: de::Error>(self, value: f64) -> Result<GridPlacement, E> {
        if value.fract() == 0.0 && (f64::from(i16::MIN)..=f64::from(i16::MAX)).contains(&value) {
            return Ok(GridPlacement {
                start: GridEdge::Line(value as i16),
                end: GridEdge::Auto,
            });
        }
        Err(E::custom(invalid_line(&value.to_string())))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<GridPlacement, E> {
        GridPlacement::parse(value).map_err(E::custom)
    }
}

impl<'de> Deserialize<'de> for GridPlacement {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(PlacementVisitor)
    }
}

impl Serialize for GridPlacement {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match (self.start, self.end) {
            (GridEdge::Line(line), GridEdge::Auto) => serializer.serialize_i16(line),
            _ => serializer.collect_str(self),
        }
    }
}

#[cfg(feature = "schema")]
mod schema {
    //! The schema of each form: what JSON it takes. A string's grammar is
    //! the parser's; the patterns say it where a regular expression can.

    use super::{GridPlacement, Insets, LayoutLength, LayoutOffset, LayoutSize, TrackList};
    use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
    use std::borrow::Cow;

    const NUMBER: &str = r"-?(\d+\.?\d*|\.\d+)";

    fn size_pattern() -> String {
        format!("^ *{NUMBER}(px)? *$")
    }

    fn length_pattern() -> String {
        format!("^ *{NUMBER}(px|%)? *$")
    }

    fn sides_pattern() -> String {
        format!("^ *{NUMBER}(px)?( +{NUMBER}(px)?){{0,3}} *$")
    }

    impl JsonSchema for LayoutLength {
        fn schema_name() -> Cow<'static, str> {
            "LayoutLength".into()
        }

        fn json_schema(_: &mut SchemaGenerator) -> Schema {
            json_schema!({
                "description": "A length: a number is a fraction of the layout picture's shorter side, \"<n>px\" pixels of the project canvas, \"<n>%\" a percent of the parent's content box along the field's axis.",
                "anyOf": [
                    { "type": "number", "format": "float" },
                    { "type": "string", "pattern": length_pattern() },
                ],
            })
        }
    }

    impl JsonSchema for LayoutSize {
        fn schema_name() -> Cow<'static, str> {
            "LayoutSize".into()
        }

        fn json_schema(_: &mut SchemaGenerator) -> Schema {
            json_schema!({
                "description": "A length that may not be a percent: a number is a fraction of the layout picture's shorter side, \"<n>px\" pixels of the project canvas.",
                "anyOf": [
                    { "type": "number", "format": "float" },
                    { "type": "string", "pattern": size_pattern() },
                ],
            })
        }
    }

    impl JsonSchema for Insets {
        fn schema_name() -> Cow<'static, str> {
            "Insets".into()
        }

        fn json_schema(generator: &mut SchemaGenerator) -> Schema {
            let side = generator.subschema_for::<LayoutSize>();
            json_schema!({
                "description": "A length on each side of a box: one length for every side, the CSS shorthand of one to four lengths (\"8px 16px\": top first, clockwise), or the sides object, missing sides 0.",
                "anyOf": [
                    { "type": "number", "format": "float" },
                    { "type": "string", "pattern": sides_pattern() },
                    {
                        "type": "object",
                        "properties": {
                            "top": side,
                            "right": side,
                            "bottom": side,
                            "left": side,
                        },
                        "additionalProperties": false,
                    },
                ],
            })
        }
    }

    impl JsonSchema for LayoutOffset {
        fn schema_name() -> Cow<'static, str> {
            "LayoutOffset".into()
        }

        fn json_schema(generator: &mut SchemaGenerator) -> Schema {
            let side = generator.subschema_for::<LayoutSize>();
            json_schema!({
                "description": "An offset, x right and y down: one or two lengths as CSS translate writes them (y 0 when missing), or {x, y}.",
                "anyOf": [
                    {
                        "type": "string",
                        "pattern": format!("^ *{NUMBER}(px)?( +{NUMBER}(px)?)? *$"),
                    },
                    {
                        "type": "object",
                        "properties": { "x": side, "y": side },
                        "required": ["x", "y"],
                        "additionalProperties": false,
                    },
                ],
            })
        }
    }

    impl JsonSchema for TrackList {
        fn schema_name() -> Cow<'static, str> {
            "TrackList".into()
        }

        fn json_schema(_: &mut SchemaGenerator) -> Schema {
            json_schema!({
                "description": "A grid track list as CSS writes it: \"1fr 2fr 120px\", \"repeat(3, minmax(100px, 1fr))\", \"repeat(auto-fill, 160px)\", \"auto min-content max-content\", \"fit-content(200px)\". A bare number is a fraction of the picture's shorter side. Named lines are not supported.",
                "type": "string",
            })
        }
    }

    impl JsonSchema for GridPlacement {
        fn schema_name() -> Cow<'static, str> {
            "GridPlacement".into()
        }

        fn json_schema(_: &mut SchemaGenerator) -> Schema {
            json_schema!({
                "description": "Where a grid item lies on one axis, as CSS writes it: 2, \"1 / 3\", \"span 2\", \"2 / span 3\", \"auto\". Lines count from 1, or from -1 at the end.",
                "anyOf": [
                    { "type": "integer", "minimum": -32768, "maximum": 32767 },
                    { "type": "string" },
                ],
            })
        }
    }
}

#[cfg(test)]
#[path = "layout_values_tests.rs"]
mod tests;

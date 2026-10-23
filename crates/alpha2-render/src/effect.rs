use alpha2_gpu::CapabilityTier;
use alpha2_time::MediaTime;
use std::borrow::{Borrow, Cow};
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// Representation an effect resource binding expects from its registered data.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EffectResourceKind {
    /// A cubic, filterable RGB lookup table, normally authored as a `.cube` file.
    Lut3d,
}

/// Stable identity of one registered effect version.
///
/// Cloning one never allocates: the engine's own passes name a static
/// string, and an identity made from a `String` shares it. Two identities are
/// equal when their text and version are, however each holds its text.
#[derive(Clone, Debug)]
pub struct EffectId {
    id: IdText,
    version: u32,
}

#[derive(Clone, Debug)]
enum IdText {
    Static(&'static str),
    Shared(Arc<str>),
}

impl EffectId {
    /// Creates an exact runtime-effect identity.
    #[must_use]
    pub fn new(id: String, version: u32) -> Self {
        Self {
            id: IdText::Shared(id.into()),
            version,
        }
    }

    /// An identity whose text is a constant, as the renderer's and the
    /// engine's own passes are named; making one allocates nothing.
    #[must_use]
    pub const fn from_static(id: &'static str, version: u32) -> Self {
        Self {
            id: IdText::Static(id),
            version,
        }
    }

    /// Returns the author-defined identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match &self.id {
            IdText::Static(id) => id,
            IdText::Shared(id) => id,
        }
    }

    /// Returns the schema version.
    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }
}

impl PartialEq for EffectId {
    fn eq(&self, other: &Self) -> bool {
        self.version == other.version && self.as_str() == other.as_str()
    }
}

impl Eq for EffectId {}

impl Hash for EffectId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // The same hash as the key it borrows as, so a lookup by key finds it.
        (self as &dyn EffectKey).hash(state);
    }
}

/// An effect identity as text and version, so a registry keyed by
/// [`EffectId`] can be asked about an `(&str, u32)` without making one.
pub(crate) trait EffectKey {
    fn key(&self) -> (&str, u32);
}

impl EffectKey for EffectId {
    fn key(&self) -> (&str, u32) {
        (self.as_str(), self.version)
    }
}

impl EffectKey for (&str, u32) {
    fn key(&self) -> (&str, u32) {
        *self
    }
}

impl<'a> Borrow<dyn EffectKey + 'a> for EffectId {
    fn borrow(&self) -> &(dyn EffectKey + 'a) {
        self
    }
}

impl Hash for dyn EffectKey + '_ {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let (id, version) = self.key();
        id.hash(state);
        version.hash(state);
    }
}

impl PartialEq for dyn EffectKey + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for dyn EffectKey + '_ {}

/// Runtime shader category.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EffectKind {
    /// An image effect: one source image (`src`, `sample_src`), plus the
    /// named inputs its definition declares.
    Effect,
    /// Two-input timeline transition.
    Transition,
    /// Zero-input picture source: its body defines `fn generate(uv: vec2f) ->
    /// vec4f`, returning a premultiplied, sRGB-encoded color.
    Generator,
}

/// Host-side parameter representation.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ParamType {
    /// One 32-bit floating-point value.
    Float,
    /// Two 32-bit floating-point values.
    Vec2,
    /// Four independent 32-bit floating-point values, such as per-channel
    /// thresholds. They keyframe component by component.
    Vec4,
    /// A straight (not premultiplied), sRGB-encoded RGBA color in `0..=1`.
    /// Its unit is always [`ParamUnit::Color`]; a four-component value that
    /// is not a color is a [`ParamType::Vec4`].
    Color,
    /// Integer index into an author-defined option list.
    Enum,
    /// Boolean encoded as a host-shareable `u32` in WGSL.
    Bool,
}

/// Semantic unit exposed to authoring hosts.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ParamUnit {
    /// Resolution-independent fraction.
    Normalized,
    /// Angle measured in degrees.
    Degrees,
    /// Straight, sRGB-encoded RGBA color.
    Color,
    /// Index into named choices.
    Enum,
    /// Boolean toggle.
    Bool,
}

/// Typed default parameter value.
#[derive(Clone, Debug, PartialEq)]
pub enum ParamValue {
    /// Scalar value.
    Float(f32),
    /// Two-dimensional value.
    Vec2([f32; 2]),
    /// Four-dimensional value.
    Vec4([f32; 4]),
    /// RGBA color.
    Color([f32; 4]),
    /// Option index.
    Enum(u32),
    /// Boolean value.
    Bool(bool),
}

/// Named values supplied to one runtime effect invocation.
///
/// A name is borrowed for `'a` or owned: the renderer's own passes name
/// static strings and a project's names live as long as the frame drawn from
/// it, so building a frame's parameters allocates only the list that holds
/// them. The list is kept sorted by name, one value per name.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EffectParams<'a> {
    values: Vec<(Cow<'a, str>, ParamValue)>,
}

/// Named resource identities supplied to one effect invocation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EffectResources {
    values: BTreeMap<String, String>,
}

impl EffectResources {
    /// Creates an empty resource set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Associates one definition slot with a registered resource identity.
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, id: impl Into<String>) -> Self {
        self.values.insert(name.into(), id.into());
        self
    }

    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values
            .iter()
            .map(|(name, id)| (name.as_str(), id.as_str()))
    }
}

impl<'a> EffectParams<'a> {
    /// Creates an empty set that falls back to every schema default.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty set with room for `capacity` named values.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            values: Vec::with_capacity(capacity),
        }
    }

    /// Adds or replaces one named value.
    #[must_use]
    pub fn with(mut self, name: impl Into<Cow<'a, str>>, value: ParamValue) -> Self {
        self.insert(name, value);
        self
    }

    /// Adds or replaces one named value in place.
    pub fn insert(&mut self, name: impl Into<Cow<'a, str>>, value: ParamValue) {
        let name = name.into();
        match self.position(&name) {
            Ok(index) => self.values[index].1 = value,
            Err(index) => self.values.insert(index, (name, value)),
        }
    }

    fn position(&self, name: &str) -> Result<usize, usize> {
        self.values
            .binary_search_by(|(held, _)| held.as_ref().cmp(name))
    }

    pub(crate) fn get(&self, name: &str) -> Option<&ParamValue> {
        self.position(name).ok().map(|index| &self.values[index].1)
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.values.iter().map(|(name, _)| name.as_ref())
    }

    /// Every value by name, in name order.
    pub(crate) fn sorted_values(&self) -> impl Iterator<Item = (&str, &ParamValue)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_ref(), value))
    }
}

/// Per-invocation values supplied by the timeline runner.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct EffectRuntime {
    /// The time the pass's settings are sampled at, exact in ticks: the
    /// timeline time for a timeline transition, and for a composition step
    /// the composition time it is sampled at (the composition's time plus
    /// the step's offset). The engine passes zero to an effect that cannot
    /// read it ([`crate::Renderer::effect_reads_time`]) and to a pass whose
    /// picture does not change with time. A shader reads it as seconds with
    /// `time()`, an `f32` that cannot tell frames apart on a long timeline
    /// (1/32 s apart from about 73 hours on), and exactly with
    /// `time_ticks()`; the cache key holds the ticks.
    pub time: MediaTime,
    /// Transition progress in the inclusive unit interval.
    pub progress: f32,
    /// Output width divided by output height.
    pub ratio: f32,
}

impl Default for EffectRuntime {
    fn default() -> Self {
        Self {
            time: MediaTime::ZERO,
            progress: 0.0,
            ratio: 1.0,
        }
    }
}

/// One runtime effect parameter.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamSpec {
    /// WGSL-safe field name.
    pub name: String,
    /// Storage and authoring type.
    pub param_type: ParamType,
    /// Value used when a project omits this parameter.
    pub default: ParamValue,
    /// Optional inclusive scalar minimum.
    pub min: Option<f32>,
    /// Optional inclusive scalar maximum.
    pub max: Option<f32>,
    /// Optional positive authoring increment.
    pub step: Option<f32>,
    /// UI and serialization unit.
    pub unit: ParamUnit,
    /// Named values for [`ParamType::Enum`].
    pub enum_values: Vec<String>,
}

/// One named external resource required by an effect definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceSpec {
    /// WGSL-safe slot name used to generate a `sample_<name>` helper.
    pub name: String,
    /// Required registered resource representation.
    pub kind: EffectResourceKind,
}

/// One image an effect samples besides its source, by the name its shader
/// reads it under: `sample_<name>(uv)` and `size_<name>()`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputSpec {
    /// WGSL-safe name, unique among the definition's parameters, resources,
    /// and inputs.
    pub name: String,
}

/// Data-only runtime effect, transition, or generator definition.
///
/// The renderer's quality controls reach every effect at run time, not
/// through its parameters: the shader reads its filter-sample budget from
/// the prelude's `max_filter_samples()` and scales its own cost by it, as
/// the built-in blur does.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectDef {
    /// Stable author-defined identifier.
    pub id: String,
    /// Schema and shader version.
    pub version: u32,
    /// Effect or transition contract.
    pub kind: EffectKind,
    /// Images the effect samples besides its source, at most
    /// [`crate::MAX_EFFECT_INPUTS`]; only an [`EffectKind::Effect`] declares
    /// them.
    pub inputs: Vec<InputSpec>,
    /// Ordered parameter schema.
    pub params: Vec<ParamSpec>,
    /// Ordered external resource schema.
    pub resources: Vec<ResourceSpec>,
    /// Author function implementing the selected contract.
    pub wgsl_body: String,
    /// Minimum required GPU capability tier.
    pub tier_min: CapabilityTier,
    /// SPDX expression or attribution text.
    pub license: Option<String>,
}

/// Automatically computed uniform member placement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UniformField {
    /// Parameter name.
    pub name: String,
    /// Byte offset from the uniform buffer start.
    pub offset: u32,
    /// Bytes reserved for the member.
    pub size: u32,
}

/// Host layout generated from a parameter schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UniformLayout {
    /// Members in schema order.
    pub fields: Vec<UniformField>,
    /// Total buffer size rounded to WGSL uniform alignment.
    pub size: u32,
}

/// Shader compiler message severity.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DiagnosticSeverity {
    /// Registration cannot continue.
    Error,
    /// Shader is valid but deserves attention.
    Warning,
    /// Informational compiler message.
    Info,
}

/// One schema or WGSL diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShaderDiagnostic {
    /// Severity category.
    pub severity: DiagnosticSeverity,
    /// Human-readable description.
    pub message: String,
    /// Optional one-based generated WGSL line.
    pub line: Option<u32>,
    /// Optional one-based generated WGSL byte column.
    pub column: Option<u32>,
}

/// Diagnostics returned when runtime registration fails.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShaderDiagnostics {
    /// All collected messages.
    pub messages: Vec<ShaderDiagnostic>,
}

impl ShaderDiagnostics {
    pub(crate) fn error(message: impl Into<String>) -> Self {
        Self {
            messages: vec![ShaderDiagnostic {
                severity: DiagnosticSeverity::Error,
                message: message.into(),
                line: None,
                column: None,
            }],
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::{EffectId, EffectKey, EffectParams, ParamValue};

    #[test]
    fn an_effect_identity_is_its_text_and_version_however_it_holds_the_text() {
        let shared = EffectId::new("alpha2.effect.vignette".to_owned(), 1);
        let constant = EffectId::from_static("alpha2.effect.vignette", 1);
        assert_eq!(shared, constant);
        assert_ne!(constant, EffectId::from_static("alpha2.effect.vignette", 2));
        assert_ne!(constant, EffectId::from_static("alpha2.effect.sharpen", 1));
        // Equal identities hash alike, so a map keyed by one finds the other or the bare key.
        let map = HashMap::from([(shared.clone(), "vignette")]);
        assert_eq!(map.get(&constant), Some(&"vignette"));
        let key: &dyn EffectKey = &("alpha2.effect.vignette", 1);
        assert_eq!(map.get_key_value(key).map(|(id, _)| id), Some(&shared));
        let wrong: &dyn EffectKey = &("alpha2.effect.vignette", 2);
        assert!(!map.contains_key(wrong));
        assert_eq!(HashSet::from([shared, constant]).len(), 1);
    }

    #[test]
    fn parameters_keep_one_value_per_name_in_name_order() {
        let project_name = String::from("b_owned");
        let params = EffectParams::with_capacity(4)
            .with("c", ParamValue::Float(3.0))
            .with(project_name.as_str(), ParamValue::Float(2.0))
            .with(String::from("a"), ParamValue::Float(1.0))
            .with("c", ParamValue::Float(4.0));
        assert_eq!(
            params.sorted_values().collect::<Vec<_>>(),
            [
                ("a", &ParamValue::Float(1.0)),
                ("b_owned", &ParamValue::Float(2.0)),
                ("c", &ParamValue::Float(4.0)),
            ]
        );
        assert_eq!(params.get("c"), Some(&ParamValue::Float(4.0)));
        assert_eq!(params.get("d"), None);
        assert_eq!(params.names().collect::<Vec<_>>(), ["a", "b_owned", "c"]);
        // However the names were given, equal values in any order are equal parameters.
        let again = EffectParams::new()
            .with(String::from("c"), ParamValue::Float(4.0))
            .with("a", ParamValue::Float(1.0))
            .with(String::from("b_owned"), ParamValue::Float(2.0));
        assert_eq!(params, again);
    }
}

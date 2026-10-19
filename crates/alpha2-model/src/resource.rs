use crate::EffectResourceId;
use serde::{Deserialize, Serialize};

/// Host-resolved, non-media data consumed by one or more visual effects.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct EffectResource {
    /// Stable identity referenced by effect invocations.
    pub id: EffectResourceId,
    /// Resource representation expected by the effect definition.
    pub kind: EffectResourceKind,
    /// Opaque host locator. The core never opens it directly.
    pub locator: String,
}

/// Portable effect-resource categories supported by the render ABI.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum EffectResourceKind {
    /// A cubic, filterable RGB lookup table, normally authored as a `.cube` file.
    Lut3d,
}

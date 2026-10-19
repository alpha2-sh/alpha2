//! Identifiers of the items a project document names.

use alpha2_media::IdError;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use std::fmt;

// Same shape as the macro that defines `alpha2_media::SourceId`, sharing its `IdError`. An ID may
// also forbid one character, which its schema then excludes by `pattern`.
macro_rules! define_id {
    ($name:ident, $description:literal $(, without = $forbidden:literal, pattern = $pattern:literal)?) => {
        #[doc = $description]
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                $(if value.contains($forbidden) {
                    return Err(D::Error::custom(format!(
                        "{} {value:?} has a `{}`",
                        stringify!($name),
                        $forbidden
                    )));
                })?
                Self::new(value).map_err(D::Error::custom)
            }
        }

        impl $name {
            /// Creates an identifier from a non-empty, stable external value.
            ///
            /// UUIDs and ULIDs are both suitable values. The engine does not
            /// generate IDs because identity belongs to the document author.
            ///
            /// # Errors
            ///
            /// Returns [`IdError::Empty`] when `value` is empty.
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                if value.is_empty() {
                    Err(IdError::Empty)
                } else {
                    Ok(Self(value))
                }
            }

            /// Returns this identifier as its serialized string value.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        #[cfg(feature = "schema")]
        impl schemars::JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                #[allow(unused_mut)]
                let mut schema = schemars::json_schema!({
                    "description": $description,
                    "type": "string",
                    "minLength": 1,
                });
                $(schema.insert("pattern".into(), $pattern.into());)?
                schema
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

define_id!(ProjectId, "A stable project identifier.");
define_id!(EffectResourceId, "A stable effect-resource identifier.");
define_id!(TrackId, "A stable timeline-track identifier.");
define_id!(ClipId, "A stable timeline-clip identifier.");
define_id!(CompositionId, "A stable composition-graph identifier.");
define_id!(
    NodeId,
    "A stable identifier of a node within one composition graph. It has no `/`, which joins the IDs of the group and media nodes a step was drawn through.",
    without = '/',
    pattern = "^[^/]*$"
);

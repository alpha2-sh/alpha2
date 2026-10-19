//! Identity of a media source.

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use std::fmt;

macro_rules! define_id {
    ($name:ident, $description:literal) => {
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
                schemars::json_schema!({
                    "description": $description,
                    "type": "string",
                    "minLength": 1,
                })
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

define_id!(SourceId, "A stable media-source identifier.");

/// An error returned when an identifier cannot be constructed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum IdError {
    /// IDs must contain at least one character.
    Empty,
}

impl std::error::Error for IdError {}

impl fmt::Display for IdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("identifier must not be empty")
    }
}

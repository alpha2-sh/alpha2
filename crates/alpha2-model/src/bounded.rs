//! Sequences with a count limit, refused while the document is parsed: reading stops at the
//! first element over the limit, so an over-long sequence never allocates its length.
//!
//! Use as `#[serde(deserialize_with = "crate::bounded::seq::<_, _, { LIMIT }>")]` beside
//! `#[cfg_attr(feature = "schema", schemars(length(max = LIMIT)))]`, which states the same limit
//! as `maxItems`. `validate()` keeps its count checks for projects built in memory.

use serde::de::{Deserialize, Deserializer, Error as _, IgnoredAny, MapAccess, SeqAccess, Visitor};
use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;

/// Elements reserved before any is read, whatever length the input claims.
const FIRST_CAPACITY: usize = 1_024;

struct Bounded<T, const MAX: usize>(Vec<T>);

impl<'de, T: Deserialize<'de>, const MAX: usize> Deserialize<'de> for Bounded<T, MAX> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Elements<T, const MAX: usize>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>, const MAX: usize> Visitor<'de> for Elements<T, MAX> {
            type Value = Vec<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "a sequence of at most {MAX} elements")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
                let hint = seq.size_hint().unwrap_or(0);
                let mut elements = Vec::with_capacity(hint.min(MAX).min(FIRST_CAPACITY));
                while elements.len() < MAX {
                    match seq.next_element()? {
                        Some(element) => elements.push(element),
                        None => return Ok(elements),
                    }
                }
                if seq.next_element::<IgnoredAny>()?.is_some() {
                    return Err(A::Error::invalid_length(MAX + 1, &self));
                }
                Ok(elements)
            }
        }

        deserializer
            .deserialize_seq(Elements::<T, MAX>(PhantomData))
            .map(Self)
    }
}

/// Reads a sequence of at most `MAX` elements.
pub(crate) fn seq<'de, D, T, const MAX: usize>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Bounded::<T, MAX>::deserialize(deserializer).map(|bounded| bounded.0)
}

/// Reads an optional sequence of at most `MAX` elements.
pub(crate) fn option_seq<'de, D, T, const MAX: usize>(
    deserializer: D,
) -> Result<Option<Vec<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<Bounded<T, MAX>>::deserialize(deserializer).map(|bounded| bounded.map(|b| b.0))
}

/// Reads a map of at most `MAX` entries; a repeated key counts once, as it is kept once.
pub(crate) fn map<'de, D, K, V, const MAX: usize>(
    deserializer: D,
) -> Result<BTreeMap<K, V>, D::Error>
where
    D: Deserializer<'de>,
    K: Deserialize<'de> + Ord,
    V: Deserialize<'de>,
{
    struct Entries<K, V, const MAX: usize>(PhantomData<(K, V)>);

    impl<'de, K, V, const MAX: usize> Visitor<'de> for Entries<K, V, MAX>
    where
        K: Deserialize<'de> + Ord,
        V: Deserialize<'de>,
    {
        type Value = BTreeMap<K, V>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "a map of at most {MAX} entries")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<BTreeMap<K, V>, A::Error> {
            let mut entries = BTreeMap::new();
            while let Some((key, value)) = access.next_entry()? {
                entries.insert(key, value);
                if entries.len() > MAX {
                    return Err(A::Error::invalid_length(MAX + 1, &self));
                }
            }
            Ok(entries)
        }
    }

    deserializer.deserialize_map(Entries::<K, V, MAX>(PhantomData))
}

#![no_main]

use alpha2_model::{
    GridPlacement, Insets, LayoutLength, LayoutOffset, LayoutSize, Paint, Rgba, TrackList,
};
use libfuzzer_sys::fuzz_target;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;

/// `value` as `T`, when it is one: then it writes a form that reads back as the same value.
fn round_trips<T>(value: &serde_json::Value)
where
    T: for<'de> Deserialize<'de> + Serialize + PartialEq + Debug,
{
    let Ok(read) = T::deserialize(value) else {
        return;
    };
    let written = serde_json::to_string(&read).expect("a parsed value writes");
    let again: T = serde_json::from_str(&written)
        .unwrap_or_else(|error| panic!("{value} wrote {written}, which does not read: {error}"));
    assert_eq!(again, read, "{value} wrote {written}");
}

// Any string, as every CSS value form a layout reads: none panics, and each it parses writes back
// as text that parses to the same value.
fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let value = serde_json::Value::String(text.to_owned());
    round_trips::<LayoutLength>(&value);
    round_trips::<LayoutSize>(&value);
    round_trips::<Insets>(&value);
    round_trips::<LayoutOffset>(&value);
    round_trips::<TrackList>(&value);
    round_trips::<GridPlacement>(&value);
    round_trips::<Rgba>(&value);
    round_trips::<Paint>(&value);
    if let Ok(tracks) = TrackList::deserialize(&value) {
        assert_eq!(
            tracks.to_string(),
            serde_json::to_value(&tracks).expect("writes")
        );
    }
});

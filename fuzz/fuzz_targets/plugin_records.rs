#![no_main]

use alpha2_plugin_abi::records::{Limits, decode, encode, records};
use libfuzzer_sys::fuzz_target;

// Any bytes a plugin emits decode as records or are refused; decoded records encode to bytes that
// decode and encode again to the same bytes. The bounded iterator reads a prefix of what decode
// reads and stops at its first refusal.
fuzz_target!(|data: &[u8]| {
    let limits = Limits {
        max_values: 256,
        max_text_bytes: 256,
    };
    let bounded = records(data, limits).collect::<Vec<_>>();
    assert!(bounded.iter().rev().skip(1).all(Result::is_ok));
    let read = bounded.iter().map_while(|record| record.as_ref().ok());
    let Ok(records) = decode(data) else {
        return;
    };
    assert!(read.zip(&records).all(|(bounded, full)| bounded == full));
    let mut bytes = Vec::new();
    for record in &records {
        encode(record, &mut bytes);
    }
    let again = decode(&bytes).expect("encoded records decode");
    let mut twice = Vec::new();
    for record in &again {
        encode(record, &mut twice);
    }
    assert_eq!(bytes, twice);
});

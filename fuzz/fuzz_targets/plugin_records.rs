#![no_main]

use alpha2_plugin_abi::records::{decode, encode};
use libfuzzer_sys::fuzz_target;

// Any bytes a plugin emits decode as records or are refused; decoded records encode to bytes that
// decode and encode again to the same bytes.
fuzz_target!(|data: &[u8]| {
    let Ok(records) = decode(data) else {
        return;
    };
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

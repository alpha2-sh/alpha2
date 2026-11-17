#![no_main]

use alpha2_render::Lut3dData;
use libfuzzer_sys::fuzz_target;

// Any text parses as a 3D `.cube` table or is refused, never panics, and an accepted table holds
// edge^3 finite samples inside a finite domain.
fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(table) = Lut3dData::from_cube(text) else {
        return;
    };
    let edge = usize::try_from(table.edge_length).expect("edge fits");
    assert_eq!(Some(table.values.len()), edge.checked_pow(3));
    assert!(table.values.iter().flatten().all(|value| value.is_finite()));
    assert!(
        table
            .domain_min
            .iter()
            .chain(&table.domain_max)
            .all(|value| value.is_finite())
    );
});

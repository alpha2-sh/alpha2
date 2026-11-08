use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::records::{Record, RecordError, decode, encode};

fn batch(records: &[Record]) -> Vec<u8> {
    let mut out = Vec::new();
    for record in records {
        encode(record, &mut out);
    }
    out
}

fn every_kind() -> Vec<Record> {
    vec![
        Record::Sample {
            output: 0,
            time: -4,
            values: vec![Some(0.25), None, Some(-1.5)],
        },
        // More than 32 channels takes a second mask word.
        Record::Sample {
            output: 3,
            time: i64::MAX,
            values: (0..40)
                .map(|i| (i % 7 != 0).then_some(f32::from(u8::try_from(i).unwrap_or(0))))
                .collect(),
        },
        Record::Sample {
            output: 1,
            time: 0,
            values: Vec::new(),
        },
        Record::Word {
            output: 2,
            start: 100,
            end: 200,
            text: String::from("caf\u{e9}"),
            speaker: Some(String::from("a")),
        },
        Record::Word {
            output: 2,
            start: 200,
            end: 300,
            text: String::from("x"),
            speaker: None,
        },
        Record::Interval {
            output: u16::MAX,
            start: 0,
            end: 48_000,
            label: Some(String::new()),
            score: Some(0.875),
        },
        Record::Interval {
            output: 4,
            start: i64::MIN,
            end: 1,
            label: None,
            score: None,
        },
    ]
}

#[test]
fn every_record_kind_reads_back_as_written() {
    let records = every_kind();
    assert_eq!(decode(&batch(&records)), Ok(records.clone()));
    for record in &records {
        assert_eq!(
            decode(&batch(core::slice::from_ref(record))),
            Ok(vec![record.clone()])
        );
    }
    assert_eq!(decode(&[]), Ok(Vec::new()));
}

#[test]
fn the_layout_is_the_documented_one() {
    let bytes = batch(&[Record::Sample {
        output: 2,
        time: 7,
        values: vec![Some(1.0), None],
    }]);
    let mut expected = vec![1, 0, 2, 0, 24, 0, 0, 0];
    expected.extend_from_slice(&7_i64.to_le_bytes());
    expected.extend_from_slice(&2_u32.to_le_bytes());
    expected.extend_from_slice(&0b10_u32.to_le_bytes());
    expected.extend_from_slice(&1.0_f32.to_le_bytes());
    expected.extend_from_slice(&0.0_f32.to_le_bytes());
    assert_eq!(bytes, expected);
}

#[test]
fn malformed_batches_name_the_record_and_the_reason() {
    let good = batch(&every_kind()[..1]);
    let patched = |change: fn(&mut Vec<u8>)| {
        let mut bytes = good.clone();
        change(&mut bytes);
        bytes
    };
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (good[..5].to_vec(), "the header ends early"),
        (patched(|b| b[1] = 1), "the reserved header byte is not 0"),
        (patched(|b| b[0] = 9), "unknown record tag"),
        (
            patched(|b| b.truncate(b.len() - 1)),
            "the body is shorter than its length",
        ),
        (
            patched(|b| {
                b[4] += 1;
                b.push(0);
            }),
            "the body is longer than its fields",
        ),
        // The count, at offset 16, claims more values than the body holds.
        (patched(|b| b[16] = 200), "the sample's fields end early"),
        // The lost mask, at offset 20, marks a fourth value of three.
        (
            patched(|b| b[20] |= 0b1000),
            "a lost mask marks a value past the count",
        ),
    ];
    for (bytes, reason) in cases {
        let mut twice = good.clone();
        twice.extend_from_slice(&bytes);
        assert_eq!(
            decode(&twice),
            Err(RecordError { index: 1, reason }),
            "{reason}"
        );
    }
    let mut word = batch(&every_kind()[3..4]);
    let text_at = 8 + 8 + 8 + 8;
    word[text_at] = 0xff;
    assert_eq!(
        decode(&word),
        Err(RecordError {
            index: 0,
            reason: "a text is not UTF-8"
        })
    );
    let mut interval = batch(&every_kind()[6..7]);
    interval[8 + 16] = 2;
    assert_eq!(
        decode(&interval),
        Err(RecordError {
            index: 0,
            reason: "unknown interval flags"
        })
    );
}

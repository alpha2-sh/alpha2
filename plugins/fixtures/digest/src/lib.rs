//! The `digest` test extractor as a plugin. It emits one `scalars` sample per delivery: the
//! delivered bytes' 32-bit FNV-1a hash, its upper and lower 16 bits over 65536, exactly as the
//! engine's `alpha2.test.digest` fixture does, so a loader can compare the two.

use alpha2_plugin_sdk::{Audio, Context, Emitter, Error, Extractor, Frame, Host};

/// 32-bit FNV-1a.
#[must_use]
pub fn fnv1a(bytes: impl IntoIterator<Item = u8>) -> u32 {
    bytes.into_iter().fold(0x811c_9dc5, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    })
}

/// A hash as the two values the digest emits, exact in f32.
#[must_use]
pub fn halves(hash: u32) -> [Option<f32>; 2] {
    let half = |bits: u32| f32::from(u16::try_from(bits & 0xffff).unwrap_or(0)) / 65_536.0;
    [Some(half(hash >> 16)), Some(half(hash))]
}

/// The extractor.
pub struct Digest;

impl Extractor for Digest {
    const DESCRIPTOR: &'static str = include_str!("../descriptor.json");

    fn begin(_context: &Context, _host: &Host) -> Result<Self, Error> {
        Ok(Self)
    }

    fn video(&mut self, frame: &Frame<'_>, out: &mut Emitter) -> Result<(), Error> {
        out.sample(
            "digest",
            frame.time,
            &halves(fnv1a(frame.data.iter().copied())),
        );
        Ok(())
    }

    fn audio(&mut self, block: &Audio<'_>, out: &mut Emitter) -> Result<(), Error> {
        let bytes = block.samples.iter().flat_map(|sample| sample.to_le_bytes());
        out.sample("digest", block.start, &halves(fnv1a(bytes)));
        Ok(())
    }
}

alpha2_plugin_sdk::export_extractor!(Digest);

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alpha2_plugin_abi::records::Record;
    use alpha2_plugin_abi::{status, video_format};
    use alpha2_plugin_sdk::testing::Instance;

    use super::*;

    const CONTEXT: &str = r#"{
        "range": { "start": 0, "end": 8000 }, "passes": [{ "start": 0, "direction": "forward" }],
        "from": "source", "video": { "size": 2, "format": "rgba8" }, "audio": null,
        "frame_rate": null, "params": {}
    }"#;

    #[test]
    fn each_delivery_emits_its_hash() {
        // SAFETY: the plugin's own entry returns its static table.
        let mut plugin = unsafe { Instance::new(alpha2_plugin_v1(), HashMap::new()) };
        assert_eq!(plugin.describe(), Digest::DESCRIPTOR);
        assert_eq!(plugin.begin(CONTEXT), status::OK);
        assert_eq!(plugin.pass(0, 0), status::OK);
        let pixels = [1, 2, 3, 255, 4, 5, 6, 255];
        assert_eq!(
            plugin.video(4000, 2, 1, video_format::RGBA8, &pixels),
            status::OK
        );
        assert_eq!(plugin.audio(0, 0, 2, 48_000, 1, &[0.5, -0.25]), status::OK);
        assert_eq!(plugin.end(), status::OK);
        let audio_bytes = [0.5_f32, -0.25]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect::<Vec<_>>();
        assert_eq!(
            plugin.take().records,
            vec![
                Record::Sample {
                    output: 0,
                    time: 4000,
                    values: halves(fnv1a(pixels)).to_vec()
                },
                Record::Sample {
                    output: 0,
                    time: 0,
                    values: halves(fnv1a(audio_bytes)).to_vec()
                },
            ]
        );
    }

    #[test]
    fn the_hash_is_fnv1a() {
        assert_eq!(fnv1a([]), 0x811c_9dc5);
        assert_eq!(fnv1a(*b"a"), 0xe40c_292c);
        assert_eq!(halves(0x8001_4000), [Some(0.500_015_26), Some(0.25)]);
    }
}

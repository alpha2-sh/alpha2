use crate::{
    Color, EffectId, EffectParams, EffectResources, EffectRuntime, ParamValue, RenderSize,
};

use crate::graph::{EffectInputs, GraphLayer, RenderCacheKey};

pub(crate) fn transient_source_key(graph: u32, index: usize) -> RenderCacheKey {
    let mut hash = StableHash::new();
    hash.bytes(b"transient-source");
    hash.u32(graph);
    hash.u64(index as u64);
    RenderCacheKey(hash.finish())
}

#[cfg(test)]
pub(crate) fn effect_cache_key(
    tag: &[u8],
    size: RenderSize,
    effect: &EffectId,
    inputs: &[RenderCacheKey],
    params: &EffectParams,
    runtime: EffectRuntime,
) -> RenderCacheKey {
    effect_cache_key_with_resources(
        tag,
        size,
        effect,
        inputs,
        params,
        &EffectResources::new(),
        runtime,
    )
}

pub(crate) fn effect_cache_key_with_resources(
    tag: &[u8],
    size: RenderSize,
    effect: &EffectId,
    inputs: &[RenderCacheKey],
    params: &EffectParams,
    resources: &EffectResources,
    runtime: EffectRuntime,
) -> RenderCacheKey {
    let mut hash = StableHash::new();
    hash.bytes(tag);
    hash.size(size);
    hash.string(effect.as_str());
    hash.u32(effect.version());
    for input in inputs {
        hash.u128(input.value());
    }
    for (name, value) in params.sorted_values() {
        hash.string(name);
        hash.param(value);
    }
    for (name, id) in resources.iter() {
        hash.string(name);
        hash.string(id);
    }
    hash.u64(runtime.time.ticks().cast_unsigned());
    hash.f32(runtime.progress);
    hash.f32(runtime.ratio);
    RenderCacheKey(hash.finish())
}

/// One key for an effect node's named inputs: each name and the key of the
/// node it reads, in name order.
pub(crate) fn named_inputs_key(
    inputs: &EffectInputs<'_>,
    keys: &[RenderCacheKey],
) -> RenderCacheKey {
    let mut hash = StableHash::new();
    hash.bytes(b"named-inputs");
    hash.u64(inputs.len() as u64);
    for (name, node) in inputs.iter() {
        hash.string(name);
        hash.u128(keys[node.index()].value());
    }
    RenderCacheKey(hash.finish())
}

pub(crate) fn composite_cache_key(
    size: RenderSize,
    layers: &[GraphLayer],
    background: Option<Color>,
    keys: &[RenderCacheKey],
) -> RenderCacheKey {
    let mut hash = StableHash::new();
    hash.bytes(b"composite");
    hash.size(size);
    if let Some(background) = background {
        hash.bytes(b"over");
        for component in [
            background.red,
            background.green,
            background.blue,
            background.alpha,
        ] {
            hash.u64(component.to_bits());
        }
    }
    for layer in layers {
        hash.u128(keys[layer.source.index()].value());
        hash.f32(layer.visual.position[0]);
        hash.f32(layer.visual.position[1]);
        hash.f32(layer.visual.anchor[0]);
        hash.f32(layer.visual.anchor[1]);
        hash.f32(layer.visual.scale[0]);
        hash.f32(layer.visual.scale[1]);
        hash.f32(layer.visual.rotation_degrees);
        hash.f32(layer.visual.opacity);
        hash.f32(layer.blend_mode.shader_value());
    }
    RenderCacheKey(hash.finish())
}

/// A node's key as the result cache holds it: its structural key and what
/// the renderer adds to every pass it draws. Quality controls, effect
/// definitions, and effect resources are not in it; changing them clears the
/// cache instead.
pub(crate) fn renderer_node_key(structural: RenderCacheKey, blend_space: f32) -> RenderCacheKey {
    let mut hash = StableHash::new();
    hash.bytes(b"result");
    hash.u128(structural.value());
    hash.f32(blend_space);
    RenderCacheKey(hash.finish())
}

/// A graph's key as [`crate::Renderer::graph_cache_key`] gives it: its
/// structural key and every renderer setting that changes its pixels, since
/// a caller keeping its own cache has no hook that clears it when one
/// changes: the blend space, the quality controls as the passes see them,
/// and the intermediate format pictures are stored in.
pub(crate) fn renderer_cache_key(
    structural: RenderCacheKey,
    blend_space: f32,
    quality: crate::RenderQualityControls,
    intermediate: wgpu::TextureFormat,
) -> RenderCacheKey {
    let mut hash = StableHash::new();
    hash.bytes(b"renderer");
    hash.u128(structural.value());
    hash.f32(blend_space);
    hash.u32(quality.max_filter_samples);
    hash.u32(quality.motion_blur_subframes);
    hash.string(match intermediate {
        wgpu::TextureFormat::Rgba16Float => "rgba16float",
        _ => "rgba8unorm",
    });
    RenderCacheKey(hash.finish())
}

struct StableHash([u64; 2]);

impl StableHash {
    const fn new() -> Self {
        Self([0xcbf2_9ce4_8422_2325, 0x6c62_272e_07bb_0142])
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.u64(bytes.len() as u64);
        for byte in bytes {
            self.mix(*byte);
        }
    }

    fn string(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.bytes(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        for byte in value.to_le_bytes() {
            self.mix(byte);
        }
    }

    fn u128(&mut self, value: u128) {
        for byte in value.to_le_bytes() {
            self.mix(byte);
        }
    }

    fn mix(&mut self, byte: u8) {
        self.0[0] ^= u64::from(byte);
        self.0[0] = self.0[0].wrapping_mul(0x0000_0100_0000_01b3);
        self.0[1] ^= u64::from(byte).wrapping_add(0x9d);
        self.0[1] = self.0[1].wrapping_mul(0x9e37_79b1_85eb_ca87);
    }

    fn f32(&mut self, value: f32) {
        self.u32(value.to_bits());
    }

    fn size(&mut self, size: RenderSize) {
        self.u32(size.width);
        self.u32(size.height);
    }

    fn param(&mut self, value: &ParamValue) {
        match value {
            ParamValue::Float(value) => {
                self.u32(0);
                self.f32(*value);
            }
            ParamValue::Vec2(values) => {
                self.u32(1);
                for value in values {
                    self.f32(*value);
                }
            }
            ParamValue::Vec4(values) => {
                self.u32(5);
                for value in values {
                    self.f32(*value);
                }
            }
            ParamValue::Color(values) => {
                self.u32(2);
                for value in values {
                    self.f32(*value);
                }
            }
            ParamValue::Enum(value) => {
                self.u32(3);
                self.u32(*value);
            }
            ParamValue::Bool(value) => {
                self.u32(4);
                self.u32(u32::from(*value));
            }
        }
    }

    const fn finish(self) -> u128 {
        ((self.0[0] as u128) << 64) | self.0[1] as u128
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_keys_sort_named_parameters_and_include_runtime() {
        let effect = EffectId::new("test".to_owned(), 1);
        let size = RenderSize::new(16, 9).expect("non-zero size");
        let first = EffectParams::new()
            .with("b", ParamValue::Float(2.0))
            .with("a", ParamValue::Float(1.0));
        let second = EffectParams::new()
            .with("a", ParamValue::Float(1.0))
            .with("b", ParamValue::Float(2.0));
        let input = [RenderCacheKey::source(7)];
        let key = effect_cache_key(
            b"effect",
            size,
            &effect,
            &input,
            &first,
            EffectRuntime::default(),
        );
        assert_eq!(
            key,
            effect_cache_key(
                b"effect",
                size,
                &effect,
                &input,
                &second,
                EffectRuntime::default(),
            )
        );
        assert_ne!(
            key,
            effect_cache_key(
                b"effect",
                size,
                &effect,
                &input,
                &second,
                EffectRuntime {
                    time: alpha2_time::MediaTime::from_ticks(1),
                    ..EffectRuntime::default()
                },
            )
        );
    }
}

//! The delivered frame size, its presentation on the CPU renderer and the `luma8` conversion,
//! computed alike on every surface.

use std::sync::Arc;

use alpha2_media::DecodedVideoFrame;
use alpha2_render::{
    BlendMode, ColorPolicy, CpuRenderer, GraphLayer, LayerTransform, RenderGraph, RenderSize,
    SourceView,
};

use super::SuppliedFrame;

/// The size a frame of displayed size `(width, height)` is delivered at for a request `size`:
/// contain-fit so the long edge is at most `size`, never upscaled, each edge rounded half up and
/// at least one pixel.
#[must_use]
pub fn delivered_size(displayed: (u32, u32), size: u32) -> (u32, u32) {
    let (width, height) = displayed;
    let long = width.max(height);
    if long == 0 || long <= size {
        return (width.max(1), height.max(1));
    }
    // floor(edge * size / long + 1/2) in integers: (2 * edge * size + long) / (2 * long).
    let edge = |value: u32| {
        let scaled =
            (2 * u64::from(value) * u64::from(size) + u64::from(long)) / (2 * u64::from(long));
        u32::try_from(scaled).unwrap_or(u32::MAX).max(1)
    };
    (edge(width), edge(height))
}

/// BT.709 Y' of straight-alpha sRGB-encoded RGBA8 pixels, rounded half up; alpha is ignored.
#[must_use]
pub fn luma8_from_rgba8(rgba: &[u8]) -> Vec<u8> {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .map(|pixel| {
            let weighted = 2_126 * u32::from(pixel[0])
                + 7_152 * u32::from(pixel[1])
                + 722 * u32::from(pixel[2]);
            u8::try_from((weighted + 5_000) / 10_000).unwrap_or(u8::MAX)
        })
        .collect()
}

/// The mixer's mono fold of `planes`, whose speakers are `speakers`. Each plane is weighted by the
/// mean of its stereo weights and summed in plane order. `None` for no planes or too many.
#[must_use]
pub fn mono_downmix(speakers: &[alpha2_media::Speaker], planes: &[Vec<f32>]) -> Option<Vec<f32>> {
    let mix = crate::downmix::Downmix::new(speakers, alpha2_audio::ChannelLayout::Mono)?;
    let frames = planes.first().map_or(0, Vec::len);
    let mut out = vec![0.0_f32; frames];
    for (plane, weight) in mix.channel(0).terms() {
        let samples = planes.get(*plane)?;
        for (sample, value) in out.iter_mut().zip(samples) {
            *sample += value * weight;
        }
    }
    Some(out)
}

/// The displayed size of a decoded frame, its visible rectangle widened by the pixel aspect ratio
/// (rounded half up) and turned upright.
#[must_use]
pub fn displayed_size(frame: &DecodedVideoFrame) -> (u32, u32) {
    let sar = frame.sample_aspect_ratio;
    let (num, den) = (u64::from(sar.num.max(1)), u64::from(sar.den.max(1)));
    let width = (2 * u64::from(frame.visible_rect.width) * num + den) / (2 * den);
    let width = u32::try_from(width).unwrap_or(u32::MAX).max(1);
    let height = frame.visible_rect.height.max(1);
    if frame.orientation.rotation.swaps_axes() {
        (height, width)
    } else {
        (width, height)
    }
}

/// A decoded frame presented on the CPU renderer as an extraction delivery: display-oriented,
/// contain-fit to at most `size` on the long edge, straight-alpha sRGB RGBA8. HDR is tone-mapped
/// with the default tone map.
///
/// # Errors
/// A frame the renderer cannot present.
pub fn present_for_extraction(
    renderer: &CpuRenderer,
    frame: Arc<DecodedVideoFrame>,
    size: u32,
) -> Result<SuppliedFrame, String> {
    let (width, height) = delivered_size(displayed_size(&frame), size);
    let target = RenderSize::new(width, height).map_err(|error| error.to_string())?;
    let pts = frame.pts;
    let source = alpha2_render::decoded::present_decoded(
        renderer,
        target,
        ColorPolicy::ToneMapHdr(alpha2_render::hdr::HdrToneMap::default()),
        frame,
    )?;
    let mut graph = RenderGraph::new(target);
    let node = graph.add_source(SourceView::from(&source));
    let output = graph
        .add_composite(vec![GraphLayer {
            source: node,
            visual: LayerTransform {
                position: [0.5, 0.5],
                anchor: [0.5, 0.5],
                scale: [1.0, 1.0],
                rotation_degrees: 0.0,
                opacity: 1.0,
            },
            blend_mode: BlendMode::Normal,
        }])
        .map_err(|error| error.to_string())?;
    graph
        .set_output(output)
        .map_err(|error| error.to_string())?;
    let image = renderer
        .execute_graph(&graph)
        .map_err(|error| error.to_string())?;
    Ok(SuppliedFrame {
        pts,
        width,
        height,
        rgba: unpremultiply(image.read_rgba8()),
    })
}

/// Straight alpha from premultiplied RGBA8, rounded half up.
fn unpremultiply(mut rgba: Vec<u8>) -> Vec<u8> {
    for pixel in rgba.as_chunks_mut::<4>().0 {
        let alpha = u32::from(pixel[3]);
        if alpha == 0 {
            pixel[..3].fill(0);
        } else if alpha < 255 {
            for channel in &mut pixel[..3] {
                *channel =
                    u8::try_from(((u32::from(*channel) * 255 * 2 + alpha) / (2 * alpha)).min(255))
                        .unwrap_or(u8::MAX);
            }
        }
    }
    rgba
}

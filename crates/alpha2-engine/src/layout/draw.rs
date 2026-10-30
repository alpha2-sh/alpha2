//! A prepared layout drawn into the render graph: one composite of its
//! paint list over transparency, as a text generator's picture is.

use super::paint::{PaintItem, Placement};
use crate::TextTexture;
use alpha2_render::{
    BlendMode, GraphError, GraphLayer, LayerTransform, RenderError, RenderGraph, RenderNodeId,
    RenderSize,
};
use std::sync::Arc;

/// A layout resolved and drawn at one render size: its paint list, and the
/// store key of each text node's texture (`None` for empty text).
///
/// A host's [`crate::TextTextures`] prepares it, and lends it to the graph
/// through [`crate::FrameProvider::layout`].
#[derive(Debug)]
pub struct PreparedLayout {
    pub(crate) items: Vec<PaintItem>,
    pub(crate) texts: Vec<Option<u64>>,
}

/// A prepared layout with the textures its text nodes draw.
#[derive(Clone, Debug)]
pub(crate) struct LaidOut<'t> {
    pub(crate) layout: Arc<PreparedLayout>,
    pub(crate) texts: Vec<Option<TextTexture<'t>>>,
}

impl<'t> LaidOut<'t> {
    /// `layout` with its textures from `frames`, or `None` when one is
    /// missing.
    pub(crate) fn attach(
        layout: Arc<PreparedLayout>,
        frames: &'t dyn crate::FrameProvider,
    ) -> Option<Self> {
        let texts = layout
            .texts
            .iter()
            .map(|key| {
                key.map(|key| frames.text(key))
                    .map_or(Some(None), |text| text.map(Some))
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self { layout, texts })
    }

    /// The layout's picture: its paint list composited over transparency.
    pub(crate) fn add_to(&self, graph: &mut RenderGraph<'t>) -> Result<RenderNodeId, RenderError> {
        let layers = self.layers(graph, &self.layout.items)?;
        graph
            .add_composite(layers)
            .map_err(GraphError::into_render_error)
    }

    fn layers(
        &self,
        graph: &mut RenderGraph<'t>,
        items: &[PaintItem],
    ) -> Result<Vec<GraphLayer>, RenderError> {
        let size = graph.size();
        let canvas = [size.width, size.height];
        let mut layers = Vec::with_capacity(items.len());
        for item in items {
            let (source, place, extent) = match item {
                PaintItem::Vector { scene, place } => {
                    (graph.add_vector(Arc::clone(scene)), *place, canvas)
                }
                PaintItem::Text { index, place } => {
                    let texture = self
                        .texts
                        .get(*index)
                        .copied()
                        .flatten()
                        .ok_or(RenderError::MissingText)?;
                    let visual = layer_transform(*place, [texture.width, texture.height], size);
                    layers.push(crate::frame::text_layer(graph, texture, visual)?);
                    continue;
                }
                PaintItem::Group { items, clip, place } => {
                    let inner = self.layers(graph, items)?;
                    let mut node = graph
                        .add_composite(inner)
                        .map_err(GraphError::into_render_error)?;
                    if let Some(clip) = clip {
                        let coverage = graph.add_vector(Arc::clone(clip));
                        node = crate::frame::coverage_mask(
                            graph,
                            node,
                            &[(coverage, false, alpha2_model::MaskMode::Add)],
                        )?;
                    }
                    (node, *place, canvas)
                }
            };
            layers.push(GraphLayer {
                source,
                visual: layer_transform(place, extent, size),
                blend_mode: BlendMode::Normal,
            });
        }
        Ok(layers)
    }
}

/// `place` as the compositor's layer transform, for a source `extent`
/// pixels in size drawn 1:1 on a picture of `size`.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn layer_transform(place: Placement, extent: [u32; 2], size: RenderSize) -> LayerTransform {
    let output = [f64::from(size.width), f64::from(size.height)];
    let extent = extent.map(f64::from);
    LayerTransform {
        position: [
            (place.position[0] / output[0]) as f32,
            (place.position[1] / output[1]) as f32,
        ],
        anchor: [
            (place.anchor[0] / extent[0]) as f32,
            (place.anchor[1] / extent[1]) as f32,
        ],
        scale: [
            (place.scale[0] * extent[0] / output[0]) as f32,
            (place.scale[1] * extent[1] / output[1]) as f32,
        ],
        rotation_degrees: place.rotation_degrees as f32,
        opacity: place.opacity,
    }
}

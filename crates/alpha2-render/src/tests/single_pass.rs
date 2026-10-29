//! Test shortcuts that draw one effect, transition, blur or presented source.
//!
//! Each builds the one-node render graph or the source preparation a project
//! frame uses and runs it, so a test reaches the product path without
//! building a graph at every call site. They take `wgpu` views because the
//! tests make their fixtures as raw textures.

use super::*;
use crate::builtins::GAUSSIAN_BLUR_ID;
use crate::{EffectId, EffectInputs, EffectResources};

impl Renderer {
    /// Draws a registered effect or transition over `source_a` (and
    /// `source_b` for a transition), its parameters' defaults filling the
    /// rest.
    pub(crate) fn render_effect(
        &self,
        id: &EffectId,
        source_a: &wgpu::TextureView,
        source_b: Option<&wgpu::TextureView>,
        size: RenderSize,
        params: &EffectParams,
        runtime: EffectRuntime,
    ) -> Result<RenderedTexture, RenderError> {
        self.render_effect_with_resources(
            id,
            source_a,
            source_b,
            size,
            params,
            &EffectResources::new(),
            runtime,
        )
    }

    /// [`Self::render_effect`] with named external resources.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_effect_with_resources(
        &self,
        id: &EffectId,
        source_a: &wgpu::TextureView,
        source_b: Option<&wgpu::TextureView>,
        size: RenderSize,
        params: &EffectParams,
        resources: &EffectResources,
        runtime: EffectRuntime,
    ) -> Result<RenderedTexture, RenderError> {
        let definition = self
            .effect_definition(id)
            .ok_or(RenderError::UnknownEffect)?;
        let (kind, inputs) = (definition.kind, definition.inputs.clone());
        let mut graph = RenderGraph::new(size);
        let source_a = graph.add_source(SourceView::from_wgpu(source_a));
        let output = match kind {
            // An effect's named inputs all show `source_b`, or else the source itself.
            EffectKind::Effect => {
                let shown = match source_b {
                    Some(view) => graph.add_source(SourceView::from_wgpu(view)),
                    None => source_a,
                };
                let named = inputs.iter().fold(EffectInputs::new(), |named, input| {
                    named.with(input.name.clone(), shown)
                });
                graph
                    .add_effect_with_inputs(
                        id.clone(),
                        source_a,
                        named,
                        params.clone(),
                        resources.clone(),
                        runtime,
                    )
                    .map_err(GraphError::into_render_error)?
            }
            EffectKind::Transition => {
                let source_b = source_b.ok_or(RenderError::MissingEffectInput)?;
                let source_b = graph.add_source(SourceView::from_wgpu(source_b));
                graph
                    .add_transition_with_resources(
                        id.clone(),
                        source_a,
                        source_b,
                        params.clone(),
                        resources.clone(),
                        runtime,
                    )
                    .map_err(GraphError::into_render_error)?
            }
            EffectKind::Generator => return Err(RenderError::InvalidGraph),
        };
        graph
            .set_output(output)
            .map_err(GraphError::into_render_error)?;
        self.execute_graph(&graph)
            .map_err(GraphError::into_render_error)
    }

    /// Draws the built-in Gaussian blur, `radius` a fraction of the output's
    /// shorter side, as a project's effect at the source's aspect ratio does. Call
    /// [`Renderer::register_builtin_gaussian_blur`] first.
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn render_gaussian_blur(
        &self,
        source: &wgpu::TextureView,
        size: RenderSize,
        radius: f32,
    ) -> Result<RenderedTexture, RenderError> {
        self.render_effect(
            &EffectId::from_static(GAUSSIAN_BLUR_ID, 1),
            source,
            None,
            size,
            &EffectParams::new().with("radius", ParamValue::Float(radius)),
            EffectRuntime {
                ratio: size.width as f32 / size.height as f32,
                ..EffectRuntime::default()
            },
        )
    }

    /// Presents an RGBA texture into `presentation`'s output size, as a
    /// decoded RGBA source frame is, and submits the presentation at once.
    pub(crate) fn render_rgba_presented(
        &self,
        source: &wgpu::Texture,
        source_size: RenderSize,
        presentation: SourcePresentation,
        alpha: crate::SourceAlpha,
    ) -> Result<RenderedTexture, RenderError> {
        let prepared = self.prepare_rgba_presented_inner(
            &crate::pool::default_view(source),
            source_size,
            presentation,
            crate::yuv::PresentedEncoding::Srgb(alpha),
            Vec::new(),
        )?;
        let (texture, pending) = prepared.into_parts();
        if let Some(pending) = pending {
            self.submit_pending_source(&pending);
        }
        Ok(texture)
    }
}

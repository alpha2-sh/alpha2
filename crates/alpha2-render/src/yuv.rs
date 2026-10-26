use crate::hdr::HdrToneMap;
use crate::{
    Color, ColorDiagnostic, ColorPolicy, RenderError, RenderSize, RenderedTexture, Renderer,
    pipeline::{CachedSampledPipeline, CachedYuvPipeline},
};
use alpha2_media::{
    ChromaLocation, ColorMatrix, ColorMetadata, ColorPrimaries, ColorRange, Orientation, Rect,
    TransferFunction, YuvFormat,
};
use alpha2_shaders::{RGBA_PRESENT_WGSL, YUV_TO_LINEAR_WGSL, YUV16_TO_LINEAR_WGSL};
use std::fmt::Debug;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::pool::UploadTexture;
use crate::uniform::UniformArenaLease;

/// A rendered source texture paired with presentation work that can be folded
/// into the command encoder of its first consumer.
#[derive(Debug)]
pub struct PreparedSourceTexture {
    texture: RenderedTexture,
    pending: Option<PendingSourceWork>,
}

impl PreparedSourceTexture {
    /// Separates the sampleable output from its one-shot presentation work.
    #[must_use]
    pub fn into_parts(self) -> (RenderedTexture, Option<PendingSourceWork>) {
        (self.texture, self.pending)
    }

    /// Returns the maximum GPU texture bytes retained until presentation.
    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        texture_bytes(self.texture.size, self.texture.format).saturating_add(
            self.pending
                .as_ref()
                .map_or(0, PendingSourceWork::retained_bytes),
        )
    }
}

/// One source-presentation pass waiting to be encoded by its first consumer.
#[derive(Debug)]
pub struct PendingSourceWork {
    state: Mutex<Option<SourceWork>>,
    retained_bytes: u64,
    submitted: AtomicBool,
}

#[derive(Debug)]
pub(crate) struct SourceWork {
    pub(crate) label: &'static str,
    /// Whether the pass converts color into the working space, which a
    /// plane-for-plane repack does not.
    pub(crate) conversion: bool,
    pub(crate) pipeline: wgpu::RenderPipeline,
    pub(crate) bind_group: wgpu::BindGroup,
    pub(crate) output_view: wgpu::TextureView,
    pub(crate) uniforms: UniformArenaLease,
    pub(crate) uploads: Vec<UploadTexture>,
    pub(crate) external_leases: Vec<Arc<dyn ExternalTextureLease>>,
}

/// Keeps an imported native texture and its decoder surface alive until the
/// queue has completed the submission that samples it.
pub trait ExternalTextureLease: Debug + Send + Sync + 'static {
    /// Approximate native allocation bytes retained by this lease.
    fn retained_bytes(&self) -> u64;

    /// Stages native waits or signals on the renderer's next queue submission.
    fn prepare_submission(&self) -> bool {
        true
    }

    /// Flushes any native work accumulated by `prepare_submission`.
    ///
    /// # Errors
    /// The error the frame fails with, normally
    /// [`RenderError::ExternalTextureSync`] carrying what the native
    /// synchronization's status said about the device.
    fn flush_submission(&self) -> Result<(), RenderError> {
        Ok(())
    }

    /// Called immediately after the command buffer that uses the resource is submitted.
    fn submitted(&self) {}

    /// Native signals this lease has already staged on the renderer's queue.
    ///
    /// Staging is idempotent per frame and cleared when the submission it
    /// belongs to completes, so a lease that still carries one when the next
    /// frame begins staging has leaked a signal from the previous frame onto
    /// this one's submission. Only the debug assertion in
    /// `stage_source_submissions` reads this.
    fn staged_signals(&self) -> usize {
        0
    }
}

/// Native leases whose submission has completed, waiting to be dropped.
///
/// Parking them keeps `FFmpeg` frame release out of `wgpu`'s queue submission,
/// where the shared-queue lock is held. See [`Renderer::submit`].
#[derive(Debug, Default)]
pub(crate) struct RetiredLeases {
    state: Mutex<LeaseState>,
    /// Notified by each completion that parks leases, on whichever thread
    /// runs it.
    pub(crate) parks: crate::settle::CallbackSignal,
    /// Test-only: each completion parks its leases from a thread of its own
    /// this long after its callback ran, as one another thread's poll
    /// collected does.
    #[cfg(test)]
    pub(crate) park_elsewhere_after: Mutex<Option<std::time::Duration>>,
}

#[derive(Debug, Default)]
struct LeaseState {
    parked: Vec<Arc<dyn ExternalTextureLease>>,
    /// Submissions whose completion has yet to park their leases.
    awaited: usize,
}

impl RetiredLeases {
    fn lock(&self) -> std::sync::MutexGuard<'_, LeaseState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Counts a submission whose completion will park leases.
    fn await_completion(&self) {
        self.lock().awaited += 1;
    }

    fn park(&self, leases: Vec<Arc<dyn ExternalTextureLease>>) {
        {
            let mut state = self.lock();
            state.parked.extend(leases);
            state.awaited = state.awaited.saturating_sub(1);
        }
        self.parks.notify();
    }

    /// How many submitted completions have not parked their leases yet.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn awaited(&self) -> usize {
        self.lock().awaited
    }

    /// Drops every parked lease and reports how many there were.
    ///
    /// Called only outside a queue submission.
    pub(crate) fn drain(&self) -> usize {
        let leases = std::mem::take(&mut self.lock().parked);
        let released = leases.len();
        drop(leases);
        released
    }
}

pub(crate) struct SourceWorkTicket<'a> {
    owner: &'a PendingSourceWork,
    work: Option<SourceWork>,
}

impl PendingSourceWork {
    pub(crate) fn new(work: SourceWork) -> Self {
        let retained_bytes = work
            .uploads
            .iter()
            .fold(0_u64, |total, input| {
                total.saturating_add(input.retained_bytes())
            })
            .saturating_add(work.external_leases.iter().fold(0_u64, |total, lease| {
                total.saturating_add(lease.retained_bytes())
            }));
        Self {
            state: Mutex::new(Some(work)),
            retained_bytes,
            submitted: AtomicBool::new(false),
        }
    }

    /// Returns bytes retained only until the pending pass is submitted.
    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        if self.submitted.load(Ordering::Acquire) {
            0
        } else {
            self.retained_bytes
        }
    }

    /// Takes the work, unless a frame already has; see
    /// [`crate::graph_submit::begin_source_work`] for why only under the
    /// frame lock.
    pub(crate) fn begin(
        &self,
        _frame: &crate::renderer::FrameGuard,
    ) -> Option<SourceWorkTicket<'_>> {
        let work = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()?;
        Some(SourceWorkTicket {
            owner: self,
            work: Some(work),
        })
    }
}

impl SourceWorkTicket<'_> {
    pub(crate) fn encode(&self, encoder: &mut wgpu::CommandEncoder) {
        let work = self.work.as_ref().expect("live source work ticket");
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(work.label),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &work.output_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&work.pipeline);
        pass.set_bind_group(0, &work.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }

    pub(crate) fn flush_uniforms(&self, queue: &wgpu::Queue) {
        self.work
            .as_ref()
            .expect("live source work ticket")
            .uniforms
            .flush(queue);
    }

    /// Whether no lease in this ticket has a signal staged yet.
    pub(crate) fn has_no_staged_signals(&self) -> bool {
        self.work.as_ref().is_none_or(|work| {
            work.external_leases
                .iter()
                .all(|lease| lease.staged_signals() == 0)
        })
    }

    pub(crate) fn prepare_submission(&self) -> bool {
        let work = self.work.as_ref().expect("live source work ticket");
        work.external_leases
            .iter()
            .all(|lease| lease.prepare_submission())
    }

    pub(crate) fn flush_submission(&self) -> Result<(), RenderError> {
        let work = self.work.as_ref().expect("live source work ticket");
        work.external_leases
            .iter()
            .try_for_each(|lease| lease.flush_submission())
    }

    pub(crate) fn complete(mut self, renderer: &Renderer, standalone_submission: bool) {
        let work = self.work.take().expect("live source work ticket");
        let work_conversion = work.conversion;
        work.uniforms.recycle();
        drop(work.uploads);
        if !work.external_leases.is_empty() {
            let leases = work.external_leases;
            for lease in &leases {
                lease.submitted();
            }
            // The callback runs inside `wgpu`'s submission, so it only parks the leases:
            // dropping them here inverts `FFmpeg`'s frame lock order and deadlocks.
            let retired = Arc::clone(&renderer.retired_leases);
            retired.await_completion();
            #[cfg(test)]
            let elsewhere = *retired
                .park_elsewhere_after
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            renderer.gpu.on_submitted_work_done(move || {
                #[cfg(test)]
                if let Some(delay) = elsewhere {
                    std::thread::spawn(move || {
                        std::thread::sleep(delay);
                        retired.park(leases);
                    });
                    return;
                }
                retired.park(leases);
            });
        }
        self.owner.submitted.store(true, Ordering::Release);
        crate::diagnostics::record_source_work(
            &renderer.diagnostics,
            1,
            standalone_submission,
            work_conversion,
        );
    }
}

impl Drop for SourceWorkTicket<'_> {
    fn drop(&mut self) {
        let Some(work) = self.work.take() else {
            return;
        };
        let mut state = self
            .owner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert!(state.is_none());
        *state = Some(work);
    }
}

/// How an RGBA source texture stores color relative to its alpha.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SourceAlpha {
    /// Color independent of alpha, as decoders and `copyExternalImageToTexture`
    /// without premultiplication deliver it. Presentation premultiplies it.
    Straight,
    /// Color already multiplied by alpha, as a browser 2D canvas stores it.
    Premultiplied,
}

/// How the RGBA presentation pass reads its source texture.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum PresentedEncoding {
    /// sRGB-encoded color, straight or premultiplied.
    Srgb(SourceAlpha),
    /// Premultiplied linear-light color in BT.709 primaries, encoded with the
    /// sRGB curve extended to values above `1.0` and below `0.0`.
    Linear,
}

/// GPU presentation transform applied while converting a decoded source.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SourcePresentation {
    /// Destination canvas. Pixels outside the contained source are transparent.
    pub output_size: RenderSize,
    /// Visible crop inside the coded source.
    pub visible_rect: Rect,
    /// How the source is shown upright: mirrored, then rotated.
    pub orientation: Orientation,
    /// Horizontal pixel aspect-ratio numerator and denominator.
    pub sample_aspect_ratio: (u32, u32),
    /// Chroma sample position. An unspecified location is sampled as centered.
    pub chroma_siting: ChromaLocation,
}

impl SourcePresentation {
    /// Creates an unrotated, square-pixel full-frame presentation.
    #[must_use]
    pub const fn identity(size: RenderSize) -> Self {
        Self {
            output_size: size,
            visible_rect: Rect {
                x: 0,
                y: 0,
                width: size.width,
                height: size.height,
            },
            orientation: Orientation::UPRIGHT,
            sample_aspect_ratio: (1, 1),
            chroma_siting: ChromaLocation::Unspecified,
        }
    }
}

/// Borrowed 8-bit planar 4:2:0 source.
#[derive(Copy, Clone, Debug)]
pub struct Yuv420Frame<'a> {
    /// Luma bytes including row padding.
    pub y: &'a [u8],
    /// Blue-difference chroma bytes including row padding.
    pub u: &'a [u8],
    /// Red-difference chroma bytes including row padding.
    pub v: &'a [u8],
    /// Luma row stride.
    pub y_stride: u32,
    /// Chroma row stride shared by U and V.
    pub chroma_stride: u32,
    /// Coded dimensions.
    pub size: RenderSize,
    /// Straight source alpha.
    pub alpha: f32,
    /// Optional stream color metadata.
    pub color: Option<ColorMetadata>,
}

impl<'a> From<Yuv420Frame<'a>> for YuvFrame<'a> {
    fn from(frame: Yuv420Frame<'a>) -> Self {
        Self {
            format: YuvFormat::I420,
            planes: [
                SourcePlane {
                    bytes: frame.y,
                    stride: frame.y_stride,
                },
                SourcePlane {
                    bytes: frame.u,
                    stride: frame.chroma_stride,
                },
                SourcePlane {
                    bytes: frame.v,
                    stride: frame.chroma_stride,
                },
            ],
            alpha_plane: None,
            size: frame.size,
            alpha: frame.alpha,
            color: frame.color,
        }
    }
}

/// One borrowed source plane: rows `stride` bytes apart.
#[derive(Copy, Clone, Debug)]
pub struct SourcePlane<'a> {
    /// Plane bytes including row padding.
    pub bytes: &'a [u8],
    /// Bytes between adjacent rows.
    pub stride: u32,
}

/// Borrowed planar or semi-planar YCbCr source of any subsampling and depth
/// [`YuvFormat::is_supported`] accepts.
#[derive(Copy, Clone, Debug)]
pub struct YuvFrame<'a> {
    /// How the planes store their samples.
    pub format: YuvFormat,
    /// Luma, then U and V, or luma and the interleaved chroma plane, in
    /// which case the third plane is ignored.
    pub planes: [SourcePlane<'a>; 3],
    /// Straight, full-range alpha at the luma's size and depth: present
    /// exactly when [`YuvFormat::alpha`] is set.
    pub alpha_plane: Option<SourcePlane<'a>>,
    /// Coded dimensions.
    pub size: RenderSize,
    /// Straight source alpha.
    pub alpha: f32,
    /// Optional stream color metadata.
    pub color: Option<ColorMetadata>,
}

/// How an uploaded plane stores samples on the GPU.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum PlaneStorage {
    /// 8-bit samples as `R8Unorm`/`Rg8Unorm`.
    Unorm8,
    /// 16-bit containers as `R16Unorm`/`Rg16Unorm`.
    Unorm16,
    /// 16-bit containers as they are, in `R16Uint`/`Rg16Uint`, for a device
    /// without 16-bit normalized textures (browser WebGPU never has them).
    /// The conversion's `u16_planes` build filters and normalizes them, so
    /// no sample is touched on the CPU.
    Uint16,
}

impl PlaneStorage {
    fn texture_format(self, interleaved: bool) -> wgpu::TextureFormat {
        match (self, interleaved) {
            (Self::Unorm8, false) => wgpu::TextureFormat::R8Unorm,
            (Self::Unorm8, true) => wgpu::TextureFormat::Rg8Unorm,
            (Self::Unorm16, false) => wgpu::TextureFormat::R16Unorm,
            (Self::Unorm16, true) => wgpu::TextureFormat::Rg16Unorm,
            (Self::Uint16, false) => wgpu::TextureFormat::R16Uint,
            (Self::Uint16, true) => wgpu::TextureFormat::Rg16Uint,
        }
    }

    /// Factor that turns a sampled value into `code / (2^depth - 1)`.
    ///
    /// A `u16` container samples as `container / 65535`, normalized by the
    /// texture or by the conversion itself: a low-aligned 10-bit code
    /// reaches only `1023 / 65535` of full scale, and `P010`'s high-aligned
    /// one `65472 / 65535`.
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn code_scale(self, format: YuvFormat) -> f32 {
        match self {
            Self::Unorm8 => 1.0,
            Self::Unorm16 | Self::Uint16 => {
                let maximum = ((1_u32 << format.bit_depth) - 1) as f32;
                let alignment = if format.msb_aligned {
                    (1_u32 << (16 - u32::from(format.bit_depth))) as f32
                } else {
                    1.0
                };
                65535.0 / (maximum * alignment)
            }
        }
    }
}

/// Borrowed views of a decoded YCbCr frame already resident on the GPU.
#[derive(Copy, Clone, Debug)]
#[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
pub struct GpuVideoPlanes<'a> {
    /// Luma view.
    pub y: &'a wgpu::TextureView,
    /// U view, or the interleaved UV view.
    pub chroma: &'a wgpu::TextureView,
    /// V view; absent when chroma is interleaved.
    pub v: Option<&'a wgpu::TextureView>,
    /// How the views store their samples: `R8`/`RG8` for eight bits,
    /// `R16`/`RG16` normalized containers above.
    pub format: YuvFormat,
    /// Coded dimensions.
    pub size: RenderSize,
    /// Straight source alpha.
    pub alpha: f32,
    /// Optional stream color metadata.
    pub color: Option<ColorMetadata>,
}

impl Renderer {
    /// Prepares an uploaded RGBA texture for presentation by its first frame
    /// consumer, without issuing a queue submission.
    ///
    /// # Errors
    /// Returns an image-data error for invalid presentation metadata.
    pub fn prepare_rgba_presented(
        &self,
        source: UploadTexture,
        source_size: RenderSize,
        presentation: SourcePresentation,
        alpha: SourceAlpha,
    ) -> Result<PreparedSourceTexture, RenderError> {
        let view = source.view().clone();
        let prepared = self.prepare_rgba_presented_inner(
            &view,
            source_size,
            presentation,
            PresentedEncoding::Srgb(alpha),
            vec![source],
        )?;
        Ok(prepared)
    }

    /// Prepares an uploaded texture of premultiplied linear-light RGBA in
    /// BT.709 primaries ([`Self::upload_linear_rgba16f`]) for presentation
    /// like [`Self::prepare_rgba_presented`]. It is filtered in linear light
    /// and stored like every presented source, sRGB-encoded and
    /// premultiplied, the curve extended to keep values above `1.0` and below
    /// `0.0` as far as the intermediate format holds them. Color with zero
    /// alpha is not kept.
    ///
    /// # Errors
    /// Returns an image-data error for invalid presentation metadata.
    pub fn prepare_linear_presented(
        &self,
        source: UploadTexture,
        source_size: RenderSize,
        presentation: SourcePresentation,
    ) -> Result<PreparedSourceTexture, RenderError> {
        let view = source.view().clone();
        self.prepare_rgba_presented_inner(
            &view,
            source_size,
            presentation,
            PresentedEncoding::Linear,
            vec![source],
        )
    }

    /// Uploads a straight-alpha RGBA8 plane into a pooled texture and
    /// prepares its presentation like [`Self::prepare_rgba_presented`].
    ///
    /// # Errors
    /// Returns an image-data error for a plane shorter than `size` or invalid
    /// presentation metadata.
    pub fn prepare_rgba8_presented(
        &self,
        pixels: SourcePlane<'_>,
        size: RenderSize,
        presentation: SourcePresentation,
    ) -> Result<PreparedSourceTexture, RenderError> {
        let texture = self.write_rgba8(size, pixels)?;
        self.prepare_rgba_presented(texture, size, presentation, SourceAlpha::Straight)
    }

    pub(crate) fn prepare_rgba_presented_inner(
        &self,
        source_view: &wgpu::TextureView,
        source_size: RenderSize,
        presentation: SourcePresentation,
        encoding: PresentedEncoding,
        inputs: Vec<UploadTexture>,
    ) -> Result<PreparedSourceTexture, RenderError> {
        self.validate_size(source_size)?;
        self.validate_size(presentation.output_size)?;
        validate_presentation(source_size, presentation)?;
        let output_format = self.intermediate_format();
        let output = self.textures.acquire(
            self.gpu.device(),
            presentation.output_size,
            output_format,
            "alpha2 RGBA presentation target",
        );
        let cached = self.rgba_presentation_pipeline();
        let uniform_bytes = presentation_uniform_bytes(
            source_size,
            presentation,
            encoding,
            self.presentation_filter_samples(),
        );
        let mut uniforms = self.uniform_buffers.acquire(
            self.gpu.device(),
            u64::try_from(uniform_bytes.len()).unwrap_or(u64::MAX),
        );
        let uniform = uniforms.write(&uniform_bytes);
        let bind_group = self
            .gpu
            .device()
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("alpha2 RGBA presentation bind group"),
                layout: &cached.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(source_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&cached.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: uniforms.binding(uniform),
                    },
                ],
            });
        let output_view = output.view().clone();
        Ok(PreparedSourceTexture {
            texture: RenderedTexture::pooled(presentation.output_size, output_format, output),
            pending: Some(PendingSourceWork::new(SourceWork {
                label: "alpha2 RGBA presentation pass",
                conversion: true,
                pipeline: cached.pipeline.clone(),
                bind_group,
                output_view,
                uniforms,
                uploads: inputs,
                external_leases: Vec::new(),
            })),
        })
    }

    /// Submits one pending source presentation immediately.
    ///
    /// Returns `false` when another consumer already submitted the work, and
    /// also when the source's lease cannot stage its native synchronization or
    /// the submission fails: the work then stays pending, and a later
    /// consumer's frame submits it.
    pub fn submit_pending_source(&self, pending: &PendingSourceWork) -> bool {
        let frame = self.frame_lock.hold();
        let Some(ticket) = pending.begin(&frame) else {
            return false;
        };
        let mut encoder = self.command_encoder("alpha2 source presentation encoder");
        ticket.encode(&mut encoder);
        ticket.flush_uniforms(self.gpu.queue());
        if !ticket.prepare_submission() || ticket.flush_submission().is_err() {
            return false;
        }
        if self.submit([encoder.finish()]).is_err() {
            return false;
        }
        ticket.complete(self, true);
        true
    }

    /// Uploads YUV planes and converts them into premultiplied sRGB storage
    /// on the GPU, ready for effects, layer composition, and presentation.
    /// Transfer decoding happens before sRGB encoding and premultiplication.
    ///
    /// # Errors
    /// Returns an image-data error for malformed planes or alpha values, and
    /// [`RenderError::UnsupportedHdr`] when [`ColorPolicy::RejectHdr`]
    /// meets an HDR source (the other policies tone-map it or skip it, and
    /// report which in the diagnostics).
    #[allow(clippy::too_many_lines)]
    pub fn render_yuv420p(
        &self,
        frame: Yuv420Frame<'_>,
        policy: ColorPolicy,
    ) -> Result<(RenderedTexture, Vec<ColorDiagnostic>), RenderError> {
        self.render_yuv420p_presented(frame, SourcePresentation::identity(frame.size), policy)
    }

    fn rgba_presentation_pipeline(&self) -> &CachedSampledPipeline {
        self.pipelines.rgba_present.get_or_init(|| {
            let output_format = self.intermediate_format();
            let bind_group_layout =
                self.gpu
                    .device()
                    .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                        label: Some("alpha2 RGBA presentation layout"),
                        entries: &[
                            wgpu::BindGroupLayoutEntry {
                                binding: 0,
                                visibility: wgpu::ShaderStages::FRAGMENT,
                                ty: wgpu::BindingType::Texture {
                                    sample_type: wgpu::TextureSampleType::Float {
                                        filterable: true,
                                    },
                                    view_dimension: wgpu::TextureViewDimension::D2,
                                    multisampled: false,
                                },
                                count: None,
                            },
                            wgpu::BindGroupLayoutEntry {
                                binding: 1,
                                visibility: wgpu::ShaderStages::FRAGMENT,
                                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                                count: None,
                            },
                            wgpu::BindGroupLayoutEntry {
                                binding: 2,
                                visibility: wgpu::ShaderStages::FRAGMENT,
                                ty: wgpu::BindingType::Buffer {
                                    ty: wgpu::BufferBindingType::Uniform,
                                    has_dynamic_offset: false,
                                    min_binding_size: None,
                                },
                                count: None,
                            },
                        ],
                    });
            let sampler = self.gpu.device().create_sampler(&wgpu::SamplerDescriptor {
                label: Some("alpha2 RGBA presentation sampler"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            });
            let shader = self
                .gpu
                .device()
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("alpha2 RGBA presentation shader"),
                    source: wgpu::ShaderSource::Wgsl(RGBA_PRESENT_WGSL.into()),
                });
            let layout =
                self.gpu
                    .device()
                    .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                        label: Some("alpha2 RGBA presentation pipeline layout"),
                        bind_group_layouts: &[Some(&bind_group_layout)],
                        immediate_size: 0,
                    });
            let pipeline =
                self.gpu
                    .device()
                    .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                        label: Some("alpha2 RGBA presentation pipeline"),
                        layout: Some(&layout),
                        vertex: wgpu::VertexState {
                            module: &shader,
                            entry_point: Some("vs"),
                            buffers: &[],
                            compilation_options: wgpu::PipelineCompilationOptions::default(),
                        },
                        fragment: Some(wgpu::FragmentState {
                            module: &shader,
                            entry_point: Some("fs"),
                            targets: &[Some(output_format.into())],
                            compilation_options: wgpu::PipelineCompilationOptions::default(),
                        }),
                        primitive: wgpu::PrimitiveState::default(),
                        depth_stencil: None,
                        multisample: wgpu::MultisampleState::default(),
                        multiview_mask: None,
                        cache: None,
                    });
            CachedSampledPipeline {
                bind_group_layout,
                pipeline,
                sampler,
            }
        })
    }

    /// Uploads and converts YUV420P while applying crop, sample aspect ratio,
    /// rotation, scaling (bilinear, averaged over the footprint when it
    /// shrinks), and transparent contain placement on GPU.
    ///
    /// # Errors
    /// Returns an image-data error for malformed planes, presentation
    /// metadata or alpha values, and
    /// [`RenderError::UnsupportedHdr`] when [`ColorPolicy::RejectHdr`]
    /// meets an HDR source (the other policies tone-map it or skip it, and
    /// report which in the diagnostics).
    #[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
    pub fn render_yuv420p_presented(
        &self,
        frame: Yuv420Frame<'_>,
        presentation: SourcePresentation,
        policy: ColorPolicy,
    ) -> Result<(RenderedTexture, Vec<ColorDiagnostic>), RenderError> {
        let (prepared, diagnostics) =
            self.prepare_yuv420p_presented(frame, presentation, policy)?;
        let (texture, pending) = prepared.into_parts();
        if let Some(pending) = pending {
            self.submit_pending_source(&pending);
        }
        Ok((texture, diagnostics))
    }

    /// Uploads YUV420 planes and prepares their conversion for the command
    /// encoder of the first frame that consumes the result.
    ///
    /// # Errors
    /// Returns an image-data error for malformed input, and
    /// [`RenderError::UnsupportedHdr`] when [`ColorPolicy::RejectHdr`]
    /// meets an HDR source (the other policies tone-map it or skip it, and
    /// report which in the diagnostics).
    #[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
    pub fn prepare_yuv420p_presented(
        &self,
        frame: Yuv420Frame<'_>,
        presentation: SourcePresentation,
        policy: ColorPolicy,
    ) -> Result<(PreparedSourceTexture, Vec<ColorDiagnostic>), RenderError> {
        self.prepare_yuv_presented(frame.into(), presentation, policy)
    }

    /// Uploads planar or semi-planar YCbCr of any supported subsampling and
    /// depth, and prepares its conversion like
    /// [`Self::prepare_yuv420p_presented`].
    ///
    /// Samples above eight bits upload as 16-bit normalized textures, or,
    /// on a device without them, as the unsigned 16-bit containers they
    /// are, which the conversion normalizes on the GPU.
    ///
    /// # Errors
    /// Returns an image-data error for an unsupported format or malformed
    /// planes, and
    /// [`RenderError::UnsupportedHdr`] when [`ColorPolicy::RejectHdr`]
    /// meets an HDR source (the other policies tone-map it or skip it, and
    /// report which in the diagnostics).
    pub fn prepare_yuv_presented(
        &self,
        frame: YuvFrame<'_>,
        presentation: SourcePresentation,
        policy: ColorPolicy,
    ) -> Result<(PreparedSourceTexture, Vec<ColorDiagnostic>), RenderError> {
        let mut diagnostics = Vec::new();
        let color = frame.color.unwrap_or_else(|| {
            diagnostics.push(ColorDiagnostic::MissingMetadataFallback);
            ColorMetadata::UNDECLARED
        });
        let tone = match hdr_handling(color, policy, &mut diagnostics)? {
            HdrHandling::Sdr => None,
            HdrHandling::ToneMap(tone) => Some(tone),
            HdrHandling::Skip => {
                return self
                    .render_solid(presentation.output_size, Color::rgba(0.0, 0.0, 0.0, 0.0))
                    .map(|texture| {
                        (
                            PreparedSourceTexture {
                                texture,
                                pending: None,
                            },
                            diagnostics,
                        )
                    });
            }
        };
        let format = frame.format;
        if !frame.alpha.is_finite()
            || !(0.0..=1.0).contains(&frame.alpha)
            || !format.is_supported()
            || format.alpha != frame.alpha_plane.is_some()
        {
            return Err(RenderError::InvalidImageData);
        }
        self.validate_size(presentation.output_size)?;
        validate_presentation(frame.size, presentation)?;
        let storage = if format.bytes_per_sample() == 1 {
            PlaneStorage::Unorm8
        } else if self
            .features
            .contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM)
        {
            PlaneStorage::Unorm16
        } else {
            PlaneStorage::Uint16
        };
        let (chroma_width, chroma_height) = format.chroma_size(frame.size.width, frame.size.height);
        let pairs = if format.interleaved_chroma { 2 } else { 1 };
        let plane_specs = [
            ("alpha2 Y plane", frame.size.width, frame.size.height, 1_u32),
            ("alpha2 U plane", chroma_width, chroma_height, pairs),
            ("alpha2 V plane", chroma_width, chroma_height, 1),
        ];
        let mut uploads = Vec::with_capacity(format.plane_count());
        let alpha_spec = ("alpha2 A plane", frame.size.width, frame.size.height, 1_u32);
        for (plane, (label, width, height, samples)) in frame
            .planes
            .iter()
            .zip(plane_specs)
            .take(format.color_plane_count())
            .chain(frame.alpha_plane.iter().map(|plane| (plane, alpha_spec)))
        {
            let row_bytes = width
                .checked_mul(samples)
                .and_then(|row| row.checked_mul(format.bytes_per_sample()))
                .ok_or(RenderError::InvalidImageData)?;
            validate_plane(plane.bytes, plane.stride, row_bytes, height)?;
            uploads.push(upload_plane(
                self,
                label,
                *plane,
                (width, height),
                storage.texture_format(samples == 2),
            )?);
        }
        let mut views = uploads
            .iter()
            .map(|upload| upload.view().clone())
            .collect::<Vec<_>>();
        let alpha_view = format.alpha.then(|| views.pop()).flatten();
        let planes = GpuVideoPlanes {
            y: &views[0],
            chroma: &views[1],
            v: views.get(2),
            format,
            size: frame.size,
            alpha: frame.alpha,
            color: Some(color),
        };
        let prepared = self.prepare_gpu_yuv_presented_inner(
            planes,
            alpha_view.as_ref(),
            presentation,
            tone,
            storage,
            uploads,
            Vec::new(),
        )?;
        Ok((prepared, diagnostics))
    }

    /// Prepares conversion of native GPU video planes without copying them to CPU.
    ///
    /// The external lease is retained through queue completion. I420 accepts
    /// three R8 views and NV12 accepts an R8 luma plus an RG8 chroma view.
    ///
    /// # Errors
    /// Returns an image-data error for an invalid plane contract, and
    /// [`RenderError::UnsupportedHdr`] when [`ColorPolicy::RejectHdr`]
    /// meets an HDR source (the other policies tone-map it or skip it, and
    /// report which in the diagnostics).
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn prepare_gpu_yuv_presented(
        &self,
        planes: GpuVideoPlanes<'_>,
        presentation: SourcePresentation,
        policy: ColorPolicy,
        lease: Arc<dyn ExternalTextureLease>,
    ) -> Result<(PreparedSourceTexture, Vec<ColorDiagnostic>), RenderError> {
        let mut diagnostics = Vec::new();
        let color = planes.color.unwrap_or_else(|| {
            diagnostics.push(ColorDiagnostic::MissingMetadataFallback);
            ColorMetadata::UNDECLARED
        });
        let tone = match hdr_handling(color, policy, &mut diagnostics)? {
            HdrHandling::Sdr => None,
            HdrHandling::ToneMap(tone) => Some(tone),
            HdrHandling::Skip => {
                return self
                    .render_solid(presentation.output_size, Color::rgba(0.0, 0.0, 0.0, 0.0))
                    .map(|texture| {
                        (
                            PreparedSourceTexture {
                                texture,
                                pending: None,
                            },
                            diagnostics,
                        )
                    });
            }
        };
        let planes = GpuVideoPlanes {
            color: Some(color),
            ..planes
        };
        let storage = if planes.format.bytes_per_sample() == 1 {
            PlaneStorage::Unorm8
        } else {
            PlaneStorage::Unorm16
        };
        let prepared = self.prepare_gpu_yuv_presented_inner(
            planes,
            None,
            presentation,
            tone,
            storage,
            Vec::new(),
            vec![lease],
        )?;
        Ok((prepared, diagnostics))
    }

    /// Prepares the conversion of `planes`, stored as `storage`, with
    /// `alpha_plane` the straight alpha of a format that has one.
    #[allow(clippy::too_many_arguments)]
    fn prepare_gpu_yuv_presented_inner(
        &self,
        planes: GpuVideoPlanes<'_>,
        alpha_plane: Option<&wgpu::TextureView>,
        presentation: SourcePresentation,
        tone: Option<HdrToneMap>,
        storage: PlaneStorage,
        uploads: Vec<UploadTexture>,
        external_leases: Vec<Arc<dyn ExternalTextureLease>>,
    ) -> Result<PreparedSourceTexture, RenderError> {
        if !planes.alpha.is_finite() || !(0.0..=1.0).contains(&planes.alpha) {
            return Err(RenderError::InvalidImageData);
        }
        if !planes.format.is_supported()
            || planes.format.interleaved_chroma == planes.v.is_some()
            || planes.format.alpha != alpha_plane.is_some()
        {
            return Err(RenderError::InvalidImageData);
        }
        self.validate_size(planes.size)?;
        self.validate_size(presentation.output_size)?;
        validate_presentation(planes.size, presentation)?;
        let color = planes.color.unwrap_or_default();
        let output_format = self.intermediate_format();
        let output = self.textures.acquire(
            self.gpu.device(),
            presentation.output_size,
            output_format,
            "alpha2 YUV conversion target",
        );
        let output_view = output.view().clone();
        let cached = self.yuv_pipeline(storage == PlaneStorage::Uint16);
        let uniform_bytes = uniform_bytes(
            color,
            tone,
            planes.alpha,
            planes.size,
            presentation,
            planes.format,
            storage.code_scale(planes.format),
            self.presentation_filter_samples(),
        );
        let mut uniforms = self.uniform_buffers.acquire(
            self.gpu.device(),
            u64::try_from(uniform_bytes.len()).unwrap_or(u64::MAX),
        );
        let uniform = uniforms.write(&uniform_bytes);
        let bind_group = self
            .gpu
            .device()
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("alpha2 YUV bind group"),
                layout: &cached.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(planes.y),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(planes.chroma),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(
                            planes.v.unwrap_or(planes.chroma),
                        ),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::Sampler(&cached.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: uniforms.binding(uniform),
                    },
                    wgpu::BindGroupEntry {
                        binding: 5,
                        // An opaque format reads no alpha; any plane fills the slot.
                        resource: wgpu::BindingResource::TextureView(
                            alpha_plane.unwrap_or(planes.y),
                        ),
                    },
                ],
            });
        Ok(PreparedSourceTexture {
            texture: RenderedTexture::pooled(presentation.output_size, output_format, output),
            pending: Some(PendingSourceWork::new(SourceWork {
                label: "alpha2 YUV conversion pass",
                conversion: true,
                pipeline: cached.pipeline.clone(),
                bind_group,
                output_view,
                uniforms,
                uploads,
                external_leases,
            })),
        })
    }
}

pub(crate) fn validate_plane(
    bytes: &[u8],
    stride: u32,
    width: u32,
    height: u32,
) -> Result<(), RenderError> {
    let required = usize::try_from(stride).ok().and_then(|stride| {
        usize::try_from(height)
            .ok()
            .and_then(|height| stride.checked_mul(height))
    });
    if stride < width || required != Some(bytes.len()) {
        Err(RenderError::InvalidImageData)
    } else {
        Ok(())
    }
}

fn texture_bytes(size: RenderSize, format: wgpu::TextureFormat) -> u64 {
    let block = format.block_copy_size(None).unwrap_or(4);
    u64::from(size.width)
        .saturating_mul(u64::from(size.height))
        .saturating_mul(u64::from(block))
}

/// Uploads one plane of `width` by `height` sample positions as `texture_format`.
pub(crate) fn upload_plane(
    renderer: &Renderer,
    label: &'static str,
    plane: SourcePlane<'_>,
    (width, height): (u32, u32),
    texture_format: wgpu::TextureFormat,
) -> Result<crate::UploadTexture, RenderError> {
    let size = RenderSize::new(width, height)?;
    renderer.validate_size(size)?;
    let texture =
        renderer
            .upload_textures
            .acquire(renderer.gpu.device(), size, texture_format, label);
    renderer.gpu.queue().write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: texture.texture(),
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        plane.bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(plane.stride),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    Ok(texture)
}

impl Renderer {
    /// The most samples a presentation pass averages for one output pixel
    /// when it shrinks a source: the quality budget the composite's
    /// minification filter also takes, at most 64.
    #[allow(clippy::cast_precision_loss)]
    fn presentation_filter_samples(&self) -> f32 {
        self.quality_controls().max_filter_samples as f32
    }

    /// The conversion's bindings: four planes (Y, U or UV, V, alpha) of
    /// `sample_type`, the plane sampler, and the uniform.
    fn yuv_bind_group_layout(&self, sample_type: wgpu::TextureSampleType) -> wgpu::BindGroupLayout {
        let texture = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type,
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        self.gpu
            .device()
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("alpha2 YUV layout"),
                entries: &[
                    texture(0),
                    texture(1),
                    texture(2),
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 4,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    texture(5),
                ],
            })
    }

    /// The conversion pipeline for normalized planes, or, with
    /// `integer_planes`, for 16-bit containers uploaded as unsigned integers.
    fn yuv_pipeline(&self, integer_planes: bool) -> &CachedYuvPipeline {
        let (cache, source) = if integer_planes {
            (&self.pipelines.yuv16, YUV16_TO_LINEAR_WGSL)
        } else {
            (&self.pipelines.yuv, YUV_TO_LINEAR_WGSL)
        };
        cache.get_or_init(|| {
            let bind_group_layout = self.yuv_bind_group_layout(if integer_planes {
                wgpu::TextureSampleType::Uint
            } else {
                wgpu::TextureSampleType::Float { filterable: true }
            });
            let shader = self
                .gpu
                .device()
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("alpha2 YUV conversion shader"),
                    source: wgpu::ShaderSource::Wgsl(source.into()),
                });
            let pipeline_layout =
                self.gpu
                    .device()
                    .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                        label: Some("alpha2 YUV pipeline layout"),
                        bind_group_layouts: &[Some(&bind_group_layout)],
                        immediate_size: 0,
                    });
            let pipeline =
                self.gpu
                    .device()
                    .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                        label: Some("alpha2 YUV pipeline"),
                        layout: Some(&pipeline_layout),
                        vertex: wgpu::VertexState {
                            module: &shader,
                            entry_point: Some("vs"),
                            buffers: &[],
                            compilation_options: wgpu::PipelineCompilationOptions::default(),
                        },
                        fragment: Some(wgpu::FragmentState {
                            module: &shader,
                            entry_point: Some("fs_srgb"),
                            targets: &[Some(self.intermediate_format().into())],
                            compilation_options: wgpu::PipelineCompilationOptions::default(),
                        }),
                        primitive: wgpu::PrimitiveState::default(),
                        depth_stencil: None,
                        multisample: wgpu::MultisampleState::default(),
                        multiview_mask: None,
                        cache: None,
                    });
            let sampler = self.gpu.device().create_sampler(&wgpu::SamplerDescriptor {
                label: Some("alpha2 YUV sampler"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            });
            CachedYuvPipeline {
                bind_group_layout,
                pipeline,
                sampler,
            }
        })
    }
}

pub(crate) fn validate_presentation(
    source: RenderSize,
    presentation: SourcePresentation,
) -> Result<(), RenderError> {
    let visible = presentation.visible_rect;
    if visible.width == 0
        || visible.height == 0
        || presentation.sample_aspect_ratio.0 == 0
        || presentation.sample_aspect_ratio.1 == 0
        || visible
            .x
            .checked_add(visible.width)
            .is_none_or(|right| right > source.width)
        || visible
            .y
            .checked_add(visible.height)
            .is_none_or(|bottom| bottom > source.height)
    {
        Err(RenderError::InvalidImageData)
    } else {
        Ok(())
    }
}

/// What the conversion does with a source's signal.
pub(crate) enum HdrHandling {
    /// An SDR signal, decoded as SDR.
    Sdr,
    /// An HDR signal tone-mapped into the working space.
    ToneMap(HdrToneMap),
    /// An HDR signal the policy skips: a transparent picture.
    Skip,
}

/// How `policy` takes `color`, with its diagnostic recorded.
pub(crate) fn hdr_handling(
    color: ColorMetadata,
    policy: ColorPolicy,
    diagnostics: &mut Vec<ColorDiagnostic>,
) -> Result<HdrHandling, RenderError> {
    if !crate::is_hdr(color) {
        return Ok(HdrHandling::Sdr);
    }
    match policy {
        ColorPolicy::RejectHdr => {
            diagnostics.push(ColorDiagnostic::UnsupportedHdr);
            Err(RenderError::UnsupportedHdr)
        }
        ColorPolicy::WarnHdr => {
            diagnostics.push(ColorDiagnostic::UnsupportedHdr);
            Ok(HdrHandling::Skip)
        }
        ColorPolicy::ToneMapHdr(tone) => {
            diagnostics.push(ColorDiagnostic::ToneMappedHdr);
            Ok(HdrHandling::ToneMap(tone))
        }
    }
}

/// The presentation shaders' `orientation` word: clockwise quarter turns in
/// the low two bits, and bit 2 set when the stored picture is mirrored first.
fn orientation_uniform(orientation: Orientation) -> u32 {
    orientation.rotation.quarter_turns() | (u32::from(orientation.mirror) << 2)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub(crate) fn presentation_uniform_bytes(
    source: RenderSize,
    presentation: SourcePresentation,
    encoding: PresentedEncoding,
    max_filter_samples: f32,
) -> Vec<u8> {
    let visible = presentation.visible_rect;
    let sar = f64::from(presentation.sample_aspect_ratio.0)
        / f64::from(presentation.sample_aspect_ratio.1);
    let unrotated_width = f64::from(visible.width) * sar;
    let unrotated_height = f64::from(visible.height);
    let (display_width, display_height) = if presentation.orientation.rotation.swaps_axes() {
        (unrotated_height, unrotated_width)
    } else {
        (unrotated_width, unrotated_height)
    };
    let scale = (f64::from(presentation.output_size.width) / display_width)
        .min(f64::from(presentation.output_size.height) / display_height);
    let draw_width = display_width * scale;
    let draw_height = display_height * scale;
    let draw_x = (f64::from(presentation.output_size.width) - draw_width) * 0.5;
    let draw_y = (f64::from(presentation.output_size.height) - draw_height) * 0.5;
    let values = [
        presentation.output_size.width as f32,
        presentation.output_size.height as f32,
        source.width as f32,
        source.height as f32,
        visible.x as f32,
        visible.y as f32,
        visible.width as f32,
        visible.height as f32,
        draw_x as f32,
        draw_y as f32,
        draw_width as f32,
        draw_height as f32,
    ];
    let mut bytes = Vec::with_capacity(64);
    for value in values {
        bytes.extend_from_slice(&value.to_ne_bytes());
    }
    bytes.extend_from_slice(&orientation_uniform(presentation.orientation).to_ne_bytes());
    let mode = match encoding {
        PresentedEncoding::Srgb(SourceAlpha::Straight) => 0_u32,
        PresentedEncoding::Srgb(SourceAlpha::Premultiplied) => 1,
        PresentedEncoding::Linear => 2,
    };
    bytes.extend_from_slice(&mode.to_ne_bytes());
    bytes.extend_from_slice(&max_filter_samples.to_ne_bytes());
    bytes.extend_from_slice(&[0; 4]);
    bytes
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::too_many_arguments
)]
pub(crate) fn uniform_bytes(
    color: ColorMetadata,
    tone: Option<HdrToneMap>,
    alpha: f32,
    source: RenderSize,
    presentation: SourcePresentation,
    format: YuvFormat,
    code_scale: f32,
    max_filter_samples: f32,
) -> Vec<u8> {
    let mut bytes = color_uniform_bytes(color, tone, alpha, format.bit_depth, code_scale);
    // The word after `alpha` says whether an alpha plane is read, and the one after it
    // scales its samples to full range.
    let (alpha_plane, alpha_scale) = if format.alpha {
        (1_u32, code_scale)
    } else {
        (0, 0.0)
    };
    bytes[88..92].copy_from_slice(&alpha_plane.to_ne_bytes());
    bytes[92..96].copy_from_slice(&alpha_scale.to_ne_bytes());
    let visible = presentation.visible_rect;
    let sar = f64::from(presentation.sample_aspect_ratio.0)
        / f64::from(presentation.sample_aspect_ratio.1);
    let unrotated_width = f64::from(visible.width) * sar;
    let unrotated_height = f64::from(visible.height);
    let (display_width, display_height) = if presentation.orientation.rotation.swaps_axes() {
        (unrotated_height, unrotated_width)
    } else {
        (unrotated_width, unrotated_height)
    };
    let scale = (f64::from(presentation.output_size.width) / display_width)
        .min(f64::from(presentation.output_size.height) / display_height);
    let draw_width = display_width * scale;
    let draw_height = display_height * scale;
    let draw_x = (f64::from(presentation.output_size.width) - draw_width) * 0.5;
    let draw_y = (f64::from(presentation.output_size.height) - draw_height) * 0.5;
    let (chroma_width, chroma_height) = format.chroma_size(source.width, source.height);
    let chroma_size = [chroma_width as f32, chroma_height as f32];
    let factor_x = (1_u32 << format.chroma_shift_x) as f32;
    let factor_y = (1_u32 << format.chroma_shift_y) as f32;
    // A co-sited chroma sample sits `factor / 2 - 0.5` luma samples before its group's centre.
    let (cosited_x, cosited_y) = match presentation.chroma_siting {
        ChromaLocation::Left => (true, false),
        ChromaLocation::TopLeft => (true, true),
        ChromaLocation::Unspecified | ChromaLocation::Center => (false, false),
    };
    let chroma_offset = [
        if cosited_x { factor_x * 0.5 - 0.5 } else { 0.0 },
        if cosited_y { factor_y * 0.5 - 0.5 } else { 0.0 },
    ];
    let presentation_values = [
        presentation.output_size.width as f32,
        presentation.output_size.height as f32,
        source.width as f32,
        source.height as f32,
        visible.x as f32,
        visible.y as f32,
        visible.width as f32,
        visible.height as f32,
        draw_x as f32,
        draw_y as f32,
        draw_width as f32,
        draw_height as f32,
        chroma_size[0],
        chroma_size[1],
        chroma_offset[0],
        chroma_offset[1],
    ];
    for value in presentation_values {
        bytes.extend_from_slice(&value.to_ne_bytes());
    }
    bytes.extend_from_slice(&orientation_uniform(presentation.orientation).to_ne_bytes());
    let plane_layout = u32::from(format.interleaved_chroma);
    bytes.extend_from_slice(&plane_layout.to_ne_bytes());
    for scale in [1.0 / factor_x, 1.0 / factor_y] {
        bytes.extend_from_slice(&scale.to_ne_bytes());
    }
    bytes.extend(hdr_uniform_bytes(color, tone));
    for bound in chroma_window(visible, format) {
        bytes.extend_from_slice(&bound.to_ne_bytes());
    }
    for value in [max_filter_samples, 0.0, 0.0, 0.0] {
        bytes.extend_from_slice(&value.to_ne_bytes());
    }
    bytes
}

/// The chroma samples that belong to `visible`, as the first and last sample
/// centres on each axis (`[x0, y0, x1, y1]`, in chroma sample positions): the
/// samples whose luma group reaches into the visible rect. Chroma sampling is
/// clamped to it, so the padding a decoder surface carries beyond its picture
/// never mixes into the picture's last row or column. When the surface is the
/// picture this is the whole plane, `0.5 ..= size - 0.5`.
#[allow(clippy::cast_precision_loss)]
fn chroma_window(visible: Rect, format: YuvFormat) -> [f32; 4] {
    let first = |origin: u32, shift: u8| (origin >> shift) as f32 + 0.5;
    let last =
        |origin: u32, length: u32, shift: u8| (origin + length).div_ceil(1 << shift) as f32 - 0.5;
    [
        first(visible.x, format.chroma_shift_x),
        first(visible.y, format.chroma_shift_y),
        last(visible.x, visible.width, format.chroma_shift_x),
        last(visible.y, visible.height, format.chroma_shift_y),
    ]
}

/// The HDR part of the conversion uniform: the primaries matrix into BT.709,
/// the tone map's peaks and their PQ codes, and HLG's system gamma.
fn hdr_uniform_bytes(color: ColorMetadata, tone: Option<HdrToneMap>) -> Vec<u8> {
    let gamut = crate::hdr::gamut_matrix(color.primaries, ColorPrimaries::Bt709);
    let tone = tone.unwrap_or_default();
    #[allow(clippy::cast_precision_loss)]
    let peak = tone.source_peak_nits as f32;
    let white = crate::hdr::SDR_WHITE_NITS;
    let gamma = 1.2 + 0.42 * (peak / 1_000.0).log10();
    let mut bytes = Vec::with_capacity(80);
    for value in gamut
        .into_iter()
        .flat_map(|row| [row[0], row[1], row[2], 0.0])
        .chain([
            peak,
            white,
            crate::hdr::pq_inverse_eotf(peak),
            crate::hdr::pq_inverse_eotf(white),
            gamma,
            0.0,
            0.0,
            0.0,
        ])
    {
        bytes.extend_from_slice(&value.to_ne_bytes());
    }
    bytes
}

fn color_uniform_bytes(
    color: ColorMetadata,
    tone: Option<HdrToneMap>,
    alpha: f32,
    bit_depth: u8,
    code_scale: f32,
) -> Vec<u8> {
    // 0 decodes SDR with the sRGB EOTF; 1 and 2 bring PQ and HLG in through the tone
    // map.
    let transfer: u32 = match (tone, color.transfer) {
        (Some(_), TransferFunction::Pq) => 1,
        (Some(_), TransferFunction::Hlg) => 2,
        _ => 0,
    };
    if color.matrix == ColorMatrix::Rgb {
        return identity_rgb_uniform_bytes(alpha, transfer);
    }
    let (kr, kb) = color
        .matrix
        .luma_weights()
        .expect("RGB components are handled above");
    let kg = 1.0 - kr - kb;
    let rows = [
        [1.0, 0.0, 2.0 * (1.0 - kr), 0.0],
        [
            1.0,
            -2.0 * kb * (1.0 - kb) / kg,
            -2.0 * kr * (1.0 - kr) / kg,
            0.0,
        ],
        [1.0, 2.0 * (1.0 - kb), 0.0, 0.0],
    ];
    let (scale, offset) = range_scale_offset(color.range, bit_depth, code_scale);
    let mut bytes = Vec::with_capacity(96);
    for value in rows.into_iter().flatten().chain(scale).chain(offset) {
        bytes.extend_from_slice(&value.to_ne_bytes());
    }
    bytes.extend_from_slice(&transfer.to_ne_bytes());
    bytes.extend_from_slice(&alpha.to_ne_bytes());
    bytes.extend_from_slice(&[0; 8]);
    bytes
}

/// Component scale and offset that map sampled codes to normalized YCbCr.
///
/// The range's levels at the bit depth come from
/// [`ColorRange::levels`](alpha2_media::ColorRange::levels), the one
/// definition export packs with too. `code` is the storage's
/// [`PlaneStorage::code_scale`], folded in before the range conversion.
fn range_scale_offset(range: ColorRange, depth: u8, code: f32) -> ([f32; 4], [f32; 4]) {
    let levels = range.levels(depth);
    let (luma_scale, chroma_scale) = match range {
        ColorRange::Full => (code, code),
        ColorRange::Limited => (
            code * levels.maximum / levels.luma_span,
            code * levels.maximum / levels.chroma_span,
        ),
    };
    let chroma_offset = 0.0 - levels.chroma_center / levels.chroma_span;
    (
        [luma_scale, chroma_scale, chroma_scale, 0.0],
        [
            0.0 - levels.luma_black / levels.luma_span,
            chroma_offset,
            chroma_offset,
            0.0,
        ],
    )
}

fn identity_rgb_uniform_bytes(alpha: f32, transfer: u32) -> Vec<u8> {
    let rows = [
        [1.0_f32, 0.0, 0.0, 0.0],
        [0.0_f32, 1.0, 0.0, 0.0],
        [0.0_f32, 0.0, 1.0, 0.0],
    ];
    let mut bytes = Vec::with_capacity(96);
    for value in rows
        .into_iter()
        .flatten()
        .chain([1.0, 1.0, 1.0, 0.0])
        .chain([0.0; 4])
    {
        bytes.extend_from_slice(&value.to_ne_bytes());
    }
    bytes.extend_from_slice(&transfer.to_ne_bytes());
    bytes.extend_from_slice(&alpha.to_ne_bytes());
    bytes.extend_from_slice(&[0; 8]);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use alpha2_media::{ColorPrimaries, DynamicRange};

    #[test]
    fn parked_leases_are_dropped_only_when_drained() {
        // The callback runs under the decoder queue lock; dropping an FFmpeg frame there inverts
        // FFmpeg's frame lock order and deadlocks the export.
        #[derive(Debug)]
        struct CountingLease(Arc<std::sync::atomic::AtomicUsize>);

        impl Drop for CountingLease {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Release);
            }
        }

        impl ExternalTextureLease for CountingLease {
            fn retained_bytes(&self) -> u64 {
                0
            }
        }

        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let retired = RetiredLeases::default();
        let lease: Arc<dyn ExternalTextureLease> = Arc::new(CountingLease(Arc::clone(&drops)));
        retired.park(vec![lease]);
        assert_eq!(drops.load(Ordering::Acquire), 0, "parking must not release");
        retired.drain();
        assert_eq!(drops.load(Ordering::Acquire), 1, "drain must release");
        retired.drain();
        assert_eq!(drops.load(Ordering::Acquire), 1, "drain must be idempotent");
    }

    #[test]
    fn ten_bit_limited_range_normalizes_the_p010_container() {
        let (scale, offset) = range_scale_offset(
            ColorRange::Limited,
            10,
            PlaneStorage::Unorm16.code_scale(YuvFormat::P010),
        );
        // A limited-range 10-bit black sample is code 64, stored as 64 << 6.
        let black = f32::from(64_u16 << 6) / 65535.0;
        assert!((black * scale[0] + offset[0]).abs() < 1e-4);
        // Nominal white is code 940.
        let white = f32::from(940_u16 << 6) / 65535.0;
        assert!((white * scale[0] + offset[0] - 1.0).abs() < 1e-4);
        // Neutral chroma is code 512.
        let neutral = f32::from(512_u16 << 6) / 65535.0;
        assert!((neutral * scale[1] + offset[1]).abs() < 1e-4);
    }

    #[test]
    fn integer_containers_take_the_scale_normalized_ones_do() {
        // The integer build reads a container as `container / 65535`, as an R16Unorm
        // texture samples it.
        for format in [YuvFormat::planar(1, 1, 10), YuvFormat::P010] {
            assert_eq!(
                PlaneStorage::Uint16.code_scale(format).to_bits(),
                PlaneStorage::Unorm16.code_scale(format).to_bits()
            );
        }
        assert_eq!(
            PlaneStorage::Uint16.texture_format(false),
            wgpu::TextureFormat::R16Uint
        );
        assert_eq!(
            PlaneStorage::Uint16.texture_format(true),
            wgpu::TextureFormat::Rg16Uint
        );
    }

    #[test]
    fn eight_bit_range_constants_are_unchanged() {
        let bits = |values: [f32; 4]| values.map(f32::to_bits);
        let (scale, offset) = range_scale_offset(ColorRange::Limited, 8, 1.0);
        assert_eq!(
            bits(scale),
            bits([255.0 / 219.0, 255.0 / 224.0, 255.0 / 224.0, 0.0])
        );
        assert_eq!(
            bits(offset),
            bits([-16.0 / 219.0, -128.0 / 224.0, -128.0 / 224.0, 0.0])
        );
        let (scale, offset) = range_scale_offset(ColorRange::Full, 8, 1.0);
        assert_eq!(bits(scale), bits([1.0, 1.0, 1.0, 0.0]));
        assert_eq!(
            bits(offset),
            bits([0.0, -128.0 / 255.0, -128.0 / 255.0, 0.0])
        );
    }

    #[test]
    fn rgb_uniform_uses_identity_matrix_without_chroma_offsets() {
        let bytes = uniform_bytes(
            ColorMetadata {
                primaries: ColorPrimaries::Bt709,
                transfer: TransferFunction::Srgb,
                matrix: ColorMatrix::Rgb,
                range: ColorRange::Limited,
                dynamic_range: DynamicRange::Sdr,
            },
            None,
            1.0,
            RenderSize::new(2, 2).expect("size"),
            SourcePresentation::identity(RenderSize::new(2, 2).expect("size")),
            YuvFormat::I420,
            1.0,
            64.0,
        );
        let floats = bytes[..80]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| f32::from_ne_bytes(*bytes))
            .collect::<Vec<_>>();
        assert_eq!(
            floats.into_iter().map(f32::to_bits).collect::<Vec<_>>(),
            vec![
                1.0, 0.0, 0.0, 0.0, // matrix row 0
                0.0, 1.0, 0.0, 0.0, // matrix row 1
                0.0, 0.0, 1.0, 0.0, // matrix row 2
                1.0, 1.0, 1.0, 0.0, // component scale
                0.0, 0.0, 0.0, 0.0, // component offset
            ]
            .into_iter()
            .map(f32::to_bits)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_chroma_window_is_the_visible_pictures_chroma() {
        let window = |x, y, width, height, format| {
            chroma_window(
                Rect {
                    x,
                    y,
                    width,
                    height,
                },
                format,
            )
            .map(f32::to_bits)
        };
        let bits = |bounds: [f32; 4]| bounds.map(f32::to_bits);
        // 1080p decoded into 1920 x 1088: chroma rows 540..544 are padding.
        assert_eq!(
            window(0, 0, 1920, 1080, YuvFormat::I420),
            bits([0.5, 0.5, 959.5, 539.5])
        );
        // 1366 wide in a 1376 surface: the last column's chroma is sample 682.
        assert_eq!(
            window(0, 0, 1366, 768, YuvFormat::I420),
            bits([0.5, 0.5, 682.5, 383.5])
        );
        // An odd crop keeps the chroma sample its first luma column shares.
        assert_eq!(
            window(1, 3, 5, 4, YuvFormat::I420),
            bits([0.5, 1.5, 2.5, 3.5])
        );
        assert_eq!(
            window(2, 1, 3, 2, YuvFormat::planar(0, 0, 8)),
            bits([2.5, 1.5, 4.5, 2.5])
        );
    }
}

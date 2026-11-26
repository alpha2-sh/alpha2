use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc;

use crate::bind_group::OffsetBindGroup;
use crate::canary::{CANARY_BYTES, Canary};
use crate::composite::PixelRect;
use crate::renderer::PassEncoder;
use crate::submission_planner::Piece;
use crate::{
    ReadbackFailure, RenderError, RenderSize, RenderedTexture, Renderer,
    pipeline::{
        CachedLayoutPackPipeline, CachedPipeline, CachedYuvReadbackPipeline, LayoutPackPass,
    },
    pool::{ReadbackBufferLease, TextureLease},
    uniform::UniformArenaLease,
};
use alpha2_gpu::GpuContext;
use alpha2_media::{ChromaLocation, ColorMatrix, PixelLayout, YuvEncoding};
use alpha2_shaders::{LAYOUT_PACK_WGSL, READBACK_PACK_WGSL, RGB_TO_YUV420_WGSL};

const RGBA8_BYTES_PER_PIXEL: u32 = 4;
const RGBA16F_BYTES_PER_PIXEL: u32 = 8;
const YUV420_PLANE_COUNT: u64 = 3;
const LAYOUT_PACK_PASSES: u64 = 2;

/// D3D12 texture-to-buffer copies must start on this; wgpu's DX12 workaround for an unaligned
/// start pads rows and overwrites whatever follows.
const PLANE_PLACEMENT_ALIGNMENT: u64 = 512;

/// Where each plane of one GPU-packed readback sits in its staging buffer, in storage order.
///
/// Each plane starts on [`PLANE_PLACEMENT_ALIGNMENT`] so DX12's padded-copy workaround cannot
/// overwrite the next plane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StagingLayout {
    planes: Vec<StagedPlane>,
    bytes: u64,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct StagedPlane {
    offset: u64,
    pitch: u32,
    row_bytes: u32,
    rows: u32,
}

impl StagingLayout {
    /// Planes of `(row_bytes, rows)`, in order.
    fn new(planes: impl IntoIterator<Item = (u32, u32)>) -> Result<Self, RenderError> {
        let overflow = || RenderError::TextureReadback(ReadbackFailure::Layout);
        let mut staged = Vec::new();
        let mut end = 0_u64;
        for (row_bytes, rows) in planes {
            let pitch = aligned_copy_row(row_bytes)?;
            let offset = end
                .checked_next_multiple_of(PLANE_PLACEMENT_ALIGNMENT)
                .ok_or_else(overflow)?;
            end = u64::from(pitch)
                .checked_mul(u64::from(rows))
                .and_then(|region| region.checked_add(offset))
                .ok_or_else(overflow)?;
            staged.push(StagedPlane {
                offset,
                pitch,
                row_bytes,
                rows,
            });
        }
        Ok(Self {
            planes: staged,
            bytes: end,
        })
    }

    pub(crate) fn yuv420(size: RenderSize) -> Result<Self, RenderError> {
        let (chroma_width, chroma_rows) = (size.width / 2, size.height / 2);
        Self::new([
            (size.width, size.height),
            (chroma_width, chroma_rows),
            (chroma_width, chroma_rows),
        ])
    }

    pub(crate) fn of_layout(layout: PixelLayout, size: RenderSize) -> Result<Self, RenderError> {
        let planes = layout
            .planes(size.width, size.height)
            .into_iter()
            .map(|(width, rows, bytes)| {
                width
                    .checked_mul(bytes)
                    .map(|row_bytes| (row_bytes, rows))
                    .ok_or(RenderError::TextureReadback(ReadbackFailure::Layout))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(planes)
    }

    fn packed_bytes(&self) -> u64 {
        self.planes.iter().fold(0_u64, |bytes, plane| {
            bytes.saturating_add(u64::from(plane.row_bytes) * u64::from(plane.rows))
        })
    }
}

/// Staging bytes of one planar YUV420 readback of `size`, including row padding, plane
/// alignment and the 16-byte canary. A budget must reserve this per in-flight readback.
///
/// # Errors
/// Returns [`RenderError::TextureReadback`] when the layout overflows.
pub fn yuv420p_staging_bytes(size: RenderSize) -> Result<u64, RenderError> {
    StagingLayout::yuv420(size).map(|layout| layout.bytes + CANARY_BYTES)
}

/// The mapped buffer a readback of a `size` frame packed into `layout` takes
/// ([`Renderer::begin_layout_readback`]), canary included: each plane's rows padded to the copy
/// alignment.
///
/// # Errors
/// [`RenderError::TextureReadback`] when the layout overflows.
pub fn layout_staging_bytes(size: RenderSize, layout: PixelLayout) -> Result<u64, RenderError> {
    StagingLayout::of_layout(layout, size).map(|staging| staging.bytes + CANARY_BYTES)
}

fn check_layout_target(target: &RenderedTexture, layout: PixelLayout) -> Result<(), RenderError> {
    let (step_x, step_y) = layout.chroma.subsampling();
    let readable = match target.format {
        wgpu::TextureFormat::Rgba16Float => true,
        wgpu::TextureFormat::Rgba8Unorm => layout.bit_depth == alpha2_media::BitDepth::Eight,
        _ => false,
    };
    if readable
        && target.size.width.is_multiple_of(step_x)
        && target.size.height.is_multiple_of(step_y)
    {
        Ok(())
    } else {
        Err(RenderError::TextureReadback(
            ReadbackFailure::UnsupportedTarget,
        ))
    }
}

fn plane_rows(
    mapped: &[u8],
    plane: StagedPlane,
) -> Result<std::iter::Take<std::slice::ChunksExact<'_, u8>>, RenderError> {
    let offset = usize::try_from(plane.offset)
        .map_err(|_| RenderError::TextureReadback(ReadbackFailure::Layout))?;
    let pitch = usize::try_from(plane.pitch)
        .map_err(|_| RenderError::TextureReadback(ReadbackFailure::Layout))?;
    let width = usize::try_from(plane.row_bytes)
        .map_err(|_| RenderError::TextureReadback(ReadbackFailure::Layout))?;
    let rows = usize::try_from(plane.rows)
        .map_err(|_| RenderError::TextureReadback(ReadbackFailure::Layout))?;
    let region = pitch
        .checked_mul(rows)
        .and_then(|length| length.checked_add(offset))
        .and_then(|end| mapped.get(offset..end))
        .filter(|_| width <= pitch)
        .ok_or(RenderError::TextureReadback(ReadbackFailure::Layout))?;
    Ok(region.chunks_exact(pitch).take(rows))
}

pub(crate) struct EncodedLayoutReadback {
    buffer: ReadbackBufferLease,
    size: RenderSize,
    staging: StagingLayout,
    pixel_layout: PixelLayout,
    staging_bytes: u64,
    canary: Canary,
    planes: Vec<TextureLease>,
}

pub(crate) struct EncodedYuv420pReadback {
    buffer: ReadbackBufferLease,
    size: RenderSize,
    layout: StagingLayout,
    staging_bytes: u64,
    canary: Canary,
    y_texture: TextureLease,
    u_texture: TextureLease,
    v_texture: TextureLease,
}

impl Renderer {
    /// Uploads tightly packed premultiplied sRGB RGBA8 pixels into a pooled upload texture.
    ///
    /// # Errors
    /// [`RenderError::InvalidImageData`] when `pixels` is not four bytes per pixel.
    pub fn upload_rgba8(
        &self,
        size: RenderSize,
        pixels: &[u8],
    ) -> Result<crate::UploadTexture, RenderError> {
        self.write_rgba8(
            size,
            crate::SourcePlane {
                bytes: pixels,
                stride: size
                    .width
                    .checked_mul(RGBA8_BYTES_PER_PIXEL)
                    .ok_or(RenderError::InvalidImageData)?,
            },
        )
    }

    /// Uploads tightly packed premultiplied linear RGBA half floats (BT.709, `1.0` = SDR white) for
    /// [`Renderer::prepare_linear_presented`].
    ///
    /// # Errors
    /// [`RenderError::InvalidImageData`] when `pixels` is not four halves per pixel.
    pub fn upload_linear_rgba16f(
        &self,
        size: RenderSize,
        pixels: &[u16],
    ) -> Result<crate::UploadTexture, RenderError> {
        // Avoids a frame-sized copy per frame on little-endian targets.
        let bytes: std::borrow::Cow<'_, [u8]> = if cfg!(target_endian = "little") {
            std::borrow::Cow::Borrowed(bytemuck::cast_slice(pixels))
        } else {
            std::borrow::Cow::Owned(pixels.iter().flat_map(|half| half.to_le_bytes()).collect())
        };
        self.write_linear_rgba16f(
            size,
            crate::SourcePlane {
                bytes: &bytes,
                stride: size
                    .width
                    .checked_mul(RGBA16F_BYTES_PER_PIXEL)
                    .ok_or(RenderError::InvalidImageData)?,
            },
        )
    }

    /// Writes strided little-endian RGBA half-float rows into a pooled upload texture.
    ///
    /// # Errors
    /// [`RenderError::InvalidImageData`] for a plane shorter than `size`.
    pub fn write_linear_rgba16f(
        &self,
        size: RenderSize,
        plane: crate::SourcePlane<'_>,
    ) -> Result<crate::UploadTexture, RenderError> {
        self.validate_size(size)?;
        let texture = self.upload_textures.acquire(
            self.gpu.device(),
            size,
            wgpu::TextureFormat::Rgba16Float,
            "alpha2 linear RGBA upload texture",
        );
        self.write_plane(&texture, size, plane, RGBA16F_BYTES_PER_PIXEL)?;
        Ok(texture)
    }

    pub(crate) fn write_rgba8(
        &self,
        size: RenderSize,
        plane: crate::SourcePlane<'_>,
    ) -> Result<crate::UploadTexture, RenderError> {
        let texture = self.acquire_rgba8_upload_texture(size)?;
        self.write_plane(&texture, size, plane, RGBA8_BYTES_PER_PIXEL)?;
        Ok(texture)
    }

    fn write_plane(
        &self,
        texture: &crate::UploadTexture,
        size: RenderSize,
        plane: crate::SourcePlane<'_>,
        bytes_per_pixel: u32,
    ) -> Result<(), RenderError> {
        let row_bytes = size
            .width
            .checked_mul(bytes_per_pixel)
            .ok_or(RenderError::InvalidImageData)?;
        let required = usize::try_from(plane.stride)
            .ok()
            .zip(usize::try_from(size.height).ok())
            .and_then(|(stride, rows)| stride.checked_mul(rows));
        if plane.stride < row_bytes || required != Some(plane.bytes.len()) {
            return Err(RenderError::InvalidImageData);
        }
        self.gpu.queue().write_texture(
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
                rows_per_image: Some(size.height),
            },
            wgpu::Extent3d {
                width: size.width,
                height: size.height,
                depth_or_array_layers: 1,
            },
        );
        Ok(())
    }

    /// Copies one RGBA8 render result into tightly packed premultiplied host memory.
    ///
    /// Blocks on the GPU, so native and test only; callers that cannot block use
    /// [`Renderer::begin_rgba8_readback`]. Half-float intermediates are quantized to RGBA8 on the
    /// GPU first.
    ///
    /// # Errors
    /// [`RenderError::TextureReadback`] for another target format or a failed mapping.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn read_rgba8(&self, target: &RenderedTexture) -> Result<Vec<u8>, RenderError> {
        self.begin_rgba8_readback(target)?.wait()
    }

    /// Submits a readback copy and returns an unmapped handle.
    ///
    /// The copy is queued on return, so mapping can resolve on an event loop via
    /// [`Rgba8Readback::map_async`]. Used for stills, thumbnails and goldens; exports use YUV or
    /// NV12 readbacks instead.
    ///
    /// # Errors
    /// [`RenderError::TextureReadback`] for an unsupported format or unrepresentable row layout.
    pub fn begin_rgba8_readback(
        &self,
        target: &RenderedTexture,
    ) -> Result<Rgba8Readback, RenderError> {
        self.ensure_gpu_runs_work()?;
        self.validate_size(target.size)?;
        if !matches!(
            target.format,
            wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Rgba16Float
        ) {
            return Err(RenderError::TextureReadback(
                ReadbackFailure::UnsupportedTarget,
            ));
        }

        let unpadded_bytes_per_row = target
            .size
            .width
            .checked_mul(RGBA8_BYTES_PER_PIXEL)
            .ok_or(RenderError::TextureReadback(ReadbackFailure::Layout))?;
        let alignment = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bytes_per_row = unpadded_bytes_per_row
            .checked_add(alignment - 1)
            .map(|value| value / alignment)
            .and_then(|rows| rows.checked_mul(alignment))
            .ok_or(RenderError::TextureReadback(ReadbackFailure::Layout))?;
        let image_bytes = u64::from(padded_bytes_per_row)
            .checked_mul(u64::from(target.size.height))
            .ok_or(RenderError::TextureReadback(ReadbackFailure::Layout))?;
        // Rows, then the canary.
        let buffer_size = image_bytes
            .checked_add(CANARY_BYTES)
            .ok_or(RenderError::TextureReadback(ReadbackFailure::Layout))?;
        let mut encoder = self.command_encoder("alpha2 RGBA8 readback encoder");
        let buffer =
            self.readback_buffers
                .acquire(self.gpu.device(), buffer_size, "alpha2 RGBA8 readback");
        let packed_texture = self.encode_readback_pack(target, &mut encoder)?;
        let copy_source = packed_texture
            .as_ref()
            .map_or(&target.texture, TextureLease::texture);
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: copy_source,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: buffer.buffer(),
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(target.size.height),
                },
            },
            wgpu::Extent3d {
                width: target.size.width,
                height: target.size.height,
                depth_or_array_layers: 1,
            },
        );
        // Same encoder, after the copy, so it lands only if the copy ran.
        let canary = self
            .canaries
            .encode(&self.gpu, &mut encoder, buffer.buffer(), image_bytes)?;
        let command_buffer = encoder.finish();
        #[cfg(not(target_arch = "wasm32"))]
        let submission = self.submit([command_buffer])?;
        #[cfg(target_arch = "wasm32")]
        self.submit([command_buffer])?;
        let render_passes = u64::from(packed_texture.is_some());
        let copy_bytes =
            u64::from(unpadded_bytes_per_row).saturating_mul(u64::from(target.size.height));
        crate::diagnostics::record_readback_work(
            &self.diagnostics,
            render_passes,
            1,
            copy_bytes,
            true,
        );
        Ok(Rgba8Readback {
            buffer,
            gpu: self.gpu.clone(),
            size: target.size,
            padded_bytes_per_row,
            unpadded_bytes_per_row,
            #[cfg(not(target_arch = "wasm32"))]
            submission,
            #[cfg(not(target_arch = "wasm32"))]
            timeout: self.gpu_wait_timeout,
            staging_bytes: buffer_size,
            image_bytes,
            canary,
            mapping_requested: AtomicBool::new(false),
        })
    }

    /// Converts a render result to 8-bit planar YUV420 on the GPU and submits an async readback.
    ///
    /// The size must be even. Chroma is filtered for the siting `encoding.chroma` names
    /// ([`alpha2_media::VideoCodec::chroma_location`]).
    ///
    /// # Errors
    /// [`RenderError::TextureReadback`] for unsupported formats, odd dimensions or unrepresentable
    /// layouts, and [`RenderError::UnsupportedOutputColor`] for a color the pack cannot write.
    pub fn begin_yuv420p_readback(
        &self,
        target: &RenderedTexture,
        encoding: YuvEncoding,
    ) -> Result<Yuv420pReadback, RenderError> {
        let mut command_encoder = self.command_encoder("alpha2 YUV420P readback encoder");
        let mut uniforms = self
            .uniform_buffers
            .acquire(self.gpu.device(), YUV_PACK_UNIFORM_BYTES);
        let readback_work =
            self.encode_yuv420p_readback(target, encoding, &mut command_encoder, &mut uniforms)?;
        uniforms.flush(self.gpu.queue());
        #[cfg(not(target_arch = "wasm32"))]
        let submission = self.submit([command_encoder.finish()])?;
        #[cfg(target_arch = "wasm32")]
        self.submit([command_encoder.finish()])?;
        uniforms.recycle();
        Ok(self.complete_yuv420p_readback(
            readback_work,
            #[cfg(not(target_arch = "wasm32"))]
            submission,
            true,
        ))
    }

    /// Packs a render result into `layout`'s planes on the GPU and submits an async readback of
    /// them ([`Yuv420pReadback::finish_into_planes`]), for outputs other than 8-bit 4:2:0.
    ///
    /// Codes are the CPU pack's within one, chroma sited as `encoding.chroma` names. 10-bit output
    /// requires the `rgba16float` intermediate.
    ///
    /// # Errors
    /// [`RenderError::TextureReadback`] for an unsupported format, a 10-bit layout over an 8-bit
    /// target, a size the chroma cannot subsample or an unrepresentable layout, and
    /// [`RenderError::UnsupportedOutputColor`] for a color the pack cannot write.
    pub fn begin_layout_readback(
        &self,
        target: &RenderedTexture,
        encoding: YuvEncoding,
        layout: PixelLayout,
    ) -> Result<Yuv420pReadback, RenderError> {
        let mut encoder = self.command_encoder("alpha2 layout readback encoder");
        let mut uniforms = self
            .uniform_buffers
            .acquire(self.gpu.device(), LAYOUT_PACK_UNIFORM_BYTES);
        let readback_work =
            self.encode_layout_readback(target, encoding, layout, &mut encoder, &mut uniforms)?;
        uniforms.flush(self.gpu.queue());
        #[cfg(not(target_arch = "wasm32"))]
        let submission = self.submit([encoder.finish()])?;
        #[cfg(target_arch = "wasm32")]
        self.submit([encoder.finish()])?;
        uniforms.recycle();
        Ok(self.complete_layout_readback(
            readback_work,
            #[cfg(not(target_arch = "wasm32"))]
            submission,
            true,
        ))
    }

    pub(crate) fn encode_layout_readback(
        &self,
        target: &RenderedTexture,
        encoding: YuvEncoding,
        layout: PixelLayout,
        encoder: &mut impl PassEncoder,
        uniforms: &mut UniformArenaLease,
    ) -> Result<EncodedLayoutReadback, RenderError> {
        self.ensure_gpu_runs_work()?;
        self.validate_size(target.size)?;
        crate::color::check_packable(encoding)?;
        check_layout_target(target, layout)?;
        let staging = StagingLayout::of_layout(layout, target.size)?;
        let buffer_size = staging.bytes + CANARY_BYTES;
        let buffer =
            self.readback_buffers
                .acquire(self.gpu.device(), buffer_size, "alpha2 layout readback");
        let [luma, chroma] = LayoutPackPass::of_layout(layout);
        let chroma_size = layout_chroma_size(target.size, layout);
        let mut luma_planes = self.layout_pack_planes(luma, target.size);
        let chroma_planes = self.layout_pack_planes(chroma, chroma_size);
        let uniform = uniforms.write(bytemuck::cast_slice(&crate::layout_pack::gpu_params(
            encoding, layout,
        )))?;
        let cached = self.layout_pack_pipeline()?;
        let bind_group = OffsetBindGroup {
            group: self.bind_group(&wgpu::BindGroupDescriptor {
                label: Some("alpha2 layout pack bind group"),
                layout: &cached.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(target.view()),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: uniforms.dynamic_binding(uniform.size()),
                    },
                ],
            }),
            offsets: [uniform.dynamic_offset()],
        };
        let key = crate::submission_pilot::PassKind::Pack.key(target.size.pixels());
        for (pass, label, planes, size, work) in [
            (
                luma,
                "alpha2 GPU layout luma pack pass",
                &luma_planes,
                target.size,
                layout_luma_work(target.size),
            ),
            (
                chroma,
                "alpha2 GPU layout chroma pack pass",
                &chroma_planes,
                chroma_size,
                layout_chroma_work(target.size, layout),
            ),
        ] {
            let pipeline = self.layout_pack_program(cached, pass)?;
            let views = planes
                .iter()
                .map(TextureLease::view)
                .collect::<arrayvec::ArrayVec<_, 2>>();
            encoder.placed_pass(work, PixelRect::whole(size), key, |encoder, piece| {
                encode_yuv_render_pass(encoder, label, pipeline, &bind_group, &views, piece);
            });
        }
        let alpha_plane = layout.alpha.then(|| luma_planes.pop()).flatten();
        let planes = luma_planes
            .into_iter()
            .chain(chroma_planes)
            .chain(alpha_plane)
            .collect::<Vec<_>>();
        encoder.place_whole(layout_copy_work(target.size, layout));
        let encoder = encoder.commands();
        for (texture, staged) in planes.iter().zip(&staging.planes) {
            let texture = texture.texture();
            let size = RenderSize::new(texture.width(), texture.height())?;
            encode_plane_copy(
                encoder,
                texture,
                buffer.buffer(),
                staged.offset,
                staged.pitch,
                size,
            );
        }
        let canary = self
            .canaries
            .encode(&self.gpu, encoder, buffer.buffer(), staging.bytes)?;
        Ok(EncodedLayoutReadback {
            buffer,
            size: target.size,
            staging,
            pixel_layout: layout,
            staging_bytes: buffer_size,
            canary,
            planes,
        })
    }

    pub(crate) fn complete_layout_readback(
        &self,
        encoded: EncodedLayoutReadback,
        #[cfg(not(target_arch = "wasm32"))] submission: crate::submission_clock::ClockedSubmission,
        standalone_submission: bool,
    ) -> Yuv420pReadback {
        let EncodedLayoutReadback {
            buffer,
            size,
            staging,
            pixel_layout,
            staging_bytes,
            canary,
            planes,
        } = encoded;
        crate::diagnostics::record_readback_work(
            &self.diagnostics,
            LAYOUT_PACK_PASSES,
            u64::try_from(planes.len()).unwrap_or(u64::MAX),
            staging.packed_bytes(),
            standalone_submission,
        );
        drop(planes);
        Yuv420pReadback {
            buffer,
            gpu: self.gpu.clone(),
            size,
            layout: staging,
            pixel_layout: Some(pixel_layout),
            #[cfg(not(target_arch = "wasm32"))]
            submission,
            #[cfg(not(target_arch = "wasm32"))]
            timeout: self.gpu_wait_timeout,
            staging_bytes,
            canary,
            mapping_requested: AtomicBool::new(false),
        }
    }

    pub(crate) fn encode_yuv420p_readback(
        &self,
        target: &RenderedTexture,
        encoding: YuvEncoding,
        encoder: &mut impl PassEncoder,
        uniforms: &mut UniformArenaLease,
    ) -> Result<EncodedYuv420pReadback, RenderError> {
        self.ensure_gpu_runs_work()?;
        self.validate_size(target.size)?;
        crate::color::check_packable(encoding)?;
        if !matches!(
            target.format,
            wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Rgba16Float
        ) || !target.size.width.is_multiple_of(2)
            || !target.size.height.is_multiple_of(2)
        {
            return Err(RenderError::TextureReadback(
                ReadbackFailure::UnsupportedTarget,
            ));
        }
        let layout = StagingLayout::yuv420(target.size)?;
        let uv_rows = target.size.height / 2;
        let chroma_width = target.size.width / 2;
        let buffer_size = layout.bytes + CANARY_BYTES;
        let buffer = self.readback_buffers.acquire(
            self.gpu.device(),
            buffer_size,
            "alpha2 YUV420P readback",
        );
        let y_texture = self.textures.acquire(
            self.gpu.device(),
            target.size,
            wgpu::TextureFormat::R8Unorm,
            "alpha2 GPU Y readback texture",
        );
        let uv_size = RenderSize {
            width: chroma_width,
            height: uv_rows,
        };
        let u_texture = self.textures.acquire(
            self.gpu.device(),
            uv_size,
            wgpu::TextureFormat::R8Unorm,
            "alpha2 GPU U readback texture",
        );
        let v_texture = self.textures.acquire(
            self.gpu.device(),
            uv_size,
            wgpu::TextureFormat::R8Unorm,
            "alpha2 GPU V readback texture",
        );
        let uniform = uniforms.write(bytemuck::cast_slice(&yuv_uniform(encoding)))?;
        self.encode_yuv420_pack(
            target,
            [y_texture.view(), u_texture.view(), v_texture.view()],
            encoder,
            uniforms,
            uniform,
        )?;
        let encoder = encoder.commands();
        let textures = [
            (y_texture.texture(), target.size),
            (u_texture.texture(), uv_size),
            (v_texture.texture(), uv_size),
        ];
        for ((texture, size), plane) in textures.into_iter().zip(&layout.planes) {
            encode_plane_copy(
                encoder,
                texture,
                buffer.buffer(),
                plane.offset,
                plane.pitch,
                size,
            );
        }
        // Same encoder, after the plane copies. Not counted as a copy in diagnostics.
        let canary = self
            .canaries
            .encode(&self.gpu, encoder, buffer.buffer(), layout.bytes)?;
        Ok(EncodedYuv420pReadback {
            buffer,
            size: target.size,
            layout,
            staging_bytes: buffer_size,
            canary,
            y_texture,
            u_texture,
            v_texture,
        })
    }

    pub(crate) fn complete_yuv420p_readback(
        &self,
        encoded: EncodedYuv420pReadback,
        #[cfg(not(target_arch = "wasm32"))] submission: crate::submission_clock::ClockedSubmission,
        standalone_submission: bool,
    ) -> Yuv420pReadback {
        let EncodedYuv420pReadback {
            buffer,
            size,
            layout,
            staging_bytes,
            canary,
            y_texture,
            u_texture,
            v_texture,
        } = encoded;
        drop((y_texture, u_texture, v_texture));
        let texture_bytes = yuv420_tight_bytes(size).expect("validated YUV420 dimensions");
        crate::diagnostics::record_readback_work(
            &self.diagnostics,
            2,
            3,
            texture_bytes,
            standalone_submission,
        );
        Yuv420pReadback {
            buffer,
            gpu: self.gpu.clone(),
            size,
            layout,
            pixel_layout: None,
            #[cfg(not(target_arch = "wasm32"))]
            submission,
            #[cfg(not(target_arch = "wasm32"))]
            timeout: self.gpu_wait_timeout,
            staging_bytes,
            canary,
            mapping_requested: AtomicBool::new(false),
        }
    }

    /// Packs one rendered frame into caller-owned NV12 plane textures.
    ///
    /// Same conversion as [`Self::begin_yuv420p_readback`] without the readback. wgpu allows only
    /// `TEXTURE_BINDING` on `TextureFormat::NV12`, so the native layer copies these `R8Unorm` and
    /// `Rg8Unorm` targets into the encoder's image afterwards.
    ///
    /// # Errors
    /// [`RenderError::TextureReadback`] for an unaccepted target format or size,
    /// [`RenderError::InvalidPlaneTarget`] for mismatched plane textures,
    /// [`RenderError::UnsupportedOutputColor`] for a color the pack cannot write, and
    /// [`RenderError::GpuWorkDropped`] once the device was found dropping work.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn encode_nv12_pack(
        &self,
        target: &RenderedTexture,
        planes: Nv12PlaneTargets<'_>,
        encoding: YuvEncoding,
        encoder: &mut wgpu::CommandEncoder,
    ) -> Result<EncodedNv12Pack, RenderError> {
        let encode = self.open_encode();
        let mut uniforms = self
            .uniform_buffers
            .acquire(self.gpu.device(), YUV_PACK_UNIFORM_BYTES);
        self.encode_nv12_pack_into(target, planes, encoding, encoder, &mut uniforms)?;
        uniforms.flush(self.gpu.queue());
        Ok(EncodedNv12Pack {
            uniforms: Some(uniforms),
            _encode: encode,
        })
    }

    pub(crate) fn encode_nv12_pack_into(
        &self,
        target: &RenderedTexture,
        planes: Nv12PlaneTargets<'_>,
        encoding: YuvEncoding,
        encoder: &mut impl PassEncoder,
        uniforms: &mut UniformArenaLease,
    ) -> Result<(), RenderError> {
        // A pack has no canary of its own; an earlier failed readback canary applies.
        self.ensure_gpu_runs_work()?;
        self.validate_size(target.size)?;
        crate::color::check_packable(encoding)?;
        if !matches!(
            target.format,
            wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Rgba16Float
        ) || !target.size.width.is_multiple_of(2)
            || !target.size.height.is_multiple_of(2)
        {
            return Err(RenderError::TextureReadback(
                ReadbackFailure::UnsupportedTarget,
            ));
        }
        planes.check(target.size)?;
        let uniform = uniforms.write(bytemuck::cast_slice(&yuv_uniform(encoding)))?;
        let cached = self.yuv_readback_pipeline()?;
        let bind_group = self.yuv_pack_bind_group(target, cached, uniforms, uniform);
        encode_pack_passes(
            encoder,
            &bind_group,
            target.size,
            (
                "alpha2 GPU NV12 Y pack pass",
                &cached.y_pipeline,
                &[planes.luma],
            ),
            (
                "alpha2 GPU NV12 UV pack pass",
                &cached.uv_nv12_pipeline,
                &[planes.chroma],
            ),
        );
        Ok(())
    }

    fn yuv_pack_bind_group(
        &self,
        target: &RenderedTexture,
        cached: &CachedYuvReadbackPipeline,
        uniforms: &crate::uniform::UniformArenaLease,
        uniform: crate::uniform::UniformSlice,
    ) -> OffsetBindGroup<1> {
        let group = self.bind_group(&wgpu::BindGroupDescriptor {
            label: Some("alpha2 RGB to YUV bind group"),
            layout: &cached.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(target.view()),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: uniforms.dynamic_binding(uniform.size()),
                },
            ],
        });
        OffsetBindGroup {
            group,
            offsets: [uniform.dynamic_offset()],
        }
    }

    pub(crate) fn encode_pilot_pack(
        &self,
        target: &RenderedTexture,
        plane_views: [&wgpu::TextureView; 3],
        encoder: &mut crate::renderer::RenderEncoder,
        uniforms: &mut UniformArenaLease,
    ) -> Result<(), RenderError> {
        let uniform = uniforms.write(bytemuck::cast_slice(&yuv_uniform(YuvEncoding::export(
            alpha2_media::VideoCodec::H264,
        ))))?;
        self.encode_yuv420_pack(target, plane_views, encoder, uniforms, uniform)
    }

    fn encode_yuv420_pack(
        &self,
        target: &RenderedTexture,
        plane_views: [&wgpu::TextureView; 3],
        encoder: &mut impl PassEncoder,
        uniforms: &crate::uniform::UniformArenaLease,
        uniform: crate::uniform::UniformSlice,
    ) -> Result<(), RenderError> {
        let cached = self.yuv_readback_pipeline()?;
        let bind_group = self.yuv_pack_bind_group(target, cached, uniforms, uniform);
        let [y_view, u_view, v_view] = plane_views;
        encode_pack_passes(
            encoder,
            &bind_group,
            target.size,
            ("alpha2 GPU Y pack pass", &cached.y_pipeline, &[y_view]),
            (
                "alpha2 GPU UV pack pass",
                &cached.uv_pipeline,
                &[u_view, v_view],
            ),
        );
        Ok(())
    }

    fn yuv_readback_pipeline(&self) -> Result<&CachedYuvReadbackPipeline, RenderError> {
        crate::gpu_errors::checked_cell(&self.pipelines.yuv_readback, self.gpu.device(), || {
            let bind_group_layout =
                self.gpu
                    .device()
                    .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                        label: Some("alpha2 RGB to YUV layout"),
                        entries: &[
                            wgpu::BindGroupLayoutEntry {
                                binding: 0,
                                visibility: wgpu::ShaderStages::FRAGMENT,
                                ty: wgpu::BindingType::Texture {
                                    sample_type: wgpu::TextureSampleType::Float {
                                        filterable: false,
                                    },
                                    view_dimension: wgpu::TextureViewDimension::D2,
                                    multisampled: false,
                                },
                                count: None,
                            },
                            wgpu::BindGroupLayoutEntry {
                                binding: 1,
                                visibility: wgpu::ShaderStages::FRAGMENT,
                                ty: wgpu::BindingType::Buffer {
                                    ty: wgpu::BufferBindingType::Uniform,
                                    has_dynamic_offset: true,
                                    min_binding_size: None,
                                },
                                count: None,
                            },
                        ],
                    });
            let shader = self
                .gpu
                .device()
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("alpha2 RGB to YUV shader"),
                    source: wgpu::ShaderSource::Wgsl(RGB_TO_YUV420_WGSL.into()),
                });
            let layout =
                self.gpu
                    .device()
                    .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                        label: Some("alpha2 RGB to YUV pipeline layout"),
                        bind_group_layouts: &[Some(&bind_group_layout)],
                        immediate_size: 0,
                    });
            CachedYuvReadbackPipeline {
                y_pipeline: yuv_render_pipeline(
                    self.gpu.device(),
                    &layout,
                    &shader,
                    "y_fs",
                    &[wgpu::TextureFormat::R8Unorm],
                ),
                uv_pipeline: yuv_render_pipeline(
                    self.gpu.device(),
                    &layout,
                    &shader,
                    "uv_fs",
                    &[wgpu::TextureFormat::R8Unorm, wgpu::TextureFormat::R8Unorm],
                ),
                uv_nv12_pipeline: yuv_render_pipeline(
                    self.gpu.device(),
                    &layout,
                    &shader,
                    "uv_nv12_fs",
                    &[wgpu::TextureFormat::Rg8Unorm],
                ),
                bind_group_layout,
            }
        })
    }

    fn layout_pack_pipeline(&self) -> Result<&CachedLayoutPackPipeline, RenderError> {
        crate::gpu_errors::checked_cell(&self.pipelines.layout_pack, self.gpu.device(), || {
            let device = self.gpu.device();
            let bind_group_layout =
                device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("alpha2 layout pack layout"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                                view_dimension: wgpu::TextureViewDimension::D2,
                                multisampled: false,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 1,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Uniform,
                                has_dynamic_offset: true,
                                min_binding_size: None,
                            },
                            count: None,
                        },
                    ],
                });
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("alpha2 layout pack shader"),
                source: wgpu::ShaderSource::Wgsl(LAYOUT_PACK_WGSL.into()),
            });
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("alpha2 layout pack pipeline layout"),
                bind_group_layouts: &[Some(&bind_group_layout)],
                immediate_size: 0,
            });
            CachedLayoutPackPipeline {
                bind_group_layout,
                pipeline_layout,
                shader,
                programs: Default::default(),
            }
        })
    }

    fn layout_pack_planes(&self, pass: LayoutPackPass, size: RenderSize) -> Vec<TextureLease> {
        pass.target_formats()
            .into_iter()
            .map(|format| {
                self.textures.acquire(
                    self.gpu.device(),
                    size,
                    format,
                    "alpha2 GPU layout pack plane",
                )
            })
            .collect()
    }

    fn layout_pack_program<'a>(
        &self,
        cached: &'a CachedLayoutPackPipeline,
        pass: LayoutPackPass,
    ) -> Result<&'a wgpu::RenderPipeline, RenderError> {
        crate::gpu_errors::checked_cell(&cached.programs[pass.index()], self.gpu.device(), || {
            yuv_render_pipeline(
                self.gpu.device(),
                &cached.pipeline_layout,
                &cached.shader,
                pass.entry_point(),
                &pass.target_formats(),
            )
        })
    }

    fn encode_readback_pack(
        &self,
        target: &RenderedTexture,
        encoder: &mut wgpu::CommandEncoder,
    ) -> Result<Option<TextureLease>, RenderError> {
        if target.format != wgpu::TextureFormat::Rgba16Float {
            return Ok(None);
        }
        let texture = self.textures.acquire(
            self.gpu.device(),
            target.size,
            wgpu::TextureFormat::Rgba8Unorm,
            "alpha2 GPU-packed RGBA8 readback texture",
        );
        let cached = self.readback_pack_pipeline()?;
        let bind_group = self.bind_group(&wgpu::BindGroupDescriptor {
            label: Some("alpha2 readback pack bind group"),
            layout: &cached.bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(target.view()),
            }],
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("alpha2 GPU RGBA8 readback pack pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: texture.view(),
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
            pass.set_pipeline(&cached.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        Ok(Some(texture))
    }

    fn readback_pack_pipeline(&self) -> Result<&CachedPipeline, RenderError> {
        crate::gpu_errors::checked_cell(&self.pipelines.readback_pack, self.gpu.device(), || {
            let bind_group_layout =
                self.gpu
                    .device()
                    .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                        label: Some("alpha2 readback pack layout"),
                        entries: &[wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                                view_dimension: wgpu::TextureViewDimension::D2,
                                multisampled: false,
                            },
                            count: None,
                        }],
                    });
            let shader = self
                .gpu
                .device()
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("alpha2 readback pack shader"),
                    source: wgpu::ShaderSource::Wgsl(READBACK_PACK_WGSL.into()),
                });
            let layout =
                self.gpu
                    .device()
                    .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                        label: Some("alpha2 readback pack pipeline layout"),
                        bind_group_layouts: &[Some(&bind_group_layout)],
                        immediate_size: 0,
                    });
            let pipeline =
                self.gpu
                    .device()
                    .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                        label: Some("alpha2 readback pack pipeline"),
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
                            targets: &[Some(wgpu::TextureFormat::Rgba8Unorm.into())],
                            compilation_options: wgpu::PipelineCompilationOptions::default(),
                        }),
                        primitive: wgpu::PrimitiveState::default(),
                        depth_stencil: None,
                        multisample: wgpu::MultisampleState::default(),
                        multiview_mask: None,
                        cache: None,
                    });
            CachedPipeline {
                bind_group_layout,
                pipeline,
            }
        })
    }
}

/// A submitted texture readback whose buffer has not been mapped yet.
///
/// Owns its staging buffer, so it can move into an async browser task.
#[derive(Debug)]
pub struct Rgba8Readback {
    buffer: ReadbackBufferLease,
    gpu: GpuContext,
    size: RenderSize,
    padded_bytes_per_row: u32,
    unpadded_bytes_per_row: u32,
    #[cfg(not(target_arch = "wasm32"))]
    submission: crate::submission_clock::ClockedSubmission,
    #[cfg(not(target_arch = "wasm32"))]
    timeout: std::time::Duration,
    staging_bytes: u64,
    image_bytes: u64,
    canary: Canary,
    mapping_requested: AtomicBool,
}

impl Rgba8Readback {
    /// Test-only: expect a different canary, simulating a stale staging buffer.
    #[cfg(test)]
    pub(crate) fn expecting(mut self, canary: Canary) -> Self {
        self.canary = canary;
        self
    }

    /// The pixel dimensions of the result.
    #[must_use]
    pub const fn size(&self) -> RenderSize {
        self.size
    }

    /// Bytes held by the staging buffer.
    #[must_use]
    pub const fn staging_bytes(&self) -> u64 {
        self.staging_bytes
    }

    /// Blocks a native caller until this readback is mapped, without draining the whole queue.
    ///
    /// # Errors
    /// [`RenderError::TextureReadback`] when mapping or device polling fails.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn wait_ready(&self) -> Result<(), RenderError> {
        let (sender, receiver) = mpsc::channel();
        self.map_async(move |mapped| {
            let _ = sender.send(mapped);
        });
        self.submission.wait(&self.gpu, self.timeout)?;
        receive_mapped(&self.gpu, &receiver, self.timeout)?
    }

    /// Waits for this readback and returns tightly packed RGBA8 pixels.
    ///
    /// # Errors
    /// [`RenderError::TextureReadback`] when mapping, polling or row packing fails.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn wait(self) -> Result<Vec<u8>, RenderError> {
        self.wait_ready()?;
        self.finish()
    }

    /// Requests buffer mapping and reports readiness exactly once.
    ///
    /// A refused or repeated request reports a failure, which callers must treat as a failed frame.
    pub fn map_async(&self, on_ready: impl FnOnce(Result<(), RenderError>) + Send + 'static) {
        map_staging(&self.gpu, &self.buffer, &self.mapping_requested, on_ready);
    }

    /// Polls backends that need it to resolve a mapping; browser `WebGPU` ignores this.
    pub fn poll(&self) {
        let _ = self.gpu.device().poll(wgpu::PollType::Poll);
    }

    /// Packs the mapped rows into tightly packed premultiplied RGBA8.
    ///
    /// # Errors
    /// [`RenderError::TextureReadback`] when the buffer is not mapped, and
    /// [`RenderError::GpuWorkDropped`] when the GPU did not run the copy; the buffer is then not
    /// recycled and the renderer refuses further work.
    pub fn finish(self) -> Result<Vec<u8>, RenderError> {
        let Ok(view) = self.buffer.buffer().slice(..).get_mapped_range() else {
            // Unmapping an unmapped buffer is a validation error. Dropping the lease destroys the
            // buffer and ends any pending mapping.
            return Err(RenderError::TextureReadback(ReadbackFailure::NotMapped));
        };
        let image_bytes = usize::try_from(self.image_bytes)
            .map_err(|_| RenderError::TextureReadback(ReadbackFailure::Layout))?;
        let checked = view
            .get(image_bytes..)
            .ok_or(RenderError::TextureReadback(ReadbackFailure::Layout))
            .and_then(|found| self.canary.check(found));
        if let Err(error) = checked {
            drop(view);
            self.buffer.buffer().unmap();
            return Err(error);
        }
        let mapped = &view[..image_bytes];
        let padded_row = usize::try_from(self.padded_bytes_per_row)
            .map_err(|_| RenderError::TextureReadback(ReadbackFailure::Layout))?;
        let unpadded_row = usize::try_from(self.unpadded_bytes_per_row)
            .map_err(|_| RenderError::TextureReadback(ReadbackFailure::Layout))?;
        let packed = if padded_row == unpadded_row {
            mapped.to_vec()
        } else {
            let capacity = unpadded_row
                .checked_mul(
                    usize::try_from(self.size.height)
                        .map_err(|_| RenderError::TextureReadback(ReadbackFailure::Layout))?,
                )
                .ok_or(RenderError::TextureReadback(ReadbackFailure::Layout))?;
            let mut packed = Vec::with_capacity(capacity);
            for row in mapped.chunks_exact(padded_row) {
                packed.extend_from_slice(&row[..unpadded_row]);
            }
            packed
        };
        drop(view);
        self.buffer.buffer().unmap();
        self.buffer.recycle();
        Ok(packed)
    }
}

/// GPU state an NV12 pack recorded into a caller's encoder still needs.
///
/// Drop it only after that encoder is submitted; dropping earlier lets a later renderer call
/// overwrite the pack's uniforms.
#[must_use = "hold the pack until its encoder is submitted"]
#[derive(Debug)]
#[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
pub struct EncodedNv12Pack {
    uniforms: Option<crate::uniform::UniformArenaLease>,
    /// Defers destruction of released pool resources until the pack is dropped.
    _encode: crate::retire::EncodeScope,
}

impl Drop for EncodedNv12Pack {
    fn drop(&mut self) {
        if let Some(uniforms) = self.uniforms.take() {
            uniforms.recycle();
        }
    }
}

/// Default views of caller-owned NV12 plane textures for a readback-free pack.
///
/// `luma` views an `R8Unorm` texture at the output size; `chroma` an `Rg8Unorm` texture at half
/// size with U in red and V in green. Keep the views with their textures: a pack creates none.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
pub struct Nv12PlaneTargets<'a> {
    /// Full-size single-channel luma target.
    pub luma: &'a wgpu::TextureView,
    /// Half-size two-channel interleaved chroma target.
    pub chroma: &'a wgpu::TextureView,
}

impl Nv12PlaneTargets<'_> {
    pub(crate) fn check(self, size: RenderSize) -> Result<(), RenderError> {
        let attachment = |texture: &wgpu::Texture| {
            texture
                .usage()
                .contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
                && texture.mip_level_count() == 1
                && texture.sample_count() == 1
                && texture.depth_or_array_layers() == 1
        };
        let (luma, chroma) = (self.luma.texture(), self.chroma.texture());
        if luma.format() != wgpu::TextureFormat::R8Unorm
            || chroma.format() != wgpu::TextureFormat::Rg8Unorm
            || luma.width() != size.width
            || luma.height() != size.height
            || chroma.width() != size.width / 2
            || chroma.height() != size.height / 2
            || !attachment(luma)
            || !attachment(chroma)
        {
            return Err(RenderError::InvalidPlaneTarget);
        }
        Ok(())
    }
}

/// A caller-owned destination for one packed plane.
#[derive(Debug)]
pub struct PlaneTarget<'a> {
    pub(crate) bytes: &'a mut [u8],
    pub(crate) stride: usize,
}

impl<'a> PlaneTarget<'a> {
    /// Describes plane memory whose rows start `stride` bytes apart.
    #[must_use]
    pub fn new(bytes: &'a mut [u8], stride: usize) -> Self {
        Self { bytes, stride }
    }

    /// The plane memory.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.bytes
    }

    /// Reborrows the plane memory.
    pub fn reborrow(&mut self) -> PlaneTarget<'_> {
        PlaneTarget {
            bytes: self.bytes,
            stride: self.stride,
        }
    }

    pub(crate) fn fits(&self, width: usize, height: usize) -> Result<(), RenderError> {
        if self.stride < width {
            return Err(RenderError::TextureReadback(
                ReadbackFailure::DestinationTooSmall,
            ));
        }
        // The last row needs only its visible width, as `FFmpeg` sizes a frame's final plane.
        let needed = self
            .stride
            .checked_mul(height.saturating_sub(1))
            .and_then(|rows| rows.checked_add(width))
            .ok_or(RenderError::TextureReadback(
                ReadbackFailure::DestinationTooSmall,
            ))?;
        if self.bytes.len() < needed {
            return Err(RenderError::TextureReadback(
                ReadbackFailure::DestinationTooSmall,
            ));
        }
        Ok(())
    }

    fn row(&mut self, index: usize, width: usize) -> Result<&mut [u8], RenderError> {
        let start = index
            .checked_mul(self.stride)
            .ok_or(RenderError::TextureReadback(
                ReadbackFailure::DestinationTooSmall,
            ))?;
        let end = start
            .checked_add(width)
            .ok_or(RenderError::TextureReadback(
                ReadbackFailure::DestinationTooSmall,
            ))?;
        self.bytes
            .get_mut(start..end)
            .ok_or(RenderError::TextureReadback(
                ReadbackFailure::DestinationTooSmall,
            ))
    }
}

/// Destinations for the three planes of one packed frame.
#[derive(Debug)]
pub struct Yuv420pTargets<'a> {
    /// Full-resolution luma.
    pub y: PlaneTarget<'a>,
    /// Half-resolution blue-difference chroma.
    pub u: PlaneTarget<'a>,
    /// Half-resolution red-difference chroma.
    pub v: PlaneTarget<'a>,
}

/// Tightly packed planar YUV420 data produced by a GPU readback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Yuv420pPlanes {
    /// Full-resolution luma samples.
    pub y: Vec<u8>,
    /// Half-width, half-height blue-difference chroma samples.
    pub u: Vec<u8>,
    /// Half-width, half-height red-difference chroma samples.
    pub v: Vec<u8>,
}

/// A submitted GPU pack of a frame into YUV planes whose buffer is not mapped yet.
#[derive(Debug)]
pub struct Yuv420pReadback {
    buffer: ReadbackBufferLease,
    gpu: GpuContext,
    size: RenderSize,
    layout: StagingLayout,
    pixel_layout: Option<PixelLayout>,
    #[cfg(not(target_arch = "wasm32"))]
    submission: crate::submission_clock::ClockedSubmission,
    #[cfg(not(target_arch = "wasm32"))]
    timeout: std::time::Duration,
    staging_bytes: u64,
    canary: Canary,
    mapping_requested: AtomicBool,
}

impl Yuv420pReadback {
    /// The luma-plane dimensions.
    #[must_use]
    pub const fn size(&self) -> RenderSize {
        self.size
    }

    /// Bytes held by the staging buffer.
    #[must_use]
    pub const fn staging_bytes(&self) -> u64 {
        self.staging_bytes
    }

    /// Blocks a native caller until this readback is mapped.
    ///
    /// # Errors
    /// Returns [`RenderError::TextureReadback`] when mapping or polling fails.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn wait_ready(&self) -> Result<(), RenderError> {
        let (sender, receiver) = mpsc::channel();
        self.map_async(move |mapped| {
            let _ = sender.send(mapped);
        });
        self.wait_for_submission()?;
        self.wait_for_mapped(&receiver)?
    }

    /// Waits for what a [`Self::map_async`] callback sends to `receiver`, after
    /// [`Self::wait_for_submission`], polling the device, for at most the renderer's wait timeout.
    ///
    /// # Errors
    /// [`RenderError::GpuTimeout`] when nothing arrives in time, and
    /// [`ReadbackFailure::CallbackDropped`] when the callback was dropped without sending.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn wait_for_mapped<T>(&self, receiver: &mpsc::Receiver<T>) -> Result<T, RenderError> {
        receive_mapped(&self.gpu, receiver, self.timeout)
    }

    /// Blocks until this readback's submission completes; may follow an earlier
    /// [`Self::map_async`].
    ///
    /// # Errors
    /// [`RenderError::GpuTimeout`] past the renderer's wait timeout, or
    /// [`RenderError::TextureReadback`] when device polling fails.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn wait_for_submission(&self) -> Result<(), RenderError> {
        self.submission.wait(&self.gpu, self.timeout)?;
        Ok(())
    }

    /// Waits for this readback and returns tightly packed planes.
    ///
    /// # Errors
    /// [`RenderError::TextureReadback`] when mapping, polling or row packing fails.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn wait(self) -> Result<Yuv420pPlanes, RenderError> {
        self.wait_ready()?;
        self.finish()
    }

    /// Requests buffer mapping and reports readiness exactly once.
    pub fn map_async(&self, on_ready: impl FnOnce(Result<(), RenderError>) + Send + 'static) {
        map_staging(&self.gpu, &self.buffer, &self.mapping_requested, on_ready);
    }

    /// Polls backends that need it to resolve a mapping.
    pub fn poll(&self) {
        let _ = self.gpu.device().poll(wgpu::PollType::Poll);
    }

    /// Packs mapped planes into caller-owned, possibly strided, plane buffers, avoiding the
    /// allocation and copy of [`Yuv420pReadback::finish`].
    ///
    /// # Errors [`RenderError::TextureReadback`] when the buffer is not mapped, the layout is
    /// unrepresentable or a target is too small, and [`RenderError::GpuWorkDropped`] when the GPU
    /// did not run the copy (the buffer is not recycled and the renderer refuses further work).
    pub fn finish_into(self, targets: &mut Yuv420pTargets<'_>) -> Result<(), RenderError> {
        if self.pixel_layout.is_some() {
            return Err(RenderError::TextureReadback(ReadbackFailure::Layout));
        }
        self.copy_planes(&mut [&mut targets.y, &mut targets.u, &mut targets.v])
    }

    fn copy_planes(self, targets: &mut [&mut PlaneTarget<'_>]) -> Result<(), RenderError> {
        if targets.len() != self.layout.planes.len() {
            return Err(RenderError::TextureReadback(ReadbackFailure::Layout));
        }
        let Ok(mapped) = self.buffer.buffer().slice(..).get_mapped_range() else {
            // Not mapped, so nothing to unmap.
            return Err(RenderError::TextureReadback(ReadbackFailure::NotMapped));
        };
        let result = (|| {
            self.check_canary(&mapped)?;
            for (plane, target) in self.layout.planes.iter().zip(targets.iter_mut()) {
                let width = usize::try_from(plane.row_bytes)
                    .map_err(|_| RenderError::TextureReadback(ReadbackFailure::Layout))?;
                let rows = usize::try_from(plane.rows)
                    .map_err(|_| RenderError::TextureReadback(ReadbackFailure::Layout))?;
                target.fits(width, rows)?;
                for (index, row) in plane_rows(&mapped, *plane)?.enumerate() {
                    target.row(index, width)?.copy_from_slice(&row[..width]);
                }
            }
            Ok(())
        })();
        drop(mapped);
        self.buffer.buffer().unmap();
        if result.is_ok() {
            self.buffer.recycle();
        }
        result
    }

    /// Packs mapped planes into three tightly packed planes.
    ///
    /// # Errors
    /// As [`Self::finish_into`].
    pub fn finish(self) -> Result<Yuv420pPlanes, RenderError> {
        let planes = self.unpack(|[y, u, v]| Yuv420pPlanes { y, u, v })?;
        Ok(planes)
    }

    /// Plane offsets and strides in the bytes [`Self::finish_mapped`] hands over, in storage
    /// order.
    #[must_use]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn plane_layout(&self) -> Vec<StagingPlane> {
        self.layout
            .planes
            .iter()
            .map(|plane| StagingPlane {
                offset: plane.offset,
                stride: plane.pitch,
            })
            .collect()
    }

    /// Hands the mapped staging buffer to `read`, then unmaps it.
    ///
    /// Planes are at the offsets and strides [`Self::plane_layout`] reports, rows padded to the
    /// copy alignment. A strided consumer such as a `WebCodecs` `VideoFrame` can copy straight from
    /// the mapping; in a browser that copy can stay in JavaScript (`BufferView::as_uint8array`).
    ///
    /// # Errors
    /// As [`Self::finish_into`]; `read` is not called then.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn finish_mapped<T>(
        self,
        read: impl FnOnce(&wgpu::BufferView) -> T,
    ) -> Result<T, RenderError> {
        let Ok(canary) = self
            .buffer
            .buffer()
            .slice(self.image_bytes()..)
            .get_mapped_range()
        else {
            // Not mapped, so nothing to unmap.
            return Err(RenderError::TextureReadback(ReadbackFailure::NotMapped));
        };
        let checked = self.canary.check(&canary);
        drop(canary);
        if let Err(error) = checked {
            self.buffer.buffer().unmap();
            return Err(error);
        }
        let Ok(mapped) = self
            .buffer
            .buffer()
            .slice(..self.image_bytes())
            .get_mapped_range()
        else {
            self.buffer.buffer().unmap();
            return Err(RenderError::TextureReadback(ReadbackFailure::NotMapped));
        };
        let result = read(&mapped);
        drop(mapped);
        self.buffer.buffer().unmap();
        self.buffer.recycle();
        Ok(result)
    }

    /// Copies the three planes out tightly packed and hands them to `pack`.
    fn unpack<T>(self, pack: impl FnOnce([Vec<u8>; 3]) -> T) -> Result<T, RenderError> {
        if self.pixel_layout.is_some() {
            return Err(RenderError::TextureReadback(ReadbackFailure::Layout));
        }
        let Ok(mapped) = self.buffer.buffer().slice(..).get_mapped_range() else {
            // Not mapped, so nothing to unmap.
            return Err(RenderError::TextureReadback(ReadbackFailure::NotMapped));
        };
        if let Err(error) = self.check_canary(&mapped) {
            drop(mapped);
            self.buffer.buffer().unmap();
            return Err(error);
        }
        let [y, u, v] = [0, 1, 2].map(|index| self.layout.planes[index]);
        let planes = [y, u, v].map(|plane| {
            let width = usize::try_from(plane.row_bytes)
                .map_err(|_| RenderError::TextureReadback(ReadbackFailure::Layout))?;
            let rows = usize::try_from(plane.rows)
                .map_err(|_| RenderError::TextureReadback(ReadbackFailure::Layout))?;
            plane_rows(&mapped, plane).map(|source| {
                let mut tight = Vec::with_capacity(width * rows);
                for row in source {
                    tight.extend_from_slice(&row[..width]);
                }
                tight
            })
        });
        drop(mapped);
        self.buffer.buffer().unmap();
        let [y, u, v] = planes;
        let planes = [y?, u?, v?];
        self.buffer.recycle();
        Ok(pack(planes))
    }

    /// Bytes before the canary.
    fn image_bytes(&self) -> u64 {
        self.layout.bytes
    }

    /// The layout a layout readback ([`Renderer::begin_layout_readback`]) packed, `None` for an
    /// 8-bit 4:2:0 readback.
    #[must_use]
    pub fn pixel_layout(&self) -> Option<PixelLayout> {
        self.pixel_layout
    }

    /// Copies the mapped planes into one target per plane, in storage order
    /// ([`PixelLayout::planes`], or Y, U, V for 8-bit 4:2:0).
    ///
    /// # Errors
    /// As [`Self::finish_into`], and [`RenderError::TextureReadback`] for a wrong target count.
    pub fn finish_into_planes(self, targets: &mut [PlaneTarget<'_>]) -> Result<(), RenderError> {
        self.copy_planes(&mut targets.iter_mut().collect::<Vec<_>>())
    }

    /// Checks that the mapped buffer ends in this readback's canary; a failing buffer is not
    /// recycled.
    fn check_canary(&self, mapped: &[u8]) -> Result<(), RenderError> {
        let start = usize::try_from(self.image_bytes())
            .map_err(|_| RenderError::TextureReadback(ReadbackFailure::Layout))?;
        let found = mapped
            .get(start..)
            .ok_or(RenderError::TextureReadback(ReadbackFailure::Layout))?;
        self.canary.check(found)
    }
}

/// A map callback runs on whichever thread's poll collected the mapping, possibly after this
/// thread's own wait returned.
#[cfg(not(target_arch = "wasm32"))]
fn receive_mapped<T>(
    gpu: &GpuContext,
    receiver: &mpsc::Receiver<T>,
    timeout: std::time::Duration,
) -> Result<T, RenderError> {
    crate::settle::receive_polling(receiver, timeout, || {
        let _ = gpu.device().poll(wgpu::PollType::Poll);
    })
}

/// How the next [`map_staging`] on this thread delivers its callback in tests.
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum MapReport {
    /// From another thread, after this delay.
    Elsewhere(std::time::Duration),
    Never,
}

#[cfg(test)]
thread_local! {
    pub(crate) static MAP_REPORT: std::cell::Cell<Option<MapReport>> =
        const { std::cell::Cell::new(None) };
}

fn map_staging(
    gpu: &GpuContext,
    buffer: &ReadbackBufferLease,
    requested: &AtomicBool,
    on_ready: impl FnOnce(Result<(), RenderError>) + Send + 'static,
) {
    #[cfg(test)]
    let on_ready = {
        let report = MAP_REPORT.with(std::cell::Cell::take);
        move |result: Result<(), RenderError>| match report {
            None => on_ready(result),
            Some(MapReport::Elsewhere(delay)) => {
                std::thread::spawn(move || {
                    std::thread::sleep(delay);
                    on_ready(result);
                });
            }
            Some(MapReport::Never) => std::mem::forget(on_ready),
        }
    };
    if requested.swap(true, Ordering::AcqRel) {
        on_ready(Err(RenderError::TextureReadback(
            ReadbackFailure::DuplicateMapRequest,
        )));
        return;
    }
    gpu.map_async(
        &buffer.buffer().slice(..),
        wgpu::MapMode::Read,
        move |result| {
            // `BufferAsyncError` names no cause; device diagnostics may explain it.
            on_ready(result.map_err(|_| RenderError::TextureReadback(ReadbackFailure::MapFailed)));
        },
    );
}

/// Where one plane sits in a mapped readback staging buffer.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct StagingPlane {
    /// Byte offset of the plane's first row.
    pub offset: u64,
    /// Bytes from the start of one row to the start of the next.
    pub stride: u32,
}

fn aligned_copy_row(width: u32) -> Result<u32, RenderError> {
    let alignment = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    width
        .checked_add(alignment - 1)
        .map(|value| value / alignment)
        .and_then(|rows| rows.checked_mul(alignment))
        .ok_or(RenderError::TextureReadback(ReadbackFailure::Layout))
}

fn yuv420_tight_bytes(size: RenderSize) -> Result<u64, RenderError> {
    u64::from(size.width)
        .checked_mul(u64::from(size.height))
        .and_then(|luma| luma.checked_mul(YUV420_PLANE_COUNT))
        .map(|three_planes| three_planes / 2)
        .ok_or(RenderError::TextureReadback(ReadbackFailure::Layout))
}

/// Pack-shader uniforms for `encoding`: luma weights, range levels and chroma co-siting axes,
/// from the same definitions import decodes with so a round trip is the identity.
const YUV_PACK_UNIFORM_FLOATS: usize = 12;

pub(crate) const YUV_PACK_UNIFORM_BYTES: u64 =
    crate::uniform::float_uniform_bytes(YUV_PACK_UNIFORM_FLOATS);

pub(crate) fn yuv_uniform(encoding: YuvEncoding) -> [f32; YUV_PACK_UNIFORM_FLOATS] {
    let YuvEncoding { color, chroma } = encoding;
    // Components that are already RGB have no pack: they take BT.709's.
    let (kr, kb) = color
        .matrix
        .luma_weights()
        .or(ColorMatrix::Bt709.luma_weights())
        .expect("BT.709 is a YCbCr matrix");
    let levels = color.range.levels(8);
    let (cosited_x, cosited_y) = match chroma {
        ChromaLocation::Left => (true, false),
        ChromaLocation::TopLeft => (true, true),
        ChromaLocation::Unspecified | ChromaLocation::Center => (false, false),
    };
    [
        kr,
        1.0 - kr - kb,
        kb,
        0.0,
        levels.luma_black / levels.maximum,
        levels.luma_span / levels.maximum,
        levels.chroma_center / levels.maximum,
        levels.chroma_span / levels.maximum,
        f32::from(u8::from(cosited_x)),
        f32::from(u8::from(cosited_y)),
        0.0,
        0.0,
    ]
}

fn yuv_render_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    fragment_entry: &'static str,
    target_formats: &[wgpu::TextureFormat],
) -> wgpu::RenderPipeline {
    let targets = target_formats
        .iter()
        .copied()
        .map(|format| Some(format.into()))
        .collect::<Vec<_>>();
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("alpha2 RGB to YUV pipeline"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs"),
            buffers: &[],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some(fragment_entry),
            targets: &targets,
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

type PackPass<'a> = (
    &'static str,
    &'a wgpu::RenderPipeline,
    &'a [&'a wgpu::TextureView],
);

fn encode_pack_passes(
    encoder: &mut impl PassEncoder,
    bind_group: &OffsetBindGroup<1>,
    size: RenderSize,
    luma: PackPass<'_>,
    chroma: PackPass<'_>,
) {
    let chroma_size = RenderSize {
        width: size.width.div_ceil(2),
        height: size.height.div_ceil(2),
    };
    let pass = crate::submission_pilot::PassKind::Pack.key(size.pixels());
    for ((label, pipeline, targets), work, size) in [
        (luma, pack_luma_work(size), size),
        (chroma, pack_chroma_work(size), chroma_size),
    ] {
        encoder.placed_pass(work, PixelRect::whole(size), pass, |encoder, piece| {
            encode_yuv_render_pass(encoder, label, pipeline, bind_group, targets, piece);
        });
    }
}

pub(crate) fn pack_luma_work(size: RenderSize) -> crate::FrameCost {
    crate::FrameCost::of(PACK_SAMPLE_COST, size.pixels())
}

pub(crate) fn pack_chroma_work(size: RenderSize) -> crate::FrameCost {
    let chroma = u64::from(size.width.div_ceil(2)) * u64::from(size.height.div_ceil(2));
    crate::FrameCost::of(PACK_SAMPLE_COST.times(4), chroma)
}

pub(crate) fn layout_chroma_size(size: RenderSize, layout: PixelLayout) -> RenderSize {
    let (step_x, step_y) = layout.chroma.subsampling();
    RenderSize {
        width: size.width / step_x,
        height: size.height / step_y,
    }
}

pub(crate) fn layout_luma_work(size: RenderSize) -> crate::FrameCost {
    crate::FrameCost::of(PACK_SAMPLE_COST, size.pixels())
}

pub(crate) fn layout_chroma_work(size: RenderSize, layout: PixelLayout) -> crate::FrameCost {
    let (step_x, step_y) = layout.chroma.subsampling();
    crate::FrameCost::of(
        PACK_SAMPLE_COST.times(u64::from(step_x * step_y)),
        layout_chroma_size(size, layout).pixels(),
    )
}

pub(crate) fn layout_copy_work(size: RenderSize, layout: PixelLayout) -> crate::FrameCost {
    let [luma, chroma] = LayoutPackPass::of_layout(layout);
    let texels = luma.target_formats().len() as u64 * size.pixels()
        + chroma.target_formats().len() as u64 * layout_chroma_size(size, layout).pixels();
    crate::FrameCost::of(PACK_SAMPLE_COST, texels)
}

pub(crate) fn layout_pack_work(size: RenderSize, layout: PixelLayout) -> crate::FrameCost {
    let mut work = layout_luma_work(size);
    work.add_cost(layout_chroma_work(size, layout));
    work.add_cost(layout_copy_work(size, layout));
    work
}

pub(crate) const LAYOUT_PACK_UNIFORM_BYTES: u64 =
    (crate::layout_pack::GPU_PARAMS_WORDS * size_of::<u32>()) as u64;

const PACK_SAMPLE_COST: crate::effect_cost::EffectCost =
    crate::effect_cost::EffectCost::fixed(1, 0);

fn encode_yuv_render_pass(
    encoder: &mut wgpu::CommandEncoder,
    label: &'static str,
    pipeline: &wgpu::RenderPipeline,
    bind_group: &OffsetBindGroup<1>,
    targets: &[&wgpu::TextureView],
    Piece { scissor, first }: Piece,
) {
    let load = if first {
        wgpu::LoadOp::Clear(wgpu::Color::BLACK)
    } else {
        wgpu::LoadOp::Load
    };
    let attachments = targets
        .iter()
        .map(|target| {
            Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load,
                    store: wgpu::StoreOp::Store,
                },
            })
        })
        .collect::<arrayvec::ArrayVec<_, 2>>();
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &attachments,
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    if let Some(scissor) = scissor {
        if scissor.area() == 0 {
            return;
        }
        pass.set_scissor_rect(scissor.x, scissor.y, scissor.width, scissor.height);
    }
    pass.set_pipeline(pipeline);
    bind_group.set(&mut pass, 0);
    pass.draw(0..3, 0..1);
}

fn encode_plane_copy(
    encoder: &mut wgpu::CommandEncoder,
    texture: &wgpu::Texture,
    buffer: &wgpu::Buffer,
    offset: u64,
    bytes_per_row: u32,
    size: RenderSize,
) {
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(size.height),
            },
        },
        wgpu::Extent3d {
            width: size.width,
            height: size.height,
            depth_or_array_layers: 1,
        },
    );
}

#[cfg(test)]
pub(crate) fn read_rgba8(renderer: &Renderer, target: &RenderedTexture) -> Vec<u8> {
    renderer
        .read_rgba8(target)
        .expect("test texture readback succeeds")
}

#[cfg(test)]
#[path = "readback_tests.rs"]
mod layout_tests;

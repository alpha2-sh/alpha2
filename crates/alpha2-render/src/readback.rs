use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc;

use crate::canary::{CANARY_BYTES, Canary};
use crate::{
    ReadbackFailure, RenderError, RenderSize, RenderedTexture, Renderer,
    pipeline::{CachedPipeline, CachedYuvReadbackPipeline},
    pool::{ReadbackBufferLease, TextureLease},
    uniform::{UniformArenaLease, f32_bytes},
};
use alpha2_gpu::GpuContext;
use alpha2_media::{ChromaLocation, ColorMatrix, YuvEncoding};
use alpha2_shaders::{READBACK_PACK_WGSL, RGB_TO_YUV420_WGSL};

const RGBA8_BYTES_PER_PIXEL: u32 = 4;
const RGBA16F_BYTES_PER_PIXEL: u32 = 8;
const YUV420_PLANE_COUNT: u64 = 3;

/// D3D12's `D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT`: a texture-to-buffer copy
/// must start on it, and wgpu's DX12 backend works around an unaligned start
/// with an intermediate copy whose padded rows overwrite whatever follows.
const PLANE_PLACEMENT_ALIGNMENT: u64 = 512;

/// Where each plane of one I420 readback sits in its staging buffer.
///
/// Each plane starts on [`PLANE_PLACEMENT_ALIGNMENT`] with copy-aligned rows.
/// Separate regions prevent DX12's padded-copy workaround from overwriting
/// adjacent chroma planes when unrestricted copy pitches are unavailable.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct Yuv420StagingLayout {
    luma_pitch: u32,
    chroma_pitch: u32,
    u_offset: u64,
    v_offset: u64,
    bytes: u64,
}

impl Yuv420StagingLayout {
    pub(crate) fn new(size: RenderSize) -> Result<Self, RenderError> {
        let luma_pitch = aligned_copy_row(size.width)?;
        let chroma_pitch = aligned_copy_row(size.width / 2)?;
        let region = |pitch: u32, rows: u32| u64::from(pitch).checked_mul(u64::from(rows));
        let place = |end: u64| end.checked_next_multiple_of(PLANE_PLACEMENT_ALIGNMENT);
        let chroma_bytes = region(chroma_pitch, size.height / 2)
            .ok_or(RenderError::TextureReadback(ReadbackFailure::Layout))?;
        let u_offset = region(luma_pitch, size.height)
            .and_then(place)
            .ok_or(RenderError::TextureReadback(ReadbackFailure::Layout))?;
        let v_offset = u_offset
            .checked_add(chroma_bytes)
            .and_then(place)
            .ok_or(RenderError::TextureReadback(ReadbackFailure::Layout))?;
        let bytes = v_offset
            .checked_add(chroma_bytes)
            .ok_or(RenderError::TextureReadback(ReadbackFailure::Layout))?;
        Ok(Self {
            luma_pitch,
            chroma_pitch,
            u_offset,
            v_offset,
            bytes,
        })
    }

    /// The plane regions as `(offset, pitch, width, rows)`, in Y, U, V order.
    fn planes(self, size: RenderSize) -> [(u64, u32, u32, u32); 3] {
        let (chroma_width, chroma_rows) = (size.width / 2, size.height / 2);
        [
            (0, self.luma_pitch, size.width, size.height),
            (self.u_offset, self.chroma_pitch, chroma_width, chroma_rows),
            (self.v_offset, self.chroma_pitch, chroma_width, chroma_rows),
        ]
    }
}

/// The bytes one planar YUV420 readback of `size` stages: each plane's rows
/// padded to the copy alignment, each plane starting on a placement boundary,
/// and the 16-byte canary that proves the GPU wrote them. This is what an
/// in-flight readback holds, and what a budget must reserve for it.
///
/// # Errors
/// Returns [`RenderError::TextureReadback`] when the layout overflows.
pub fn yuv420p_staging_bytes(size: RenderSize) -> Result<u64, RenderError> {
    Yuv420StagingLayout::new(size).map(|layout| layout.bytes + CANARY_BYTES)
}

/// The rows of one plane region of a mapped staging buffer, `width` bytes each.
fn plane_rows(
    mapped: &[u8],
    (offset, pitch, width, rows): (u64, u32, u32, u32),
) -> Result<std::iter::Take<std::slice::ChunksExact<'_, u8>>, RenderError> {
    let offset = usize::try_from(offset)
        .map_err(|_| RenderError::TextureReadback(ReadbackFailure::Layout))?;
    let pitch = usize::try_from(pitch)
        .map_err(|_| RenderError::TextureReadback(ReadbackFailure::Layout))?;
    let width = usize::try_from(width)
        .map_err(|_| RenderError::TextureReadback(ReadbackFailure::Layout))?;
    let rows =
        usize::try_from(rows).map_err(|_| RenderError::TextureReadback(ReadbackFailure::Layout))?;
    let region = pitch
        .checked_mul(rows)
        .and_then(|length| length.checked_add(offset))
        .and_then(|end| mapped.get(offset..end))
        .filter(|_| width <= pitch)
        .ok_or(RenderError::TextureReadback(ReadbackFailure::Layout))?;
    Ok(region.chunks_exact(pitch).take(rows))
}

pub(crate) struct EncodedYuv420pReadback {
    buffer: ReadbackBufferLease,
    size: RenderSize,
    layout: Yuv420StagingLayout,
    staging_bytes: u64,
    canary: Canary,
    y_texture: TextureLease,
    u_texture: TextureLease,
    v_texture: TextureLease,
    uniforms: UniformArenaLease,
}

impl EncodedYuv420pReadback {
    pub(crate) fn flush_uniforms(&self, queue: &wgpu::Queue) {
        self.uniforms.flush(queue);
    }
}

impl Renderer {
    /// Uploads tightly packed, premultiplied sRGB RGBA8 pixels for shader
    /// sampling, into a texture from the renderer's upload pool.
    ///
    /// # Errors
    /// Returns [`RenderError::InvalidImageData`] when `pixels` does not contain
    /// exactly four bytes per declared pixel.
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

    /// Uploads tightly packed premultiplied linear-light RGBA as IEEE 754 half
    /// floats (their bits), in BT.709 primaries with `1.0` at SDR white, for
    /// [`Renderer::prepare_linear_presented`].
    ///
    /// # Errors
    /// Returns [`RenderError::InvalidImageData`] when `pixels` does not contain
    /// exactly four halves per declared pixel.
    pub fn upload_linear_rgba16f(
        &self,
        size: RenderSize,
        pixels: &[u16],
    ) -> Result<crate::UploadTexture, RenderError> {
        // The halves already are little-endian rows on every little-endian target; a copy
        // would cost a frame-sized allocation per OpenEXR frame.
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

    /// Writes rows of little-endian RGBA half floats `plane.stride` bytes
    /// apart into a pooled upload texture.
    ///
    /// # Errors
    /// Returns [`RenderError::InvalidImageData`] for a plane shorter than
    /// `size`.
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

    /// Writes rows of RGBA8 pixels `plane.stride` bytes apart into a pooled
    /// upload texture.
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

    /// Copies one RGBA8 render result into tightly packed host memory.
    ///
    /// The returned pixels retain alpha2's premultiplied-alpha convention.
    ///
    /// This blocks on GPU completion, so it exists only for native and test
    /// callers. A caller that cannot block uses
    /// [`Renderer::begin_rgba8_readback`] and maps on its event loop: no
    /// browser thread may wait on the device, and a wait would never see the
    /// mapping resolve.
    ///
    /// Half-float negotiated intermediates are clamped and quantized to RGBA8
    /// by a cached GPU pass before mapping.
    ///
    /// # Errors
    /// Returns [`RenderError::TextureReadback`] for another target format or
    /// when the GPU buffer cannot be mapped.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn read_rgba8(&self, target: &RenderedTexture) -> Result<Vec<u8>, RenderError> {
        self.begin_rgba8_readback(target)?.wait()
    }

    /// Submits a readback copy and returns a handle that has not been mapped.
    ///
    /// The copy is already queued when this returns, so the caller only has to
    /// wait for [`Rgba8Readback::map_async`] to report readiness: mapping can
    /// resolve on an event loop instead of blocking, and the result never
    /// travels through a presentation surface. RGBA8 readbacks serve stills,
    /// thumbnails and golden images. No export reads RGBA8: a browser export
    /// reads [`Renderer::begin_yuv420p_readback`]'s planes through
    /// [`Yuv420pReadback::map_async`] and the `wgpu` interop's
    /// `Yuv420pReadback::finish_mapped`, and a native one packs YUV or NV12.
    ///
    /// # Errors
    /// Returns [`RenderError::TextureReadback`] for an unsupported target
    /// format or a row layout that cannot be represented.
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
        // The rows, then the canary that proves the GPU wrote them.
        let buffer_size = image_bytes
            .checked_add(CANARY_BYTES)
            .ok_or(RenderError::TextureReadback(ReadbackFailure::Layout))?;
        let mut encoder = self.command_encoder("alpha2 RGBA8 readback encoder");
        let buffer =
            self.readback_buffers
                .acquire(self.gpu.device(), buffer_size, "alpha2 RGBA8 readback");
        let packed_texture = self.encode_readback_pack(target, &mut encoder);
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
        // After the copy, in the same encoder: it lands only if the copy ran.
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

    /// Converts a render result to planar 8-bit YUV420 on the GPU and submits
    /// one asynchronous readback.
    ///
    /// The output size must be even because both chroma planes are subsampled
    /// by two in each dimension. Each chroma sample is filtered for where
    /// `encoding.chroma` sites it: the location the output codec implies
    /// ([`alpha2_media::VideoCodec::chroma_location`]).
    ///
    /// # Errors
    /// Returns [`RenderError::TextureReadback`] for unsupported formats, odd
    /// dimensions, or layouts that cannot be represented.
    pub fn begin_yuv420p_readback(
        &self,
        target: &RenderedTexture,
        encoding: YuvEncoding,
    ) -> Result<Yuv420pReadback, RenderError> {
        let mut command_encoder = self.command_encoder("alpha2 YUV420P readback encoder");
        let readback_work = self.encode_yuv420p_readback(target, encoding, &mut command_encoder)?;
        readback_work.flush_uniforms(self.gpu.queue());
        #[cfg(not(target_arch = "wasm32"))]
        let submission = self.submit([command_encoder.finish()])?;
        #[cfg(target_arch = "wasm32")]
        self.submit([command_encoder.finish()])?;
        Ok(self.complete_yuv420p_readback(
            readback_work,
            #[cfg(not(target_arch = "wasm32"))]
            submission,
            true,
        ))
    }

    pub(crate) fn encode_yuv420p_readback(
        &self,
        target: &RenderedTexture,
        encoding: YuvEncoding,
        encoder: &mut wgpu::CommandEncoder,
    ) -> Result<EncodedYuv420pReadback, RenderError> {
        self.ensure_gpu_runs_work()?;
        self.validate_size(target.size)?;
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
        let layout = Yuv420StagingLayout::new(target.size)?;
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
        let uniform_bytes = f32_bytes(yuv_uniform(encoding));
        let mut uniforms = self.uniform_buffers.acquire(
            self.gpu.device(),
            u64::try_from(uniform_bytes.len()).unwrap_or(u64::MAX),
        );
        let uniform = uniforms.write(&uniform_bytes);
        self.encode_yuv420_pack(
            target,
            [y_texture.view(), u_texture.view(), v_texture.view()],
            encoder,
            &uniforms,
            uniform,
        );
        encode_plane_copy(
            encoder,
            y_texture.texture(),
            buffer.buffer(),
            0,
            layout.luma_pitch,
            target.size,
        );
        encode_plane_copy(
            encoder,
            u_texture.texture(),
            buffer.buffer(),
            layout.u_offset,
            layout.chroma_pitch,
            uv_size,
        );
        encode_plane_copy(
            encoder,
            v_texture.texture(),
            buffer.buffer(),
            layout.v_offset,
            layout.chroma_pitch,
            uv_size,
        );
        // After the plane copies, in the same encoder: it lands only if they ran. Not
        // counted as a copy operation in the diagnostics, which describe the frame's work.
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
            uniforms,
        })
    }

    pub(crate) fn complete_yuv420p_readback(
        &self,
        encoded: EncodedYuv420pReadback,
        #[cfg(not(target_arch = "wasm32"))] submission: wgpu::SubmissionIndex,
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
            uniforms,
        } = encoded;
        uniforms.recycle();
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
    /// This is the readback-free half of [`Self::begin_yuv420p_readback`]:
    /// the same conversion and the same chroma filter for `encoding.chroma`,
    /// written into a full-size `R8Unorm` luma target and a half-size
    /// `Rg8Unorm` chroma target instead of three planes and a staging buffer.
    /// wgpu 30 allows only `TEXTURE_BINDING` on `TextureFormat::NV12`, so the
    /// encoder's own image cannot be a render target and these two are copied
    /// into it afterwards by the native layer.
    ///
    /// # Errors
    /// Returns [`RenderError::TextureReadback`] when the target format or size
    /// is not one this pack accepts, [`RenderError::InvalidPlaneTarget`] when
    /// the plane textures do not match it (format, size, or a texture that
    /// cannot be rendered to), and [`RenderError::GpuWorkDropped`] once the
    /// renderer has found the device dropping work.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn encode_nv12_pack(
        &self,
        target: &RenderedTexture,
        planes: Nv12PlaneTargets<'_>,
        encoding: YuvEncoding,
        encoder: &mut wgpu::CommandEncoder,
    ) -> Result<EncodedNv12Pack, RenderError> {
        // No readback here carries a canary, but a readback that found the device dropping
        // work speaks for this pack too.
        self.ensure_gpu_runs_work()?;
        let encode = self.open_encode();
        self.validate_size(target.size)?;
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
        let chroma_size = RenderSize {
            width: target.size.width / 2,
            height: target.size.height / 2,
        };
        // The planes may be a hardware encoder's, so what the passes rely on is checked here rather
        // than left to an uncaptured validation error.
        let attachment = |texture: &wgpu::Texture| {
            texture
                .usage()
                .contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
                && texture.mip_level_count() == 1
                && texture.sample_count() == 1
                && texture.depth_or_array_layers() == 1
        };
        if planes.luma.format() != wgpu::TextureFormat::R8Unorm
            || planes.chroma.format() != wgpu::TextureFormat::Rg8Unorm
            || planes.luma.width() != target.size.width
            || planes.luma.height() != target.size.height
            || planes.chroma.width() != chroma_size.width
            || planes.chroma.height() != chroma_size.height
            || !attachment(planes.luma)
            || !attachment(planes.chroma)
        {
            return Err(RenderError::InvalidPlaneTarget);
        }
        let uniform_bytes = f32_bytes(yuv_uniform(encoding));
        let mut uniforms = self.uniform_buffers.acquire(
            self.gpu.device(),
            u64::try_from(uniform_bytes.len()).unwrap_or(u64::MAX),
        );
        let uniform = uniforms.write(&uniform_bytes);
        let cached = self.yuv_readback_pipeline();
        let bind_group = self.yuv_pack_bind_group(target, cached, &uniforms, uniform);
        let luma_view = crate::pool::default_view(planes.luma);
        encode_yuv_render_pass(
            encoder,
            "alpha2 GPU NV12 Y pack pass",
            &cached.y_pipeline,
            &bind_group,
            &luma_view,
        );
        let chroma_view = crate::pool::default_view(planes.chroma);
        encode_yuv_render_pass(
            encoder,
            "alpha2 GPU NV12 UV pack pass",
            &cached.uv_nv12_pipeline,
            &bind_group,
            &chroma_view,
        );
        // `write` only fills the host copy; without the flush the shader reads stale or zero data.
        uniforms.flush(self.gpu.queue());
        // The passes run when the caller submits `encoder`; released here, the next renderer call
        // could rewrite this buffer before that submission.
        Ok(EncodedNv12Pack {
            uniforms: Some(uniforms),
            _encode: encode,
        })
    }

    fn yuv_pack_bind_group(
        &self,
        target: &RenderedTexture,
        cached: &CachedYuvReadbackPipeline,
        uniforms: &crate::uniform::UniformArenaLease,
        uniform: crate::uniform::UniformSlice,
    ) -> wgpu::BindGroup {
        self.gpu
            .device()
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("alpha2 RGB to YUV bind group"),
                layout: &cached.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(target.view()),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: uniforms.binding(uniform),
                    },
                ],
            })
    }

    fn encode_yuv420_pack(
        &self,
        target: &RenderedTexture,
        plane_views: [&wgpu::TextureView; 3],
        encoder: &mut wgpu::CommandEncoder,
        uniforms: &crate::uniform::UniformArenaLease,
        uniform: crate::uniform::UniformSlice,
    ) {
        let cached = self.yuv_readback_pipeline();
        let bind_group = self.yuv_pack_bind_group(target, cached, uniforms, uniform);
        let [y_view, u_view, v_view] = plane_views;
        encode_yuv_render_pass(
            encoder,
            "alpha2 GPU Y pack pass",
            &cached.y_pipeline,
            &bind_group,
            y_view,
        );
        encode_yuv_render_pass_pair(
            encoder,
            "alpha2 GPU UV pack pass",
            &cached.uv_pipeline,
            &bind_group,
            [u_view, v_view],
        );
    }

    fn yuv_readback_pipeline(&self) -> &CachedYuvReadbackPipeline {
        self.pipelines.yuv_readback.get_or_init(|| {
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
                                    has_dynamic_offset: false,
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

    fn encode_readback_pack(
        &self,
        target: &RenderedTexture,
        encoder: &mut wgpu::CommandEncoder,
    ) -> Option<TextureLease> {
        if target.format != wgpu::TextureFormat::Rgba16Float {
            return None;
        }
        let texture = self.textures.acquire(
            self.gpu.device(),
            target.size,
            wgpu::TextureFormat::Rgba8Unorm,
            "alpha2 GPU-packed RGBA8 readback texture",
        );
        let cached = self.readback_pack_pipeline();
        let bind_group = self
            .gpu
            .device()
            .create_bind_group(&wgpu::BindGroupDescriptor {
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
        Some(texture)
    }

    fn readback_pack_pipeline(&self) -> &CachedPipeline {
        self.pipelines.readback_pack.get_or_init(|| {
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
/// The handle owns its staging buffer, so it outlives the renderer borrow and
/// can be moved into an asynchronous browser task. Any pack target returns to
/// the texture pool after its copy submission, before buffer mapping finishes.
#[derive(Debug)]
pub struct Rgba8Readback {
    buffer: ReadbackBufferLease,
    gpu: GpuContext,
    size: RenderSize,
    padded_bytes_per_row: u32,
    unpadded_bytes_per_row: u32,
    #[cfg(not(target_arch = "wasm32"))]
    submission: wgpu::SubmissionIndex,
    #[cfg(not(target_arch = "wasm32"))]
    timeout: std::time::Duration,
    staging_bytes: u64,
    /// Where the rows end and the canary begins.
    image_bytes: u64,
    canary: Canary,
    mapping_requested: AtomicBool,
}

impl Rgba8Readback {
    /// Expects `canary` in place of the one the GPU copied, as a staging
    /// buffer holding an earlier readback's bytes would look.
    #[cfg(test)]
    pub(crate) fn expecting(mut self, canary: Canary) -> Self {
        self.canary = canary;
        self
    }

    /// Returns the pixel dimensions the mapped result will contain.
    #[must_use]
    pub const fn size(&self) -> RenderSize {
        self.size
    }

    /// Returns bytes retained by the GPU-visible staging buffer.
    #[must_use]
    pub const fn staging_bytes(&self) -> u64 {
        self.staging_bytes
    }

    /// Blocks a native caller until this specific readback submission is mapped.
    ///
    /// Other work may be submitted between creating and waiting on the handle;
    /// the stored submission index prevents this wait from draining the entire
    /// device queue.
    ///
    /// # Errors
    /// Returns [`RenderError::TextureReadback`] when mapping or device polling
    /// fails.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn wait_ready(&self) -> Result<(), RenderError> {
        let (sender, receiver) = mpsc::channel();
        self.map_async(move |mapped| {
            let _ = sender.send(mapped);
        });
        self.gpu
            .device()
            .poll(wgpu::PollType::Wait {
                submission_index: Some(self.submission.clone()),
                timeout: Some(self.timeout),
            })
            .map_err(crate::error::poll_error)?;
        receiver
            .recv()
            .map_err(|_| RenderError::TextureReadback(ReadbackFailure::CallbackDropped))?
    }

    /// Waits for this readback and packs its mapped rows into RGBA8 pixels.
    ///
    /// # Errors
    /// Returns [`RenderError::TextureReadback`] when mapping, polling, or row
    /// packing fails.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn wait(self) -> Result<Vec<u8>, RenderError> {
        self.wait_ready()?;
        self.finish()
    }

    /// Requests buffer mapping and reports readiness exactly once.
    ///
    /// The callback receives the failure when mapping was refused or was
    /// already requested, which a caller must treat as a failed frame rather
    /// than as empty pixels.
    pub fn map_async(&self, on_ready: impl FnOnce(Result<(), RenderError>) + Send + 'static) {
        map_staging(&self.gpu, &self.buffer, &self.mapping_requested, on_ready);
    }

    /// Advances backends that need explicit polling to resolve a mapping.
    ///
    /// Browser WebGPU resolves mapping on its own event loop and ignores this;
    /// wgpu-core backends need it to make progress without a blocking wait.
    pub fn poll(&self) {
        let _ = self.gpu.device().poll(wgpu::PollType::Poll);
    }

    /// Packs the mapped rows into tightly packed premultiplied RGBA8.
    ///
    /// # Errors
    /// Returns [`RenderError::TextureReadback`] when the buffer is not mapped,
    /// and [`RenderError::GpuWorkDropped`] when the GPU did not run the copy:
    /// the buffer holds no pixels of this readback, so it is not recycled,
    /// and the renderer refuses further work.
    pub fn finish(self) -> Result<Vec<u8>, RenderError> {
        let Ok(view) = self.buffer.buffer().slice(..).get_mapped_range() else {
            // Unmapping an unmapped buffer is itself a validation error. Not recycled, the lease
            // destroys the buffer, which also ends a pending mapping.
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

/// The GPU state an NV12 pack recorded into a caller's encoder still needs.
///
/// Keep it until that encoder has been submitted, then drop it. Dropping it
/// earlier returns the pack's uniforms to the renderer's pool while the
/// passes that read them are unsubmitted, so a later renderer call can
/// overwrite them first.
#[must_use = "hold the pack until its encoder is submitted"]
#[derive(Debug)]
#[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
pub struct EncodedNv12Pack {
    uniforms: Option<crate::uniform::UniformArenaLease>,
    /// The caller's encoder is the renderer's encode too: nothing the pools
    /// let go of is destroyed until the pack is dropped, after submission.
    _encode: crate::retire::EncodeScope,
}

impl Drop for EncodedNv12Pack {
    fn drop(&mut self) {
        if let Some(uniforms) = self.uniforms.take() {
            uniforms.recycle();
        }
    }
}

/// Caller-owned NV12 plane textures for a readback-free pack.
///
/// `luma` is `R8Unorm` at the output size; `chroma` is `Rg8Unorm` at half
/// that, carrying U in red and V in green. Both belong to the caller because
/// on the encode path they are copied into a hardware encoder's image, which
/// wgpu cannot render into directly.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
pub struct Nv12PlaneTargets<'a> {
    /// Full-size single-channel luma target.
    pub luma: &'a wgpu::Texture,
    /// Half-size two-channel interleaved chroma target.
    pub chroma: &'a wgpu::Texture,
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

    /// The plane memory, as written.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.bytes
    }

    pub(crate) fn fits(&self, width: usize, height: usize) -> Result<(), RenderError> {
        if self.stride < width {
            return Err(RenderError::TextureReadback(
                ReadbackFailure::DestinationTooSmall,
            ));
        }
        // The last row only needs its visible width, not a whole stride, which is how
        // `FFmpeg` sizes the final plane of a frame.
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

/// A submitted GPU RGB-to-YUV420 conversion whose buffer is not mapped yet.
#[derive(Debug)]
pub struct Yuv420pReadback {
    buffer: ReadbackBufferLease,
    gpu: GpuContext,
    size: RenderSize,
    layout: Yuv420StagingLayout,
    #[cfg(not(target_arch = "wasm32"))]
    submission: wgpu::SubmissionIndex,
    #[cfg(not(target_arch = "wasm32"))]
    timeout: std::time::Duration,
    staging_bytes: u64,
    canary: Canary,
    mapping_requested: AtomicBool,
}

impl Yuv420pReadback {
    /// Returns the luma-plane dimensions.
    #[must_use]
    pub const fn size(&self) -> RenderSize {
        self.size
    }

    /// Returns bytes retained by the GPU-visible staging buffer.
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
        receiver
            .recv()
            .map_err(|_| RenderError::TextureReadback(ReadbackFailure::CallbackDropped))?
    }

    /// Blocks native GPU progress through this readback's submission.
    ///
    /// This can be paired with an earlier [`Self::map_async`] call when a
    /// scheduler wants mapping to begin before it eventually has to wait.
    ///
    /// # Errors
    /// Returns [`RenderError::GpuTimeout`] when the submission does not
    /// complete within the renderer's wait timeout, or
    /// [`RenderError::TextureReadback`] when device polling fails.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn wait_for_submission(&self) -> Result<(), RenderError> {
        self.gpu
            .device()
            .poll(wgpu::PollType::Wait {
                submission_index: Some(self.submission.clone()),
                timeout: Some(self.timeout),
            })
            .map_err(crate::error::poll_error)?;
        Ok(())
    }

    /// Waits for this readback and returns tightly packed planes.
    ///
    /// # Errors
    /// Returns [`RenderError::TextureReadback`] when mapping, polling, or row
    /// packing fails.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn wait(self) -> Result<Yuv420pPlanes, RenderError> {
        self.wait_ready()?;
        self.finish()
    }

    /// Requests buffer mapping and reports readiness exactly once.
    pub fn map_async(&self, on_ready: impl FnOnce(Result<(), RenderError>) + Send + 'static) {
        map_staging(&self.gpu, &self.buffer, &self.mapping_requested, on_ready);
    }

    /// Advances backends that require explicit polling.
    pub fn poll(&self) {
        let _ = self.gpu.device().poll(wgpu::PollType::Poll);
    }

    /// Packs mapped Y and UV rows into plane buffers the caller already owns.
    ///
    /// [`Yuv420pReadback::finish`] allocates three vectors per frame, which is
    /// what the browser path wants. A native caller that already owns the
    /// memory the encoder will read from can pack straight into it, which
    /// removes both that allocation and the copy that would follow it. The
    /// pixels written are the same either way.
    ///
    /// Each target may be strided, because encoder plane memory usually is.
    ///
    /// # Errors
    /// Returns [`RenderError::TextureReadback`] when the buffer is not mapped,
    /// its layout cannot be represented, or a target is too small, and
    /// [`RenderError::GpuWorkDropped`] when the GPU did not run the copy: the
    /// buffer holds no frame of this readback, so it is not recycled, and the
    /// renderer refuses further work.
    pub fn finish_into(self, targets: &mut Yuv420pTargets<'_>) -> Result<(), RenderError> {
        let Ok(mapped) = self.buffer.buffer().slice(..).get_mapped_range() else {
            // Not mapped: nothing to unmap (see `Rgba8Readback::finish`).
            return Err(RenderError::TextureReadback(ReadbackFailure::NotMapped));
        };
        let result = (|| {
            self.check_canary(&mapped)?;
            let planes = self.layout.planes(self.size);
            let destinations = [&mut targets.y, &mut targets.u, &mut targets.v];
            for (plane, target) in planes.into_iter().zip(destinations) {
                let (_, _, width, rows) = plane;
                let width = usize::try_from(width)
                    .map_err(|_| RenderError::TextureReadback(ReadbackFailure::Layout))?;
                let rows = usize::try_from(rows)
                    .map_err(|_| RenderError::TextureReadback(ReadbackFailure::Layout))?;
                target.fits(width, rows)?;
                for (index, row) in plane_rows(&mapped, plane)?.enumerate() {
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

    /// Packs mapped Y and UV rows into three tightly packed planes.
    ///
    /// # Errors
    /// Returns [`RenderError::TextureReadback`] when the buffer is not mapped
    /// or its layout cannot be represented, and [`RenderError::GpuWorkDropped`]
    /// when the GPU did not run the copy, as [`Self::finish_into`] does.
    pub fn finish(self) -> Result<Yuv420pPlanes, RenderError> {
        let planes = self.unpack(|[y, u, v]| Yuv420pPlanes { y, u, v })?;
        Ok(planes)
    }

    /// Where the Y, U, and V planes sit in the bytes [`Self::finish_mapped`]
    /// hands over.
    #[must_use]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn plane_layout(&self) -> [StagingPlane; 3] {
        self.layout
            .planes(self.size)
            .map(|(offset, stride, _, _)| StagingPlane { offset, stride })
    }

    /// Hands the mapped staging buffer to `read`, then unmaps it.
    ///
    /// The bytes are the frame as the GPU copied it out: the Y, U, and V
    /// planes at the offsets and strides [`Self::plane_layout`] reports, each
    /// row padded to the copy alignment. A consumer that takes strided planes,
    /// such as a `WebCodecs` `VideoFrame` given a `layout`, copies the frame
    /// once, straight from the mapping. On browser WebGPU, where the mapping
    /// is a JavaScript `ArrayBuffer`, that copy can stay in JavaScript
    /// (`BufferView::as_uint8array`), without passing through wasm memory.
    ///
    /// # Errors
    /// Returns [`RenderError::TextureReadback`] when the buffer is not mapped,
    /// and [`RenderError::GpuWorkDropped`] when the GPU did not run the copy,
    /// as [`Self::finish_into`] does; `read` is not called then.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn finish_mapped<T>(
        self,
        read: impl FnOnce(&wgpu::BufferView) -> T,
    ) -> Result<T, RenderError> {
        let Ok(canary) = self
            .buffer
            .buffer()
            .slice(self.layout.bytes..)
            .get_mapped_range()
        else {
            // Not mapped: nothing to unmap (see `Rgba8Readback::finish`).
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
            .slice(..self.layout.bytes)
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

    /// Copies the three planes out of the mapped staging buffer, tightly
    /// packed, and hands them to `pack`.
    fn unpack<T>(self, pack: impl FnOnce([Vec<u8>; 3]) -> T) -> Result<T, RenderError> {
        let Ok(mapped) = self.buffer.buffer().slice(..).get_mapped_range() else {
            // Not mapped: nothing to unmap (see `Rgba8Readback::finish`).
            return Err(RenderError::TextureReadback(ReadbackFailure::NotMapped));
        };
        if let Err(error) = self.check_canary(&mapped) {
            drop(mapped);
            self.buffer.buffer().unmap();
            return Err(error);
        }
        let planes = self.layout.planes(self.size).map(|plane| {
            let (_, _, width, rows) = plane;
            let width = usize::try_from(width)
                .map_err(|_| RenderError::TextureReadback(ReadbackFailure::Layout))?;
            let rows = usize::try_from(rows)
                .map_err(|_| RenderError::TextureReadback(ReadbackFailure::Layout))?;
            plane_rows(&mapped, plane).map(|source| {
                // Row by row, so the vector is sized and each row copied at once.
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

    /// Checks that the mapped staging buffer ends in this readback's canary.
    /// A buffer that fails is not recycled: it holds no frame of this
    /// readback, and the device that should have written it is done.
    fn check_canary(&self, mapped: &[u8]) -> Result<(), RenderError> {
        let start = usize::try_from(self.layout.bytes)
            .map_err(|_| RenderError::TextureReadback(ReadbackFailure::Layout))?;
        let found = mapped
            .get(start..)
            .ok_or(RenderError::TextureReadback(ReadbackFailure::Layout))?;
        self.canary.check(found)
    }
}

/// Maps a readback's staging buffer once, reporting where it failed.
fn map_staging(
    gpu: &GpuContext,
    buffer: &ReadbackBufferLease,
    requested: &AtomicBool,
    on_ready: impl FnOnce(Result<(), RenderError>) + Send + 'static,
) {
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
            // `BufferAsyncError` names no cause; the device's diagnostics hold whatever
            // explains it.
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

/// The pack shader's coding of `encoding` into 8-bit YCbCr: the matrix's
/// luma weights and the range's levels, from the same definitions import
/// decodes with, so a round trip through both is the identity; and the axes
/// on which chroma is co-sited with luma, as import reads them back.
pub(crate) fn yuv_uniform(encoding: YuvEncoding) -> [f32; 12] {
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

fn encode_yuv_render_pass(
    encoder: &mut wgpu::CommandEncoder,
    label: &'static str,
    pipeline: &wgpu::RenderPipeline,
    bind_group: &wgpu::BindGroup,
    target: &wgpu::TextureView,
) {
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: target,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, bind_group, &[]);
    pass.draw(0..3, 0..1);
}

fn encode_yuv_render_pass_pair(
    encoder: &mut wgpu::CommandEncoder,
    label: &'static str,
    pipeline: &wgpu::RenderPipeline,
    bind_group: &wgpu::BindGroup,
    targets: [&wgpu::TextureView; 2],
) {
    let attachments = targets.map(|target| {
        Some(wgpu::RenderPassColorAttachment {
            view: target,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                store: wgpu::StoreOp::Store,
            },
        })
    });
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &attachments,
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, bind_group, &[]);
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

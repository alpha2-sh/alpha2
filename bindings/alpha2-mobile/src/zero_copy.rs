//! Decoded frames that stay on the GPU. On Android, hardware decoders render into buffers the
//! renderer's Vulkan device imports; on Apple platforms, `VideoToolbox` pixel buffers are sampled
//! as `Metal` textures. Either replaces planes copied through memory.
//!
//! The GPU renderer takes this path when the device imports decoder buffers and the session's
//! [`crate::VideoFrameMode`] allows it. Everything else keeps the CPU path: the CPU renderer, GL
//! devices, devices without the import, Android's software decoders (whose pictures are made in
//! memory), pictures a `VideoToolbox` session copies, and [`crate::VideoFrameMode::Cpu`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use alpha2_gpu::GpuContext;
use alpha2_host_core::export::{EncoderTargets, SurfaceImport};
use alpha2_host_core::video_stream::VideoBackends;

/// What a renderer whose device imports decoder surfaces decodes and imports with. Made with the
/// renderer's own device, which the import requires.
pub(crate) struct GpuFrames {
    /// Decoders whose hardware sessions render into importable surfaces.
    pub decoders: Arc<dyn VideoBackends>,
    /// Prepares those surfaces for the renderer.
    pub import: Arc<dyn SurfaceImport>,
    imported: Arc<AtomicU64>,
    fallbacks: Arc<AtomicU64>,
    targets: Option<TargetsFactory>,
    in_place: Arc<AtomicU64>,
}

type TargetsFactory = Box<dyn Fn(Arc<AtomicU64>) -> Box<dyn EncoderTargets> + Send + Sync>;

impl GpuFrames {
    pub(crate) fn encoder_targets(&self) -> Option<Box<dyn EncoderTargets>> {
        self.targets
            .as_ref()
            .map(|targets| targets(Arc::clone(&self.in_place)))
    }

    pub(crate) fn rendered_in_place(&self) -> u64 {
        self.in_place.load(Ordering::Relaxed)
    }

    /// How many decoded frames were imported from surfaces so far.
    pub(crate) fn imported(&self) -> u64 {
        self.imported.load(Ordering::Relaxed)
    }

    pub(crate) fn fallbacks(&self) -> u64 {
        self.fallbacks.load(Ordering::Relaxed)
    }
}

/// `gpu`, reopened to import decoder surfaces when `allowed` and the platform and device can,
/// with what decodes into them and imports them; otherwise `gpu` unchanged and `None`.
pub(crate) fn open(gpu: GpuContext, allowed: bool) -> (GpuContext, Option<GpuFrames>) {
    if !allowed {
        return (gpu, None);
    }
    #[cfg(target_os = "android")]
    {
        android::open(gpu)
    }
    #[cfg(target_vendor = "apple")]
    {
        apple::open(gpu)
    }
    #[cfg(not(any(target_os = "android", target_vendor = "apple")))]
    {
        (gpu, None)
    }
}

#[cfg(target_os = "android")]
mod android {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use alpha2_android_media::{AndroidSurface, AndroidVideo, VideoOutput};
    use alpha2_gpu::GpuContext;
    use alpha2_host_core::export::{GpuSource, SurfaceImport, prepare_gpu_planes};
    use alpha2_host_core::video_stream::{FrameFacts, SurfaceLease};
    use alpha2_media::YuvFormat;
    use alpha2_native_gpu::{AhbImporter, AhbPlanes, NativePlaneLayout};
    use alpha2_render::{ColorPolicy, ExternalTextureLease, GpuVideoPlanes, RenderSize, Renderer};

    use super::GpuFrames;

    pub(super) fn open(gpu: GpuContext) -> (GpuContext, Option<GpuFrames>) {
        match alpha2_native_gpu::reopen_for_ahb_import(gpu) {
            Ok((gpu, importer)) => {
                let output = VideoOutput::Surfaces {
                    ten_bit: importer.supports_ten_bit(),
                };
                let count = Arc::new(AtomicU64::new(0));
                let frames = GpuFrames {
                    decoders: Arc::new(AndroidVideo { output }),
                    import: Arc::new(AhbImport {
                        importer: Arc::new(importer),
                        imported: Arc::clone(&count),
                    }),
                    imported: count,
                    fallbacks: Arc::default(),
                    targets: None,
                    in_place: Arc::default(),
                };
                (gpu, Some(frames))
            }
            Err((gpu, reason)) => {
                alpha2_os::diagnostic!(
                    "alpha2: decoded frames are copied through memory: {reason}"
                );
                (gpu, None)
            }
        }
    }

    /// Imports Android decoder buffers on the renderer's device, counting them.
    struct AhbImport {
        importer: Arc<AhbImporter>,
        imported: Arc<AtomicU64>,
    }

    /// Keeps a frame's planes and decoder buffer until the renderer drops it after sampling.
    struct PlaneLease(Arc<AhbPlanes>, Arc<AhbImporter>);

    impl std::fmt::Debug for PlaneLease {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.debug_tuple("PlaneLease").field(&self.0).finish()
        }
    }

    impl Drop for PlaneLease {
        fn drop(&mut self) {
            let _ = self.1.collect();
        }
    }

    impl ExternalTextureLease for PlaneLease {
        fn retained_bytes(&self) -> u64 {
            self.0.retained_bytes()
        }
    }

    impl SurfaceImport for AhbImport {
        fn prepare(
            &self,
            renderer: &Renderer,
            size: RenderSize,
            color: ColorPolicy,
            facts: &FrameFacts,
            lease: &Arc<dyn SurfaceLease>,
        ) -> Result<GpuSource, String> {
            let surface = lease
                .as_any()
                .downcast_ref::<AndroidSurface>()
                .ok_or_else(|| "the surface is not an Android decoder buffer".to_owned())?;
            let owner: Box<dyn Send> = Box::new(Arc::clone(lease));
            // SAFETY: `owner` holds the decoder's image, so its buffer lives and the decoder does
            // not write it until the importer drops `owner` after the conversion.
            let imported = unsafe {
                self.importer
                    .import(surface.hardware_buffer(), surface.ten_bit(), owner)
            }
            .map_err(|error| error.to_string())?;
            self.imported.fetch_add(1, Ordering::Relaxed);
            let coded = RenderSize::new(imported.coded_size[0], imported.coded_size[1])
                .map_err(|error| error.to_string())?;
            let format = match imported.layout {
                NativePlaneLayout::P010 => YuvFormat::P010,
                NativePlaneLayout::Nv12 => YuvFormat::NV12,
                NativePlaneLayout::I420 => {
                    return Err(
                        "the import made three planes, which decoder buffers never have".into(),
                    );
                }
            };
            prepare_gpu_planes(
                renderer,
                size,
                color,
                facts,
                GpuVideoPlanes {
                    y: &imported.y,
                    chroma: &imported.uv,
                    v: None,
                    format,
                    size: coded,
                    alpha: 1.0,
                    color: facts.color,
                },
                Arc::new(PlaneLease(imported.lease, Arc::clone(&self.importer))),
            )
        }

        fn collect(&self) {
            let _ = self.importer.collect();
        }
    }
}

#[cfg(target_vendor = "apple")]
mod apple {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use alpha2_apple_media::{AppleSurface, AppleVideo, VideoOutput};
    use alpha2_gpu::GpuContext;
    use alpha2_host_core::encode::LentBuffer;
    use alpha2_host_core::export::{
        EncoderTargets, GpuSource, LentPlanes, SurfaceImport, prepare_gpu, prepare_gpu_planes,
    };
    use alpha2_host_core::video_stream::{BackendImage, FrameFacts, StreamFrame, SurfaceLease};
    use alpha2_media::YuvFormat;
    use alpha2_native_gpu::{
        MetalEncoderPlanes, MetalImportLease, NativePlaneLayout, SharedMetalContext,
        pixel_buffer_surface_id,
    };
    use alpha2_render::{ColorPolicy, ExternalTextureLease, GpuVideoPlanes, RenderSize, Renderer};

    use super::GpuFrames;

    pub(super) fn open(gpu: GpuContext) -> (GpuContext, Option<GpuFrames>) {
        match SharedMetalContext::from_gpu(gpu.clone()) {
            Ok(context) => {
                let output = VideoOutput::Surfaces {
                    ten_bit: context.supports_ten_bit(),
                };
                let imported = Arc::new(AtomicU64::new(0));
                let fallbacks = Arc::new(AtomicU64::new(0));
                let targets = context.clone();
                let frames = GpuFrames {
                    decoders: Arc::new(AppleVideo { output }),
                    import: Arc::new(MetalImport {
                        context,
                        imported: Arc::clone(&imported),
                        fallbacks: Arc::clone(&fallbacks),
                        reported: AtomicBool::new(false),
                    }),
                    imported,
                    fallbacks,
                    targets: Some(Box::new(move |in_place| {
                        Box::new(MetalTargets {
                            context: targets.clone(),
                            generation: None,
                            wraps: Vec::new(),
                            clock: 0,
                            in_place,
                            reported: false,
                        })
                    })),
                    in_place: Arc::default(),
                };
                (gpu, Some(frames))
            }
            Err(error) => {
                alpha2_os::diagnostic!("alpha2: decoded frames are copied through memory: {error}");
                (gpu, None)
            }
        }
    }

    struct MetalImport {
        context: SharedMetalContext,
        imported: Arc<AtomicU64>,
        fallbacks: Arc<AtomicU64>,
        reported: AtomicBool,
    }

    #[derive(Debug)]
    struct PlaneLease(Arc<MetalImportLease>);

    impl ExternalTextureLease for PlaneLease {
        fn retained_bytes(&self) -> u64 {
            self.0.retained_bytes()
        }
    }

    impl MetalImport {
        fn copy(
            &self,
            renderer: &Renderer,
            size: RenderSize,
            color: ColorPolicy,
            facts: &FrameFacts,
            surface: &AppleSurface,
            error: &str,
        ) -> Result<GpuSource, String> {
            if !self.reported.swap(true, Ordering::Relaxed) {
                alpha2_os::diagnostic!(
                    "alpha2: a decoded frame is copied through memory: the Metal import refused \
                     it: {error}"
                );
            }
            let (format, planes) = surface.copy()?;
            let frame = StreamFrame {
                facts: *facts,
                image: BackendImage::Cpu { format, planes },
            }
            .to_cpu()
            .ok_or_else(|| "the copy is not a frame in memory".to_owned())?;
            let prepared = prepare_gpu(renderer, size, color, &frame)?;
            self.fallbacks.fetch_add(1, Ordering::Relaxed);
            Ok(prepared)
        }
    }

    impl SurfaceImport for MetalImport {
        fn prepare(
            &self,
            renderer: &Renderer,
            size: RenderSize,
            color: ColorPolicy,
            facts: &FrameFacts,
            lease: &Arc<dyn SurfaceLease>,
        ) -> Result<GpuSource, String> {
            let surface = lease
                .as_any()
                .downcast_ref::<AppleSurface>()
                .ok_or_else(|| "the surface is not a VideoToolbox pixel buffer".to_owned())?;
            let owner: Box<dyn Send + Sync> = Box::new(Arc::clone(lease));
            #[cfg(test)]
            let refused = hooks::refuse_import();
            #[cfg(not(test))]
            let refused = false;
            let imported = if refused {
                Err(alpha2_native_gpu::MetalImportError::Missing(
                    "refused by a test",
                ))
            } else {
                // SAFETY: `owner` holds the surface, so its pixel buffer lives and the decoder's
                // pool does not reuse it until the lease drops after the renderer's work.
                unsafe {
                    self.context
                        .import_pixel_buffer(surface.pixel_buffer(), owner)
                }
            };
            let imported = match imported {
                Ok(imported) => imported,
                Err(error) => {
                    return self.copy(renderer, size, color, facts, surface, &error.to_string());
                }
            };
            let coded = RenderSize::new(imported.coded_size[0], imported.coded_size[1])
                .map_err(|error| error.to_string())?;
            let format = match imported.layout {
                NativePlaneLayout::P010 => YuvFormat::P010,
                NativePlaneLayout::Nv12 => YuvFormat::NV12,
                NativePlaneLayout::I420 => {
                    return Err(
                        "the import made three planes, which pixel buffers never have".into(),
                    );
                }
            };
            let prepared = prepare_gpu_planes(
                renderer,
                size,
                color,
                facts,
                GpuVideoPlanes {
                    y: &imported.y,
                    chroma: &imported.uv,
                    v: None,
                    format,
                    size: coded,
                    alpha: 1.0,
                    color: facts.color,
                },
                Arc::new(PlaneLease(imported.lease)),
            )?;
            self.imported.fetch_add(1, Ordering::Relaxed);
            Ok(prepared)
        }
    }

    const SPARE_WRAPS: usize = 2;

    struct Wrap {
        surface: u32,
        _planes: MetalEncoderPlanes,
        views: LentPlanes,
        used: u64,
    }

    struct MetalTargets {
        context: SharedMetalContext,
        generation: Option<u64>,
        wraps: Vec<Wrap>,
        clock: u64,
        in_place: Arc<AtomicU64>,
        reported: bool,
    }

    impl MetalTargets {
        fn wrap(&mut self, buffer: &LentBuffer, size: RenderSize) -> Result<LentPlanes, String> {
            if self.generation != Some(buffer.generation()) {
                self.wraps.clear();
                self.generation = Some(buffer.generation());
            }
            // SAFETY: the lent buffer holds its pixel buffer for the call.
            let surface = unsafe { pixel_buffer_surface_id(buffer.surface()) }
                .ok_or_else(|| "the encoder's buffer has no IOSurface".to_owned())?;
            self.clock += 1;
            if let Some(wrap) = self.wraps.iter_mut().find(|wrap| wrap.surface == surface) {
                wrap.used = self.clock;
                return Ok(wrap.views.clone());
            }
            // SAFETY: as above; the planes keep the IOSurface, not the buffer, alive.
            let planes = unsafe { self.context.pixel_buffer_planes(buffer.surface()) }
                .map_err(|error| error.to_string())?;
            let (luma, chroma) = (planes.luma.size(), planes.chroma.size());
            if (luma.width, luma.height) != (size.width, size.height)
                || (chroma.width, chroma.height) != (size.width / 2, size.height / 2)
            {
                return Err(format!(
                    "the encoder's {}x{} buffer with {}x{} chroma does not take a {}x{} frame",
                    luma.width, luma.height, chroma.width, chroma.height, size.width, size.height
                ));
            }
            let views = LentPlanes {
                luma: planes
                    .luma
                    .create_view(&wgpu::TextureViewDescriptor::default()),
                chroma: planes
                    .chroma
                    .create_view(&wgpu::TextureViewDescriptor::default()),
            };
            if self.wraps.len() >= buffer.most_out() + SPARE_WRAPS
                && let Some(oldest) = (0..self.wraps.len()).min_by_key(|at| self.wraps[*at].used)
            {
                self.wraps.swap_remove(oldest);
            }
            self.wraps.push(Wrap {
                surface,
                _planes: planes,
                views: views.clone(),
                used: self.clock,
            });
            Ok(views)
        }
    }

    impl EncoderTargets for MetalTargets {
        fn planes(&mut self, buffer: &LentBuffer, size: RenderSize) -> Result<LentPlanes, String> {
            let wrapped = self.wrap(buffer, size);
            if let Err(reason) = &wrapped
                && !std::mem::replace(&mut self.reported, true)
            {
                alpha2_os::diagnostic!(
                    "alpha2 export: frames are read back for the encoder: {reason}"
                );
            }
            wrapped
        }

        fn finished(&mut self, _buffer: &LentBuffer) -> Result<(), String> {
            let gpu = self.context.gpu();
            let submitted = gpu.submit(std::iter::empty());
            let timeout = alpha2_native_gpu::metal_wait_timeout();
            match gpu.device().poll(wgpu::PollType::Wait {
                submission_index: Some(submitted),
                timeout: Some(timeout),
            }) {
                Ok(wgpu::PollStatus::WaitSucceeded | wgpu::PollStatus::QueueEmpty) => {
                    self.in_place.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
                Ok(wgpu::PollStatus::Poll) | Err(wgpu::PollError::Timeout) => Err(format!(
                    "the render into an encoder buffer: {} (after {timeout:?})",
                    alpha2_media::GPU_TIMEOUT_MESSAGE
                )),
                Err(error) => Err(format!(
                    "waiting for the render into an encoder buffer failed: {error}"
                )),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::sync::Arc;

        use alpha2_apple_media::AppleVideoEncoders;
        use alpha2_gpu::GpuContext;
        use alpha2_host_core::encode::{
            Lend, LentBuffer, VideoEncodeBackend, VideoEncodeConfig, VideoEncoders,
        };
        use alpha2_host_core::export::EncoderTargets;
        use alpha2_media::{BitrateMode, ColorMetadata, PixelLayout, VideoRateControl};
        use alpha2_native_gpu::SharedMetalContext;
        use alpha2_render::RenderSize;
        use alpha2_time::FrameRate;

        use super::MetalTargets;

        fn encoder() -> Box<dyn VideoEncodeBackend> {
            AppleVideoEncoders
                .open(&VideoEncodeConfig {
                    codec: alpha2_media::VideoCodec::H264,
                    width: 320,
                    height: 180,
                    frame_rate: FrameRate::new(30, 1).expect("rate"),
                    layout: PixelLayout::YUV420P,
                    rate: VideoRateControl {
                        bitrate: 1_000_000,
                        mode: BitrateMode::Variable,
                        quantizer: None,
                    },
                    color: ColorMetadata::EXPORT,
                    keyframe_interval: 30,
                    software: false,
                })
                .expect("an encoder opens")
        }

        fn lend(encoder: &mut Box<dyn VideoEncodeBackend>) -> LentBuffer {
            match encoder.lender().expect("lends").lend().expect("a lend") {
                Lend::Buffer(buffer) => buffer,
                Lend::Busy => panic!("busy"),
            }
        }

        #[test]
        fn a_pool_buffer_is_wrapped_once_per_generation_and_the_least_recent_goes_first() {
            let Ok(gpu) = pollster::block_on(GpuContext::request_product()) else {
                alpha2_test_support::skip!(alpha2_test_support::Capability::Metal, "no GPU");
                return;
            };
            let context = SharedMetalContext::from_gpu(gpu).expect("a Metal device");
            let mut targets = MetalTargets {
                context,
                generation: None,
                wraps: Vec::new(),
                clock: 0,
                in_place: Arc::default(),
                reported: false,
            };
            let size = RenderSize::new(320, 180).expect("size");
            let mut encoder = encoder();
            let held = (0..3).map(|_| lend(&mut encoder)).collect::<Vec<_>>();
            for buffer in held.iter().chain(&held) {
                targets.planes(buffer, size).expect("render targets");
            }
            assert_eq!(
                targets.wraps.len(),
                3,
                "a buffer lent again is wrapped again"
            );
            let bounded = |buffer: &LentBuffer, generation, most_out| {
                LentBuffer::new(buffer.surface(), generation, most_out, Box::new(()))
            };
            let first = targets.wraps[0].surface;
            targets
                .planes(&bounded(&held[0], 0, 1), size)
                .expect("targets");
            targets
                .planes(&bounded(&held[1], 0, 1), size)
                .expect("targets");
            let fourth = lend(&mut encoder);
            targets
                .planes(&bounded(&fourth, 0, 1), size)
                .expect("targets");
            assert_eq!(targets.wraps.len(), 3, "most out plus the spare wraps");
            assert!(
                targets
                    .wraps
                    .iter()
                    .all(|wrap| wrap.surface != held_surface(&held[2])),
                "the least recently lent buffer goes first"
            );
            assert!(targets.wraps.iter().any(|wrap| wrap.surface == first));
            targets
                .planes(&bounded(&held[0], 1, 16), size)
                .expect("targets");
            assert_eq!(
                targets.wraps.len(),
                1,
                "a new pool generation drops the old wraps"
            );
            let odd = RenderSize::new(322, 180).expect("size");
            assert!(targets.planes(&held[1], odd).is_err());
        }

        fn held_surface(buffer: &LentBuffer) -> u32 {
            // SAFETY: a live lent buffer.
            unsafe { alpha2_native_gpu::pixel_buffer_surface_id(buffer.surface()) }
                .expect("an IOSurface")
        }
    }

    #[cfg(test)]
    pub(crate) mod hooks {
        use std::sync::atomic::{AtomicU64, Ordering};

        static REFUSALS: AtomicU64 = AtomicU64::new(0);

        pub(crate) fn refuse_imports(count: u64) {
            REFUSALS.store(count, Ordering::SeqCst);
        }

        pub(super) fn refuse_import() -> bool {
            REFUSALS
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
        }
    }
}

#[cfg(all(test, target_vendor = "apple"))]
pub(crate) use apple::hooks;

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
use alpha2_host_core::export::SurfaceImport;
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
}

impl GpuFrames {
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
    use alpha2_host_core::export::{GpuSource, SurfaceImport, prepare_gpu, prepare_gpu_planes};
    use alpha2_host_core::video_stream::{BackendImage, FrameFacts, StreamFrame, SurfaceLease};
    use alpha2_media::YuvFormat;
    use alpha2_native_gpu::{MetalImportLease, NativePlaneLayout, SharedMetalContext};
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

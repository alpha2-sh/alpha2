//! Decoded frames that stay on the GPU. On Android, hardware decoders render into buffers the
//! renderer's Vulkan device imports, instead of byte buffers copied through memory.
//!
//! The GPU renderer takes this path when the device imports decoder buffers and the session's
//! [`crate::VideoFrameMode`] allows it. Everything else keeps the CPU path: the CPU renderer, GL
//! devices, devices without the import, software decoders (whose pictures are made in memory),
//! and [`crate::VideoFrameMode::Cpu`].

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
    /// How many surfaces `import` prepared.
    imported: Arc<AtomicU64>,
}

impl GpuFrames {
    /// How many decoded frames were imported from surfaces so far.
    pub(crate) fn imported(&self) -> u64 {
        self.imported.load(Ordering::Relaxed)
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
    #[cfg(not(target_os = "android"))]
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
                        importer,
                        imported: Arc::clone(&count),
                    }),
                    imported: count,
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
        importer: AhbImporter,
        imported: Arc<AtomicU64>,
    }

    /// Keeps a frame's plane textures until the renderer's work on them completes.
    #[derive(Debug)]
    struct PlaneLease(Arc<AhbPlanes>);

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
                _ => YuvFormat::NV12,
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
                Arc::new(PlaneLease(imported.lease)),
            )
        }
    }
}

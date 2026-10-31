//! GPU texture upload and the export frame providers.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::decode::{DecodeCursorKey, DecodedFrameSet, DecodedSourceFrame, decode_cursor_key};
use crate::error_message;
use alpha2_engine::FrameProvider;
#[cfg(all(target_os = "windows", feature = "hwdecode"))]
use alpha2_ffmpeg_bridge::D3D12FrameLease;
#[cfg(all(target_os = "macos", feature = "hwdecode"))]
use alpha2_ffmpeg_bridge::VideoToolboxFrameLease;
#[cfg(all(target_os = "linux", feature = "hwdecode"))]
use alpha2_ffmpeg_bridge::VulkanFrameLease;
use alpha2_media::{
    CancelToken, DecodeEvent, DecodeRequest, DecodeRequestKind, DecodedVideoFrame, Decoder,
    PixelFormat, SourceId,
};
use alpha2_model::{MediaKind, Project};
use alpha2_native::{NativeDecoder, NativeVideoFrame};
#[cfg(all(target_os = "windows", feature = "hwdecode"))]
use alpha2_native_gpu::{D3D12CopyLease, SharedD3D12Context, SurfaceBudget, SurfaceReservation};
#[cfg(all(target_os = "macos", feature = "hwdecode"))]
use alpha2_native_gpu::{MetalImportLease, SharedMetalContext, SurfaceBudget, SurfaceReservation};
#[cfg(all(target_os = "linux", feature = "hwdecode"))]
use alpha2_native_gpu::{
    SharedVulkanContext, SurfaceBudget, SurfaceReservation, VulkanImportLease,
};
use alpha2_render::{
    ColorPolicy, PendingSourceWork, RenderCacheKey, RenderSize, RenderedTexture, Renderer,
    SourcePlane, SourcePresentation, SourceView, YuvFrame,
};
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
use alpha2_render::{ExternalTextureLease, GpuVideoPlanes};
use alpha2_time::MediaTime;

/// One prepared source texture and what preparing it cost.
pub struct NativeUpload {
    pub texture: UploadedTexture,
    /// Whether the frame reached the GPU as YUV planes rather than as RGBA.
    pub direct_yuv: bool,
    /// Whether the import had to wait for a completed decoder surface.
    pub surface_budget_stalled: bool,
    /// Where preparing this source spent its time.
    pub timings: SourceUploadTimings,
}

/// Where preparing one source texture spent its time.
///
/// `sourceConvertUploadMs` covers a software plane copy and a hardware surface
/// import, which are different work on different hardware paths, plus any wait
/// for surface budget that is not preparation at all. Comparing the decode
/// columns needs them apart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SourceUploadTimings {
    /// Writing decoded CPU planes into GPU-visible memory.
    pub cpu_plane_upload: Duration,
    /// Importing or copying a decoder surface onto the renderer's device.
    pub gpu_import: Duration,
    /// Blocked reclaiming decoder surfaces before this frame could reserve.
    pub surface_budget_wait: Duration,
}

impl SourceUploadTimings {
    /// Adds another source's spans to these, saturating on overflow.
    #[must_use]
    pub fn merged(self, other: Self) -> Self {
        Self {
            cpu_plane_upload: self.cpu_plane_upload.saturating_add(other.cpu_plane_upload),
            gpu_import: self.gpu_import.saturating_add(other.gpu_import),
            surface_budget_wait: self
                .surface_budget_wait
                .saturating_add(other.surface_budget_wait),
        }
    }
}

/// Why a decoded frame could not be prepared on the renderer's device.
#[derive(Debug)]
pub(crate) enum SurfaceError {
    /// This frame could not be read where it is, for a reason of its own,
    /// such as a failed import or per-frame allocation: its pixels can still
    /// be downloaded from the decoder and uploaded.
    Import(String),
    /// The device, or the decoder surface budget, could not serve the frame.
    /// A download waits on that same device, `FFmpeg`'s transfer without a
    /// timeout, so falling back would turn a bounded `GPU_TIMEOUT` into a hang
    /// and report the failure as an import fallback.
    Device(String),
}

impl SurfaceError {
    pub(crate) fn into_message(self) -> String {
        match self {
            Self::Import(message) | Self::Device(message) => message,
        }
    }
}

impl From<String> for SurfaceError {
    /// A failure is the frame's own unless it names a device fault.
    fn from(message: String) -> Self {
        if crate::protocol::names_device_fault(&message) {
            Self::Device(message)
        } else {
            Self::Import(message)
        }
    }
}

impl From<&str> for SurfaceError {
    fn from(message: &str) -> Self {
        Self::from(message.to_owned())
    }
}

/// What the next export's GPU frame supply on this thread is made to fail.
#[cfg(test)]
#[cfg_attr(
    not(all(
        any(target_os = "linux", target_os = "macos"),
        feature = "hwencode",
        feature = "hwdecode"
    )),
    allow(
        dead_code,
        reason = "only the hardware tests inject a fault, and only where a GPU-resident export imports decoder surfaces"
    )
)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum InjectedSurfaceFault {
    /// Every decoder surface fails to import, as a failed per-frame
    /// allocation does.
    Import,
    /// The decoder surface budget holds no surface at all.
    EmptyBudget,
}

#[cfg(test)]
thread_local! {
    /// Taken by the next [`crate::host::Host::export_gpu_frames`] on this thread.
    pub(crate) static INJECT_SURFACE_FAULT: std::cell::Cell<Option<InjectedSurfaceFault>> =
        const { std::cell::Cell::new(None) };
}

/// A source texture on the renderer's device, with the GPU work it still
/// waits on before it may be sampled.
pub struct UploadedTexture {
    /// Keeps the presented texture alive for as long as its view is used.
    pub _texture: RenderedTexture,
    pub view: wgpu::TextureView,
    pub pending: Option<PendingSourceWork>,
}

/// The decoded picture a source texture was made from: what its pixels depend
/// on within one export, whatever times it is asked for at.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct PhysicalFrameKey {
    generation: u64,
    source: SourceId,
    pts: MediaTime,
}

impl PhysicalFrameKey {
    pub(crate) fn new(generation: u64, source: &SourceId, frame: &NativeVideoFrame) -> Self {
        Self {
            generation,
            source: source.clone(),
            pts: frame.pts(),
        }
    }

    fn new_cpu(generation: u64, source: &SourceId, frame: &DecodedVideoFrame) -> Self {
        Self {
            generation,
            source: source.clone(),
            pts: frame.pts,
        }
    }
}

/// A video frame installed for the times it is asked for at, with the picture
/// it shows: several times can show one picture (a source at a lower rate
/// than the export, or slowed down), and share its texture and its
/// [`FrameProvider::source_key`].
pub struct InstalledFrame {
    picture: PhysicalFrameKey,
    pub texture: Arc<UploadedTexture>,
}

/// The installed frames by source and by the time each was asked for at.
///
/// Keyed by source first, so a lookup borrows the [`SourceId`] it is asked
/// with: [`FrameProvider`] asks once per clip per frame, and an owned
/// `(SourceId, MediaTime)` key would copy the source's ID every time. A
/// source keeps its entry only while it has frames.
pub struct RequestedFrames<T = InstalledFrame> {
    sources: HashMap<SourceId, HashMap<MediaTime, T>>,
}

impl<T> Default for RequestedFrames<T> {
    fn default() -> Self {
        Self {
            sources: HashMap::new(),
        }
    }
}

impl<T> RequestedFrames<T> {
    pub fn get(&self, source: &SourceId, time: MediaTime) -> Option<&T> {
        self.sources.get(source)?.get(&time)
    }

    pub fn contains(&self, source: &SourceId, time: MediaTime) -> bool {
        self.get(source, time).is_some()
    }

    /// Installs `frame` for `source` at `time`, copying the source's ID only
    /// when it has no frames yet.
    pub fn insert(&mut self, source: &SourceId, time: MediaTime, frame: T) {
        if let Some(times) = self.sources.get_mut(source) {
            times.insert(time, frame);
        } else {
            self.sources
                .insert(source.clone(), HashMap::from([(time, frame)]));
        }
    }

    /// Keeps only the frames `needed` asks for.
    pub fn retain_needed(&mut self, needed: &HashSet<(&SourceId, MediaTime)>) {
        self.sources.retain(|source, times| {
            times.retain(|time, _| needed.contains(&(source, *time)));
            !times.is_empty()
        });
    }

    pub fn clear(&mut self) {
        self.sources.clear();
    }

    /// Frames installed, over every source.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.sources.values().map(HashMap::len).sum()
    }
}

#[derive(Default)]
pub struct FrameInstallMetrics {
    pub duration: Duration,
    pub direct_yuv_frames: u64,
    pub rgba_fallback_frames: u64,
    pub physical_reuses: u64,
    pub hardware_import_frames: u64,
    pub hardware_copy_frames: u64,
    pub source_upload_bytes: u64,
    pub source_gpu_copy_bytes: u64,
    pub native_surface_bytes: u64,
    pub native_surfaces: u32,
    /// Times a frame waited for completed surfaces before it could import.
    pub surface_budget_stalls: u64,
    /// Hardware frames whose import failed and which were uploaded from a
    /// CPU download of the surface instead.
    pub import_fallbacks: u64,
    /// The bytes those downloads brought to the CPU, and the time they took.
    pub source_download_bytes: u64,
    pub source_download: Duration,
    /// The `duration` above, split by the work that consumed it.
    pub timings: SourceUploadTimings,
    /// Stills held on the GPU after this frame: the ones it shows.
    pub resident_stills: usize,
}

impl FrameInstallMetrics {
    /// Accounts for one prepared source and returns its texture.
    ///
    /// Byte counters stay split by path: a software frame's planes are an
    /// upload, a copied decoder surface is a GPU copy, and a directly imported
    /// surface is neither.
    fn record_upload(
        &mut self,
        upload: NativeUpload,
        hardware: bool,
        retained_bytes: u64,
    ) -> UploadedTexture {
        if upload.surface_budget_stalled {
            self.surface_budget_stalls = self.surface_budget_stalls.saturating_add(1);
        }
        self.timings = self.timings.merged(upload.timings);
        self.direct_yuv_frames = self
            .direct_yuv_frames
            .saturating_add(u64::from(upload.direct_yuv));
        self.rgba_fallback_frames = self
            .rgba_fallback_frames
            .saturating_add(u64::from(!upload.direct_yuv));
        self.hardware_import_frames = self
            .hardware_import_frames
            .saturating_add(u64::from(hardware));
        #[cfg(all(target_os = "windows", feature = "hwdecode"))]
        if hardware {
            self.hardware_copy_frames = self.hardware_copy_frames.saturating_add(1);
            self.source_gpu_copy_bytes = self.source_gpu_copy_bytes.saturating_add(retained_bytes);
        }
        if !hardware {
            self.source_upload_bytes = self.source_upload_bytes.saturating_add(retained_bytes);
        }
        upload.texture
    }
}

/// The source textures one frame of a pipelined export reads, prepared from
/// the decode pool's frame sets. Decoder surfaces are imported in place when
/// the platform allows it, within the decoder surface budget.
pub struct NativeGpuFrames {
    pub size: RenderSize,
    /// How HDR frames come into the working space: the project's color pipeline.
    pub color: ColorPolicy,
    generation: Option<u64>,
    pub frames: RequestedFrames,
    physical_frames: HashMap<PhysicalFrameKey, Arc<UploadedTexture>>,
    /// The stills the last frame showed, on the GPU, and no others.
    pub images: HashMap<SourceId, UploadedTexture>,
    /// Stills the last frame showed whose picture arrived with a frame taken
    /// from the render cache, so it was never uploaded: they are uploaded
    /// when a rendered frame shows them, since the decode pool does not send
    /// a still the previous frame showed again.
    waiting_images: HashMap<SourceId, Arc<NativeVideoFrame>>,
    /// Whether a hardware frame that fails to import may reach the export
    /// through a CPU download instead; off when hardware decode is required.
    pub import_fallback: bool,
    /// Every hardware frame fails to import, so the fallback runs anywhere.
    #[cfg(test)]
    fail_imports: bool,
    #[cfg(all(target_os = "linux", feature = "hwdecode"))]
    vulkan: Option<SharedVulkanContext>,
    #[cfg(all(target_os = "macos", feature = "hwdecode"))]
    metal: Option<SharedMetalContext>,
    #[cfg(all(target_os = "windows", feature = "hwdecode"))]
    d3d12: Option<SharedD3D12Context>,
    #[cfg(all(
        any(target_os = "linux", target_os = "macos", target_os = "windows"),
        feature = "hwdecode"
    ))]
    surface_budget: SurfaceBudget,
}

impl NativeGpuFrames {
    pub fn new(size: RenderSize) -> Self {
        Self {
            size,
            color: alpha2_engine::color_policy(&alpha2_model::ColorPipeline::default()),
            generation: None,
            frames: RequestedFrames::default(),
            physical_frames: HashMap::new(),
            images: HashMap::new(),
            waiting_images: HashMap::new(),
            import_fallback: true,
            #[cfg(test)]
            fail_imports: false,
            #[cfg(all(target_os = "linux", feature = "hwdecode"))]
            vulkan: None,
            #[cfg(all(target_os = "macos", feature = "hwdecode"))]
            metal: None,
            #[cfg(all(target_os = "windows", feature = "hwdecode"))]
            d3d12: None,
            #[cfg(all(
                any(target_os = "linux", target_os = "macos", target_os = "windows"),
                feature = "hwdecode"
            ))]
            surface_budget: SurfaceBudget::new(0, 0),
        }
    }

    #[cfg(all(target_os = "linux", feature = "hwdecode"))]
    pub fn with_vulkan(size: RenderSize, vulkan: Option<SharedVulkanContext>) -> Self {
        let mut frames = Self::new(size);
        frames.vulkan = vulkan;
        let (max_surfaces, max_bytes) = crate::config::configured_decode_surface_limits();
        frames.surface_budget = SurfaceBudget::new(max_surfaces, max_bytes);
        frames
    }

    #[cfg(all(target_os = "macos", feature = "hwdecode"))]
    pub fn with_metal(size: RenderSize, metal: Option<SharedMetalContext>) -> Self {
        let mut frames = Self::new(size);
        frames.metal = metal;
        let (max_surfaces, max_bytes) = crate::config::configured_decode_surface_limits();
        frames.surface_budget = SurfaceBudget::new(max_surfaces, max_bytes);
        frames
    }

    #[cfg(all(target_os = "windows", feature = "hwdecode"))]
    pub fn with_d3d12(size: RenderSize, d3d12: Option<SharedD3D12Context>) -> Self {
        let mut frames = Self::new(size);
        frames.d3d12 = d3d12;
        let (max_surfaces, max_bytes) = crate::config::configured_decode_surface_limits();
        frames.surface_budget = SurfaceBudget::new(max_surfaces, max_bytes);
        frames
    }

    /// Makes this supply fail as `fault` says.
    #[cfg(test)]
    pub(crate) fn inject(&mut self, fault: InjectedSurfaceFault) {
        match fault {
            InjectedSurfaceFault::Import => self.fail_imports = true,
            InjectedSurfaceFault::EmptyBudget => {
                #[cfg(all(
                    any(target_os = "linux", target_os = "macos", target_os = "windows"),
                    feature = "hwdecode"
                ))]
                {
                    self.surface_budget = SurfaceBudget::new(0, 0);
                }
            }
        }
    }

    /// The injected import failure, for a hardware frame.
    #[cfg(test)]
    fn injected_import_failure(&self, frame: &NativeVideoFrame) -> Result<(), SurfaceError> {
        if self.fail_imports && frame.is_hardware() {
            return Err(SurfaceError::Import("injected import failure".to_owned()));
        }
        Ok(())
    }

    fn upload_native(
        &self,
        renderer: &Renderer,
        frame: &NativeVideoFrame,
    ) -> Result<NativeUpload, SurfaceError> {
        #[cfg(test)]
        self.injected_import_failure(frame)?;
        #[cfg(all(target_os = "linux", feature = "hwdecode"))]
        {
            upload_native_frame(
                renderer,
                self.size,
                self.color,
                frame,
                self.vulkan.as_ref(),
                &self.surface_budget,
            )
        }
        #[cfg(not(all(target_os = "linux", feature = "hwdecode")))]
        #[cfg(not(all(target_os = "macos", feature = "hwdecode")))]
        #[cfg(not(all(target_os = "windows", feature = "hwdecode")))]
        {
            upload_native_frame(renderer, self.size, self.color, frame)
        }
        #[cfg(all(target_os = "macos", feature = "hwdecode"))]
        {
            upload_native_frame(
                renderer,
                self.size,
                self.color,
                frame,
                self.metal.as_ref(),
                &self.surface_budget,
            )
        }
        #[cfg(all(target_os = "windows", feature = "hwdecode"))]
        {
            upload_native_frame(
                renderer,
                self.size,
                self.color,
                frame,
                self.d3d12.as_ref(),
                &self.surface_budget,
            )
        }
    }

    /// Uploads one frame, and when a hardware surface cannot be imported,
    /// downloads its pixels and uploads those instead.
    ///
    /// Returns whether the surface itself was imported. An import can fail
    /// for reasons that have nothing to do with the stream, such as a failed
    /// per-frame allocation, and the pixels are still on the decoder, so `auto`
    /// hardware decode survives it. A device or budget failure is not one of
    /// those: it fails the frame ([`SurfaceError::Device`]).
    pub(crate) fn upload_or_download(
        &self,
        renderer: &Renderer,
        frame: &NativeVideoFrame,
        metrics: &mut FrameInstallMetrics,
    ) -> Result<(NativeUpload, bool), String> {
        match self.upload_native(renderer, frame) {
            Ok(upload) => Ok((upload, true)),
            Err(SurfaceError::Import(error)) if frame.is_hardware() && self.import_fallback => {
                let download = Instant::now();
                let cpu = frame.to_cpu().map_err(|download| {
                    format!(
                        "{error}; downloading the surface instead also failed: {}",
                        error_message(download)
                    )
                })?;
                metrics.source_download =
                    metrics.source_download.saturating_add(download.elapsed());
                metrics.source_download_bytes = metrics
                    .source_download_bytes
                    .saturating_add(plane_bytes(&cpu));
                metrics.import_fallbacks = metrics.import_fallbacks.saturating_add(1);
                let upload = self
                    .upload_native(renderer, &NativeVideoFrame::Cpu(cpu))
                    .map_err(SurfaceError::into_message)?;
                Ok((upload, false))
            }
            Err(error) => Err(error.into_message()),
        }
    }

    /// Lets go of every still `stills` does not show.
    fn keep_stills(&mut self, stills: &[SourceId]) {
        self.images.retain(|source, _| stills.contains(source));
        self.waiting_images
            .retain(|source, _| stills.contains(source));
    }

    /// Takes a decoded frame set whose frame is not rendered, because the
    /// render cache already holds it, without touching the GPU.
    ///
    /// Nothing is uploaded: an upload is a queue write, and with no render to
    /// submit it, its staging memory would stay allocated for as long as
    /// frames keep coming from the cache. The stills it shows are
    /// still the ones held, because the decode pool will not send them again
    /// for the next frame; a still whose picture came with this set waits on
    /// the CPU for a rendered frame to upload it.
    pub fn pass(&mut self, decoded: DecodedFrameSet) {
        self.keep_stills(&decoded.stills);
        for frame in decoded.frames {
            if frame.kind == MediaKind::Image && !self.images.contains_key(&frame.source) {
                self.waiting_images.insert(frame.source, frame.frame);
            }
        }
    }

    /// Uploads the sources one rendered frame reads and keeps nothing it does
    /// not: video frames of other times and stills it does not show.
    pub fn install(
        &mut self,
        renderer: &Renderer,
        decoded: DecodedFrameSet,
    ) -> Result<FrameInstallMetrics, String> {
        let started = Instant::now();
        let mut metrics = FrameInstallMetrics::default();
        let generation = decoded.generation;
        if self.generation != Some(generation) {
            self.generation = Some(generation);
            self.frames.clear();
            self.physical_frames.clear();
        }
        let needed = decoded
            .frames
            .iter()
            .filter(|frame| frame.kind != MediaKind::Image)
            .map(|frame| (&frame.source, frame.requested))
            .collect::<HashSet<_>>();
        let physical_needed = decoded
            .frames
            .iter()
            .filter(|frame| frame.kind != MediaKind::Image)
            .map(|frame| PhysicalFrameKey::new(generation, &frame.source, &frame.frame))
            .collect::<HashSet<_>>();
        self.frames.retain_needed(&needed);
        self.physical_frames
            .retain(|key, _| physical_needed.contains(key));
        self.keep_stills(&decoded.stills);
        let DecodedFrameSet {
            index,
            frames,
            stills,
            ..
        } = decoded;
        for decoded in frames {
            if decoded.kind == MediaKind::Image {
                self.waiting_images.remove(&decoded.source);
                if self.images.contains_key(&decoded.source) {
                    continue;
                }
                let hardware = matches!(&*decoded.frame, NativeVideoFrame::Hardware { .. });
                let retained_bytes = decoded.frame.retained_bytes();
                let (upload, imported) =
                    self.upload_or_download(renderer, &decoded.frame, &mut metrics)?;
                let uploaded = metrics.record_upload(upload, hardware && imported, retained_bytes);
                self.images.insert(decoded.source, uploaded);
            } else {
                self.install_video(renderer, generation, &decoded, &mut metrics)?;
            }
        }
        for (source, frame) in std::mem::take(&mut self.waiting_images) {
            let hardware = matches!(&*frame, NativeVideoFrame::Hardware { .. });
            let retained_bytes = frame.retained_bytes();
            let (upload, imported) = self.upload_or_download(renderer, &frame, &mut metrics)?;
            let uploaded = metrics.record_upload(upload, hardware && imported, retained_bytes);
            self.images.insert(source, uploaded);
        }
        if let Some(missing) = stills
            .iter()
            .find(|source| !self.images.contains_key(*source))
        {
            return Err(format!(
                "{}still {} reached frame {index} without its picture",
                crate::protocol::internal_tag(),
                missing.as_str(),
            ));
        }
        metrics.resident_stills = self.images.len();
        metrics.duration = started.elapsed();
        #[cfg(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        ))]
        {
            let usage = self.surface_budget.usage();
            metrics.native_surface_bytes = usage.bytes;
            metrics.native_surfaces = usage.surfaces;
        }
        Ok(metrics)
    }

    /// Installs a video frame for the time it was asked for at, sharing the
    /// texture of its picture when another time already shows it.
    fn install_video(
        &mut self,
        renderer: &Renderer,
        generation: u64,
        decoded: &DecodedSourceFrame,
        metrics: &mut FrameInstallMetrics,
    ) -> Result<(), String> {
        if self.frames.contains(&decoded.source, decoded.requested) {
            return Ok(());
        }
        let picture = PhysicalFrameKey::new(generation, &decoded.source, &decoded.frame);
        let texture = if let Some(uploaded) = self.physical_frames.get(&picture) {
            metrics.physical_reuses = metrics.physical_reuses.saturating_add(1);
            Arc::clone(uploaded)
        } else {
            let hardware = matches!(&*decoded.frame, NativeVideoFrame::Hardware { .. });
            let retained_bytes = decoded.frame.retained_bytes();
            let (upload, imported) = self.upload_or_download(renderer, &decoded.frame, metrics)?;
            let uploaded =
                Arc::new(metrics.record_upload(upload, hardware && imported, retained_bytes));
            self.physical_frames
                .insert(picture.clone(), Arc::clone(&uploaded));
            uploaded
        };
        self.frames.insert(
            &decoded.source,
            decoded.requested,
            InstalledFrame { picture, texture },
        );
        Ok(())
    }
}

impl FrameProvider for NativeGpuFrames {
    fn texture_for(&self, source: &SourceId, time: MediaTime) -> Option<SourceView<'_>> {
        self.frames
            .get(source, time)
            .map(|frame| SourceView::from_wgpu(&frame.texture.view))
            .or_else(|| {
                self.images
                    .get(source)
                    .map(|texture| SourceView::from_wgpu(&texture.view))
            })
    }

    fn source_key(&self, source: &SourceId, time: MediaTime) -> Option<RenderCacheKey> {
        export_source_key(
            source,
            self.frames.get(source, time).map(|frame| &frame.picture),
            self.images.contains_key(source),
        )
    }

    fn pending_source_work(
        &self,
        source: &SourceId,
        time: MediaTime,
    ) -> Option<&PendingSourceWork> {
        self.frames
            .get(source, time)
            .and_then(|frame| frame.texture.pending.as_ref())
            .or_else(|| {
                self.images
                    .get(source)
                    .and_then(|texture| texture.pending.as_ref())
            })
    }
}

/// What supplying one frame's sources on the coordinator cost.
#[derive(Default)]
pub struct SourceSupplyMetrics {
    pub decode: Duration,
    pub convert_upload: Duration,
    pub direct_yuv_frames: u64,
    pub rgba_fallback_frames: u64,
    pub physical_reuses: u64,
    /// Video pictures decoded, all of them on the CPU: this supply opens
    /// software decoders only.
    pub software_decoded_frames: u64,
}

/// The source textures one frame reads, decoded synchronously on the calling
/// thread with software decoders: the comparison path's frame supply.
pub struct NativeFrames {
    pub size: RenderSize,
    /// Sources an extension provider draws, which no decoder opens.
    pub providers: std::sync::Arc<crate::providers::ProviderMedia>,
    /// How HDR frames come into the working space: the project's color pipeline.
    pub color: ColorPolicy,
    pub paths: HashMap<SourceId, PathBuf>,
    pub kinds: HashMap<SourceId, MediaKind>,
    /// The color each video source's project declares, which its frames carry.
    pub declared_colors: HashMap<SourceId, alpha2_media::ColorMetadata>,
    pub decoders: HashMap<DecodeCursorKey, NativeDecoder>,
    pub decoded: HashMap<DecodeCursorKey, DecodedPosition>,
    pub frames: RequestedFrames,
    physical_frames: HashMap<PhysicalFrameKey, Arc<UploadedTexture>>,
    pub images: HashMap<SourceId, UploadedTexture>,
    /// Frames supplied so far: the index of the one being supplied, which
    /// the provider sources' budget keeps open.
    supplied: u64,
    /// The export's decode policy, and where its decoders report.
    decoding: std::sync::Arc<crate::recovery::ExportDecoding>,
}

/// The color each host-supplied video source of `project` declares
/// (`color_metadata`), which wins over what its frames and stream are tagged
/// with, on every host. A still is read as sRGB code values whatever it
/// declares.
pub(crate) fn declared_video_colors(
    project: &Project,
) -> HashMap<SourceId, alpha2_media::ColorMetadata> {
    project
        .sources
        .iter()
        .filter(|source| source.kind == MediaKind::Video && source.is_host_supplied())
        .filter_map(|source| Some((source.id.clone(), source.color_metadata?)))
        .collect()
}

impl NativeFrames {
    pub fn new(
        size: RenderSize,
        project: &Project,
        assets: &HashMap<String, PathBuf>,
        decoding: std::sync::Arc<crate::recovery::ExportDecoding>,
    ) -> Result<Self, String> {
        let mut paths = HashMap::new();
        let mut kinds = HashMap::new();
        for source in project
            .used_sources()
            .into_iter()
            .filter(|source| source.is_host_supplied())
        {
            kinds.insert(source.id.clone(), source.kind);
            if source.provider.is_some() {
                continue;
            }
            let path = assets.get(source.id.as_str()).ok_or_else(|| {
                crate::protocol::missing_input(format!(
                    "missing uploaded asset {}",
                    source.id.as_str()
                ))
            })?;
            paths.insert(source.id.clone(), path.clone());
        }
        Ok(Self {
            size,
            providers: crate::providers::ProviderMedia::none(),
            color: alpha2_engine::color_policy(&project.settings.color),
            paths,
            kinds,
            declared_colors: declared_video_colors(project),
            decoders: HashMap::new(),
            decoded: HashMap::new(),
            frames: RequestedFrames::default(),
            physical_frames: HashMap::new(),
            images: HashMap::new(),
            supplied: 0,
            decoding,
        })
    }

    /// Supplies the frames `needs` asks for: what the clips and their
    /// compositions read at one instant ([`alpha2_eval::frame_needs`]), so a
    /// generator or composition source asks for nothing and a composition's
    /// media steps each read through their own cursor.
    pub fn supply(
        &mut self,
        renderer: &Renderer,
        needs: &[alpha2_eval::FrameNeed],
    ) -> Result<SourceSupplyMetrics, String> {
        let mut metrics = SourceSupplyMetrics::default();
        self.supplied += 1;
        // Keep only this frame's exact requests so long exports do not retain one full-resolution
        // texture per frame.
        let needed = needs
            .iter()
            .map(|need| (&need.source_id, need.media_time))
            .collect::<HashSet<_>>();
        self.frames.retain_needed(&needed);
        // So are stills: one this frame does not show is let go.
        let shown = needs
            .iter()
            .map(|need| &need.source_id)
            .collect::<HashSet<_>>();
        self.images.retain(|source, _| shown.contains(source));
        let active_cursors = needs.iter().map(decode_cursor_key).collect::<HashSet<_>>();
        self.decoders
            .retain(|cursor, _| active_cursors.contains(cursor));
        self.decoded
            .retain(|cursor, _| active_cursors.contains(cursor));
        for need in needs {
            if self.texture_for(&need.source_id, need.media_time).is_some() {
                continue;
            }
            self.supply_need(
                renderer,
                decode_cursor_key(need),
                need.media_time,
                &mut metrics,
            )?;
        }
        let physical_needed = self
            .decoded
            .iter()
            .map(|((source, _, _), position)| PhysicalFrameKey::new_cpu(0, source, &position.frame))
            .collect::<HashSet<_>>();
        self.physical_frames
            .retain(|key, _| physical_needed.contains(key));
        Ok(metrics)
    }

    /// Supplies the frame `cursor`'s source shows at `requested`, read
    /// through that cursor's decoder.
    pub fn supply_need(
        &mut self,
        renderer: &Renderer,
        cursor: DecodeCursorKey,
        requested: MediaTime,
        metrics: &mut SourceSupplyMetrics,
    ) -> Result<(), String> {
        let source = cursor.0.clone();
        let kind = self
            .kinds
            .get(&source)
            .copied()
            .ok_or_else(|| format!("unknown source {}", source.as_str()))?;
        if self.providers.supplies(&source) {
            return self.supply_provided(renderer, &cursor, kind, requested, metrics);
        }
        if !self.decoders.contains_key(&cursor) {
            let path = self.paths.get(&source).ok_or_else(|| {
                crate::protocol::missing_input(format!(
                    "missing source path for {}",
                    source.as_str()
                ))
            })?;
            let decode_started = Instant::now();
            let mut decoder = NativeDecoder::open(path).map_err(error_message)?;
            decoder.declare_color(self.declared_colors.get(&source).copied());
            self.decoders.insert(
                cursor.clone(),
                crate::recovery::video(decoder, &source, path, &self.decoding),
            );
            metrics.decode = metrics.decode.saturating_add(decode_started.elapsed());
        }
        if kind == MediaKind::Image {
            return self.supply_image(renderer, &cursor, metrics);
        }
        self.supply_video(renderer, cursor, requested, metrics)
    }

    /// Supplies a frame an extension provider draws, uploaded like a decoded one.
    fn supply_provided(
        &mut self,
        renderer: &Renderer,
        cursor: &DecodeCursorKey,
        kind: MediaKind,
        requested: MediaTime,
        metrics: &mut SourceSupplyMetrics,
    ) -> Result<(), String> {
        let source = cursor.0.clone();
        if kind == MediaKind::Image && self.images.contains_key(&source) {
            return Ok(());
        }
        let decode_started = Instant::now();
        let frame = Arc::new(self.providers.frame(cursor, requested, self.supplied)?);
        metrics.decode = metrics.decode.saturating_add(decode_started.elapsed());
        let mut upload = |frames: &Self| {
            let upload_started = Instant::now();
            let upload = upload_frame(renderer, frames.size, frames.color, &frame)?;
            metrics.convert_upload = metrics
                .convert_upload
                .saturating_add(upload_started.elapsed());
            metrics.rgba_fallback_frames = metrics.rgba_fallback_frames.saturating_add(1);
            Ok::<_, String>(upload.texture)
        };
        if kind == MediaKind::Image {
            let texture = upload(self)?;
            self.images.insert(source, texture);
            return Ok(());
        }
        let physical_key = PhysicalFrameKey::new_cpu(0, &source, &frame);
        let uploaded = if let Some(uploaded) = self.physical_frames.get(&physical_key) {
            metrics.physical_reuses = metrics.physical_reuses.saturating_add(1);
            Arc::clone(uploaded)
        } else {
            let uploaded = Arc::new(upload(self)?);
            self.physical_frames
                .insert(physical_key.clone(), Arc::clone(&uploaded));
            uploaded
        };
        self.frames.insert(
            &source,
            requested,
            InstalledFrame {
                picture: physical_key,
                texture: uploaded,
            },
        );
        Ok(())
    }

    /// Decodes a still's one picture and keeps it on the GPU while it shows.
    pub fn supply_image(
        &mut self,
        renderer: &Renderer,
        cursor: &DecodeCursorKey,
        metrics: &mut SourceSupplyMetrics,
    ) -> Result<(), String> {
        let source = cursor.0.clone();
        let decoder = self
            .decoders
            .get_mut(cursor)
            .ok_or_else(|| "decoder disappeared".to_owned())?;
        let decode_started = Instant::now();
        let DecodeEvent::Video(frame) = decoder
            .decode_next(&CancelToken::default())
            .map_err(error_message)?
        else {
            return Err(format!(
                "image source {} did not decode a frame",
                source.as_str()
            ));
        };
        metrics.decode = metrics.decode.saturating_add(decode_started.elapsed());
        let upload_started = Instant::now();
        let upload = upload_frame(renderer, self.size, self.color, &frame)?;
        let (uploaded, direct_yuv) = (upload.texture, upload.direct_yuv);
        metrics.direct_yuv_frames = metrics
            .direct_yuv_frames
            .saturating_add(u64::from(direct_yuv));
        metrics.rgba_fallback_frames = metrics
            .rgba_fallback_frames
            .saturating_add(u64::from(!direct_yuv));
        metrics.convert_upload = metrics
            .convert_upload
            .saturating_add(upload_started.elapsed());
        self.images.insert(source, uploaded);
        Ok(())
    }

    /// Advances the cursor's decoder to `requested`, seeking unless the time
    /// follows on from the cursor's last one, and uploads the frame shown.
    pub fn supply_video(
        &mut self,
        renderer: &Renderer,
        cursor: DecodeCursorKey,
        requested: MediaTime,
        metrics: &mut SourceSupplyMetrics,
    ) -> Result<(), String> {
        let decode_started = Instant::now();
        let source = cursor.0.clone();
        let decoder = self
            .decoders
            .get_mut(&cursor)
            .ok_or_else(|| "decoder disappeared".to_owned())?;
        let previous = self.decoded.remove(&cursor);
        let mut position = if let Some(position) =
            previous.filter(|item| crate::decode::decodes_on(item.requested, requested))
        {
            position
        } else {
            let result = decoder
                .seek(
                    DecodeRequest {
                        source: source.clone(),
                        time: requested,
                        kind: DecodeRequestKind::Seek,
                        generation: 0,
                    },
                    &CancelToken::default(),
                )
                .map_err(error_message)?;
            let DecodeEvent::Video(frame) = result.event else {
                return Err(format!(
                    "video source {} did not decode a frame",
                    source.as_str()
                ));
            };
            metrics.software_decoded_frames = metrics.software_decoded_frames.saturating_add(1);
            DecodedPosition {
                requested,
                frame: Arc::new(frame),
            }
        };
        // As the pipelined decode resolves it: the last frame at or before the deadline.
        let deadline = decoder.presentation_deadline(requested);
        while decoder
            .peek_video_pts(&CancelToken::default())
            .map_err(error_message)?
            .is_some_and(|pts| pts <= deadline)
        {
            match decoder
                .decode_next(&CancelToken::default())
                .map_err(error_message)?
            {
                DecodeEvent::Video(frame) => {
                    metrics.software_decoded_frames =
                        metrics.software_decoded_frames.saturating_add(1);
                    position.frame = Arc::new(frame);
                }
                event => {
                    return Err(format!(
                        "video source {} returned {event:?} for a peeked frame",
                        source.as_str()
                    ));
                }
            }
        }
        position.requested = requested;
        metrics.decode = metrics.decode.saturating_add(decode_started.elapsed());
        let physical_key = PhysicalFrameKey::new_cpu(0, &source, &position.frame);
        let uploaded = if let Some(uploaded) = self.physical_frames.get(&physical_key) {
            metrics.physical_reuses = metrics.physical_reuses.saturating_add(1);
            Arc::clone(uploaded)
        } else {
            let upload_started = Instant::now();
            let upload = upload_frame(renderer, self.size, self.color, &position.frame)?;
            let (uploaded, direct_yuv) = (upload.texture, upload.direct_yuv);
            metrics.convert_upload = metrics
                .convert_upload
                .saturating_add(upload_started.elapsed());
            metrics.direct_yuv_frames = metrics
                .direct_yuv_frames
                .saturating_add(u64::from(direct_yuv));
            metrics.rgba_fallback_frames = metrics
                .rgba_fallback_frames
                .saturating_add(u64::from(!direct_yuv));
            let uploaded = Arc::new(uploaded);
            self.physical_frames
                .insert(physical_key.clone(), Arc::clone(&uploaded));
            uploaded
        };
        self.decoded.insert(cursor, position);
        self.frames.insert(
            &source,
            requested,
            InstalledFrame {
                picture: physical_key,
                texture: uploaded,
            },
        );
        Ok(())
    }
}

/// The frame a decode cursor last presented, and the time it was asked for.
pub struct DecodedPosition {
    pub requested: MediaTime,
    pub frame: Arc<DecodedVideoFrame>,
}

#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
fn gpu_plane_format(layout: alpha2_native_gpu::NativePlaneLayout) -> alpha2_media::YuvFormat {
    match layout {
        alpha2_native_gpu::NativePlaneLayout::I420 => alpha2_media::YuvFormat::I420,
        alpha2_native_gpu::NativePlaneLayout::Nv12 => alpha2_media::YuvFormat::NV12,
        alpha2_native_gpu::NativePlaneLayout::P010 => alpha2_media::YuvFormat::P010,
    }
}

/// Reserves one decoder surface, reclaiming completed ones when the budget is
/// full.
///
/// Running out of budget is backpressure, not a failure: the surfaces already
/// handed to the GPU are released as soon as their submission completes, so
/// waiting for that and retrying keeps the export going. Only a budget too
/// small to hold the surfaces of a single frame cannot be satisfied this way,
/// and that is a configuration error worth reporting as one. Both it and a
/// reclaim the device did not complete in time are [`SurfaceError::Device`]:
/// a download of the same surface would meet them again.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
fn reserve_surface(
    renderer: &Renderer,
    budget: &SurfaceBudget,
    bytes: u64,
) -> Result<(SurfaceReservation, bool, Duration), SurfaceError> {
    if let Ok(reservation) = budget.try_reserve(bytes) {
        return Ok((reservation, false, Duration::ZERO));
    }
    let waited = Instant::now();
    let _ = renderer
        .reclaim_external_leases()
        .map_err(|error| SurfaceError::Device(error_message(error)))?;
    let reclaimed = budget.try_reserve(bytes).map_err(|_| {
        let usage = budget.usage();
        SurfaceError::Device(format!(
            "the decoder surface budget cannot hold this frame: {bytes} more bytes are needed \
             while {} surfaces and {} bytes are retained by work in flight. Raise \
             ALPHA2_DECODE_SURFACES or ALPHA2_DECODE_SURFACE_BYTES, or lower \
             ALPHA2_DECODE_LOOKAHEAD.",
            usage.surfaces, usage.bytes
        ))
    })?;
    Ok((reclaimed, true, waited.elapsed()))
}

/// Prepares a decoded frame on the renderer's device: a CPU frame is
/// uploaded, a hardware one imported (or copied, on D3D12) within
/// `surface_budget`.
///
/// # Errors
/// An HDR frame the color policy refuses, a surface from another device, or
/// a failed import ([`SurfaceError::Import`]) or device wait
/// ([`SurfaceError::Device`]).
#[allow(clippy::too_many_lines)]
pub fn upload_native_frame(
    renderer: &Renderer,
    size: RenderSize,
    color: ColorPolicy,
    frame: &NativeVideoFrame,
    #[cfg(all(target_os = "linux", feature = "hwdecode"))] vulkan: Option<&SharedVulkanContext>,
    #[cfg(all(target_os = "linux", feature = "hwdecode"))] surface_budget: &SurfaceBudget,
    #[cfg(all(target_os = "macos", feature = "hwdecode"))] metal: Option<&SharedMetalContext>,
    #[cfg(all(target_os = "macos", feature = "hwdecode"))] surface_budget: &SurfaceBudget,
    #[cfg(all(target_os = "windows", feature = "hwdecode"))] d3d12: Option<&SharedD3D12Context>,
    #[cfg(all(target_os = "windows", feature = "hwdecode"))] surface_budget: &SurfaceBudget,
) -> Result<NativeUpload, SurfaceError> {
    let frame_color = match frame {
        NativeVideoFrame::Cpu(frame) => frame.color,
        NativeVideoFrame::Hardware { metadata, .. } => metadata.color,
    };
    refuse_hdr(color, frame_color)?;
    match frame {
        NativeVideoFrame::Cpu(frame) => Ok(upload_frame(renderer, size, color, frame)?),
        NativeVideoFrame::Hardware { metadata, surface } => {
            #[cfg(all(target_os = "linux", feature = "hwdecode"))]
            {
                let context = vulkan
                    .ok_or_else(|| "hardware frame has no shared Vulkan context".to_owned())?;
                let lease = Arc::clone(surface)
                    .into_any()
                    .downcast::<VulkanFrameLease>()
                    .map_err(|_| "hardware frame is not an FFmpeg Vulkan surface".to_owned())?;
                // Reserve before frame/queue acquire so pressure cannot leave a decoder surface
                // locked while waiting for budget.
                let (reservation, stalled, surface_budget_wait) =
                    reserve_surface(renderer, surface_budget, surface.retained_bytes())?;
                let import_started = Instant::now();
                let imported = context.import_surface(lease).map_err(error_message)?;
                // Normalize against the imported image's own extent so the shader cannot disagree
                // with the texture it samples.
                let coded = RenderSize::new(imported.coded_size[0], imported.coded_size[1])
                    .map_err(error_message)?;
                let presentation = SourcePresentation {
                    output_size: size,
                    visible_rect: metadata.visible_rect,
                    orientation: metadata.orientation,
                    sample_aspect_ratio: (
                        metadata.sample_aspect_ratio.num,
                        metadata.sample_aspect_ratio.den,
                    ),
                    chroma_siting: metadata.chroma_location,
                };
                let external: Arc<dyn ExternalTextureLease> = Arc::new(HostVulkanLease {
                    import: Arc::clone(&imported.lease),
                    _reservation: reservation,
                });
                let (prepared, diagnostics) = renderer
                    .prepare_gpu_yuv_presented(
                        GpuVideoPlanes {
                            y: &imported.y,
                            chroma: &imported.uv,
                            v: None,
                            format: gpu_plane_format(imported.layout),
                            size: coded,
                            alpha: 1.0,
                            color: metadata.color,
                        },
                        presentation,
                        color,
                        external,
                    )
                    .map_err(error_message)?;
                if diagnostics
                    .iter()
                    .any(|item| matches!(item, alpha2_render::ColorDiagnostic::UnsupportedHdr))
                {
                    return Err("the project skips HDR sources".into());
                }
                let (presented, pending) = prepared.into_parts();
                let view = presented.view().clone();
                Ok(NativeUpload {
                    texture: UploadedTexture {
                        view,
                        pending,
                        _texture: presented,
                    },
                    direct_yuv: true,
                    surface_budget_stalled: stalled,
                    timings: SourceUploadTimings {
                        cpu_plane_upload: Duration::ZERO,
                        gpu_import: import_started.elapsed(),
                        surface_budget_wait,
                    },
                })
            }
            #[cfg(not(all(target_os = "linux", feature = "hwdecode")))]
            #[cfg(not(all(target_os = "macos", feature = "hwdecode")))]
            #[cfg(not(all(target_os = "windows", feature = "hwdecode")))]
            {
                let _ = (metadata, surface);
                Err("hardware frame import is unavailable".into())
            }
            #[cfg(all(target_os = "macos", feature = "hwdecode"))]
            {
                let context =
                    metal.ok_or_else(|| "hardware frame has no shared Metal context".to_owned())?;
                if metadata.device_identity != context.identity().device {
                    return Err("VideoToolbox frame belongs to another Metal device".into());
                }
                let lease = Arc::clone(surface)
                    .into_any()
                    .downcast::<VideoToolboxFrameLease>()
                    .map_err(|_| "hardware frame is not a VideoToolbox surface".to_owned())?;
                let (reservation, stalled, surface_budget_wait) =
                    reserve_surface(renderer, surface_budget, surface.retained_bytes())?;
                let import_started = Instant::now();
                let imported = context.import_surface(lease).map_err(error_message)?;
                let surface_color = videotoolbox_color(metadata.color, &imported);
                // Normalize against the imported image's own extent so the shader cannot disagree
                // with the texture it samples.
                let coded = RenderSize::new(imported.coded_size[0], imported.coded_size[1])
                    .map_err(error_message)?;
                let presentation = native_presentation(size, metadata, imported.visible_rect);
                let external: Arc<dyn ExternalTextureLease> = Arc::new(HostMetalLease {
                    import: Arc::clone(&imported.lease),
                    _reservation: reservation,
                });
                let (prepared, diagnostics) = renderer
                    .prepare_gpu_yuv_presented(
                        GpuVideoPlanes {
                            y: &imported.y,
                            chroma: &imported.uv,
                            v: None,
                            format: gpu_plane_format(imported.layout),
                            size: coded,
                            alpha: 1.0,
                            color: Some(surface_color),
                        },
                        presentation,
                        color,
                        external,
                    )
                    .map_err(error_message)?;
                if diagnostics
                    .iter()
                    .any(|item| matches!(item, alpha2_render::ColorDiagnostic::UnsupportedHdr))
                {
                    return Err("the project skips HDR sources".into());
                }
                let (presented, pending) = prepared.into_parts();
                let view = presented.view().clone();
                Ok(NativeUpload {
                    texture: UploadedTexture {
                        view,
                        pending,
                        _texture: presented,
                    },
                    direct_yuv: true,
                    surface_budget_stalled: stalled,
                    timings: SourceUploadTimings {
                        cpu_plane_upload: Duration::ZERO,
                        gpu_import: import_started.elapsed(),
                        surface_budget_wait,
                    },
                })
            }
            #[cfg(all(target_os = "windows", feature = "hwdecode"))]
            {
                let context =
                    d3d12.ok_or_else(|| "hardware frame has no shared D3D12 context".to_owned())?;
                if metadata.device_identity != context.identity().device {
                    return Err("D3D12VA frame belongs to another D3D12 device".into());
                }
                let lease = Arc::clone(surface)
                    .into_any()
                    .downcast::<D3D12FrameLease>()
                    .map_err(|_| "hardware frame is not a D3D12VA surface".to_owned())?;
                let (reservation, stalled, surface_budget_wait) = reserve_surface(
                    renderer,
                    surface_budget,
                    surface.retained_bytes().saturating_mul(2),
                )?;
                let import_started = Instant::now();
                let imported = context.copy_surface(lease).map_err(error_message)?;
                let coded =
                    RenderSize::new(imported.width, imported.height).map_err(error_message)?;
                let presentation = native_presentation(
                    size,
                    metadata,
                    [
                        metadata.visible_rect.x,
                        metadata.visible_rect.y,
                        metadata.visible_rect.width,
                        metadata.visible_rect.height,
                    ],
                );
                let external: Arc<dyn ExternalTextureLease> = Arc::new(HostD3D12Lease {
                    import: Arc::clone(&imported.lease),
                    _reservation: reservation,
                });
                let (prepared, diagnostics) = renderer
                    .prepare_gpu_yuv_presented(
                        GpuVideoPlanes {
                            y: &imported.y,
                            chroma: &imported.uv,
                            v: None,
                            format: gpu_plane_format(imported.layout),
                            size: coded,
                            alpha: 1.0,
                            color: metadata.color,
                        },
                        presentation,
                        color,
                        external,
                    )
                    .map_err(error_message)?;
                if diagnostics
                    .iter()
                    .any(|item| matches!(item, alpha2_render::ColorDiagnostic::UnsupportedHdr))
                {
                    return Err("the project skips HDR sources".into());
                }
                let (presented, pending) = prepared.into_parts();
                let view = presented.view().clone();
                Ok(NativeUpload {
                    texture: UploadedTexture {
                        view,
                        pending,
                        _texture: presented,
                    },
                    direct_yuv: true,
                    surface_budget_stalled: stalled,
                    timings: SourceUploadTimings {
                        cpu_plane_upload: Duration::ZERO,
                        gpu_import: import_started.elapsed(),
                        surface_budget_wait,
                    },
                })
            }
        }
    }
}

#[cfg(all(any(target_os = "macos", target_os = "windows"), feature = "hwdecode"))]
fn native_presentation(
    size: RenderSize,
    metadata: &alpha2_native::NativeVideoFrameMetadata,
    visible_rect: [u32; 4],
) -> SourcePresentation {
    SourcePresentation {
        output_size: size,
        visible_rect: alpha2_media::Rect {
            x: visible_rect[0],
            y: visible_rect[1],
            width: visible_rect[2],
            height: visible_rect[3],
        },
        orientation: metadata.orientation,
        sample_aspect_ratio: (
            metadata.sample_aspect_ratio.num,
            metadata.sample_aspect_ratio.den,
        ),
        chroma_siting: metadata.chroma_location,
    }
}

#[cfg(all(target_os = "linux", feature = "hwdecode"))]
#[derive(Debug)]
struct HostVulkanLease {
    import: Arc<VulkanImportLease>,
    _reservation: SurfaceReservation,
}

#[cfg(all(target_os = "linux", feature = "hwdecode"))]
impl ExternalTextureLease for HostVulkanLease {
    fn retained_bytes(&self) -> u64 {
        self.import.retained_bytes()
    }
    fn prepare_submission(&self) -> bool {
        self.import.prepare_submission();
        true
    }
    fn flush_submission(&self) -> Result<(), alpha2_render::RenderError> {
        self.import.flush_submission().map_err(|error| {
            // The frame fails with only what the status said about the device; the call and its
            // VkResult are in the log.
            alpha2_os::diagnostic!(
                "alpha2 native: decoded surface synchronization failed: {error}"
            );
            alpha2_render::RenderError::ExternalTextureSync(error.gpu_fault())
        })
    }
    fn submitted(&self) {
        let _ = self.import.submitted();
    }
    fn staged_signals(&self) -> usize {
        self.import.staged_signals()
    }
}

#[cfg(all(target_os = "macos", feature = "hwdecode"))]
#[derive(Debug)]
struct HostMetalLease {
    import: Arc<MetalImportLease>,
    _reservation: SurfaceReservation,
}

#[cfg(all(target_os = "macos", feature = "hwdecode"))]
impl ExternalTextureLease for HostMetalLease {
    fn retained_bytes(&self) -> u64 {
        self.import.retained_bytes()
    }

    fn submitted(&self) {
        self.import.submitted();
    }
}

#[cfg(all(target_os = "windows", feature = "hwdecode"))]
#[derive(Debug)]
struct HostD3D12Lease {
    import: Arc<D3D12CopyLease>,
    _reservation: SurfaceReservation,
}

#[cfg(all(target_os = "windows", feature = "hwdecode"))]
impl ExternalTextureLease for HostD3D12Lease {
    fn retained_bytes(&self) -> u64 {
        self.import.retained_bytes()
    }

    fn prepare_submission(&self) -> bool {
        self.import.prepare_submission();
        true
    }

    fn submitted(&self) {
        self.import.submitted();
    }
}

/// Refuses an HDR frame in a project whose color pipeline refuses HDR, in
/// words that say so.
pub(crate) fn refuse_hdr(
    color: ColorPolicy,
    frame: Option<alpha2_media::ColorMetadata>,
) -> Result<(), String> {
    if color == ColorPolicy::RejectHdr && frame.is_some_and(alpha2_render::is_hdr) {
        return Err(crate::protocol::code_tag(
            "INVALID_INPUT",
            "the project refuses HDR sources (its color pipeline's hdr is \"reject\")",
        ));
    }
    Ok(())
}

/// The renderer's view of a decoded YCbCr frame: its luma and chroma planes,
/// then its alpha plane when the format has one.
pub(crate) fn yuv_source(
    frame: &DecodedVideoFrame,
    format: alpha2_media::YuvFormat,
    size: RenderSize,
) -> YuvFrame<'_> {
    let plane = |index: usize| {
        frame.planes.get(index).map_or(
            SourcePlane {
                bytes: &[],
                stride: 0,
            },
            |plane| SourcePlane {
                bytes: &plane.bytes,
                stride: plane.stride,
            },
        )
    };
    let colors = format.color_plane_count();
    // With interleaved chroma the third plane is not read: an empty one, never the alpha plane.
    let third = if colors == 3 {
        plane(2)
    } else {
        SourcePlane {
            bytes: &[],
            stride: 0,
        }
    };
    YuvFrame {
        format,
        planes: [plane(0), plane(1), third],
        alpha_plane: format.alpha.then(|| plane(colors)),
        size,
        alpha: 1.0,
        color: frame.color,
    }
}

/// Uploads a CPU frame's planes and presents it at `size` on the GPU.
///
/// # Errors
/// An HDR frame the color policy refuses, a plane count that does not match
/// the pixel format, an unsupported format, or a renderer failure.
pub fn upload_frame(
    renderer: &Renderer,
    size: RenderSize,
    color: ColorPolicy,
    frame: &DecodedVideoFrame,
) -> Result<NativeUpload, String> {
    refuse_hdr(color, frame.color)?;
    let coded = frame
        .planes
        .first()
        .ok_or_else(|| "native decoder frame has no planes".to_owned())?;
    let coded_size = RenderSize::new(coded.width, coded.height).map_err(error_message)?;
    let presentation = SourcePresentation {
        output_size: size,
        visible_rect: frame.visible_rect,
        orientation: frame.orientation,
        sample_aspect_ratio: (frame.sample_aspect_ratio.num, frame.sample_aspect_ratio.den),
        chroma_siting: frame.chroma_location,
    };
    let plane = |index: usize| {
        frame.planes.get(index).map_or(
            SourcePlane {
                bytes: &[],
                stride: 0,
            },
            |plane| SourcePlane {
                bytes: &plane.bytes,
                stride: plane.stride,
            },
        )
    };
    let upload_started = Instant::now();
    // Both layouts upload planes into pooled textures and present on the GPU.
    let (prepared, direct_yuv) = match frame.pixel_format {
        PixelFormat::Yuv(format) => {
            if frame.planes.len() != format.plane_count() {
                return Err(format!(
                    "native YUV decoder frame must contain {} planes",
                    format.plane_count()
                ));
            }
            let (prepared, diagnostics) = renderer
                .prepare_yuv_presented(yuv_source(frame, format, coded_size), presentation, color)
                .map_err(error_message)?;
            if diagnostics.iter().any(|diagnostic| {
                matches!(diagnostic, alpha2_render::ColorDiagnostic::UnsupportedHdr)
            }) {
                return Err("the project skips HDR sources".to_owned());
            }
            (prepared, true)
        }
        PixelFormat::Rgba8 => {
            if frame.planes.len() != 1 {
                return Err("native RGBA decoder frame must contain one plane".to_owned());
            }
            let prepared = renderer
                .prepare_rgba8_presented(plane(0), coded_size, presentation)
                .map_err(error_message)?;
            (prepared, false)
        }
        PixelFormat::LinearRgbaF16 => {
            if frame.planes.len() != 1 {
                return Err("a linear RGBA frame must contain one plane".to_owned());
            }
            let upload = renderer
                .write_linear_rgba16f(coded_size, plane(0))
                .map_err(error_message)?;
            let prepared = renderer
                .prepare_linear_presented(upload, coded_size, presentation)
                .map_err(error_message)?;
            (prepared, false)
        }
        other => return Err(format!("native decoder returned {other:?}")),
    };
    let (presented, pending) = prepared.into_parts();
    let view = presented.view().clone();
    Ok(NativeUpload {
        texture: UploadedTexture {
            view,
            pending,
            _texture: presented,
        },
        direct_yuv,
        surface_budget_stalled: false,
        timings: SourceUploadTimings {
            cpu_plane_upload: upload_started.elapsed(),
            ..SourceUploadTimings::default()
        },
    })
}

impl FrameProvider for NativeFrames {
    fn texture_for(&self, source: &SourceId, time: MediaTime) -> Option<SourceView<'_>> {
        self.frames
            .get(source, time)
            .map(|frame| SourceView::from_wgpu(&frame.texture.view))
            .or_else(|| {
                self.images
                    .get(source)
                    .map(|texture| SourceView::from_wgpu(&texture.view))
            })
    }

    fn source_key(&self, source: &SourceId, time: MediaTime) -> Option<RenderCacheKey> {
        export_source_key(
            source,
            self.frames.get(source, time).map(|frame| &frame.picture),
            self.images.contains_key(source),
        )
    }

    fn pending_source_work(
        &self,
        source: &SourceId,
        time: MediaTime,
    ) -> Option<&PendingSourceWork> {
        self.frames
            .get(source, time)
            .and_then(|frame| frame.texture.pending.as_ref())
            .or_else(|| {
                self.images
                    .get(source)
                    .and_then(|texture| texture.pending.as_ref())
            })
    }
}

/// Bytes one in-flight readback holds in its mapped staging buffer.
pub fn estimated_staging_bytes(size: RenderSize, _format: wgpu::TextureFormat) -> u64 {
    // Pack textures return to the pool once their copy is submitted; only the mapped buffer stays
    // with the readback, laid out as the renderer lays it out.
    alpha2_render::yuv420p_staging_bytes(size).unwrap_or(u64::MAX)
}

/// Bytes of one tightly packed 8-bit 4:2:0 frame of `size`.
pub fn packed_yuv420_bytes(size: RenderSize) -> u64 {
    u64::from(size.width)
        .saturating_mul(u64::from(size.height))
        .saturating_mul(3)
        .div_euclid(2)
}

/// A source texture's key within one export. One export reads one project
/// with one set of assets, proxies, and decode and color policies, so a
/// source's decoded picture, or its still image, is fixed for the export: the
/// picture a video frame shows (`picture`: the source, the decode generation
/// and the picture's timestamp) or the source of a still names it. The
/// picture, not the time it was asked for at, so every output frame that
/// shows one picture has one key, as it has one texture. Keys from different
/// exports could collide, so the host clears the result cache when each
/// export starts.
pub(crate) fn export_source_key(
    source: &SourceId,
    picture: Option<&PhysicalFrameKey>,
    image: bool,
) -> Option<RenderCacheKey> {
    let mut hash = std::hash::DefaultHasher::new();
    if let Some(picture) = picture {
        std::hash::Hash::hash(&("frame", picture), &mut hash);
    } else if image {
        std::hash::Hash::hash(&(source.as_str(), "image"), &mut hash);
    } else {
        return None;
    }
    Some(RenderCacheKey::source(std::hash::Hasher::finish(&hash)))
}

/// What copying one decoded picture into encoder planes cost.
#[cfg(feature = "hwencode")]
#[derive(Debug, Default)]
pub struct NativeRepack {
    /// Whether the picture was a decoder surface read in place.
    pub hardware_import: bool,
    /// Bytes a GPU copy of that surface moved (D3D12, which copies rather
    /// than imports), or zero where it was read in place or uploaded.
    pub gpu_copy_bytes: u64,
    /// Whether a surface that failed to import was downloaded instead.
    pub import_fallback: bool,
    /// The bytes that download brought to the CPU, and the time it took.
    pub download_bytes: u64,
    pub download: Duration,
    /// Whether the import waited for completed decoder surfaces.
    pub surface_budget_stalled: bool,
    /// CPU plane bytes written to the GPU.
    pub upload_bytes: u64,
    /// Where preparing the picture spent its time.
    pub timings: SourceUploadTimings,
    /// Decoder-surface bytes and surfaces retained once the picture is
    /// copied, the surfaces of copies still in flight included.
    pub native_surface_bytes: u64,
    pub native_surfaces: u32,
}

/// Whether `metadata`'s picture, shown as it is, is already a `size` output
/// frame encoded as `encoding`: the renderer would neither move, scale,
/// rotate nor recolor it, and its chroma already sits where the output's must.
#[cfg(feature = "hwencode")]
pub(crate) fn check_repackable(
    metadata: &alpha2_native::NativeVideoFrameMetadata,
    size: RenderSize,
    encoding: alpha2_media::YuvEncoding,
) -> Result<(), String> {
    use alpha2_media::{ColorMetadata, YuvFormat};
    use alpha2_native::NativePlaneLayout;
    let visible = metadata.visible_rect;
    if (visible.width, visible.height) != (size.width, size.height) {
        return Err(format!(
            "a decoded picture is {}x{}, not the {}x{} output",
            visible.width, visible.height, size.width, size.height
        ));
    }
    if !metadata.orientation.is_upright() {
        return Err("a decoded picture is rotated or mirrored".to_owned());
    }
    let aspect = metadata.sample_aspect_ratio;
    if aspect != alpha2_media::SampleAspectRatio::default() {
        return Err(format!(
            "a decoded picture shows {}:{} pixels, which a render makes square",
            aspect.num, aspect.den
        ));
    }
    // The renderer decodes an undeclared picture as `UNDECLARED`, so that is what it would have
    // shown.
    let color = metadata.color.unwrap_or(ColorMetadata::UNDECLARED);
    if color != encoding.color {
        return Err(format!(
            "a decoded picture is {color:?}, not the output's {:?}",
            encoding.color
        ));
    }
    if !alpha2_native::same_chroma_siting(metadata.chroma_location, encoding.chroma) {
        return Err(format!(
            "a decoded picture's chroma sits {:?}, not where the output's does ({:?})",
            metadata.chroma_location, encoding.chroma
        ));
    }
    if !matches!(
        metadata.plane_layout,
        NativePlaneLayout::GpuNv12 | NativePlaneLayout::GpuI420
    ) && metadata.plane_layout != NativePlaneLayout::CpuYuv(YuvFormat::I420)
        && metadata.plane_layout != NativePlaneLayout::CpuYuv(YuvFormat::NV12)
    {
        return Err(format!(
            "a decoded picture is {:?}, not 8-bit 4:2:0",
            metadata.plane_layout
        ));
    }
    Ok(())
}

#[cfg(feature = "hwencode")]
impl NativeGpuFrames {
    /// Copies one decoded picture into NV12 plane targets unchanged: a
    /// decoder surface is read where it is, and a CPU picture is uploaded.
    ///
    /// A surface that cannot be imported is downloaded and copied from the
    /// CPU instead when this supply falls back at all, as an install would.
    ///
    /// # Errors
    /// Why the picture is not the output's as it is, or why the copy failed.
    pub fn repack(
        &self,
        renderer: &Renderer,
        frame: &NativeVideoFrame,
        targets: alpha2_render::Nv12PlaneTargets<'_>,
        encoding: alpha2_media::YuvEncoding,
        timings: &mut alpha2_render::FrameSubmitTimings,
    ) -> Result<NativeRepack, String> {
        check_repackable(&frame.metadata(), self.size, encoding)?;
        let repack = match self.repack_native(renderer, frame, targets, timings) {
            Ok(repack) => repack,
            Err(SurfaceError::Import(error)) if frame.is_hardware() && self.import_fallback => {
                let download = Instant::now();
                let cpu = frame.to_cpu().map_err(|download| {
                    format!(
                        "{error}; downloading the surface instead also failed: {}",
                        error_message(download)
                    )
                })?;
                let download = download.elapsed();
                let mut repack = repack_cpu(renderer, &cpu, targets, timings)?;
                repack.import_fallback = true;
                repack.download_bytes = plane_bytes(&cpu);
                repack.download = download;
                repack
            }
            Err(error) => return Err(error.into_message()),
        };
        let (native_surface_bytes, native_surfaces) = self.surface_usage();
        Ok(NativeRepack {
            native_surface_bytes,
            native_surfaces,
            ..repack
        })
    }

    /// Decoder-surface bytes and surfaces retained now.
    #[cfg_attr(
        not(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        )),
        allow(clippy::unused_self)
    )]
    fn surface_usage(&self) -> (u64, u32) {
        #[cfg(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        ))]
        {
            let usage = self.surface_budget.usage();
            (usage.bytes, usage.surfaces)
        }
        #[cfg(not(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        )))]
        {
            (0, 0)
        }
    }

    #[cfg_attr(
        not(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        )),
        allow(clippy::unused_self)
    )]
    #[allow(clippy::too_many_lines)]
    fn repack_native(
        &self,
        renderer: &Renderer,
        frame: &NativeVideoFrame,
        targets: alpha2_render::Nv12PlaneTargets<'_>,
        timings: &mut alpha2_render::FrameSubmitTimings,
    ) -> Result<NativeRepack, SurfaceError> {
        #[cfg(test)]
        self.injected_import_failure(frame)?;
        let (metadata, surface) = match frame {
            NativeVideoFrame::Cpu(frame) => {
                return Ok(repack_cpu(renderer, frame, targets, timings)?);
            }
            NativeVideoFrame::Hardware { metadata, surface } => (metadata, surface),
        };
        #[cfg(all(target_os = "linux", feature = "hwdecode"))]
        {
            let context = self
                .vulkan
                .as_ref()
                .ok_or_else(|| "hardware frame has no shared Vulkan context".to_owned())?;
            let lease = Arc::clone(surface)
                .into_any()
                .downcast::<VulkanFrameLease>()
                .map_err(|_| "hardware frame is not an FFmpeg Vulkan surface".to_owned())?;
            let (reservation, stalled, surface_budget_wait) =
                reserve_surface(renderer, &self.surface_budget, surface.retained_bytes())?;
            let import_started = Instant::now();
            let imported = context.import_surface(lease).map_err(error_message)?;
            let external: Arc<dyn ExternalTextureLease> = Arc::new(HostVulkanLease {
                import: Arc::clone(&imported.lease),
                _reservation: reservation,
            });
            Ok(repack_imported(
                renderer,
                ImportedPlanes {
                    y: &imported.y,
                    uv: &imported.uv,
                    format: gpu_plane_format(imported.layout),
                    coded_size: imported.coded_size,
                    color: metadata.color,
                    lease: external,
                },
                metadata.visible_rect,
                targets,
                timings,
                SourceUploadTimings {
                    gpu_import: import_started.elapsed(),
                    surface_budget_wait,
                    ..SourceUploadTimings::default()
                },
                stalled,
            )?)
        }
        #[cfg(all(target_os = "macos", feature = "hwdecode"))]
        {
            let context = self
                .metal
                .as_ref()
                .ok_or_else(|| "hardware frame has no shared Metal context".to_owned())?;
            if metadata.device_identity != context.identity().device {
                return Err("VideoToolbox frame belongs to another Metal device".into());
            }
            let lease = Arc::clone(surface)
                .into_any()
                .downcast::<VideoToolboxFrameLease>()
                .map_err(|_| "hardware frame is not a VideoToolbox surface".to_owned())?;
            let (reservation, stalled, surface_budget_wait) =
                reserve_surface(renderer, &self.surface_budget, surface.retained_bytes())?;
            let import_started = Instant::now();
            let imported = context.import_surface(lease).map_err(error_message)?;
            let surface_color = videotoolbox_color(metadata.color, &imported);
            let [x, y, width, height] = imported.visible_rect;
            let external: Arc<dyn ExternalTextureLease> = Arc::new(HostMetalLease {
                import: Arc::clone(&imported.lease),
                _reservation: reservation,
            });
            Ok(repack_imported(
                renderer,
                ImportedPlanes {
                    y: &imported.y,
                    uv: &imported.uv,
                    format: gpu_plane_format(imported.layout),
                    coded_size: imported.coded_size,
                    color: Some(surface_color),
                    lease: external,
                },
                alpha2_media::Rect {
                    x,
                    y,
                    width,
                    height,
                },
                targets,
                timings,
                SourceUploadTimings {
                    gpu_import: import_started.elapsed(),
                    surface_budget_wait,
                    ..SourceUploadTimings::default()
                },
                stalled,
            )?)
        }
        // A D3D12VA surface is copied rather than imported; both are held until the copy retires.
        #[cfg(all(target_os = "windows", feature = "hwdecode"))]
        {
            let context = self
                .d3d12
                .as_ref()
                .ok_or_else(|| "hardware frame has no shared D3D12 context".to_owned())?;
            if metadata.device_identity != context.identity().device {
                return Err("D3D12VA frame belongs to another D3D12 device"
                    .to_owned()
                    .into());
            }
            let lease = Arc::clone(surface)
                .into_any()
                .downcast::<D3D12FrameLease>()
                .map_err(|_| "hardware frame is not a D3D12VA surface".to_owned())?;
            let retained_bytes = surface.retained_bytes();
            let (reservation, stalled, surface_budget_wait) = reserve_surface(
                renderer,
                &self.surface_budget,
                retained_bytes.saturating_mul(2),
            )?;
            let import_started = Instant::now();
            let imported = context.copy_surface(lease).map_err(error_message)?;
            let external: Arc<dyn ExternalTextureLease> = Arc::new(HostD3D12Lease {
                import: Arc::clone(&imported.lease),
                _reservation: reservation,
            });
            let repack = repack_imported(
                renderer,
                ImportedPlanes {
                    y: &imported.y,
                    uv: &imported.uv,
                    format: gpu_plane_format(imported.layout),
                    coded_size: [imported.width, imported.height],
                    color: metadata.color,
                    lease: external,
                },
                metadata.visible_rect,
                targets,
                timings,
                SourceUploadTimings {
                    gpu_import: import_started.elapsed(),
                    surface_budget_wait,
                    ..SourceUploadTimings::default()
                },
                stalled,
            )?;
            Ok(NativeRepack {
                gpu_copy_bytes: retained_bytes,
                ..repack
            })
        }
        #[cfg(not(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        )))]
        {
            let _ = (metadata, surface, renderer, targets, timings);
            Err("a hardware frame cannot be read in place on this host".into())
        }
    }
}

/// A decoder surface imported onto the renderer's device.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode",
    feature = "hwencode"
))]
struct ImportedPlanes<'a> {
    y: &'a wgpu::TextureView,
    uv: &'a wgpu::TextureView,
    format: alpha2_media::YuvFormat,
    coded_size: [u32; 2],
    color: Option<alpha2_media::ColorMetadata>,
    lease: Arc<dyn ExternalTextureLease>,
}

/// Copies an imported surface's `visible` picture into the targets.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode",
    feature = "hwencode"
))]
fn repack_imported(
    renderer: &Renderer,
    planes: ImportedPlanes<'_>,
    visible: alpha2_media::Rect,
    targets: alpha2_render::Nv12PlaneTargets<'_>,
    timings: &mut alpha2_render::FrameSubmitTimings,
    import: SourceUploadTimings,
    surface_budget_stalled: bool,
) -> Result<NativeRepack, String> {
    let size =
        RenderSize::new(planes.coded_size[0], planes.coded_size[1]).map_err(error_message)?;
    renderer
        .repack_nv12_timed(
            alpha2_render::RepackSource::Gpu {
                planes: GpuVideoPlanes {
                    y: planes.y,
                    chroma: planes.uv,
                    v: None,
                    format: planes.format,
                    size,
                    alpha: 1.0,
                    color: planes.color,
                },
                lease: planes.lease,
            },
            visible,
            targets,
            timings,
        )
        .map_err(error_message)?;
    Ok(NativeRepack {
        hardware_import: true,
        surface_budget_stalled,
        timings: import,
        ..NativeRepack::default()
    })
}

/// Uploads a CPU picture's planes and copies them into the targets.
#[cfg(feature = "hwencode")]
fn repack_cpu(
    renderer: &Renderer,
    frame: &DecodedVideoFrame,
    targets: alpha2_render::Nv12PlaneTargets<'_>,
    timings: &mut alpha2_render::FrameSubmitTimings,
) -> Result<NativeRepack, String> {
    let PixelFormat::Yuv(format) = frame.pixel_format else {
        return Err(format!("a decoded picture is {:?}", frame.pixel_format));
    };
    let coded = frame
        .planes
        .first()
        .ok_or_else(|| "native decoder frame has no planes".to_owned())?;
    let size = RenderSize::new(coded.width, coded.height).map_err(error_message)?;
    let plane = |index: usize| {
        frame.planes.get(index).map_or(
            SourcePlane {
                bytes: &[],
                stride: 0,
            },
            |plane| SourcePlane {
                bytes: &plane.bytes,
                stride: plane.stride,
            },
        )
    };
    // The upload is written inside the repack's submission, so its time is in the submit spans.
    renderer
        .repack_nv12_timed(
            alpha2_render::RepackSource::Cpu(YuvFrame {
                format,
                planes: [plane(0), plane(1), plane(2)],
                alpha_plane: None,
                size,
                alpha: 1.0,
                color: frame.color,
            }),
            frame.visible_rect,
            targets,
            timings,
        )
        .map_err(error_message)?;
    Ok(NativeRepack {
        upload_bytes: frame
            .planes
            .iter()
            .map(|plane| u64::try_from(plane.bytes.len()).unwrap_or(u64::MAX))
            .sum(),
        ..NativeRepack::default()
    })
}

/// The colour a `VideoToolbox` surface's samples are in: the stream's, with the range its pixel
/// buffer's format says, which is what the planes hold. A buffer whose range differs from the
/// stream's tag is reported once per pair of format and tag.
#[cfg(all(target_os = "macos", feature = "hwdecode"))]
fn videotoolbox_color(
    tagged: Option<alpha2_media::ColorMetadata>,
    imported: &alpha2_native_gpu::ImportedMetalFrame,
) -> alpha2_media::ColorMetadata {
    use alpha2_media::ColorRange;
    static REPORTED: std::sync::Mutex<Vec<(String, bool)>> = std::sync::Mutex::new(Vec::new());
    let range = if imported.full_range {
        ColorRange::Full
    } else {
        ColorRange::Limited
    };
    let color = tagged.unwrap_or(alpha2_media::ColorMetadata::UNDECLARED);
    if color.range != range
        && let Ok(mut reported) = REPORTED.lock()
    {
        let key = (
            imported.pixel_format.clone(),
            color.range == ColorRange::Full,
        );
        if !reported.contains(&key) {
            alpha2_os::diagnostic!(
                "alpha2-host: VideoToolbox returned {} for a {}-range stream; its samples are read \
                 as {} range",
                imported.pixel_format,
                if key.1 { "full" } else { "limited" },
                if imported.full_range {
                    "full"
                } else {
                    "limited"
                }
            );
            reported.push(key);
        }
    }
    alpha2_media::ColorMetadata { range, ..color }
}

/// The bytes of `frame`'s planes, as a download of a decoder surface brought them to the CPU.
fn plane_bytes(frame: &alpha2_media::DecodedVideoFrame) -> u64 {
    frame
        .planes
        .iter()
        .map(|plane| plane.bytes.len() as u64)
        .sum()
}

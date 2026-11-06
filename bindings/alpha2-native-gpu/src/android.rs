//! Android decoder buffers (`AHardwareBuffer`) imported into wgpu's Vulkan device without a copy
//! through memory.
//!
//! wgpu has no `AHardwareBuffer` import, `VkSamplerYcbcrConversion` or immutable samplers, and
//! decoder buffers usually have driver-private (external) formats. So a buffer is imported in raw
//! Vulkan on wgpu's device and sampled through a conversion in the `RGB_IDENTITY` model (chroma
//! reconstruction only, no color math) into wgpu-owned R8/RG8 or R16/RG16 plane textures, which
//! the renderer's per-plane YUV path reads as it reads uploaded planes. Buffers with a concrete
//! format take the same pass, which costs one GPU copy of the planes and keeps a single path.

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::fmt;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::ThreadId;
use std::time::Duration;

use alpha2_gpu::{ExternalQueueLock, GpuContext};
use ash::vk;

use crate::NativePlaneLayout;

/// Compiled from `shaders/ahb_planes.vert` and `.frag` with
/// `glslc -O --target-env=vulkan1.1 <file> -o <file>.spv`.
const VERTEX_SHADER: &[u8] = include_bytes!("../shaders/ahb_planes.vert.spv");
const FRAGMENT_SHADER: &[u8] = include_bytes!("../shaders/ahb_planes.frag.spv");

/// `AHARDWAREBUFFER_FORMAT_YCbCr_P010`.
const AHB_FORMAT_P010: u32 = 0x36;

/// Conversions submitted and not yet seen complete before an import waits for the oldest. Each
/// holds a decoder image, so this bounds what the importer takes from a decoder's pool.
pub const MAX_CONVERSIONS_IN_FLIGHT: usize = 3;

/// Combined image sampler descriptors one set may consume: a YCbCr conversion may count as one
/// per plane.
const DESCRIPTORS_PER_SET: u32 = 4;

static WAIT_TIMEOUT_NS: AtomicU64 = AtomicU64::new(30_000_000_000);

pub(crate) fn set_wait_timeout(timeout: Duration) {
    WAIT_TIMEOUT_NS.store(
        u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
}

/// Why a device cannot import decoder buffers, or why one buffer failed to import.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AhbImportError(String);

impl fmt::Display for AhbImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for AhbImportError {}

fn failed(step: &str, result: vk::Result) -> AhbImportError {
    AhbImportError(format!("{step} failed: {result}"))
}

/// The lock wgpu's queue calls and this module's submissions share. Re-entrant, as
/// [`ExternalQueueLock`] requires.
#[derive(Debug, Default)]
struct QueueLock {
    holder: Mutex<(Option<ThreadId>, usize)>,
    released: Condvar,
}

impl QueueLock {
    fn acquire(&self) {
        let me = std::thread::current().id();
        let mut holder = self.holder.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            match holder.0 {
                None => {
                    *holder = (Some(me), 1);
                    return;
                }
                Some(owner) if owner == me => {
                    holder.1 += 1;
                    return;
                }
                Some(_) => {
                    holder = self
                        .released
                        .wait(holder)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
        }
    }

    fn release(&self) {
        let mut holder = self.holder.lock().unwrap_or_else(PoisonError::into_inner);
        holder.1 = holder.1.saturating_sub(1);
        if holder.1 == 0 {
            holder.0 = None;
            self.released.notify_one();
        }
    }
}

#[derive(Debug)]
struct SharedQueueLock(Arc<QueueLock>);

impl ExternalQueueLock for SharedQueueLock {
    fn lock(&self) {
        self.0.acquire();
    }

    fn unlock(&self) {
        self.0.release();
    }
}

fn format_supports(
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
    format: vk::Format,
    features: vk::FormatFeatureFlags,
) -> bool {
    // SAFETY: `physical` belongs to `instance`, which outlives the call.
    let properties = unsafe { instance.get_physical_device_format_properties(physical, format) };
    properties.optimal_tiling_features.contains(features)
}

/// Checks that the adapter can import, and says whether it can write 10-bit planes.
fn adapter_support(
    hal: &wgpu_hal::vulkan::Adapter,
    features: wgpu::Features,
) -> Result<bool, AhbImportError> {
    let capabilities = hal.physical_device_capabilities();
    let version = capabilities.properties().api_version;
    if version < vk::API_VERSION_1_1 {
        return Err(AhbImportError(format!(
            "the Vulkan device is version {}.{}, and the import needs 1.1",
            vk::api_version_major(version),
            vk::api_version_minor(version)
        )));
    }
    for extension in [
        ash::android::external_memory_android_hardware_buffer::NAME,
        ash::ext::queue_family_foreign::NAME,
    ] {
        if !capabilities.supports_extension(extension) {
            return Err(AhbImportError(format!(
                "the Vulkan device lacks {}",
                extension.to_string_lossy()
            )));
        }
    }
    let instance = hal.shared_instance().raw_instance();
    let physical = hal.raw_physical_device();
    let mut ycbcr = vk::PhysicalDeviceSamplerYcbcrConversionFeatures::default();
    let mut queried = vk::PhysicalDeviceFeatures2::default().push_next(&mut ycbcr);
    // SAFETY: `physical` belongs to `instance`, and the chained structure outlives the call.
    unsafe { instance.get_physical_device_features2(physical, &mut queried) };
    if ycbcr.sampler_ycbcr_conversion != vk::TRUE {
        return Err(AhbImportError(
            "the Vulkan device has no sampler YCbCr conversion".into(),
        ));
    }
    let target = vk::FormatFeatureFlags::COLOR_ATTACHMENT | vk::FormatFeatureFlags::SAMPLED_IMAGE;
    Ok(features.contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM)
        && format_supports(instance, physical, vk::Format::R16_UNORM, target)
        && format_supports(instance, physical, vk::Format::R16G16_UNORM, target))
}

/// Sets `samplerYcbcrConversion` in a feature structure already in `info`'s chain; `false` when
/// there is none. A second structure of that type would make the chain invalid.
///
/// # Safety
/// `info`'s chain must consist of live Vulkan structures this function may write.
unsafe fn enable_ycbcr_in_chain(info: &vk::DeviceCreateInfo<'_>) -> bool {
    let mut next = info.p_next.cast::<vk::BaseOutStructure<'_>>().cast_mut();
    // SAFETY: every link is a live structure starting with `sType` and `pNext`, per the caller.
    while let Some(structure) = unsafe { next.as_mut() } {
        if structure.s_type == vk::StructureType::PHYSICAL_DEVICE_SAMPLER_YCBCR_CONVERSION_FEATURES
        {
            let features = next.cast::<vk::PhysicalDeviceSamplerYcbcrConversionFeatures<'_>>();
            // SAFETY: the structure type says this is that structure.
            unsafe { (*features).sampler_ycbcr_conversion = vk::TRUE };
            return true;
        }
        next = structure.p_next;
    }
    false
}

/// Opens a device on `adapter` as wgpu-hal would for `features` and `limits`, plus the
/// extensions and feature the import needs, and whether it writes 10-bit planes.
fn open_device(
    adapter: &wgpu::Adapter,
    features: wgpu::Features,
    limits: &wgpu::Limits,
) -> Result<(wgpu_hal::OpenDevice<wgpu_hal::api::Vulkan>, bool), AhbImportError> {
    // SAFETY: the guard is only read, and dropped before the adapter is used again.
    let hal = unsafe { adapter.as_hal::<wgpu_hal::api::Vulkan>() }
        .ok_or_else(|| AhbImportError("the GPU device is not Vulkan".into()))?;
    let ten_bit = adapter_support(&hal, features)?;
    let mut extensions = hal.required_device_extensions(features);
    for extension in [
        ash::android::external_memory_android_hardware_buffer::NAME,
        ash::ext::queue_family_foreign::NAME,
    ] {
        if !extensions.contains(&extension) {
            extensions.push(extension);
        }
    }
    let mut device_features = hal.physical_device_features(&extensions, features);
    let names = extensions
        .iter()
        .map(|name| name.as_ptr())
        .collect::<Vec<_>>();
    // wgpu-hal opens queue family 0 with one queue, and `device_from_raw` is told the same.
    let priorities = [1.0];
    let queues = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(0)
        .queue_priorities(&priorities)];
    let mut ycbcr =
        vk::PhysicalDeviceSamplerYcbcrConversionFeatures::default().sampler_ycbcr_conversion(true);
    let mut info = device_features.add_to_device_create(
        vk::DeviceCreateInfo::default()
            .queue_create_infos(&queues)
            .enabled_extension_names(&names),
    );
    // SAFETY: the chain consists of `device_features`' structures, mutably borrowed by `info`.
    if !unsafe { enable_ycbcr_in_chain(&info) } {
        info = info.push_next(&mut ycbcr);
    }
    let instance = hal.shared_instance().raw_instance();
    // SAFETY: the extensions and features were checked against this physical device, which
    // belongs to `instance`.
    let raw = unsafe { instance.create_device(hal.raw_physical_device(), &info, None) }
        .map_err(|error| failed("vkCreateDevice", error))?;
    // SAFETY: `raw` was made from this adapter with these extensions, wgpu-hal's features and
    // queue family 0; wgpu-hal takes ownership of it.
    let open = unsafe {
        hal.device_from_raw(
            raw,
            None,
            &extensions,
            features,
            limits,
            &wgpu::MemoryHints::default(),
            0,
            0,
        )
    }
    .map_err(|error| AhbImportError(format!("wgpu-hal adoption: {error}")))?;
    Ok((open, ten_bit))
}

/// Reopens `gpu`'s device with what decoder buffer imports need, on the same adapter with the
/// same features and limits, and makes its importer.
///
/// The new device opens before `gpu` is let go. A device that is not Vulkan 1.1, lacks
/// `VK_ANDROID_external_memory_android_hardware_buffer` or `VK_EXT_queue_family_foreign`, or
/// has no sampler YCbCr conversion gives `gpu` back with the reason.
///
/// # Errors
/// `gpu` unchanged, and why it cannot import.
pub fn reopen_for_ahb_import(
    gpu: GpuContext,
) -> Result<(GpuContext, AhbImporter), (GpuContext, AhbImportError)> {
    let adapter = gpu.adapter().clone();
    let features = gpu.device().features();
    let limits = gpu.device().limits();
    let (open, ten_bit) = match open_device(&adapter, features, &limits) {
        Ok(opened) => opened,
        Err(error) => return Err((gpu, error)),
    };
    let descriptor = wgpu::DeviceDescriptor {
        required_features: features,
        required_limits: limits,
        ..wgpu::DeviceDescriptor::default()
    };
    // SAFETY: `open` was made from this adapter with exactly the descriptor's features.
    let (device, queue) = match unsafe { adapter.create_device_from_hal(open, &descriptor) } {
        Ok(created) => created,
        Err(error) => return Err((gpu, AhbImportError(format!("wgpu adoption: {error}")))),
    };
    drop(gpu);
    let lock = Arc::new(QueueLock::default());
    let shared: Arc<dyn ExternalQueueLock> = Arc::new(SharedQueueLock(Arc::clone(&lock)));
    let gpu = GpuContext::from_parts_with_queue_lock(adapter, device, queue, Some(shared));
    match AhbImporter::new(gpu.clone(), lock, ten_bit) {
        Ok(importer) => Ok((gpu, importer)),
        Err(error) => Err((gpu, error)),
    }
}

/// Raw handles of wgpu's device, valid while the importer holds its [`GpuContext`].
struct Raw {
    device: ash::Device,
    hardware_buffers: ash::android::external_memory_android_hardware_buffer::Device,
    queue: vk::Queue,
    family: u32,
    memory: vk::PhysicalDeviceMemoryProperties,
}

impl Raw {
    /// A memory type `bits` allows, with `wanted` properties when one has them.
    fn memory_type(&self, bits: u32, wanted: vk::MemoryPropertyFlags) -> Option<u32> {
        let candidates = || {
            (0_u32..)
                .zip(self.memory.memory_types_as_slice())
                .filter(|(index, _)| bits & (1 << index) != 0)
        };
        candidates()
            .find(|(_, kind)| kind.property_flags.contains(wanted))
            .or_else(|| candidates().next())
            .map(|(index, _)| index)
    }
}

/// The objects one buffer format is sampled with: its `VkSamplerYcbcrConversion`, the immutable
/// sampler using it, and the luma and chroma pipelines. Null handles are not yet made.
#[derive(Default)]
struct Kit {
    conversion: vk::SamplerYcbcrConversion,
    sampler: vk::Sampler,
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    pipelines: [vk::Pipeline; 2],
}

/// What a kit is cached by: the Vulkan format, the external format and whether planes are 10-bit.
type KitKey = (i32, u64, bool);

/// A reusable command buffer, its fence and the descriptor pool of its one set.
struct Slot {
    command: vk::CommandBuffer,
    fence: vk::Fence,
    descriptors: vk::DescriptorPool,
}

/// One buffer's import objects. Null handles are not yet made.
#[derive(Default)]
struct Frame {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    framebuffers: [vk::Framebuffer; 2],
    target_views: [vk::ImageView; 2],
    /// The plane textures, held so wgpu cannot free them while the conversion writes them.
    targets: Vec<wgpu::Texture>,
}

/// A submitted conversion and what must live until it completes.
struct InFlight {
    slot: Slot,
    frame: Frame,
    /// The decoder's image, returned once the conversion has read it.
    owner: Box<dyn Send>,
}

struct State {
    kits: HashMap<KitKey, Kit>,
    render_passes: HashMap<i32, vk::RenderPass>,
    vertex: vk::ShaderModule,
    fragment: vk::ShaderModule,
    commands: vk::CommandPool,
    idle: Vec<Slot>,
    in_flight: VecDeque<InFlight>,
}

/// Imports decoder buffers on the device [`reopen_for_ahb_import`] opened.
pub struct AhbImporter {
    state: Mutex<State>,
    raw: Raw,
    queue_lock: Arc<QueueLock>,
    ten_bit: bool,
    // Last, so the device outlives every object above.
    gpu: GpuContext,
}

impl fmt::Debug for AhbImporter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AhbImporter")
            .field("ten_bit", &self.ten_bit)
            .finish_non_exhaustive()
    }
}

/// The plane textures of one imported buffer, which the renderer keeps until it is done.
#[derive(Debug)]
pub struct AhbPlanes {
    y: wgpu::Texture,
    uv: wgpu::Texture,
    bytes: u64,
}

impl AhbPlanes {
    /// Bytes the plane textures hold.
    #[must_use]
    pub const fn retained_bytes(&self) -> u64 {
        self.bytes
    }

    /// The luma and chroma textures, which may also be copied from.
    #[must_use]
    pub const fn textures(&self) -> [&wgpu::Texture; 2] {
        [&self.y, &self.uv]
    }
}

/// A decoder buffer converted into plane textures.
#[derive(Debug)]
pub struct ImportedAhbFrame {
    /// Luma: `R8Unorm`, or `R16Unorm` holding P010 codes.
    pub y: wgpu::TextureView,
    /// Interleaved chroma at half size: `Rg8Unorm`, or `Rg16Unorm` holding P010 codes.
    pub uv: wgpu::TextureView,
    /// [`NativePlaneLayout::Nv12`] or [`NativePlaneLayout::P010`].
    pub layout: NativePlaneLayout,
    /// The buffer's whole extent, which the caller crops to the visible area.
    pub coded_size: [u32; 2],
    /// Keeps the plane textures.
    pub lease: Arc<AhbPlanes>,
}

/// The two planes of a buffer: Vulkan and wgpu formats, and extent.
struct PlaneTarget {
    format: vk::Format,
    texture_format: wgpu::TextureFormat,
    width: u32,
    height: u32,
}

fn plane_targets(width: u32, height: u32, ten_bit: bool) -> [PlaneTarget; 2] {
    let (luma, chroma) = if ten_bit {
        (
            (vk::Format::R16_UNORM, wgpu::TextureFormat::R16Unorm),
            (vk::Format::R16G16_UNORM, wgpu::TextureFormat::Rg16Unorm),
        )
    } else {
        (
            (vk::Format::R8_UNORM, wgpu::TextureFormat::R8Unorm),
            (vk::Format::R8G8_UNORM, wgpu::TextureFormat::Rg8Unorm),
        )
    };
    [
        PlaneTarget {
            format: luma.0,
            texture_format: luma.1,
            width,
            height,
        },
        PlaneTarget {
            format: chroma.0,
            texture_format: chroma.1,
            width: width.div_ceil(2),
            height: height.div_ceil(2),
        },
    ]
}

/// The viewport covering `target`; plane extents are far below where `f32` loses integers.
#[allow(clippy::cast_precision_loss)]
fn viewport(target: &PlaneTarget) -> vk::Viewport {
    vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: target.width as f32,
        height: target.height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }
}

fn spirv(bytes: &[u8]) -> Vec<u32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|word| u32::from_le_bytes(*word))
        .collect()
}

fn whole_image() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}

impl AhbImporter {
    fn new(
        gpu: GpuContext,
        queue_lock: Arc<QueueLock>,
        ten_bit: bool,
    ) -> Result<Self, AhbImportError> {
        let raw = {
            // SAFETY: the context was just made on a Vulkan device; the handles copied out stay
            // valid while `gpu`, which the importer keeps, keeps the device.
            let hal = unsafe { gpu.device().as_hal::<wgpu_hal::api::Vulkan>() }
                .ok_or_else(|| AhbImportError("the GPU device is not Vulkan".into()))?;
            let instance = hal.shared_instance().raw_instance();
            let device = hal.raw_device().clone();
            // SAFETY: the physical device belongs to `instance`.
            let memory = unsafe {
                instance.get_physical_device_memory_properties(hal.raw_physical_device())
            };
            Raw {
                hardware_buffers:
                    ash::android::external_memory_android_hardware_buffer::Device::new(
                        instance, &device,
                    ),
                device,
                queue: hal.raw_queue(),
                family: hal.queue_family_index(),
                memory,
            }
        };
        let mut state = State {
            kits: HashMap::new(),
            render_passes: HashMap::new(),
            vertex: vk::ShaderModule::null(),
            fragment: vk::ShaderModule::null(),
            commands: vk::CommandPool::null(),
            idle: Vec::new(),
            in_flight: VecDeque::new(),
        };
        let made = (|| {
            state.vertex = shader_module(&raw.device, VERTEX_SHADER)?;
            state.fragment = shader_module(&raw.device, FRAGMENT_SHADER)?;
            // SAFETY: the family is the one wgpu's queue belongs to.
            state.commands = unsafe {
                raw.device.create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                        .queue_family_index(raw.family),
                    None,
                )
            }
            .map_err(|error| failed("vkCreateCommandPool", error))?;
            Ok(())
        })();
        if let Err(error) = made {
            destroy_state(&raw.device, &mut state);
            return Err(error);
        }
        Ok(Self {
            state: Mutex::new(state),
            raw,
            queue_lock,
            ten_bit,
            gpu,
        })
    }

    /// Whether 10-bit buffers import, into R16/RG16 planes.
    #[must_use]
    pub const fn supports_ten_bit(&self) -> bool {
        self.ten_bit
    }

    /// The device the planes are made on.
    #[must_use]
    pub const fn gpu(&self) -> &GpuContext {
        &self.gpu
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Converts `buffer` into plane textures. `ten_bit` says the decoder was asked for 10 bits;
    /// a P010 buffer is 10-bit regardless.
    ///
    /// The conversion is submitted to wgpu's queue before this returns, so every later wgpu
    /// submission sees the planes. `owner` is dropped once the conversion has read `buffer`.
    ///
    /// # Safety
    /// `buffer` must be a live `AHardwareBuffer` that nothing writes until `owner` is dropped.
    ///
    /// # Errors
    /// A buffer Vulkan cannot sample, a 10-bit buffer on a device without 16-bit planes, a
    /// conversion that did not finish in time, or a failed Vulkan call.
    pub unsafe fn import(
        &self,
        buffer: NonNull<c_void>,
        ten_bit: bool,
        owner: Box<dyn Send>,
    ) -> Result<ImportedAhbFrame, AhbImportError> {
        let mut description = ndk_sys::AHardwareBuffer_Desc {
            width: 0,
            height: 0,
            layers: 0,
            format: 0,
            usage: 0,
            stride: 0,
            rfu0: 0,
            rfu1: 0,
        };
        // SAFETY: the caller keeps `buffer` alive; `description` outlives the call.
        unsafe { ndk_sys::AHardwareBuffer_describe(buffer.as_ptr().cast(), &raw mut description) };
        let (width, height) = (description.width, description.height);
        if width == 0 || height == 0 {
            return Err(AhbImportError("the decoder buffer is empty".into()));
        }
        let mut format = vk::AndroidHardwareBufferFormatPropertiesANDROID::default();
        let mut properties =
            vk::AndroidHardwareBufferPropertiesANDROID::default().push_next(&mut format);
        // SAFETY: the buffer is live, and the chained structure outlives the call.
        unsafe {
            self.raw
                .hardware_buffers
                .get_android_hardware_buffer_properties(buffer.as_ptr().cast(), &mut properties)
        }
        .map_err(|error| failed("vkGetAndroidHardwareBufferPropertiesANDROID", error))?;
        let memory = (properties.allocation_size, properties.memory_type_bits);
        if !format
            .format_features
            .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
        {
            return Err(AhbImportError(format!(
                "Vulkan cannot sample decoder buffers of format {:#x}",
                description.format
            )));
        }
        let ten_bit = ten_bit
            || description.format == AHB_FORMAT_P010
            || format.format == vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16;
        if ten_bit && !self.ten_bit {
            return Err(AhbImportError(
                "this GPU has no 16-bit planes for 10-bit decoder buffers".into(),
            ));
        }
        let targets = plane_targets(width, height, ten_bit);
        let mut state = self.lock();
        self.reclaim(&mut state, MAX_CONVERSIONS_IN_FLIGHT - 1)?;
        let key = (format.format.as_raw(), format.external_format, ten_bit);
        if !state.kits.contains_key(&key) {
            let mut kit = Kit::default();
            if let Err(error) = self.make_kit(&mut state, &mut kit, &format, &targets, ten_bit) {
                destroy_kit(&self.raw.device, &kit);
                return Err(error);
            }
            state.kits.insert(key, kit);
        }
        let slot = self.slot(&mut state)?;
        let mut frame = Frame::default();
        let made = self.make_frame(
            &mut state, &mut frame, &slot, key, buffer, &format, memory, &targets,
        );
        let views = match made {
            Ok(views) => views,
            Err(error) => {
                destroy_frame(&self.raw.device, &frame);
                self.recycle(&mut state, slot);
                return Err(error);
            }
        };
        let [y_texture, uv_texture] = [frame.targets[0].clone(), frame.targets[1].clone()];
        state.in_flight.push_back(InFlight { slot, frame, owner });
        let sample = if ten_bit { 2 } else { 1 };
        // Luma has one component per texel and chroma two.
        let bytes = targets
            .iter()
            .zip([1, 2])
            .map(|(plane, components)| {
                u64::from(plane.width) * u64::from(plane.height) * components * sample
            })
            .sum();
        let [y, uv] = views;
        Ok(ImportedAhbFrame {
            y,
            uv,
            layout: if ten_bit {
                NativePlaneLayout::P010
            } else {
                NativePlaneLayout::Nv12
            },
            coded_size: [width, height],
            lease: Arc::new(AhbPlanes {
                y: y_texture,
                uv: uv_texture,
                bytes,
            }),
        })
    }

    /// Frees the conversions that completed, first waiting for the oldest ones until at most
    /// `most` remain.
    fn reclaim(&self, state: &mut State, most: usize) -> Result<(), AhbImportError> {
        let device = &self.raw.device;
        while let Some(oldest) = state.in_flight.front() {
            let fence = oldest.slot.fence;
            // SAFETY: the fence belongs to this device.
            let done = unsafe { device.get_fence_status(fence) }
                .map_err(|error| failed("vkGetFenceStatus", error))?;
            if !done {
                if state.in_flight.len() <= most {
                    break;
                }
                // SAFETY: as above.
                unsafe {
                    device.wait_for_fences(&[fence], true, WAIT_TIMEOUT_NS.load(Ordering::Relaxed))
                }
                .map_err(|error| match error {
                    vk::Result::TIMEOUT => {
                        AhbImportError("a decoder buffer conversion did not finish in time".into())
                    }
                    error => failed("vkWaitForFences", error),
                })?;
            }
            if let Some(done) = state.in_flight.pop_front() {
                destroy_frame(device, &done.frame);
                self.recycle(state, done.slot);
                drop(done.owner);
            }
        }
        Ok(())
    }

    /// Puts a slot whose work is done or was never submitted back for reuse.
    fn recycle(&self, state: &mut State, slot: Slot) {
        let device = &self.raw.device;
        // SAFETY: nothing submitted uses the slot's fence or set any more.
        let reset = unsafe {
            device.reset_fences(&[slot.fence]).and_then(|()| {
                device
                    .reset_descriptor_pool(slot.descriptors, vk::DescriptorPoolResetFlags::empty())
            })
        };
        if reset.is_ok() {
            state.idle.push(slot);
        } else {
            destroy_slot(device, state.commands, &slot);
        }
    }

    fn slot(&self, state: &mut State) -> Result<Slot, AhbImportError> {
        if let Some(slot) = state.idle.pop() {
            return Ok(slot);
        }
        let device = &self.raw.device;
        let mut slot = Slot {
            command: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
            descriptors: vk::DescriptorPool::null(),
        };
        let made = (|| {
            // SAFETY: the pool belongs to this device and is only used under the state lock.
            slot.command = unsafe {
                device.allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(state.commands)
                        .command_buffer_count(1),
                )
            }
            .map_err(|error| failed("vkAllocateCommandBuffers", error))?[0];
            // SAFETY: plain object creation on this device.
            slot.fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }
                .map_err(|error| failed("vkCreateFence", error))?;
            let sizes = [vk::DescriptorPoolSize {
                ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: DESCRIPTORS_PER_SET,
            }];
            // SAFETY: as above.
            slot.descriptors = unsafe {
                device.create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(1)
                        .pool_sizes(&sizes),
                    None,
                )
            }
            .map_err(|error| failed("vkCreateDescriptorPool", error))?;
            Ok(())
        })();
        match made {
            Ok(()) => Ok(slot),
            Err(error) => {
                destroy_slot(device, state.commands, &slot);
                Err(error)
            }
        }
    }

    fn render_pass(
        &self,
        state: &mut State,
        format: vk::Format,
    ) -> Result<vk::RenderPass, AhbImportError> {
        if let Some(pass) = state.render_passes.get(&format.as_raw()) {
            return Ok(*pass);
        }
        let attachments = [vk::AttachmentDescription::default()
            .format(format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::DONT_CARE)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            // wgpu adopts the planes as shader resources, in this layout.
            .final_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let color = [vk::AttachmentReference::default()
            .attachment(0)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
        let subpasses = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&color)];
        // The writes must be visible to wgpu's later submissions, which sample the planes with no
        // barrier of their own: the planes are already in the state wgpu tracks.
        let dependencies = [vk::SubpassDependency::default()
            .src_subpass(0)
            .dst_subpass(vk::SUBPASS_EXTERNAL)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags::ALL_COMMANDS)
            .dst_access_mask(vk::AccessFlags::MEMORY_READ)];
        // SAFETY: plain object creation on this device.
        let pass = unsafe {
            self.raw.device.create_render_pass(
                &vk::RenderPassCreateInfo::default()
                    .attachments(&attachments)
                    .subpasses(&subpasses)
                    .dependencies(&dependencies),
                None,
            )
        }
        .map_err(|error| failed("vkCreateRenderPass", error))?;
        state.render_passes.insert(format.as_raw(), pass);
        Ok(pass)
    }

    /// Makes the conversion, sampler and pipelines for buffers of `format`.
    fn make_kit(
        &self,
        state: &mut State,
        kit: &mut Kit,
        format: &vk::AndroidHardwareBufferFormatPropertiesANDROID<'_>,
        targets: &[PlaneTarget; 2],
        ten_bit: bool,
    ) -> Result<(), AhbImportError> {
        let device = &self.raw.device;
        let external = format.format == vk::Format::UNDEFINED;
        let mut external_format = vk::ExternalFormatANDROID::default()
            .external_format(if external { format.external_format } else { 0 });
        // RGB_IDENTITY with full range returns the code values as stored, and nearest chroma
        // sampled at chroma texel centers returns each chroma sample unchanged.
        let conversion = vk::SamplerYcbcrConversionCreateInfo::default()
            .format(format.format)
            .ycbcr_model(vk::SamplerYcbcrModelConversion::RGB_IDENTITY)
            .ycbcr_range(vk::SamplerYcbcrRange::ITU_FULL)
            .components(format.sampler_ycbcr_conversion_components)
            .x_chroma_offset(format.suggested_x_chroma_offset)
            .y_chroma_offset(format.suggested_y_chroma_offset)
            .chroma_filter(vk::Filter::NEAREST)
            .push_next(&mut external_format);
        // SAFETY: the device enabled sampler YCbCr conversion, and the format comes from it.
        kit.conversion = unsafe { device.create_sampler_ycbcr_conversion(&conversion, None) }
            .map_err(|error| failed("vkCreateSamplerYcbcrConversion", error))?;
        let mut sampled = vk::SamplerYcbcrConversionInfo::default().conversion(kit.conversion);
        // SAFETY: a YCbCr sampler with the address modes and filters the conversion allows.
        kit.sampler = unsafe {
            device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::NEAREST)
                    .min_filter(vk::Filter::NEAREST)
                    .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .push_next(&mut sampled),
                None,
            )
        }
        .map_err(|error| failed("vkCreateSampler", error))?;
        let samplers = [kit.sampler];
        let bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .immutable_samplers(&samplers)];
        // SAFETY: plain object creation; a YCbCr sampler must be immutable, as it is here.
        kit.set_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        }
        .map_err(|error| failed("vkCreateDescriptorSetLayout", error))?;
        let set_layouts = [kit.set_layout];
        // SAFETY: plain object creation on this device.
        kit.layout = unsafe {
            device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts),
                None,
            )
        }
        .map_err(|error| failed("vkCreatePipelineLayout", error))?;
        for (plane, target) in targets.iter().enumerate() {
            let pass = self.render_pass(state, target.format)?;
            kit.pipelines[plane] =
                self.make_pipeline(state, kit.layout, pass, plane == 1, ten_bit)?;
        }
        Ok(())
    }

    fn make_pipeline(
        &self,
        state: &State,
        layout: vk::PipelineLayout,
        pass: vk::RenderPass,
        chroma: bool,
        ten_bit: bool,
    ) -> Result<vk::Pipeline, AhbImportError> {
        let constants = [u32::from(chroma), u32::from(ten_bit)];
        let constant_bytes = constants
            .iter()
            .flat_map(|value| value.to_ne_bytes())
            .collect::<Vec<_>>();
        let entries = [
            vk::SpecializationMapEntry::default()
                .constant_id(0)
                .offset(0)
                .size(4),
            vk::SpecializationMapEntry::default()
                .constant_id(1)
                .offset(4)
                .size(4),
        ];
        let specialization = vk::SpecializationInfo::default()
            .map_entries(&entries)
            .data(&constant_bytes);
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(state.vertex)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(state.fragment)
                .name(c"main")
                .specialization_info(&specialization),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let info = [vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&assembly)
            .viewport_state(&viewport)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .color_blend_state(&blend)
            .dynamic_state(&dynamic)
            .layout(layout)
            .render_pass(pass)];
        // SAFETY: every referenced object belongs to this device and outlives the call.
        let pipelines = unsafe {
            self.raw
                .device
                .create_graphics_pipelines(vk::PipelineCache::null(), &info, None)
        }
        .map_err(|(_, error)| failed("vkCreateGraphicsPipelines", error))?;
        Ok(pipelines[0])
    }

    /// Makes a plane texture wgpu owns, and the view the conversion renders into.
    #[allow(clippy::too_many_lines)]
    fn make_target(
        &self,
        frame: &mut Frame,
        target: &PlaneTarget,
    ) -> Result<vk::ImageView, AhbImportError> {
        let device = &self.raw.device;
        let extent = vk::Extent3D {
            width: target.width,
            height: target.height,
            depth: 1,
        };
        // SAFETY: plain object creation on this device.
        let image = unsafe {
            device.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(target.format)
                    .extent(extent)
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(
                        vk::ImageUsageFlags::COLOR_ATTACHMENT
                            | vk::ImageUsageFlags::SAMPLED
                            | vk::ImageUsageFlags::TRANSFER_SRC,
                    )
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
        }
        .map_err(|error| failed("vkCreateImage", error))?;
        // SAFETY: the image was just made on this device.
        let requirements = unsafe { device.get_image_memory_requirements(image) };
        let memory = self
            .raw
            .memory_type(
                requirements.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )
            .ok_or_else(|| AhbImportError("no memory type holds a plane".into()))
            .and_then(|index| {
                // SAFETY: a plain allocation of a type the image allows.
                unsafe {
                    device.allocate_memory(
                        &vk::MemoryAllocateInfo::default()
                            .allocation_size(requirements.size)
                            .memory_type_index(index),
                        None,
                    )
                }
                .map_err(|error| failed("vkAllocateMemory", error))
            });
        let memory = match memory {
            Ok(memory) => memory,
            Err(error) => {
                // SAFETY: the image is unused.
                unsafe { device.destroy_image(image, None) };
                return Err(error);
            }
        };
        // SAFETY: the memory was allocated for this image's requirements.
        if let Err(error) = unsafe { device.bind_image_memory(image, memory, 0) } {
            // SAFETY: both are unused.
            unsafe {
                device.destroy_image(image, None);
                device.free_memory(memory, None);
            }
            return Err(failed("vkBindImageMemory", error));
        }
        let size = wgpu::Extent3d {
            width: target.width,
            height: target.height,
            depth_or_array_layers: 1,
        };
        let texture = {
            // SAFETY: the context is Vulkan, checked when the importer was made.
            let hal = unsafe { self.gpu.device().as_hal::<wgpu_hal::api::Vulkan>() }
                .ok_or_else(|| AhbImportError("the GPU device is not Vulkan".into()))?;
            // SAFETY: the image and its dedicated memory were made on this device with the
            // descriptor's format and size; wgpu-hal takes ownership of both.
            unsafe {
                hal.texture_from_raw(
                    image,
                    &wgpu_hal::TextureDescriptor {
                        label: Some("alpha2 decoder plane"),
                        size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: target.texture_format,
                        usage: wgpu::TextureUses::RESOURCE,
                        memory_flags: wgpu_hal::MemoryFlags::empty(),
                        view_formats: Vec::new(),
                    },
                    None,
                    wgpu_hal::vulkan::TextureMemory::Dedicated(memory),
                )
            }
        };
        // SAFETY: the conversion leaves the image in the shader-read layout before any wgpu
        // submission can use it, and the texture is this device's.
        let texture = unsafe {
            self.gpu
                .device()
                .create_texture_from_hal::<wgpu_hal::api::Vulkan>(
                    texture,
                    &wgpu::TextureDescriptor {
                        label: Some("alpha2 decoder plane"),
                        size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: target.texture_format,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
                        view_formats: &[],
                    },
                    wgpu::TextureUses::RESOURCE,
                )
        };
        frame.targets.push(texture);
        // SAFETY: the image lives while `frame` holds its texture.
        unsafe {
            device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(target.format)
                    .subresource_range(whole_image()),
                None,
            )
        }
        .map_err(|error| failed("vkCreateImageView", error))
    }

    /// Imports `buffer`, makes its planes, and records and submits the conversion.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn make_frame(
        &self,
        state: &mut State,
        frame: &mut Frame,
        slot: &Slot,
        key: KitKey,
        buffer: NonNull<c_void>,
        format: &vk::AndroidHardwareBufferFormatPropertiesANDROID<'_>,
        (allocation_size, memory_bits): (vk::DeviceSize, u32),
        targets: &[PlaneTarget; 2],
    ) -> Result<[wgpu::TextureView; 2], AhbImportError> {
        let device = &self.raw.device;
        let external = format.format == vk::Format::UNDEFINED;
        let (width, height) = (targets[0].width, targets[0].height);
        let mut external_memory = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::ANDROID_HARDWARE_BUFFER_ANDROID);
        let mut external_format =
            vk::ExternalFormatANDROID::default().external_format(format.external_format);
        let mut image = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format.format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external_memory);
        if external {
            image = image.push_next(&mut external_format);
        }
        // SAFETY: an image of the buffer's extent and reported format, for AHB memory.
        frame.image = unsafe { device.create_image(&image, None) }
            .map_err(|error| failed("vkCreateImage", error))?;
        let memory_type = self
            .raw
            .memory_type(memory_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
            .ok_or_else(|| AhbImportError("no memory type imports the buffer".into()))?;
        let mut import =
            vk::ImportAndroidHardwareBufferInfoANDROID::default().buffer(buffer.as_ptr());
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(frame.image);
        // SAFETY: the buffer is live per the caller; importing takes Vulkan's own reference to
        // it, which freeing the memory drops.
        frame.memory = unsafe {
            device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(allocation_size)
                    .memory_type_index(memory_type)
                    .push_next(&mut import)
                    .push_next(&mut dedicated),
                None,
            )
        }
        .map_err(|error| failed("vkAllocateMemory (AHardwareBuffer import)", error))?;
        // SAFETY: the memory is the image's dedicated import.
        unsafe { device.bind_image_memory(frame.image, frame.memory, 0) }
            .map_err(|error| failed("vkBindImageMemory", error))?;
        let kit = state
            .kits
            .get(&key)
            .ok_or_else(|| AhbImportError("the buffer format has no conversion".into()))?;
        let mut sampled = vk::SamplerYcbcrConversionInfo::default().conversion(kit.conversion);
        // SAFETY: a view of the whole image through its format's conversion, as YCbCr sampling
        // requires.
        frame.view = unsafe {
            device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(frame.image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format.format)
                    .subresource_range(whole_image())
                    .push_next(&mut sampled),
                None,
            )
        }
        .map_err(|error| failed("vkCreateImageView", error))?;
        let (set_layout, pipeline_layout, pipelines) = (kit.set_layout, kit.layout, kit.pipelines);
        let layouts = [set_layout];
        // SAFETY: the slot's pool was reset and holds one set of this size.
        let set = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(slot.descriptors)
                    .set_layouts(&layouts),
            )
        }
        .map_err(|error| failed("vkAllocateDescriptorSets", error))?[0];
        let picture = [vk::DescriptorImageInfo::default()
            .image_view(frame.view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        // SAFETY: the set and view are live; the sampler is the layout's immutable one.
        unsafe {
            device.update_descriptor_sets(
                &[vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&picture)],
                &[],
            );
        }
        let mut passes = [vk::RenderPass::null(); 2];
        for (plane, target) in targets.iter().enumerate() {
            frame.target_views[plane] = self.make_target(frame, target)?;
            passes[plane] = self.render_pass(state, target.format)?;
            let attachments = [frame.target_views[plane]];
            // SAFETY: the view's format matches the pass, and it lives as long as the frame.
            frame.framebuffers[plane] = unsafe {
                device.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(passes[plane])
                        .attachments(&attachments)
                        .width(target.width)
                        .height(target.height)
                        .layers(1),
                    None,
                )
            }
            .map_err(|error| failed("vkCreateFramebuffer", error))?;
        }
        let command = slot.command;
        // SAFETY: the slot's command buffer is idle; its pool allows resetting on begin.
        unsafe {
            device.begin_command_buffer(
                command,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
        }
        .map_err(|error| failed("vkBeginCommandBuffer", error))?;
        // The decoder wrote the buffer outside Vulkan. Acquiring it from the foreign queue family
        // makes those writes visible.
        let acquire = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .dst_queue_family_index(self.raw.family)
            .image(frame.image)
            .subresource_range(whole_image());
        // SAFETY: recording into the command buffer begun above; every object is live.
        unsafe {
            device.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[acquire],
            );
            for (plane, target) in targets.iter().enumerate() {
                let area = vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: target.width,
                        height: target.height,
                    },
                };
                device.cmd_begin_render_pass(
                    command,
                    &vk::RenderPassBeginInfo::default()
                        .render_pass(passes[plane])
                        .framebuffer(frame.framebuffers[plane])
                        .render_area(area),
                    vk::SubpassContents::INLINE,
                );
                device.cmd_set_viewport(command, 0, &[viewport(target)]);
                device.cmd_set_scissor(command, 0, &[area]);
                device.cmd_bind_pipeline(
                    command,
                    vk::PipelineBindPoint::GRAPHICS,
                    pipelines[plane],
                );
                device.cmd_bind_descriptor_sets(
                    command,
                    vk::PipelineBindPoint::GRAPHICS,
                    pipeline_layout,
                    0,
                    &[set],
                    &[],
                );
                device.cmd_draw(command, 3, 1, 0, 0);
                device.cmd_end_render_pass(command);
            }
            // Ownership goes back to the decoder with no layout change, which could write the
            // buffer.
            let release = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::empty())
                .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(self.raw.family)
                .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .image(frame.image)
                .subresource_range(whole_image());
            device.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[release],
            );
            device.end_command_buffer(command)
        }
        .map_err(|error| failed("vkEndCommandBuffer", error))?;
        let commands = [command];
        let submit = vk::SubmitInfo::default().command_buffers(&commands);
        self.queue_lock.acquire();
        // SAFETY: the queue is wgpu's, held under the lock every wgpu queue call takes.
        let submitted = unsafe { device.queue_submit(self.raw.queue, &[submit], slot.fence) };
        self.queue_lock.release();
        submitted.map_err(|error| failed("vkQueueSubmit", error))?;
        Ok([
            frame.targets[0].create_view(&wgpu::TextureViewDescriptor::default()),
            frame.targets[1].create_view(&wgpu::TextureViewDescriptor::default()),
        ])
    }
}

impl Drop for AhbImporter {
    fn drop(&mut self) {
        let device = self.raw.device.clone();
        let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        while let Some(oldest) = state.in_flight.pop_front() {
            // SAFETY: the fence belongs to this device.
            let waited = unsafe {
                device.wait_for_fences(
                    &[oldest.slot.fence],
                    true,
                    WAIT_TIMEOUT_NS.load(Ordering::Relaxed),
                )
            };
            if waited.is_err() {
                // The GPU may still read these objects and the decoder images, so they leak.
                std::mem::forget(oldest);
                for leaked in state.in_flight.drain(..) {
                    std::mem::forget(leaked);
                }
                return;
            }
            destroy_frame(&device, &oldest.frame);
            destroy_slot(&device, state.commands, &oldest.slot);
        }
        destroy_state(&device, state);
    }
}

fn shader_module(device: &ash::Device, bytes: &[u8]) -> Result<vk::ShaderModule, AhbImportError> {
    let code = spirv(bytes);
    // SAFETY: the code is SPIR-V validated for Vulkan 1.1 when it was compiled.
    unsafe { device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None) }
        .map_err(|error| failed("vkCreateShaderModule", error))
}

// The destroy functions below accept null handles, which Vulkan ignores.

fn destroy_kit(device: &ash::Device, kit: &Kit) {
    // SAFETY: no submitted work uses the kit any more.
    unsafe {
        for pipeline in kit.pipelines {
            device.destroy_pipeline(pipeline, None);
        }
        device.destroy_pipeline_layout(kit.layout, None);
        device.destroy_descriptor_set_layout(kit.set_layout, None);
        device.destroy_sampler(kit.sampler, None);
        device.destroy_sampler_ycbcr_conversion(kit.conversion, None);
    }
}

fn destroy_frame(device: &ash::Device, frame: &Frame) {
    // SAFETY: no submitted work uses the frame any more; its plane textures belong to wgpu and
    // drop with it.
    unsafe {
        for framebuffer in frame.framebuffers {
            device.destroy_framebuffer(framebuffer, None);
        }
        for view in frame.target_views {
            device.destroy_image_view(view, None);
        }
        device.destroy_image_view(frame.view, None);
        device.destroy_image(frame.image, None);
        device.free_memory(frame.memory, None);
    }
}

fn destroy_slot(device: &ash::Device, commands: vk::CommandPool, slot: &Slot) {
    // SAFETY: no submitted work uses the slot any more.
    unsafe {
        if slot.command != vk::CommandBuffer::null() {
            device.free_command_buffers(commands, &[slot.command]);
        }
        device.destroy_fence(slot.fence, None);
        device.destroy_descriptor_pool(slot.descriptors, None);
    }
}

fn destroy_state(device: &ash::Device, state: &mut State) {
    for slot in state.idle.drain(..) {
        destroy_slot(device, state.commands, &slot);
    }
    for (_, kit) in state.kits.drain() {
        destroy_kit(device, &kit);
    }
    // SAFETY: no submitted work uses these any more.
    unsafe {
        for (_, pass) in state.render_passes.drain() {
            device.destroy_render_pass(pass, None);
        }
        device.destroy_shader_module(state.vertex, None);
        device.destroy_shader_module(state.fragment, None);
        device.destroy_command_pool(state.commands, None);
    }
}

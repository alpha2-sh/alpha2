//! A queue the renderer shares with a native decoder is only touched under
//! the decoder's lock, including by the native submission `wgpu` makes when a
//! frame's sources complete.
//!
//! The witness is a timeline signal staged on the native queue: the first
//! native submission takes it, so the lock can tell whether that happened
//! while it was held. Any Vulkan device will do; this crate is where a test
//! may reach the native queue.
#![cfg(all(target_os = "linux", feature = "hwdevice"))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use alpha2_gpu::{ExternalQueueLock, GpuContext};
use alpha2_media::ColorMetadata;
use alpha2_render::{
    ColorPolicy, ExternalTextureLease, GpuVideoPlanes, RenderSize, Renderer, SourcePresentation,
};
use ash::vk;

/// One sampled plane texture holding `bytes`.
fn plane(
    gpu: &GpuContext,
    format: wgpu::TextureFormat,
    width: u32,
    bytes: &[u8],
    row: u32,
) -> wgpu::Texture {
    let texture = gpu.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("queue lock test plane"),
        size: wgpu::Extent3d {
            width,
            height: width,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    gpu.queue().write_texture(
        texture.as_image_copy(),
        bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(row),
            rows_per_image: Some(width),
        },
        texture.size(),
    );
    texture
}

/// A signal staged on the native queue, which the next native submission
/// takes.
#[derive(Debug)]
struct Witness {
    queue: wgpu::Queue,
    semaphore: vk::Semaphore,
    /// Above every value the semaphore was signalled to before.
    value: u64,
}

impl Witness {
    fn hal(&self) -> impl std::ops::Deref<Target = wgpu::hal::vulkan::Queue> + '_ {
        // SAFETY: the test device is Vulkan; the queue is only asked about its staged signals.
        unsafe { self.queue.as_hal::<wgpu::hal::api::Vulkan>() }.expect("a Vulkan queue")
    }

    fn stage(&self) {
        self.hal()
            .add_signal_semaphore(self.semaphore, Some(self.value));
    }

    /// Whether the signal is still staged, leaving it so.
    fn staged(&self) -> bool {
        let queue = self.hal();
        let staged = queue.remove_signal_semaphore(self.semaphore);
        if staged {
            queue.add_signal_semaphore(self.semaphore, Some(self.value));
        }
        staged
    }
}

#[derive(Debug, Default)]
struct Watch {
    depth: usize,
    /// Witnesses staged so far, each signalling one more than the last.
    staged: u64,
    witness: Option<Witness>,
    taken_under_lock: bool,
    taken_without_lock: bool,
}

impl Watch {
    /// Notes the witness taken, if it was, and stops watching it.
    fn settle(&mut self, under_lock: bool) {
        if self
            .witness
            .as_ref()
            .is_some_and(|witness| !witness.staged())
        {
            self.witness = None;
            if under_lock {
                self.taken_under_lock = true;
            } else {
                self.taken_without_lock = true;
            }
        }
    }
}

/// A re-entrant stand-in for the decoder's queue lock that watches when the
/// witness signal leaves the queue.
#[derive(Debug, Default)]
struct WatchingLock(Mutex<Watch>);

impl WatchingLock {
    fn watch(&self) -> std::sync::MutexGuard<'_, Watch> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl WatchingLock {
    /// Stages a witness on `queue`'s native queue and watches it.
    fn stage(&self, queue: &wgpu::Queue, semaphore: vk::Semaphore) {
        let mut watch = self.watch();
        watch.staged += 1;
        let witness = Witness {
            queue: queue.clone(),
            semaphore,
            value: watch.staged,
        };
        witness.stage();
        watch.witness = Some(witness);
    }
}

impl ExternalQueueLock for WatchingLock {
    fn lock(&self) {
        let mut watch = self.watch();
        if watch.depth == 0 {
            // Gone before anyone took the lock: a submission went to the queue without it.
            watch.settle(false);
        }
        watch.depth += 1;
    }

    fn unlock(&self) {
        let mut watch = self.watch();
        watch.depth -= 1;
        if watch.depth == 0 {
            watch.settle(true);
        }
    }
}

/// A decoder surface whose frame, once submitted, has a write land on the
/// queue before its completion is asked for, as another thread's upload can,
/// and stages the witness behind it.
#[derive(Debug)]
struct LateWriteLease {
    lock: Arc<WatchingLock>,
    queue: wgpu::Queue,
    buffer: wgpu::Buffer,
    semaphore: vk::Semaphore,
    submitted: AtomicBool,
}

impl ExternalTextureLease for LateWriteLease {
    fn retained_bytes(&self) -> u64 {
        6
    }

    fn submitted(&self) {
        self.submitted.store(true, Ordering::Release);
        self.queue.write_buffer(&self.buffer, 0, &[1; 4]);
        self.lock.stage(&self.queue, self.semaphore);
    }
}

/// A Vulkan device whose queue is shared under a [`WatchingLock`], a raw
/// queue handle for staging the witness, and the witness's semaphore.
struct Watched {
    gpu: GpuContext,
    queue: wgpu::Queue,
    lock: Arc<WatchingLock>,
    raw: ash::Device,
    semaphore: vk::Semaphore,
}

impl Watched {
    fn new() -> Self {
        let turn = alpha2_gpu::serialize_acquisition();
        let instance = alpha2_gpu::new_instance(wgpu::Backends::VULKAN);
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .expect("a Vulkan adapter");
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .expect("a device");
        drop(turn);
        let lock = Arc::new(WatchingLock::default());
        let shared: Arc<dyn ExternalQueueLock> = lock.clone();
        let gpu =
            GpuContext::from_parts_with_queue_lock(adapter, device, queue.clone(), Some(shared));
        // SAFETY: the device is Vulkan and outlives the semaphore, which `finish` destroys once the
        // device is idle.
        let raw = unsafe { gpu.device().as_hal::<wgpu::hal::api::Vulkan>() }
            .expect("a Vulkan device")
            .raw_device()
            .clone();
        let mut timeline = vk::SemaphoreTypeCreateInfo::default()
            .semaphore_type(vk::SemaphoreType::TIMELINE)
            .initial_value(0);
        // SAFETY: a plain timeline semaphore of this device.
        let semaphore = unsafe {
            raw.create_semaphore(
                &vk::SemaphoreCreateInfo::default().push_next(&mut timeline),
                None,
            )
        }
        .expect("a timeline semaphore");
        Self {
            gpu,
            queue,
            lock,
            raw,
            semaphore,
        }
    }

    /// Stages the witness behind whatever the queue holds now.
    fn stage(&self) {
        self.lock.stage(&self.queue, self.semaphore);
    }

    /// Whether the witness left the queue under the lock, and whether
    /// without it; each stage resets the answer.
    fn verdict(&self) -> (bool, bool) {
        let mut watch = self.lock.watch();
        watch.settle(false);
        let verdict = (watch.taken_under_lock, watch.taken_without_lock);
        watch.taken_under_lock = false;
        watch.taken_without_lock = false;
        verdict
    }

    fn finish(self) {
        self.gpu
            .device()
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("the device drains");
        // SAFETY: the device is idle, so nothing waits on or signals the semaphore.
        unsafe { self.raw.destroy_semaphore(self.semaphore, None) };
    }
}

#[test]
fn completing_a_frame_from_a_decoder_surface_touches_the_queue_only_under_its_lock() {
    let watched = Watched::new();
    let Watched {
        gpu,
        queue,
        lock,
        semaphore,
        ..
    } = &watched;
    let (gpu, lock, semaphore) = (gpu.clone(), Arc::clone(lock), *semaphore);
    let queue = queue.clone();
    let renderer = Renderer::new(gpu.clone());
    let y = plane(&gpu, wgpu::TextureFormat::R8Unorm, 2, &[16; 4], 2);
    let uv = plane(&gpu, wgpu::TextureFormat::Rg8Unorm, 1, &[128, 128], 2);
    let y_view = y.create_view(&wgpu::TextureViewDescriptor::default());
    let uv_view = uv.create_view(&wgpu::TextureViewDescriptor::default());
    let lease = Arc::new(LateWriteLease {
        lock: Arc::clone(&lock),
        queue,
        buffer: gpu.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("late write"),
            size: 4,
            usage: wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }),
        semaphore,
        submitted: AtomicBool::new(false),
    });
    let size = RenderSize::new(2, 2).expect("size");
    let (prepared, _) = renderer
        .prepare_gpu_yuv_presented(
            GpuVideoPlanes {
                y: &y_view,
                chroma: &uv_view,
                v: None,
                format: alpha2_media::YuvFormat::NV12,
                size,
                alpha: 1.0,
                color: Some(ColorMetadata::default()),
            },
            SourcePresentation::identity(size),
            ColorPolicy::RejectHdr,
            lease.clone(),
        )
        .expect("NV12 source prepares");
    let (_texture, pending) = prepared.into_parts();
    let pending = pending.expect("conversion remains pending");
    assert!(renderer.submit_pending_source(&pending));
    assert!(lease.submitted.load(Ordering::Acquire));
    assert_eq!(
        watched.verdict(),
        (true, false),
        "the write pending after the frame went to the shared queue without its lock"
    );
    drop((pending, renderer, lease));
    watched.finish();
}

#[test]
fn every_call_of_the_context_that_can_reach_the_queue_holds_its_lock() {
    let watched = Watched::new();
    let gpu = &watched.gpu;
    let buffer = gpu.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some("pending write"),
        size: 4,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    // Waiting for submitted work flushes writes still pending.
    gpu.queue().write_buffer(&buffer, 0, &[1; 4]);
    watched.stage();
    gpu.on_submitted_work_done(|| {});
    assert_eq!(watched.verdict(), (true, false), "on_submitted_work_done");

    // Mapping a buffer flushes the writes pending to it.
    gpu.queue().write_buffer(&buffer, 0, &[2; 4]);
    watched.stage();
    gpu.map_async(&buffer.slice(..), wgpu::MapMode::Read, |result| {
        result.expect("maps");
    });
    assert_eq!(watched.verdict(), (true, false), "map_async");
    gpu.device()
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("the device drains");
    buffer.unmap();

    watched.stage();
    gpu.submit([]);
    assert_eq!(watched.verdict(), (true, false), "submit");
    watched.finish();
}

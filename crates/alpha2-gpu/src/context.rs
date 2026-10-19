use std::sync::Arc;
use std::time::Duration;

use web_time::Instant;

use crate::diagnostic::DiagnosticHook;
use crate::{
    Capabilities, CapabilityTier, GpuDiagnostic, GpuDiagnosticHandler, GpuError, IntermediateFormat,
};

/// A GPU device and queue owned by one render host.
///
/// Cloning is cheap and shares one device: `wgpu`'s adapter, device, and queue
/// are handles, and a clone also observes the diagnostic handler installed
/// through [`GpuContext::set_diagnostic_handler`]. Two clones are one GPU
/// device, not two, so a caller that wants an independent device requests one.
///
/// # The queue (`wgpu` interop)
///
/// The queue is not a field. Every `wgpu` call that can reach the native
/// queue is a method here, and takes the shared-queue lock when the context
/// has one: `submit`, [`Self::on_submitted_work_done`] and `map_async` (both
/// flush writes still pending from a submission `wgpu` rejected), and
/// `present`. Dropping the last clone drops the queue under the lock too,
/// because `wgpu` waits for the native queue to go idle there. `queue` hands
/// out the handle for everything else (writes, timestamps, backend interop),
/// and the clippy configuration of every crate that uses `wgpu` refuses the
/// queue-reaching calls on it. Those methods, and the ones that adopt or hand
/// out the device and adapter, are the `wgpu-interop` feature's.
#[derive(Clone, Debug)]
pub struct GpuContext {
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: Arc<LockedQueue>,
    // Shared, so that a clone (one per readback) copies no strings.
    capabilities: Arc<Capabilities>,
    diagnostics: Arc<DiagnosticHook>,
}

/// The queue and the lock every call that reaches it takes.
#[derive(Debug)]
struct LockedQueue {
    // Declared before `lock`, so that it drops while `Drop` holds the lock.
    queue: wgpu::Queue,
    lock: QueueLockSlot,
}

/// The shared-queue lock, released when it drops if [`LockedQueue`]'s `Drop`
/// took it.
#[derive(Debug)]
struct QueueLockSlot {
    lock: Option<Arc<dyn ExternalQueueLock>>,
    held: bool,
}

impl LockedQueue {
    // wgpu's browser handles are neither Send nor Sync, so on wasm32 this Arc only shares the queue
    // among clones of one thread's context, as an Rc would; one type keeps every target's context
    // the same.
    #[cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
    fn shared(queue: wgpu::Queue, lock: Option<Arc<dyn ExternalQueueLock>>) -> Arc<Self> {
        Arc::new(Self {
            queue,
            lock: QueueLockSlot { lock, held: false },
        })
    }

    /// Runs `call` holding the shared-queue lock, when there is one.
    fn locked<T>(&self, call: impl FnOnce(&wgpu::Queue) -> T) -> T {
        let guard = self.lock.lock.as_ref().map(|lock| {
            lock.lock();
            QueueGuard(lock.as_ref())
        });
        let result = call(&self.queue);
        drop(guard);
        result
    }
}

impl Drop for LockedQueue {
    fn drop(&mut self) {
        // wgpu waits for the native queue to go idle when its last handle drops, a queue
        // access like a submission, so it must happen under the lock.
        if let Some(lock) = &self.lock.lock {
            lock.lock();
            self.lock.held = true;
        }
    }
}

impl Drop for QueueLockSlot {
    fn drop(&mut self) {
        if self.held
            && let Some(lock) = &self.lock
        {
            lock.unlock();
        }
    }
}

/// Serializes queue access with another API that shares the same queue.
///
/// A queue must be externally synchronized. When a native decoder submits to
/// the same queue as the renderer, both sides have to take one lock, and
/// neither owns the other's submissions. [`GpuContext`] takes it around every
/// call that can reach the native queue, not only submissions. Implementations
/// must be re-entrant: one thread can submit while already holding the lock,
/// for example when a queue-completion callback releases a resource.
pub trait ExternalQueueLock: std::fmt::Debug + Send + Sync + 'static {
    /// Acquires exclusive access to the shared queue.
    fn lock(&self);
    /// Releases access taken by [`ExternalQueueLock::lock`].
    fn unlock(&self);
}

/// Where one [`GpuContext::submit_timed`] call spent its time.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
#[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
pub struct SubmitTimings {
    /// Blocked acquiring the queue lock shared with another API.
    pub queue_lock_wait: Duration,
    /// Inside `wgpu`'s own submission, with the lock already held.
    pub queue_submit: Duration,
}

struct QueueGuard<'a>(&'a dyn ExternalQueueLock);

impl Drop for QueueGuard<'_> {
    fn drop(&mut self) {
        self.0.unlock();
    }
}

impl GpuContext {
    /// Adopts an already-created adapter, device, and queue.
    ///
    /// This is used by native interop backends that must create one logical
    /// device satisfying both wgpu and a platform decoder before either API is
    /// initialized.
    #[must_use]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn from_parts(adapter: wgpu::Adapter, device: wgpu::Device, queue: wgpu::Queue) -> Self {
        Self::from_parts_with_queue_lock(adapter, device, queue, None)
    }

    /// Adopts existing handles whose queue is shared with another API.
    ///
    /// Every call of this context that can reach the native queue takes
    /// `queue_lock` first; see [`GpuContext`]. Such a context has no surface:
    /// it is made for a native decoder's device, and presenting is the one
    /// queue access a caller could otherwise reach through `wgpu` directly.
    #[must_use]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn from_parts_with_queue_lock(
        adapter: wgpu::Adapter,
        device: wgpu::Device,
        queue: wgpu::Queue,
        queue_lock: Option<Arc<dyn ExternalQueueLock>>,
    ) -> Self {
        let capabilities = Arc::new(Capabilities::from_adapter(&adapter));
        let diagnostics = Arc::new(DiagnosticHook::new());
        install_device_callbacks(&device, &diagnostics);
        Self {
            adapter,
            device,
            queue: LockedQueue::shared(queue, queue_lock),
            capabilities,
            diagnostics,
        }
    }

    /// The logical device.
    #[must_use]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub const fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// The queue, for the calls that never reach the native queue: buffer and
    /// texture writes (recorded until the next submission), timestamp
    /// periods, and backend interop.
    ///
    /// Submitting, presenting, waiting for submitted work and mapping go
    /// through this context instead, so that they hold the shared-queue lock.
    #[must_use]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue.queue
    }

    /// Calls `callback` once the work submitted so far completes, holding the
    /// shared-queue lock when one is set.
    ///
    /// `wgpu` flushes writes still pending here, and they are pending after a
    /// submission it rejected: that flush is a native submission.
    pub fn on_submitted_work_done(&self, callback: impl FnOnce() + Send + 'static) {
        #[expect(clippy::disallowed_methods, reason = "this is the locked wrapper")]
        self.queue
            .locked(|queue| queue.on_submitted_work_done(callback));
    }

    /// Maps `slice` for `mode`, holding the shared-queue lock when one is set.
    ///
    /// `wgpu` first submits the writes still pending to that buffer.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn map_async(
        &self,
        slice: &wgpu::BufferSlice<'_>,
        mode: wgpu::MapMode,
        callback: impl FnOnce(Result<(), wgpu::BufferAsyncError>) + wgpu::WasmNotSend + 'static,
    ) {
        #[expect(clippy::disallowed_methods, reason = "this is the locked wrapper")]
        self.queue.locked(|_| slice.map_async(mode, callback));
    }

    /// Presents `texture`, holding the shared-queue lock when one is set.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn present(&self, texture: wgpu::SurfaceTexture) {
        #[expect(clippy::disallowed_methods, reason = "this is the locked wrapper")]
        self.queue.locked(|queue| queue.present(texture));
    }

    /// Submits command buffers, holding the shared-queue lock when one is set.
    ///
    /// Callers must use this instead of submitting through the queue directly,
    /// or a native decoder sharing the queue could submit concurrently.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn submit<I>(&self, command_buffers: I) -> wgpu::SubmissionIndex
    where
        I: IntoIterator<Item = wgpu::CommandBuffer>,
    {
        self.submit_measured(command_buffers, false).0
    }

    /// Submits command buffers and reports where the call spent its time.
    ///
    /// The two halves answer different questions: waiting for the lock means
    /// the native decoder is submitting on the same queue, while time inside
    /// `wgpu`'s submit is this process's own encoding and validation cost.
    /// Plain [`GpuContext::submit`] reads no clock at all.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn submit_timed<I>(&self, command_buffers: I) -> (wgpu::SubmissionIndex, SubmitTimings)
    where
        I: IntoIterator<Item = wgpu::CommandBuffer>,
    {
        self.submit_measured(command_buffers, true)
    }

    fn submit_measured<I>(
        &self,
        command_buffers: I,
        measure: bool,
    ) -> (wgpu::SubmissionIndex, SubmitTimings)
    where
        I: IntoIterator<Item = wgpu::CommandBuffer>,
    {
        let mut timings = SubmitTimings::default();
        let started = measure.then(Instant::now);
        let index = self.queue.locked(|queue| {
            let acquired = started.map(|started| {
                let now = Instant::now();
                timings.queue_lock_wait = now.duration_since(started);
                now
            });
            #[expect(clippy::disallowed_methods, reason = "this is the locked wrapper")]
            let index = queue.submit(command_buffers);
            if let Some(acquired) = acquired {
                timings.queue_submit = acquired.elapsed();
            }
            index
        });
        (index, timings)
    }

    /// Requests a surface-independent device.
    ///
    /// # Errors
    /// Returns [`GpuError`] when adapter or device acquisition fails.
    pub async fn request() -> Result<Self, GpuError> {
        let _turn = crate::serialize_acquisition();
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        Self::request_compatible(&instance, None).await
    }

    /// Requests a surface-independent device, preferring a discrete/high-performance adapter.
    ///
    /// `WGPU_POWER_PREF` still takes precedence when it is explicitly set.
    ///
    /// # Errors
    /// Returns [`GpuError`] when adapter or device acquisition fails.
    pub async fn request_high_performance() -> Result<Self, GpuError> {
        Self::request_high_performance_without(wgpu::Features::empty()).await
    }

    /// Requests a device as [`Self::request_high_performance`] does, on the
    /// same adapter and with the same limits, without the optional features in
    /// `withheld`.
    ///
    /// A native device stands in for a browser's this way: browser WebGPU
    /// never has 16-bit normalized textures, so the path a browser takes for
    /// 10-bit frames is tested on a device that withholds them. Opening such a
    /// device straight from wgpu would skip the adapter fallback below and
    /// fail where wgpu prefers an adapter that makes no device.
    ///
    /// # Errors
    /// Returns [`GpuError`] when adapter or device acquisition fails.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub async fn request_high_performance_without(
        withheld: wgpu::Features,
    ) -> Result<Self, GpuError> {
        let _turn = crate::serialize_acquisition();
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let preference =
            wgpu::PowerPreference::from_env().unwrap_or(wgpu::PowerPreference::HighPerformance);
        Self::request_compatible_with_preference(&instance, None, preference, withheld).await
    }

    /// Requests a high-performance device on `backends` only.
    ///
    /// A host whose hardware decoder shares its device with the renderer
    /// needs that device on the decoder's API, so it cannot let wgpu pick.
    /// `WGPU_BACKEND`, when set, still wins, for debugging.
    ///
    /// # Errors
    /// Returns [`GpuError`] when no adapter on `backends` can create a device.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub async fn request_high_performance_on(backends: wgpu::Backends) -> Result<Self, GpuError> {
        let _turn = crate::serialize_acquisition();
        let instance = Self::instance_on(backends);
        let preference =
            wgpu::PowerPreference::from_env().unwrap_or(wgpu::PowerPreference::HighPerformance);
        Self::request_compatible_with_preference(
            &instance,
            None,
            preference,
            wgpu::Features::empty(),
        )
        .await
    }

    /// Requests a high-performance device on `backends`, as
    /// [`Self::request_high_performance_on`] does, that `accept` takes.
    ///
    /// A hardware decoder that shares the renderer's device may not open on
    /// every adapter: on a laptop whose discrete GPU's driver has no D3D12
    /// video device, D3D12VA opens on the integrated GPU only. When `accept`
    /// refuses the preferred adapter's device, a device is opened on each
    /// other GPU adapter in turn, in the order that request falls back
    /// through, until `accept` takes one; a CPU adapter is not tried. When
    /// none is taken, the result holds the preferred adapter's device, to
    /// render on without what `accept` makes.
    ///
    /// # Errors
    /// Returns [`GpuError`] when no adapter on `backends` can create a device.
    #[cfg(not(target_arch = "wasm32"))]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub async fn request_high_performance_on_accepted<T, E>(
        backends: wgpu::Backends,
        mut accept: impl FnMut(&Self) -> Result<T, E>,
    ) -> Result<AcceptedDevice<T, E>, GpuError> {
        let _turn = crate::serialize_acquisition();
        let instance = Self::instance_on(backends);
        let preference =
            wgpu::PowerPreference::from_env().unwrap_or(wgpu::PowerPreference::HighPerformance);
        let preferred = Self::request_compatible_with_preference(
            &instance,
            None,
            preference,
            wgpu::Features::empty(),
        )
        .await?;
        let mut refusals = AdapterRefusals(Vec::new());
        match accept(&preferred) {
            Ok(value) => {
                return Ok(AcceptedDevice {
                    context: preferred,
                    accepted: Some(value),
                    refusals,
                });
            }
            Err(error) => refusals.0.push((preferred.adapter.get_info().name, error)),
        }
        let tried = preferred.adapter.get_info();
        let mut others: Vec<wgpu::Adapter> = instance
            .enumerate_adapters(wgpu::Backends::all())
            .await
            .into_iter()
            .filter(|other| {
                let info = other.get_info();
                info != tried && info.device_type != wgpu::DeviceType::Cpu
            })
            .collect();
        others.sort_by_key(|other| fallback_rank(other.get_info().device_type, preference));
        for other in others {
            let name = other.get_info().name;
            let Ok(context) = Self::open_device(other, wgpu::Features::empty()).await else {
                continue;
            };
            match accept(&context) {
                Ok(value) => {
                    return Ok(AcceptedDevice {
                        context,
                        accepted: Some(value),
                        refusals,
                    });
                }
                Err(error) => refusals.0.push((name, error)),
            }
        }
        Ok(AcceptedDevice {
            context: preferred,
            accepted: None,
            refusals,
        })
    }

    /// Every adapter on `backends`, in the order
    /// [`Self::request_high_performance_on_accepted`] tries them: the one
    /// `wgpu` prefers first, then the other GPUs by kind for the preference,
    /// and CPU adapters last, which that request never tries. Each comes
    /// with its position in the backend's own enumeration, which names it
    /// for as long as the machine's adapters stay the same.
    ///
    /// A caller that has to know what every adapter can do, and not only
    /// whether the first acceptable one exists, opens each one it wants
    /// with [`Self::open_adapter`].
    #[cfg(not(target_arch = "wasm32"))]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub async fn adapters_by_preference_on(
        backends: wgpu::Backends,
    ) -> Vec<(usize, wgpu::Adapter)> {
        let _turn = crate::serialize_acquisition();
        let instance = Self::instance_on(backends);
        let preference =
            wgpu::PowerPreference::from_env().unwrap_or(wgpu::PowerPreference::HighPerformance);
        let preferred = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: preference,
                ..Default::default()
            })
            .await
            .ok()
            .map(|adapter| adapter.get_info());
        let mut adapters: Vec<(usize, wgpu::Adapter)> = instance
            .enumerate_adapters(wgpu::Backends::all())
            .await
            .into_iter()
            .enumerate()
            .collect();
        // A stable sort, so adapters of one kind keep the backend's own order.
        adapters.sort_by_key(|(_, adapter)| {
            let info = adapter.get_info();
            if info.device_type == wgpu::DeviceType::Cpu {
                (2, u8::MAX)
            } else if preferred.as_ref() == Some(&info) {
                (0, 0)
            } else {
                (1, fallback_rank(info.device_type, preference))
            }
        });
        adapters
    }

    /// Opens a device on `adapter`, with the features and limits every
    /// request here opens one with.
    ///
    /// # Errors
    /// Returns [`GpuError`] when the adapter cannot create a device.
    #[cfg(not(target_arch = "wasm32"))]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub async fn open_adapter(adapter: wgpu::Adapter) -> Result<Self, GpuError> {
        let _turn = crate::serialize_acquisition();
        Self::open_device(adapter, wgpu::Features::empty()).await
    }

    /// The adapter this device was opened on.
    #[must_use]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub const fn adapter(&self) -> &wgpu::Adapter {
        &self.adapter
    }

    /// An instance on `backends`, which `WGPU_BACKEND` overrides when set.
    fn instance_on(backends: wgpu::Backends) -> wgpu::Instance {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = backends;
        wgpu::Instance::new(descriptor.with_env())
    }

    /// Requests a device compatible with an optional presentation surface.
    ///
    /// WebGL2 requires a compatible surface during adapter selection, while
    /// browser WebGPU and native headless callers may pass `None`.
    ///
    /// # Errors
    /// Returns [`GpuError`] when adapter or device acquisition fails.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub async fn request_compatible(
        instance: &wgpu::Instance,
        compatible_surface: Option<&wgpu::Surface<'_>>,
    ) -> Result<Self, GpuError> {
        let preference = wgpu::PowerPreference::from_env().unwrap_or_default();
        Self::request_compatible_with_preference(
            instance,
            compatible_surface,
            preference,
            wgpu::Features::empty(),
        )
        .await
    }

    /// Opens a device on the adapter `wgpu` prefers, or, when that adapter
    /// cannot make one, on the next adapter that can.
    ///
    /// An adapter can be listed and still fail every device request: on a
    /// Linux machine whose Vulkan loader offers only lavapipe, the preferred
    /// high-performance adapter is the discrete GPU's OpenGL driver, which
    /// fails with a lost device. Giving up there would leave a working Vulkan
    /// device unused and the host unable to start. Natively the other
    /// adapters are tried in the preference's order; the error returned when
    /// all fail is the preferred adapter's. A browser offers one adapter.
    /// Every device is opened without the optional features in `withheld`.
    async fn request_compatible_with_preference(
        instance: &wgpu::Instance,
        compatible_surface: Option<&wgpu::Surface<'_>>,
        power_preference: wgpu::PowerPreference,
        withheld: wgpu::Features,
    ) -> Result<Self, GpuError> {
        let _turn = crate::serialize_acquisition();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface,
                power_preference,
                ..Default::default()
            })
            .await
            .map_err(GpuError::adapter)?;
        #[cfg(not(target_arch = "wasm32"))]
        let preferred = adapter.get_info();
        let error = match Self::open_device(adapter, withheld).await {
            Ok(context) => return Ok(context),
            Err(error) => error,
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut others: Vec<wgpu::Adapter> = instance
                .enumerate_adapters(wgpu::Backends::all())
                .await
                .into_iter()
                .filter(|other| other.get_info() != preferred)
                .filter(|other| {
                    compatible_surface.is_none_or(|surface| other.is_surface_supported(surface))
                })
                .collect();
            others
                .sort_by_key(|other| fallback_rank(other.get_info().device_type, power_preference));
            for other in others {
                if let Ok(context) = Self::open_device(other, withheld).await {
                    return Ok(context);
                }
            }
        }
        Err(error)
    }

    async fn open_device(
        adapter: wgpu::Adapter,
        withheld: wgpu::Features,
    ) -> Result<Self, GpuError> {
        let capabilities = Arc::new(Capabilities::from_adapter(&adapter));
        // 16-bit norm formats carry 10-bit planes CoreVideo hands over as standalone R16/RG16
        // textures; NV12/P010 are needed only for one bi-planar decoder resource.
        let optional_features = wgpu::Features::TIMESTAMP_QUERY
            | wgpu::Features::TEXTURE_FORMAT_16BIT_NORM
            | if cfg!(target_os = "windows") {
                wgpu::Features::TEXTURE_FORMAT_NV12 | wgpu::Features::TEXTURE_FORMAT_P010
            } else {
                wgpu::Features::empty()
            };
        let required_features = adapter.features() & optional_features & !withheld;
        let required_limits =
            wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits());
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                required_features,
                required_limits,
                ..wgpu::DeviceDescriptor::default()
            })
            .await
            .map_err(GpuError::device)?;
        let diagnostics = Arc::new(DiagnosticHook::new());
        install_device_callbacks(&device, &diagnostics);
        Ok(Self {
            adapter,
            device,
            queue: LockedQueue::shared(queue, None),
            capabilities,
            diagnostics,
        })
    }

    /// Routes uncaptured errors and device-loss notifications to `handler`.
    ///
    /// Without a handler, uncaptured errors are dropped in release builds and
    /// panic in debug builds or under `ALPHA2_GPU_STRICT=1`, as wgpu's default
    /// handler does; device loss is dropped. The driver may invoke the handler
    /// from an arbitrary thread, so it must not block. Installing a handler
    /// replaces any previous one. On `wasm32` the browser owns these callbacks
    /// and the handler is never invoked.
    pub fn set_diagnostic_handler(&self, handler: impl Fn(GpuDiagnostic) + Send + Sync + 'static) {
        let handler: GpuDiagnosticHandler = Arc::new(handler);
        self.diagnostics.set(Some(handler));
    }

    /// Drops the installed diagnostic handler, if any.
    pub fn clear_diagnostic_handler(&self) {
        self.diagnostics.set(None);
    }

    /// Returns immutable capability decisions.
    #[must_use]
    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// This device, with renderers made on it storing their intermediate
    /// pictures in `Rgba8Unorm`, as they do on an adapter that cannot render,
    /// filter, and copy half floats.
    ///
    /// Every adapter supports the 8-bit format, so the fallback can be run,
    /// tested, and compared with half float on any device. The returned
    /// context shares the device, queue, and diagnostic handler with `self`,
    /// as a clone does; only its capabilities differ.
    #[must_use]
    pub fn with_rgba8_intermediate(&self) -> Self {
        let mut capabilities = Capabilities::clone(&self.capabilities);
        capabilities.intermediate_format = IntermediateFormat::Rgba8Unorm;
        Self {
            capabilities: Arc::new(capabilities),
            ..self.clone()
        }
    }

    /// This device, reported as `tier`: renderers made on it apply that
    /// tier's limits, as they do on an adapter of that tier (a WebGL2 or
    /// GL ES one for Tier B).
    ///
    /// So the downlevel policy can be run and tested on any device, as
    /// [`Self::with_rgba8_intermediate`] lets the 8-bit fallback be. The
    /// returned context shares the device, queue, and diagnostic handler
    /// with `self`; only its capabilities differ.
    #[must_use]
    pub fn with_tier(&self, tier: CapabilityTier) -> Self {
        let mut capabilities = Capabilities::clone(&self.capabilities);
        capabilities.tier = tier;
        Self {
            capabilities: Arc::new(capabilities),
            ..self.clone()
        }
    }

    /// Chooses a configuration supported by this context and surface, in a
    /// format that stores what is written to it as it is.
    ///
    /// What the renderer presents is already sRGB-encoded, so an sRGB surface
    /// format would encode it a second time and show it too bright. wgpu's
    /// default for a WebGL2 canvas is `Rgba8UnormSrgb`; the surface's
    /// non-sRGB format is taken wherever it offers one.
    #[must_use]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn surface_config(
        &self,
        surface: &wgpu::Surface<'_>,
        width: u32,
        height: u32,
    ) -> Option<wgpu::SurfaceConfiguration> {
        let mut config = surface.get_default_config(&self.adapter, width, height)?;
        if let Some(format) = surface
            .get_capabilities(&self.adapter)
            .formats
            .into_iter()
            .find(|format| !format.is_srgb())
        {
            config.format = format;
        }
        Some(config)
    }
}

/// What [`GpuContext::request_high_performance_on_accepted`] found: the device
/// to render on and what `accept` made of it, or, when no adapter's device
/// was taken, the preferred adapter's device and no value.
#[derive(Debug)]
#[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
pub struct AcceptedDevice<T, E> {
    /// The device `accept` took, or the preferred adapter's when none was.
    pub context: GpuContext,
    /// What `accept` made of `context`; `None` when it took no device.
    pub accepted: Option<T>,
    /// Each adapter `accept` refused before, in the order they were tried.
    pub refusals: AdapterRefusals<E>,
}

/// The adapters whose devices a caller refused, each by name with its
/// reason, in the order they were tried.
#[derive(Debug)]
#[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
pub struct AdapterRefusals<E>(pub Vec<(String, E)>);

impl<E: std::fmt::Display> std::fmt::Display for AdapterRefusals<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, (adapter, error)) in self.0.iter().enumerate() {
            if index > 0 {
                formatter.write_str("; ")?;
            }
            write!(formatter, "{adapter}: {error}")?;
        }
        Ok(())
    }
}

impl<E: std::fmt::Display + std::fmt::Debug> std::error::Error for AdapterRefusals<E> {}

/// Where an adapter of `device_type` comes among the fallbacks for
/// `preference`: the kinds of GPU it asks for first, a CPU last. Without a
/// preference the enumeration order stands.
#[cfg(not(target_arch = "wasm32"))]
fn fallback_rank(device_type: wgpu::DeviceType, preference: wgpu::PowerPreference) -> u8 {
    use wgpu::DeviceType::{Cpu, DiscreteGpu, IntegratedGpu, Other, VirtualGpu};
    let order: &[wgpu::DeviceType] = match preference {
        wgpu::PowerPreference::HighPerformance => {
            &[DiscreteGpu, IntegratedGpu, VirtualGpu, Other, Cpu]
        }
        wgpu::PowerPreference::LowPower => &[IntegratedGpu, DiscreteGpu, VirtualGpu, Other, Cpu],
        wgpu::PowerPreference::None => return 0,
    };
    order
        .iter()
        .position(|kind| *kind == device_type)
        .and_then(|rank| u8::try_from(rank).ok())
        .unwrap_or(u8::MAX)
}

// A library must not write to process stderr, so device events go through the same opt-in hook for
// both ordinary and externally-created devices.
#[cfg(not(target_arch = "wasm32"))]
fn install_device_callbacks(device: &wgpu::Device, diagnostics: &Arc<DiagnosticHook>) {
    let uncaptured = Arc::clone(diagnostics);
    device.on_uncaptured_error(Arc::new(move |error| {
        uncaptured.emit(GpuDiagnostic::uncaptured(&error));
    }));
    let lost = Arc::clone(diagnostics);
    device.set_device_lost_callback(move |reason, message| {
        lost.emit(GpuDiagnostic::DeviceLost {
            reason: format!("{reason:?}"),
            message,
        });
    });
}

#[cfg(target_arch = "wasm32")]
fn install_device_callbacks(_device: &wgpu::Device, _diagnostics: &Arc<DiagnosticHook>) {}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::fallback_rank;
    use wgpu::DeviceType::{Cpu, DiscreteGpu, IntegratedGpu, Other, VirtualGpu};
    use wgpu::PowerPreference;

    #[test]
    fn fallback_adapters_follow_the_preference_and_a_cpu_comes_last() {
        let ordered = |preference| {
            let mut kinds = vec![Cpu, Other, VirtualGpu, IntegratedGpu, DiscreteGpu];
            kinds.sort_by_key(|kind| fallback_rank(*kind, preference));
            kinds
        };
        assert_eq!(
            ordered(PowerPreference::HighPerformance),
            [DiscreteGpu, IntegratedGpu, VirtualGpu, Other, Cpu]
        );
        assert_eq!(
            ordered(PowerPreference::LowPower),
            [IntegratedGpu, DiscreteGpu, VirtualGpu, Other, Cpu]
        );
        assert_eq!(
            ordered(PowerPreference::None),
            [Cpu, Other, VirtualGpu, IntegratedGpu, DiscreteGpu]
        );
    }
}

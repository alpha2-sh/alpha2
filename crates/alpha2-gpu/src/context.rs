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
        let capabilities = Arc::new(Capabilities::from_device(&adapter, &device));
        crate::validation_log::watch_device(&device, &capabilities.summary());
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
        let instance = crate::new_instance(wgpu::Backends::all());
        Self::request_compatible(&instance, None).await
    }

    /// Requests the product device with the default request: the device a
    /// host renders on, as [`Self::request_product_with`] picks it.
    ///
    /// # Errors
    /// Returns [`GpuError`] when no adapter opens a device.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn request_product() -> Result<Self, GpuError> {
        Self::request_product_with(&ProductRequest::default()).await
    }

    /// Requests the product device: on this OS's product backends
    /// ([`product_backends`]), the adapter `ALPHA2_GPU_ADAPTER` pins, else the
    /// first in the high-performance order that opens a device.
    ///
    /// Each backend group is enumerated once. A DX12 group that lists no
    /// adapter is a failed DX12 instance (WARP is always listed on Windows),
    /// reported with the shader compiler it was made with. OpenGL adapters
    /// older than desktop GL 4.3 or GLES 3.1 are refused with the version.
    /// `WGPU_POWER_PREF` is not consulted.
    ///
    /// # Errors
    /// Returns [`GpuError`]: a pin that names no adapter, a DX12 instance
    /// that did not start, no hardware adapter under
    /// [`SoftwareAdapters::Refuse`], or the first device error.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn request_product_with(request: &ProductRequest) -> Result<Self, GpuError> {
        let _turn = crate::serialize_acquisition();
        let mut tried = ProductAttempt::default();
        for adapter in product_candidates(request).await? {
            match Self::open_device(adapter.adapter, request.withheld).await {
                Ok(context) => return Ok(context),
                Err(error) => tried.refused(adapter.name, error),
            }
        }
        Err(tried.into_error(request))
    }

    /// Requests the product device that `accept` takes, trying adapters in
    /// the order [`Self::request_product_with`] does.
    ///
    /// A hardware decoder that shares the renderer's device may not open on
    /// every adapter: on a laptop whose discrete GPU's driver has no D3D12
    /// video device, D3D12VA opens on the integrated GPU only. When `accept`
    /// refuses the first device, a device is opened on each other hardware
    /// adapter in turn until `accept` takes one; a CPU adapter is tried only
    /// when pinned. When none is taken, the result holds the first device, to
    /// render on without what `accept` makes.
    ///
    /// # Errors
    /// Returns [`GpuError`] when no adapter opens a device.
    #[cfg(not(target_arch = "wasm32"))]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub async fn request_product_accepted<T, E>(
        mut accept: impl FnMut(&Self) -> Result<T, E>,
    ) -> Result<AcceptedDevice<T, E>, GpuError> {
        let _turn = crate::serialize_acquisition();
        let request = ProductRequest::default();
        let mut tried = ProductAttempt::default();
        let mut refusals = AdapterRefusals(Vec::new());
        let mut first: Option<Self> = None;
        for candidate in product_candidates(&request).await? {
            if first.is_some() && candidate.cpu && !candidate.pinned {
                continue;
            }
            let context = match Self::open_device(candidate.adapter, request.withheld).await {
                Ok(context) => context,
                Err(error) => {
                    tried.refused(candidate.name, error);
                    continue;
                }
            };
            match accept(&context) {
                Ok(value) => {
                    return Ok(AcceptedDevice {
                        context,
                        accepted: Some(value),
                        refusals,
                    });
                }
                Err(error) => {
                    refusals.0.push((candidate.name, error));
                    first.get_or_insert(context);
                }
            }
        }
        match first {
            Some(context) => Ok(AcceptedDevice {
                context,
                accepted: None,
                refusals,
            }),
            None => Err(tried.into_error(&request)),
        }
    }

    /// Every adapter on `backends`, in the order a product request tries
    /// them (the high-performance rank, enumeration order within a rank),
    /// each with its position in the backend's own enumeration, which names it
    /// for as long as the machine's adapters stay the same. Enumerates once.
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
        let instance = crate::new_instance(backends);
        let mut adapters: Vec<Option<wgpu::Adapter>> = instance
            .enumerate_adapters(wgpu::Backends::all())
            .await
            .into_iter()
            .map(Some)
            .collect();
        let kinds: Vec<(usize, wgpu::DeviceType)> = adapters
            .iter()
            .enumerate()
            .map(|(index, adapter)| {
                let kind = adapter.as_ref().map_or(wgpu::DeviceType::Other, |adapter| {
                    adapter.get_info().device_type
                });
                (index, kind)
            })
            .collect();
        crate::adapter_pin::product_order(&kinds)
            .into_iter()
            .filter_map(|index| adapters[index].take().map(|adapter| (index, adapter)))
            .collect()
    }

    /// Opens a device on `adapter`, with the features and limits every
    /// request here opens one with.
    ///
    /// # Errors
    /// Returns [`GpuError`] when the adapter cannot create a device.
    #[cfg(not(target_arch = "wasm32"))]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub async fn open_adapter(adapter: wgpu::Adapter) -> Result<Self, GpuError> {
        Self::open_adapter_without(adapter, wgpu::Features::empty()).await
    }

    /// Opens a device on `adapter` as [`Self::open_adapter`] does, without
    /// the optional features in `withheld`, so a test can reach the paths a
    /// device without them takes on an adapter that has them.
    ///
    /// # Errors
    /// Returns [`GpuError`] when the adapter cannot create the device.
    #[cfg(not(target_arch = "wasm32"))]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub async fn open_adapter_without(
        adapter: wgpu::Adapter,
        withheld: wgpu::Features,
    ) -> Result<Self, GpuError> {
        let _turn = crate::serialize_acquisition();
        Self::open_device(adapter, withheld).await
    }

    /// The adapter this device was opened on.
    #[must_use]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub const fn adapter(&self) -> &wgpu::Adapter {
        &self.adapter
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
        let capabilities = Arc::new(Capabilities::from_device(&adapter, &device));
        crate::validation_log::watch_device(&device, &capabilities.summary());
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

/// What [`GpuContext::request_product_accepted`] found: the device to render
/// on and what `accept` made of it, or, when no adapter's device was taken,
/// the first device and no value.
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

/// Whether a product request may open a device on a software adapter
/// (lavapipe, llvmpipe, WARP, `SwiftShader`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SoftwareAdapters {
    /// A software adapter is tried after every hardware one.
    #[default]
    Allow,
    /// A software adapter is never opened unless `ALPHA2_GPU_ADAPTER` pins it:
    /// the host's `auto` renders on the CPU instead.
    Refuse,
}

/// What a product request may open.
#[derive(Clone, Debug, Default)]
pub struct ProductRequest {
    /// Whether software adapters are acceptable.
    pub software: SoftwareAdapters,
    /// Optional features the device is opened without, so a test can reach
    /// the paths a device without them takes.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub withheld: wgpu::Features,
}

/// The backends a product device is tried on, in order: Windows DX12, macOS
/// Metal, Linux and other native systems Vulkan then GL, the browser any.
/// `WGPU_BACKEND`, when set, is the one group instead.
#[must_use]
pub fn product_backends() -> Vec<wgpu::Backends> {
    if let Some(backends) = wgpu::Backends::from_env() {
        return vec![backends];
    }
    if cfg!(target_arch = "wasm32") {
        vec![wgpu::Backends::all()]
    } else if cfg!(target_os = "windows") {
        vec![wgpu::Backends::DX12]
    } else if cfg!(any(target_os = "macos", target_os = "ios")) {
        vec![wgpu::Backends::METAL]
    } else {
        vec![wgpu::Backends::VULKAN, wgpu::Backends::GL]
    }
}

/// An adapter a product request will try.
#[cfg(not(target_arch = "wasm32"))]
struct Candidate {
    adapter: wgpu::Adapter,
    name: String,
    cpu: bool,
    pinned: bool,
}

/// What a product request tried and why each failed.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
struct ProductAttempt {
    refusals: Vec<(String, String)>,
    first: Option<GpuError>,
}

#[cfg(not(target_arch = "wasm32"))]
impl ProductAttempt {
    fn refused(&mut self, adapter: String, error: GpuError) {
        self.refusals.push((adapter, error.to_string()));
        self.first.get_or_insert(error);
    }

    fn into_error(self, request: &ProductRequest) -> GpuError {
        match (request.software, self.first) {
            (SoftwareAdapters::Allow, Some(first)) => first,
            _ => GpuError::no_hardware(Vec::new(), self.refusals),
        }
    }
}

/// The adapters of the first product backend group that has one acceptable
/// adapter, in the order a product request tries them.
#[cfg(not(target_arch = "wasm32"))]
async fn product_candidates(request: &ProductRequest) -> Result<Vec<Candidate>, GpuError> {
    let pin = crate::AdapterPin::from_env();
    let mut listed = Vec::new();
    let mut software = Vec::new();
    let mut refusals = Vec::new();
    for group in product_backends() {
        let instance = crate::new_instance(group);
        let adapters = instance.enumerate_adapters(group).await;
        if group == wgpu::Backends::DX12 && adapters.is_empty() {
            return Err(GpuError::no_dx12_adapter());
        }
        let kinds: Vec<(usize, wgpu::DeviceType)> = adapters
            .iter()
            .enumerate()
            .map(|(index, adapter)| (index, adapter.get_info().device_type))
            .collect();
        let mut adapters: Vec<Option<wgpu::Adapter>> = adapters.into_iter().map(Some).collect();
        let mut candidates = Vec::new();
        for index in crate::adapter_pin::product_order(&kinds) {
            let Some(adapter) = adapters[index].take() else {
                continue;
            };
            let info = adapter.get_info();
            listed.push(format!("[{index}] {} ({:?})", info.name, info.backend));
            let pinned = pin.names(index, &info.name);
            if pin.is_pin() && !pinned {
                continue;
            }
            let cpu = info.device_type == wgpu::DeviceType::Cpu;
            if cpu && request.software == SoftwareAdapters::Refuse && !pinned {
                software.push(info.name);
                continue;
            }
            if info.backend == wgpu::Backend::Gl
                && let Err(reason) = crate::adapter_pin::gl_version_supported(&info.driver_info)
            {
                refusals.push((info.name.clone(), reason));
                continue;
            }
            candidates.push(Candidate {
                adapter,
                name: info.name,
                cpu,
                pinned,
            });
        }
        if !candidates.is_empty() {
            return Ok(candidates);
        }
    }
    if pin.is_pin() {
        return Err(GpuError::pin(format!(
            "{} names no adapter; the adapters are {}",
            pin.setting(),
            if listed.is_empty() {
                "none".to_owned()
            } else {
                listed.join(", ")
            }
        )));
    }
    Err(GpuError::no_hardware(software, refusals))
}

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

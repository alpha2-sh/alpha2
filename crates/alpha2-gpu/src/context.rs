use std::sync::Arc;
use std::time::Duration;

use web_time::Instant;

use crate::diagnostic::DiagnosticHook;
use crate::{
    Capabilities, CapabilityTier, GpuDiagnostic, GpuDiagnosticHandler, GpuError, IntermediateFormat,
};

/// A GPU device and queue owned by one render host.
///
/// Clones are cheap and share the same device and diagnostic handler
/// ([`GpuContext::set_diagnostic_handler`]).
///
/// # The queue (`wgpu` interop)
///
/// Every `wgpu` call that can reach the native queue is a method here and takes the shared-queue
/// lock when there is one: `submit`, [`Self::on_submitted_work_done`] and `map_async` (both may
/// flush pending writes), and `present`. Dropping the last clone also drops the queue under the
/// lock, because `wgpu` waits for the native queue to idle there. `queue` hands out the handle
/// for everything else, and each crate's clippy configuration forbids queue-reaching calls on it.
#[derive(Clone, Debug)]
pub struct GpuContext {
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: Arc<LockedQueue>,
    // Shared so clones (one per readback) copy no strings.
    capabilities: Arc<Capabilities>,
    diagnostics: Arc<DiagnosticHook>,
    rate: Arc<crate::rate::GpuRate>,
}

/// The queue and its shared lock.
#[derive(Debug)]
struct LockedQueue {
    // Declared before `lock` so it drops while `Drop` holds the lock.
    queue: wgpu::Queue,
    lock: QueueLockSlot,
}

/// The shared-queue lock, released on drop if [`LockedQueue`]'s `Drop` took it.
#[derive(Debug)]
struct QueueLockSlot {
    lock: Option<Arc<dyn ExternalQueueLock>>,
    held: bool,
}

impl LockedQueue {
    // On wasm32 the handles are not Send or Sync, so this Arc acts as an Rc; one type keeps every
    // target the same.
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
        // Dropping the last handle waits for the native queue to idle, a queue access that needs
        // the lock.
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
/// A native decoder sharing the renderer's queue must take the same lock. [`GpuContext`] takes
/// it around every call that can reach the native queue. Implementations must be re-entrant,
/// because a queue-completion callback may submit while the lock is held.
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
    /// Used by native interop backends that create one device for both wgpu and a platform
    /// decoder.
    #[must_use]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn from_parts(adapter: wgpu::Adapter, device: wgpu::Device, queue: wgpu::Queue) -> Self {
        Self::from_parts_with_queue_lock(adapter, device, queue, None)
    }

    /// Adopts existing handles whose queue is shared with another API.
    ///
    /// Every call that can reach the native queue takes `queue_lock` first; see [`GpuContext`].
    /// Such a context has no surface, since presenting would bypass the lock.
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
            rate: Arc::default(),
        }
    }

    /// The logical device.
    #[must_use]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub const fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// The queue, for calls that never reach the native queue (writes, timestamp periods,
    /// backend interop). Submit, present, wait and map through this context instead.
    #[must_use]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue.queue
    }

    /// Calls `callback` once submitted work completes, holding the shared-queue lock.
    ///
    /// `wgpu` flushes pending writes here, which is a native submission.
    pub fn on_submitted_work_done(&self, callback: impl FnOnce() + Send + 'static) {
        #[expect(clippy::disallowed_methods, reason = "this is the locked wrapper")]
        self.queue
            .locked(|queue| queue.on_submitted_work_done(callback));
    }

    /// Maps `slice` for `mode`, holding the shared-queue lock, since `wgpu` may first submit
    /// pending writes.
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

    /// Submits command buffers, holding the shared-queue lock. Never submit through the queue
    /// directly.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn submit<I>(&self, command_buffers: I) -> wgpu::SubmissionIndex
    where
        I: IntoIterator<Item = wgpu::CommandBuffer>,
    {
        self.submit_measured(command_buffers, false).0
    }

    /// Submits command buffers and reports where the call spent its time.
    ///
    /// Lock wait reflects a native decoder using the queue; submit time is `wgpu`'s own
    /// validation cost. Plain [`GpuContext::submit`] reads no clock.
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

    /// Requests the product device with the default request ([`Self::request_product_with`]).
    ///
    /// # Errors
    /// Returns [`GpuError`] when no adapter opens a device.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn request_product() -> Result<Self, GpuError> {
        Self::request_product_with(&ProductRequest::default()).await
    }

    /// Requests the product device on [`product_backends`]: the adapter `ALPHA2_GPU_ADAPTER`
    /// pins, else the first in high-performance order that opens a device.
    ///
    /// A DX12 group that lists no adapter is a failed instance, since WARP is always listed.
    /// GL adapters older than GL 4.3 or GLES 3.1 are refused. `WGPU_POWER_PREF` is ignored.
    ///
    /// # Errors
    /// Returns [`GpuError`] for an unmatched pin, a failed DX12 instance, no hardware adapter
    /// under [`SoftwareAdapters::Refuse`], or the first device error.
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

    /// Requests the product device that `accept` takes, in [`Self::request_product_with`] order.
    ///
    /// A shared-device hardware decoder may open on only some adapters (D3D12VA needs a D3D12
    /// video device). Each hardware adapter is tried until `accept` takes one; a CPU adapter
    /// only when pinned. If none is taken, the result holds the first device.
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

    /// Every adapter on `backends` in product-request order, each with its enumeration index.
    /// Open them with [`Self::open_adapter`].
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

    /// Opens a device on `adapter` with the standard features and limits.
    ///
    /// # Errors
    /// Returns [`GpuError`] when the adapter cannot create a device.
    #[cfg(not(target_arch = "wasm32"))]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub async fn open_adapter(adapter: wgpu::Adapter) -> Result<Self, GpuError> {
        Self::open_adapter_without(adapter, wgpu::Features::empty()).await
    }

    /// Like [`Self::open_adapter`] but without the optional features in `withheld`, for tests.
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

    /// Opens a device on the adapter `wgpu` prefers, falling back to the next adapter that can.
    ///
    /// A listed adapter can still fail every device request (such as a GL driver beside a
    /// lavapipe-only Vulkan loader). On failure of all, the preferred adapter's error is returned.
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
        // 16-bit norm formats hold CoreVideo's 10-bit planes as R16/RG16 textures; NV12/P010 are
        // only needed for bi-planar decoder resources.
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
            rate: Arc::default(),
        })
    }

    /// Routes uncaptured errors and device-loss notifications to `handler`.
    ///
    /// Without a handler, uncaptured errors are dropped in release and panic in debug or under
    /// `ALPHA2_GPU_STRICT=1`; device loss is dropped. The handler may run on any thread and must
    /// not block. It replaces any previous handler and is never invoked on `wasm32`.
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

    /// This device's throughput rate.
    #[must_use]
    pub fn rate(&self) -> &Arc<crate::rate::GpuRate> {
        &self.rate
    }

    /// This device with an `Rgba8Unorm` intermediate format, so the 8-bit fallback can be tested
    /// on any device. Shares the device, queue and handler with `self`.
    #[must_use]
    pub fn with_rgba8_intermediate(&self) -> Self {
        let mut capabilities = Capabilities::clone(&self.capabilities);
        capabilities.intermediate_format = IntermediateFormat::Rgba8Unorm;
        Self {
            capabilities: Arc::new(capabilities),
            ..self.clone()
        }
    }

    /// This device reported as `tier`, so downlevel policy can be tested on any device. Shares
    /// the device, queue and handler with `self`.
    #[must_use]
    pub fn with_tier(&self, tier: CapabilityTier) -> Self {
        let mut capabilities = Capabilities::clone(&self.capabilities);
        capabilities.tier = tier;
        Self {
            capabilities: Arc::new(capabilities),
            ..self.clone()
        }
    }

    /// Chooses a surface configuration with a non-sRGB format where available.
    ///
    /// Presented frames are already sRGB-encoded, so an sRGB format would encode them twice
    /// (wgpu defaults WebGL2 canvases to `Rgba8UnormSrgb`).
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

/// The result of [`GpuContext::request_product_accepted`].
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

/// Refused adapters by name with reasons, in the order tried.
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
    /// Software adapters open only when `ALPHA2_GPU_ADAPTER` pins them.
    Refuse,
}

/// What a product request may open.
#[derive(Clone, Debug, Default)]
pub struct ProductRequest {
    /// Whether software adapters are acceptable.
    pub software: SoftwareAdapters,
    /// Optional features to withhold, for tests.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub withheld: wgpu::Features,
}

/// Product backend groups in order: DX12 on Windows, Metal on macOS, Vulkan then GL elsewhere,
/// any in the browser. `WGPU_BACKEND` replaces them when set.
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

/// The adapters of the first product backend group with an acceptable adapter, in request order.
#[cfg(not(target_arch = "wasm32"))]
async fn product_candidates(request: &ProductRequest) -> Result<Vec<Candidate>, GpuError> {
    let pin = crate::AdapterPolicy::from_env();
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
            let cpu = crate::software_adapter(&info);
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

/// Fallback rank for `preference`: requested GPU kinds first, CPU last.
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

// A library must not write to stderr, so device events go through the opt-in hook.
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

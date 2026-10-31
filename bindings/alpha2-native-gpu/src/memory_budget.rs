//! How much device memory the platform says the renderer's adapter may use.
//!
//! Each graphics API that has a budget reports it its own way: DXGI's local
//! segment budget (`QueryVideoMemoryInfo`), the device-local heap budgets of
//! Vulkan's `VK_EXT_memory_budget`, and Metal's `recommendedMaxWorkingSetSize`.
//! An adapter on another backend, or a Vulkan device without the extension,
//! has none.

/// The device memory budget of `adapter` in bytes, as its graphics API reports
/// it, or `None` where the API reports none.
#[must_use]
pub fn adapter_memory_budget(adapter: &wgpu::Adapter) -> Option<u64> {
    platform_budget(adapter).filter(|bytes| *bytes > 0)
}

/// The sum of the budgets of the device-local heaps, when the device has
/// `VK_EXT_memory_budget`.
#[cfg(target_os = "linux")]
fn platform_budget(adapter: &wgpu::Adapter) -> Option<u64> {
    use ash::vk;

    // SAFETY: the guard only borrows the live adapter for these queries.
    let hal = unsafe { adapter.as_hal::<wgpu_hal::api::Vulkan>() }?;
    let physical = hal.raw_physical_device();
    let instance = hal.shared_instance().raw_instance();
    // SAFETY: `physical` is a physical device of `instance`, which outlives the call.
    let extensions = unsafe { instance.enumerate_device_extension_properties(physical) }.ok()?;
    let has_budget = extensions
        .iter()
        .any(|extension| extension.extension_name_as_c_str() == Ok(ash::ext::memory_budget::NAME));
    if !has_budget {
        return None;
    }
    let mut budget = vk::PhysicalDeviceMemoryBudgetPropertiesEXT::default();
    let mut properties = vk::PhysicalDeviceMemoryProperties2::default().push_next(&mut budget);
    // SAFETY: the device has the extension, so the chained structure is one the call fills; both
    // structures outlive the call. wgpu creates its instance at Vulkan 1.1 or later, which has the
    // entry point.
    unsafe { instance.get_physical_device_memory_properties2(physical, &mut properties) };
    let heaps = properties.memory_properties;
    let count = usize::try_from(heaps.memory_heap_count).ok()?;
    let total = heaps.memory_heaps[..count.min(heaps.memory_heaps.len())]
        .iter()
        .zip(budget.heap_budget)
        .filter(|(heap, _)| heap.flags.contains(vk::MemoryHeapFlags::DEVICE_LOCAL))
        .map(|(_, bytes)| bytes)
        .fold(0_u64, u64::saturating_add);
    Some(total)
}

/// The budget of the adapter's local memory segment group.
#[cfg(target_os = "windows")]
fn platform_budget(adapter: &wgpu::Adapter) -> Option<u64> {
    // SAFETY: the guard only borrows the live adapter for the query.
    let hal = unsafe { adapter.as_hal::<wgpu_hal::api::Dx12>() }?;
    let info = hal
        .raw_adapter()
        .query_video_memory_info(windows::Win32::Graphics::Dxgi::DXGI_MEMORY_SEGMENT_GROUP_LOCAL)
        .ok()?;
    Some(info.Budget)
}

/// The device's recommended working set.
#[cfg(target_os = "macos")]
fn platform_budget(adapter: &wgpu::Adapter) -> Option<u64> {
    use objc2_metal::MTLDevice;

    // SAFETY: the guard only borrows the live adapter for the query.
    let hal = unsafe { adapter.as_hal::<wgpu_hal::api::Metal>() }?;
    Some(hal.raw_device().recommendedMaxWorkingSetSize())
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn platform_budget(_: &wgpu::Adapter) -> Option<u64> {
    None
}

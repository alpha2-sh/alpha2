//! The Vulkan shim's device setup and queue locks, without a GPU.
//!
//! `alpha2_vulkan_device_init` stores the handles it is given and calls
//! nothing through them; only `av_hwdevice_ctx_init` would. A context set up
//! over dummy handles therefore runs all of the shim's own setup (the queue
//! family table, the feature chain, the extension copies, the locks), and
//! dropping it runs the shim's free callback, here and under valgrind. Its one
//! requirement is a Vulkan loader to resolve `vkGetInstanceProcAddr` from.

// A device context's buffer data is the `AVHWDeviceContext` FFmpeg allocated, aligned for it.
#![allow(clippy::cast_ptr_alignment)]

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use super::*;
use interop::Alpha2VulkanQueueFamily as QueueFamily;

unsafe extern "C" {
    fn alpha2_vulkan_device_queue_families(
        context: *const ffi::AVHWDeviceContext,
        output: *mut QueueFamily,
        capacity: u32,
    ) -> i32;
}

/// `VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_COMPUTE_BIT | VK_QUEUE_TRANSFER_BIT`.
const GRAPHICS: u32 = 0x1 | 0x2 | 0x4;
/// `VK_QUEUE_VIDEO_DECODE_BIT_KHR`.
const DECODE: u32 = 0x20;
/// `VK_QUEUE_VIDEO_ENCODE_BIT_KHR`.
const ENCODE: u32 = 0x40;
/// `VK_VIDEO_CODEC_OPERATION_DECODE_H264_BIT_KHR`.
const DECODE_CAPS: u32 = 0x1;
/// `VK_VIDEO_CODEC_OPERATION_ENCODE_H264_BIT_KHR`.
const ENCODE_CAPS: u32 = 0x1_0000;

/// A Vulkan device context the shim set up over dummy handles, with graphics
/// on family 0, decode on `decode` and, when given, encode on `encode`.
fn shim_device(decode: u32, encode: Option<u32>) -> AVHWDeviceContext {
    let instance_extensions = [
        c"VK_KHR_get_physical_device_properties2",
        c"VK_EXT_debug_utils",
    ]
    .map(CStr::as_ptr);
    let device_extensions = [
        c"VK_KHR_video_queue",
        c"VK_KHR_video_decode_queue",
        c"VK_KHR_video_decode_h264",
    ]
    .map(CStr::as_ptr);
    let info = VulkanDeviceDescriptor {
        instance: 0x10,
        physical_device: 0x20,
        device: 0x30,
        graphics_queue_family: 0,
        graphics_queue_count: 1,
        decode_queue_family: decode,
        decode_queue_count: 1,
        decode_video_caps: DECODE_CAPS,
        encode_queue_family: encode.unwrap_or(0),
        encode_queue_count: u32::from(encode.is_some()),
        encode_video_caps: ENCODE_CAPS,
        instance_extensions: instance_extensions.as_ptr(),
        instance_extension_count: 2,
        device_extensions: device_extensions.as_ptr(),
        device_extension_count: 3,
        ..Default::default()
    };
    let context = alloc_hwdevice(ffi::AV_HWDEVICE_TYPE_VULKAN, "Vulkan").expect("a Vulkan context");
    // SAFETY: a freshly allocated, writable Vulkan device context. The shim copies the extension
    // names before it returns and stores the dummy handles without calling through them.
    let status = unsafe {
        alpha2_vulkan_device_init(
            context.data.cast::<ffi::AVHWDeviceContext>(),
            &raw const info,
        )
    };
    assert_eq!(status, 0, "the shim needs a Vulkan loader to resolve from");
    context
}

/// The context's queue-family table as (family, queues, flags, video caps).
fn families(context: &AVHWDeviceContext) -> Vec<(i32, i32, u32, u32)> {
    let mut table = [QueueFamily::default(); 8];
    // SAFETY: the context is live, and the shim writes at most `table.len()` entries.
    let count = unsafe {
        alpha2_vulkan_device_queue_families(
            context.data.cast::<ffi::AVHWDeviceContext>().cast_const(),
            table.as_mut_ptr(),
            8,
        )
    };
    let count = usize::try_from(count).expect("a context the shim set up");
    table[..count]
        .iter()
        .map(|family| (family.index, family.count, family.flags, family.video_caps))
        .collect()
}

/// Every topology the device can have gives `FFmpeg` one entry per queue
/// family, carrying each role on it, including a video family that both decodes
/// and encodes, which no GPU this project tests on has.
#[test]
fn the_shim_gives_ffmpeg_one_queue_family_entry_per_family_the_device_has() {
    let graphics = (0, 1, GRAPHICS, 0);
    for (decode, encode, expected) in [
        (
            1,
            Some(2),
            vec![
                graphics,
                (1, 1, DECODE, DECODE_CAPS),
                (2, 1, ENCODE, ENCODE_CAPS),
            ],
        ),
        (
            1,
            Some(1),
            vec![graphics, (1, 1, DECODE | ENCODE, DECODE_CAPS | ENCODE_CAPS)],
        ),
        (1, None, vec![graphics, (1, 1, DECODE, DECODE_CAPS)]),
        (
            1,
            Some(0),
            vec![
                (0, 1, GRAPHICS | ENCODE, ENCODE_CAPS),
                (1, 1, DECODE, DECODE_CAPS),
            ],
        ),
        (0, None, vec![(0, 1, GRAPHICS | DECODE, DECODE_CAPS)]),
        (
            0,
            Some(0),
            vec![(0, 1, GRAPHICS | DECODE | ENCODE, DECODE_CAPS | ENCODE_CAPS)],
        ),
        (
            0,
            Some(2),
            vec![
                (0, 1, GRAPHICS | DECODE, DECODE_CAPS),
                (2, 1, ENCODE, ENCODE_CAPS),
            ],
        ),
    ] {
        let context = shim_device(decode, encode);
        assert_eq!(
            families(&context),
            expected,
            "decode on {decode}, encode on {encode:?}"
        );
    }
}

/// A queue lock is only made over a device the shim set up, for a family it
/// keeps a lock for. Over a context `FFmpeg` made it would lock nothing, and
/// over one of another device type it would read that type's private data as
/// the shim's locks.
#[test]
fn a_queue_lock_is_only_made_over_a_device_the_bridge_set_up() {
    let ffmpeg_own = alloc_hwdevice(ffi::AV_HWDEVICE_TYPE_VULKAN, "Vulkan").expect("a context");
    assert!(matches!(
        VulkanQueueLock::new(ffmpeg_own, 0),
        Err(BridgeError::InvalidInput(_))
    ));

    let device = shim_device(1, None);
    assert!(VulkanQueueLock::new(device.clone(), 0).is_ok());
    assert!(VulkanQueueLock::new(device.clone(), 7).is_ok());
    assert!(
        VulkanQueueLock::new(device.clone(), 8).is_err(),
        "a family the shim keeps no lock for"
    );

    // Only Vulkan is compiled into the pinned SDK, so another device type is stood in for by
    // relabelling this one; it is restored before the context is freed.
    let raw = device.data.cast::<ffi::AVHWDeviceContext>();
    // SAFETY: `device` keeps the context alive, and nothing else reads it meanwhile.
    unsafe { (*raw).type_ = ffi::AV_HWDEVICE_TYPE_CUDA };
    let refused = VulkanQueueLock::new(device.clone(), 0);
    // SAFETY: as above.
    unsafe { (*raw).type_ = ffi::AV_HWDEVICE_TYPE_VULKAN };
    assert!(
        refused.is_err(),
        "a CUDA context's private data is not the shim's"
    );
}

/// The guard holds the lock against other threads until it drops, and one
/// thread may hold it more than once.
#[test]
fn a_queue_lock_guard_holds_other_threads_off_until_it_drops() {
    let lock = Arc::new(VulkanQueueLock::new(shim_device(1, None), 0).expect("a queue lock"));
    let outer = lock.lock();
    let inner = lock.lock();
    let entered = Arc::new(AtomicBool::new(false));
    let waiter = std::thread::spawn({
        let lock = Arc::clone(&lock);
        let entered = Arc::clone(&entered);
        move || {
            let _guard = lock.lock();
            entered.store(true, Ordering::Release);
        }
    });
    std::thread::sleep(Duration::from_millis(100));
    assert!(!entered.load(Ordering::Acquire), "held twice");
    drop(inner);
    std::thread::sleep(Duration::from_millis(100));
    assert!(!entered.load(Ordering::Acquire), "still held once");
    drop(outer);
    waiter
        .join()
        .expect("the waiter takes the lock once it is free");
    assert!(entered.load(Ordering::Acquire));
}

/// What holds `FFmpeg`'s frame mutex or the bridge's queue mutex cannot be sent
/// to another thread, where its drop could not release them. Each line is
/// ambiguous, so fails to compile, if its type is `Send`.
#[test]
fn what_holds_a_shim_mutex_stays_on_its_thread() {
    trait AmbiguousIfSend<A> {
        fn some_item() {}
    }
    impl<T: ?Sized> AmbiguousIfSend<()> for T {}
    impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}
    let _ = <VulkanEncodeFrameAccess<'static> as AmbiguousIfSend<_>>::some_item;
    let _ = <VulkanQueueGuard<'static> as AmbiguousIfSend<_>>::some_item;
    let _ = <VulkanFrameAccessGuard<'static> as AmbiguousIfSend<_>>::some_item;
}

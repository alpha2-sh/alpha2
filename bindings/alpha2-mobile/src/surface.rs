//! Window surfaces from the handles apps pass: an Android `ANativeWindow` or an iOS
//! `CAMetalLayer`. wgpu's view-based paths are not used; they panic off the main thread.

use std::ffi::c_void;
use std::ptr::NonNull;

/// A window the preview draws into, owned while attached.
#[derive(Debug)]
pub(crate) enum NativeWindow {
    /// An `ANativeWindow` with a reference this window releases.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    Android(NonNull<c_void>),
    /// A `CAMetalLayer`, retained from the caller's thread until this window drops, so the app
    /// may release its own reference as soon as it has attached the layer.
    #[cfg(target_vendor = "apple")]
    MetalLayer(objc2::rc::Retained<objc2::runtime::AnyObject>),
}

// SAFETY: both handles may be used and released from any thread: an `ANativeWindow` is reference
// counted and thread safe, and a `CAMetalLayer` is only handed to wgpu, which itself retains and
// releases it off the main thread.
unsafe impl Send for NativeWindow {}

impl NativeWindow {
    /// The window at `handle` on this platform: an `ANativeWindow` pointer whose reference the
    /// window takes over on Android, a `CAMetalLayer` pointer on Apple platforms, which it retains.
    pub(crate) fn from_handle(handle: u64) -> Option<Self> {
        Self::from_pointer(NonNull::new(usize::try_from(handle).ok()? as *mut c_void)?)
    }

    #[cfg(target_vendor = "apple")]
    fn from_pointer(pointer: NonNull<c_void>) -> Option<Self> {
        // SAFETY: the app passes a live `CAMetalLayer`; retaining it on the caller's thread keeps
        // it alive whenever the queued attach runs.
        let layer = unsafe { objc2::rc::Retained::retain(pointer.as_ptr().cast()) }?;
        Some(Self::MetalLayer(layer))
    }

    #[cfg(not(target_vendor = "apple"))]
    fn from_pointer(pointer: NonNull<c_void>) -> Option<Self> {
        cfg!(target_os = "android").then_some(Self::Android(pointer))
    }

    /// A surface drawing into this window, which must outlive it.
    pub(crate) fn surface(
        &self,
        instance: &wgpu::Instance,
    ) -> Result<wgpu::Surface<'static>, String> {
        let target = match self {
            Self::Android(window) => wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(platform_display()),
                raw_window_handle: wgpu::rwh::RawWindowHandle::AndroidNdk(
                    wgpu::rwh::AndroidNdkWindowHandle::new(*window),
                ),
            },
            #[cfg(target_vendor = "apple")]
            Self::MetalLayer(layer) => wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(
                objc2::rc::Retained::as_ptr(layer).cast_mut().cast(),
            ),
        };
        // SAFETY: the handle is a live window of this platform, and the player drops the surface
        // before it releases the window.
        unsafe { instance.create_surface_unsafe(target) }.map_err(|error| error.to_string())
    }
}

/// This platform's display: what an instance that creates window surfaces is made with, and every
/// surface names the same one.
fn platform_display() -> wgpu::rwh::RawDisplayHandle {
    if cfg!(target_vendor = "apple") {
        wgpu::rwh::RawDisplayHandle::UiKit(wgpu::rwh::UiKitDisplayHandle::new())
    } else {
        wgpu::rwh::RawDisplayHandle::Android(wgpu::rwh::AndroidDisplayHandle::new())
    }
}

/// The platform display as an instance takes it.
#[derive(Debug)]
struct PlatformDisplay;

impl wgpu::rwh::HasDisplayHandle for PlatformDisplay {
    fn display_handle(&self) -> Result<wgpu::rwh::DisplayHandle<'_>, wgpu::rwh::HandleError> {
        // SAFETY: the Android and UIKit display handles carry no pointer; they are valid always.
        Ok(unsafe { wgpu::rwh::DisplayHandle::borrow_raw(platform_display()) })
    }
}

/// An instance on `backends` that can create window surfaces.
pub(crate) fn window_instance(backends: wgpu::Backends) -> wgpu::Instance {
    wgpu::Instance::new(
        alpha2_gpu::instance_descriptor(backends).with_display_handle(Box::new(PlatformDisplay)),
    )
}

impl Drop for NativeWindow {
    fn drop(&mut self) {
        #[cfg(target_os = "android")]
        {
            let Self::Android(window) = self;
            // SAFETY: the window holds one reference, taken over from `ANativeWindow_fromSurface`.
            unsafe { ndk_sys::ANativeWindow_release(window.as_ptr().cast()) };
        }
    }
}

#[cfg(all(test, target_os = "ios"))]
mod tests {
    use super::*;

    #[test]
    fn a_metal_layer_window_holds_a_reference_of_its_own_until_it_drops() {
        let layer = objc2_quartz_core::CAMetalLayer::new();
        // SAFETY: `retainCount` takes no arguments and the layer is live.
        let count = || -> usize { unsafe { objc2::msg_send![&*layer, retainCount] } };
        let before = count();
        let window =
            NativeWindow::from_handle(objc2::rc::Retained::as_ptr(&layer) as u64).expect("window");
        assert_eq!(count(), before + 1);
        drop(window);
        assert_eq!(count(), before);
    }
}

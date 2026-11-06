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
    /// A `CAMetalLayer` the app keeps alive while it is attached.
    #[cfg_attr(not(target_vendor = "apple"), allow(dead_code))]
    MetalLayer(NonNull<c_void>),
}

// SAFETY: both handles may be used from any thread: an `ANativeWindow` is reference counted and
// thread safe, and a `CAMetalLayer` is only handed to wgpu, which draws on its own thread.
unsafe impl Send for NativeWindow {}

impl NativeWindow {
    /// The window at `handle` on this platform: an `ANativeWindow` pointer whose reference the
    /// window takes over on Android, a `CAMetalLayer` pointer on iOS.
    pub(crate) fn from_handle(handle: u64) -> Option<Self> {
        let pointer = NonNull::new(usize::try_from(handle).ok()? as *mut c_void)?;
        if cfg!(target_os = "android") {
            Some(Self::Android(pointer))
        } else if cfg!(target_vendor = "apple") {
            Some(Self::MetalLayer(pointer))
        } else {
            None
        }
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
            Self::MetalLayer(layer) => {
                wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(layer.as_ptr())
            }
            #[cfg(not(target_vendor = "apple"))]
            Self::MetalLayer(_) => return Err("Metal layers exist only on Apple platforms".into()),
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
        if let Self::Android(window) = self {
            // SAFETY: the window holds one reference, taken over from `ANativeWindow_fromSurface`.
            unsafe { ndk_sys::ANativeWindow_release(window.as_ptr().cast()) };
        }
    }
}

//! Decoder output into an `AImageReader` of GPU-sampled private buffers, which the renderer
//! imports without a copy through memory.

// The NDK image reader's FFI boundary.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::ptr::{NonNull, null_mut};
use std::sync::Arc;

use alpha2_host_core::video_stream::{BackendError, SurfaceLease};
use alpha2_media::Rect;
use ndk_sys as sys;

/// Images a reader lends at once besides the reorder buffer's: the stream holds the lookahead and
/// the frame a source last answered, the importer up to its conversions in flight
/// (`MAX_CONVERSIONS_IN_FLIGHT` in `alpha2-native-gpu`, three) until the GPU has read them, and
/// the decoder needs some to keep decoding. Some decoders release pictures in decode order, so
/// the stream holds up to the stream's reorder depth more.
pub(crate) const MAX_IMAGES: i32 = 8;

/// An `AImageReader` of `AIMAGE_FORMAT_PRIVATE` buffers the GPU samples. Every image holds it,
/// since deleting a reader invalidates the images it lent.
#[derive(Debug)]
pub(crate) struct ImageReader {
    raw: NonNull<sys::AImageReader>,
    window: NonNull<sys::ANativeWindow>,
    images: i32,
}

// SAFETY: the NDK reader locks internally; it is acquired from by its session's thread only and
// deleted once, after every image holding it.
unsafe impl Send for ImageReader {}
// SAFETY: as above; shared references only reach the window pointer and image deletion.
unsafe impl Sync for ImageReader {}

impl ImageReader {
    /// A reader whose buffers default to `width` x `height` (a decoder sizes them itself), with
    /// room for `reorder_depth` pictures the stream holds back to reorder.
    pub(crate) fn new(
        width: u32,
        height: u32,
        reorder_depth: usize,
    ) -> Result<Arc<Self>, BackendError> {
        let images = MAX_IMAGES.saturating_add(i32::try_from(reorder_depth).unwrap_or(i32::MAX));
        let dimension = |value: u32| i32::try_from(value.max(1)).unwrap_or(i32::MAX);
        let mut reader = null_mut();
        // SAFETY: the out-pointer outlives the call.
        let status = unsafe {
            sys::AImageReader_newWithUsage(
                dimension(width),
                dimension(height),
                sys::AIMAGE_FORMATS::AIMAGE_FORMAT_PRIVATE.0.cast_signed(),
                sys::AHardwareBuffer_UsageFlags::AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE.0,
                images,
                &raw mut reader,
            )
        };
        let raw = NonNull::new(reader)
            .filter(|_| status.0 == 0)
            .ok_or_else(|| {
                BackendError::Unsupported(format!(
                    "no GPU-sampled image reader of {width}x{height} ({})",
                    status.0
                ))
            })?;
        let mut window = null_mut();
        // SAFETY: the reader is live; the window belongs to it.
        let status = unsafe { sys::AImageReader_getWindow(raw.as_ptr(), &raw mut window) };
        let Some(window) = NonNull::new(window).filter(|_| status.0 == 0) else {
            // SAFETY: the reader lent nothing yet.
            unsafe { sys::AImageReader_delete(raw.as_ptr()) };
            return Err(BackendError::Failed(format!(
                "the image reader has no window ({})",
                status.0
            )));
        };
        Ok(Arc::new(Self {
            raw,
            window,
            images,
        }))
    }

    /// The window a decoder renders into, valid while the reader lives.
    pub(crate) fn window(&self) -> *mut sys::ANativeWindow {
        self.window.as_ptr()
    }

    /// The next image the decoder rendered, `None` while none is queued.
    pub(crate) fn acquire(
        self: &Arc<Self>,
        ten_bit: bool,
    ) -> Result<Option<AndroidSurface>, BackendError> {
        let mut image = null_mut();
        // SAFETY: the reader is live; the out-pointer outlives the call.
        let status =
            unsafe { sys::AImageReader_acquireNextImage(self.raw.as_ptr(), &raw mut image) };
        if status == sys::media_status_t::AMEDIA_IMGREADER_NO_BUFFER_AVAILABLE {
            return Ok(None);
        }
        if status == sys::media_status_t::AMEDIA_IMGREADER_MAX_IMAGES_ACQUIRED {
            return Err(BackendError::Failed(format!(
                "all {} decoder images are still held",
                self.images
            )));
        }
        let image = NonNull::new(image)
            .filter(|_| status.0 == 0)
            .ok_or_else(|| {
                BackendError::Failed(format!("acquiring a decoder image failed ({})", status.0))
            })?;
        AndroidSurface::new(image, Arc::clone(self), ten_bit).map(Some)
    }
}

impl Drop for ImageReader {
    fn drop(&mut self) {
        // SAFETY: every image holds the reader, so none is still lent.
        unsafe { sys::AImageReader_delete(self.raw.as_ptr()) };
    }
}

/// A decoded picture in a GPU-sampled `AHardwareBuffer`, lent by the decoder's image reader
/// until this drops.
#[derive(Debug)]
pub struct AndroidSurface {
    image: NonNull<sys::AImage>,
    buffer: NonNull<sys::AHardwareBuffer>,
    timestamp_ns: i64,
    crop: Option<Rect>,
    ten_bit: bool,
    // After `image`, which is deleted first.
    _reader: Arc<ImageReader>,
}

// SAFETY: the image is only read through calls the NDK documents as thread safe and deleted once,
// on drop.
unsafe impl Send for AndroidSurface {}
// SAFETY: as above; shared references only read the buffer pointer and plain fields.
unsafe impl Sync for AndroidSurface {}

impl AndroidSurface {
    fn new(
        image: NonNull<sys::AImage>,
        reader: Arc<ImageReader>,
        ten_bit: bool,
    ) -> Result<Self, BackendError> {
        let mut buffer = null_mut();
        // SAFETY: the image is live; the buffer belongs to it.
        let status = unsafe { sys::AImage_getHardwareBuffer(image.as_ptr(), &raw mut buffer) };
        let mut timestamp_ns = 0;
        // SAFETY: as above; the out-pointer outlives the call.
        unsafe { sys::AImage_getTimestamp(image.as_ptr(), &raw mut timestamp_ns) };
        let mut rect = sys::AImageCropRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: as above.
        let cropped = unsafe { sys::AImage_getCropRect(image.as_ptr(), &raw mut rect) }.0 == 0;
        let crop = cropped
            .then(|| {
                Some(Rect {
                    x: u32::try_from(rect.left).ok()?,
                    y: u32::try_from(rect.top).ok()?,
                    width: u32::try_from(rect.right - rect.left)
                        .ok()
                        .filter(|width| *width > 0)?,
                    height: u32::try_from(rect.bottom - rect.top)
                        .ok()
                        .filter(|height| *height > 0)?,
                })
            })
            .flatten();
        let Some(buffer) = NonNull::new(buffer).filter(|_| status.0 == 0) else {
            // SAFETY: the image was acquired and is deleted once.
            unsafe { sys::AImage_delete(image.as_ptr()) };
            return Err(BackendError::Failed(format!(
                "the decoder image has no hardware buffer ({})",
                status.0
            )));
        };
        Ok(Self {
            image,
            buffer,
            timestamp_ns,
            crop,
            ten_bit,
            _reader: reader,
        })
    }

    /// The `AHardwareBuffer`, valid while this lives.
    #[must_use]
    pub fn hardware_buffer(&self) -> NonNull<c_void> {
        self.buffer.cast()
    }

    /// Whether the decoder was asked for 10-bit (P010) output.
    #[must_use]
    pub const fn ten_bit(&self) -> bool {
        self.ten_bit
    }

    /// The buffer's timestamp, as the decoder rendered it.
    pub(crate) const fn timestamp_ns(&self) -> i64 {
        self.timestamp_ns
    }

    /// The valid region in buffer pixels, when the decoder reports one (right and bottom
    /// exclusive).
    pub(crate) const fn crop(&self) -> Option<Rect> {
        self.crop
    }

    /// The buffer's width and height.
    pub(crate) fn extent(&self) -> (u32, u32) {
        let mut description = sys::AHardwareBuffer_Desc {
            width: 0,
            height: 0,
            layers: 0,
            format: 0,
            usage: 0,
            stride: 0,
            rfu0: 0,
            rfu1: 0,
        };
        // SAFETY: the buffer lives while the image does; the out-pointer outlives the call.
        unsafe { sys::AHardwareBuffer_describe(self.buffer.as_ptr(), &raw mut description) };
        (description.width, description.height)
    }
}

impl SurfaceLease for AndroidSurface {
    fn retained_bytes(&self) -> u64 {
        let (width, height) = self.extent();
        // 4:2:0 holds 1.5 samples per pixel, in two bytes each at 10 bits.
        let samples = u64::from(width) * u64::from(height) * 3 / 2;
        if self.ten_bit { samples * 2 } else { samples }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl Drop for AndroidSurface {
    fn drop(&mut self) {
        // SAFETY: the image was acquired from the reader this holds, and is deleted once.
        unsafe { sys::AImage_delete(self.image.as_ptr()) };
    }
}

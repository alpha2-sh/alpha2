//! Platform-neutral descriptions used while importing native image planes.

/// Supported decoded plane organizations.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NativePlaneLayout {
    /// Three independent 8-bit R8 images.
    I420,
    /// 8-bit R8 luma plus one interleaved RG8 chroma image.
    Nv12,
    /// 10-bit R16 luma plus one interleaved RG16 chroma image, left-aligned in
    /// 16-bit containers.
    P010,
}

impl NativePlaneLayout {
    /// Significant bits per component.
    #[must_use]
    pub const fn bit_depth(self) -> u8 {
        match self {
            Self::I420 | Self::Nv12 => 8,
            Self::P010 => 10,
        }
    }

    /// Bytes one coded pixel occupies across all planes of this layout, as a
    /// numerator over 2: 4:2:0 stores 1.5 samples a pixel.
    #[must_use]
    pub const fn bytes_per_pixel_numerator(self) -> u64 {
        match self {
            // 4:2:0 stores 1.5 samples per pixel; 10-bit doubles the container.
            Self::I420 | Self::Nv12 => 3,
            Self::P010 => 6,
        }
    }
}

/// One opaque native plane before it is wrapped by the active GPU backend.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ImportedPlaneDescriptor {
    /// Backend-native image handle.
    pub image: u64,
    /// Backend-native image format.
    pub format: u32,
    /// Plane width in samples.
    pub width: u32,
    /// Plane height in samples.
    pub height: u32,
}

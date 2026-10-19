use crate::{RenderError, Shared};

/// A GPU texture produced by the renderer. Image passes store premultiplied
/// sRGB RGBA; raw solid clears retain the supplied component values.
///
/// Read it back with [`crate::Renderer::read_rgba8`] or the renderer's other
/// readbacks, or draw it as a source through [`SourceView`].
#[derive(Clone, Debug)]
pub struct RenderedTexture {
    pub(crate) size: RenderSize,
    pub(crate) format: wgpu::TextureFormat,
    pub(crate) texture: wgpu::Texture,
    view: wgpu::TextureView,
    _recycle: Option<Shared<RecycleGuard>>,
}

#[derive(Debug)]
struct RecycleGuard {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    token: Option<crate::pool::RecycleToken>,
}

impl Drop for RecycleGuard {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            token.recycle(self.texture.clone(), self.view.clone());
        }
    }
}

impl RenderedTexture {
    #[cfg(test)]
    pub(crate) fn owned(
        size: RenderSize,
        format: wgpu::TextureFormat,
        texture: wgpu::Texture,
    ) -> Self {
        let view = crate::pool::default_view(&texture);
        Self {
            size,
            format,
            texture,
            view,
            _recycle: None,
        }
    }

    /// Texture dimensions in pixels.
    #[must_use]
    pub const fn size(&self) -> RenderSize {
        self.size
    }

    /// The GPU texture format.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    #[must_use]
    pub const fn format(&self) -> wgpu::TextureFormat {
        self.format
    }

    /// The texture this result owns.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    #[must_use]
    pub const fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }

    /// Returns the texture's default view, made once with the texture: a
    /// pooled texture keeps it across every result it holds.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    #[must_use]
    pub const fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    pub(crate) fn pooled(
        size: RenderSize,
        format: wgpu::TextureFormat,
        lease: crate::pool::TextureLease,
    ) -> Self {
        let (texture, view, recycle) = lease.into_parts();
        Self {
            size,
            format,
            texture: texture.clone(),
            view: view.clone(),
            _recycle: Some(Shared::new(RecycleGuard {
                texture,
                view,
                token: Some(recycle),
            })),
        }
    }
}

/// A texture the renderer draws from as a source, borrowed from whoever owns
/// it: a [`RenderedTexture`], an [`crate::UploadTexture`] (what
/// [`crate::Renderer::upload_rgba8`] returns), or, through the `wgpu`
/// interop, any texture view of the renderer's device.
///
/// A source texture stores premultiplied, sRGB-encoded RGBA in a non-sRGB
/// format. The owner keeps it alive and unchanged for as long as the
/// renderer may read it: until the frame drawn from it is submitted.
#[derive(Clone, Copy, Debug)]
pub struct SourceView<'a> {
    view: &'a wgpu::TextureView,
}

impl<'a> SourceView<'a> {
    /// A source drawn from `view`, a view of a texture on the renderer's
    /// device.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    #[must_use]
    pub const fn from_wgpu(view: &'a wgpu::TextureView) -> Self {
        Self { view }
    }

    /// The texture view the renderer samples.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    #[must_use]
    pub const fn wgpu_view(self) -> &'a wgpu::TextureView {
        self.view
    }
}

impl<'a> From<&'a RenderedTexture> for SourceView<'a> {
    fn from(texture: &'a RenderedTexture) -> Self {
        Self {
            view: &texture.view,
        }
    }
}

impl<'a> From<&'a crate::UploadTexture> for SourceView<'a> {
    fn from(texture: &'a crate::UploadTexture) -> Self {
        Self {
            view: texture.view(),
        }
    }
}

/// A non-zero render-target extent.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub struct RenderSize {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl RenderSize {
    /// Largest render-target edge accepted by [`RenderSize::new`].
    pub const MAX_DIMENSION: u32 = alpha2_media::MAX_FRAME_DIMENSION;
    /// Largest render-target area accepted by [`RenderSize::new`].
    pub const MAX_PIXELS: u64 = alpha2_media::MAX_FRAME_PIXELS;

    /// Creates a non-zero render size.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::ZeroDimension`] for a zero width or height.
    pub const fn new(width: u32, height: u32) -> Result<Self, RenderError> {
        if width == 0 || height == 0 {
            Err(RenderError::ZeroDimension)
        } else if width > Self::MAX_DIMENSION
            || height > Self::MAX_DIMENSION
            || (width as u64) * (height as u64) > Self::MAX_PIXELS
        {
            Err(RenderError::DimensionTooLarge)
        } else {
            Ok(Self { width, height })
        }
    }
}

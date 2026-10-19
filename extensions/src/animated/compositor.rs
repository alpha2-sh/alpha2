//! Builds each frame of an animation from partial frames, as GIF and APNG
//! define it: a frame is drawn over (or in place of) what the last one left,
//! after the last one was disposed of as it asked.

use crate::api::ExtensionError;
use crate::budget::check_canvas;

/// How a partial frame's pixels meet the canvas.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum Blend {
    /// They replace it, transparency included.
    Source,
    /// They are drawn over it with their alpha.
    Over,
    /// They are drawn over it with their alpha, in libwebp's integer
    /// arithmetic, which browsers show animated WebP with.
    OverWebp,
}

/// What happens to a frame's area once it has been shown.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum Dispose {
    /// It stays.
    None,
    /// It becomes transparent.
    Background,
    /// It returns to what it was before the frame was drawn.
    Previous,
}

/// One partial frame: straight RGBA8 pixels over a rectangle of the canvas.
pub(crate) struct PartialFrame {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Vec<u8>,
    pub(crate) blend: Blend,
    pub(crate) dispose: Dispose,
}

/// The canvas partial frames are drawn onto, straight RGBA8.
///
/// Drawing a frame costs its own rectangle, never the canvas: a frame that
/// returns to what was before it saves only the rectangle it covers.
pub(crate) struct Compositor {
    width: u32,
    height: u32,
    canvas: Vec<u8>,
    /// The last frame's disposal and rectangle, applied before the next.
    pending: Option<(Dispose, [u32; 4])>,
    /// What a frame that disposes to the previous one covered, before it
    /// was drawn: its rectangle within the canvas, and those pixels.
    saved: Option<([u32; 4], Vec<u8>)>,
    /// Whether a frame has been drawn.
    drawn: bool,
    /// Whether the last frame drawn was what libwebp calls a key frame.
    key: bool,
}

impl Compositor {
    /// A transparent canvas, allocated only once its size is one a provider
    /// opens.
    ///
    /// # Errors
    /// Why [`check_canvas`] refuses the size.
    pub(crate) fn new(width: u32, height: u32) -> Result<Self, ExtensionError> {
        check_canvas(width, height)?;
        Ok(Self {
            width,
            height,
            canvas: vec![0; width as usize * height as usize * 4],
            pending: None,
            saved: None,
            drawn: false,
            key: false,
        })
    }

    /// The whole canvas as the last frame drawn left it.
    pub(crate) fn canvas(&self) -> &[u8] {
        &self.canvas
    }

    /// Draws `frame` over what the last one left, once that is disposed of.
    ///
    /// # Errors
    /// [`ExtensionError::Malformed`] when its pixels are not the rectangle
    /// it declares.
    pub(crate) fn draw(&mut self, frame: &PartialFrame) -> Result<(), ExtensionError> {
        let declared = u64::from(frame.width) * u64::from(frame.height) * 4;
        if u64::try_from(frame.pixels.len()).ok() != Some(declared) {
            return Err(ExtensionError::Malformed(format!(
                "a frame declares {}x{} pixels and its picture holds {} bytes",
                frame.width,
                frame.height,
                frame.pixels.len()
            )));
        }
        let previous = self.pending.take();
        // libwebp draws a frame without blending where the canvas is known to be transparent (on a
        // key frame, and within the rectangle the last frame disposed to the background). Its blend
        // rounds down even over a transparent pixel, so blending there would differ.
        let full = |[x, y, width, height]: [u32; 4]| {
            x == 0 && y == 0 && width >= self.width && height >= self.height
        };
        let key = !self.drawn
            || matches!(previous, Some((Dispose::Background, rect)) if full(rect) || self.key);
        let unblended = match previous {
            _ if key => Some(self.clip([0, 0, self.width, self.height])),
            Some((Dispose::Background, rect)) => Some(self.clip(rect)),
            _ => None,
        };
        let inside = |x: u32, y: u32| {
            unblended.is_some_and(|[left, top, right, bottom]| {
                (left..right).contains(&x) && (top..bottom).contains(&y)
            })
        };
        self.drawn = true;
        self.key = key;
        match previous {
            Some((Dispose::Background, rect)) => self.clear(rect),
            Some((Dispose::Previous, _)) => {
                if let Some((rect, pixels)) = self.saved.take() {
                    self.restore(rect, &pixels);
                }
            }
            Some((Dispose::None, _)) | None => {}
        }
        let rect = [frame.x, frame.y, frame.width, frame.height];
        if frame.dispose == Dispose::Previous {
            let clipped = self.clip(rect);
            self.saved = Some((clipped, self.copy(clipped)));
        }
        let [left, top, right, bottom] = self.clip(rect);
        for y in top..bottom {
            for x in left..right {
                let from = ((y - frame.y) * frame.width + (x - frame.x)) as usize * 4;
                let to = (y * self.width + x) as usize * 4;
                let source = &frame.pixels[from..from + 4];
                let target = &mut self.canvas[to..to + 4];
                match frame.blend {
                    Blend::Source => target.copy_from_slice(source),
                    Blend::Over => over(source, target),
                    Blend::OverWebp if inside(x, y) => target.copy_from_slice(source),
                    Blend::OverWebp => over_webp(source, target),
                }
            }
        }
        self.pending = Some((frame.dispose, rect));
        Ok(())
    }

    /// A rectangle `[x, y, width, height]` within the canvas, as
    /// `[left, top, right, bottom]`.
    fn clip(&self, [x, y, width, height]: [u32; 4]) -> [u32; 4] {
        let left = x.min(self.width);
        let top = y.min(self.height);
        let right = x.saturating_add(width).min(self.width);
        let bottom = y.saturating_add(height).min(self.height);
        [left, top, right, bottom]
    }

    /// The rows of a clipped rectangle's pixels, one after another.
    fn copy(&self, [left, top, right, bottom]: [u32; 4]) -> Vec<u8> {
        let mut pixels = Vec::with_capacity(((right - left) * (bottom - top)) as usize * 4);
        for row in top..bottom {
            let from = (row * self.width + left) as usize * 4;
            let to = (row * self.width + right) as usize * 4;
            pixels.extend_from_slice(&self.canvas[from..to]);
        }
        pixels
    }

    /// Puts back what [`Self::copy`] took from a clipped rectangle.
    fn restore(&mut self, [left, top, right, bottom]: [u32; 4], pixels: &[u8]) {
        let row_bytes = (right - left) as usize * 4;
        if row_bytes == 0 {
            return;
        }
        for (row, saved) in (top..bottom).zip(pixels.chunks_exact(row_bytes)) {
            let from = (row * self.width + left) as usize * 4;
            self.canvas[from..from + row_bytes].copy_from_slice(saved);
        }
    }

    fn clear(&mut self, rect: [u32; 4]) {
        let [left, top, right, bottom] = self.clip(rect);
        for row in top..bottom {
            let from = (row * self.width + left) as usize * 4;
            let to = (row * self.width + right) as usize * 4;
            self.canvas[from..to].fill(0);
        }
    }
}

/// Straight-alpha "over", rounding to nearest: exact wherever the source is
/// opaque or transparent, which is every GIF pixel.
fn over(source: &[u8], target: &mut [u8]) {
    let source_alpha = u32::from(source[3]);
    if source_alpha == 255 {
        target.copy_from_slice(source);
        return;
    }
    if source_alpha == 0 {
        return;
    }
    let target_alpha = u32::from(target[3]);
    // Alpha scaled by 255 to keep the products exact: out = sa + ta * (1 - sa).
    let covered = target_alpha * (255 - source_alpha);
    let alpha = source_alpha * 255 + covered;
    for channel in 0..3 {
        let value =
            u32::from(source[channel]) * source_alpha * 255 + u32::from(target[channel]) * covered;
        target[channel] = u8::try_from((value + alpha / 2) / alpha).unwrap_or(255);
    }
    target[3] = u8::try_from((alpha + 127) / 255).unwrap_or(255);
}

/// libwebp's non-premultiplied blend (`BlendPixelRowNonPremult`): an opaque
/// source replaces the canvas, a transparent one leaves it exactly as it is,
/// and otherwise `dst_factor = (da * (256 - sa)) >> 8`, `a = sa + dst_factor`,
/// and each channel `((s * sa + d * dst_factor) * floor(2^24 / a)) >> 24`.
fn over_webp(source: &[u8], target: &mut [u8]) {
    let source_alpha = u32::from(source[3]);
    if source_alpha == 255 {
        target.copy_from_slice(source);
        return;
    }
    if source_alpha == 0 {
        return;
    }
    let dst_factor = (u32::from(target[3]) * (256 - source_alpha)) >> 8;
    let alpha = source_alpha + dst_factor;
    let scale = (1_u32 << 24) / alpha;
    for channel in 0..3 {
        let unscaled = u64::from(source[channel]) * u64::from(source_alpha)
            + u64::from(target[channel]) * u64::from(dst_factor);
        target[channel] = u8::try_from((unscaled * u64::from(scale)) >> 24).unwrap_or(255);
    }
    target[3] = u8::try_from(alpha).unwrap_or(255);
}

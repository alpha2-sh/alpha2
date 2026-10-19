/// Color space in which compositing arithmetic is evaluated.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum BlendSpace {
    /// Convert sRGB-encoded inputs to linear light before blending.
    #[default]
    LinearLight,
    /// Blend directly in sRGB-encoded component space.
    SrgbEncoded,
}

impl BlendSpace {
    pub(crate) const fn shader_value(self) -> f32 {
        match self {
            Self::LinearLight => 0.0,
            Self::SrgbEncoded => 1.0,
        }
    }
}

/// Artistic RGB operation applied before Porter-Duff source-over coverage.
///
/// The modes and their formulas are those of W3C Compositing and Blending
/// Level 1, evaluated in the renderer's [`BlendSpace`]. The first twelve are
/// separable (each channel on its own); `Hue`, `Saturation`, `Color`, and
/// `Luminosity` are not.
#[derive(Copy, Clone, Debug, Default, Eq, Hash, PartialEq)]
pub enum BlendMode {
    /// Preserve the source color.
    #[default]
    Normal,
    /// Multiply source and backdrop components.
    Multiply,
    /// Inverse-multiply source and backdrop components.
    Screen,
    /// Multiply dark backdrop regions and screen light regions.
    Overlay,
    /// The darker of source and backdrop, per channel.
    Darken,
    /// The lighter of source and backdrop, per channel.
    Lighten,
    /// Brighten the backdrop to reflect the source.
    ColorDodge,
    /// Darken the backdrop to reflect the source.
    ColorBurn,
    /// Multiply or screen, depending on the source.
    HardLight,
    /// Darken or lighten, depending on the source: a diffused hard light.
    SoftLight,
    /// The absolute difference of source and backdrop.
    Difference,
    /// Like difference, with lower contrast.
    Exclusion,
    /// The source's hue with the backdrop's saturation and luminosity.
    Hue,
    /// The source's saturation with the backdrop's hue and luminosity.
    Saturation,
    /// The source's hue and saturation with the backdrop's luminosity.
    Color,
    /// The source's luminosity with the backdrop's hue and saturation.
    Luminosity,
}

impl BlendMode {
    /// Every mode, in the order of W3C Compositing and Blending Level 1.
    pub const ALL: [Self; 16] = [
        Self::Normal,
        Self::Multiply,
        Self::Screen,
        Self::Overlay,
        Self::Darken,
        Self::Lighten,
        Self::ColorDodge,
        Self::ColorBurn,
        Self::HardLight,
        Self::SoftLight,
        Self::Difference,
        Self::Exclusion,
        Self::Hue,
        Self::Saturation,
        Self::Color,
        Self::Luminosity,
    ];

    /// The index `composite.wesl` switches on.
    pub(crate) const fn shader_value(self) -> f32 {
        match self {
            Self::Normal => 0.0,
            Self::Multiply => 1.0,
            Self::Screen => 2.0,
            Self::Overlay => 3.0,
            Self::Darken => 4.0,
            Self::Lighten => 5.0,
            Self::ColorDodge => 6.0,
            Self::ColorBurn => 7.0,
            Self::HardLight => 8.0,
            Self::SoftLight => 9.0,
            Self::Difference => 10.0,
            Self::Exclusion => 11.0,
            Self::Hue => 12.0,
            Self::Saturation => 13.0,
            Self::Color => 14.0,
            Self::Luminosity => 15.0,
        }
    }
}

//! The values a numeric place accepts, shared by validation and by operations that write values.

/// An inclusive range of accepted values; an unbounded end is infinite.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Domain {
    /// The least value.
    pub min: f32,
    /// The greatest value.
    pub max: f32,
}

impl Domain {
    /// Every finite value: positions, scales, rotations.
    pub const ANY: Self = Self::new(f32::NEG_INFINITY, f32::INFINITY);
    /// `0..=1`: opacities, roundness, transition progress.
    pub const UNIT: Self = Self::new(0.0, 1.0);
    /// `0..`: lengths, sizes, feathers.
    pub const NON_NEGATIVE: Self = Self::new(0.0, f32::INFINITY);
    /// `-1..=1`: pans.
    pub const PAN: Self = Self::new(-1.0, 1.0);
    /// `0..=MAX_GAIN`: clip and bus gains.
    pub const GAIN: Self = Self::new(0.0, crate::MAX_GAIN);
    /// The points of a polystar.
    pub const POLYSTAR_POINTS: Self =
        Self::new(crate::MIN_POLYSTAR_POINTS, crate::MAX_POLYSTAR_POINTS);

    /// The range `min..=max`.
    #[must_use]
    pub const fn new(min: f32, max: f32) -> Self {
        Self { min, max }
    }

    /// Whether `value` is finite and inside.
    #[must_use]
    pub fn contains(self, value: f32) -> bool {
        value.is_finite() && self.min <= value && value <= self.max
    }

    /// `value` moved to the nearest end when outside.
    #[must_use]
    pub const fn clamp(self, value: f32) -> f32 {
        value.clamp(self.min, self.max)
    }
}

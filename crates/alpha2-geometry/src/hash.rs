//! A stable 128-bit hash: the same input bits give the same key on every
//! target and in every run, unlike `std`'s randomly seeded hashers.

use kurbo::{BezPath, PathEl, Point};

/// Two FNV-style lanes over the bytes written, as the render graph keys its
/// results.
#[derive(Clone, Debug)]
pub struct StableHash([u64; 2]);

impl Default for StableHash {
    fn default() -> Self {
        Self::new()
    }
}

impl StableHash {
    /// A hash of nothing yet.
    #[must_use]
    pub const fn new() -> Self {
        Self([0xcbf2_9ce4_8422_2325, 0x6c62_272e_07bb_0142])
    }

    /// Adds `bytes`, length first, so adjacent writes cannot run together.
    pub fn bytes(&mut self, bytes: &[u8]) {
        self.u64(bytes.len() as u64);
        for byte in bytes {
            self.mix(*byte);
        }
    }

    /// Adds a 32-bit value.
    pub fn u32(&mut self, value: u32) {
        for byte in value.to_le_bytes() {
            self.mix(byte);
        }
    }

    /// Adds a 64-bit value.
    pub fn u64(&mut self, value: u64) {
        for byte in value.to_le_bytes() {
            self.mix(byte);
        }
    }

    /// Adds a float's bits.
    pub fn f32(&mut self, value: f32) {
        self.u32(value.to_bits());
    }

    /// Adds a float's bits.
    pub fn f64(&mut self, value: f64) {
        self.u64(value.to_bits());
    }

    /// Adds a point's bits.
    pub fn point(&mut self, point: Point) {
        self.f64(point.x);
        self.f64(point.y);
    }

    /// Adds every element of a path, with its kind.
    pub fn path(&mut self, path: &BezPath) {
        self.u64(path.elements().len() as u64);
        for element in path.elements() {
            match *element {
                PathEl::MoveTo(p) => {
                    self.mix(b'M');
                    self.point(p);
                }
                PathEl::LineTo(p) => {
                    self.mix(b'L');
                    self.point(p);
                }
                PathEl::QuadTo(a, b) => {
                    self.mix(b'Q');
                    self.point(a);
                    self.point(b);
                }
                PathEl::CurveTo(a, b, c) => {
                    self.mix(b'C');
                    self.point(a);
                    self.point(b);
                    self.point(c);
                }
                PathEl::ClosePath => self.mix(b'Z'),
            }
        }
    }

    fn mix(&mut self, byte: u8) {
        self.0[0] ^= u64::from(byte);
        self.0[0] = self.0[0].wrapping_mul(0x0000_0100_0000_01b3);
        self.0[1] ^= u64::from(byte).wrapping_add(0x9d);
        self.0[1] = self.0[1].wrapping_mul(0x9e37_79b1_85eb_ca87);
    }

    /// The key of everything added.
    #[must_use]
    pub fn finish(&self) -> u128 {
        (u128::from(self.0[1]) << 64) | u128::from(self.0[0])
    }
}

pub const MAX_LEVELS: usize = 4;
const MIN_LEVEL_SIDE: usize = 16;

#[derive(Clone, Debug)]
pub struct Plane {
    pub width: usize,
    pub height: usize,
    pub data: Vec<f32>,
}

impl Plane {
    pub fn from_luma(width: usize, height: usize, bytes: &[u8]) -> Self {
        Self {
            width,
            height,
            data: bytes.iter().map(|&value| f32::from(value)).collect(),
        }
    }

    pub fn at(&self, x: usize, y: usize) -> f32 {
        self.data[y * self.width + x]
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "coordinates are clamped inside a frame far smaller than 2^24 pixels"
    )]
    pub fn sample(&self, x: f32, y: f32) -> f32 {
        let x = x.clamp(0.0, (self.width - 1) as f32);
        let y = y.clamp(0.0, (self.height - 1) as f32);
        let x0 = x.floor();
        let y0 = y.floor();
        let fx = x - x0;
        let fy = y - y0;
        let (x0, y0) = (x0 as usize, y0 as usize);
        let x1 = (x0 + 1).min(self.width - 1);
        let y1 = (y0 + 1).min(self.height - 1);
        let top = self.at(x0, y0) + (self.at(x1, y0) - self.at(x0, y0)) * fx;
        let bottom = self.at(x0, y1) + (self.at(x1, y1) - self.at(x0, y1)) * fx;
        top + (bottom - top) * fy
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a frame is far smaller than 2^24 pixels"
        )]
        let (right, bottom) = ((self.width - 1) as f32, (self.height - 1) as f32);
        (0.0..=right).contains(&x) && (0.0..=bottom).contains(&y)
    }

    fn half(&self) -> Self {
        const KERNEL: [f32; 5] = [1.0 / 16.0, 4.0 / 16.0, 6.0 / 16.0, 4.0 / 16.0, 1.0 / 16.0];
        let width = self.width.div_ceil(2);
        let height = self.height.div_ceil(2);
        let clamp =
            |value: isize, len: usize| value.clamp(0, len.cast_signed() - 1).cast_unsigned();
        let mut rows = vec![0.0; width * self.height];
        for y in 0..self.height {
            for x in 0..width {
                let centre = (2 * x).cast_signed();
                rows[y * width + x] = KERNEL
                    .iter()
                    .enumerate()
                    .map(|(tap, weight)| {
                        weight * self.at(clamp(centre + tap.cast_signed() - 2, self.width), y)
                    })
                    .sum();
            }
        }
        let mut data = vec![0.0; width * height];
        for y in 0..height {
            let centre = (2 * y).cast_signed();
            for x in 0..width {
                data[y * width + x] = KERNEL
                    .iter()
                    .enumerate()
                    .map(|(tap, weight)| {
                        weight
                            * rows[clamp(centre + tap.cast_signed() - 2, self.height) * width + x]
                    })
                    .sum();
            }
        }
        Self {
            width,
            height,
            data,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Pyramid {
    pub levels: Vec<Plane>,
}

impl Pyramid {
    pub fn new(base: Plane) -> Self {
        let mut levels = vec![base];
        while levels.len() < MAX_LEVELS {
            let last = &levels[levels.len() - 1];
            if last.width.min(last.height) / 2 < MIN_LEVEL_SIDE {
                break;
            }
            let next = last.half();
            levels.push(next);
        }
        Self { levels }
    }

    pub fn base(&self) -> &Plane {
        &self.levels[0]
    }

    pub fn level_for(&self, spacing: f32) -> usize {
        let mut level = 0;
        while level + 1 < self.levels.len() && spacing >= scale_of(level + 1) {
            level += 1;
        }
        level
    }
}

pub fn scale_of(level: usize) -> f32 {
    #[expect(clippy::cast_precision_loss, reason = "a power of two is exact")]
    let pixels = (1_u32 << level) as f32;
    pixels
}

use alpha2_media::{
    BitDepth, Chroma, ChromaLocation, ColorMetadata, PixelLayout, PlaneLayout, YuvEncoding,
};

use super::{Picture, pack};
use crate::PlaneTarget;

fn layout(chroma: Chroma, bit_depth: BitDepth, alpha: bool) -> PixelLayout {
    PixelLayout {
        chroma,
        bit_depth,
        alpha,
        planes: PlaneLayout::Planar,
    }
}

const CENTRED: YuvEncoding = YuvEncoding::new(ColorMetadata::EXPORT, ChromaLocation::Center);

/// Packs a `width` x `height` picture whose texel at `(x, y)` is `texel(x, y)`
/// and returns each plane's samples, as codes.
fn packed(
    width: usize,
    height: usize,
    texel: impl Fn(usize, usize) -> [f32; 4] + Sync,
    encoding: YuvEncoding,
    layout: PixelLayout,
) -> Vec<Vec<u16>> {
    let row = |y: usize, out: &mut [[f32; 4]]| {
        for (x, value) in out.iter_mut().enumerate() {
            *value = texel(x, y);
        }
    };
    let picture = Picture {
        width,
        height,
        row: &row,
    };
    let planes = layout.planes(
        u32::try_from(width).expect("small"),
        u32::try_from(height).expect("small"),
    );
    let mut memory: Vec<Vec<u8>> = planes
        .iter()
        .map(|&(plane_width, rows, bytes)| vec![0; (plane_width * rows * bytes) as usize])
        .collect();
    let mut targets: Vec<PlaneTarget<'_>> = memory
        .iter_mut()
        .zip(&planes)
        .map(|(bytes, &(plane_width, _, size))| {
            PlaneTarget::new(bytes, (plane_width * size) as usize)
        })
        .collect();
    pack(&picture, encoding, layout, &mut targets).expect("packs");
    memory
        .iter()
        .zip(&planes)
        .map(|(bytes, &(_, _, size))| {
            if size == 1 {
                bytes.iter().map(|&code| u16::from(code)).collect()
            } else {
                bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| u16::from_le_bytes(*pair))
                    .collect()
            }
        })
        .collect()
}

#[test]
fn alpha_pack_weights_chroma_by_alpha() {
    // A 2x2 block: the top row transparent red, the bottom row opaque blue. A plain average
    // of straight color would mix in the red nobody sees; weighed by alpha, the chroma is blue's.
    let picture = |_: usize, y: usize| {
        if y == 0 {
            [0.0, 0.0, 0.0, 0.0]
        } else {
            [0.0, 0.0, 1.0, 1.0]
        }
    };
    let planes = packed(
        2,
        2,
        picture,
        CENTRED,
        layout(Chroma::Yuv420, BitDepth::Eight, true),
    );
    let blue = packed(
        2,
        2,
        |_, _| [0.0, 0.0, 1.0, 1.0],
        CENTRED,
        layout(Chroma::Yuv420, BitDepth::Eight, true),
    );
    assert_eq!(planes[1], blue[1], "Cb");
    assert_eq!(planes[2], blue[2], "Cr");
    assert_eq!(planes[3], [0, 0, 255, 255], "alpha");
    // A block nobody sees has neutral chroma.
    let clear = packed(
        2,
        2,
        |_, _| [0.0; 4],
        CENTRED,
        layout(Chroma::Yuv420, BitDepth::Eight, true),
    );
    assert_eq!((clear[1][0], clear[2][0]), (128, 128));
}

#[test]
fn straight_color_is_written_under_alpha() {
    // Half-transparent white, premultiplied: its luma is white's, its alpha half.
    let planes = packed(
        2,
        2,
        |_, _| [0.5, 0.5, 0.5, 0.5],
        CENTRED,
        layout(Chroma::Yuv444, BitDepth::Ten, true),
    );
    assert!(planes[0].iter().all(|&code| code == 940), "{:?}", planes[0]);
    assert!(planes[3].iter().all(|&code| code == 512), "{:?}", planes[3]);
}

#[test]
fn ten_bit_codes_cover_the_limited_range() {
    // A ramp from black to white over 1024 columns: luma climbs from 64 to 940 without a step
    // back, and reaches every code in between.
    let width = 1024;
    let planes = packed(
        width,
        2,
        |x, _| {
            #[allow(clippy::cast_precision_loss)]
            let value = x as f32 / (width - 1) as f32;
            [value, value, value, 1.0]
        },
        CENTRED,
        layout(Chroma::Yuv420, BitDepth::Ten, false),
    );
    let luma = &planes[0][..width];
    assert_eq!((luma[0], luma[width - 1]), (64, 940));
    assert!(luma.windows(2).all(|pair| pair[0] <= pair[1]));
    let distinct = luma.windows(2).filter(|pair| pair[0] != pair[1]).count() + 1;
    assert_eq!(distinct, 877);
    // Grey chroma is the centre code.
    assert!(planes[1].iter().all(|&code| code == 512));
}

#[test]
fn p010_is_the_planar_code_shifted() {
    let picture = |x: usize, y: usize| {
        #[allow(clippy::cast_precision_loss)]
        let value = (x + 3 * y) as f32 / 16.0;
        [value, 1.0 - value, 0.25, 1.0]
    };
    let planar_layout = layout(Chroma::Yuv420, BitDepth::Ten, false);
    let planar = packed(8, 4, picture, CENTRED, planar_layout);
    let p010 = packed(8, 4, picture, CENTRED, planar_layout.as_p010());
    assert_eq!(
        p010[0],
        planar[0].iter().map(|code| code << 6).collect::<Vec<_>>()
    );
    let interleaved: Vec<u16> = planar[1]
        .iter()
        .zip(&planar[2])
        .flat_map(|(u, v)| [u << 6, v << 6])
        .collect();
    assert_eq!(p010[1], interleaved);
}

#[test]
fn four_four_four_keeps_every_pixel_s_chroma() {
    let picture = |x: usize, _: usize| {
        if x == 0 {
            [1.0, 0.0, 0.0, 1.0]
        } else {
            [0.0, 0.0, 1.0, 1.0]
        }
    };
    let full = packed(
        2,
        1,
        picture,
        CENTRED,
        layout(Chroma::Yuv444, BitDepth::Ten, false),
    );
    // Red has Cr above the centre and blue Cb above it: no averaging between them.
    assert!(full[2][0] > 512 && full[1][1] > 512);
    let shared = packed(
        2,
        1,
        picture,
        CENTRED,
        layout(Chroma::Yuv422, BitDepth::Ten, false),
    );
    assert_eq!(shared[1].len(), 1);
}

#[test]
fn ten_bit_luma_is_eight_bit_luma_four_times_finer() {
    let picture = |x: usize, y: usize| {
        #[allow(clippy::cast_precision_loss)]
        let value = ((x * 7 + y * 13) % 64) as f32 / 63.0;
        [value, value * 0.5, 1.0 - value, 1.0]
    };
    let eight = packed(
        16,
        4,
        picture,
        CENTRED,
        layout(Chroma::Yuv420, BitDepth::Eight, false),
    );
    let ten = packed(
        16,
        4,
        picture,
        CENTRED,
        layout(Chroma::Yuv420, BitDepth::Ten, false),
    );
    for (eight, ten) in eight[0].iter().zip(&ten[0]) {
        assert!(
            (i32::from(*eight) * 4 - i32::from(*ten)).abs() <= 2,
            "{eight} {ten}"
        );
    }
}

#[test]
fn a_size_the_chroma_cannot_halve_is_refused() {
    let row = |_: usize, out: &mut [[f32; 4]]| out.fill([0.0; 4]);
    let picture = Picture {
        width: 3,
        height: 2,
        row: &row,
    };
    let mut memory = [vec![0_u8; 64], vec![0; 64], vec![0; 64]];
    let [y, u, v] = &mut memory;
    let mut targets = [
        PlaneTarget::new(y, 8),
        PlaneTarget::new(u, 8),
        PlaneTarget::new(v, 8),
    ];
    assert!(
        pack(
            &picture,
            CENTRED,
            layout(Chroma::Yuv422, BitDepth::Ten, false),
            &mut targets
        )
        .is_err()
    );
}

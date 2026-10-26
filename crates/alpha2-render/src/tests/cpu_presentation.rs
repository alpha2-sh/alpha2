//! Source presentation on the CPU against the GPU's: decoded YCbCr and RGBA
//! frames converted into the canvas picture the composite samples.

#![allow(clippy::cast_possible_truncation)]

use std::sync::Arc;

use alpha2_media::{
    ChromaLocation, ColorMatrix, ColorMetadata, ColorPrimaries, ColorRange, DynamicRange,
    Orientation, Rect, Rotation, TransferFunction, YuvFormat,
};

use super::cpu_parity::{Parity, Tolerance, assert_close};
use crate::{
    BlendMode, ColorPolicy, GraphLayer, LayerTransform, RenderGraph, RenderSize, SourceAlpha,
    SourcePlane, SourcePresentation, SourceView, YuvFrame, YuvPlanes,
};

/// Planes a test owns.
#[derive(Debug)]
struct OwnedPlanes {
    format: YuvFormat,
    planes: Vec<(Vec<u8>, u32)>,
    alpha: Option<(Vec<u8>, u32)>,
    size: RenderSize,
    color: ColorMetadata,
}

impl YuvPlanes for OwnedPlanes {
    fn frame(&self) -> YuvFrame<'_> {
        fn plane((bytes, stride): &(Vec<u8>, u32)) -> SourcePlane<'_> {
            SourcePlane {
                bytes,
                stride: *stride,
            }
        }
        let empty = SourcePlane {
            bytes: &[],
            stride: 0,
        };
        YuvFrame {
            format: self.format,
            planes: [
                plane(&self.planes[0]),
                plane(&self.planes[1]),
                self.planes.get(2).map_or(empty, plane),
            ],
            alpha_plane: self.alpha.as_ref().map(plane),
            size: self.size,
            alpha: 1.0,
            color: Some(self.color),
        }
    }
}

/// A frame of `format` whose planes hold smooth ramps and noise.
fn frame(format: YuvFormat, size: RenderSize, color: ColorMetadata) -> OwnedPlanes {
    let bytes = format.bytes_per_sample() as usize;
    let maximum = (1_u32 << format.bit_depth) - 1;
    let shift = if format.msb_aligned {
        16 - u32::from(format.bit_depth)
    } else {
        0
    };
    let mut state = 0x9e37_79b9_u32;
    let mut make = |width: u32, height: u32, components: u32, phase: u32| {
        let stride = width * components * bytes as u32 + 8 * bytes as u32;
        let mut data = vec![0_u8; (stride * height) as usize];
        for y in 0..height {
            for x in 0..width {
                for component in 0..components {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let ramp =
                        (x + y * 2 + phase + component * 37) * maximum / (width + height * 2 + 80);
                    let value = if x % 7 == 3 {
                        (state >> 8) % (maximum + 1)
                    } else {
                        ramp
                    };
                    let offset =
                        (y * stride + (x * components + component) * bytes as u32) as usize;
                    if bytes == 1 {
                        data[offset] = value as u8;
                    } else {
                        data[offset..offset + 2]
                            .copy_from_slice(&((value << shift) as u16).to_le_bytes());
                    }
                }
            }
        }
        (data, stride)
    };
    let (chroma_width, chroma_height) = format.chroma_size(size.width, size.height);
    let mut planes = vec![make(size.width, size.height, 1, 0)];
    if format.interleaved_chroma {
        planes.push(make(chroma_width, chroma_height, 2, 50));
    } else {
        planes.push(make(chroma_width, chroma_height, 1, 50));
        planes.push(make(chroma_width, chroma_height, 1, 90));
    }
    let alpha = format.alpha.then(|| make(size.width, size.height, 1, 20));
    OwnedPlanes {
        format,
        planes,
        alpha,
        size,
        color,
    }
}

fn sdr(matrix: ColorMatrix, range: ColorRange, primaries: ColorPrimaries) -> ColorMetadata {
    ColorMetadata {
        primaries,
        transfer: TransferFunction::Bt709,
        matrix,
        range,
        dynamic_range: DynamicRange::Sdr,
    }
}

fn presentation(source: RenderSize, canvas: RenderSize) -> SourcePresentation {
    SourcePresentation {
        output_size: canvas,
        ..SourcePresentation::identity(source)
    }
}

/// `view` composited once at the identity: the picture as the composite
/// samples it.
fn identity_graph(view: SourceView<'_>, canvas: RenderSize) -> RenderGraph<'_> {
    let mut graph = RenderGraph::new(canvas);
    let node = graph.add_source(view);
    let output = graph
        .add_composite(vec![GraphLayer {
            source: node,
            visual: LayerTransform {
                position: [0.5, 0.5],
                anchor: [0.5, 0.5],
                scale: [1.0, 1.0],
                rotation_degrees: 0.0,
                opacity: 1.0,
            },
            blend_mode: BlendMode::Normal,
        }])
        .expect("composite");
    graph.set_output(output).expect("output");
    graph
}

/// Draws a GPU texture and a CPU source through [`identity_graph`] and
/// compares them.
fn compare(
    parity: &Parity,
    name: &str,
    canvas: RenderSize,
    tolerance: Tolerance,
    texture: &crate::RenderedTexture,
    source: &crate::CpuSource,
) {
    let gpu_graph = identity_graph(SourceView::from(texture), canvas);
    let gpu = parity
        .gpu
        .read_rgba8(&parity.gpu.execute_graph(&gpu_graph).expect("GPU draws"))
        .expect("readback");
    let cpu_graph = identity_graph(SourceView::cpu(source), canvas);
    let cpu = parity
        .cpu
        .execute_graph(&cpu_graph)
        .expect("CPU draws")
        .read_rgba8();
    assert_close(name, canvas, parity.filtered(tolerance), &gpu, &cpu);
}

/// Presents `planes` on both renderers and compares the pictures.
fn check_yuv(
    parity: &Parity,
    name: &str,
    planes: OwnedPlanes,
    presentation: SourcePresentation,
    policy: ColorPolicy,
    tolerance: Tolerance,
) {
    let (prepared, _) = parity
        .gpu
        .prepare_yuv_presented(planes.frame(), presentation, policy)
        .expect("GPU presents");
    let (texture, pending) = prepared.into_parts();
    if let Some(pending) = &pending {
        assert!(parity.gpu.submit_pending_source(pending));
    }
    let (source, _) = parity
        .cpu
        .present_yuv(Arc::new(planes), presentation, policy)
        .expect("CPU presents");
    compare(
        parity,
        name,
        presentation.output_size,
        tolerance,
        &texture,
        &source,
    );
}

fn sized(width: u32, height: u32) -> RenderSize {
    RenderSize::new(width, height).expect("size")
}

#[test]
fn sdr_formats_present_alike() {
    let parity = Parity::new();
    let bt709 = sdr(
        ColorMatrix::Bt709,
        ColorRange::Limited,
        ColorPrimaries::Bt709,
    );
    let cases = [
        ("i420", YuvFormat::I420, bt709),
        ("nv12", YuvFormat::NV12, bt709),
        ("p010", YuvFormat::P010, bt709),
        ("yuv420p10", YuvFormat::planar(1, 1, 10), bt709),
        ("yuv444p", YuvFormat::planar(0, 0, 8), bt709),
        ("yuv422p", YuvFormat::planar(1, 0, 8), bt709),
        (
            "bt601-full",
            YuvFormat::I420,
            sdr(ColorMatrix::Bt601, ColorRange::Full, ColorPrimaries::Bt709),
        ),
        (
            "bt2020-sdr",
            YuvFormat::planar(1, 1, 10),
            sdr(
                ColorMatrix::Bt2020Ncl,
                ColorRange::Limited,
                ColorPrimaries::Bt2020,
            ),
        ),
    ];
    let size = sized(64, 48);
    for (name, format, color) in cases {
        check_yuv(
            &parity,
            &format!("present-{name}"),
            frame(format, size, color),
            presentation(size, size),
            ColorPolicy::RejectHdr,
            Tolerance::Close,
        );
    }
}

#[test]
fn placement_presents_alike() {
    let parity = Parity::new();
    let bt709 = sdr(
        ColorMatrix::Bt709,
        ColorRange::Limited,
        ColorPrimaries::Bt709,
    );
    let source = sized(80, 48);
    let large = sized(160, 96);
    let canvas = sized(96, 64);
    let cases = [
        ("contain", source, presentation(source, canvas)),
        ("shrink", source, presentation(source, sized(30, 22))),
        ("shrink-far", large, presentation(large, sized(18, 12))),
        (
            "turned-mirrored",
            source,
            SourcePresentation {
                orientation: Orientation {
                    rotation: Rotation::Deg90,
                    mirror: true,
                },
                ..presentation(source, canvas)
            },
        ),
        (
            "half-turn",
            source,
            SourcePresentation {
                orientation: Orientation {
                    rotation: Rotation::Deg180,
                    mirror: false,
                },
                ..presentation(source, canvas)
            },
        ),
        (
            "cropped-anamorphic",
            source,
            SourcePresentation {
                visible_rect: Rect {
                    x: 6,
                    y: 4,
                    width: 60,
                    height: 36,
                },
                sample_aspect_ratio: (4, 3),
                chroma_siting: ChromaLocation::Left,
                ..presentation(source, canvas)
            },
        ),
    ];
    for (name, frame_size, presentation) in cases {
        check_yuv(
            &parity,
            &format!("place-{name}"),
            frame(YuvFormat::I420, frame_size, bt709),
            presentation,
            ColorPolicy::RejectHdr,
            Tolerance::Close,
        );
    }
}

#[test]
fn alpha_planes_and_hdr_present_alike() {
    let parity = Parity::new();
    let size = sized(64, 48);
    let bt709 = sdr(ColorMatrix::Bt709, ColorRange::Full, ColorPrimaries::Bt709);
    check_yuv(
        &parity,
        "present-alpha",
        frame(YuvFormat::planar(0, 0, 16).with_alpha(), size, bt709),
        presentation(size, sized(80, 50)),
        ColorPolicy::RejectHdr,
        Tolerance::Close,
    );
    for transfer in [TransferFunction::Pq, TransferFunction::Hlg] {
        let color = ColorMetadata {
            primaries: ColorPrimaries::Bt2020,
            transfer,
            matrix: ColorMatrix::Bt2020Ncl,
            range: ColorRange::Limited,
            dynamic_range: DynamicRange::Hdr,
        };
        check_yuv(
            &parity,
            &format!("present-{transfer:?}"),
            frame(YuvFormat::P010, size, color),
            presentation(size, size),
            ColorPolicy::ToneMapHdr(crate::hdr::HdrToneMap::default()),
            Tolerance::Edges,
        );
    }
}

#[test]
fn rgba_sources_present_alike() {
    let parity = Parity::new();
    let size = sized(60, 40);
    let canvas = sized(96, 64);
    let card = super::cpu_parity::test_card(size, false);
    // Straight alpha: undo the card's premultiplication.
    let straight = card
        .chunks(4)
        .flat_map(|pixel| {
            let alpha = u16::from(pixel[3]).max(1);
            #[allow(clippy::cast_possible_truncation)]
            let channel = |value: u8| ((u16::from(value) * 255 + alpha / 2) / alpha).min(255) as u8;
            [
                channel(pixel[0]),
                channel(pixel[1]),
                channel(pixel[2]),
                pixel[3],
            ]
        })
        .collect::<Vec<u8>>();
    for (name, pixels, alpha) in [
        ("straight", straight, SourceAlpha::Straight),
        ("premultiplied", card, SourceAlpha::Premultiplied),
    ] {
        let presentation = presentation(size, canvas);
        let upload = parity.gpu.upload_rgba8(size, &pixels).expect("upload");
        let prepared = parity
            .gpu
            .prepare_rgba_presented(upload, size, presentation, alpha)
            .expect("GPU presents");
        let (texture, pending) = prepared.into_parts();
        if let Some(pending) = &pending {
            assert!(parity.gpu.submit_pending_source(pending));
        }
        let source = parity
            .cpu
            .present_rgba8(Arc::new(pixels), size, 0, presentation, alpha)
            .expect("CPU presents");
        compare(
            &parity,
            &format!("rgba-{name}"),
            canvas,
            Tolerance::Close,
            &texture,
            &source,
        );
    }
}

use super::*;

use alpha2_media::{VideoCodec, YuvEncoding};

/// A 16x4 picture of flat luma whose blue-difference chroma steps from 104 to
/// 152, all in gamut, between chroma columns 3 and 4, imported with its chroma
/// at `siting` and exported again with the chroma at `siting`: the exported U
/// row.
fn chroma_edge_round_trip(renderer: &Renderer, siting: ChromaLocation) -> Vec<u8> {
    let size = RenderSize::new(16, 4).expect("size");
    let y = [126_u8; 64];
    let u = [[104_u8; 4], [152; 4]].concat().repeat(2);
    let v = [128_u8; 16];
    let (target, _) = renderer
        .render_yuv420p_presented(
            Yuv420Frame {
                y: &y,
                u: &u,
                v: &v,
                y_stride: 16,
                chroma_stride: 8,
                size,
                alpha: 1.0,
                color: Some(ColorMetadata::EXPORT),
            },
            SourcePresentation {
                chroma_siting: siting,
                ..SourcePresentation::identity(size)
            },
            crate::ColorPolicy::RejectHdr,
        )
        .expect("the picture imports");
    let planes = renderer
        .begin_yuv420p_readback(&target, YuvEncoding::new(ColorMetadata::EXPORT, siting))
        .expect("the readback submits")
        .wait()
        .expect("the readback maps");
    planes.u[..8].to_vec()
}

#[test]
fn a_chroma_edge_stays_where_the_codec_sites_it_through_import_and_export() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    for siting in [ChromaLocation::Left, ChromaLocation::Center] {
        let row = chroma_edge_round_trip(&renderer, siting);
        // Upsampling and the pack's low-pass soften the edge, evenly on both sides: it
        // does not move.
        let below = i16::from(row[3]) - 104;
        let above = 152 - i16::from(row[4]);
        assert!(
            (below - above).abs() <= 2,
            "{siting:?}: the edge moved, {row:?} (+{below} below, -{above} above)"
        );
        for (index, expected) in [(0, 104), (1, 104), (6, 152), (7, 152)] {
            assert!(
                row[index].abs_diff(expected) <= 1,
                "{siting:?}: chroma column {index} is {}, not {expected}: {row:?}",
                row[index]
            );
        }
    }
}

#[test]
fn exports_pack_chroma_where_their_codec_implies_it() {
    for (codec, siting) in [
        (VideoCodec::H264, ChromaLocation::Left),
        (VideoCodec::Hevc, ChromaLocation::Left),
        (VideoCodec::Mpeg4Part2, ChromaLocation::Left),
        (VideoCodec::Vp9, ChromaLocation::Center),
        (VideoCodec::Av1, ChromaLocation::Center),
    ] {
        assert_eq!(
            YuvEncoding::export(codec),
            YuvEncoding::new(ColorMetadata::EXPORT, siting),
            "{codec:?}"
        );
    }
}

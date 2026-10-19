use super::*;

#[test]
fn the_staging_corpus_is_what_a_readback_stages_and_packs() {
    // @alpha2-sh/editor's estimatedStagingBytes reads the same file, so a layout change here fails
    // there until the browser's budget follows.
    let corpus: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/cases/staging-bytes-corpus.json"
    ))
    .expect("the staging corpus parses");
    let sizes = corpus["sizes"].as_array().expect("sizes");
    assert!(sizes.len() >= 8);
    let field = |size: &serde_json::Value, name: &str| {
        size[name]
            .as_u64()
            .unwrap_or_else(|| panic!("{name} is a whole number"))
    };
    for size in sizes {
        let (width, height) = (field(size, "width"), field(size, "height"));
        let render = RenderSize::new(
            u32::try_from(width).expect("width"),
            u32::try_from(height).expect("height"),
        )
        .expect("size");
        assert_eq!(
            yuv420p_staging_bytes(render).expect("staging"),
            field(size, "stagingBytes"),
            "{width}x{height}"
        );
        assert_eq!(
            Yuv420StagingLayout::new(render).expect("layout").bytes,
            field(size, "packedBytes"),
            "{width}x{height}"
        );
    }
}

#[test]
fn every_plane_starts_on_a_placement_boundary_in_its_own_region() {
    for (width, height) in [
        (8, 2),
        (64, 16),
        (200, 16),
        (1280, 720),
        (1920, 1080),
        (3840, 2160),
    ] {
        let size = RenderSize::new(width, height).expect("size");
        let layout = Yuv420StagingLayout::new(size).expect("layout");
        let planes = layout.planes(size);
        let mut end = 0;
        for (offset, pitch, plane_width, rows) in planes {
            assert_eq!(offset % PLANE_PLACEMENT_ALIGNMENT, 0, "{width}x{height}");
            assert_eq!(pitch % wgpu::COPY_BYTES_PER_ROW_ALIGNMENT, 0);
            assert!(plane_width <= pitch);
            assert!(offset >= end, "{width}x{height}: planes overlap");
            end = offset + u64::from(pitch) * u64::from(rows);
        }
        assert_eq!(end, layout.bytes);
    }
    // At 1920 wide each chroma row pads to 1024 bytes, so the two chroma planes
    // together take one 2048-byte luma pitch per chroma row and need no padding.
    let hd = Yuv420StagingLayout::new(RenderSize::new(1920, 1080).expect("size")).expect("layout");
    assert_eq!(hd.bytes, 2048 * (1080 + 540));
}

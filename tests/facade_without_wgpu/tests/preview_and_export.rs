//! The facade's default API is enough for a whole render: load a project,
//! open a device, lend a source frame through [`FrameProvider`], render a
//! preview frame and read it back, and export every frame as YUV planes. This
//! crate does not depend on `wgpu` and does not turn `wgpu-interop` on, so
//! none of it can name a `wgpu` type.
#![cfg(not(target_arch = "wasm32"))]

use alpha2::media::{SourceId, VideoCodec};
use alpha2::render::UploadTexture;
use alpha2::{
    EngineProject, ExportStep, Exporter, FrameProvider, GpuContext, MediaTime, Player, Project,
    QualityProfile, RenderSize, Renderer, SourceView,
};

/// One second of one image clip on one video track, 1280x720 at 30 fps.
const PROJECT: &str = include_str!("../../cases/builder-tutorial-project.json");
/// The image's pixels: opaque mid gray, which every RGB-to-YUV matrix maps
/// to neutral chroma.
const GRAY: [u8; 4] = [128, 128, 128, 255];

/// The host's decoded image, lent to the renderer as a source.
struct Image {
    id: SourceId,
    pixels: UploadTexture,
}

impl FrameProvider for Image {
    fn texture_for(&self, source: &SourceId, _time: MediaTime) -> Option<SourceView<'_>> {
        (source == &self.id).then(|| SourceView::from(&self.pixels))
    }
}

#[test]
fn a_project_renders_a_preview_frame_and_exports_through_the_facade_alone() {
    let project = EngineProject::new(Project::from_json(PROJECT).expect("the project loads"))
        .expect("the project evaluates");
    let gpu = pollster::block_on(GpuContext::request_product()).expect("a GPU device");
    let renderer = Renderer::new(gpu);
    let size = RenderSize::new(1280, 720).expect("a size");
    let image = Image {
        id: SourceId::new("image-1").expect("a source id"),
        pixels: renderer
            .upload_rgba8(size, &GRAY.repeat(1280 * 720))
            .expect("the image uploads"),
    };

    let mut player = Player::new(QualityProfile::preview()).expect("a player");
    let frame = player
        .render_at(&project, &renderer, &image, MediaTime::ZERO)
        .expect("the preview frame renders");
    let texture = frame.texture.expect("every source arrived");
    let pixels = renderer.read_rgba8(texture).expect("the frame reads back");
    let RenderSize { width, height } = texture.size();
    assert_eq!(pixels.len(), 4 * width as usize * height as usize);
    assert!(
        pixels.as_chunks::<4>().0.iter().all(|pixel| *pixel == GRAY),
        "the preview frame is the image"
    );

    let mut exporter = Exporter::for_project(&project, QualityProfile::export(), VideoCodec::H264)
        .expect("an exporter");
    let mut frames = 0;
    loop {
        match exporter
            .render_next(&project, &renderer, &image)
            .expect("the frame renders")
        {
            ExportStep::Rendered { readback, .. } => {
                let planes = readback.wait().expect("the frame reads back");
                assert_eq!(planes.y.len(), 1280 * 720);
                assert!(planes.y.iter().all(|luma| *luma == planes.y[0]));
                assert!(
                    planes
                        .u
                        .iter()
                        .chain(&planes.v)
                        .all(|chroma| chroma.abs_diff(128) <= 1),
                    "gray has neutral chroma"
                );
                frames += 1;
            }
            ExportStep::Waiting {
                missing_sources, ..
            } => {
                panic!("the image is always there: {missing_sources:?}")
            }
            ExportStep::Complete => break,
        }
    }
    assert_eq!(frames, 30);
}

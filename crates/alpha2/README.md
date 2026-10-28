# alpha2

The alpha2 media engine for Rust programs. It renders a timeline project (a JSON document of sources, tracks, clips, effects and transitions) on the GPU through wgpu, for preview frames and for export, and mixes its audio. The program decodes its media and encodes the output; the engine renders what the project shows from the frames the program lends it.

This crate is the facade over the engine's crates. Its modules are those crates: `audio`, `engine` (playback and export), `gpu`, `media`, `model` (the project document), `render` and `time`; the types most programs use are also at the top level. The [alpha2 README](https://github.com/alpha2-sh/alpha2#readme) explains the project document and the other ways to use the engine.

## A first render

The project is one second of one image at 1280x720 and 30 fps ([`project.json`](https://github.com/alpha2-sh/alpha2/blob/main/tests/cases/builder-tutorial-project.json)). With `alpha2` and `pollster` as dependencies:

```rust
use alpha2::media::{SourceId, VideoCodec};
use alpha2::render::UploadTexture;
use alpha2::{
    EngineProject, ExportStep, Exporter, FrameProvider, GpuContext, MediaTime, Player, Project,
    QualityProfile, RenderSize, Renderer, SourceView,
};

/// The application's decoded image, lent to the renderer for source `image-1`.
struct Image {
    id: SourceId,
    pixels: UploadTexture,
}

impl FrameProvider for Image {
    fn texture_for(&self, source: &SourceId, _time: MediaTime) -> Option<SourceView<'_>> {
        (source == &self.id).then(|| SourceView::from(&self.pixels))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let project = Project::from_json(&std::fs::read_to_string("project.json")?)?;
    let project = EngineProject::new(project)?;
    let gpu = pollster::block_on(GpuContext::request_high_performance())?;
    let mut renderer = Renderer::new(gpu);
    // The built-in effects and transitions a project can name.
    renderer.register_builtins().expect("the built-in definitions register");

    // Opaque mid gray, premultiplied sRGB RGBA8, at the project's size.
    let size = RenderSize::new(1280, 720)?;
    let gray = [128, 128, 128, 255].repeat(1280 * 720);
    let image = Image {
        id: SourceId::new("image-1")?,
        pixels: renderer.upload_rgba8(size, &gray)?,
    };

    // One preview frame, read back as RGBA8.
    let mut player = Player::new(QualityProfile::preview())?;
    let frame = player.render_at(&project, &renderer, &image, MediaTime::ZERO)?;
    if let Some(texture) = frame.texture {
        let rgba = renderer.read_rgba8(texture)?;
        println!("preview: {} bytes", rgba.len());
    }

    // Every frame of the export, as YUV 4:2:0 planes for an encoder.
    let mut exporter = Exporter::for_project(&project, QualityProfile::export(), VideoCodec::H264)?;
    loop {
        match exporter.render_next(&project, &renderer, &image)? {
            ExportStep::Rendered { readback, .. } => {
                let planes = readback.wait()?;
                // planes.y, planes.u, planes.v: hand them to an encoder.
                println!("frame: Y {} U {} V {} bytes", planes.y.len(), planes.u.len(), planes.v.len());
            }
            ExportStep::Waiting { missing_sources, .. } => {
                return Err(format!("no frame for {missing_sources:?}").into());
            }
            ExportStep::Complete => break,
        }
    }
    Ok(())
}
```

- `Project::from_json` reads and checks a document; `EngineProject` is an immutable snapshot of it, indexed for evaluation, which the player and the exporter render.
- `GpuContext::request_high_performance()` opens a device, preferring a discrete, high-performance adapter. When it fails, `GpuError::step()` says whether no adapter was found or the adapter refused a device (`GpuAcquisitionStep`), and its `source()` gives the platform's reason.
- A `FrameProvider` lends the renderer a frame for each source at the exact source time the engine asks for. `Renderer::upload_rgba8` makes one from pixels.
- `register_builtins` registers the built-in effects and transitions, each compiled the first time a frame draws it or `warmup` names it; a frame that draws an effect the renderer has not registered fails.
- `Player::render_at` renders the frame shown at a time. When a source frame is missing, it names the source (`missing_sources`) and, by the player's missing-frame policy, shows the last frame again (`repeated`) or none.
- `Exporter` walks the project's frame grid at export quality. `ExportStep::Waiting` names the sources the frame lacked: supply them and call `render_next` again, and the export continues at the same frame.

## Decoding and encoding

The engine does not read or write media files. [`alpha2-native`](https://crates.io/crates/alpha2-native) decodes, probes and encodes with FFmpeg 8 and links the FFmpeg installed where it is built; [`alpha2-host`](https://github.com/alpha2-sh/alpha2/blob/main/docs/native.md) is a complete program that exports projects to MP4 with it.

## Effects

The built-in effects and transitions are listed, with their parameters, by `alpha2::render::builtin_catalog()`. Chroma keying (`alpha2.effect.chroma-key`) and 3D LUTs (`alpha2.effect.lut-3d`, with a `.cube` file registered through `Renderer::register_lut3d`) are built-in effects a project applies like any other.

## `wgpu` interop

The API names no `wgpu` type. The `wgpu-interop` feature adds the items that do, for a program that shares its own wgpu device or textures with the engine: adopting a device (`GpuContext::from_parts`), lending a `wgpu::TextureView` as a source (`SourceView::from_wgpu`), taking the `wgpu` texture of a rendered frame, rendering into a hardware encoder's planes. With it, `alpha2::gpu::wgpu` is the `wgpu` crate the engine is built on, so the program names wgpu types through that path rather than its own `wgpu` dependency. The items behind this feature follow `wgpu`'s major version; the rest of the API does not. The documentation on docs.rs is built without the feature and does not show them.

## License

MIT, copyright Alpha2 contributors.

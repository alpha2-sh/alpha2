//! Stills: frames of a project rendered to PNG as an export renders them.

use super::*;
use alpha2_engine::{EngineProject, FramesWithText, Player, QualityProfile};

/// The complex parity project (a video, a still, a transition, compositions
/// with effects, masks, blend modes, shapes and a fill) with a title over it.
fn titled_project() -> Value {
    let mut project: Value = serde_json::from_str(include_str!(
        "../../../../tests/cases/host-parity-complex-project.json"
    ))
    .expect("complex project");
    let style = json!({"font_size": 0.25, "position": {"x": 0.5, "y": 0.8}, "anchor": {"x": 0.5, "y": 0.5}});
    project["sources"]
        .as_array_mut()
        .expect("sources")
        .push(json!({"id": "title", "kind": "generated",
            "generator": {"type": "text", "text": "Still", "style": style}}));
    let span = json!({"start": 0, "end": 180_000});
    project["timeline"]["tracks"]
        .as_array_mut()
        .expect("tracks")
        .push(json!({"id": "titles", "name": "Titles", "kind": "video", "clips": [{
            "id": "title-clip", "source_id": "title", "timeline_range": span, "source_range": span
        }]}));
    project
}

fn assets() -> HashMap<String, PathBuf> {
    HashMap::from([
        ("video".to_owned(), corpus("h264-bt709-copy.mp4")),
        ("still".to_owned(), corpus("cross_topology_parity.png")),
    ])
}

fn font() -> PathBuf {
    corpus("../fonts/Alpha2Fixture.ttf")
}

/// A still request of `project` for `stills` (time in ticks, output), with
/// `fields` added to it.
fn still_request(project: &Value, stills: &[(i64, &Path)], fields: &Value) -> Request {
    let mut request = json!({
        "protocolVersion": 1, "operation": "still", "requestId": "still",
        "project": project,
        "assets": assets(),
        "fonts": [font()],
        "stills": stills
            .iter()
            .map(|(time, output)| json!({"time": time, "output": output}))
            .collect::<Vec<_>>(),
    });
    for (key, value) in fields.as_object().expect("fields") {
        request[key] = value.clone();
    }
    parse_request(&request.to_string()).expect("a still request")
}

/// A PNG's size and RGBA8 pixels.
fn read_png(path: &Path) -> ((u32, u32), Vec<u8>) {
    let decoder = png::Decoder::new(std::io::BufReader::new(
        std::fs::File::open(path).expect("PNG"),
    ));
    let mut reader = decoder.read_info().expect("PNG header");
    let mut pixels = vec![0; reader.output_buffer_size().expect("PNG size")];
    let info = reader.next_frame(&mut pixels).expect("PNG pixels");
    assert_eq!(
        (info.color_type, info.bit_depth),
        (png::ColorType::Rgba, png::BitDepth::Eight)
    );
    pixels.truncate(info.buffer_size());
    ((info.width, info.height), pixels)
}

/// What [`Player::render_at`] draws of `project` at `time`, from its sources
/// decoded straight from their files and installed as an export installs
/// them, read back as RGBA8: the picture a still must equal, made without its
/// decode pool or its loop. The host's text store is the one the still's
/// request filled.
fn reference(host: &Host, project: &Value, time: MediaTime) -> Vec<u8> {
    let project: Project = serde_json::from_value(project.clone()).expect("project");
    let snapshot = EngineProject::new(project.clone()).expect("snapshot");
    let size = alpha2_render::RenderSize::new(project.settings.width, project.settings.height)
        .expect("size");
    let mut state = ExportDecodeState::new(&project, &assets()).expect("decode state");
    let needs = snapshot
        .frame_needs(time)
        .expect("needs")
        .into_iter()
        .filter(|need| {
            project
                .sources
                .iter()
                .any(|source| source.id == need.source_id && source.kind != MediaKind::Generated)
        })
        .collect();
    let decoded = state
        .decode_job(
            DecodeJob {
                generation: 0,
                index: 0,
                time,
                needs,
                reverse_runs: HashMap::new(),
            },
            &CancelToken::default(),
        )
        .expect("decodes");
    let mut texts = host.texts.lock().expect("texts");
    texts
        .prepare_at(host.renderer.text_upload(), &snapshot, time, size)
        .expect("texts");
    match &host.renderer {
        crate::renderer_backend::RendererBackend::Gpu(renderer) => {
            let mut frames = host.export_gpu_frames(size, &project);
            frames.install(renderer, decoded).expect("installs");
            let frames = FramesWithText::new(&frames, &texts);
            let mut player = Player::new(QualityProfile::export()).expect("player");
            let frame = player
                .render_at(&snapshot, &**renderer, &frames, time)
                .expect("renders");
            renderer
                .read_rgba8(frame.texture.expect("a picture"))
                .expect("reads back")
        }
        crate::renderer_backend::RendererBackend::Cpu(renderer) => {
            let mut frames = crate::cpu_frames::NativeCpuFrames::new(size);
            frames.install(renderer, decoded).expect("installs");
            let frames = FramesWithText::new(&frames, &texts);
            let mut player =
                Player::<std::sync::Arc<alpha2_render::CpuImage>>::new(QualityProfile::export())
                    .expect("player");
            let frame = player
                .render_at(&snapshot, &**renderer, &frames, time)
                .expect("renders");
            frame.texture.expect("a picture").read_rgba8()
        }
    }
}

/// A still is the frame an export draws: on the GPU and on the CPU, each
/// frame equals the engine's render of it byte for byte, whatever order the
/// stills are asked in, and a time between frames draws the grid frame
/// holding it.
#[test]
fn a_still_is_the_engines_frame_at_the_grid_time_that_holds_it() {
    let _gpu = gpu_test_lock();
    let project = titled_project();
    // The project runs at 24 fps: 5 000 ticks a frame. In the transition, on the badge, on the
    // card, and between two frames.
    let times = [100_000_i64, 40_000, 160_000, 62_499];
    let grid = [100_000_i64, 40_000, 160_000, 60_000];
    for renderer in [
        crate::config::RendererPolicy::Gpu,
        crate::config::RendererPolicy::Cpu,
    ] {
        let mut host =
            crate::create_host_with(VideoDecodePolicy::Software, renderer).expect("host");
        let directory = scratch_directory(&format!("stills-{renderer:?}"));
        let paths = (0..times.len())
            .map(|index| directory.join(format!("{index}.png")))
            .collect::<Vec<_>>();
        let asked = times
            .iter()
            .copied()
            .zip(paths.iter().map(PathBuf::as_path));
        let result = host
            .handle(
                still_request(&project, &asked.collect::<Vec<_>>(), &json!({})),
                &mut |_| Ok(()),
            )
            .expect("stills");
        let answers = result["stills"].as_array().expect("stills");
        assert_eq!(answers.len(), times.len());
        assert_eq!(
            result["renderer"],
            json!(format!("{renderer:?}").to_lowercase())
        );
        for (index, answer) in answers.iter().enumerate() {
            assert_eq!(answer["frameTime"], grid[index], "{answer}");
            assert_eq!(
                answer["publish"],
                json!({"method": "rename-no-replace", "stagedLocally": false, "retries": 0}),
                "{answer}"
            );
            assert_eq!(
                (answer["width"].clone(), answer["height"].clone()),
                (json!(96), json!(64))
            );
            let named = PathBuf::from(answer["output"].as_str().expect("output"));
            assert!(names_written_file(&named, &paths[index]), "{named:?}");
            let (size, pixels) = read_png(&paths[index]);
            assert_eq!(size, (96, 64));
            assert!(
                pixels.chunks(4).all(|pixel| pixel[3] == 255),
                "every frame is opaque"
            );
            let expected = reference(&host, &project, MediaTime::from_ticks(grid[index]));
            assert!(
                pixels == expected,
                "{renderer:?}: the still at {} differs from the engine's frame",
                grid[index]
            );
        }

        // In reverse order, to other files: the same pictures.
        let reversed = directory.join("reversed");
        std::fs::create_dir(&reversed).expect("directory");
        let again = (0..times.len())
            .rev()
            .map(|index| (times[index], reversed.join(format!("{index}.png"))))
            .collect::<Vec<_>>();
        host.handle(
            still_request(
                &project,
                &again
                    .iter()
                    .map(|(time, path)| (*time, path.as_path()))
                    .collect::<Vec<_>>(),
                &json!({}),
            ),
            &mut |_| Ok(()),
        )
        .expect("stills in reverse");
        for (index, path) in paths.iter().enumerate() {
            assert_eq!(
                std::fs::read(path).expect("in order"),
                std::fs::read(reversed.join(format!("{index}.png"))).expect("reversed"),
                "{renderer:?}: still {index}"
            );
        }
    }
}

/// A still takes any size, an odd one too, and draws the project at that
/// canvas size.
#[test]
fn a_still_renders_at_the_size_it_asks_for() {
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("stills-size");
    let output = directory.join("odd.png");
    let result = host
        .handle(
            still_request(
                &titled_project(),
                &[(40_000, &output)],
                &json!({"size": {"width": 47, "height": 31}}),
            ),
            &mut |_| Ok(()),
        )
        .expect("a still");
    assert_eq!(result["stills"][0]["width"], 47);
    let ((width, height), _) = read_png(&output);
    assert_eq!((width, height), (47, 31));
}

/// A refused still writes nothing: no file and no partial.
#[test]
fn a_refused_still_writes_no_file() {
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("stills-refused");
    let output = directory.join("refused.png");
    let still = directory.join("still.png");
    std::fs::copy(corpus("cross_topology_parity.png"), &still).expect("still");
    let refused = |host: &mut Host, request: Value| {
        let mut line = json!({
            "protocolVersion": 1, "operation": "still", "requestId": "still",
            "project": titled_project(),
            "assets": assets(),
            "fonts": [font()],
            "stills": [{"time": 0, "output": output}],
        });
        for (key, value) in request.as_object().expect("fields") {
            line[key] = value.clone();
        }
        let error = match parse_request(&line.to_string()) {
            Ok(request) => host.handle(request, &mut |_| Ok(())).expect_err("refused"),
            Err(error) => error,
        };
        let left = std::fs::read_dir(&*directory)
            .expect("directory")
            .map(|entry| entry.expect("entry").file_name())
            .filter(|name| name != "still.png")
            .collect::<Vec<_>>();
        assert!(left.is_empty(), "{error:?} left {left:?}");
        error
    };
    let error = refused(&mut host, json!({"size": {"width": 0, "height": 31}}));
    assert_eq!(error.code, "INVALID_INPUT", "{error:?}");
    let error = refused(
        &mut host,
        json!({"stills": [{"time": -1, "output": output}]}),
    );
    assert_eq!(error.code, "INVALID_INPUT", "{error:?}");
    let mut assets = assets();
    assets.insert("still".to_owned(), still.clone());
    let error = refused(
        &mut host,
        json!({"assets": assets, "stills": [{"time": 0, "output": still}], "overwrite": true}),
    );
    assert_eq!(error.code, "INVALID_REQUEST", "{error:?}");
    assert!(error.message.contains(r#"the asset "still""#), "{error:?}");
    // A frame that waits for a source no file supplies.
    let mut project = titled_project();
    project["timeline"]["tracks"][0]["clips"][0]["source_id"] = json!("still");
    let error = refused(&mut host, json!({"project": project, "assets": {}}));
    assert!(
        ["MISSING_INPUT", "CONFORM_FAILED"].contains(&error.code),
        "{error:?}"
    );
}

/// An existing output is replaced only when the request overwrites.
#[test]
fn a_still_replaces_a_file_only_when_asked() {
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("stills-overwrite");
    let output = directory.join("frame.png");
    std::fs::write(&output, b"old").expect("old");
    let error = host
        .handle(
            still_request(&titled_project(), &[(0, &output)], &json!({})),
            &mut |_| Ok(()),
        )
        .expect_err("refused");
    assert_eq!(error.code, "IO_ERROR", "{error:?}");
    assert_eq!(std::fs::read(&output).expect("old"), b"old");
    host.handle(
        still_request(
            &titled_project(),
            &[(0, &output)],
            &json!({"overwrite": true}),
        ),
        &mut |_| Ok(()),
    )
    .expect("replaced");
    assert_eq!(read_png(&output).0, (96, 64));
}

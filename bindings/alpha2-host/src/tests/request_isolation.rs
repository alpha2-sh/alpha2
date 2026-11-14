//! Requests that run at once on one host keep their own pictures: the renderer's result cache is
//! shared, so only each request's key scope separates them.

use super::requests::{SERVE_CHILD, ServeChild};
use super::*;

const RED: [u8; 4] = [255, 0, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const RENDERERS: [&str; 2] = ["gpu", "cpu"];

fn write_solid_png(path: &Path, rgba: [u8; 4]) {
    let file = std::fs::File::create(path).expect("png file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), 320, 180);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    let data: Vec<u8> = std::iter::repeat_n(rgba, 320 * 180).flatten().collect();
    writer.write_image_data(&data).expect("png data");
}

/// A 320x180 template whose one clip shows `source` from 0 to `end`.
fn one_clip_project(source: &str, kind: &str, end: i64) -> Value {
    json!({
        "schema_version": 1, "id": "template", "name": "Template",
        "settings": {"width": 320, "height": 180, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
        "sources": [{"id": source, "locator": format!("asset://{source}"), "kind": kind}],
        "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
            "id": "clip", "source_id": source,
            "timeline_range": {"start": 0, "end": end},
            "source_range": {"start": 0, "end": end}
        }]}]}
    })
}

fn export_request(id: &str, project: &Value, source: &str, file: &Path, output: &Path) -> Value {
    json!({
        "protocolVersion": 2, "operation": "export", "requestId": id,
        "project": project,
        "assets": {source: file},
        "startTicks": 0, "endTicks": project_end(project),
        "frameRate": {"num": 30, "den": 1},
        "includeAudio": false,
        "videoCodec": "mpeg4-part2",
        "output": output
    })
}

fn project_end(project: &Value) -> &Value {
    &project["timeline"]["tracks"][0]["clips"][0]["timeline_range"]["end"]
}

/// One letter per frame: `R` for red, `B` for blue, `?` otherwise.
fn colors_of(path: &Path) -> String {
    decoded_planes(path)
        .iter()
        .map(|planes| {
            let mean = |plane: &Vec<u8>| -> f64 {
                #[allow(clippy::cast_precision_loss)]
                let count = plane.len() as f64;
                plane.iter().map(|value| f64::from(*value)).sum::<f64>() / count
            };
            let (u, v) = (mean(&planes[1]), mean(&planes[2]));
            if v > 180.0 && u < 128.0 {
                'R'
            } else if u > 180.0 && v < 128.0 {
                'B'
            } else {
                '?'
            }
        })
        .collect()
}

fn png_color(bytes: &[u8]) -> char {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("PNG header");
    let mut pixels = vec![0; reader.output_buffer_size().expect("PNG size")];
    reader.next_frame(&mut pixels).expect("PNG pixels");
    match (pixels[0], pixels[2]) {
        (r, b) if r > 200 && b < 50 => 'R',
        (r, b) if b > 200 && r < 50 => 'B',
        _ => '?',
    }
}

fn serve_two_at_once(renderer: &str) -> ServeChild {
    let serving = ServeChild::start(
        SERVE_CHILD,
        &[
            ("ALPHA2_TEST_SERVE_ARGS", "--max-running 2"),
            ("ALPHA2_TEST_SLOW_ENCODE_MS", "60"),
            ("ALPHA2_RENDERER", renderer),
        ],
    );
    assert_eq!(serving.message(Duration::from_secs(60))["state"], "ready");
    serving
}

/// Writes `first`, waits for its first progress, then writes `second`.
fn start_overlapping(serving: &mut ServeChild, first: &Value, second: &Value) {
    serving.write(&first.to_string());
    let progress = serving.message(Duration::from_secs(120));
    assert_eq!(progress["requestId"], first["requestId"], "{progress}");
    serving.write(&second.to_string());
}

#[test]
fn one_scope_gives_one_key_and_two_scopes_give_two() {
    let source = SourceId::new("photo").expect("source ID");
    let (first, second) = (
        crate::frames::next_cache_scope(),
        crate::frames::next_cache_scope(),
    );
    let key = |scope| crate::frames::export_source_key(scope, &source, None, true);
    assert_eq!(key(first), key(first));
    assert_ne!(key(first), key(second));
    assert_eq!(
        crate::frames::export_source_key(first, &source, None, false),
        None
    );
}

#[test]
fn two_pipelined_exports_of_one_template_keep_their_own_photo() {
    let _gpu = gpu_test_lock();
    for renderer in RENDERERS {
        let directory = scratch_directory(&format!("isolation-exports-{renderer}"));
        let (red, blue) = (directory.join("red.png"), directory.join("blue.png"));
        write_solid_png(&red, RED);
        write_solid_png(&blue, BLUE);
        let project = one_clip_project("photo", "image", 2 * alpha2_time::TICKS_PER_SECOND);
        let (out_a, out_b) = (directory.join("a.mp4"), directory.join("b.mp4"));
        let mut serving = serve_two_at_once(renderer);
        start_overlapping(
            &mut serving,
            &export_request("A", &project, "photo", &red, &out_a),
            &export_request("B", &project, "photo", &blue, &out_b),
        );
        let answers: Vec<Value> =
            std::iter::repeat_with(|| serving.message(Duration::from_secs(120)))
                .filter(|message| message["type"] != "progress")
                .take(2)
                .collect();
        for answer in &answers {
            assert_eq!(answer["ok"], true, "{renderer}: {answer}");
        }
        assert_eq!(serving.finish(), Some(0));
        let (colors_a, colors_b) = (colors_of(&out_a), colors_of(&out_b));
        assert!(
            colors_a.chars().all(|c| c == 'R'),
            "{renderer} A: {colors_a}"
        );
        assert!(
            colors_b.chars().all(|c| c == 'B'),
            "{renderer} B: {colors_b}"
        );
    }
}

#[test]
fn a_still_beside_an_export_of_one_template_keeps_its_own_photo() {
    let _gpu = gpu_test_lock();
    for renderer in RENDERERS {
        let directory = scratch_directory(&format!("isolation-still-{renderer}"));
        let (red, blue) = (directory.join("red.png"), directory.join("blue.png"));
        write_solid_png(&red, RED);
        write_solid_png(&blue, BLUE);
        let project = one_clip_project("photo", "image", 2 * alpha2_time::TICKS_PER_SECOND);
        let out_a = directory.join("a.mp4");
        let still = json!({
            "protocolVersion": 2, "operation": "still", "requestId": "S",
            "project": project,
            "assets": {"photo": blue},
            "stills": [{"time": 0}],
        });
        let mut serving = serve_two_at_once(renderer);
        start_overlapping(
            &mut serving,
            &export_request("A", &project, "photo", &red, &out_a),
            &still,
        );
        let mut still_color = '?';
        for (answer, attachment) in
            std::iter::repeat_with(|| serving.message_with_attachments(Duration::from_secs(120)))
                .filter(|(message, _)| message["type"] != "progress")
                .take(2)
        {
            assert_eq!(answer["ok"], true, "{renderer}: {answer}");
            if answer["requestId"] == "S" {
                still_color = png_color(&attachment);
            }
        }
        assert_eq!(serving.finish(), Some(0));
        let colors_a = colors_of(&out_a);
        assert!(
            colors_a.chars().all(|c| c == 'R'),
            "{renderer} A: {colors_a}"
        );
        assert_eq!(
            still_color, 'B',
            "{renderer}: the still shows the export's photo"
        );
    }
}

/// An extraction of the template's timeline at the canvas size, beside an export of it.
#[test]
fn a_timeline_extraction_beside_an_export_of_one_template_sees_its_own_photo() {
    use alpha2_engine::extract::fixtures::{digest_descriptor, video_input};
    let _gpu = gpu_test_lock();
    for renderer in RENDERERS {
        let directory = scratch_directory(&format!("isolation-extract-{renderer}"));
        let (red, blue) = (directory.join("red.png"), directory.join("blue.png"));
        write_solid_png(&red, RED);
        write_solid_png(&blue, BLUE);
        let end = 2 * alpha2_time::TICKS_PER_SECOND;
        let project = one_clip_project("photo", "image", end);
        let out_a = directory.join("a.mp4");
        let descriptor = serde_json::to_value(digest_descriptor(video_input(
            alpha2_engine::extract::VideoFormat::Luma8,
            960,
        )))
        .expect("descriptor");
        let extract = json!({
            "protocolVersion": 2, "requestId": "X", "operation": "extract",
            "project": project,
            "assets": {"photo": blue},
            "descriptor": descriptor,
            "request": {
                "extractor": "alpha2.test.digest", "version": 1,
                "input": {"from": "timeline", "range": {"start": 0, "end": end}, "size": 320},
                "outputs": {"digest": "frames"}
            }
        });
        let mut serving = serve_two_at_once(renderer);
        start_overlapping(
            &mut serving,
            &export_request("A", &project, "photo", &red, &out_a),
            &extract,
        );
        let mut lumas = Vec::new();
        let mut answers = 0;
        while answers < 2 {
            let (message, attachment) = serving.message_with_attachments(Duration::from_secs(120));
            if message["type"] == "extract.step" {
                if message["step"] == "video" {
                    #[allow(clippy::cast_precision_loss)]
                    let count = attachment.len() as f64;
                    lumas.push(
                        attachment
                            .iter()
                            .map(|&value| f64::from(value))
                            .sum::<f64>()
                            / count,
                    );
                }
                let ack = json!({"protocolVersion": 2, "operation": "extract.ack",
                                 "requestId": "X", "seq": message["seq"], "emits": []});
                serving.write(&ack.to_string());
            } else if message.get("ok").is_some() {
                answers += 1;
            }
        }
        assert_eq!(serving.finish(), Some(0));
        let colors_a = colors_of(&out_a);
        assert!(
            colors_a.chars().all(|c| c == 'R'),
            "{renderer} A: {colors_a}"
        );
        assert!(
            !lumas.is_empty(),
            "{renderer}: no timeline frame reached the extractor"
        );
        // Full-range luma of pure blue is 18, of pure red 54.
        assert!(
            lumas.iter().all(|luma| *luma < 36.0),
            "{renderer}: the extraction saw the export's photo: {lumas:?}"
        );
    }
}

/// Two request contexts on one device, as `serve` makes them, export one video source id mapped to
/// a red and a blue video with the same timestamps.
#[test]
fn two_exports_of_one_template_keep_their_own_video() {
    let _gpu = gpu_test_lock();
    for renderer in RENDERERS {
        let policy = if renderer == "cpu" {
            crate::config::RendererPolicy::Cpu
        } else {
            crate::config::RendererPolicy::Gpu
        };
        let mut host = crate::create_host_with(VideoDecodePolicy::Software, policy).expect("host");
        let directory = scratch_directory(&format!("isolation-videos-{renderer}"));
        let end = 2 * alpha2_time::TICKS_PER_SECOND;
        let mut sources = Vec::new();
        for (name, color) in [("red", RED), ("blue", BLUE)] {
            let photo = directory.join(format!("{name}.png"));
            write_solid_png(&photo, color);
            let video = directory.join(format!("{name}.mp4"));
            let project = one_clip_project("photo", "image", end);
            let request = export_request(name, &project, "photo", &photo, &video);
            host.handle(
                parse_request(&request.to_string()).expect("request"),
                &mut |_| Ok(()),
            )
            .expect("source export");
            sources.push(video);
        }
        let device = std::sync::Arc::clone(host.device());
        drop(host);
        let project = one_clip_project("video", "video", end);
        let start = std::sync::Arc::new(std::sync::Barrier::new(2));
        let outputs = [directory.join("a.mp4"), directory.join("b.mp4")];
        let threads: Vec<_> = ["A", "B"]
            .into_iter()
            .enumerate()
            .map(|(index, id)| {
                let request =
                    export_request(id, &project, "video", &sources[index], &outputs[index]);
                let device = std::sync::Arc::clone(&device);
                let start = std::sync::Arc::clone(&start);
                std::thread::Builder::new()
                    .stack_size(16 << 20)
                    .spawn(move || {
                        let mut host = Host::for_request(device);
                        crate::encode::INJECT_SLOW_ENCODE
                            .with(|slow| slow.set(Some(Duration::from_millis(40))));
                        start.wait();
                        host.handle(
                            parse_request(&request.to_string()).expect("request"),
                            &mut |_| Ok(()),
                        )
                        .expect("export");
                    })
                    .expect("thread")
            })
            .collect();
        for thread in threads {
            thread.join().expect("join");
        }
        let (colors_a, colors_b) = (colors_of(&outputs[0]), colors_of(&outputs[1]));
        assert!(
            colors_a.chars().all(|c| c == 'R'),
            "{renderer} A: {colors_a}"
        );
        assert!(
            colors_b.chars().all(|c| c == 'B'),
            "{renderer} B: {colors_b}"
        );
    }
}

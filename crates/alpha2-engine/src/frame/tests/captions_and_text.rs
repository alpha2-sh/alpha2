//! Generated sources, captions, and text generators.

use super::*;

#[test]
fn generated_sources_render_without_host_frames() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(alpha2_render::generator_definition()))
        .expect("the generator compiles");
    // A black-to-white backdrop, and a red ellipse above it that its clip halves and moves right:
    // the transform applies to the generated picture as to any source.
    let project = alpha2_model::Project::from_json(
        r#"{
          "schema_version": 1, "id": "p", "name": "P",
          "settings": { "width": 64, "height": 32, "frame_rate": { "num": 30, "den": 1 }, "sample_rate": 48000 },
          "sources": [
            { "id": "backdrop", "kind": "generated", "generator": { "type": "fill", "paint": {
                "type": "linear_gradient", "start": { "x": 0, "y": 0.5 }, "end": { "x": 1, "y": 0.5 },
                "stops": [
                  { "offset": 0, "color": { "red": 0, "green": 0, "blue": 0, "alpha": 1 } },
                  { "offset": 1, "color": { "red": 1, "green": 1, "blue": 1, "alpha": 1 } }
                ] } } },
            { "id": "badge", "kind": "generated", "generator": { "type": "shape",
                "shape": { "type": "ellipse" }, "center": { "x": 0.5, "y": 0.5 }, "size": { "x": 0.5, "y": 1 },
                "fill": { "type": "solid", "color": { "red": 1, "green": 0, "blue": 0, "alpha": 1 } } } }
          ],
          "timeline": { "tracks": [
            { "id": "t1", "name": "Back", "kind": "video", "clips": [
              { "id": "c1", "source_id": "backdrop", "timeline_range": { "start": 0, "end": 120000 },
                "source_range": { "start": 0, "end": 120000 } } ], "transitions": [] },
            { "id": "t2", "name": "Front", "kind": "video", "clips": [
              { "id": "c2", "source_id": "badge", "timeline_range": { "start": 0, "end": 120000 },
                "source_range": { "start": 0, "end": 120000 },
                "visual": {
                  "position": { "mode": "constant", "value": { "x": 0.75, "y": 0.5 } },
                  "anchor": { "x": 0.5, "y": 0.5 },
                  "scale": { "mode": "constant", "value": { "x": 0.5, "y": 0.5 } },
                  "rotation_degrees": { "mode": "constant", "value": 0 },
                  "opacity": { "mode": "constant", "value": 1 } } } ], "transitions": [] }
          ] }
        }"#,
    )
    .expect("a valid project");
    let project = crate::EngineProject::new(project).expect("indexed");
    let time = MediaTime::from_ticks(60_000);
    assert_eq!(project.frame_needs(time).expect("needs"), Vec::new());
    let frame = project.evaluate(time).expect("evaluated");
    let size = RenderSize::new(64, 32).expect("size");
    let output = renderer
        .render_frame(size, &frame, &MissingFrames)
        .expect("frame renders");
    assert!(output.report.is_complete());
    let pixels = read_rgba8(&renderer, &output.texture.expect("complete frame"));
    let at = |x: usize, y: usize| pixels[(y * 64 + x) * 4..(y * 64 + x) * 4 + 4].to_vec();
    // The ellipse, a quarter of the width and half the height, centred at
    // x = 48.
    assert_eq!(at(48, 16), [255, 0, 0, 255]);
    let above = at(48, 4);
    assert!(
        above[0] == above[1] && above[1] == above[2] && above[3] == 255,
        "above the ellipse, the backdrop: {above:?}"
    );
    let (left, right) = (at(8, 16), at(30, 16));
    assert!(
        left[0] < right[0] && left[0] == left[1] && left[1] == left[2],
        "{left:?} {right:?}"
    );
}

#[test]
fn captions_show_their_cue_between_the_tracks_around_them() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(alpha2_render::generator_definition()))
        .expect("the generator compiles");
    let project = caption_project();
    let mut captions = crate::TextTextures::new();
    captions
        .add_font(
            "Alpha2 Fixture",
            std::fs::read("../../tests/fonts/Alpha2Fixture.ttf").expect("fixture font"),
            0,
        )
        .expect("font");
    let size = RenderSize::new(160, 90).expect("size");
    let render = |captions: &mut crate::TextTextures, ticks: i64| {
        let frame = project
            .evaluate(MediaTime::from_ticks(ticks))
            .expect("frame");
        captions.prepare(&renderer, &frame, size).expect("captions");
        let output = renderer
            .render_frame(size, &frame, &CaptionFrames(captions))
            .expect("frame renders");
        read_rgba8(&renderer, &output.texture.expect("complete"))
    };
    // The last frame of the first cue and the first of the second.
    let (first, second) = (
        render(&mut captions, 116_000),
        render(&mut captions, 120_000),
    );
    let at = |pixels: &[u8], x: usize, y: usize| {
        pixels[(y * 160 + x) * 4..(y * 160 + x) * 4 + 4].to_vec()
    };
    // Inside the caption's box, below its text.
    let grey = at(&render(&mut captions, 250_000), 56, 80);
    assert_ne!(first, second, "each cue draws its own text");
    assert_ne!(
        at(&first, 56, 80),
        grey,
        "the caption's box covers the backdrop"
    );
    assert_eq!(at(&first, 80, 81), [255, 0, 0, 255]);
    let after = render(&mut captions, 250_000);
    assert_eq!(at(&after, 56, 80), grey);
    // A frame's captions must be prepared at its size.
    let frame = project
        .evaluate(MediaTime::from_ticks(60_000))
        .expect("frame");
    let other = RenderSize::new(320, 180).expect("size");
    assert_eq!(
        renderer
            .render_frame(other, &frame, &CaptionFrames(&captions))
            .err(),
        Some(RenderError::MissingText)
    );
    let golden = RenderSize::new(320, 90).expect("size");
    let rows = (0..90)
        .flat_map(|row| {
            [&first, &second]
                .into_iter()
                .flat_map(move |pixels| pixels[row * 640..(row + 1) * 640].iter().copied())
        })
        .collect::<Vec<_>>();
    alpha2_golden_support::assert_rgba8_golden(
        "caption_cues",
        (golden.width, golden.height),
        &rows,
        2,
    );
}

/// The style every text test draws in: an outline and a box, so the
/// decorations' placement is compared too.
const TEXT_STYLE: &str = r#"{ "font_size": 0.2, "position": { "x": 0.5, "y": 0.6 },
    "outline": { "color": { "red": 0, "green": 0, "blue": 0, "alpha": 1 }, "width": 0.1 },
    "background": { "color": { "red": 0, "green": 0, "blue": 0.4, "alpha": 0.6 }, "padding": 0.2 } }"#;

/// A grey backdrop with `tracks` over it, `sources` (each followed by a
/// comma) and `compositions` added: a 160x90 project two seconds long.
fn text_project(sources: &str, compositions: &str, tracks: &str) -> crate::EngineProject {
    let document = r#"{
          "schema_version": 1, "id": "p", "name": "P",
          "settings": { "width": 160, "height": 90, "frame_rate": { "num": 30, "den": 1 }, "sample_rate": 48000 },
          "sources": [ SOURCES
            { "id": "grey", "kind": "generated", "generator": { "type": "fill", "paint": {
                "type": "solid", "color": { "red": 0.3, "green": 0.3, "blue": 0.3, "alpha": 1 } } } }
          ],
          "compositions": [ COMPOSITIONS ],
          "timeline": { "tracks": [
            { "id": "back", "name": "Back", "kind": "video", "clips": [
              { "id": "backdrop", "source_id": "grey", "timeline_range": { "start": 0, "end": 240000 },
                "source_range": { "start": 0, "end": 240000 } COMPOSITION } ] } TRACKS
          ] }
        }"#
    .replace("SOURCES", sources)
    .replace("COMPOSITIONS", compositions)
    .replace(
        "COMPOSITION",
        if compositions.is_empty() {
            ""
        } else {
            r#", "composition": { "id": "titled" }"#
        },
    )
    .replace("TRACKS", tracks);
    let project = alpha2_model::Project::from_json(&document).expect("a valid project");
    crate::EngineProject::new(project).expect("indexed")
}

/// `project`'s frame at one second, its text prepared with the fixture font.
fn text_pixels(renderer: &Renderer, project: &crate::EngineProject) -> Vec<u8> {
    let mut texts = crate::TextTextures::new();
    texts
        .add_font(
            "Alpha2 Fixture",
            std::fs::read("../../tests/fonts/Alpha2Fixture.ttf").expect("fixture font"),
            0,
        )
        .expect("font");
    let size = RenderSize::new(160, 90).expect("size");
    texts
        .prepare_at(renderer, project, MediaTime::from_ticks(120_000), size)
        .expect("text");
    let frame = project
        .evaluate(MediaTime::from_ticks(120_000))
        .expect("frame");
    let output = renderer
        .render_frame(size, &frame, &CaptionFrames(&texts))
        .expect("frame renders");
    read_rgba8(renderer, &output.texture.expect("complete"))
}

/// Fails naming how many pixels of two 160-wide images differ by more than
/// a code, the first one, and the largest difference, rather than every
/// byte. A text generator's picture is composited over transparency before
/// its clip is: that intermediate rounds a caption's edge pixels a code
/// apart.
fn assert_same_pixels(actual: &[u8], expected: &[u8], what: &str) {
    let differing = actual
        .chunks(4)
        .zip(expected.chunks(4))
        .enumerate()
        .filter(|(_, (a, b))| a.iter().zip(*b).any(|(a, b)| a.abs_diff(*b) > 1))
        .collect::<Vec<_>>();
    let largest = actual
        .iter()
        .zip(expected)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    assert!(
        differing.is_empty(),
        "{what}: {} pixels differ, by up to {largest}; the first at {:?}: {:?} for {:?}",
        differing.len(),
        differing
            .first()
            .map(|(index, _)| (index % 160, index / 160)),
        differing.first().map(|(_, (a, _))| a),
        differing.first().map(|(_, (_, b))| b),
    );
}

#[test]
fn a_text_generator_draws_its_text_as_a_caption_in_its_style_would() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(alpha2_render::generator_definition()))
        .expect("the generator compiles");
    let caption = text_project(
        "",
        "",
        &format!(
            r#", {{ "id": "subs", "name": "Subtitles", "kind": "caption", "captions": {{
                "style": {TEXT_STYLE},
                "cues": [ {{ "timeline_range": {{ "start": 0, "end": 240000 }}, "text": "Title" }} ] }} }}"#
        ),
    );
    let text_source = format!(
        r#"{{ "id": "title", "kind": "generated", "generator": {{ "type": "text", "text": "Title",
            "style": {TEXT_STYLE} }} }},"#
    );
    let title_track = |visual: &str| {
        format!(
            r#", {{ "id": "titles", "name": "Titles", "kind": "video", "clips": [
                {{ "id": "t", "source_id": "title", "timeline_range": {{ "start": 0, "end": 240000 }},
                  "source_range": {{ "start": 0, "end": 240000 }} {visual} }} ] }}"#
        )
    };
    let clip = text_project(&text_source, "", &title_track(""));
    let expected = text_pixels(&renderer, &caption);
    assert_ne!(
        expected,
        text_pixels(&renderer, &text_project("", "", "")),
        "the caption draws"
    );
    assert_same_pixels(&text_pixels(&renderer, &clip), &expected, "a text clip");
    let moved = text_project(
        &text_source,
        "",
        &title_track(
            r#", "visual": { "position": { "mode": "constant", "value": { "x": 0.3, "y": 0.5 } },
                "anchor": { "x": 0.5, "y": 0.5 },
                "scale": { "mode": "constant", "value": { "x": 1, "y": 1 } },
                "rotation_degrees": { "mode": "constant", "value": 0 },
                "opacity": { "mode": "constant", "value": 1 } }"#,
        ),
    );
    let moved = text_pixels(&renderer, &moved);
    assert_ne!(moved, expected);
    let at = |pixels: &[u8], x: usize, y: usize| pixels[(y * 160 + x) * 4..][..4].to_vec();
    // The box, bottom-centred at (80, 54), has its right edge right of the
    // centre unmoved; moved left by 32 pixels, that column is the backdrop.
    let grey = at(&text_pixels(&renderer, &text_project("", "", "")), 0, 0);
    let right = (80..160)
        .rev()
        .find(|&x| at(&expected, x, 48) != grey)
        .expect("the box");
    assert_eq!(at(&moved, right, 48), grey);
    assert_ne!(at(&moved, right - 32, 48), grey);
    let composition = format!(
        r#"{{ "id": "titled", "output": "merge", "nodes": [
            {{ "id": "clip", "op": {{ "type": "clip" }} }},
            {{ "id": "text", "op": {{ "type": "generator", "generator": {{ "type": "text", "text": "Title",
                "style": {TEXT_STYLE} }} }} }},
            {{ "id": "merge", "op": {{ "type": "merge", "layers": [ {{ "input": "clip" }}, {{ "input": "text" }} ] }} }}
        ] }}"#
    );
    assert_same_pixels(
        &text_pixels(&renderer, &text_project("", &composition, "")),
        &expected,
        "a text node",
    );
}

#[test]
fn a_text_generator_needs_fonts_and_its_text_prepared() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(alpha2_render::generator_definition()))
        .expect("the generator compiles");
    let project = text_project(
        r#"{ "id": "title", "kind": "generated", "generator": { "type": "text", "text": "Title" } },"#,
        "",
        r#", { "id": "titles", "name": "Titles", "kind": "video", "clips": [
            { "id": "t", "source_id": "title", "timeline_range": { "start": 0, "end": 240000 },
              "source_range": { "start": 0, "end": 240000 } } ] }"#,
    );
    assert!(project.draws_text());
    assert!(!text_project("", "", "").draws_text());
    let size = RenderSize::new(160, 90).expect("size");
    let time = MediaTime::from_ticks(120_000);
    let error = crate::TextTextures::new()
        .prepare_at(&renderer, &project, time, size)
        .expect_err("no fonts");
    assert_eq!(
        error.to_string(),
        "the project shows text, but no fonts were registered"
    );
    // Nothing prepared: the frame says so rather than drawing without the text.
    let frame = project.evaluate(time).expect("frame");
    assert_eq!(
        renderer
            .render_frame(size, &frame, &CaptionFrames(&crate::TextTextures::new()))
            .err(),
        Some(RenderError::MissingText)
    );
}

/// [`caption_project`] with a full-frame matte clip and a small green dot in
/// the top left corner that it mattes, the matte's and the dot's tracks at
/// several places: each case's name, project, and whether the dot (and so
/// the matte) is drawn during the first cue. Otherwise the dot starts after
/// the cues, so nothing names the matte until then.
fn matte_caption_projects() -> [(&'static str, crate::EngineProject, bool); 5] {
    let sources = r#"
        { "id": "white", "kind": "generated", "generator": { "type": "fill", "paint": {
            "type": "solid", "color": { "red": 1, "green": 1, "blue": 1, "alpha": 1 } } } },
        { "id": "dot", "kind": "generated", "generator": { "type": "shape",
            "shape": { "type": "rectangle" }, "center": { "x": 0.05, "y": 0.05 }, "size": { "x": 0.05, "y": 0.05 },
            "fill": { "type": "solid", "color": { "red": 0, "green": 1, "blue": 0, "alpha": 1 } } } },"#;
    let matte_track = r#"
        { "id": "mattes", "name": "Mattes", "kind": "video", "clips": [
          { "id": "m", "source_id": "white", "timeline_range": { "start": 0, "end": 360000 },
            "source_range": { "start": 0, "end": 360000 } } ] },"#;
    // The dot's clip, from `start`: before it, nothing names the matte.
    let user_track = |start: i64| {
        format!(
            r#"
        {{ "id": "users", "name": "Users", "kind": "video", "clips": [
          {{ "id": "x", "source_id": "dot", "timeline_range": {{ "start": {start}, "end": 360000 }},
            "source_range": {{ "start": 0, "end": {span} }},
            "matte": {{ "source": "m", "mode": "alpha" }} }} ] }},"#,
            span = 360_000 - start
        )
    };
    let late = user_track(300_000);
    let early = user_track(0);
    let last = |track: &str| format!(",{}", track.trim_end().trim_end_matches(','));
    [
        (
            "unused matte under the captions, its user on top",
            CaptionExtraTracks {
                under_captions: matte_track,
                top: &last(&late),
                ..CaptionExtraTracks::default()
            },
            false,
        ),
        (
            "unused matte under the captions, its user at the bottom",
            CaptionExtraTracks {
                bottom: &late,
                under_captions: matte_track,
                ..CaptionExtraTracks::default()
            },
            false,
        ),
        (
            "unused matte over the captions",
            CaptionExtraTracks {
                over_captions: matte_track,
                top: &last(&late),
                ..CaptionExtraTracks::default()
            },
            false,
        ),
        (
            "unused matte at the bottom",
            CaptionExtraTracks {
                bottom: matte_track,
                top: &last(&late),
                ..CaptionExtraTracks::default()
            },
            false,
        ),
        (
            "drawn matte under the captions",
            CaptionExtraTracks {
                under_captions: matte_track,
                top: &last(&early),
                ..CaptionExtraTracks::default()
            },
            true,
        ),
    ]
    .map(|(case, tracks, drawn)| (case, caption_project_with(sources, tracks), drawn))
}

/// A matte no active clip names is drawn nowhere, and a caption keeps its
/// place in track order over it: wherever the matte's track and its user's
/// track lie, the caption stays under the track after it.
#[test]
fn a_caption_stays_under_the_track_after_it_over_a_matte_no_clip_uses() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(alpha2_render::generator_definition()))
        .expect("the generator compiles");
    pollster::block_on(renderer.register_effect(alpha2_render::track_matte_definition()))
        .expect("the matte compiles");
    let mut captions = crate::TextTextures::new();
    captions
        .add_font(
            "Alpha2 Fixture",
            std::fs::read("../../tests/fonts/Alpha2Fixture.ttf").expect("fixture font"),
            0,
        )
        .expect("font");
    let size = RenderSize::new(160, 90).expect("size");
    let mut render = |project: &crate::EngineProject, ticks: i64| {
        let frame = project
            .evaluate(MediaTime::from_ticks(ticks))
            .expect("frame");
        captions.prepare(&renderer, &frame, size).expect("captions");
        let output = renderer
            .render_frame(size, &frame, &CaptionFrames(&captions))
            .expect("frame renders");
        read_rgba8(&renderer, &output.texture.expect("complete"))
    };
    let ticks = 60_000;
    let baseline = render(&caption_project(), ticks);
    let at = |pixels: &[u8], x: usize, y: usize| {
        pixels[(y * 160 + x) * 4..(y * 160 + x) * 4 + 4].to_vec()
    };
    assert_eq!(at(&baseline, 80, 81), [255, 0, 0, 255], "the square");
    let mut wrong = Vec::new();
    for (case, project, drawn) in matte_caption_projects() {
        let frame = render(&project, ticks);
        let right = if drawn {
            at(&frame, 8, 4) == [0, 255, 0, 255]
                && at(&frame, 80, 81) == at(&baseline, 80, 81)
                && at(&frame, 56, 80) == at(&baseline, 56, 80)
        } else {
            frame == baseline
        };
        if !right {
            wrong.push(format!(
                "{case}: the square's pixel is {:?}",
                at(&frame, 80, 81)
            ));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

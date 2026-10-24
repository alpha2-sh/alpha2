use super::*;

#[test]
fn a_font_check_refuses_what_adding_the_font_refuses() {
    let fixture = std::fs::read("../../tests/fonts/Alpha2Fixture.ttf").expect("fixture");
    assert!(check_font(&fixture, 0).is_ok());
    // A WOFF2 container, which a browser's FontFace loads whatever its name.
    let mut woff2 = b"wOF2".to_vec();
    woff2.extend_from_slice(&fixture[4..]);
    let refused = check_font(&woff2, 0).expect_err("WOFF2 is no OpenType face");
    let added = TextRasterizer::empty()
        .add_font(TextFont {
            family: "Web".into(),
            bytes: woff2.into(),
            face_index: 0,
        })
        .expect_err("the rasterizer refuses it too");
    assert_eq!(refused.to_string(), added.to_string());
}

/// FNV-1a over the pixels, as `scripts/test-wasm-parity.mjs` and `test-wasm-browser.mjs`
/// (`text-raster-parity`) compute it.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The pinned text rasters that the browser's `WasmTextRasterizer` must
/// reproduce bit for bit (`npm run test:wasm-parity`). Run with
/// `ALPHA2_UPDATE_GOLDENS=1` to re-pin after an intended change.
#[test]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn fixture_requests_match_the_pinned_rasters() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/text_raster.json"
    );
    let mut golden: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("golden")).expect("JSON");
    // The golden's own stack: the fixture, then a Hangul-only face, so mixed lines split into both.
    let mut rasterizer = TextRasterizer::new(
        golden["fonts"]
            .as_array()
            .expect("fonts")
            .iter()
            .map(|font| TextFont {
                family: font["family"].as_str().expect("family").into(),
                bytes: std::fs::read(format!(
                    "{}/../../{}",
                    env!("CARGO_MANIFEST_DIR"),
                    font["path"].as_str().expect("path")
                ))
                .expect("golden font")
                .into(),
                face_index: 0,
            })
            .collect(),
    )
    .expect("golden fonts");
    // Only `1` re-pins, as for the image goldens: `0` or an empty value compares.
    let update =
        std::env::var_os("ALPHA2_UPDATE_GOLDENS").as_deref() == Some(std::ffi::OsStr::new("1"));
    for case in golden["cases"].as_array_mut().expect("cases") {
        let request = &case["request"];
        let number = |key: &str, default: f64| request[key].as_f64().unwrap_or(default) as f32;
        let rgba = |value: &serde_json::Value| -> [u8; 4] {
            let channels = value.as_array().expect("color");
            std::array::from_fn(|index| channels[index].as_u64().expect("u8") as u8)
        };
        let result = rasterizer
            .rasterize(&TextRasterRequest {
                text: request["text"].as_str().expect("text").into(),
                font_size: number("fontSize", 0.0),
                max_width: number("maxWidth", 0.0),
                line_height: number("lineHeight", 1.2),
                align: match request["align"].as_str().unwrap_or("start") {
                    "center" => TextAlign::Center,
                    "end" => TextAlign::End,
                    _ => TextAlign::Start,
                },
                color: rgba(&request["color"]),
                emoji_policy: match request["emojiPolicy"].as_str() {
                    Some("replace") => EmojiPolicy::Replace,
                    Some("monochrome") => EmojiPolicy::Monochrome,
                    _ => EmojiPolicy::Color,
                },
                content_version: case["contentVersion"].as_u64().expect("version"),
                outline: request.get("outline").map(|outline| TextOutline {
                    color: rgba(&outline["color"]),
                    width: outline["width"].as_f64().expect("width") as f32,
                }),
                background: request.get("background").map(|background| TextBackground {
                    color: rgba(&background["color"]),
                    padding: background["padding"].as_f64().expect("padding") as f32,
                }),
                font_family: request["fontFamily"].as_str().map(Into::into),
            })
            .expect("fixture rasterizes");
        let descriptor = &result.descriptor;
        let actual = serde_json::json!({
            "width": descriptor.texture.width,
            "height": descriptor.texture.height,
            "layoutX": descriptor.layout_x,
            "layoutY": descriptor.layout_y,
            "layoutWidth": descriptor.layout_width,
            "layoutHeight": descriptor.layout_height,
            "firstBaseline": descriptor.first_baseline,
            "ascent": descriptor.ascent,
            "descent": descriptor.descent,
            "lineCount": descriptor.line_count,
            "pixelsFnv1a": format!("{:016x}", fnv1a(&result.pixels.rgba8)),
        });
        if update {
            case["expected"] = actual;
        } else {
            assert_eq!(case["expected"], actual, "{}", case["name"]);
        }
    }
    if update {
        let mut text = serde_json::to_string_pretty(&golden).expect("JSON");
        text.push('\n');
        std::fs::write(path, text).expect("golden written");
    }
}

fn fixture() -> TextRasterizer {
    let bytes =
        std::fs::read("../../tests/fonts/Alpha2Fixture.ttf").expect("checked-in fixture font");
    TextRasterizer::new(vec![TextFont {
        family: "Alpha2 Fixture".into(),
        bytes: bytes.into(),
        face_index: 0,
    }])
    .expect("valid fixture")
}

#[test]
fn lines_break_at_opportunities_and_inside_only_what_is_wider() {
    // One cluster a character, 10 px each; a space opens a break after it.
    let clusters = |text: &str| {
        let characters = text.chars().collect::<Vec<_>>();
        characters
            .iter()
            .enumerate()
            .map(|(index, &character)| Cluster {
                glyphs: index..index + 1,
                start: index,
                width: if character == 'W' { 25.0 } else { 10.0 },
                hangs: character == ' ',
                break_before: index > 0 && characters[index - 1] == ' ',
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(break_lines(&clusters("aa bb cc"), 45.0), [0..3, 3..6, 6..8]);
    // A word wider than the line breaks between clusters.
    assert_eq!(break_lines(&clusters("aaaaaa"), 25.0), [0..2, 2..4, 4..6]);
    assert_eq!(break_lines(&clusters("a aaaa"), 25.0), [0..2, 2..4, 4..6]);
    // What moves down with a wide cluster still breaks before it when the two do not fit.
    assert_eq!(break_lines(&clusters("a aW"), 30.0), [0..2, 2..3, 3..4]);
    // Spaces hang past the end rather than open a line.
    assert_eq!(break_lines(&clusters("aa     "), 25.0), vec![0..7_usize]);
    assert_eq!(break_lines(&clusters("aa bb cc"), 0.0), vec![0..8_usize]);
}

#[test]
fn metrics_pixels_cache_and_invalidation_are_stable() {
    let mut rasterizer = fixture();
    let request = TextRasterRequest {
        text: "Alpha2 wraps text".into(),
        font_size: 20.0,
        max_width: 80.0,
        line_height: 1.25,
        align: TextAlign::Center,
        color: [255, 100, 50, 200],
        emoji_policy: EmojiPolicy::Replace,
        content_version: 7,
        outline: None,
        background: None,
        font_family: None,
    };
    let first = rasterizer.rasterize(&request).expect("rasterized");
    assert!(first.descriptor.is_valid());
    assert!(first.pixels.is_valid());
    assert!(first.descriptor.line_count >= 2);
    assert!(
        first
            .pixels
            .rgba8
            .as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| { pixel[0] <= pixel[3] && pixel[1] <= pixel[3] && pixel[2] <= pixel[3] })
    );
    assert_eq!(rasterizer.rasterize(&request).expect("cached"), first);
    assert_eq!(rasterizer.cache_len(), 1);
    rasterizer.invalidate(7);
    assert_eq!(rasterizer.cache_len(), 0);
}

#[test]
fn a_cached_result_is_shared_not_copied() {
    let mut rasterizer = fixture();
    let first = rasterizer
        .rasterize(&request("Alpha2", 40.0))
        .expect("draws");
    let again = rasterizer
        .rasterize(&request("Alpha2", 40.0))
        .expect("cached");
    assert!(
        Arc::ptr_eq(&first, &again),
        "a hit hands out the cached pixels"
    );
    assert_eq!(
        rasterizer.cache_bytes(),
        first.pixels.rgba8.len() + CACHE_ENTRY_OVERHEAD
    );
}

#[test]
fn the_result_cache_keeps_its_byte_budget() {
    // Titles of about 12 MiB each, twice the default budget: the cache keeps what fits.
    let mut rasterizer = fixture();
    let mut drawn = 0;
    let mut version = 0;
    while drawn <= 2 * DEFAULT_TEXT_CACHE_BYTES {
        version += 1;
        let raster = rasterizer
            .rasterize(&TextRasterRequest {
                content_version: version,
                ..request("Alpha2 Alpha2", 600.0)
            })
            .expect("draws");
        drawn += raster.pixels.rgba8.len();
        assert!(rasterizer.cache_bytes() <= DEFAULT_TEXT_CACHE_BYTES);
    }
    let kept = rasterizer.cache_len();
    assert!(kept > 0 && kept < usize::try_from(version).expect("small"));
    let newest = rasterizer.cache.get(version).expect("the newest is kept");
    assert_eq!(newest.descriptor.texture.content_version, version);
    assert!(rasterizer.cache.get(1).is_none(), "the oldest went first");
}

#[test]
fn the_least_recently_used_result_goes_first() {
    let mut rasterizer = fixture();
    let draw = |rasterizer: &mut TextRasterizer, version| {
        rasterizer
            .rasterize(&TextRasterRequest {
                content_version: version,
                ..request("Alpha2", 40.0)
            })
            .expect("draws")
    };
    let one = draw(&mut rasterizer, 1);
    draw(&mut rasterizer, 2);
    draw(&mut rasterizer, 3);
    let entry = rasterizer.cache_bytes() / 3;
    // Using version 1 makes version 2 the least recently used.
    assert!(Arc::ptr_eq(&draw(&mut rasterizer, 1), &one));
    rasterizer.set_cache_budget(2 * entry);
    assert_eq!(rasterizer.cache_len(), 2);
    assert!(rasterizer.cache.get(2).is_none());
    assert!(Arc::ptr_eq(&draw(&mut rasterizer, 1), &one));
    // A result larger than the budget is drawn but not kept; zero keeps nothing.
    rasterizer.set_cache_budget(entry - 1);
    assert_eq!((rasterizer.cache_len(), rasterizer.cache_bytes()), (0, 0));
    let four = draw(&mut rasterizer, 4);
    assert!(four.pixels.is_valid());
    assert_eq!(rasterizer.cache_len(), 0);
    rasterizer.set_cache_budget(0);
    draw(&mut rasterizer, 5);
    assert_eq!(rasterizer.cache_len(), 0);
    rasterizer.set_cache_budget(DEFAULT_TEXT_CACHE_BYTES);
    draw(&mut rasterizer, 6);
    draw(&mut rasterizer, 7);
    rasterizer.invalidate(6);
    assert_eq!(rasterizer.cache_bytes(), entry);
    rasterizer.clear();
    assert_eq!((rasterizer.cache_len(), rasterizer.cache_bytes()), (0, 0));
}

#[test]
fn a_requested_family_draws_first_and_the_stack_fills_in() {
    let fixture = std::fs::read("../../tests/fonts/Alpha2Fixture.ttf").expect("fixture");
    let family = font_family(&fixture, 0).expect("the fixture names its family");
    assert_eq!(font_family(b"not a font", 0), None);
    // The same face twice, with a doctored copy renamed; the request picks by the name table.
    let other = "Q".repeat(family.len());
    let renamed = rename_family(&fixture, &family, &other);
    assert_eq!(font_family(&renamed, 0), Some(other.clone()));
    let rasterizer = TextRasterizer::new(vec![
        TextFont {
            family: "first".into(),
            bytes: fixture.clone().into(),
            face_index: 0,
        },
        TextFont {
            family: "second".into(),
            bytes: renamed.into(),
            face_index: 0,
        },
    ])
    .expect("fonts");
    assert_eq!(rasterizer.font_order(None), [0, 1]);
    assert_eq!(rasterizer.font_order(Some(&other.to_lowercase())), [1, 0]);
    assert_eq!(rasterizer.font_order(Some(&family.to_uppercase())), [0, 1]);
    assert_eq!(rasterizer.font_order(Some("Missing")), [0, 1]);
    assert_eq!(
        runs(&rasterizer, "A\u{10ffff}", &[1, 0]),
        [(1, "A\u{10ffff}".into())],
        "a glyph no font has stays in the run before it"
    );
}

/// The runs `text` splits into with fonts tried in `order`, as (font, text).
fn runs(rasterizer: &TextRasterizer, text: &str, order: &[usize]) -> Vec<(usize, String)> {
    runs_under(rasterizer, text, order, EmojiPolicy::Color)
}

/// The runs `text` splits into under `policy`, as (font, text).
fn runs_under(
    rasterizer: &TextRasterizer,
    text: &str,
    order: &[usize],
    policy: EmojiPolicy,
) -> Vec<(usize, String)> {
    let faces = Faces::new(&rasterizer.fonts).expect("faces");
    itemize(
        &faces,
        text,
        &ParagraphBidiInfo::new(text, None),
        order,
        policy,
    )
    .into_iter()
    .map(|run| (run.font, text[run.range].to_owned()))
    .collect()
}

#[test]
fn a_paragraph_splits_into_runs_by_font_script_and_direction() {
    let rasterizer =
        TextRasterizer::new(vec![font("Alpha2Fixture.ttf", "Alpha2 Fixture"), hangul()])
            .expect("fonts");
    let owned = |runs: &[(usize, &str)]| {
        runs.iter()
            .map(|&(font, text)| (font, text.to_owned()))
            .collect::<Vec<_>>()
    };
    // Hangul from the font that has it; the space after it would stay there, but this font has
    // none. It keeps the script before it.
    assert_eq!(
        runs(&rasterizer, "안녕 Hello", &[0, 1]),
        owned(&[(1, "안녕"), (0, " "), (0, "Hello")])
    );
    assert_eq!(
        runs(&rasterizer, "A안B", &[0, 1]),
        owned(&[(0, "A"), (1, "안"), (0, "B")])
    );
    // The family asked for first: Latin falls back to the fixture.
    assert_eq!(
        runs(&rasterizer, "안녕 Hello", &[1, 0]),
        owned(&[(1, "안녕"), (0, " "), (0, "Hello")])
    );
    // Spaces and digits stay in the font before them when it has them.
    assert_eq!(
        runs(&rasterizer, "Hi 123", &[1, 0]),
        owned(&[(0, "Hi 123")])
    );
    // A mark neither font has stays with its base, in the base's font.
    assert_eq!(
        runs(&rasterizer, "가\u{301}A", &[0, 1]),
        owned(&[(1, "가\u{301}"), (0, "A")])
    );
    let fixture = fixture();
    assert_eq!(
        runs(&fixture, "שלום 123 abc", &[0]),
        owned(&[(0, "שלום "), (0, "123"), (0, " "), (0, "abc")]),
        "a right-to-left paragraph: its numbers and Latin are left-to-right runs inside it"
    );
}

/// A 1080p caption as the engine asks for one: 54 px, wrapped at 1728,
/// centred.
fn caption(text: &str, outline: Option<TextOutline>, version: u64) -> TextRasterRequest {
    TextRasterRequest {
        text: text.into(),
        font_size: 54.0,
        max_width: 1_728.0,
        line_height: 1.2,
        align: TextAlign::Center,
        color: [255, 255, 255, 255],
        emoji_policy: EmojiPolicy::Color,
        content_version: version,
        outline,
        background: Some(TextBackground {
            color: [0, 0, 0, 160],
            padding: 8.0,
        }),
        font_family: None,
    }
}

#[test]
fn a_last_line_with_glyphs_but_no_ink_keeps_its_place() {
    let mut rasterizer = fixture();
    let outline = TextOutline {
        color: [0, 0, 0, 255],
        width: 4.3,
    };
    // Spaces, tab, NBSP, CR, ZWSP, LRM, BOM, and a lone combining mark: glyphs without width.
    for (text, lines) in [
        ("Hello\n ", 2),
        ("Hello\n\t", 2),
        ("Hello\n\u{a0}", 2),
        ("Hello\n\u{200b}", 2),
        ("Hello\n\u{200e}", 2),
        ("Hello\n\u{feff}", 2),
        ("Hello\r\n ", 2),
        ("Hello\n\u{301}", 2),
        ("two words\n \n", 2),
        ("A \nB\n  ", 3),
        ("\n ", 2),
        ("Hello\n", 1),
        ("Hello\n\n\n", 1),
        ("Hello\n \nWorld", 3),
        (" ", 1),
    ] {
        for (index, outline) in [None, Some(outline)].into_iter().enumerate() {
            let version = 1 + u64::try_from(index).expect("small");
            let result = rasterizer
                .rasterize(&caption(text, outline, version))
                .unwrap_or_else(|error| panic!("{text:?}: {error}"));
            assert_eq!(result.descriptor.line_count, lines, "{text:?}");
            rasterizer.clear();
        }
    }
}

#[test]
fn a_glyph_a_font_substitutes_but_does_not_have_draws_as_notdef() {
    let fixture = std::fs::read("../../tests/fonts/Alpha2Fixture.ttf").expect("fixture");
    let face = rustybuzz::ttf_parser::Face::parse(&fixture, 0).expect("face");
    let glyphs = face.number_of_glyphs();
    let a = face.glyph_index('A').expect("the fixture draws A").0;
    // An AAT `morx` table whose one noncontextual subtable turns A into a glyph past the font's
    // last.
    let crafted = with_table(&fixture, *b"morx", &morx_substituting(a, 65_000));
    let mut buffer = UnicodeBuffer::new();
    buffer.push_str("A");
    let substituted = rustybuzz::shape(
        &rustybuzz::Face::from_slice(&crafted, 0).expect("crafted face"),
        &[],
        buffer,
    );
    assert_eq!(
        substituted.glyph_infos()[0].glyph_id,
        65_000,
        "the shaper applies the substitution as the font says"
    );
    let mut rasterizer = TextRasterizer::new(vec![TextFont {
        family: "crafted".into(),
        bytes: crafted.into(),
        face_index: 0,
    }])
    .expect("the font loads");
    let faces = Faces::new(&rasterizer.fonts).expect("faces");
    assert_eq!(
        shape_paragraph(
            &faces,
            "A",
            &ParagraphBidiInfo::new("A", None),
            54.0,
            &[0],
            EmojiPolicy::Color,
        )
        .iter()
        .map(|glyph| glyph.glyph_index)
        .collect::<Vec<_>>(),
        [0],
        "a glyph past the font's {glyphs} draws as .notdef"
    );
    rasterizer
        .rasterize(&caption("Caption A", None, 1))
        .expect("draws");
}

/// A `morx` table of one chain with one noncontextual subtable, in every
/// direction, mapping glyph `from` to `to` (a format 8 lookup).
fn morx_substituting(from: u16, to: u16) -> Vec<u8> {
    let lookup = [8_u16, from, 1, to]
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .collect::<Vec<_>>();
    let subtable_length = 12 + lookup.len();
    let chain_length = 16 + subtable_length;
    let mut table = Vec::new();
    table.extend(2_u16.to_be_bytes()); // version
    table.extend(0_u16.to_be_bytes());
    table.extend(1_u32.to_be_bytes()); // chains
    table.extend(1_u32.to_be_bytes()); // default flags
    table.extend(u32::try_from(chain_length).expect("small").to_be_bytes());
    table.extend(0_u32.to_be_bytes()); // features
    table.extend(1_u32.to_be_bytes()); // subtables
    table.extend(u32::try_from(subtable_length).expect("small").to_be_bytes());
    table.push(0x20); // coverage: all directions
    table.extend(0_u16.to_be_bytes());
    table.push(4); // noncontextual
    table.extend(1_u32.to_be_bytes()); // feature flags
    table.extend(lookup);
    table
}

/// The single-face font `bytes` with one more table: a new directory in
/// front, the old file after it, the table at the end.
fn with_table(bytes: &[u8], tag: [u8; 4], table: &[u8]) -> Vec<u8> {
    let read16 = |at: usize| usize::from(u16::from_be_bytes([bytes[at], bytes[at + 1]]));
    let read32 = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().expect("four bytes"));
    let count = read16(4);
    let directory = 12 + 16 * (count + 1);
    let shift = u32::try_from(directory).expect("small");
    let mut records = (0..count)
        .map(|index| {
            let at = 12 + 16 * index;
            let tag: [u8; 4] = bytes[at..at + 4].try_into().expect("tag");
            (tag, read32(at + 4), read32(at + 8) + shift, read32(at + 12))
        })
        .collect::<Vec<_>>();
    let end = directory + bytes.len().next_multiple_of(4);
    records.push((
        tag,
        0,
        u32::try_from(end).expect("small"),
        u32::try_from(table.len()).expect("small"),
    ));
    records.sort_by_key(|record| record.0);
    let mut out = bytes[..4].to_vec();
    out.extend(u16::try_from(count + 1).expect("small").to_be_bytes());
    out.extend([0; 6]); // search hints, which readers recompute
    for (tag, checksum, offset, length) in records {
        out.extend(tag);
        out.extend(checksum.to_be_bytes());
        out.extend(offset.to_be_bytes());
        out.extend(length.to_be_bytes());
    }
    out.extend(bytes);
    out.resize(end, 0);
    out.extend(table);
    out
}

/// The band as a tap-by-tap pass over every pixel computes it: the
/// definition [`dilate`] must match byte for byte.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn dilate_tap_by_tap(coverage: &[u8], width: u32, height: u32, radius: f32) -> Vec<u8> {
    let extent = i64::from(disc_extent(radius));
    let mut kernel = Vec::new();
    for dy in -extent..=extent {
        for dx in -extent..=extent {
            let distance = ((dx * dx + dy * dy) as f32).sqrt();
            let weight = (radius + 0.5 - distance).clamp(0.0, 1.0);
            if weight > 0.0 {
                kernel.push((dx, dy, (weight * 255.0).round() as u32));
            }
        }
    }
    let (width, height) = (i64::from(width), i64::from(height));
    let mut band = vec![0_u8; coverage.len()];
    for y in 0..height {
        for x in 0..width {
            let mut highest = 0_u32;
            for &(dx, dy, weight) in &kernel {
                let (sx, sy) = (x + dx, y + dy);
                if sx < 0 || sy < 0 || sx >= width || sy >= height {
                    continue;
                }
                let value = u32::from(coverage[(sy * width + sx) as usize]) * weight / 255;
                highest = highest.max(value);
            }
            band[(y * width + x) as usize] = highest as u8;
        }
    }
    band
}

#[test]
fn the_outline_band_is_the_disc_dilation_byte_for_byte() {
    // Sparse ink anywhere, against the texture's edges too, and none at all.
    let mut seed = 0x2545_f491_u32;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    for (width, height) in [(1, 1), (7, 3), (40, 23), (71, 45)] {
        for density in [0, 3, 40] {
            let coverage = (0..width * height)
                .map(|_| {
                    if next() % 100 < density {
                        u8::try_from(next() % 256).expect("byte")
                    } else {
                        0
                    }
                })
                .collect::<Vec<_>>();
            for radius in [
                0.01, 0.3, 0.5, 0.75, 1.0, 1.49, 2.5, 3.7, 6.0, 11.2, 30.5, 64.0,
            ] {
                assert_eq!(
                    dilate(&coverage, width, height, radius),
                    dilate_tap_by_tap(&coverage, width, height, radius),
                    "{width}x{height}, {density}% ink, radius {radius}"
                );
            }
        }
    }
    let mut rasterizer = fixture();
    let request = TextRasterRequest {
        font_size: 24.0,
        max_width: 0.0,
        ..caption("Alpha2 wraps\ntext Ag", None, 1)
    };
    let text = rasterizer.rasterize(&request).expect("text");
    let (width, height) = (
        text.descriptor.texture.width,
        text.descriptor.texture.height,
    );
    let coverage = text
        .pixels
        .rgba8
        .as_chunks::<4>()
        .0
        .iter()
        .map(|pixel| pixel[3])
        .collect::<Vec<_>>();
    for radius in [1.9, 4.3, 13.5] {
        assert_eq!(
            dilate(&coverage, width, height, radius),
            dilate_tap_by_tap(&coverage, width, height, radius),
            "caption, radius {radius}"
        );
    }
}

#[test]
fn an_outline_costs_what_its_ink_does_not_what_its_texture_does() {
    // A two-letter cue on a 4K canvas with the thickest outline: the texture is the wrap width,
    // 3456 px, but the ink is two glyphs.
    let mut rasterizer = fixture();
    let request = TextRasterRequest {
        font_size: 216.0,
        max_width: 3_456.0,
        ..caption(
            "Hi",
            Some(TextOutline {
                color: [0, 0, 0, 255],
                width: 64.0,
            }),
            1,
        )
    };
    let result = rasterizer.rasterize(&request).expect("draws");
    assert!(result.descriptor.texture.width > 3_456);
    // Tap by tap over the texture this took 43 s in release; the ink is the same at every width.
    alpha2_scaling_support::assert_growth_below(
        "an outlined cue as its texture widens",
        3_456 / alpha2_scaling_support::GROWTH,
        // Work that follows the texture grows eight times; the work here is mostly the ink's.
        4,
        |width| {
            (
                fixture(),
                TextRasterRequest {
                    #[allow(clippy::cast_precision_loss)] // At most 3,456.
                    max_width: width as f32,
                    ..request.clone()
                },
            )
        },
        |(rasterizer, request)| {
            rasterizer.rasterize(request).expect("draws");
        },
    );
}

/// `text` in the fixture font, white, alone on a line no wrap reaches.
fn plain(
    rasterizer: &mut TextRasterizer,
    text: &str,
    size: f32,
    line_height: f32,
) -> Arc<TextRasterResult> {
    rasterizer.clear();
    rasterizer
        .rasterize(&TextRasterRequest {
            text: text.into(),
            font_size: size,
            max_width: 0.0,
            line_height,
            align: TextAlign::Start,
            color: [255, 255, 255, 255],
            emoji_policy: EmojiPolicy::Color,
            content_version: 1,
            outline: None,
            background: None,
            font_family: None,
        })
        .unwrap_or_else(|error| panic!("{text:?}: {error}"))
}

/// Coverage by layout-box position: `(x, y)` from the box's top-left,
/// which may be negative inside the margin.
fn ink_at(result: &TextRasterResult, x: i64, y: i64) -> u8 {
    #[allow(clippy::cast_possible_truncation)]
    let (left, top) = (
        result.descriptor.layout_x as i64 + x,
        result.descriptor.layout_y as i64 + y,
    );
    let (width, height) = (
        i64::from(result.descriptor.texture.width),
        i64::from(result.descriptor.texture.height),
    );
    if left < 0 || top < 0 || left >= width || top >= height {
        return 0;
    }
    result.pixels.rgba8[usize::try_from((top * width + left) * 4 + 3).expect("index")]
}

fn total_ink(result: &TextRasterResult) -> u64 {
    result
        .pixels
        .rgba8
        .as_chunks::<4>()
        .0
        .iter()
        .map(|pixel| u64::from(pixel[3]))
        .sum()
}

/// The layout-box rectangle the result's ink covers, as (left, top, right, bottom).
fn ink_box(result: &TextRasterResult) -> (i64, i64, i64, i64) {
    let width = i64::from(result.descriptor.texture.width);
    #[allow(clippy::cast_possible_truncation)]
    let (dx, dy) = (
        result.descriptor.layout_x as i64,
        result.descriptor.layout_y as i64,
    );
    let mut bounds = (i64::MAX, i64::MAX, i64::MIN, i64::MIN);
    for (index, pixel) in result.pixels.rgba8.as_chunks::<4>().0.iter().enumerate() {
        if pixel[3] > 0 {
            let index = i64::try_from(index).expect("index");
            let (x, y) = (index % width - dx, index / width - dy);
            bounds = (
                bounds.0.min(x),
                bounds.1.min(y),
                bounds.2.max(x),
                bounds.3.max(y),
            );
        }
    }
    bounds
}

#[test]
fn accents_above_the_line_are_drawn_not_cut() {
    let mut rasterizer = fixture();
    for size in [36.0, 54.0, 108.0] {
        for (bare, accented) in [
            ("A", "Ä"),
            ("O", "Ö"),
            ("U", "Ü"),
            ("N", "Ñ"),
            ("E", "É"),
            ("ARGER", "ÄRGER"),
        ] {
            let bare = plain(&mut rasterizer, bare, size, 1.2);
            let marked = plain(&mut rasterizer, accented, size, 1.2);
            assert!(marked.descriptor.is_valid(), "{accented} at {size}");
            assert!(
                total_ink(&marked) > total_ink(&bare),
                "{accented} at {size} px draws no more than its letter"
            );
            // The mark reaches above the nominal ascent, into the margin; the box and its first
            // baseline stay put.
            assert!(ink_box(&marked).1 < 0, "{accented} at {size}");
            assert_eq!(
                marked.descriptor.first_baseline.to_bits(),
                bare.descriptor.first_baseline.to_bits()
            );
            assert_eq!(
                marked.descriptor.layout_height.to_bits(),
                bare.descriptor.layout_height.to_bits()
            );
            assert!(marked.descriptor.ascent > size * 0.8);
        }
    }
}

#[test]
fn tight_lines_keep_every_baseline_and_descender() {
    let mut rasterizer = fixture();
    let loose = total_ink(&plain(&mut rasterizer, "gjpy", 54.0, 2.0));
    for line_height in [0.5, 0.8, 1.0] {
        let tight = plain(&mut rasterizer, "gjpy", 54.0, line_height);
        assert!(tight.descriptor.is_valid(), "line height {line_height}");
        assert_eq!(
            total_ink(&tight),
            loose,
            "line height {line_height} cuts ink"
        );
        let two = plain(&mut rasterizer, "Hg\nHg", 54.0, line_height);
        assert!(two.descriptor.is_valid(), "two lines at {line_height}");
        assert!(two.descriptor.first_baseline <= two.descriptor.layout_height);
    }
}

#[test]
fn a_later_glyph_never_erases_an_earlier_one() {
    let mut rasterizer = fixture();
    // Each pair overlaps its boxes: kerning pulls the second glyph left over the first.
    for (first, pair) in [("L", "LT"), ("A", "AV"), ("T", "TA"), ("f", "fj")] {
        let alone = plain(&mut rasterizer, first, 48.0, 1.2);
        let together = plain(&mut rasterizer, pair, 48.0, 1.2);
        let (left, top, right, bottom) = ink_box(&alone);
        for y in top..=bottom {
            for x in left..=right {
                assert!(
                    ink_at(&together, x, y) >= ink_at(&alone, x, y),
                    "{pair}: the second glyph erased its first at ({x}, {y})"
                );
            }
        }
    }
    // A dagesh is a mark of its own: bet with one is not bet.
    let bet = plain(&mut rasterizer, "\u{5d1}", 48.0, 1.2);
    let with_dagesh = plain(&mut rasterizer, "\u{5d1}\u{5bc}", 48.0, 1.2);
    assert!(total_ink(&with_dagesh) > total_ink(&bet));
}

#[test]
fn marks_sit_where_the_shaper_anchors_them() {
    let mut rasterizer = fixture();
    let base = plain(&mut rasterizer, "X", 100.0, 1.2);
    let marked = plain(&mut rasterizer, "X\u{304}", 100.0, 1.2);
    let (left, top, right, _) = ink_box(&base);
    let mut mark = (i64::MAX, i64::MAX, i64::MIN, i64::MIN);
    for y in -60..160 {
        for x in -60..160 {
            if ink_at(&marked, x, y) > ink_at(&base, x, y) {
                mark = (mark.0.min(x), mark.1.min(y), mark.2.max(x), mark.3.max(y));
            }
        }
    }
    assert!(mark.0 <= mark.2, "the macron is drawn");
    assert!(
        mark.3 < top,
        "the macron rows {}..={} reach the capital's top row {top}",
        mark.1,
        mark.3
    );
    // Twice the offset of the macron's centre from the capital's, in pixels.
    let offset = (mark.0 + mark.2) - (left + right);
    assert!(
        offset.abs() <= 4,
        "the macron is {offset} half-pixels off the capital's centre"
    );
}

fn font(path: &str, family: &str) -> TextFont {
    TextFont {
        family: family.into(),
        bytes: std::fs::read(format!("../../tests/fonts/{path}"))
            .expect("checked-in fixture font")
            .into(),
        face_index: 0,
    }
}

fn hangul() -> TextFont {
    font("Alpha2FixtureHangul.ttf", "Alpha2 Fixture Hangul")
}

/// `text` in white at `size`, wrapped at `max_width`, aligned by `align`.
fn wrapped(
    rasterizer: &mut TextRasterizer,
    text: &str,
    size: f32,
    max_width: f32,
    align: TextAlign,
) -> Arc<TextRasterResult> {
    rasterizer.clear();
    rasterizer
        .rasterize(&TextRasterRequest {
            max_width,
            align,
            ..request(text, size)
        })
        .unwrap_or_else(|error| panic!("{text:?}: {error}"))
}

fn request(text: &str, size: f32) -> TextRasterRequest {
    TextRasterRequest {
        text: text.into(),
        font_size: size,
        max_width: 0.0,
        line_height: 1.2,
        align: TextAlign::Start,
        color: [255, 255, 255, 255],
        emoji_policy: EmojiPolicy::Color,
        content_version: 1,
        outline: None,
        background: None,
        font_family: None,
    }
}

/// Asserts `a` has the ink `b` has over the layout-box rectangle, `b` read `down` rows higher.
fn assert_same_ink(
    a: &TextRasterResult,
    b: &TextRasterResult,
    columns: std::ops::Range<i64>,
    rows: std::ops::Range<i64>,
    down: i64,
    what: &str,
) {
    for y in rows {
        for x in columns.clone() {
            assert_eq!(
                ink_at(a, x, y),
                ink_at(b, x, y - down),
                "{what}: ink differs at ({x}, {y})"
            );
        }
    }
}

#[test]
fn registering_a_font_outlines_none_of_its_glyphs() {
    // Ten registrations of a 6,253-glyph face, which must not outline every glyph up front.
    let read = || super::OUTLINES_READ.with(std::cell::Cell::get);
    let fixture = font("Alpha2Fixture.ttf", "Alpha2 Fixture");
    let before = read();
    let mut rasterizer = TextRasterizer::empty();
    for _ in 0..10 {
        rasterizer.add_font(fixture.clone()).expect("registers");
    }
    assert_eq!(read(), before, "registering read glyph outlines");
    assert_eq!(rasterizer.font_count(), 10);
    rasterizer.rasterize(&request("Hi", 40.0)).expect("draws");
    let drawn = read() - before;
    assert!(
        (1..=4).contains(&drawn),
        "drawing two letters read {drawn} outlines"
    );
}

#[test]
fn a_wrapped_right_to_left_paragraph_reads_its_lines_top_down() {
    let mut rasterizer = fixture();
    // 40 px, 48 px a line: "שלום עולם" ("hello world") takes two lines.
    let both = wrapped(&mut rasterizer, "שלום עולם", 40.0, 93.5, TextAlign::Start);
    assert_eq!(both.descriptor.line_count, 2);
    let first = wrapped(&mut rasterizer, "שלום", 40.0, 93.5, TextAlign::Start);
    let second = wrapped(&mut rasterizer, "עולם", 40.0, 93.5, TextAlign::Start);
    assert_same_ink(&both, &first, -12..106, -12..46, 0, "the first line");
    assert_same_ink(&both, &second, -12..106, 46..100, 48, "the second line");
}

#[test]
fn numbers_in_a_right_to_left_line_keep_their_order() {
    let mut rasterizer = fixture();
    // "שלום 123" shows as "123 שלום": the digits read left to right at the line's left.
    let mixed = plain(&mut rasterizer, "שלום 123", 40.0, 1.2);
    let digits = plain(&mut rasterizer, "123", 40.0, 1.2);
    #[allow(clippy::cast_possible_truncation)]
    let width = digits.descriptor.layout_width.ceil() as i64;
    assert_same_ink(&mixed, &digits, -12..width, -12..48, 0, "the digits");
}

#[test]
fn arabic_in_a_left_to_right_line_joins_as_it_does_alone() {
    let mut rasterizer = fixture();
    let word = "مرحبا";
    let alone = plain(&mut rasterizer, word, 40.0, 1.2);
    let latin = plain(&mut rasterizer, "Hi ", 40.0, 1.2);
    // Narrow enough that the Arabic word takes the second line on its own, at the left.
    let width = alone
        .descriptor
        .layout_width
        .max(latin.descriptor.layout_width)
        + 2.0;
    let mixed = wrapped(&mut rasterizer, "Hi مرحبا", 40.0, width, TextAlign::Start);
    assert_eq!(mixed.descriptor.line_count, 2);
    #[allow(clippy::cast_possible_truncation)]
    let columns = -12..width as i64 + 12;
    assert_same_ink(&mixed, &alone, columns, 40..100, 48, "the Arabic word");
}

#[test]
fn start_and_end_follow_the_paragraph_direction() {
    let mut rasterizer = fixture();
    for (text, start_is_left) in [("Hello", true), ("שלום", false), ("مرحبا Hi", false)] {
        let start = wrapped(&mut rasterizer, text, 40.0, 400.0, TextAlign::Start);
        let end = wrapped(&mut rasterizer, text, 40.0, 400.0, TextAlign::End);
        let (start_left, _, _, _) = ink_box(&start);
        let (end_left, _, _, _) = ink_box(&end);
        assert_eq!(start_left < end_left, start_is_left, "{text:?}");
    }
}

#[test]
fn lines_break_where_unicode_allows_and_nowhere_else() {
    let mut rasterizer = fixture();
    // A no-break space glues "1 2": the line breaks at the ordinary space.
    let glued = plain(&mut rasterizer, "1\u{a0}2", 40.0, 1.2)
        .descriptor
        .layout_width;
    let lines = wrapped(
        &mut rasterizer,
        "a 1\u{a0}2",
        40.0,
        glued + 1.0,
        TextAlign::Start,
    );
    assert_eq!(lines.descriptor.line_count, 2);
    let digits = wrapped(
        &mut rasterizer,
        "1\u{a0}2",
        40.0,
        glued + 1.0,
        TextAlign::Start,
    );
    assert_same_ink(
        &lines,
        &digits,
        -12..80,
        46..100,
        48,
        "NBSP: the second line",
    );
    let word = plain(&mut rasterizer, "bbbbbb", 40.0, 1.2)
        .descriptor
        .layout_width;
    let lines = wrapped(
        &mut rasterizer,
        "aaaa\u{200b}bbbbbb",
        40.0,
        word + 5.0,
        TextAlign::Start,
    );
    assert_eq!(lines.descriptor.line_count, 2);
    let first = wrapped(&mut rasterizer, "aaaa", 40.0, word + 5.0, TextAlign::Start);
    #[allow(clippy::cast_possible_truncation)]
    let columns = -12..word as i64 + 12;
    assert_same_ink(&lines, &first, columns, -12..46, 0, "ZWSP: the first line");
}

#[test]
fn line_and_paragraph_separators_break_and_a_tab_is_a_space() {
    let mut rasterizer = fixture();
    for (text, as_text) in [
        ("a\tb", "a b"),
        ("a\rb", "a\nb"),
        ("ab\r\ncd", "ab\ncd"),
        ("a\u{85}b", "a\nb"),
        ("a\u{2028}b", "a\nb"),
        ("a\u{2029}b", "a\nb"),
        ("a\u{b}b", "a\nb"),
        ("a\u{c}b", "a\nb"),
    ] {
        let drawn = plain(&mut rasterizer, text, 40.0, 1.2);
        let expected = plain(&mut rasterizer, as_text, 40.0, 1.2);
        assert_eq!(drawn, expected, "{text:?} draws as {as_text:?}");
    }
}

#[test]
fn a_word_after_a_space_never_overflows_the_line() {
    let mut rasterizer = fixture();
    // A capital W is wider than "I " and the room left after it: the word breaks between its
    // glyphs.
    let lines = wrapped(&mut rasterizer, "I WW", 100.0, 170.0, TextAlign::Start);
    let (left, _, right, _) = ink_box(&lines);
    assert!(left >= 0 && right < 170, "ink spans {left}..={right}");
    assert_eq!(lines.descriptor.line_count, 3);
}

#[test]
fn replace_takes_each_emoji_whole_and_nothing_else() {
    let mut rasterizer = fixture();
    let mut draw = |text: &str, emoji_policy| {
        rasterizer.clear();
        rasterizer
            .rasterize(&TextRasterRequest {
                emoji_policy,
                ..request(text, 40.0)
            })
            .expect("draws")
    };
    // Symbols that are not emoji stay.
    for text in ["\u{2713} Done", "\u{2794}", "\u{2776}"] {
        assert_eq!(
            draw(text, EmojiPolicy::Replace),
            draw(text, EmojiPolicy::Monochrome),
            "{text:?}"
        );
    }
    // An emoji, whatever its block, sequence, or modifiers, becomes one replacement character.
    for (text, replaced) in [
        ("\u{2b50}", "\u{fffd}"),
        ("\u{231a}", "\u{fffd}"),
        ("\u{1f642}", "\u{fffd}"),
        ("\u{1f1f0}\u{1f1f7}", "\u{fffd}"),
        ("\u{1f44d}\u{1f3fd}", "\u{fffd}"),
        ("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}", "\u{fffd}"),
        ("\u{2764}\u{fe0f}", "\u{fffd}"),
        ("A\u{1f642}B", "A\u{fffd}B"),
    ] {
        assert_eq!(
            draw(text, EmojiPolicy::Replace),
            draw(replaced, EmojiPolicy::Monochrome),
            "{text:?}"
        );
    }
}

/// The emoji corpus both text hosts read: `OffscreenTextRasterizer` replaces the same
/// graphemes (`packages/alpha2/test/text-raster.test.mjs`).
#[test]
fn replace_follows_the_shared_emoji_corpus() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/cases/emoji-replace-corpus.json"
    );
    let corpus: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("corpus")).expect("JSON");
    for case in corpus["cases"].as_array().expect("cases") {
        let text = case["text"].as_str().expect("text");
        assert_eq!(
            normalize(text, EmojiPolicy::Replace),
            case["replaced"].as_str().expect("replaced"),
            "{}",
            case["name"]
        );
        assert_eq!(
            normalize(text, EmojiPolicy::Monochrome),
            text,
            "{}",
            case["name"]
        );
    }
}

#[test]
fn requests_outside_the_limits_are_refused() {
    type Change = fn(&mut TextRasterRequest);
    fn outline(width: f32) -> TextOutline {
        TextOutline {
            color: [0; 4],
            width,
        }
    }
    fn background(padding: f32) -> TextBackground {
        TextBackground {
            color: [0; 4],
            padding,
        }
    }
    let mut rasterizer = fixture();
    let mut outcome = |change: Change| {
        let mut request = request("a", 40.0);
        change(&mut request);
        rasterizer.clear();
        rasterizer.rasterize(&request).map(|_| ())
    };
    let refused: [(&str, Change); 15] = [
        ("font size NaN", |r| r.font_size = f32::NAN),
        ("font size 0", |r| r.font_size = 0.0),
        ("font size 2049", |r| r.font_size = 2_049.0),
        ("64 KiB + 1 of text", |r| r.text = "a".repeat(64 * 1024 + 1)),
        ("line height 0", |r| r.line_height = 0.0),
        ("line height 10.1", |r| r.line_height = 10.1),
        ("version 0", |r| r.content_version = 0),
        ("max width NaN", |r| r.max_width = f32::NAN),
        ("max width 16385", |r| r.max_width = 16_385.0),
        ("outline 0", |r| r.outline = Some(outline(0.0))),
        ("outline NaN", |r| r.outline = Some(outline(f32::NAN))),
        ("outline 65", |r| r.outline = Some(outline(65.0))),
        ("padding -1", |r| r.background = Some(background(-1.0))),
        ("padding NaN", |r| r.background = Some(background(f32::NAN))),
        ("padding 257", |r| r.background = Some(background(257.0))),
    ];
    for (what, change) in refused {
        assert_eq!(
            outcome(change),
            Err(TextRasterError::InvalidRequest),
            "{what}"
        );
    }
    let accepted: [(&str, Change); 5] = [
        ("font size 2048", |r| r.font_size = 2_048.0),
        ("64 KiB of text", |r| {
            (r.text, r.font_size, r.max_width) = ("a".repeat(64 * 1024), 1.0, 16_000.0);
        }),
        ("line height 10", |r| r.line_height = 10.0),
        ("outline 64", |r| r.outline = Some(outline(64.0))),
        ("padding 256", |r| r.background = Some(background(256.0))),
    ];
    for (what, change) in accepted {
        assert_eq!(outcome(change), Ok(()), "{what}");
    }
    // A texture past 16384 pixels on either side is too large to draw.
    let oversized: [(&str, Change); 2] = [
        ("ten lines at 2048 px", |r| {
            (r.text, r.font_size) = ("a\n".repeat(10), 2_048.0);
        }),
        ("a line 20,000 px wide", |r| {
            (r.text, r.font_size) = ("W".repeat(200), 100.0);
        }),
    ];
    for (what, change) in oversized {
        assert_eq!(outcome(change), Err(TextRasterError::TooLarge), "{what}");
    }
}

#[test]
fn a_stack_without_fonts_or_with_a_broken_one_is_refused() {
    assert_eq!(
        TextRasterizer::new(Vec::new()).err(),
        Some(TextRasterError::NoFonts)
    );
    assert_eq!(
        TextRasterizer::empty().rasterize(&request("a", 40.0)),
        Err(TextRasterError::NoFonts)
    );
    let garbage = TextFont {
        family: "garbage".into(),
        bytes: b"not a font at all".as_slice().into(),
        face_index: 0,
    };
    let past_the_last_face = TextFont {
        face_index: 1,
        ..font("Alpha2Fixture.ttf", "Alpha2 Fixture")
    };
    for broken in [garbage, past_the_last_face] {
        let family = broken.family.clone();
        assert!(
            matches!(
                TextRasterizer::new(vec![broken.clone()]),
                Err(TextRasterError::InvalidFont(_))
            ),
            "{family}"
        );
        let mut rasterizer = fixture();
        assert!(matches!(
            rasterizer.add_font(broken),
            Err(TextRasterError::InvalidFont(_))
        ));
        assert_eq!(rasterizer.font_count(), 1, "a font that fails is not kept");
    }
}

#[test]
fn a_version_is_reused_until_a_font_or_clear_drops_it() {
    let mut rasterizer = fixture();
    let hello = TextRasterRequest {
        content_version: 9,
        ..request("Hello", 40.0)
    };
    let first = rasterizer.rasterize(&hello).expect("draws");
    // The caller keeps a version for one content: a request with it gets that raster back.
    let world = TextRasterRequest {
        text: "World".into(),
        ..hello.clone()
    };
    assert_eq!(rasterizer.rasterize(&world).expect("cached"), first);
    rasterizer.clear();
    assert_eq!(rasterizer.cache_len(), 0);
    let fresh = rasterizer.rasterize(&world).expect("draws");
    assert_ne!(fresh, first, "clear drops every raster");
    // A font changes what a version draws: Hangul the fixture lacks is drawn once a font has
    // it.
    let greeting = TextRasterRequest {
        content_version: 10,
        ..request("안녕", 40.0)
    };
    let boxes = rasterizer.rasterize(&greeting).expect("draws");
    rasterizer.add_font(hangul()).expect("registers");
    assert_eq!(rasterizer.cache_len(), 0, "a new font drops every raster");
    assert_ne!(rasterizer.rasterize(&greeting).expect("draws"), boxes);
    rasterizer.invalidate(10);
    assert_eq!(rasterizer.cache_len(), 0);
}

#[test]
fn a_line_of_two_scripts_draws_each_from_the_font_that_has_it() {
    let mut stack =
        TextRasterizer::new(vec![font("Alpha2Fixture.ttf", "Alpha2 Fixture"), hangul()])
            .expect("fonts");
    let mut korean = TextRasterizer::new(vec![hangul()]).expect("font");
    let mut latin = fixture();
    let mixed = plain(&mut stack, "안녕 Hello", 40.0, 1.2);
    let greeting = plain(&mut korean, "안녕", 40.0, 1.2);
    #[allow(clippy::cast_possible_truncation)]
    let width = greeting.descriptor.layout_width.floor() as i64;
    assert_same_ink(&mixed, &greeting, -12..width, -12..48, 0, "the Hangul");
    let boxes = plain(&mut latin, "안녕", 40.0, 1.2);
    assert_ne!(
        total_ink(&greeting),
        total_ink(&boxes),
        "the fixture alone draws .notdef"
    );
    stack.clear();
    let asked = stack
        .rasterize(&TextRasterRequest {
            font_family: Some("alpha2 fixture hangul".into()),
            ..request("안녕 Hello", 40.0)
        })
        .expect("draws");
    assert_eq!(asked, mixed, "neither font has what the other draws");
}

#[test]
fn background_boxes_cost_their_area_not_every_line_at_every_pixel() {
    // 500 one-letter lines, where trying every box at every pixel took about 3 s in release.
    let mut rasterizer = fixture();
    let request = TextRasterRequest {
        max_width: 400.0,
        background: Some(TextBackground {
            color: [0, 0, 0, 160],
            padding: 2.0,
        }),
        ..request(&vec!["a"; 500].join("\n"), 8.0)
    };
    let result = rasterizer.rasterize(&request).expect("draws");
    assert_eq!(result.descriptor.line_count, 500);
    alpha2_scaling_support::assert_linear(
        "background boxes",
        500 / alpha2_scaling_support::GROWTH,
        |lines| {
            (
                fixture(),
                TextRasterRequest {
                    text: vec!["a"; lines].join("\n"),
                    ..request.clone()
                },
            )
        },
        |(rasterizer, request)| {
            rasterizer.rasterize(request).expect("draws");
        },
    );
}

/// The background as a pass over every box at every pixel computes it:
/// the definition [`compose`] must match byte for byte.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn background_box_by_box(
    request: &TextRasterRequest,
    pixels: usize,
    width: u32,
    margin: [u32; 2],
    line_boxes: &[(f32, f32, f32)],
) -> Vec<u8> {
    let background = request.background.expect("a background");
    let [margin_x, margin_y] = margin.map(|margin| margin as f32);
    let padding = background.padding;
    let mut rgba8 = vec![0_u8; pixels * 4];
    for index in 0..pixels {
        let x = (index % width as usize) as f32;
        let y = (index / width as usize) as f32;
        let covered = line_boxes
            .iter()
            .map(|&(left, line_width, top)| {
                let (left, right, top, bottom) = (
                    margin_x + left - padding,
                    margin_x + left + line_width + padding,
                    margin_y + top - padding,
                    margin_y + top + request.font_size + padding,
                );
                let across = (right.min(x + 1.0) - left.max(x)).clamp(0.0, 1.0);
                let down = (bottom.min(y + 1.0) - top.max(y)).clamp(0.0, 1.0);
                across * down
            })
            .fold(0.0_f32, f32::max);
        let pixel = over(
            premultiplied(request.color, 0),
            premultiplied(background.color, (covered * 255.0).round() as u8),
        );
        for (channel, value) in pixel.iter().enumerate() {
            rgba8[index * 4 + channel] = (*value).min(255) as u8;
        }
    }
    rgba8
}

#[test]
fn the_background_is_every_line_box_byte_for_byte() {
    // Boxes apart, touching, and overlapping (padding past half the line gap), at fractional edges.
    for (padding, line_height) in [(0.0, 1.2), (2.5, 1.2), (9.75, 1.05), (30.0, 1.0)] {
        let font_size = 21.3;
        let request = TextRasterRequest {
            background: Some(TextBackground {
                color: [10, 20, 30, 200],
                padding,
            }),
            line_height,
            ..request("", font_size)
        };
        let advance = font_size * line_height;
        let lines = [
            (3.25, 40.5),
            (0.0, 77.75),
            (12.6, 0.0),
            (5.5, 20.0),
            (0.4, 61.1),
        ];
        let line_boxes = lines
            .iter()
            .enumerate()
            .filter(|(_, (_, width))| *width > 0.0)
            .map(|(line, &(left, width))| {
                #[allow(clippy::cast_precision_loss)]
                (left, width, line as f32 * advance)
            })
            .collect::<Vec<_>>();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let margin = padding.ceil() as u32;
        let width = 90 + 2 * margin;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let height = (advance * 5.0).ceil() as u32 + 2 * margin;
        let pixels = (width * height) as usize;
        assert_eq!(
            compose(
                &request,
                &vec![0; pixels],
                None,
                width,
                [margin, margin],
                &line_boxes
            ),
            background_box_by_box(&request, pixels, width, [margin, margin], &line_boxes),
            "padding {padding}, line height {line_height}"
        );
    }
}

/// `bytes` with every name record that reads `from` rewritten to `to`,
/// an ASCII name of the same length, so no table moves.
fn rename_family(bytes: &[u8], from: &str, to: &str) -> Vec<u8> {
    assert_eq!(from.len(), to.len());
    let mut out = bytes.to_vec();
    let utf16 = |text: &str| text.encode_utf16().flat_map(u16::to_be_bytes).collect();
    for (needle, with) in [
        (from.as_bytes().to_vec(), to.as_bytes().to_vec()),
        (utf16(from), utf16(to)),
    ] {
        let mut at = 0;
        while let Some(found) = out[at..].windows(needle.len()).position(|w| w == needle) {
            out[at + found..at + found + needle.len()].copy_from_slice(&with);
            at += found + needle.len();
        }
    }
    out
}

/// The fixture, then the color fixtures: Noto's `COLRv1` and CBDT emoji and
/// the drawn COLR glyphs at U+E000.
fn color_stack() -> TextRasterizer {
    TextRasterizer::new(vec![
        font("Alpha2Fixture.ttf", "Alpha2 Fixture"),
        font("Alpha2FixtureEmojiColr.ttf", "Alpha2 Fixture Emoji Colr"),
        font(
            "Alpha2FixtureEmojiBitmap.ttf",
            "Alpha2 Fixture Emoji Bitmap",
        ),
        font("Alpha2FixtureColor.ttf", "Alpha2 Fixture Color"),
    ])
    .expect("color fixtures")
}

/// The premultiplied pixel at layout-box position `(x, y)`.
fn pixel_at(result: &TextRasterResult, x: i64, y: i64) -> [u8; 4] {
    #[allow(clippy::cast_possible_truncation)]
    let (left, top) = (
        result.descriptor.layout_x as i64 + x,
        result.descriptor.layout_y as i64 + y,
    );
    let width = i64::from(result.descriptor.texture.width);
    let index = usize::try_from((top * width + left) * 4).expect("inside the texture");
    result.pixels.rgba8[index..index + 4]
        .try_into()
        .expect("four channels")
}

#[test]
fn an_emoji_tries_the_color_fonts_first_and_a_text_request_the_others() {
    let rasterizer = color_stack();
    let order = [0, 1, 2, 3];
    let under = |text: &str, order: &[usize], policy| runs_under(&rasterizer, text, order, policy);
    // A heart is text by default: the first font with one draws it, the color font when it comes
    // first, but U+FE0E asks for text.
    let (heart, text_heart) = ("\u{2764}", "\u{2764}\u{fe0e}");
    assert_eq!(
        under(heart, &order, EmojiPolicy::Color),
        [(0, heart.into())]
    );
    assert_eq!(
        under(heart, &[1, 0, 2, 3], EmojiPolicy::Color),
        [(1, heart.into())]
    );
    assert_eq!(
        under(text_heart, &[1, 0, 2, 3], EmojiPolicy::Color),
        [(0, text_heart.into())]
    );
    // U+FE0F asks for an emoji: a color font, though a text font with a heart comes first.
    assert_eq!(
        under("\u{2764}\u{fe0f}", &order, EmojiPolicy::Color),
        [(1, "\u{2764}\u{fe0f}".into())]
    );
    // Monochrome asks for outlines: the text font first.
    assert_eq!(
        under("\u{2764}\u{fe0f}", &order, EmojiPolicy::Monochrome),
        [(0, "\u{2764}\u{fe0f}".into())]
    );
    // Each sequence stays whole, in the color font, not in the text font before it.
    let emoji = "\u{1f441}\u{fe0f}\u{1f44d}\u{1f3fd}\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{1f1f0}\u{1f1f7}1\u{fe0f}\u{20e3}";
    assert_eq!(
        under(&format!("A {emoji}"), &order, EmojiPolicy::Color),
        [(0, "A ".into()), (1, emoji.into())]
    );
    // A family the stack names first draws them, the bitmaps.
    assert_eq!(
        under(emoji, &[2, 0, 1, 3], EmojiPolicy::Color),
        [(2, emoji.into())]
    );
}

#[test]
fn a_stack_without_color_fonts_keeps_emoji_where_they_fell() {
    // No color font: an emoji under the color policy falls back, and stays, as any grapheme does.
    let rasterizer = fixture();
    for policy in [EmojiPolicy::Color, EmojiPolicy::Monochrome] {
        assert_eq!(
            runs_under(&rasterizer, "A\u{2764}\u{fe0f}", &[0], policy),
            [(0, "A\u{2764}\u{fe0f}".into())]
        );
    }
}

#[test]
fn a_color_glyph_keeps_its_colors_and_takes_the_text_color_where_asked() {
    let mut rasterizer = color_stack();
    // At 100 px on a 1000-unit em: a red square over x 10..90 and y 0..80, an octagon in the text's
    // color, and a half-transparent blue bar across y 30..50.
    let mut draw = |color, emoji_policy| {
        rasterizer.clear();
        rasterizer
            .rasterize(&TextRasterRequest {
                color,
                emoji_policy,
                ..request("\u{e000}", 100.0)
            })
            .expect("draws")
    };
    let blue = draw([0, 0, 255, 255], EmojiPolicy::Color);
    let green = draw([0, 255, 0, 255], EmojiPolicy::Color);
    let faded = draw([0, 0, 255, 128], EmojiPolicy::Color);
    let monochrome = draw([0, 0, 255, 255], EmojiPolicy::Monochrome);
    // The square's corner, outside the octagon: the palette's red whatever the text's color.
    assert_eq!(pixel_at(&blue, 12, 2), [255, 0, 0, 255]);
    assert_eq!(pixel_at(&green, 12, 2), [255, 0, 0, 255]);
    // The octagon above the bar: the text's color.
    assert_eq!(pixel_at(&blue, 50, 15), [0, 0, 255, 255]);
    assert_eq!(pixel_at(&green, 50, 15), [0, 255, 0, 255]);
    // The bar: blue at 128/255 over the octagon.
    assert_eq!(pixel_at(&green, 50, 40), [0, 127, 128, 255]);
    // The text's alpha is the glyph's opacity.
    assert_eq!(pixel_at(&faded, 12, 2), [128, 0, 0, 128]);
    assert_eq!(pixel_at(&faded, 50, 15), [0, 0, 128, 128]);
    // Monochrome draws the base glyph's outline, the square, in the text's color.
    assert_eq!(pixel_at(&monochrome, 12, 2), [0, 0, 255, 255]);
    assert_eq!(pixel_at(&monochrome, 50, 40), [0, 0, 255, 255]);
}

#[test]
fn an_outline_surrounds_a_color_glyph() {
    let mut rasterizer = color_stack();
    let plain = rasterizer
        .rasterize(&request("\u{e000}", 100.0))
        .expect("draws");
    let outlined = rasterizer
        .rasterize(&TextRasterRequest {
            content_version: 2,
            outline: Some(TextOutline {
                color: [0, 0, 0, 255],
                width: 4.0,
            }),
            ..request("\u{e000}", 100.0)
        })
        .expect("draws");
    // Three pixels left of the square: nothing, then the band.
    assert_eq!(pixel_at(&plain, 7, 40), [0, 0, 0, 0]);
    assert_eq!(pixel_at(&outlined, 7, 40), [0, 0, 0, 255]);
    // The glyph goes over its band.
    assert_eq!(pixel_at(&outlined, 12, 2), [255, 0, 0, 255]);
}

#[test]
fn a_bitmap_glyph_fills_its_em_at_any_size() {
    let mut rasterizer = color_stack();
    for (version, size) in [(1, 20.0), (2, 109.0), (3, 300.0)] {
        let result = rasterizer
            .rasterize(&TextRasterRequest {
                content_version: version,
                font_family: Some("Alpha2 Fixture Emoji Bitmap".into()),
                ..request("\u{1f600}", size)
            })
            .expect("draws");
        let (left, top, right, bottom) = ink_box(&result);
        #[allow(clippy::cast_possible_truncation)]
        let em = size as i64;
        // The face spans about an em across, from the baseline's height above to below it.
        assert!(
            (left..=right).contains(&(em / 2)),
            "{size}: {left}..{right}"
        );
        assert!(
            right - left >= em * 3 / 4 && right - left <= em * 5 / 4,
            "{size}: {left}..{right}"
        );
        assert!(
            bottom - top >= em * 3 / 4 && bottom - top <= em * 5 / 4,
            "{size}: {top}..{bottom}"
        );
        // Its middle is the face's opaque yellow, averaged or interpolated.
        let [red, green, blue, alpha] = pixel_at(
            &result,
            i64::midpoint(left, right),
            top + (bottom - top) / 4,
        );
        assert!(
            alpha == 255 && red > 200 && green > 150 && blue < 100,
            "{size}: {red} {green} {blue} {alpha}"
        );
    }
}

/// Asserts `measure`'s ink meets every pixel `result` gives any alpha and
/// reaches at most `slack` pixels past them.
#[allow(clippy::cast_precision_loss)]
fn assert_ink_fits(label: &str, measure: &TextMeasure, result: &TextRasterResult, slack: f32) {
    let [left, top, right, bottom] = measure.ink.expect("the text draws");
    let (x0, y0, x1, y1) = ink_box(result);
    let (x0, y0, x1, y1) = (x0 as f32, y0 as f32, x1 as f32 + 1.0, y1 as f32 + 1.0);
    assert!(
        left < x0 + 1.0 && top < y0 + 1.0 && right > x1 - 1.0 && bottom > y1 - 1.0,
        "{label}: {:?} misses ink {:?}",
        measure.ink,
        (x0, y0, x1, y1)
    );
    assert!(
        left >= x0 - slack && top >= y0 - slack && right <= x1 + slack && bottom <= y1 + slack,
        "{label}: {:?} is loose around ink {:?}",
        measure.ink,
        (x0, y0, x1, y1)
    );
}

#[test]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn a_measure_holds_the_ink_the_raster_draws_and_no_more() {
    let mut rasterizer = color_stack();
    rasterizer.add_font(hangul()).expect("hangul");
    let outline = Some(TextOutline {
        color: [0, 0, 0, 255],
        width: 6.0,
    });
    let cases = [
        ("round", request("Qgo&@ jy", 64.0)),
        (
            "wrapped",
            TextRasterRequest {
                max_width: 300.0,
                align: TextAlign::Center,
                ..request("Wrapped onto several lines of text", 48.0)
            },
        ),
        (
            "outlined",
            TextRasterRequest {
                outline,
                ..request("Outlined Qg", 56.0)
            },
        ),
        (
            "captioned",
            caption("Boxed and outlined\nsecond line", outline, 1),
        ),
        ("accented", request("\u{c5}\u{e9}\u{1e9e} \u{f1}", 72.0)),
        (
            "right to left",
            TextRasterRequest {
                max_width: 400.0,
                align: TextAlign::End,
                ..request("\u{5e9}\u{5dc}\u{5d5}\u{5dd} abc", 40.0)
            },
        ),
        ("hangul", request("\u{d55c}\u{ae00} text", 50.0)),
        ("colr emoji", request("a\u{2764}\u{fe0f}b", 60.0)),
        (
            "bitmap emoji",
            TextRasterRequest {
                font_family: Some("Alpha2 Fixture Emoji Bitmap".into()),
                ..request("\u{2764}\u{fe0f}", 60.0)
            },
        ),
    ];
    for (label, request) in cases {
        let measure = rasterizer.measure(&request).expect("measures");
        assert_eq!(
            rasterizer.cache_len(),
            0,
            "{label}: measuring caches nothing"
        );
        rasterizer.clear();
        let result = rasterizer.rasterize(&request).expect("draws");
        rasterizer.clear();
        let descriptor = &result.descriptor;
        assert_eq!(
            (measure.box_width, measure.box_height, measure.line_count),
            (
                descriptor.layout_width.ceil() as u32,
                descriptor.layout_height.ceil() as u32,
                descriptor.line_count
            ),
            "{label}"
        );
        assert_eq!(
            measure.decoration_margin,
            request.decoration_margin(),
            "{label}"
        );
        // A COLR glyph is measured by what it paints with, which may hold a little more than it
        // paints; a bitmap drawn larger than its strike interpolates past its last pixels.
        let slack = if label.ends_with("emoji") { 2.0 } else { 1.0 };
        assert_ink_fits(label, &measure, &result, slack);
        let [left, top, right, bottom] = measure.content;
        assert!(
            left <= right && top <= bottom,
            "{label}: {:?}",
            measure.content
        );
    }
}

#[test]
fn content_follows_the_advances_and_ink_the_glyphs() {
    let rasterizer = fixture();
    let short = rasterizer.measure(&request("ace", 64.0)).expect("measures");
    let tall = rasterizer.measure(&request("agj", 64.0)).expect("measures");
    // The content spans the lines' em boxes whatever their ink; the descenders and the dot reach
    // further.
    assert_eq!(
        [short.content[1], short.content[3]],
        [tall.content[1], tall.content[3]]
    );
    assert_eq!(short.content[0], 0.0);
    let [_, short_top, _, short_bottom] = short.ink.expect("ink");
    let [_, tall_top, _, tall_bottom] = tall.ink.expect("ink");
    assert!(tall_bottom > short_bottom + 8.0 && tall_top < short_top);
    // Centred in a wider box, the line sits in its middle.
    let centred = rasterizer
        .measure(&TextRasterRequest {
            max_width: 1_000.0,
            align: TextAlign::Center,
            ..request("ace", 64.0)
        })
        .expect("measures");
    let width = short.content[2] - short.content[0];
    assert_eq!(centred.box_width, 1_000);
    assert!((centred.content[0] - (1_000.0 - width) / 2.0).abs() < 1e-3);
    // Spaces alone draw nothing, and hang past the line as trailing spaces do.
    let blank = rasterizer.measure(&request("   ", 64.0)).expect("measures");
    assert_eq!(blank.ink, None);
    assert_eq!(blank.content[0], blank.content[2]);
}

#[test]
fn a_measure_refuses_what_a_raster_refuses() {
    assert_eq!(
        TextRasterizer::empty().measure(&request("a", 10.0)),
        Err(TextRasterError::NoFonts)
    );
    assert_eq!(
        fixture().measure(&request("a", f32::NAN)),
        Err(TextRasterError::InvalidRequest)
    );
}

#[test]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn shaped_lines_break_as_the_rasterizer_breaks_them() {
    let mut rasterizer = color_stack();
    rasterizer.add_font(hangul()).expect("hangul");
    let cases = [
        request("Qgo&@ jy", 64.0),
        TextRasterRequest {
            max_width: 300.0,
            align: TextAlign::Center,
            ..request("Wrapped onto several lines of text", 48.0)
        },
        TextRasterRequest {
            max_width: 400.0,
            ..request(
                "\u{5e9}\u{5dc}\u{5d5}\u{5dd} abc \u{5e9}\u{5dc}\u{5d5}\u{5dd}",
                40.0,
            )
        },
        TextRasterRequest {
            max_width: 120.0,
            ..request(
                "\u{d55c}\u{ae00} text \u{d55c}\u{ae00}\u{d55c}\u{ae00}",
                50.0,
            )
        },
        request("a\u{2764}\u{fe0f}b", 60.0),
        request("blank lines\n\nkept\n\n\n", 30.0),
        TextRasterRequest {
            line_height: 0.6,
            max_width: 90.0,
            ..request("tight lines wrap here", 30.0)
        },
        TextRasterRequest {
            max_width: 40.0,
            ..request("Supercalifragilistic", 30.0)
        },
    ];
    for request in cases {
        let measure = rasterizer.measure(&request).expect("measures");
        let lines = rasterizer
            .shape(&request)
            .expect("shapes")
            .lines_at(request.max_width);
        assert_eq!(lines.line_count, measure.line_count, "{:?}", request.text);
        assert_eq!(
            lines.height.max(1.0).ceil() as u32,
            measure.box_height,
            "{:?}",
            request.text
        );
    }
}

#[test]
fn min_content_is_the_widest_run_between_opportunities() {
    let clusters = |text: &str| {
        let characters = text.chars().collect::<Vec<_>>();
        characters
            .iter()
            .enumerate()
            .map(|(index, &character)| Cluster {
                glyphs: index..index + 1,
                start: index,
                width: if character == 'W' { 25.0 } else { 10.0 },
                hangs: character == ' ',
                break_before: index > 0 && characters[index - 1] == ' ',
            })
            .collect::<Vec<_>>()
    };
    let shaped = |paragraphs: &[&str]| ShapedText {
        paragraphs: paragraphs.iter().map(|text| clusters(text)).collect(),
        font_size: 10.0,
        line_advance: 12.0,
    };
    let text = shaped(&["I WW  "]);
    // The run `WW`, not one `W` the breaker could fall back to; trailing spaces hang.
    assert_eq!(text.min_content_width(), 50.0);
    assert_eq!(text.max_content_width(), 70.0);
    assert_eq!(text.lines_at(0.0).line_count, 1);
    assert_eq!(text.lines_at(70.0).line_count, 1);
    assert_eq!(text.lines_at(50.0).line_count, 2);
    assert_eq!(text.lines_at(50.0).widest, 50.0);
    // Each paragraph is a line of its own; the widest sets max-content.
    let text = shaped(&["aa", "aaaa a", ""]);
    assert_eq!(text.max_content_width(), 60.0);
    assert_eq!(text.min_content_width(), 40.0);
    let lines = text.lines_at(0.0);
    // The blank last paragraph takes no room, as in the rasterizer.
    assert_eq!(lines.line_count, 2);
    assert_eq!(lines.height, 24.0);
}

#[test]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn text_at_its_max_content_width_never_wraps() {
    let rasterizer = fixture();
    for text in [
        "Ada Lovelace",
        "Wrote the first published algorithm for a machine, 1843",
        "a b",
        "two\nparagraphs here",
    ] {
        let request = request(text, 37.3);
        let shaped = rasterizer.shape(&request).expect("shapes");
        let unwrapped = shaped.lines_at(0.0);
        let at_max = shaped.lines_at(shaped.max_content_width() + 1.0 / 64.0);
        assert_eq!(at_max.line_count, unwrapped.line_count, "{text}");
        let measure = rasterizer.measure(&request).expect("measures");
        assert_eq!(
            shaped.max_content_width().max(1.0).ceil() as u32,
            measure.box_width,
            "{text}"
        );
        // At min-content the text breaks at every opportunity and splits no word.
        let at_min = shaped.lines_at(shaped.min_content_width() + 1.0 / 64.0);
        assert!(
            at_min.widest <= shaped.min_content_width() + 1.0 / 64.0,
            "{text}"
        );
    }
    assert_eq!(
        TextRasterizer::empty().shape(&request("a", 10.0)).err(),
        Some(TextRasterError::NoFonts)
    );
    assert_eq!(
        rasterizer.shape(&request("a", f32::NAN)).err(),
        Some(TextRasterError::InvalidRequest)
    );
}

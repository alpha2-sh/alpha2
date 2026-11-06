//! The pure-Rust still decoder against the `FFmpeg` host on every still fixture.

use alpha2_host_core::stills::{StillFormat, decode_still};

use super::*;

/// Largest per-channel difference allowed against the `FFmpeg` host's shown pixels.
///
/// 8-bit lossless sources match exactly; the host dithers 16-bit PNG samples to 8 bits. Lossy
/// decoders differ by a code value in IDCT rounding, which the YCbCr matrix turns into up to 3
/// (1 in luma plus 1.772 from blue chroma); subsampled chroma adds 1 for upsampling.
fn tolerance(format: StillFormat, bytes: &[u8], theirs: &DecodedVideoFrame) -> u8 {
    let subsampled = matches!(
        theirs.pixel_format,
        PixelFormat::Yuv(yuv) if yuv.chroma_shift_x + yuv.chroma_shift_y > 0
    );
    match format {
        // The IHDR bit depth.
        StillFormat::Png => u8::from(bytes.get(24) == Some(&16)),
        StillFormat::WebP if !bytes.windows(4).any(|kind| kind == b"VP8 ") => 0,
        StillFormat::Jpeg | StillFormat::WebP => 3 + u8::from(subsampled),
    }
}

fn still_fixtures() -> Vec<PathBuf> {
    let mut paths = ["", "extensions", "parity-frames"]
        .into_iter()
        .flat_map(|directory| {
            let directory = corpus(directory);
            fs::read_dir(&directory)
                .unwrap_or_else(|error| panic!("{}: {error}", directory.display()))
                .map(|entry| entry.expect("directory entry").path())
        })
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| matches!(extension, "png" | "jpg" | "jpeg" | "webp"))
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

/// One sample of `plane` at `(x, y)`, on the 8-bit scale.
fn sample(plane: &Plane, format: YuvFormat, x: u32, y: u32) -> f64 {
    let start = usize::try_from(y * plane.stride).expect("row");
    if format.bit_depth <= 8 {
        return f64::from(plane.bytes[start + x as usize]);
    }
    let at = start + 2 * x as usize;
    let mut value = u16::from_le_bytes([plane.bytes[at], plane.bytes[at + 1]]);
    if format.msb_aligned {
        value >>= 16 - u32::from(format.bit_depth);
    }
    f64::from(value) / f64::from(1_u32 << (format.bit_depth - 8))
}

/// Bilinear chroma at luma pixel `(x, y)`, sited as the renderer sites it.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn chroma(frame: &DecodedVideoFrame, format: YuvFormat, component: u32, x: u32, y: u32) -> f64 {
    let (plane, interleaved) = if format.interleaved_chroma {
        (&frame.planes[1], Some(component))
    } else {
        (&frame.planes[1 + component as usize], None)
    };
    let (width, height) = format.chroma_size(frame.planes[0].width, frame.planes[0].height);
    let factor_x = f64::from(1_u32 << format.chroma_shift_x);
    let factor_y = f64::from(1_u32 << format.chroma_shift_y);
    let (cosited_x, cosited_y) = match frame.chroma_location {
        ChromaLocation::Left => (true, false),
        ChromaLocation::TopLeft => (true, true),
        ChromaLocation::Unspecified | ChromaLocation::Center => (false, false),
    };
    let offset = |cosited: bool, factor: f64| if cosited { factor * 0.5 - 0.5 } else { 0.0 };
    let u = (f64::from(x) + 0.5 + offset(cosited_x, factor_x)) / factor_x - 0.5;
    let v = (f64::from(y) + 0.5 + offset(cosited_y, factor_y)) / factor_y - 0.5;
    let at = |column: f64, row: f64| {
        let column = column.clamp(0.0, f64::from(width - 1)) as u32;
        let row = row.clamp(0.0, f64::from(height - 1)) as u32;
        match interleaved {
            Some(component) => sample(plane, format, column * 2 + component, row),
            None => sample(plane, format, column, row),
        }
    };
    let (u0, v0) = (u.floor(), v.floor());
    let (fu, fv) = (u - u0, v - v0);
    let top = at(u0, v0) * (1.0 - fu) + at(u0 + 1.0, v0) * fu;
    let bottom = at(u0, v0 + 1.0) * (1.0 - fu) + at(u0 + 1.0, v0 + 1.0) * fu;
    top * (1.0 - fv) + bottom * fv
}

/// `frame` as straight RGBA, converted as the renderer shows it.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn shown_rgba(frame: &DecodedVideoFrame) -> Vec<u8> {
    let (width, height) = (frame.planes[0].width, frame.planes[0].height);
    let format = match frame.pixel_format {
        PixelFormat::Rgba8 => {
            let plane = &frame.planes[0];
            return (0..height)
                .flat_map(|y| {
                    let start = (y * plane.stride) as usize;
                    plane.bytes[start..start + width as usize * 4].to_vec()
                })
                .collect();
        }
        PixelFormat::Yuv(format) => format,
        other => panic!("a still decoded to {other:?}"),
    };
    let color = frame.color.unwrap_or(ColorMetadata::UNDECLARED);
    let (kr, kb) = match color.matrix {
        ColorMatrix::Bt601 => (0.299, 0.114),
        ColorMatrix::Bt2020Ncl => (0.2627, 0.0593),
        _ => (0.2126, 0.0722),
    };
    let to_8 = |value: f64| (value * 255.0).round().clamp(0.0, 255.0) as u8;
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        for x in 0..width {
            let luma = sample(&frame.planes[0], format, x, y);
            let (first, second) = (
                chroma(frame, format, 0, x, y),
                chroma(frame, format, 1, x, y),
            );
            let rgb = if color.matrix == ColorMatrix::Rgb {
                [luma / 255.0, first / 255.0, second / 255.0]
            } else {
                let (y, cb, cr) = if color.range == ColorRange::Full {
                    (
                        luma / 255.0,
                        (first - 128.0) / 255.0,
                        (second - 128.0) / 255.0,
                    )
                } else {
                    (
                        (luma - 16.0) / 219.0,
                        (first - 128.0) / 224.0,
                        (second - 128.0) / 224.0,
                    )
                };
                let r = y + 2.0 * (1.0 - kr) * cr;
                let b = y + 2.0 * (1.0 - kb) * cb;
                [r, (y - kr * r - kb * b) / (1.0 - kr - kb), b]
            };
            let alpha = if format.alpha {
                to_8(sample(frame.planes.last().expect("alpha"), format, x, y) / 255.0)
            } else {
                255
            };
            rgba.extend_from_slice(&[to_8(rgb[0]), to_8(rgb[1]), to_8(rgb[2]), alpha]);
        }
    }
    rgba
}

/// The first picture the `FFmpeg` host decodes from `path`.
fn ffmpeg_frame(path: &Path) -> Result<DecodedVideoFrame, DecodeError> {
    let mut decoder = NativeDecoder::open(path)?;
    let cancel = CancelToken::default();
    loop {
        match decoder.decode_next(&cancel)? {
            DecodeEvent::Video(frame) => return Ok(frame),
            DecodeEvent::EndOfStream => return Err(DecodeError::OutOfRange),
            DecodeEvent::Retry | DecodeEvent::Audio(_) => {}
        }
    }
}

/// Compares one still with the `FFmpeg` host: its largest channel difference, `None` when both
/// refuse it, or why they disagree.
fn compare_still(path: &Path) -> Result<Option<u8>, String> {
    let name = path.display();
    let bytes = fs::read(path).map_err(|error| format!("{name}: {error}"))?;
    let (ours, theirs) = match (decode_still(&bytes), ffmpeg_frame(path)) {
        (Ok(ours), Ok(theirs)) => (ours, theirs),
        (Err(_), Err(_)) => return Ok(None),
        (Err(error), Ok(_)) => {
            return Err(format!("{name}: refused, FFmpeg decodes it: {error:?}"));
        }
        (Ok(_), Err(error)) => {
            return Err(format!("{name}: decoded, FFmpeg refuses it: {error:?}"));
        }
    };
    let info = ours.info;
    let stored = (theirs.planes[0].width, theirs.planes[0].height);
    if stored != (info.width, info.height)
        || theirs.visible_rect.width != info.width
        || theirs.visible_rect.height != info.height
    {
        return Err(format!(
            "{name}: stored {}x{}, FFmpeg {stored:?} showing {:?}",
            info.width, info.height, theirs.visible_rect
        ));
    }
    if theirs.orientation != info.orientation {
        return Err(format!(
            "{name}: orientation {:?}, FFmpeg {:?}",
            info.orientation, theirs.orientation
        ));
    }
    let shown = shown_rgba(&theirs);
    let frame = ours.into_video_frame();
    if frame.sample_aspect_ratio != theirs.sample_aspect_ratio {
        return Err(format!(
            "{name}: pixel aspect {:?}, FFmpeg {:?}",
            frame.sample_aspect_ratio, theirs.sample_aspect_ratio
        ));
    }
    let effective = |color: Option<ColorMetadata>| {
        let color = color.unwrap_or(ColorMetadata::UNDECLARED);
        (color.primaries, color.dynamic_range)
    };
    if effective(frame.color) != effective(theirs.color) {
        return Err(format!(
            "{name}: color {:?}, FFmpeg {:?}",
            frame.color, theirs.color
        ));
    }
    let max = frame.planes[0]
        .bytes
        .iter()
        .zip(&shown)
        .map(|(ours, theirs)| ours.abs_diff(*theirs))
        .max()
        .unwrap_or(0);
    let allowed = tolerance(info.format, &bytes, &theirs);
    if max > allowed {
        return Err(format!(
            "{name}: pixels differ by up to {max}, over {allowed} ({:?} {:?})",
            theirs.pixel_format, theirs.color
        ));
    }
    Ok(Some(max))
}

/// Compares every still in `paths`, printing each largest difference, and panics on a mismatch.
fn compare_stills(paths: impl IntoIterator<Item = PathBuf>) {
    let mut failures = Vec::new();
    for path in paths {
        match compare_still(&path) {
            Ok(Some(max)) => eprintln!("{}: max difference {max}", path.display()),
            Ok(None) => eprintln!("{}: refused by both", path.display()),
            Err(failure) => failures.push(failure),
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn every_still_fixture_decodes_like_the_ffmpeg_host() {
    let fixtures = still_fixtures();
    assert!(fixtures.len() >= 12, "{fixtures:?}");
    compare_stills(fixtures);
}

/// Compares the stills in the directory `ALPHA2_STILLS_DIR` names, such as camera photos.
#[test]
#[ignore = "needs ALPHA2_STILLS_DIR"]
fn stills_in_a_directory_decode_like_the_ffmpeg_host() {
    let directory = std::env::var_os("ALPHA2_STILLS_DIR").expect("ALPHA2_STILLS_DIR");
    let mut paths = fs::read_dir(directory)
        .expect("stills directory")
        .map(|entry| entry.expect("directory entry").path())
        .collect::<Vec<_>>();
    paths.sort();
    compare_stills(paths);
}

const VARIANT_WIDTH: u32 = 19;
const VARIANT_HEIGHT: u32 = 7;

/// Samples that vary in every bit, wrapped to bytes.
fn wave(count: u32, scale: u32) -> Vec<u8> {
    (0..count)
        .map(|index| index.wrapping_mul(scale).to_le_bytes()[0])
        .collect()
}

fn write_png(
    label: &str,
    (color, depth): (png::ColorType, png::BitDepth),
    data: &[u8],
    setup: impl FnOnce(&mut png::Encoder<'_, std::io::BufWriter<fs::File>>),
    chunks: &[(&[u8; 4], &[u8])],
) -> PathBuf {
    let path = output_path(label).with_extension("png");
    let file = std::io::BufWriter::new(fs::File::create(&path).expect("create"));
    let mut encoder = png::Encoder::new(file, VARIANT_WIDTH, VARIANT_HEIGHT);
    encoder.set_color(color);
    encoder.set_depth(depth);
    setup(&mut encoder);
    let mut writer = encoder.write_header().expect("header");
    for (kind, chunk) in chunks {
        writer
            .write_chunk(png::chunk::ChunkType(**kind), chunk)
            .expect("chunk");
    }
    writer.write_image_data(data).expect("rows");
    writer.finish().expect("finish");
    path
}

/// A little-endian TIFF structure holding only `Orientation`.
fn exif_orientation(value: u16) -> Vec<u8> {
    let mut tiff = b"II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
    tiff.extend_from_slice(&value.to_le_bytes());
    tiff.extend_from_slice(&[0; 6]);
    tiff
}

/// Compares the stills a test wrote, then removes them.
fn compare_written(paths: &[PathBuf]) {
    let failures = paths
        .iter()
        .filter_map(|path| {
            let outcome = compare_still(path);
            fs::remove_file(path).expect("remove");
            outcome.err()
        })
        .collect::<Vec<_>>();
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn png_variants_decode_like_the_ffmpeg_host() {
    let pixels = VARIANT_WIDTH * VARIANT_HEIGHT;
    // Low bytes over the whole range test how 16-bit samples round.
    let wide = wave(pixels * 8, 40_503);
    let rgb = wave(pixels * 3, 13);
    let gray = wave(VARIANT_HEIGHT * VARIANT_WIDTH.div_ceil(2), 59);
    let indexed = (0..pixels)
        .map(|index| (index % 4).to_le_bytes()[0])
        .collect::<Vec<_>>();
    compare_written(&[
        write_png(
            "still-rgba16",
            (png::ColorType::Rgba, png::BitDepth::Sixteen),
            &wide,
            |_| {},
            &[],
        ),
        write_png(
            "still-gray-alpha16",
            (png::ColorType::GrayscaleAlpha, png::BitDepth::Sixteen),
            &wide[..(pixels * 4) as usize],
            |_| {},
            &[],
        ),
        write_png(
            "still-gray4",
            (png::ColorType::Grayscale, png::BitDepth::Four),
            &gray,
            |_| {},
            &[],
        ),
        write_png(
            "still-palette",
            (png::ColorType::Indexed, png::BitDepth::Eight),
            &indexed,
            |encoder| {
                encoder.set_palette(vec![200, 10, 30, 0, 255, 0, 40, 40, 200, 255, 255, 255]);
                encoder.set_trns(vec![255, 0, 128]);
            },
            &[],
        ),
        write_png(
            "still-exif-png",
            (png::ColorType::Rgb, png::BitDepth::Eight),
            &rgb,
            |_| {},
            &[(b"eXIf", &exif_orientation(6))],
        ),
    ]);
}

#[test]
fn webp_variants_decode_like_the_ffmpeg_host() {
    let pixels = VARIANT_WIDTH * VARIANT_HEIGHT;
    let paths = [
        (
            "still-webp-alpha",
            image_webp::ColorType::Rgba8,
            wave(pixels * 4, 31),
            None,
        ),
        (
            "still-webp-exif",
            image_webp::ColorType::Rgb8,
            wave(pixels * 3, 13),
            Some(3),
        ),
    ]
    .map(|(label, color, data, orientation)| {
        let path = output_path(label).with_extension("webp");
        let mut webp = Vec::new();
        let mut writer = image_webp::WebPEncoder::new(&mut webp);
        if let Some(value) = orientation {
            writer.set_exif_metadata(exif_orientation(value));
        }
        writer
            .encode(&data, VARIANT_WIDTH, VARIANT_HEIGHT, color)
            .expect("lossless WebP");
        fs::write(&path, webp).expect("write");
        path
    });
    compare_written(&paths);
}

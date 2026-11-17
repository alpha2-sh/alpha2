use std::path::Path;

use super::*;

fn corpus(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/corpus")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn crc32(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!0_u32, |crc, &byte| {
        (0..8).fold(crc ^ u32::from(byte), |crc, _| {
            (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1))
        })
    })
}

fn png_chunk(out: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    out.extend_from_slice(&u32::try_from(data.len()).expect("small").to_be_bytes());
    let start = out.len();
    out.extend_from_slice(&kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// A zlib stream of stored deflate blocks.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let blocks = data.chunks(65_535).collect::<Vec<_>>();
    for (index, block) in blocks.iter().enumerate() {
        out.push(u8::from(index + 1 == blocks.len()));
        let length = u16::try_from(block.len()).expect("block");
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&(!length).to_le_bytes());
        out.extend_from_slice(block);
    }
    if blocks.is_empty() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    let (a, b) = data.iter().fold((1_u32, 0_u32), |(a, b), &byte| {
        let a = (a + u32::from(byte)) % 65_521;
        (a, (b + a) % 65_521)
    });
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

/// A PNG of 8-bit RGBA `pixels`, Adam7-interlaced when asked, with `extra` chunks after `IHDR`.
fn handmade_png(
    width: u32,
    height: u32,
    pixels: &[u8],
    interlaced: bool,
    extra: &[(&[u8; 4], &[u8])],
) -> Vec<u8> {
    const PASSES: [(u32, u32, u32, u32); 7] = [
        (0, 0, 8, 8),
        (4, 0, 8, 8),
        (0, 4, 4, 8),
        (2, 0, 4, 4),
        (0, 2, 2, 4),
        (1, 0, 2, 2),
        (0, 1, 1, 2),
    ];
    let passes: &[(u32, u32, u32, u32)] = if interlaced { &PASSES } else { &[(0, 0, 1, 1)] };
    let mut raw = Vec::new();
    for &(x0, y0, dx, dy) in passes {
        let columns = (x0..width).step_by(dx as usize).collect::<Vec<_>>();
        if columns.is_empty() {
            continue;
        }
        for y in (y0..height).step_by(dy as usize) {
            raw.push(0);
            for &x in &columns {
                let at = ((y * width + x) * 4) as usize;
                raw.extend_from_slice(&pixels[at..at + 4]);
            }
        }
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, u8::from(interlaced)]);
    png_chunk(&mut out, *b"IHDR", &header);
    for (kind, data) in extra {
        png_chunk(&mut out, **kind, data);
    }
    png_chunk(&mut out, *b"IDAT", &zlib_stored(&raw));
    png_chunk(&mut out, *b"IEND", &[]);
    out
}

fn encoded_png(
    width: u32,
    height: u32,
    color: png::ColorType,
    depth: png::BitDepth,
    setup: impl FnOnce(&mut png::Encoder<'_, &mut Vec<u8>>),
    data: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(color);
        encoder.set_depth(depth);
        setup(&mut encoder);
        let mut writer = encoder.write_header().expect("header");
        writer.write_image_data(data).expect("rows");
    }
    out
}

/// A TIFF structure whose IFD0 holds one entry: `tag` of `kind` with `value`.
fn tiff(big_endian: bool, tag: u16, kind: u16, value: u32) -> Vec<u8> {
    let u16_bytes = |value: u16| {
        if big_endian {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        }
    };
    let u32_bytes = |value: u32| {
        if big_endian {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        }
    };
    let mut out = if big_endian {
        b"MM\0*".to_vec()
    } else {
        b"II*\0".to_vec()
    };
    out.extend_from_slice(&u32_bytes(8));
    out.extend_from_slice(&u16_bytes(1));
    out.extend_from_slice(&u16_bytes(tag));
    out.extend_from_slice(&u16_bytes(kind));
    out.extend_from_slice(&u32_bytes(1));
    if kind == 3 {
        out.extend_from_slice(&u16_bytes(u16::try_from(value).expect("short")));
        out.extend_from_slice(&[0, 0]);
    } else {
        out.extend_from_slice(&u32_bytes(value));
    }
    out.extend_from_slice(&u32_bytes(0));
    out
}

fn gradient(width: u32, height: u32) -> Vec<u8> {
    (0..width * height)
        .flat_map(|index| {
            let (x, y) = (index % width, index / width);
            [x * 37, y * 53, (x + y) * 11, 255 - x * 7].map(|value| value.to_le_bytes()[0])
        })
        .collect()
}

#[test]
fn a_format_is_named_by_its_signature() {
    assert_eq!(
        still_format(&[0xff, 0xd8, 0xff, 0xe0]),
        Some(StillFormat::Jpeg)
    );
    assert_eq!(
        still_format(b"\x89PNG\r\n\x1a\n\0\0\0\x0d"),
        Some(StillFormat::Png)
    );
    assert_eq!(
        still_format(b"RIFF\x10\0\0\0WEBPVP8 "),
        Some(StillFormat::WebP)
    );
    assert_eq!(still_format(b"RIFF\x10\0\0\0WAVEfmt "), None);
    assert_eq!(still_format(b"RIFF\x10\0\0\0WEB"), None);
    assert_eq!(still_format(b"GIF89a"), None);
    assert_eq!(still_format(&[]), None);
    assert!(matches!(
        decode_still(b"GIF89a\x01\0\x01\0"),
        Err(DecodeError::Unsupported(_))
    ));
}

#[test]
fn an_exif_orientation_is_read_in_either_byte_order_and_type() {
    assert_eq!(exif_orientation(&tiff(false, 0x0112, 3, 6)), Some(6));
    assert_eq!(exif_orientation(&tiff(true, 0x0112, 3, 8)), Some(8));
    assert_eq!(exif_orientation(&tiff(true, 0x0112, 4, 3)), Some(3));
    let mut prefixed = b"Exif\0\0".to_vec();
    prefixed.extend_from_slice(&tiff(false, 0x0112, 3, 5));
    assert_eq!(exif_orientation(&prefixed), None);
    assert_eq!(exif_orientation(&tiff(false, 0x0110, 3, 6)), None);
    assert_eq!(exif_orientation(&tiff(false, 0x0112, 2, 6)), None);
    let whole = tiff(false, 0x0112, 3, 6);
    for length in 0..whole.len() - 6 {
        assert_eq!(exif_orientation(&whole[..length]), None, "{length} bytes");
    }
    assert_eq!(exif_orientation(b"XX*\0\x08\0\0\0"), None);
}

#[test]
fn a_jpeg_is_stored_and_oriented_as_its_exif_says() {
    for value in 1..=8_u16 {
        let name = format!("exif-orientation-{value}.jpg");
        let bytes = corpus(&name);
        let expected = Orientation::from_exif(value).expect("Exif orientation");
        let stored = if expected.rotation.swaps_axes() {
            (48, 64)
        } else {
            (64, 48)
        };
        let info = probe_still(&bytes).unwrap_or_else(|error| panic!("{name}: {error:?}"));
        assert_eq!(
            (info.format, (info.width, info.height), info.orientation),
            (StillFormat::Jpeg, stored, expected),
            "{name}"
        );
        let decoded = decode_still(&bytes).unwrap_or_else(|error| panic!("{name}: {error:?}"));
        assert_eq!(decoded.info, info, "{name}");
        assert_eq!(decoded.rgba.len(), 64 * 48 * 4, "{name}");
        assert!(
            decoded
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| pixel[3] == 255)
        );
    }
}

#[test]
fn a_jpeg_exif_segment_is_found_past_other_segments_and_fill_bytes() {
    let tiff = tiff(true, 0x0112, 3, 7);
    let mut bytes = vec![0xff, 0xd8, 0xff, 0xe0, 0, 4, 0, 0, 0xff, 0xff, 0xff, 0xe1];
    bytes.extend_from_slice(&u16::try_from(tiff.len() + 8).expect("small").to_be_bytes());
    bytes.extend_from_slice(b"Exif\0\0");
    bytes.extend_from_slice(&tiff);
    bytes.extend_from_slice(&[0xff, 0xda]);
    assert_eq!(jpeg_exif(&bytes), Some(tiff.as_slice()));
    let scan_first = [0xff, 0xd8, 0xff, 0xda, 0, 2, 0xff, 0xe1];
    assert_eq!(jpeg_exif(&scan_first), None);
    for length in 0..bytes.len() - 2 {
        assert!(jpeg_exif(&bytes[..length]).is_none(), "{length} bytes");
    }
}

#[test]
fn a_png_decodes_to_straight_rgba_from_every_color_type() {
    let rgb = [10, 20, 30, 40, 50, 60];
    let decoded = decode_still(&encoded_png(
        2,
        1,
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        |_| {},
        &rgb,
    ))
    .expect("RGB");
    assert_eq!(decoded.rgba, [10, 20, 30, 255, 40, 50, 60, 255]);
    let gray_alpha = decode_still(&encoded_png(
        2,
        1,
        png::ColorType::GrayscaleAlpha,
        png::BitDepth::Eight,
        |_| {},
        &[7, 100, 9, 0],
    ))
    .expect("gray and alpha");
    assert_eq!(gray_alpha.rgba, [7, 7, 7, 100, 9, 9, 9, 0]);
    let one_bit = decode_still(&encoded_png(
        3,
        1,
        png::ColorType::Grayscale,
        png::BitDepth::One,
        |_| {},
        &[0b1010_0000],
    ))
    .expect("1-bit gray");
    assert_eq!(
        one_bit.rgba,
        [255, 255, 255, 255, 0, 0, 0, 255, 255, 255, 255, 255]
    );
    let palette = decode_still(&encoded_png(
        3,
        1,
        png::ColorType::Indexed,
        png::BitDepth::Two,
        |encoder| {
            encoder.set_palette(vec![1, 2, 3, 4, 5, 6, 7, 8, 9]);
            encoder.set_trns(vec![0, 128]);
        },
        &[0b0001_1000],
    ))
    .expect("palette");
    assert_eq!(palette.rgba, [1, 2, 3, 0, 4, 5, 6, 128, 7, 8, 9, 255]);
    let wide = [
        0xffff_u16, 0xff80, 0x0080, 0x007f, 0x12ff, 0xfedc, 0, 0x0101,
    ];
    let wide_bytes = wide
        .iter()
        .flat_map(|v| v.to_be_bytes())
        .collect::<Vec<_>>();
    let rgba16 = decode_still(&encoded_png(
        2,
        1,
        png::ColorType::Rgba,
        png::BitDepth::Sixteen,
        |_| {},
        &wide_bytes,
    ))
    .expect("16-bit RGBA");
    assert_eq!(rgba16.rgba, [255, 255, 1, 0, 0x13, 0xff, 0, 1]);
    let gray16 = decode_still(&encoded_png(
        1,
        1,
        png::ColorType::Grayscale,
        png::BitDepth::Sixteen,
        |encoder| encoder.set_trns(vec![0x12, 0x34]),
        &[0x12, 0x34],
    ))
    .expect("16-bit gray with tRNS");
    assert_eq!(gray16.rgba, [0x12, 0x12, 0x12, 0]);
}

#[test]
fn an_interlaced_png_decodes_as_its_progressive_twin() {
    let (width, height) = (13, 11);
    let pixels = gradient(width, height);
    let progressive = decode_still(&handmade_png(width, height, &pixels, false, &[])).expect("png");
    let interlaced = decode_still(&handmade_png(width, height, &pixels, true, &[])).expect("adam7");
    assert_eq!(progressive.rgba, pixels);
    assert_eq!(interlaced.rgba, pixels);
}

#[test]
fn a_png_and_a_webp_take_their_exif_orientation() {
    let pixels = gradient(3, 2);
    let exif = tiff(false, 0x0112, 3, 6);
    let png = handmade_png(3, 2, &pixels, false, &[(b"eXIf", &exif)]);
    let info = probe_still(&png).expect("png");
    assert_eq!(info.orientation, Orientation::from_exif(6).expect("6"));
    assert_eq!((info.width, info.height), (3, 2));
    let mut webp = Vec::new();
    let mut encoder = image_webp::WebPEncoder::new(&mut webp);
    encoder.set_exif_metadata(tiff(true, 0x0112, 3, 2));
    encoder
        .encode(&pixels, 3, 2, image_webp::ColorType::Rgba8)
        .expect("lossless WebP");
    let decoded = decode_still(&webp).expect("webp");
    assert_eq!(
        decoded.info.orientation,
        Orientation::from_exif(2).expect("2")
    );
    assert_eq!(decoded.info.format, StillFormat::WebP);
    assert_eq!(decoded.rgba, pixels);
    let opaque = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
        .collect::<Vec<_>>();
    let mut rgb = Vec::new();
    image_webp::WebPEncoder::new(&mut rgb)
        .encode(&opaque, 3, 2, image_webp::ColorType::Rgb8)
        .expect("lossless WebP");
    let decoded = decode_still(&rgb).expect("webp");
    assert_eq!(decoded.info.orientation, Orientation::UPRIGHT);
    let expected = opaque
        .as_chunks::<3>()
        .0
        .iter()
        .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
        .collect::<Vec<_>>();
    assert_eq!(decoded.rgba, expected);
}

#[test]
fn a_picture_past_the_frame_limits_is_refused_from_its_header() {
    // An IHDR alone: the refusal comes before any sample is read.
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend_from_slice(&16_385_u32.to_be_bytes());
    header.extend_from_slice(&1_u32.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    png_chunk(&mut png, *b"IHDR", &header);
    assert!(matches!(
        probe_still(&png),
        Err(DecodeError::Unsupported(_))
    ));
    assert!(matches!(
        decode_still(&png),
        Err(DecodeError::Unsupported(_))
    ));

    let jpeg = corpus("exif-orientation-1.jpg");
    let frame = jpeg
        .windows(2)
        .position(|marker| marker == [0xff, 0xc0])
        .expect("baseline frame header");
    for (width, height) in [(16_384_u16, 16_384_u16), (16_385, 1)] {
        let mut patched = jpeg.clone();
        patched[frame + 5..frame + 7].copy_from_slice(&height.to_be_bytes());
        patched[frame + 7..frame + 9].copy_from_slice(&width.to_be_bytes());
        let refused = decode_still(&patched);
        assert!(
            matches!(refused, Err(DecodeError::Unsupported(_))),
            "{width}x{height}: {refused:?}"
        );
    }

    // VP8L packs width - 1 and height - 1 in 14 bits each after its signature byte.
    let mut webp = b"RIFF\x1a\0\0\0WEBPVP8L\x0e\0\0\0\x2f".to_vec();
    let size = 0x3ffe_u32 | (0x3ffe << 14);
    webp.extend_from_slice(&size.to_le_bytes());
    webp.extend_from_slice(&[0; 9]);
    let probed = probe_still(&webp);
    assert!(
        matches!(probed, Err(DecodeError::Unsupported(_))),
        "{probed:?}"
    );
    assert!(matches!(
        decode_still(&webp),
        Err(DecodeError::Unsupported(_))
    ));
}

#[test]
fn a_decoded_still_is_one_straight_rgba_frame() {
    let decoded = decode_still(&corpus("exif-orientation-6.jpg")).expect("jpeg");
    let frame = decoded.into_video_frame();
    assert_eq!(frame.pixel_format, PixelFormat::Rgba8);
    assert_eq!(
        frame.visible_rect,
        Rect {
            x: 0,
            y: 0,
            width: 48,
            height: 64
        }
    );
    assert_eq!(frame.orientation, Orientation::from_exif(6).expect("6"));
    assert_eq!(frame.sample_aspect_ratio, SampleAspectRatio::default());
    assert_eq!(frame.planes.len(), 1);
    assert_eq!(frame.planes[0].stride, 48 * 4);
    assert_eq!(frame.planes[0].bytes.len(), 48 * 64 * 4);
}

#[test]
fn truncated_stills_fail_without_panicking() {
    let pixels = gradient(5, 4);
    let mut webp = Vec::new();
    image_webp::WebPEncoder::new(&mut webp)
        .encode(&pixels, 5, 4, image_webp::ColorType::Rgba8)
        .expect("lossless WebP");
    for whole in [
        corpus("exif-orientation-1.jpg"),
        handmade_png(5, 4, &pixels, true, &[]),
        webp,
    ] {
        for length in (0..whole.len()).step_by(7) {
            let _ = decode_still(&whole[..length]);
        }
    }
}

#[test]
fn a_decoder_panic_is_a_decode_error() {
    let failed = contained::<()>(StillFormat::WebP, || panic!("huffman overflow"));
    assert!(
        matches!(&failed, Err(DecodeError::Adapter(message)) if message.contains("huffman")),
        "{failed:?}"
    );
}

#[test]
fn an_animated_webp_is_refused_and_an_animated_png_reads_its_default_image() {
    let webp = corpus("extensions/animation.webp");
    assert!(matches!(
        probe_still(&webp),
        Err(DecodeError::Unsupported(_))
    ));
    assert!(matches!(
        decode_still(&webp),
        Err(DecodeError::Unsupported(_))
    ));
    let png = decode_still(&corpus("extensions/animation.png")).expect("APNG");
    assert_eq!((png.info.width, png.info.height), (48, 32));
}

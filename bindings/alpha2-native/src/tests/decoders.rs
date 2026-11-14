use super::*;

#[test]
fn every_listed_decoder_is_in_the_pinned_runtime() {
    crate::require_pinned_ffmpeg_for_tests();
    let present = crate::source_decoders();
    let missing = crate::SOURCE_DECODERS
        .iter()
        .filter(|name| !present.contains(name))
        .collect::<Vec<_>>();
    assert!(missing.is_empty(), "missing decoders: {missing:?}");
}

#[test]
fn a_stream_of_an_unlisted_codec_is_unsupported_unless_every_decoder_is_allowed() {
    // The FFV1 stream relabelled as Cinepak, a codec the runtime has and the list leaves out.
    let mut avi = std::fs::read(corpus("odd-dimensions.avi")).expect("corpus AVI");
    for at in 0..avi.len() - 4 {
        if &avi[at..at + 4] == b"FFV1" {
            avi[at..at + 4].copy_from_slice(b"cvid");
        }
    }
    let path = std::env::temp_dir().join(format!("alpha2-cinepak-{}.avi", std::process::id()));
    std::fs::write(&path, &avi).expect("fixture");
    let probed = probe(&path).expect("probes");
    let video = probed.selected_video_stream().expect("a video stream");
    assert_eq!(video.codec, "cinepak");
    let reason = software_decode_unsupported(video).expect("refused");
    assert!(reason.contains("cinepak"), "{reason}");
    match NativeDecoder::open(&path) {
        Err(DecodeError::Unsupported(reason)) => assert!(reason.contains("cinepak"), "{reason}"),
        other => panic!("opened: {:?}", other.map(drop)),
    }
    crate::decode::set_every_decoder(true);
    let allowed = software_decode_unsupported(video);
    let opened = NativeDecoder::open(&path).map(drop);
    crate::decode::set_every_decoder(false);
    std::fs::remove_file(&path).ok();
    assert_eq!(allowed, None);
    assert!(
        !matches!(&opened, Err(DecodeError::Unsupported(reason)) if reason.contains("cinepak")),
        "{opened:?}"
    );
}

//! The backend conformance suite on `VideoToolbox` and `AudioToolbox`, run on macOS, in the iOS
//! simulator, or on a device with the corpus copied to `ALPHA2_TESTS_DIR`.

// The capability probes call the media frameworks directly.
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::sync::Arc;

use alpha2_backend_conformance::{ContainerBackend, conformance_tests, corpus};
use alpha2_container::{AudioCodec, TrackKind, VideoCodec};
use alpha2_host_core::audio_stream::WithPcm;
use objc2_audio_toolbox::{
    kAudioFormatProperty_AvailableDecodeNumberChannels, kAudioFormatProperty_DecodeFormatIDs,
};
use objc2_core_audio_types::AudioStreamBasicDescription;
use objc2_core_foundation::{CFData, CFRetained, CFString};
use objc2_core_media::{
    CMFormatDescription, CMVideoFormatDescriptionCreate,
    kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms,
};
use objc2_video_toolbox::{
    VTDecompressionSession, kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder,
};

use crate::audio::format_property;
use crate::config::es_descriptor;
use crate::sys::{self, code, dictionary};
use crate::{AppleAudio, AppleVideo};

/// The default `kind` track of the file at `path`.
fn track(path: &Path, kind: TrackKind) -> Option<alpha2_container::TrackInfo> {
    let reader = alpha2_container::open(path).ok()?;
    let index = alpha2_container::default_track(reader.tracks(), kind)?;
    Some(reader.tracks()[index].clone())
}

/// The 32-bit words of an `AudioFormatGetProperty` result.
fn format_words(property: u32, specifier: &[u8]) -> Vec<u32> {
    format_property(property, specifier)
        .unwrap_or_default()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|word| u32::from_ne_bytes(*word))
        .collect()
}

/// Whether `AudioToolbox` lists a decoder for the default audio track of `path` and, when it says,
/// for the track's channel count.
fn system_decodes_audio(path: &Path) -> bool {
    let Some(audio) = track(path, TrackKind::Audio).and_then(|track| track.audio) else {
        return false;
    };
    let id = match audio.codec {
        AudioCodec::Aac => code(*b"aac "),
        AudioCodec::Mp3 => code(*b".mp3"),
        AudioCodec::Opus => code(*b"opus"),
        AudioCodec::Flac => code(*b"flac"),
        AudioCodec::Alac => code(*b"alac"),
        AudioCodec::Ac3 => code(*b"ac-3"),
        AudioCodec::Eac3 => code(*b"ec-3"),
        _ => return false,
    };
    if !format_words(kAudioFormatProperty_DecodeFormatIDs, &[]).contains(&id) {
        return false;
    }
    let format = AudioStreamBasicDescription {
        mSampleRate: f64::from(audio.sample_rate),
        mFormatID: id,
        mFormatFlags: 0,
        mBytesPerPacket: 0,
        mFramesPerPacket: 0,
        mBytesPerFrame: 0,
        mChannelsPerFrame: audio.channels,
        mBitsPerChannel: 0,
        mReserved: 0,
    };
    // SAFETY: the description is plain data read as bytes.
    let specifier = unsafe {
        std::slice::from_raw_parts(
            ptr::from_ref(&format).cast::<u8>(),
            size_of::<AudioStreamBasicDescription>(),
        )
    };
    let channels = format_words(
        kAudioFormatProperty_AvailableDecodeNumberChannels,
        specifier,
    );
    channels.is_empty() || channels.contains(&audio.channels)
}

/// Whether a `VideoToolbox` session, a hardware one when `hardware` asks, opens for the default
/// video track of `path`, from the track's sample description alone.
fn system_decodes_video(path: &Path, hardware: bool) -> bool {
    let Some(video) = track(path, TrackKind::Video).and_then(|track| track.video) else {
        return false;
    };
    let private = &video.codec_private[..];
    let (codec_type, atom, body) = match video.codec {
        VideoCodec::H264 => (*b"avc1", "avcC", private.to_vec()),
        VideoCodec::Hevc => (*b"hvc1", "hvcC", private.to_vec()),
        VideoCodec::Av1 => (*b"av01", "av1C", private.to_vec()),
        VideoCodec::Vp9 => (*b"vp09", "vpcC", private.to_vec()),
        VideoCodec::Mpeg4Part2 => {
            let mut esds = vec![0; 4];
            esds.extend_from_slice(&es_descriptor(0x20, 4, private));
            (*b"mp4v", "esds", esds)
        }
        _ => return false,
    };
    if let Some(register) = sys::symbol(c"VTRegisterSupplementalVideoDecoderIfAvailable") {
        // SAFETY: the symbol is `void VTRegisterSupplementalVideoDecoderIfAvailable(CMVideoCodecType)`.
        let register: unsafe extern "C" fn(u32) = unsafe { std::mem::transmute(register) };
        // SAFETY: any codec type is accepted.
        unsafe { register(code(codec_type)) };
    }
    let atoms = dictionary(&[(&CFString::from_str(atom), &CFData::from_bytes(&body))]);
    // SAFETY: the key is a framework constant.
    let key = unsafe { kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms };
    let extensions = dictionary(&[(key, &atoms)]);
    let mut description: *const CMFormatDescription = ptr::null();
    // SAFETY: `extensions` is a property-list dictionary and `description` a valid out pointer.
    let status = unsafe {
        CMVideoFormatDescriptionCreate(
            None,
            code(codec_type),
            i32::try_from(video.width).unwrap_or(i32::MAX),
            i32::try_from(video.height).unwrap_or(i32::MAX),
            Some(extensions.as_opaque()),
            NonNull::from(&mut description),
        )
    };
    let Some(description) = NonNull::new(description.cast_mut()).filter(|_| status == 0) else {
        return false;
    };
    // SAFETY: a successful create returns a +1 reference.
    let description = unsafe { CFRetained::from_raw(description) };
    // SAFETY: a framework constant.
    let require = unsafe { kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder };
    let specification = dictionary(&[(require, sys::boolean(true))]);
    let mut session: *mut VTDecompressionSession = ptr::null_mut();
    // SAFETY: the specification holds the documented value type, no callback is needed for a
    // session that decodes nothing, and `session` is a valid out pointer.
    let status = unsafe {
        VTDecompressionSession::create(
            None,
            &description,
            hardware.then(|| specification.as_opaque()),
            None,
            ptr::null(),
            NonNull::from(&mut session),
        )
    };
    let Some(session) = NonNull::new(session) else {
        return false;
    };
    // SAFETY: a successful create returns a +1 reference.
    let session = unsafe { CFRetained::from_raw(session) };
    // SAFETY: the session is live and not used after this.
    unsafe { session.invalidate() };
    status == 0
}

/// The fixtures whether a system decodes depends on, and the probe that asks it.
enum Probe {
    Audio,
    /// Any video decoder.
    Video,
    /// A hardware video decoder: the software H.264 decoder opens sessions for streams beyond
    /// 8-bit 4:2:0 that it cannot decode, which the backend refuses.
    HardwareVideo,
}

fn probe(name: &str) -> Option<Probe> {
    Some(match name {
        "av1.mp4" | "av1-colocated.mp4" | "cfr-bt709-limited.mp4" | "long-gop.mp4" => Probe::Video,
        "hdr-pq.mp4" | "hdr-pq-topleft.mp4" => Probe::HardwareVideo,
        "audio-ac3-5.1.mp4"
        | "audio-eac3-5.0.mp4"
        | "audio-aac-3.0.mp4"
        | "audio-adts-3.0.aac"
        | "audio-aac-quad.mp4"
        | "audio-aac-5.1.mp4"
        | "audio-aac-7.1.mp4"
        | "audio-opus-5.1.ogg"
        | "audio-opus-5.1.webm"
        | "audio-dts-5.1.mkv" => Probe::Audio,
        _ => return None,
    })
}

/// Whether the system decodes the file at `path`, for a fixture `probe` names.
fn system_decodes(path: &Path, probe: &Probe) -> bool {
    match probe {
        Probe::Audio => system_decodes_audio(path),
        Probe::Video => system_decodes_video(path, false),
        Probe::HardwareVideo => system_decodes_video(path, true),
    }
}

/// Refuses only what the container layer does not read and what the system itself declares no
/// decoder for, so the backend must open every fixture the system decodes.
fn refuses(name: &str) -> Option<&'static str> {
    match name.rsplit('.').next().unwrap_or_default() {
        "ts" | "avi" => return Some("the container layer reads no MPEG-TS or AVI"),
        _ => {}
    }
    let probe = probe(name)?;
    (!system_decodes(&corpus(name), &probe)).then_some("this system declares no decoder for it")
}

fn apple() -> ContainerBackend {
    ContainerBackend::new(
        "apple",
        Arc::new(AppleVideo),
        Arc::new(WithPcm(AppleAudio)),
        refuses,
    )
}

conformance_tests!(apple());

#[test]
fn every_probed_fixture_the_system_decodes_opens() {
    use alpha2_host_core::audio_stream::AudioBackends;
    use alpha2_host_core::video_stream::{BackendKind, VideoBackends};
    let names = [
        "av1.mp4",
        "av1-colocated.mp4",
        "cfr-bt709-limited.mp4",
        "long-gop.mp4",
        "hdr-pq.mp4",
        "hdr-pq-topleft.mp4",
        "audio-ac3-5.1.mp4",
        "audio-eac3-5.0.mp4",
        "audio-aac-3.0.mp4",
        "audio-adts-3.0.aac",
        "audio-aac-quad.mp4",
        "audio-aac-5.1.mp4",
        "audio-aac-7.1.mp4",
        "audio-opus-5.1.ogg",
        "audio-opus-5.1.webm",
        "audio-dts-5.1.mkv",
    ];
    let mut failures = Vec::new();
    for name in names {
        let probe = probe(name).expect("a probed fixture");
        let path = corpus(name);
        let opened = match probe {
            Probe::Audio => track(&path, TrackKind::Audio)
                .map(|track| AppleAudio.open(&track).map(drop).map_err(|e| e.to_string())),
            Probe::Video | Probe::HardwareVideo => track(&path, TrackKind::Video).map(|track| {
                AppleVideo
                    .open(&track, BackendKind::Hardware)
                    .or_else(|_| AppleVideo.open(&track, BackendKind::Software))
                    .map(drop)
                    .map_err(|e| e.to_string())
            }),
        }
        .unwrap_or_else(|| Err("no track".into()));
        match (system_decodes(&path, &probe), opened) {
            (true, Err(error)) => failures.push(format!("{name}: the system decodes it: {error}")),
            (false, Ok(())) => failures.push(format!("{name}: opened without a system decoder")),
            _ => {}
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Samples by which a decoded tone of `frequency` Hz lags a sine starting at sample 0, measured
/// over 0.1 s to 0.3 s.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn tone_offset(samples: &[f32], frequency: f64, rate: u32) -> f64 {
    let step = std::f64::consts::TAU * frequency / f64::from(rate);
    let window = rate as usize / 10..rate as usize * 3 / 10;
    let (mut sine, mut cosine) = (0.0, 0.0);
    for (index, sample) in samples
        .iter()
        .enumerate()
        .take(window.end)
        .skip(window.start)
    {
        let angle = step * index as f64;
        sine += f64::from(*sample) * angle.sin();
        cosine += f64::from(*sample) * angle.cos();
    }
    -cosine.atan2(sine) / step
}

/// A stereo libopus stream with a 300 Hz tone on the left and 500 Hz on the right, written to a
/// temporary file.
fn stereo_opus() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "alpha2-apple-opus-stereo-{}.ogg",
        std::process::id()
    ));
    std::fs::write(&path, include_bytes!("testdata/audio-opus-stereo.ogg")).expect("temp file");
    path
}

#[test]
fn decoded_tones_keep_their_timing() {
    use alpha2_host_core::audio_stream::AudioStream;
    use alpha2_media::{CancelToken, DecodeEvent, Decoder, Speaker};
    let stereo = stereo_opus();
    // Codecs whose decoders drop leading samples or hold some back, and some that do neither.
    let cases = [
        ("audio-ac3-5.1.mp4", Speaker::FrontLeft, 300.0, 48_000),
        ("audio-eac3-5.0.mp4", Speaker::FrontLeft, 300.0, 48_000),
        ("audio-opus-5.1.ogg", Speaker::FrontLeft, 300.0, 48_000),
        ("audio-opus-stereo.ogg", Speaker::FrontRight, 500.0, 48_000),
        ("audio-mp3-lame.mp3", Speaker::FrontCenter, 440.0, 44_100),
        ("audio-aac-5.1.mp4", Speaker::FrontLeft, 300.0, 48_000),
        ("audio-flac-4.0.flac", Speaker::FrontLeft, 300.0, 48_000),
        ("audio-vorbis-5.1.ogg", Speaker::FrontLeft, 300.0, 48_000),
    ];
    let mut failures = Vec::new();
    for (name, speaker, frequency, rate) in cases {
        let path = if name == "audio-opus-stereo.ogg" {
            stereo.clone()
        } else {
            corpus(name)
        };
        if probe(name).is_some_and(|probe| !system_decodes(&path, &probe)) {
            continue;
        }
        let mut stream = AudioStream::open_file(
            &path,
            &WithPcm(AppleAudio),
            rate,
            None,
            alpha2_host_core::DecodePolicy::default(),
        )
        .unwrap_or_else(|error| panic!("{name}: {error}"));
        let channel = stream
            .speakers()
            .speakers()
            .iter()
            .position(|named| *named == speaker)
            .expect("speaker");
        // The tone starts with the stream's first sample, after any skipped priming.
        let mut first = None;
        let mut samples = Vec::new();
        while let DecodeEvent::Audio(block) = stream
            .decode_next(&CancelToken::default())
            .unwrap_or_else(|error| panic!("{name}: {error}"))
        {
            let start = *first.get_or_insert(block.start_sample);
            let at = i64::try_from(samples.len()).expect("small");
            assert_eq!(block.start_sample - start, at, "{name}");
            samples.extend_from_slice(&block.channels[channel]);
        }
        let offset = tone_offset(&samples, frequency, rate);
        if offset.abs() > 1.0 {
            failures.push(format!("{name}: the tone is {offset:.2} samples late"));
        }
    }
    let _ = std::fs::remove_file(&stereo);
    assert!(failures.is_empty(), "{failures:#?}");
}


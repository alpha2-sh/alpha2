use std::path::PathBuf;

use alpha2_media::{ColorMetadata, SampleAspectRatio};
use alpha2_time::FrameRate;

use super::encoder::{
    OpenH264Rate, OpenH264Settings, annex_b_units, avc_decoder_configuration, encoder_params,
    length_prefixed, slice_count, thread_count,
};
use super::ffi::EncParamExt;
use super::library::{Lookup, OpenH264Provider, OpenH264Status, cached_library_path, resolve};

fn settings(rate: OpenH264Rate) -> OpenH264Settings {
    OpenH264Settings {
        width: 1920,
        height: 1080,
        frame_rate: FrameRate::new(30_000, 1001).unwrap(),
        color: ColorMetadata::EXPORT,
        sample_aspect_ratio: SampleAspectRatio::default(),
        rate,
        keyframe_interval: 60,
    }
}

#[test]
fn slices_follow_the_height_and_threads_follow_the_slices() {
    assert_eq!(slice_count(144), 1);
    assert_eq!(slice_count(539), 1);
    assert_eq!(slice_count(720), 2);
    assert_eq!(slice_count(1080), 4);
    assert_eq!(slice_count(1920), 7);
    assert_eq!(slice_count(4320), 8);
    assert_eq!(thread_count(slice_count(1080)), 4);
    assert_eq!(thread_count(1), 1);
}

#[test]
fn a_quantizer_turns_rate_control_off_and_sets_the_layer_qp() {
    let params = encoder_params(
        &EncParamExt::zeroed(),
        &settings(OpenH264Rate::Quantizer(27)),
    )
    .unwrap();
    assert_eq!(params.rc_mode, -1, "RC_OFF_MODE");
    assert_eq!(params.spatial_layers[0].d_layer_qp, 27);
    assert_eq!(params.target_bitrate, 0);
    assert_eq!((params.min_qp, params.max_qp), (1, 51));
    assert_eq!(params.spatial_layers[0].profile_idc, 100, "PRO_HIGH");
    assert_eq!(params.entropy_coding_mode_flag, 1, "CABAC");
    assert_eq!(params.complexity_mode, 1, "MEDIUM_COMPLEXITY");
    assert!(!params.enable_frame_skip);
    assert!(!params.use_load_balancing);
    assert_eq!(params.intra_period, 60);
    assert_eq!(params.sps_pps_id_strategy, 0, "CONSTANT_ID");
    assert_eq!(
        (params.spatial_layer_num, params.temporal_layer_num),
        (1, 1)
    );
    assert_eq!(params.multiple_thread_idc, 4);
    let slices = params.spatial_layers[0].slice_argument;
    assert_eq!(
        (slices.slice_mode, slices.slice_num),
        (1, 4),
        "SM_FIXEDSLCNUM_SLICE"
    );
    assert!((params.max_frame_rate - 29.97).abs() < 0.001);
}

#[test]
fn a_bitrate_runs_the_bitrate_mode_over_the_full_qp_range() {
    let variable = encoder_params(
        &EncParamExt::zeroed(),
        &settings(OpenH264Rate::Bitrate {
            target: 2_000_000,
            constant: false,
        }),
    )
    .unwrap();
    assert_eq!(variable.rc_mode, 1, "RC_BITRATE_MODE");
    assert_eq!(variable.target_bitrate, 2_000_000);
    assert_eq!(variable.spatial_layers[0].spatial_bitrate, 2_000_000);
    assert_eq!(variable.max_bitrate, 0, "a variable rate has no cap");
    assert_eq!((variable.min_qp, variable.max_qp), (1, 51));
    let constant = encoder_params(
        &EncParamExt::zeroed(),
        &settings(OpenH264Rate::Bitrate {
            target: 2_000_000,
            constant: true,
        }),
    )
    .unwrap();
    assert_eq!(constant.max_bitrate, 2_000_000);
    assert_eq!(constant.spatial_layers[0].max_spatial_bitrate, 2_000_000);
    let too_high = encoder_params(
        &EncParamExt::zeroed(),
        &settings(OpenH264Rate::Bitrate {
            target: u64::MAX,
            constant: false,
        }),
    );
    assert!(too_high.is_err());
}

#[test]
fn one_slice_pictures_run_one_thread() {
    let mut small = settings(OpenH264Rate::Quantizer(31));
    small.height = 360;
    small.width = 640;
    let params = encoder_params(&EncParamExt::zeroed(), &small).unwrap();
    assert_eq!(params.multiple_thread_idc, 1);
    assert_eq!(params.spatial_layers[0].slice_argument.slice_mode, 0);
}

#[test]
fn the_vui_carries_the_export_colour_and_the_pixel_aspect() {
    let mut anamorphic = settings(OpenH264Rate::Quantizer(27));
    anamorphic.sample_aspect_ratio = SampleAspectRatio { num: 32, den: 24 };
    let params = encoder_params(&EncParamExt::zeroed(), &anamorphic).unwrap();
    let layer = params.spatial_layers[0];
    assert!(layer.video_signal_type_present);
    assert!(layer.color_description_present);
    assert!(!layer.full_range);
    // BT.709 primaries, transfer and matrix are H.273 code 1.
    assert_eq!(
        (
            layer.color_primaries,
            layer.transfer_characteristics,
            layer.color_matrix
        ),
        (1, 1, 1)
    );
    assert!(layer.aspect_ratio_present);
    assert_eq!(layer.aspect_ratio, 255, "ASP_EXT_SAR");
    assert_eq!(
        (layer.aspect_ratio_ext_width, layer.aspect_ratio_ext_height),
        (4, 3)
    );
    let square = encoder_params(
        &EncParamExt::zeroed(),
        &settings(OpenH264Rate::Quantizer(27)),
    )
    .unwrap();
    assert!(!square.spatial_layers[0].aspect_ratio_present);
}

#[test]
fn annex_b_splits_on_three_and_four_byte_start_codes() {
    let stream = [
        0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0xac, //
        0, 0, 1, 0x68, 0xee, 0x3c, 0x80, //
        0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x03, 0x00, 0x00,
    ];
    let units = annex_b_units(&stream);
    assert_eq!(
        units,
        vec![
            &[0x67, 0x64, 0x00, 0x28, 0xac][..],
            &[0x68, 0xee, 0x3c, 0x80][..],
            &[0x65, 0x88, 0x84, 0x00, 0x03][..],
        ]
    );
    assert_eq!(annex_b_units(&[]), Vec::<&[u8]>::new());
    assert_eq!(annex_b_units(&[0, 0, 0, 1]), Vec::<&[u8]>::new());
}

#[test]
fn units_become_four_byte_length_prefixed() {
    let packet = length_prefixed(&[&[0x65, 0x88], &[0x41]]).unwrap();
    assert_eq!(packet, [0, 0, 0, 2, 0x65, 0x88, 0, 0, 0, 1, 0x41]);
}

#[test]
fn avcc_names_the_sps_profile_and_level_and_carries_the_high_extension() {
    let sps = [0x67, 100, 0x00, 40, 0xac, 0xd9];
    let pps = [0x68, 0xeb, 0xe3, 0xcb];
    let record = avc_decoder_configuration(&sps, &[&pps]).unwrap();
    assert_eq!(
        record,
        [
            1, 100, 0x00, 40, 0xff, 0xe1, 0, 6, 0x67, 100, 0x00, 40, 0xac, 0xd9, 1, 0, 4, 0x68,
            0xeb, 0xe3, 0xcb, 0xfd, 0xf8, 0xf8, 0
        ]
    );
    let baseline = [0x67, 66, 0xc0, 30, 0x95];
    let record = avc_decoder_configuration(&baseline, &[&pps]).unwrap();
    assert_eq!(record.len(), 6 + 2 + baseline.len() + 1 + 2 + pps.len());
    assert!(avc_decoder_configuration(&[0x67, 100], &[&pps]).is_err());
}

#[test]
fn turned_off_it_loads_nothing() {
    let status = resolve(&Lookup {
        enabled: Some("0".into()),
        library: Some(PathBuf::from("/nonexistent/libopenh264.so")),
        unverified: false,
        cache: None,
        system: true,
    });
    assert!(matches!(status, OpenH264Status::Disabled));
    assert_eq!(status.remedy(), None);
    assert!(status.reason().unwrap().contains("ALPHA2_OPENH264=0"));
}

#[test]
fn nothing_installed_says_where_it_looked_and_how_to_install() {
    let cache = tempdir("nothing-installed");
    let status = resolve(&Lookup {
        enabled: None,
        library: None,
        unverified: false,
        cache: Some(cache.clone()),
        system: false,
    });
    let OpenH264Status::NotInstalled(reason) = &status else {
        panic!("{status:?}");
    };
    if let Some(path) = cached_library_path(&cache) {
        assert!(reason.contains(&path.display().to_string()), "{reason}");
        assert!(status.remedy().unwrap().contains("install --openh264"));
    }
    std::fs::remove_dir_all(cache).unwrap();
}

#[test]
fn a_library_with_another_hash_is_refused_before_it_is_loaded() {
    let dir = tempdir("wrong-hash");
    let path = dir.join("libopenh264.so");
    // Not even a shared object: loading it would fail differently, so the message proves the hash
    // was checked first.
    std::fs::write(&path, b"not Cisco's binary").unwrap();
    let status = resolve(&Lookup {
        enabled: None,
        library: Some(path.clone()),
        unverified: false,
        cache: None,
        system: false,
    });
    let OpenH264Status::Unusable(reason) = &status else {
        panic!("{status:?}");
    };
    if cached_library_path(&dir).is_some() {
        assert!(reason.contains("is not Cisco's OpenH264"), "{reason}");
        assert!(reason.contains("SHA-256"), "{reason}");
    }
    // The same file in the cache is refused the same way, and the remedy is a reinstall.
    if let Some(cached) = cached_library_path(&dir) {
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::copy(&path, &cached).unwrap();
        let status = resolve(&Lookup {
            enabled: None,
            library: None,
            unverified: false,
            cache: Some(dir.clone()),
            system: false,
        });
        assert!(matches!(status, OpenH264Status::Unusable(_)), "{status:?}");
        assert!(status.remedy().unwrap().contains("--force"));
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_unverified_file_that_is_not_a_library_is_reported_with_the_loader_message() {
    let dir = tempdir("not-a-library");
    let path = dir.join("libopenh264.so");
    std::fs::write(&path, b"not a shared object").unwrap();
    let status = resolve(&Lookup {
        enabled: None,
        library: Some(path),
        unverified: true,
        cache: None,
        system: false,
    });
    let OpenH264Status::Unusable(reason) = &status else {
        panic!("{status:?}");
    };
    assert!(reason.contains("was found but is not loadable"), "{reason}");
    std::fs::remove_dir_all(dir).unwrap();
}

/// A shared object built from `source` with the system C compiler, or `None`
/// where there is none (the tests that need one say so and pass).
#[cfg(unix)]
fn fake_library(dir: &std::path::Path, name: &str, source: &str) -> Option<PathBuf> {
    let c = dir.join(format!("{name}.c"));
    std::fs::write(&c, source).unwrap();
    let library = dir.join(format!("{name}.so"));
    let built = {
        let _child = crate::tests::child_process_lock();
        std::process::Command::new(std::env::var("CC").unwrap_or_else(|_| "cc".into()))
            .args(["-shared", "-fPIC", "-o"])
            .arg(&library)
            .arg(&c)
            .status()
            .ok()?
    };
    built.success().then_some(library)
}

#[cfg(unix)]
const FAKE_ENCODER: &str = "
typedef struct { unsigned major, minor, revision, reserved; } Version;
int WelsCreateSVCEncoder(void **encoder) { *encoder = 0; return 1; }
void WelsDestroySVCEncoder(void *encoder) { (void)encoder; }
void WelsGetCodecVersionEx(Version *version) {
  version->major = MAJOR; version->minor = MINOR; version->revision = 0; version->reserved = 0;
}
";

#[cfg(unix)]
#[test]
fn a_library_of_another_api_version_or_without_the_entry_points_is_refused() {
    let dir = tempdir("fake-libraries");
    let unverified = |path: PathBuf| {
        resolve(&Lookup {
            enabled: None,
            library: Some(path),
            unverified: true,
            cache: None,
            system: false,
        })
    };
    let Some(newer) = fake_library(
        &dir,
        "newer",
        &format!("#define MAJOR 3\n#define MINOR 0\n{FAKE_ENCODER}"),
    ) else {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::CCompiler,
            "no C compiler: the fake-library cases did not run"
        );
        return;
    };
    let OpenH264Status::Unusable(reason) = unverified(newer) else {
        panic!("a 3.0 library loaded");
    };
    assert!(reason.contains("is OpenH264 3.0.0"), "{reason}");
    let compatible = fake_library(
        &dir,
        "compatible",
        &format!("#define MAJOR 2\n#define MINOR 6\n{FAKE_ENCODER}"),
    )
    .unwrap();
    match unverified(compatible) {
        OpenH264Status::Available(library) => {
            assert_eq!(library.version(), "2.6.0");
            assert_eq!(library.provider(), OpenH264Provider::System);
        }
        other => panic!("{other:?}"),
    }
    let decoder_only = fake_library(
        &dir,
        "decoder",
        "int WelsCreateDecoder(void **d) { return 0; }",
    )
    .unwrap();
    let OpenH264Status::Unusable(reason) = unverified(decoder_only) else {
        panic!("a library without the encoder loaded");
    };
    assert!(
        reason.contains("is not an OpenH264 encoder library (no WelsCreateSVCEncoder"),
        "{reason}"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// Cisco's binary, when the run names it: CI's `OpenH264` job downloads it and
/// sets `ALPHA2_OPENH264_LIBRARY`.
#[test]
fn cisco_s_binary_loads_as_itself() {
    let Some(path) = std::env::var_os("ALPHA2_OPENH264_LIBRARY") else {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::Openh264,
            "ALPHA2_OPENH264_LIBRARY is not set: Cisco's binary was not loaded"
        );
        return;
    };
    let status = resolve(&Lookup {
        enabled: None,
        library: Some(PathBuf::from(path)),
        unverified: false,
        cache: None,
        system: false,
    });
    let OpenH264Status::Available(library) = status else {
        panic!("{status:?}");
    };
    assert_eq!(library.provider(), OpenH264Provider::Cisco);
    assert_eq!(library.version(), super::OPENH264_VERSION);
}

fn tempdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "alpha2-openh264-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

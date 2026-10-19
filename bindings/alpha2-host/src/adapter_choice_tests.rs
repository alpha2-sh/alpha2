use super::*;

/// A hardware adapter that renders, with what it decodes and encodes.
fn adapter(index: usize, name: &str, decodes: bool, encodes: &[NativeVideoCodec]) -> AdapterRecord {
    AdapterRecord {
        index,
        name: name.to_owned(),
        software: false,
        renders: Ok(()),
        decodes: if decodes {
            Ok(())
        } else {
            Err("D3D12 adapter does not expose NV12 plane views".to_owned())
        },
        encodes: ENCODE_CODECS
            .iter()
            .map(|codec| {
                (
                    *codec,
                    if encodes.contains(codec) {
                        Ok(())
                    } else {
                        Err(format!(
                            "{name} reports no D3D12 video encoder for {}",
                            codec.as_str()
                        ))
                    },
                )
            })
            .collect(),
    }
}

fn warp(index: usize) -> AdapterRecord {
    AdapterRecord {
        software: true,
        ..adapter(index, "Microsoft Basic Render Driver", false, &[])
    }
}

const H264: NativeVideoCodec = NativeVideoCodec::H264;
const HEVC: NativeVideoCodec = NativeVideoCodec::Hevc;

/// A hybrid laptop whose discrete GPU (first in high-performance order)
/// has neither a D3D12VA decoder nor an encoder, and whose integrated
/// one has both.
fn hybrid() -> Vec<AdapterRecord> {
    vec![
        adapter(1, "NVIDIA GeForce MX450", false, &[]),
        adapter(0, "Intel(R) Iris(R) Xe Graphics", true, &[H264, HEVC]),
        warp(2),
    ]
}

#[test]
fn the_setting_parses_into_a_policy() {
    for (value, policy) in [
        (None, AdapterPolicy::Default),
        (Some(""), AdapterPolicy::Default),
        (Some("default"), AdapterPolicy::Default),
        (Some("encoder"), AdapterPolicy::Encoder),
        (Some(" 1 "), AdapterPolicy::Index(1)),
        (Some("Iris"), AdapterPolicy::Name("Iris".to_owned())),
    ] {
        assert_eq!(AdapterPolicy::parse(value), policy, "{value:?}");
    }
    assert!(AdapterPolicy::Index(0).is_pin());
    assert!(!AdapterPolicy::Encoder.is_pin());
}

/// `default` ignores encoders: the first adapter decode accepts when the
/// decode policy allows the GPU, and the first that renders when it does
/// not.
#[test]
fn default_takes_the_first_adapter_the_decode_policy_accepts() {
    let records = hybrid();
    assert_eq!(
        choose_adapter(&records, &AdapterPolicy::Default, true, H264),
        Ok(1)
    );
    assert_eq!(
        choose_adapter(&records, &AdapterPolicy::Default, false, H264),
        Ok(0)
    );
    // No adapter decodes: the preferred one renders, decoding in software.
    let mut none_decode = hybrid();
    none_decode[1].decodes = Err("no".to_owned());
    assert_eq!(
        choose_adapter(&none_decode, &AdapterPolicy::Default, true, H264),
        Ok(0)
    );
    let mut lost = hybrid();
    lost[0].renders = Err("device lost".to_owned());
    assert_eq!(
        choose_adapter(&lost, &AdapterPolicy::Default, false, H264),
        Ok(1)
    );
}

/// `encoder` renders where the default codec encodes, preferring an
/// adapter that also decodes, and is `default` where none encodes.
#[test]
fn encoder_takes_the_first_adapter_that_encodes_the_default_codec() {
    let records = hybrid();
    assert_eq!(
        choose_adapter(&records, &AdapterPolicy::Encoder, false, H264),
        Ok(1)
    );
    let both = vec![
        adapter(1, "NVIDIA GeForce RTX 4060", true, &[H264, HEVC]),
        adapter(0, "Intel(R) UHD Graphics", true, &[H264]),
    ];
    assert_eq!(
        choose_adapter(&both, &AdapterPolicy::Encoder, true, H264),
        Ok(0)
    );
    // Of two that encode, the one that also decodes, when decode wants the GPU.
    let one_decodes = vec![
        adapter(0, "A", false, &[H264]),
        adapter(1, "B", true, &[H264]),
    ];
    assert_eq!(
        choose_adapter(&one_decodes, &AdapterPolicy::Encoder, true, H264),
        Ok(1)
    );
    assert_eq!(
        choose_adapter(&one_decodes, &AdapterPolicy::Encoder, false, H264),
        Ok(0)
    );
    let hevc_only = vec![
        adapter(0, "A", false, &[HEVC]),
        adapter(1, "B", true, &[HEVC]),
    ];
    assert_eq!(
        choose_adapter(&hevc_only, &AdapterPolicy::Encoder, true, H264),
        Ok(1)
    );
    assert_eq!(
        choose_adapter(&hevc_only, &AdapterPolicy::Encoder, false, H264),
        Ok(0)
    );
}

/// A pin names one adapter, by index or name, whatever it can do, and
/// one that matches nothing refuses, listing what there is.
#[test]
fn a_pin_takes_the_adapter_it_names_and_refuses_one_that_names_none() {
    let records = hybrid();
    assert_eq!(
        choose_adapter(&records, &AdapterPolicy::Index(1), true, H264),
        Ok(0)
    );
    assert_eq!(
        choose_adapter(
            &records,
            &AdapterPolicy::Name("iris".to_owned()),
            false,
            H264
        ),
        Ok(1)
    );
    assert_eq!(
        choose_adapter(
            &records,
            &AdapterPolicy::Name("basic render".to_owned()),
            true,
            H264
        ),
        Ok(2)
    );
    let refused = choose_adapter(
        &records,
        &AdapterPolicy::Name("Radeon".to_owned()),
        true,
        H264,
    )
    .expect_err("a pin that matches nothing");
    assert!(
        refused.contains("ALPHA2_GPU_ADAPTER=Radeon matches no DX12 adapter"),
        "{refused}"
    );
    assert!(
        refused.contains("[1] NVIDIA GeForce MX450, [0] Intel(R) Iris(R) Xe Graphics"),
        "{refused}"
    );
    let refused = choose_adapter(&records, &AdapterPolicy::Index(7), true, H264)
        .expect_err("an index past the last adapter");
    assert!(
        refused.contains("ALPHA2_GPU_ADAPTER=7 matches no DX12 adapter"),
        "{refused}"
    );
    let mut lost = hybrid();
    lost[1].renders = Err("device lost".to_owned());
    let refused = choose_adapter(&lost, &AdapterPolicy::Index(0), true, H264)
        .expect_err("a pinned adapter that does not render");
    assert!(
        refused.contains("does not render (device lost)"),
        "{refused}"
    );
}

/// Software adapters come after every hardware one, wherever they are
/// listed, are never taken for decode or encode, and render only when
/// nothing else does.
#[test]
fn software_adapters_come_last() {
    let records = vec![warp(2), adapter(0, "GPU", false, &[])];
    assert_eq!(
        choose_adapter(&records, &AdapterPolicy::Default, true, H264),
        Ok(1)
    );
    let mut warp_decodes = vec![warp(2), adapter(0, "GPU", false, &[])];
    warp_decodes[0].decodes = Ok(());
    warp_decodes[0].encodes = vec![(H264, Ok(()))];
    assert_eq!(
        choose_adapter(&warp_decodes, &AdapterPolicy::Default, true, H264),
        Ok(1)
    );
    assert_eq!(
        choose_adapter(&warp_decodes, &AdapterPolicy::Encoder, true, H264),
        Ok(1)
    );
    let mut only_warp = vec![adapter(0, "GPU", false, &[]), warp(1)];
    only_warp[0].renders = Err("no device".to_owned());
    assert_eq!(
        choose_adapter(&only_warp, &AdapterPolicy::Encoder, true, H264),
        Ok(1)
    );
    assert!(
        only_warp[1].summary().contains("(a software adapter)"),
        "{}",
        only_warp[1].summary()
    );
    only_warp[1].renders = Err("no device".to_owned());
    let refused = choose_adapter(&only_warp, &AdapterPolicy::Default, false, H264)
        .expect_err("no adapter renders");
    assert!(refused.starts_with("no DX12 adapter renders"), "{refused}");
}

/// On the hybrid laptop the reason names the adapter that could encode,
/// and the setting that renders there: `encoder` for the default codec,
/// where it would take that adapter, and its index for any other.
#[test]
fn the_reason_names_another_adapter_that_could_encode() {
    let records = hybrid();
    let elsewhere = encode_elsewhere(&records, 0, false);
    assert_eq!(
        elsewhere,
        vec![
            (
                H264,
                "Intel(R) Iris(R) Xe Graphics reports a D3D12 video encoder for h264 \
                     (ALPHA2_GPU_ADAPTER=encoder renders there)"
                    .to_owned()
            ),
            (
                HEVC,
                "Intel(R) Iris(R) Xe Graphics reports a D3D12 video encoder for hevc \
                     (ALPHA2_GPU_ADAPTER=0 renders there)"
                    .to_owned()
            ),
        ]
    );
    let notes = EncodeAdapterNotes {
        no_device: None,
        elsewhere,
    };
    let reason = notes.explain(H264, "NVIDIA GeForce MX450 has no encoder".to_owned());
    assert!(
        reason.starts_with("NVIDIA GeForce MX450 has no encoder; Intel(R) Iris(R) Xe"),
        "{reason}"
    );
    assert_eq!(notes.explain(NativeVideoCodec::Av1, "x".to_owned()), "x");
    assert_eq!(encode_elsewhere(&records, 1, true), Vec::new());
    let mut warp_encodes = hybrid();
    warp_encodes[1].encodes.clear();
    warp_encodes[2].encodes = vec![(H264, Ok(()))];
    assert_eq!(encode_elsewhere(&warp_encodes, 0, false), Vec::new());
}

/// Decode's reason lists each adapter the choice considered; a pin lists
/// its own.
#[test]
fn the_decode_reason_lists_every_adapter_considered() {
    let mut records = hybrid();
    records[1].decodes = Err("no video device".to_owned());
    assert_eq!(
        decode_refusals(&records, 0, false),
        "NVIDIA GeForce MX450: D3D12 adapter does not expose NV12 plane views; \
             Intel(R) Iris(R) Xe Graphics: no video device"
    );
    assert_eq!(
        decode_refusals(&records, 1, true),
        "Intel(R) Iris(R) Xe Graphics: no video device"
    );
}

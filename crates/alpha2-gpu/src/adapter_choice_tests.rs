use super::*;

/// A hardware adapter that renders, with what it decodes and encodes on the GPU.
fn adapter(index: usize, name: &str, decodes: bool, encodes: &[&'static str]) -> AdapterRecord {
    AdapterRecord {
        decodes: if decodes {
            Ok(())
        } else {
            Err("D3D12 adapter does not expose NV12 plane views".to_owned())
        },
        renders: Ok(()),
        gpu_encodes: GPU_ENCODE_CODECS
            .iter()
            .map(|&codec| {
                (
                    codec,
                    if encodes.contains(&codec) {
                        Ok(())
                    } else {
                        Err(format!("{name}: ID3D12VideoDevice unavailable"))
                    },
                )
            })
            .collect(),
        ..AdapterRecord::unasked(index, name.to_owned(), "dx12", false)
    }
}

fn warp(index: usize) -> AdapterRecord {
    AdapterRecord {
        software: true,
        ..adapter(index, "Microsoft Basic Render Driver", false, &[])
    }
}

/// A hybrid laptop as the high-performance order lists it: the discrete GPU, whose D3D12 device
/// has no video device, first; the integrated one, which decodes and encodes, second.
fn hybrid() -> Vec<AdapterRecord> {
    vec![
        adapter(1, "NVIDIA GeForce MX450", false, &[]),
        adapter(0, "Intel(R) Iris(R) Xe Graphics", true, &["h264", "hevc"]),
        warp(2),
    ]
}

fn first(records: &[AdapterRecord], policy: &AdapterPolicy, decode: bool) -> (usize, ChoiceReason) {
    let order = rank_adapters(records, policy, decode).expect("an adapter");
    (order.order[0], order.reason)
}

#[test]
fn auto_takes_the_adapter_that_encodes_most_codecs() {
    let records = hybrid();
    for decode in [false, true] {
        assert_eq!(
            first(&records, &AdapterPolicy::Auto, decode),
            (1, ChoiceReason::MostGpuEncoders),
            "decode {decode}"
        );
    }
    let report = AdapterChoiceReport::ranked(
        AdapterPolicy::Auto,
        records.clone(),
        &rank_adapters(&records, &AdapterPolicy::Auto, false).expect("an adapter"),
    );
    assert_eq!(report.chosen, Some(1));
    assert_eq!(
        report.explanation,
        "Intel(R) Iris(R) Xe Graphics encodes h264,hevc on the GPU, more codecs than NVIDIA \
         GeForce MX450, which encodes nothing on the GPU (NVIDIA GeForce MX450: \
         ID3D12VideoDevice unavailable)"
    );
    // Three codecs beat two, wherever the adapter is in the order.
    let three = vec![
        adapter(0, "A", true, &["h264", "hevc"]),
        adapter(1, "B", false, &["h264", "hevc", "av1"]),
    ];
    assert_eq!(
        first(&three, &AdapterPolicy::Auto, true),
        (1, ChoiceReason::MostGpuEncoders)
    );
}

#[test]
fn auto_breaks_ties_by_decode_then_order() {
    let one_decodes = vec![
        adapter(0, "A", false, &["h264"]),
        adapter(1, "B", true, &["h264"]),
    ];
    assert_eq!(
        first(&one_decodes, &AdapterPolicy::Auto, true),
        (1, ChoiceReason::Decodes)
    );
    // Decode in software: decoding tells nothing apart, so the order does.
    assert_eq!(
        first(&one_decodes, &AdapterPolicy::Auto, false),
        (0, ChoiceReason::HighPerformanceOrder)
    );
    let alike = vec![adapter(0, "A", true, &[]), adapter(1, "B", true, &[])];
    assert_eq!(
        first(&alike, &AdapterPolicy::Auto, true),
        (0, ChoiceReason::HighPerformanceOrder)
    );
    let mut lost = hybrid();
    lost[1].renders = Err("device lost".to_owned());
    assert_eq!(
        first(&lost, &AdapterPolicy::Auto, true),
        (0, ChoiceReason::OnlyAdapter)
    );
}

#[test]
fn performance_takes_the_first_hardware_renderer() {
    let records = hybrid();
    for decode in [false, true] {
        let order = rank_adapters(&records, &AdapterPolicy::Performance, decode).expect("one");
        assert_eq!(order.order, [0, 1, 2]);
        assert_eq!(order.reason, ChoiceReason::PerformancePolicy);
    }
}

#[test]
fn a_pin_takes_only_what_it_names() {
    let records = hybrid();
    let pinned = |policy: AdapterPolicy| rank_adapters(&records, &policy, true);
    let order = pinned(AdapterPolicy::Index(1)).expect("index 1");
    assert_eq!((order.order, order.reason), (vec![0], ChoiceReason::Pinned));
    assert_eq!(
        pinned(AdapterPolicy::Name("iris".to_owned()))
            .expect("iris")
            .order,
        [1]
    );
    assert_eq!(
        pinned(AdapterPolicy::Name("basic render".to_owned()))
            .expect("WARP")
            .order,
        [2]
    );
    let refused = pinned(AdapterPolicy::Name("Radeon".to_owned())).expect_err("no Radeon");
    assert!(
        refused.contains("ALPHA2_GPU_ADAPTER=Radeon matches no adapter"),
        "{refused}"
    );
    assert!(
        refused.contains("[1] NVIDIA GeForce MX450, [0] Intel(R) Iris(R) Xe Graphics"),
        "{refused}"
    );
    // A removed policy name is a name like any other.
    let refused = pinned(AdapterPolicy::parse(Some("encoder"))).expect_err("no such adapter");
    assert!(
        refused.contains("ALPHA2_GPU_ADAPTER=encoder matches no adapter"),
        "{refused}"
    );
    let refused = pinned(AdapterPolicy::Index(7)).expect_err("past the last adapter");
    assert!(
        refused.contains("ALPHA2_GPU_ADAPTER=7 matches no adapter"),
        "{refused}"
    );
    let mut lost = hybrid();
    lost[1].renders = Err("device lost".to_owned());
    let refused = rank_adapters(&lost, &AdapterPolicy::Index(0), true).expect_err("lost");
    assert!(
        refused.contains("does not render (device lost)"),
        "{refused}"
    );
    // A second row of a GPU another row is, recorded as not rendering, leaves the name pin the
    // row that renders.
    let rows = [
        AdapterRecord {
            renders: Err("same GPU as [1], not opened".to_owned()),
            ..adapter(0, "Intel(R) Iris(R) Xe Graphics", false, &[])
        },
        adapter(1, "Intel(R) Iris(R) Xe Graphics", false, &["h264"]),
    ];
    assert_eq!(
        rank_adapters(&rows, &AdapterPolicy::Name("intel".to_owned()), false)
            .expect("the second row")
            .order,
        [1]
    );
}

#[test]
fn software_adapters_come_last() {
    let records = vec![warp(2), adapter(0, "GPU", false, &[])];
    assert_eq!(first(&records, &AdapterPolicy::Auto, true).0, 1);
    // A software adapter that claims to decode and encode is still last.
    let mut claims = vec![warp(2), adapter(0, "GPU", false, &[])];
    claims[0].decodes = Ok(());
    claims[0].gpu_encodes = vec![("h264", Ok(()))];
    assert_eq!(
        first(&claims, &AdapterPolicy::Auto, true),
        (1, ChoiceReason::OnlyAdapter)
    );
    assert_eq!(first(&claims, &AdapterPolicy::Performance, true).0, 1);
    let mut only_warp = vec![adapter(0, "GPU", false, &[]), warp(1)];
    only_warp[0].renders = Err("no device".to_owned());
    assert_eq!(
        first(&only_warp, &AdapterPolicy::Auto, true),
        (1, ChoiceReason::OnlyAdapter)
    );
    assert!(
        only_warp[1].summary().contains("(a software adapter)"),
        "{}",
        only_warp[1].summary()
    );
    only_warp[1].renders = Err("no device".to_owned());
    let refused = rank_adapters(&only_warp, &AdapterPolicy::Auto, false).expect_err("none");
    assert!(refused.starts_with("no adapter renders"), "{refused}");
}

#[test]
fn auto_does_not_change_adapter_with_the_decode_policy_on_a_hybrid_laptop() {
    let records = hybrid();
    assert_eq!(
        first(&records, &AdapterPolicy::Auto, false).0,
        first(&records, &AdapterPolicy::Auto, true).0
    );
}

#[test]
fn the_reason_names_another_adapter_that_could_encode() {
    let records = hybrid();
    assert_eq!(
        encode_elsewhere(&records, 0),
        vec![
            (
                "h264",
                "Intel(R) Iris(R) Xe Graphics encodes h264 on the GPU (ALPHA2_GPU_ADAPTER=0 \
                 renders there)"
                    .to_owned()
            ),
            (
                "hevc",
                "Intel(R) Iris(R) Xe Graphics encodes hevc on the GPU (ALPHA2_GPU_ADAPTER=0 \
                 renders there)"
                    .to_owned()
            ),
        ]
    );
    assert_eq!(encode_elsewhere(&records, 1), Vec::new());
    let mut warp_encodes = hybrid();
    warp_encodes[1].gpu_encodes.clear();
    warp_encodes[2].gpu_encodes = vec![("h264", Ok(()))];
    assert_eq!(encode_elsewhere(&warp_encodes, 0), Vec::new());
}

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

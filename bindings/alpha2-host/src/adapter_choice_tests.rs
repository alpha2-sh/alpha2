use super::*;
use alpha2_gpu::AdapterRecord;

/// The second DXGI row a hybrid laptop's driver lists for its integrated GPU
/// (no PCI address, same ids) is that GPU again; two identical GPUs, each
/// with an address, are two.
#[test]
fn a_second_row_of_the_same_gpu_is_not_opened() {
    let row = |index, vendor_device, pci_address, software| RowIdentity {
        index,
        vendor_device,
        pci_address,
        luid: None,
        software,
    };
    let integrated = (0x8086, 0x9a49);
    let laptop = [
        row(2, (0x10de, 0x1f97), Some((88, 0, 0)), false),
        row(0, integrated, None, false),
        row(1, integrated, Some((0, 2, 0)), false),
        row(3, (0x1414, 0x8c), None, true),
    ];
    assert_eq!(duplicate_rows(&laptop), [(0, 1)]);
    let twins = [
        row(0, (0x10de, 0x2684), Some((1, 0, 0)), false),
        row(1, (0x10de, 0x2684), Some((2, 0, 0)), false),
    ];
    assert_eq!(duplicate_rows(&twins), []);
}

/// A hybrid laptop pinned to its discrete GPU: the report names every
/// adapter with what it does, and an export's reason names the adapter that
/// could encode.
#[test]
fn the_report_names_every_adapter_and_the_one_that_could_encode() {
    let mut discrete = AdapterRecord::unasked(1, "NVIDIA GeForce MX450".to_owned(), "dx12", false);
    discrete.renders = Ok(());
    discrete.luid = Some(0x1_349e);
    discrete.decodes = Err("ID3D12VideoDevice unavailable".to_owned());
    discrete.gpu_encodes = vec![("h264", Err("ID3D12VideoDevice unavailable".to_owned()))];
    let mut integrated =
        AdapterRecord::unasked(0, "Intel(R) Iris(R) Xe Graphics".to_owned(), "dx12", false);
    integrated.renders = Ok(());
    integrated.decodes = Ok(());
    integrated.gpu_encodes = vec![
        ("h264", Ok(())),
        ("hevc", Ok(())),
        ("av1", Err("no".to_owned())),
    ];
    let records = vec![discrete, integrated];
    let policy = AdapterPolicy::Index(1);
    let order = alpha2_gpu::rank_adapters(&records, &policy, true).expect("pinned");
    let report = AdapterChoiceReport::ranked(policy, records, &order);
    let json = adapter_choice_json(&report);
    assert_eq!(json["policy"], "index:1");
    assert_eq!(json["chosen"], 1);
    assert_eq!(json["reason"], "pinned");
    assert_eq!(
        json["explanation"],
        "ALPHA2_GPU_ADAPTER=1 names NVIDIA GeForce MX450"
    );
    let candidates = json["candidates"].as_array().expect("candidates");
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0]["luid"], "0x000000000001349e");
    assert_eq!(candidates[0]["decodes"], false);
    assert_eq!(candidates[0]["gpuEncodes"], serde_json::json!([]));
    assert_eq!(
        candidates[0]["gpuEncodeReason"],
        "ID3D12VideoDevice unavailable"
    );
    assert_eq!(
        candidates[1]["gpuEncodes"],
        serde_json::json!(["h264", "hevc"])
    );
    assert_eq!(candidates[1]["gpuEncodeReason"], serde_json::Value::Null);
    assert_eq!(candidates[1]["luid"], serde_json::Value::Null);
    let notes = EncodeAdapterNotes::new(None, Some(&report));
    let reason = notes.explain(
        NativeVideoCodec::Hevc,
        "NVIDIA GeForce MX450 has no encoder".to_owned(),
    );
    assert_eq!(
        reason,
        "NVIDIA GeForce MX450 has no encoder; Intel(R) Iris(R) Xe Graphics encodes hevc on the \
         GPU (ALPHA2_GPU_ADAPTER=0 renders there)"
    );
    assert_eq!(notes.explain(NativeVideoCodec::Av1, "x".to_owned()), "x");
}

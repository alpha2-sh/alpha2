//! A refused device gives way to the next adapter in product-request order.
#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashSet;

use alpha2_gpu::GpuContext;

// One backend only: Windows drivers corrupt the heap when devices are opened on several
// backends in one process.
const BACKENDS: wgpu::Backends = if cfg!(target_os = "windows") {
    wgpu::Backends::DX12
} else if cfg!(target_os = "macos") {
    wgpu::Backends::METAL
} else {
    wgpu::Backends::VULKAN
};

fn adapter_name(context: &GpuContext) -> String {
    context.capabilities().adapter_name.clone()
}

fn offered(
    mut accept: impl FnMut(usize) -> bool,
) -> (alpha2_gpu::AcceptedDevice<usize, String>, Vec<String>) {
    let mut names = Vec::new();
    let found = pollster::block_on(GpuContext::request_product_accepted(|context| {
        names.push(adapter_name(context));
        let index = names.len() - 1;
        if accept(index) {
            Ok(index)
        } else {
            Err(format!("refused {index}"))
        }
    }))
    .expect("a GPU device");
    (found, names)
}

#[test]
fn a_taken_preferred_device_is_the_one_a_product_request_opens() {
    let preferred = pollster::block_on(GpuContext::request_product()).expect("a GPU device");
    let (found, names) = offered(|_| true);
    assert_eq!(names, [adapter_name(&preferred)]);
    assert_eq!(found.accepted, Some(0));
    assert_eq!(adapter_name(&found.context), adapter_name(&preferred));
    assert_eq!(found.refusals.to_string(), "", "nothing was refused");
}

#[test]
fn every_gpu_adapter_is_offered_once_and_with_none_taken_the_preferred_one_renders() {
    let (found, names) = offered(|_| false);
    assert_eq!(found.accepted, None);
    assert_eq!(
        adapter_name(&found.context),
        names[0],
        "the preferred adapter"
    );
    // Refusals keep their adapter names and reasons, in order.
    assert_eq!(
        found.refusals.0,
        names
            .iter()
            .enumerate()
            .map(|(index, name)| (name.clone(), format!("refused {index}")))
            .collect::<Vec<_>>()
    );
    let shown = found.refusals.to_string();
    assert!(
        shown.starts_with(&format!("{}: refused 0", names[0])),
        "{shown}"
    );
    // A CPU adapter is never offered after the preferred one.
    let _turn = alpha2_gpu::serialize_acquisition();
    let instance = alpha2_gpu::new_instance(BACKENDS);
    let cpu: HashSet<String> = pollster::block_on(instance.enumerate_adapters(BACKENDS))
        .into_iter()
        .map(|adapter| adapter.get_info())
        .filter(|info| info.device_type == wgpu::DeviceType::Cpu)
        .map(|info| info.name)
        .collect();
    assert!(
        names.iter().skip(1).all(|name| !cpu.contains(name)),
        "{names:?} offers a CPU adapter among {cpu:?}"
    );
}

#[test]
fn a_refused_preferred_device_gives_way_to_the_next_adapter_taken() {
    let (all, _) = offered(|_| false);
    let (found, names) = offered(|index| index > 0);
    if all.refusals.0.len() < 2 {
        assert_eq!(found.accepted, None);
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::MultiAdapter,
            "this machine has one GPU adapter on {BACKENDS:?}"
        );
        return;
    }
    assert_eq!(found.accepted, Some(1));
    assert_eq!(adapter_name(&found.context), names[1]);
    assert_eq!(
        found.refusals.0,
        [(names[0].clone(), "refused 0".to_owned())],
        "the preferred adapter's refusal is kept"
    );
}

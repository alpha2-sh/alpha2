"""Writes vad.onnx: Silero VAD's 16 kHz graph, without the sample-rate branch tract cannot type.

The published `silero_vad_op18_ifless.onnx` holds both rates behind one `If` on its `sr` input.
This keeps the 16 kHz branch as the graph, drops `sr` and the 8 kHz weights, and changes nothing
else: inputs `input` (1, 576: 64 samples of context, then 512 new ones) and `state` (2, 1, 128);
outputs `output` (the speech probability) and `stateN`.

    python derive.py silero_vad_op18_ifless.onnx vad.onnx    # needs the `onnx` package
"""

import sys

import onnx
from onnx import helper, numpy_helper

SAMPLE_RATE = 16000


def main(source: str, target: str) -> None:
    model = onnx.load(source)
    graph = model.graph
    initializers = {tensor.name: tensor for tensor in graph.initializer}
    branch = next(node for node in graph.node if node.op_type == "If")
    equal = next(node for node in graph.node if node.output[0] == branch.input[0])
    rate = numpy_helper.to_array(initializers[equal.input[1]]).item()
    if rate != SAMPLE_RATE:
        raise SystemExit(f"the If compares the rate with {rate}, not {SAMPLE_RATE}")
    taken = next(attribute.g for attribute in branch.attribute if attribute.name == "then_branch")
    nodes = list(taken.node)
    for inner, outer in zip(taken.output, branch.output):
        nodes.append(helper.make_node("Identity", [inner.name], [outer]))
    read = {name for node in nodes for name in node.input}
    inputs = [value for value in graph.input if value.name in ("input", "state")]
    kept = [tensor for name, tensor in initializers.items() if name in read]
    derived = helper.make_graph(nodes, graph.name, inputs, list(graph.output), kept)
    derived.initializer.extend(taken.initializer)
    result = helper.make_model(derived, opset_imports=model.opset_import, producer_name="alpha2")
    result.ir_version = model.ir_version
    onnx.checker.check_model(result)
    onnx.save(result, target)


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])

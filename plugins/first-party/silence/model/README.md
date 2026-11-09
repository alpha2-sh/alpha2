# The voice-activity model

`../resources/vad.onnx` is Silero VAD v5 (https://github.com/snakers4/silero-vad, MIT License, the
text in `LICENSE`), its 16 kHz graph only. `derive.py` writes it from the published
`src/silero_vad/data/silero_vad_op18_ifless.onnx` by keeping the branch of the graph's one `If`
taken at 16 kHz, which tract cannot type with both branches present; the weights are unchanged.

| File | SHA-256 |
| --- | --- |
| `silero_vad_op18_ifless.onnx` (published) | `7671cd04b004e9076da0d4a7b1a5aec36adf161c39230c1cb94a4fd5db6bbd28` |
| `vad.onnx` (derived) | `a02089a54f5d8c9317c6b3b26745497d75b2d7ffb94521c918dcfc1b99bcf658` |

Inputs: `input` (1, 576), 64 samples of the previous block then 512 new ones at 16 kHz; `state`
(2, 1, 128), zeros at the start. Outputs: `output` (1, 1), the probability of speech; `stateN`, the
next state.

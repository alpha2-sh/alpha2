# GPU watchdog cases

Runtime effects and projects whose GPU work is long enough to matter to the GPU watchdog: the
operating system resets a GPU whose submission runs past its timeout (2 s on Windows), so the
renderer predicts each pass's time from its cost and cuts a frame into submissions that each stay
under an eighth of the watchdog.

| file | what |
|---|---|
| `heavy-effects.json` | `gpu-watchdog.heavy`: 1024 taps, each weighted by eight dependent transcendental calls |
| `light-effects.json` | `gpu-watchdog.light`: 64 taps, 256 transcendental calls |
| `max-effects.json` | `gpu-watchdog.max`: a body at the 32 KiB limit whose cost registration refuses; never draw it on a GPU |
| `heavy-<W>x<H>.json` | one two-frame clip through the heavy effect at 1920x1080, 3840x2160 and 7680x4320 |
| `light-<W>x<H>.json` | the same through the light effect at 1920x1080 and 3840x2160 |
| `builtin-worst-3840x2160.json` | the costliest built-in effects and transitions at their largest parameters |
| `builtin-worst.segments.json` | the tick range of each built-in in that project (5000 ticks per frame) |

Every project reads `fixture://parity-rgb` (`tests/corpus/parity-rgb.mov`); the built-in project
also reads `fixture://parity-image` (`tests/corpus/parity-image.png`). For example:

```sh
ALPHA2_EXPORT_TRACE=heavy.csv alpha2-host export --project tests/cases/gpu-watchdog/heavy-3840x2160.json \
  --asset video=tests/corpus/parity-rgb.mov --effects tests/cases/gpu-watchdog/heavy-effects.json \
  --renderer gpu --no-audio --video-codec mpeg4-part2 --out heavy.mp4
```

## GPU time per frame

The host has no per-frame GPU timestamps on every adapter. The export trace
(`ALPHA2_EXPORT_TRACE`) records, per frame, when its work was submitted and when its readback
mapped; frames run on the GPU in submission order, so frame `n`'s GPU time is about

```
readbackReady[n] - max(submitted[n], readbackReady[n-1])
```

an upper bound that includes the map latency, and meaningful while the export waits on the GPU
(`readbackReady > readbackWaitStarted`). Completions seen in one poll move time between
consecutive frames, never out of their sum, so sums over a run or a segment are the robust figure.
The trace also gives each frame's submissions, its predicted GPU time (`predictedMs`, when the
frame was heavy enough to be planned) and its predicted cost per kind of work (`costFetches`,
`costTranscendentals`, `costArithmetic`: counts per pixel times the pixels each pass covers).
Running with a large `ALPHA2_GPU_WATCHDOG_MS` keeps every frame one submission while measuring.

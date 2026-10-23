# alpha2

The alpha2 media engine for the browser: a WebAssembly build of the engine and the TypeScript that drives it. It renders a timeline project on the GPU for preview and export, from frames and audio the application decodes, and hands back the rendered frames and the mixed audio for the application to encode.

For an editor with documents, undo, and a ready preview and export bridge, see [`@alpha2-sh/editor`](https://github.com/alpha2-sh/alpha2/blob/main/packages/alpha2-editor/README.md), which is built on this package. The [alpha2 README](https://github.com/alpha2-sh/alpha2#readme) explains the project document and the other packages.

## Install

```sh
npm install alpha2
```

- The WebAssembly modules need Chrome 96, Firefox 89 or Safari 16.4, or later.
- Serve `.wasm` files as `application/wasm`; otherwise the loader falls back to a slower way of compiling them and warns.
- Audio preview needs a cross-origin isolated page (`Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Embedder-Policy: require-corp`).
- [mediabunny](https://www.npmjs.com/package/mediabunny) is an optional dependency, for the decoders and the MP4, MOV, MKV and WebM writer in `alpha2/mediabunny`. It installs `@types/dom-webcodecs` 0.1.13, whose declarations clash with TypeScript 5.9's DOM library. A TypeScript application either compiles with `skipLibCheck` or adds `"overrides": { "@types/dom-webcodecs": "0.1.19" }` to its `package.json`; one that imports `alpha2/mediabunny` also needs `"ESNext.Disposable"` in its `lib`.

## Render a frame

```js
import init, { WebEffectRegistry } from "alpha2/wasm";
import { ProjectBuilder } from "alpha2/builder";

await init();
const engine = await WebEffectRegistry.createForHtmlCanvas(document.querySelector("canvas"));

const builder = new ProjectBuilder({ name: "Tutorial project", width: 1280, height: 720, frameRate: 30 });
const image = builder.source("image", "asset://image-1", { id: "image-1" });
builder.videoTrack({ name: "Main video" }).clip({ source: image, at: 0, duration: 1 });
const project = builder.build(); // passes validateProject, or throws
engine.loadProject(JSON.stringify(project));

// The application decodes its media; here, one opaque white image (premultiplied sRGB RGBA8).
engine.uploadRgbaImage("image-1", 1280, 720, new Uint8Array(1280 * 720 * 4).fill(255));
engine.renderAt(0n); // draws the frame at time 0 on the canvas
```

- `init()` loads `alpha2_wasm_bg.wasm` from beside the module. A bundler that moves it passes its URL, a `Response`, its bytes or a compiled `WebAssembly.Module` as `init({ module_or_path })`; the files are `alpha2/wasm/alpha2_wasm_bg.wasm` (the engine), `alpha2/wasm/alpha2_audio_worklet.wasm` (the audio worklet) and `alpha2/wasm/alpha2_extensions_bg.wasm` (SVG, animated images, Lottie, OpenEXR and image sequences, loaded only for such a source).
- `createForHtmlCanvas` draws on a page canvas; a worker passes an `OffscreenCanvas` to `createForCanvas`. Either renders with WebGPU, or with WebGL2 when the browser has no WebGPU.
- A project is the engine's JSON document. `alpha2/project` has its TypeScript types, tick conversions and `validateProject`, which reports what the engine would refuse before the document is sent; `alpha2/builder` writes one with times in seconds. `loadProject` checks the document again and throws what it refuses.
- `renderAt(ticks)` returns JSON that names the sources it had no frame for (`missingSources`, `missingFrames`). The engine decides which frame of each source a time needs: `frameNeeds(ticks)` lists them with the exact source time, and the application decodes and uploads those (`uploadVideoFrame` for a `VideoFrame`, `uploadRgbaFrame` for pixels at a source time, `uploadRgbaImage` for a still). `FrameSupply` in `alpha2/frame-supply` does this for a decoder per clip, and keeps the uploads within a byte budget.

## Export

Export renders offscreen at export quality, on the project's exact frame grid, and never touches the canvas. Each frame comes back as planar I420 (BT.709, limited range) with padded rows:

```js
import { videoFrameFromI420 } from "alpha2/video-frames";

engine.beginExport(0n, 120_000n, 30, 1, "h264"); // [start, end) in ticks, 30/1 fps, chroma placed for H.264
for (;;) {
  const ticket = engine.beginExportFrame();
  if (ticket.kind === "complete") break;
  if (ticket.kind === "waiting") throw new Error(`upload ${ticket.missingSources} first`);
  const frame = await ticket.readback;
  const video = await videoFrameFromI420(frame.pixels, {
    format: "I420",
    codedWidth: frame.width,
    codedHeight: frame.height,
    layout: frame.layout,
    colorSpace: frame.colorSpace,
    timestamp: Math.round(Number(frame.timeTicks) / 0.12), // ticks to microseconds
  });
  video.close(); // or hand it to a VideoEncoder
}
engine.endExport();
```

A `waiting` ticket names the source frames the next frame needs (`missingFrames`); upload them and ask again, and the export continues at the same frame. `beginExportFrame` can be called again before a readback resolves, so that the GPU renders the next frame while the last one is read back. `exportToSink` in `alpha2/exporter` runs an export this way into an encoder sink, and `MediabunnySink` in `alpha2/mediabunny` is a WebCodecs sink for it that writes MP4, MOV, MKV or WebM.

## Audio

The engine mixes the project's audio. The application uploads each source's decoded audio, resampled to the project's sample rate (`uploadAudioBlock`), and `renderAudio` mixes a block of the timeline: clip gain, pan and fades, effects, track mute and solo, ducking, and the master bus. A block that needs audio the engine does not have yet is listed by `audioMissingSources` rather than mixed with a gap.

For preview, `alpha2/audio-timeline` moves the mixed blocks to the audio thread through a ring in shared memory, which needs a cross-origin isolated page (`canShareAudioMemory()` says whether this page is one), and `alpha2/audio-worklet` plays them in an AudioWorklet. `alpha2/loudness` measures loudness (ITU-R BS.1770-4, EBU R128) and plans normalization; `alpha2/waveform` computes waveform summaries.

## Preview in a worker

`alpha2/worker` runs the engine in a worker that owns the GPU device and an `OffscreenCanvas` (`installRenderWorker`, `createWasmRenderWorkerHost`), and `WorkerRenderBackend` drives it from the page. `Alpha2Player` in `alpha2/player` presents times through such a backend: it serializes presentations, asks for the frames a presentation lacks and shows them once they arrive, and makes the device again after a device loss.

## Subpaths

| Import | What it has |
| --- | --- |
| `alpha2/wasm` | The engine: `init`, `WebEffectRegistry` (rendering, export, audio mixing), and the engine's functions for rate control, captions, loudness and waveforms. |
| `alpha2/project` | The project document's types and limits, tick conversions, `validateProject`. |
| `alpha2/builder` | `ProjectBuilder`: documents from seconds, with generated ids. |
| `alpha2/edit` | Editing operations on a project: split, trim, ripple, roll, slip, slide, retime, insert, overwrite, append, move. |
| `alpha2/captions` | SRT and WebVTT import into caption tracks. |
| `alpha2/geometry` | Shape geometry for editors, from the engine's own code: item bounds, hit tests, nearest points, path lengths, trims, resampling, convert to path, motion paths. |
| `alpha2/effects` | Effect definitions and the built-in catalog's types; `RuntimeEffectRegistry`, a typed wrapper for registering WGSL effects and effect resources at run time. |
| `alpha2/extensions`, `alpha2/extensions-wasm` | SVG, animated image, Lottie, OpenEXR and image sequence sources, from their own WebAssembly module (`loadExtensions`). |
| `alpha2/text` | Text rasterizers: `WasmTextRasterizer` (the engine's, the same pixels as a native export) and `OffscreenTextRasterizer` (the browser's). |
| `alpha2/assets` | Media identity, proxies and conform reports. |
| `alpha2/frame-supply` | `FrameSupply`: decodes and uploads the frames a time needs. |
| `alpha2/media` | Decoder contracts, `WebCodecsVideoDecoder`, `DecoderRefillCoordinator`. |
| `alpha2/mediabunny` | Decoders and an MP4, MOV, MKV and WebM sink on mediabunny (an optional dependency). |
| `alpha2/hdr-frames`, `alpha2/video-frames` | Uploading planar and HDR `VideoFrame`s; `VideoFrame`s from exported I420 planes. |
| `alpha2/player`, `alpha2/worker` | `Alpha2Player` and the render worker. |
| `alpha2/exporter` | `Alpha2Exporter` and `exportToSink`. |
| `alpha2/export-destination` | Where an export's bytes go: a file, or memory. |
| `alpha2/export-failure`, `alpha2/export-retry` | How export failures are classified, and a retry policy over them. |
| `alpha2/containers` | The export containers (MP4, MOV, MKV, WebM) and the codecs each offers. |
| `alpha2/rate-control` | Output bitrates for the engine's quality levels. |
| `alpha2/audio`, `alpha2/audio-worker` | Offline audio rendering into a sink, in a worker or not. |
| `alpha2/audio-timeline`, `alpha2/audio-worklet`, `alpha2/audio-worklet-processor` | Real-time audio: the shared ring, its pump, and the AudioWorklet and its processor module. |
| `alpha2/loudness`, `alpha2/waveform` | Loudness measurement and normalization; waveform summaries. |
| `alpha2/robustness` | Browser capability detection and degradation reports. |
| `alpha2` | `alpha2/assets`, `audio`, `audio-timeline`, `audio-worker`, `audio-worklet`, `containers`, `effects`, `exporter`, `extensions`, `frame-supply`, `hdr-frames`, `loudness`, `media`, `player`, `project`, `rate-control`, `robustness`, `text`, `waveform` and `worker` in one import. |

Each module's declarations document its API.

## License

MIT, copyright Alpha2 contributors. The package includes [`THIRD_PARTY_NOTICES.md`](https://github.com/alpha2-sh/alpha2/blob/main/THIRD_PARTY_NOTICES.md) for the components built into it, and `wasm/rust-crate-licenses.md` with the licenses of the Rust crates the wasm modules are compiled from.

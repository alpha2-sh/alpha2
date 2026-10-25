# α2

alpha2 is a portable, general-purpose media engine for WebAssembly and native. It renders a timeline project (sources, video, audio and caption tracks, clips, effects and transitions) on the GPU, for preview and for export.

Every target runs the same rendering code, so a project renders the same in a browser preview, in a server export and in a Rust program.

## Packages

| You are building | Use |
| --- | --- |
| A browser app that previews and exports, decoding and encoding the media itself | [`alpha2`](packages/alpha2/README.md) (npm) |
| A browser editor: documents, commands, undo, and a preview and export bridge | [`@alpha2-sh/editor`](packages/alpha2-editor/README.md) (npm) |
| A React canvas over the `alpha2` browser player | [`@alpha2-sh/react`](packages/alpha2-react/README.md) (npm) |
| MP4, MOV, MKV or WebM export from Node.js or Electron | [`@alpha2-sh/node`](packages/alpha2-node/README.md) (npm) and `alpha2-host` |
| MP4, MOV, MKV or WebM export from any other language | `alpha2-host serve` and its [stdio protocol](docs/host-protocol.md) |
| A Rust program | [`alpha2`](crates/alpha2/README.md) (crate) |

## Quick start

Each example renders the same project, one second of one image at 1280x720 and 30 fps. Times are in ticks, 120,000 to the second; the format is [`schema/project.v1.schema.json`](schema/project.v1.schema.json).

```json
{
  "schema_version": 1,
  "id": "tutorial-project",
  "name": "Tutorial project",
  "settings": {
    "width": 1280,
    "height": 720,
    "frame_rate": { "num": 30, "den": 1 },
    "sample_rate": 48000
  },
  "sources": [{ "id": "image-1", "locator": "asset://image-1", "kind": "image" }],
  "timeline": {
    "tracks": [
      {
        "id": "video-1",
        "name": "Main video",
        "kind": "video",
        "clips": [
          {
            "id": "clip-1",
            "source_id": "image-1",
            "timeline_range": { "start": 0, "end": 120000 },
            "source_range": { "start": 0, "end": 120000 }
          }
        ],
        "transitions": []
      }
    ]
  }
}
```

### Browser

`npm install alpha2`. The application decodes its media and uploads the pixels; the engine draws the frame on a canvas.

```js
import init, { WebEffectRegistry } from "alpha2/wasm";

await init();
const engine = await WebEffectRegistry.createForHtmlCanvas(document.querySelector("canvas"));
engine.loadProject(JSON.stringify(project));
// Source `image-1`: opaque white, premultiplied sRGB RGBA8.
engine.uploadRgbaImage("image-1", 1280, 720, new Uint8Array(1280 * 720 * 4).fill(255));
engine.renderAt(0n); // draws the frame at time 0
```

### Native

Native export runs `alpha2-host`, which renders on the GPU and decodes and encodes with FFmpeg 8: MP4 with H.264 video and AAC audio by default, and MOV, Matroska or WebM (VP9 or AV1 with Opus) when the output's extension or `container` says so. Get it one of four ways:

- **Fetch** it for Node.js with `npx @alpha2-sh/node install`, which unpacks the release archive of the package's version into a cache where `@alpha2-sh/node` finds it.
- **Download** an archive from the [Releases page](https://github.com/alpha2-sh/alpha2/releases). `alpha2-<version>-<target>-bundled` carries FFmpeg; `-thin` loads the FFmpeg 8 installed on the system.
- **Build** the same archive from a clone, on the target platform (Git Bash on Windows). The script fetches the pinned FFmpeg build and writes the archives to `target/alpha2-native-release/archives/`.

  ```sh
  git clone https://github.com/alpha2-sh/alpha2.git
  cd alpha2
  scripts/build-native-release.sh bundled
  ```

- **Install** with `cargo install alpha2-host`, which builds against the FFmpeg 8 on the machine.

Then export from the command line:

```sh
alpha2-host export --project project.json --asset image-1=image.png --out tutorial.mp4
```

or from Node.js (`npm install @alpha2-sh/node`, then `npx @alpha2-sh/node install`):

```js
import { readFile } from "node:fs/promises";
import { exportProject } from "@alpha2-sh/node";

const project = JSON.parse(await readFile("project.json", "utf8"));
await exportProject(
  {
    project,
    assets: { "image-1": "image.png" },
    startTicks: 0,
    endTicks: 120_000, // one second
    frameRate: { num: 30, den: 1 },
    output: "tutorial.mp4",
  },
  // finds the host in the cache; { hostPath } names an unpacked archive's bin/alpha2-host instead
);
```

Each `--asset` gives the file of the project source with that id. [`docs/native.md`](docs/native.md) covers the archives, every `alpha2-host` command and `alpha2`, the archives' second program, which probes, transcodes and thumbnails media files.

### Rust

`cargo add alpha2`. The program lends the engine its decoded frames and takes back rendered frames and YUV planes; [`crates/alpha2`](crates/alpha2/README.md) has the whole example.

## Platform support

| Platform | GPU | Hardware decode and encode | Archive |
| --- | --- | --- | --- |
| Browser | WebGPU; WebGL2 when drawing on a canvas without WebGPU | by the application | none |
| Linux x64, arm64 | Vulkan | Vulkan Video | `linux-x64`, `linux-arm64` |
| macOS (Apple silicon) | Metal | VideoToolbox | `macos-arm64` |
| Windows x64 | Direct3D 12 | D3D12VA | `win-x64` (bundled only) |

Native exports encode on the GPU wherever the device and codec allow it. Hardware decoding is on by default on Linux and opt-in on macOS and Windows (`--video-decode auto` on `alpha2-host export`, `videoDecode: "auto"` in `@alpha2-sh/node`).

- **Browsers**: Chrome 96, Firefox 89 or Safari 16.4, or later. Audio preview needs a cross-origin isolated page (`Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Embedder-Policy: require-corp`). Serve `.wasm` files as `application/wasm`.
- **Node.js**: 20 or later.
- **React**: `@alpha2-sh/react` needs React 18 or later, `@alpha2-sh/editor/react` React 19.3 or a later 19.
- **Rust**: 1.88 or later.

## License

alpha2 is available under the [MIT License](LICENSE.md), copyright Alpha2 contributors. Third-party components are listed in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

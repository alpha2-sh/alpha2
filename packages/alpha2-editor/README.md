# @alpha2-sh/editor

An editing SDK for alpha2: editing sessions over the engine's project document, with typed commands, validation and undo, and a browser bridge that previews, plays and exports the document with the [`alpha2`](https://github.com/alpha2-sh/alpha2/blob/main/packages/alpha2/README.md) engine.

## Install

```sh
npm install @alpha2-sh/editor alpha2
```

`alpha2` is a peer dependency at the same version as this package. React is an optional peer, needed only for `@alpha2-sh/editor/react` (React 19.3 or a later 19). The browser requirements are `alpha2`'s.

## Documents and commands

The root import is the editing core. It loads no DOM, React, WebAssembly or audio, so it also runs in Node.js.

```js
import { createEditor, createEmptyDocument, serializeDocument, parseDocument } from "@alpha2-sh/editor";

const editor = createEditor({ document: createEmptyDocument({ width: 1280, height: 720 }) });
const result = editor.dispatch({
  type: "track.add",
  track: { id: "video", name: "Video", kind: "video", clips: [] },
});
if (!result.ok) throw result.error; // an EditorError with a `code`
editor.undo();
editor.redo();
const json = serializeDocument(editor.document); // portable JSON; media bytes stay apart
const reopened = parseDocument(json);
editor.dispose();
```

- An editor document holds an engine project (`document.project`), the editor's metadata, and a descriptor of each asset. `serializeDocument` and `parseDocument` write and read it as JSON; `parseDocument` also reads a plain engine project.
- `dispatch` applies one `Command` (the typed union covers the project, tracks, clips, effects, transitions, caption cues, composition graphs, assets and markers); a `transaction` applies several and is checked once, by the document it ends with. A command the document's rules refuse returns `{ ok: false, error }` and changes nothing. `canExecute` asks without applying.
- Every change that commits is one undo step. The history keeps up to 120 steps and 64 MiB by default (`createEditor({ historyLimit, historyByteLimit })`).
- `beginGesture()` previews a drag: each `update(command)` shows a candidate in `editor.draft` without touching the document or the history, and `commit()` adds one undo step (`cancel()` adds none).
- `subscribe`, `getSnapshot` and `subscribeSelector` notify views of changes. A refusal is an `EditorError`, whose `code` says which check failed.

## Browser bridge

`@alpha2-sh/editor/browser` connects a session to the engine in the browser: import, preview, playback with audio, and export to an MP4 file through WebCodecs.

```js
import { createEditor, createEmptyDocument } from "@alpha2-sh/editor";
import { createBrowserBridge } from "@alpha2-sh/editor/browser";

const editor = createEditor({ document: createEmptyDocument({ width: 1280, height: 720 }) });
const bridge = createBrowserBridge({
  editor,
  // URLs of alpha2/wasm/alpha2_wasm_bg.wasm, alpha2/wasm/alpha2_audio_worklet.wasm and
  // alpha2/audio-worklet-processor, as the application's bundler serves them.
  runtimeAssets: { wasmUrl, audioWasmUrl, workletUrl },
});
await bridge.bindPreview(canvas);

const { assetIds } = await bridge.importFiles(files); // from an <input type="file">
const trackId = crypto.randomUUID();
const placed = editor.dispatch({
  type: "transaction",
  commands: [
    { type: "track.add", track: { id: trackId, kind: "video", name: "Video", clips: [] } },
    {
      type: "clip.insert",
      trackId,
      clip: {
        id: crypto.randomUUID(),
        source_id: assetIds[0],
        timeline_range: { start: 0, end: 120000 },
        source_range: { start: 0, end: 120000 },
        time_map: { type: "forward" },
      },
    },
  ],
});
if (!placed.ok) throw placed.error;
await bridge.seek(0);

const artifact = await bridge.startExport({ videoCodec: "avc" }).result;
// artifact.kind "file": a temporary file in the origin's private storage; release it once downloaded.
if (artifact.kind === "file") await artifact.release();

await bridge.dispose();
await editor.dispose();
```

- Import checks each file with the engine (media, fonts, `.cube` LUTs, SRT and WebVTT captions through `importCaptions`) and adds it to the document in one undoable step. `importSequence` imports numbered PNG or OpenEXR files as one image sequence.
- `bindPreview` draws on a canvas. With `runtimeAssets.workerFactory`, a worker running the `@alpha2-sh/editor/preview-worker` entry renders the preview off the main thread; without one, or when the worker fails, the main thread does. `hashWorkerFactory` (`@alpha2-sh/editor/hash-worker`) and `extensionsWorkerFactory` (`@alpha2-sh/editor/extensions-worker`, with `extensionsWasmUrl`) move file hashing and SVG, animated image, Lottie and OpenEXR reading into workers too.
- `seek`, `play` and `pause` move the playhead; with audio, playback follows the audio clock. Audio preview needs a cross-origin isolated page; editing and video preview do not.
- `startExport` returns a job with `result`, progress (`subscribe`) and `cancel`. The output goes to a temporary file in the origin's private storage by default (`release()` deletes it), to a file the user picked (`output: { kind: "file", handle }`), or to memory (`output: { kind: "memory", maxBytes }`). Whether the browser encodes a codec is the browser's to say: `bridge.videoEncoders()` asks ahead, and an export with a codec the browser does not encode is refused before it renders.

## React

`@alpha2-sh/editor/react` has `EditorProvider`, `useEditor`, `useEditorSelector(selector, equal?)` and `EditorPreview`, which binds a bridge's preview to a canvas it makes:

```jsx
<EditorProvider editor={editor}>
  <EditorPreview bridge={bridge} onError={reportError} />
</EditorProvider>
```

The application owns the session: unmounting the provider does not dispose it.

## Native export

`@alpha2-sh/editor/native-client` (`createNativeClient({ baseUrl })`) exports a document on an HTTP export service that the application runs, for example on top of [`@alpha2-sh/node`](https://github.com/alpha2-sh/alpha2/blob/main/packages/alpha2-node/README.md). It uploads the document's media (`bridge.nativeAssets`) and follows the job to a downloadable file.

## License

MIT, copyright Alpha2 contributors.

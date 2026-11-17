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
- `dispatch` applies one `Command` (the typed union covers the project, tracks, clips, effects, transitions, caption cues, composition graphs, assets, data sets and markers); a `transaction` applies several and is checked once, by the document it ends with. A command the document's rules refuse returns `{ ok: false, error }` and changes nothing. `canExecute` asks without applying.
- Every change that commits is one undo step. The history keeps up to 120 steps and 64 MiB by default (`createEditor({ historyLimit, historyByteLimit })`).
- `beginGesture()` previews a drag: each `update(command)` shows a candidate in `editor.draft` without touching the document or the history, and `commit()` adds one undo step (`cancel()` adds none).
- `subscribe`, `getSnapshot` and `subscribeSelector` notify views of changes. A refusal is an `EditorError`, whose `code` says which check failed.

## Data sets

A project's data sets (tracking points, levels, words, intervals an extractor returned) are changed by commands, so adding one and binding to it is one undo step.

```js
import { dataValue } from "@alpha2-sh/editor";

editor.dispatch({
  type: "transaction",
  commands: [
    { type: "data.put", dataSet: faceTrack }, // replaces the set with its id in place
    {
      type: "clip.update",
      id: "title",
      patch: { visual: { position: dataValue({ data: faceTrack.id, channel: "face", via: { clip: "interview" }, space: "canvas" }) } },
    },
  ],
});
```

- `data.remove` is refused with `data_referenced` while anything reads the set; `error.details.references` lists the places (`dataReferences` from `alpha2/project` lists them for any project). `asset.remove`, `clip.delete` and `track.remove` are refused the same way while a data set is anchored to the source or a binding reads through the clip.
- Timeline commands keep data consistent as the engine's edit operations do: a ripple that reaches every track (every other track sync-locked) cuts or opens timeline-anchored sets with the timeline, a split points the right part's bindings that read through the clip at the new part, and a pasted clip's bindings through itself read through its new id. Source-anchored sets never change.
- Keyframe helpers throw `data_bound` on a value that follows data (`isDataBound`).
- A replaced set is held whole in the undo history, so a put of a set near `MAX_DATA_VALUES` can evict most of it under the 64 MiB default.

## Edit operations

The engine's edit operations (`remove_ranges`, `split_at`, `bake`) run in the runtime you already have; the session applies the result as one undo step, refusing it (`stale_edit`) when the document changed after the project was read.

```js
// Desktop: @alpha2-sh/node computes, no WebAssembly in the app.
const request = { op: "remove_ranges", ranges: { data: "silence" }, target: { track: "v1" } };
const { project, read } = editor.editProject();
const applied = await client.edit(project, request);
const result = editor.applyEdit(applied, { read, request }); // { document, report }

// Bake a binding and drop its data set in the same step.
editor.applyEdit(baked, { read, request: bakeRequest, then: [{ type: "data.remove", id: "face" }] });
```

- In a page, `bridge.applyEdit(request, { then, label })` does all of it with the bridge's own engine and waits for the engine to load.
- Refusals (`unknown_data`, `nothing_to_bake`, ...) come from the engine call itself. Clips the operation creates keep the editor metadata of the clip they were cut from (`report.created[].from`).
- A step applied with a label (by default the operation's name) shows it as `snapshot.undoLabel` and `snapshot.redoLabel`.

## Extraction

Extractors run in the runtime too; the session puts their data sets in as one undo step. A timeline-anchored result is refused (`stale_extraction`) when the timeline changed while the extractor ran; data put in between does not count.

```js
const { project, read } = editor.extractionProject();
const { dataSets } = await client.extract({ project, assets, request, plugin: { manifest, sha256 } });
editor.applyExtraction({ dataSets, timings: {} }, { read });
```

- In a page, `bridge.extract(run, { plugin, signal, onProgress })` reads the source's original media held by the bridge and resolves with `{ result, read }` for `applyExtraction`. It reads source input; timeline input is refused there.
- `bridge.conform()` lists data sets made from media that has since changed in `stale_data`; their `made_by` says how to run them again.

## Browser bridge

`@alpha2-sh/editor/browser` connects a session to the engine in the browser: import, preview, playback with audio, and export to an MP4, MOV, MKV or WebM file through WebCodecs.

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

const artifact = await bridge.startExport({ container: "webm" }).result;
// artifact.kind "file": a temporary file in the origin's private storage; release it once downloaded.
if (artifact.kind === "file") await artifact.release();

await bridge.dispose();
await editor.dispose();
```

- Import checks each file with the engine (media, fonts, `.cube` LUTs, SRT and WebVTT captions through `importCaptions`) and adds it to the document in one undoable step. `importSequence` imports numbered PNG or OpenEXR files as one image sequence. A source whose provider reads no file, such as a LaTeX formula (`{ format: "latex", latex, font_size }`), is kept in the project itself: `putProviderSource` adds or edits one in one undoable step, and it draws in preview and both exports with no file to relink.
- `bindPreview` draws on a canvas. With `runtimeAssets.workerFactory`, a worker running the `@alpha2-sh/editor/preview-worker` entry renders the preview off the main thread; without one, or when the worker fails, the main thread does. `hashWorkerFactory` (`@alpha2-sh/editor/hash-worker`) and `extensionsWorkerFactory` (`@alpha2-sh/editor/extensions-worker`, with `extensionsWasmUrl` and `extensionModuleUrls` such as `{ pdf }`) move file hashing and SVG, animated image, Lottie, OpenEXR and PDF reading into workers too.
- `seek`, `play` and `pause` move the playhead; with audio, playback follows the audio clock. Audio preview needs a cross-origin isolated page; editing and video preview do not.
- `startExport` returns a job with `result`, progress (`subscribe`) and `cancel`. `container` (`mp4` by default, `mov`, `mkv` or `webm`) names the file and decides the codecs: an absent `videoCodec` or `audioCodec` is the container's default (H.264 and AAC, or VP9 and Opus for WebM, with Opus where a browser cannot encode AAC in MP4 or MKV), and one the container does not offer is refused. The output goes to a temporary file in the origin's private storage by default (`release()` deletes it), to a file the user picked (`output: { kind: "file", handle }`), or to memory (`output: { kind: "memory", maxBytes }`). Whether the browser encodes a codec is the browser's to say: `bridge.videoEncoders()` asks ahead, and an export with a codec the browser does not encode is refused before it renders.

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

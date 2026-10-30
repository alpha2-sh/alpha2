// Packs alpha2 and @alpha2-sh/editor, installs the tarballs into the editor consumer fixture, and
// checks what a downstream app sees: Node imports without a DOM, strict type checks, a Vite build
// with no private paths, the built page in a browser, and the React entry point. Chrome by
// default; `--browser safari|firefox` (or ALPHA2_BROWSER) picks a WebDriver browser, and the run
// fails without it. Writes target/editor-reports/package.json.
import {
  mkdtemp,
  readFile,
  writeFile,
  cp,
  rm,
  readdir,
  mkdir,
} from "node:fs/promises";
import { execFileSync } from "node:child_process";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { resolve, join, extname } from "node:path";
import { browserForRun, runBrowserPage } from "./lib/browser-page.mjs";
import { probeRun } from "./lib/browser-probe.mjs";
import {
  animatedGif,
  largeLottie,
  refusedProviderFiles,
} from "./lib/extension-import-fixtures.mjs";
// Chosen before any work: a run without its browser fails here, not after packing and building.
const pageBrowser = browserForRun({
  argv: process.argv.slice(2),
  required: true,
});
await probeRun("editor-package", pageBrowser);
const dir = await mkdtemp(join(tmpdir(), "alpha2-editor-packed-"));
const run = (file, args, options = {}) =>
  execFileSync(file, args, {
    cwd: dir,
    encoding: "utf8",
    stdio: "pipe",
    ...options,
  });
// npm is a `.cmd` on Windows, which execFile cannot start without a shell, so the npm running this
// script (`npm_execpath`) is run with Node instead.
const npm = (args, options) =>
  /\.[cm]?js$/.test(process.env.npm_execpath ?? "")
    ? run(process.execPath, [process.env.npm_execpath, ...args], options)
    : run("npm", args, { shell: process.platform === "win32", ...options });
let server;
try {
  const tarballs = [];
  for (const name of ["alpha2", "alpha2-editor"]) {
    const packed = JSON.parse(
      npm([
        "pack",
        resolve("packages", name),
        "--json",
        "--pack-destination",
        dir,
      ]),
    )[0];
    tarballs.push(join(dir, packed.filename));
  }
  await cp("tests/fixtures/editor-consumer", dir, { recursive: true });
  // The override the READMEs ask a TypeScript consumer for: mediabunny 1.56.2 pins
  // @types/dom-webcodecs 0.1.13, which clashes with TypeScript 5.9's DOM library.
  await writeFile(
    join(dir, "package.json"),
    JSON.stringify({
      private: true,
      type: "module",
      overrides: { "@types/dom-webcodecs": "0.1.19" },
    }),
  );
  // Not `--offline`: `npm ci` caches tarballs but no registry metadata, so a version range could not
  // resolve on a machine whose cache only `npm ci` filled.
  npm([
    "install",
    "--ignore-scripts",
    "--prefer-offline",
    "--no-audit",
    "--no-fund",
    ...tarballs,
  ]);
  run(process.execPath, [
    "--input-type=module",
    "-e",
    `const native=await import('@alpha2-sh/editor/native-client');if(typeof native.createNativeClient!=='function')throw Error('native client export');const e=await import('@alpha2-sh/editor');if(typeof window!=='undefined')throw Error('DOM');const s=e.createEditor();s.dispose();`,
  ]);
  await writeFile(
    join(dir, "consumer.ts"),
    `import { createEditor, createEmptyDocument, type Command } from '@alpha2-sh/editor';\nimport { createBrowserBridge, createMediaAdapter, createWorkerPreview } from '@alpha2-sh/editor/browser';\nimport { createNativeClient } from '@alpha2-sh/editor/native-client';\nconst native=createNativeClient({baseUrl:'http://localhost/api/native'});void native.dispose();\nconst editor=createEditor({document:createEmptyDocument()});const command:Command={type:'project.update',patch:{name:'typed'}};editor.dispatch(command); const bridge=createBrowserBridge({editor,runtimeAssets:{wasmUrl:'engine.wasm',extensionsWorkerFactory:()=>new Worker('extensions-worker.js')},mediaAdapter:createMediaAdapter({extensionsWasmUrl:'extensions.wasm'})});void bridge.dispose(); const worker=createWorkerPreview(new Worker('worker.js'),{sessionId:'typed',canvas:new OffscreenCanvas(64,48)}); void worker.dispose(); editor.dispose();\n`,
  );
  // Checked as a strict consumer would: no skipLibCheck, with the installed ambient @types loaded.
  const typecheck = (file, options = {}) =>
    writeFile(
      join(dir, "tsconfig.json"),
      JSON.stringify({
        compilerOptions: {
          noEmit: true,
          strict: true,
          target: "ES2022",
          module: "NodeNext",
          moduleResolution: "NodeNext",
          ...options,
        },
        files: [file],
      }),
    ).then(() =>
      run(process.execPath, [
        resolve("node_modules/typescript/bin/tsc"),
        "-p",
        "tsconfig.json",
      ]),
    );
  await typecheck("consumer.ts");
  // The optional mediabunny adapter's declarations name mediabunny's, which reference
  // @types/dom-webcodecs and use `Disposable` (the `esnext.disposable` library).
  await writeFile(
    join(dir, "mediabunny-consumer.ts"),
    `import * as wasm from 'alpha2/wasm';\nimport { audioEncoderSampleRate, MediabunnySink } from 'alpha2/mediabunny';\nconst audioSampleRate = await audioEncoderSampleRate('aac', { sampleRate: 22050, numberOfChannels: 2, bitrate: 128000 });\nconst sink = new MediabunnySink({ container: 'mp4', audioCodec: 'aac', audioBitrate: 128000, audioSampleRate: audioSampleRate ?? undefined, audioResampler: wasm, destination: { kind: 'memory', maxBytes: 1 << 20 } });\nvoid sink.finish();\n`,
  );
  await typecheck("mediabunny-consumer.ts", {
    lib: ["ES2022", "ESNext.Disposable", "DOM", "DOM.Iterable"],
  });
  await mkdir(join(dir, "public"), { recursive: true });
  await cp(
    "tests/corpus/h264-long-gop.mp4",
    join(dir, "public/test-video.mp4"),
  );
  await cp(
    "tests/corpus/audio-only-aac-priming.m4a",
    join(dir, "public/test-audio.m4a"),
  );
  // A 1280x720 125-frame GIF and a 9 MB Lottie file that take seconds to open, and two small files
  // its budgets refuse.
  await writeFile(
    join(dir, "public/clip.gif"),
    animatedGif({ width: 1280, height: 720, frames: 125 }),
  );
  await writeFile(
    join(dir, "public/squares.json"),
    largeLottie({ layers: 20000 }),
  );
  for (const [name, text] of Object.entries(refusedProviderFiles))
    await writeFile(join(dir, "public", name), text);
  const build = run(process.execPath, [
    resolve("node_modules/vite/bin/vite.js"),
    "build",
    "--base=/editor/",
  ]);
  const files = await readdir(join(dir, "dist/assets"));
  let jsBytes = 0;
  for (const file of files.filter((f) => f.endsWith(".js"))) {
    const bytes = await readFile(join(dir, "dist/assets", file));
    jsBytes += bytes.length;
    if (/\/packages\/|node:fs|@alpha2-sh\/node/.test(bytes.toString()))
      throw Error("Private path leaked into bundle: " + file);
  }
  server = createServer(async (req, res) => {
    try {
      res.setHeader("Cross-Origin-Opener-Policy", "same-origin");
      res.setHeader("Cross-Origin-Embedder-Policy", "require-corp");
      const path =
        new URL(req.url, "http://localhost").pathname.replace(
          /^\/editor\/?/,
          "",
        ) || "index.html";
      res.setHeader(
        "Content-Type",
        {
          ".html": "text/html",
          ".js": "text/javascript",
          ".wasm": "application/wasm",
        }[extname(path)] ?? "application/octet-stream",
      );
      res.end(await readFile(join(dir, "dist", path)));
    } catch (e) {
      res.statusCode = 404;
      res.end(String(e));
    }
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const browser = JSON.parse(
    decodeURIComponent(
      await runBrowserPage(
        pageBrowser,
        `http://127.0.0.1:${server.address().port}/editor/?test`,
        { timeoutMs: 120000 },
      ),
    ),
  );
  const fallback = JSON.parse(
    decodeURIComponent(
      await runBrowserPage(
        pageBrowser,
        `http://127.0.0.1:${server.address().port}/editor/?test&fallback`,
        { timeoutMs: 120000 },
      ),
    ),
  );
  const contextLoss = JSON.parse(
    decodeURIComponent(
      await runBrowserPage(
        pageBrowser,
        `http://127.0.0.1:${server.address().port}/editor/?test&context-loss`,
        { timeoutMs: 120000 },
      ),
    ),
  );
  const incompleteGraph = JSON.parse(
    decodeURIComponent(
      await runBrowserPage(
        pageBrowser,
        `http://127.0.0.1:${server.address().port}/editor/?test&incomplete-graph`,
        { timeoutMs: 120000 },
      ),
    ),
  );
  const extensions = JSON.parse(
    decodeURIComponent(
      await runBrowserPage(
        pageBrowser,
        `http://127.0.0.1:${server.address().port}/editor/?test&extensions`,
        { timeoutMs: 120000 },
      ),
    ),
  );
  // An export with audio into temporary storage, under WebKit's estimate and without AudioEncoder.
  const exportPage = async (mode) =>
    JSON.parse(
      decodeURIComponent(
        await runBrowserPage(
          pageBrowser,
          `http://127.0.0.1:${server.address().port}/editor/?test&export=${mode}`,
          { timeoutMs: 120000 },
        ),
      ),
    );
  const webkitStorage = await exportPage("webkit-storage");
  const noAudioEncoder = await exportPage("no-audio-encoder");
  // An exact NTSC rate over a range that ends between frames.
  const exactRate = await exportPage("exact-rate");
  // The consumer page's own Export button: the file it downloads is released.
  const exportButton = await exportPage("button");
  // React is installed only after the independent vanilla consumer has passed.
  npm([
    "install",
    "--ignore-scripts",
    "--prefer-offline",
    "--no-audit",
    "--no-fund",
    "react@19.3.0",
    "react-dom@19.3.0",
    "@types/react@19.3.0",
    "@types/react-dom@19.3.0",
  ]);
  await writeFile(
    join(dir, "react-consumer.tsx"),
    `import {createEditor} from '@alpha2-sh/editor';
import {EditorProvider, EditorPreview, useEditorSelector} from '@alpha2-sh/editor/react';
import {createBrowserBridge} from '@alpha2-sh/editor/browser';
const editor=createEditor(); const bridge=createBrowserBridge({editor});
function Selected(){return <span>{useEditorSelector(s=>s.document.project.name)}</span>}
export const app=<EditorProvider editor={editor}><Selected/><EditorPreview bridge={bridge}/></EditorProvider>;
`,
  );
  await typecheck("react-consumer.tsx", { jsx: "react-jsx" });
  run(process.execPath, [
    "--input-type=module",
    "-e",
    `import {createElement} from 'react';import {renderToString} from 'react-dom/server';import {createEditor} from '@alpha2-sh/editor';import {EditorProvider,useEditorSelector} from '@alpha2-sh/editor/react';const editor=createEditor();function Probe(){return createElement('span',null,useEditorSelector(s=>s.document.project.name))}const result=renderToString(createElement(EditorProvider,{editor},createElement(Probe)));if(!result.includes('Untitled')||editor.disposed)throw Error('React public contract');await editor.dispose();`,
  ]);
  const result = {
    ok:
      browser.ok &&
      fallback.ok &&
      contextLoss.ok &&
      incompleteGraph.ok &&
      extensions.ok &&
      webkitStorage.ok &&
      noAudioEncoder.ok &&
      exactRate.ok &&
      exportButton.ok,
    runner: pageBrowser.name,
    base: "/editor/",
    installedFromTarballs: true,
    rootWithoutDOM: true,
    reactPublicEntrypoint: true,
    typecheck: true,
    javascriptBytes: jsBytes,
    browser,
    fallback,
    contextLoss,
    incompleteGraph,
    extensions,
    webkitStorage,
    noAudioEncoder,
    exactRate,
    exportButton,
  };
  await mkdir("target/editor-reports", { recursive: true });
  await writeFile(
    "target/editor-reports/package.json",
    JSON.stringify(result, null, 2) + "\n",
  );
  console.log(result);
  if (!result.ok) {
    console.log(build);
    process.exitCode = 1;
  }
} finally {
  server?.close();
  await rm(dir, { recursive: true, force: true });
}

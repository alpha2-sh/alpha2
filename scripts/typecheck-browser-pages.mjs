import { resolve } from "node:path";
import ts from "typescript";

import { probePage } from "./lib/browser-probe.mjs";
import { parityPages } from "./lib/host-cli-parity/browser-server.mjs";
import {
  cases,
  perfCases,
  videoImportCases,
} from "./lib/wasm-browser/cases.mjs";
import { page } from "./lib/wasm-browser/page.mjs";

const root = resolve(import.meta.dirname, "..").replaceAll("\\", "/");
/** @type {ts.FormatDiagnosticsHost} */
const formatHost = {
  getCanonicalFileName: (file) => file,
  getCurrentDirectory: () => root,
  getNewLine: () => "\n",
};

/** @type {Map<string, string>} */
const pages = new Map([
  [
    "browser-probe",
    probePage({ engineModule: "/packages/alpha2/wasm/alpha2_wasm.js" }),
  ],
]);
for (const [name, body] of Object.entries({ ...cases, ...perfCases }))
  pages.set(`wasm-browser/${name}`, page(body(), videoImportCases.has(name)));
for (const [path, render] of Object.entries(parityPages))
  pages.set(`host-cli-parity${path}`, render());

/** @type {Map<string, string>} */
const scripts = new Map();
for (const [name, html] of pages) {
  const found = [
    ...html.matchAll(/<script type="module">([\s\S]*?)<\/script>/g),
  ].map((match) => match[1]);
  if (found.length === 0) throw new Error(`the ${name} page has no module`);
  found.forEach((script, index) => {
    scripts.set(
      `${root}/target/browser-pages/${name}.${index}.mjs`,
      `${script}\nexport {};\n`,
    );
  });
}

const { options, errors } = ts.convertCompilerOptionsFromJson(
  {
    noEmit: true,
    target: "ES2022",
    module: "ESNext",
    moduleResolution: "Bundler",
    lib: ["ESNext", "DOM", "DOM.Iterable", "DOM.AsyncIterable"],
    types: ["dom-webcodecs", "@webgpu/types"],
    allowJs: true,
    checkJs: true,
    skipLibCheck: true,
    paths: {
      "/packages/*": [`${root}/packages/*`],
      "/scripts/*": [`${root}/scripts/*`],
    },
  },
  root,
);
if (errors.length > 0)
  throw new Error(ts.formatDiagnostics(errors, formatHost));

const host = ts.createCompilerHost(options);
const { fileExists, getSourceFile, readFile } = host;
host.fileExists = (file) => scripts.has(file) || fileExists(file);
host.readFile = (file) => scripts.get(file) ?? readFile(file);
host.getSourceFile = (file, language, ...rest) => {
  const text = scripts.get(file);
  return text === undefined
    ? getSourceFile(file, language, ...rest)
    : ts.createSourceFile(file, text, language);
};
const program = ts.createProgram([...scripts.keys()], options, host);
const diagnostics = ts.getPreEmitDiagnostics(program);
if (diagnostics.length > 0) {
  console.error(
    ts.formatDiagnosticsWithColorAndContext(diagnostics, formatHost),
  );
  process.exitCode = 1;
}

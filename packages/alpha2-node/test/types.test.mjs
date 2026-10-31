// Nothing checks the host's JSON against the declarations at run time; this keeps them in step.
import assert from "node:assert/strict";
import { join, resolve } from "node:path";
import test from "node:test";

import ts from "typescript";

const packageDirectory = resolve(import.meta.dirname, "..");

function typeErrors(source) {
  const file = join(packageDirectory, "test/declared-shapes-check.ts");
  const options = {
    strict: true,
    noEmit: true,
    target: ts.ScriptTarget.ES2022,
    module: ts.ModuleKind.NodeNext,
    moduleResolution: ts.ModuleResolutionKind.NodeNext,
    skipLibCheck: true,
    types: ["node"],
  };
  const host = ts.createCompilerHost(options);
  // The compiler uses forward slashes even on Windows.
  const isChecked = (name) => resolve(name) === file;
  const getSourceFile = host.getSourceFile.bind(host);
  host.getSourceFile = (name, language, ...rest) =>
    isChecked(name)
      ? ts.createSourceFile(name, source, language)
      : getSourceFile(name, language, ...rest);
  const fileExists = host.fileExists.bind(host);
  host.fileExists = (name) => isChecked(name) || fileExists(name);
  const program = ts.createProgram([file], options, host);
  const checked = program.getSourceFile(file);
  assert.ok(checked, `${file} is not in the program`);
  return [
    ...program.getSyntacticDiagnostics(checked),
    ...program.getSemanticDiagnostics(checked),
  ].map((diagnostic) =>
    ts.flattenDiagnosticMessageText(diagnostic.messageText, "\n"),
  );
}

test("the declared export result and capabilities have the host's non-scalar fields", () => {
  assert.deepEqual(
    typeErrors(`
import type { HostCapabilities, NativeExportResult } from "../dist/index.js";

declare const result: NativeExportResult;
declare const capabilities: HostCapabilities;
const samples: number = result.performance.encodeChannelDepthSamples?.length ?? 0;
const fallbacks: number = result.performance.hardwareFallbackReasons?.["unsupported-profile"] ?? 0;
const reserved: number | undefined = result.performance.gpuEncodeBudget?.reservedBytes;
const why: string | null = capabilities.capabilities.decode.hardwareUnavailable;
const renderer: "gpu" | "cpu" = result.renderer;
const fellBack: string | null = capabilities.rendererReason ?? result.rendererReason;
const threads: number | undefined = capabilities.rendererThreads ?? result.rendererThreads;
const cpuMs: number | undefined = result.performance.cpuRenderMs;
const cpuBytes: number | undefined = result.performance.cpuImageHighWaterBytes;
// A counter the declarations do not name is any of the values the host writes, null included.
const other: number | boolean | string | null | object | undefined = result.performance.renderMs;
export { samples, fallbacks, reserved, why, renderer, fellBack, threads, cpuMs, cpuBytes, other };
`),
    [],
  );
});

test("a declared shape mismatch is reported", () => {
  const errors = typeErrors(`
import type { NativeExportResult } from "../dist/index.js";

declare const result: NativeExportResult;
const renderer: number = result.renderer;
export { renderer };
`);
  assert.equal(errors.length, 1, errors.join("\n"));
  assert.match(errors[0], /not assignable to type 'number'/);
});

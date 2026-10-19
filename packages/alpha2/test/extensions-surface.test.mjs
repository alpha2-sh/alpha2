// `extensions.ts` describes the provider glue, which is generated after `tsc`, so the compiler
// never compares the two: these check the generated declarations satisfy that description and
// run the wrapper's common paths against the packaged module.
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import test from "node:test";

import ts from "typescript";

const root = resolve(import.meta.dirname, "../../..");
const packageDirectory = join(root, "packages/alpha2");
const wasmDirectory = join(packageDirectory, "wasm");
const corpus = join(root, "tests/corpus/extensions");
const packaged = existsSync(join(wasmDirectory, "alpha2_extensions_bg.wasm"));
// Skipped before `npm run build` packages the wasm; ALPHA2_REQUIRE_WASM=1 (set by CI after the
// build) makes a missing package fail them.
const skip =
  !packaged &&
  process.env.ALPHA2_REQUIRE_WASM !== "1" &&
  "the extension wasm package is not built";

/**
 * Type-checks `source` as a module of `packages/alpha2/test`, with the package's compiler options,
 * and returns its diagnostics as text.
 */
function typeErrors(source) {
  const configPath = join(packageDirectory, "tsconfig.json");
  const parsed = ts.getParsedCommandLineOfConfigFile(
    configPath,
    {},
    { ...ts.sys, onUnRecoverableConfigFileDiagnostic: () => {} },
  );
  const file = join(packageDirectory, "test/glue-surface-check.ts");
  const options = {
    ...parsed.options,
    noEmit: true,
    rootDir: packageDirectory,
  };
  const host = ts.createCompilerHost(options);
  // The compiler names files with forward slashes, on Windows too, so a name is resolved before it
  // is compared with `file`.
  const isChecked = (name) => resolve(name) === file;
  const getSourceFile = host.getSourceFile.bind(host);
  host.getSourceFile = (name, language, ...rest) =>
    isChecked(name)
      ? ts.createSourceFile(name, source, language)
      : getSourceFile(name, language, ...rest);
  const fileExists = host.fileExists.bind(host);
  host.fileExists = (name) => isChecked(name) || fileExists(name);
  const program = ts.createProgram(
    [file, join(packageDirectory, "src/extensions-wasm.d.ts")],
    options,
    host,
  );
  const checked = program.getSourceFile(file);
  // Without it the diagnostics below would be every file's but this one's, and none would say so.
  assert.ok(checked, `${file} is not in the program`);
  return [
    ...program.getSyntacticDiagnostics(checked),
    ...program.getSemanticDiagnostics(checked),
  ].map((diagnostic) =>
    ts.flattenDiagnosticMessageText(diagnostic.messageText, "\n"),
  );
}

const GLUE_CHECK = `
import type { Extensions } from "../src/extensions.js";
import type * as Glue from "../wasm/alpha2_extensions.js";

declare const glue: typeof Glue;
// The generated module must be what the wrapper says it calls: every function, constructor, method
// and getter it uses, with the arguments it passes and the results it reads.
export const described: ConstructorParameters<typeof Extensions>[0] = glue;
`;

test(
  "the generated provider glue is what the wrapper describes",
  { skip },
  () => {
    assert.deepEqual(typeErrors(GLUE_CHECK), []);
  },
);

test(
  "a glue whose surface drifted from the description fails the check",
  { skip },
  () => {
    // The same check against modules changed as a change on the Rust side would leave them: a frame
    // that lost a getter, and a render that takes another argument.
    const drifts = [
      {
        member: "halfPixels",
        glue: `type Frame = Omit<Glue.RenderedFrame, "halfPixels">;
type Registry = Omit<Glue.ExtensionRegistry, "render"> & {
  render(handle: number, timeTicks: bigint, fitWidth: number, fitHeight: number): Frame;
};`,
      },
      {
        member: "render",
        glue: `type Registry = Omit<Glue.ExtensionRegistry, "render"> & {
  render(handle: number, timeTicks: bigint, fitWidth: number, fitHeight: number, scale: number): Glue.RenderedFrame;
};`,
      },
    ];
    for (const { member, glue } of drifts) {
      const errors = typeErrors(
        GLUE_CHECK.replace(
          "declare const glue: typeof Glue;",
          `${glue}
declare const glue: Omit<typeof Glue, "ExtensionRegistry"> & { ExtensionRegistry: new () => Registry };`,
        ),
      );
      assert.equal(errors.length, 1, errors.join("\n"));
      assert.match(errors[0], new RegExp(member), errors[0]);
    }
  },
);

test(
  "a source renders at the precision it holds, and releases once",
  { skip },
  async () => {
    const { loadExtensions } = await import("alpha2/extensions");
    const glue = await import("alpha2/extensions-wasm");
    glue.initSync({
      module: readFileSync(join(wasmDirectory, "alpha2_extensions_bg.wasm")),
    });
    const extensions = await loadExtensions();
    const read = (name) => new Uint8Array(readFileSync(join(corpus, name)));

    const exr = extensions.open({ format: "exr" }, read("half.exr"));
    const linear = exr.render(0n, { width: 64, height: 64 });
    assert.equal(linear.precision, "linear_f16");
    assert.ok(linear.pixels instanceof Uint16Array);
    assert.equal(linear.pixels.length, linear.width * linear.height * 4);
    assert.deepEqual(linear.diagnostics, []);

    const svg = extensions.open({ format: "svg" }, read("shapes.svg"));
    const fit = { width: 40, height: 30 };
    const srgb = svg.render(0n, fit);
    assert.equal(srgb.precision, "rgba8");
    assert.ok(srgb.pixels instanceof Uint8Array);
    assert.equal(srgb.pixels.length, srgb.width * srgb.height * 4);
    assert.ok(srgb.width <= fit.width && srgb.height <= fit.height);
    // Each frame owns its pixels: a later render does not write into an earlier answer.
    const copy = [...srgb.pixels];
    svg.render(0n, { width: 20, height: 15 });
    assert.deepEqual([...srgb.pixels], copy);

    for (const source of [exr, svg]) {
      source.release();
      source.release();
      assert.throws(() => source.render(0n, fit), /the source is released/);
      assert.deepEqual(source.needs(0n), []);
    }
    const again = extensions.open({ format: "svg" }, read("shapes.svg"));
    assert.equal(again.render(0n, fit).precision, "rgba8");
    again.release();
  },
);

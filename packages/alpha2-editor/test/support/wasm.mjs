// The packaged engine wasm exists only after `npm run build`, and the browser modules import it
// statically: a file that needs them imports them only when `wasmSkip` is false.
import nodeTest from "node:test";
import { existsSync } from "node:fs";
import { join, resolve } from "node:path";
import {
  missingCapability,
  suiteOf,
} from "../../../../scripts/lib/capabilities.mjs";

export const wasmDirectory = join(
  resolve(import.meta.dirname, "..", "..", ".."),
  "alpha2",
  "wasm",
);

/**
 * The node:test `skip` option for the cases of `url` that need the wasm package: its reason when
 * the package is not built, else false. ALPHA2_REQUIRE=wasm fails instead.
 *
 * @param {string} url the test file's `import.meta.url`
 * @returns {string | false}
 */
export function wasmSkip(url) {
  const built = ["alpha2_wasm.js", "alpha2_wasm_bg.wasm"].every((file) =>
    existsSync(join(wasmDirectory, file)),
  );
  return (
    !built &&
    missingCapability("wasm", suiteOf(url), "the wasm package is not built")
  );
}

/**
 * node:test's `test`, skipping every case with `skip` when that is a reason.
 *
 * @param {string | false} skip
 * @returns {typeof nodeTest}
 */
export function testUnless(skip) {
  if (!skip) return nodeTest;
  return (name, options, fn) =>
    fn === undefined
      ? nodeTest(name, { skip }, options)
      : nodeTest(name, { ...options, skip }, fn);
}

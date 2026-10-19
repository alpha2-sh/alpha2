// Lets the import resolve without the generated glue (as in `build:js` alone); the generated types
// are checked against `ExtensionModule` by `test/extensions-surface.test.mjs`.
declare module "alpha2/extensions-wasm";

# Vanilla browser consumer fixture

`npm run test:editor:package` copies this fixture into an external project to verify the packed SDK through Vite and a browser.

Copy this directory outside the repository, install `alpha2`, `@alpha2-sh/editor` and Vite, then run `vite` or `vite build --base=/editor/`. This example uses only package exports. The build entry resolves Wasm and bundles the audio worklet; the library receives URLs without depending on Vite syntax.

Serve with `Cross-Origin-Opener-Policy: same-origin` and `Cross-Origin-Embedder-Policy: require-corp` for audio preview. Without isolation, editing and video preview remain available; check `bridge.capabilities`. The library produces an export artifact; downloading it is application code.

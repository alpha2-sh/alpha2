// The page every case runs in: what a case's body reports, and how an import the machine
// cannot do is told from the engine's failure.

export function page(body, videoImport = false) {
  return `<!doctype html>
<html><head><meta charset="utf-8"><title>pending</title>
<script type="importmap">{"imports":{"alpha2/extensions-wasm":"/packages/alpha2/wasm/alpha2_extensions.js","mediabunny":"/node_modules/mediabunny/dist/bundles/mediabunny.mjs"}}</script>
</head><body>
<script type="module">
const VIDEO_IMPORT = ${videoImport};
const report = (value) => {
  document.title = encodeURIComponent(JSON.stringify(value));
  const result = document.createElement("pre");
  result.textContent = JSON.stringify(value, null, 2);
  document.body.append(result);
};
// Headless Chrome, and SwiftShader's WebGPU in any Chrome, lose the GPU
// instance when a VideoFrame is imported. That is the machine, not the case:
// run these under a virtual display with the real GPU, e.g.
//   ALPHA2_CHROME_HEADED=1 ALPHA2_CHROME_GPU_FLAGS="--use-angle=vulkan
//   --enable-features=Vulkan --ignore-gpu-blocklist" xvfb-run -a npm run test:wasm-browser
// Only there is a lost device a skip: anywhere else it is the engine's, and fails the case, as does
// a loss the case saw before it imported anything.
// Chrome's stack starts with the message; Firefox's and Safari's are the frames alone.
const described = (error) => {
  const message = String(error?.message ?? error);
  const stack = String(error?.stack ?? "");
  return stack.includes(message) ? stack : stack ? message + "\\n" + stack : message;
};
const importUnavailable = (error) =>
  /external Instance reference no longer exists|device was lost|Device was destroyed/i.test(String(error?.message ?? error));
const cannotImportHere = async () => {
  if (/HeadlessChrome/.test(navigator.userAgent)) return true;
  const info = (await navigator.gpu?.requestAdapter())?.info ?? {};
  return /swiftshader/i.test([info.vendor, info.architecture, info.description].join(" "));
};
try {
${body}
} catch (error) {
  if (VIDEO_IMPORT && !error?.beforeImport && importUnavailable(error) && (await cannotImportHere()))
    report({ skipped: "this browser cannot import a VideoFrame into WebGPU (" + String(error?.message ?? error) + ")" });
  else report({ error: described(error) });
}
</script></body></html>`;
}

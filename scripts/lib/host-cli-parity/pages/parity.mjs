import { PEAK_CODES } from "../../parity-tolerance.mjs";

// Renders the parity project's one frame with the packaged engine and compares it, in I420 code
// space, with the golden converted as the GPU pass converts it; the native export's frame, as the
// media CLI decodes it, is compared with the golden itself.
export function parityPage() {
  return `<!doctype html>
<html><head><title>pending</title></head><body><script type="module">
import init, { WebEffectRegistry } from "/packages/alpha2/wasm/alpha2_wasm.js";
const PEAK_CODES = ${PEAK_CODES};
const done = (value) => { document.title = encodeURIComponent(JSON.stringify(value)); };
try {
  document.title = "stage-init";
  await init();
  document.title = "stage-project";
  const project = await fetch("/tests/cases/host-parity-project.json").then((response) => response.json());
  const golden = await pixels("/tests/corpus/cross_topology_parity.png");
  document.title = "stage-engine";
  const surface = document.createElement("canvas");
  surface.width = 64;
  surface.height = 48;
  document.body.append(surface);
  const engine = await WebEffectRegistry.createForHtmlCanvas(surface);
  document.title = "stage-render";
  engine.loadProject(JSON.stringify(project));
  engine.uploadRgbaImage("image-source", 64, 48, golden);
  const ticket = engine.beginExportAt(0n, "h264");
  const rendered = ticket.kind === "rendered" ? await ticket.readback : ticket;
  if (rendered.kind !== "rendered") throw new Error("wasm did not render the parity frame");
  if (rendered.format !== "I420") throw new Error(\`wasm export format is \${rendered.format}, not I420\`);
  // Wasm export reads back planar I420, so the RGBA golden is converted with
  // the same coefficients the GPU pass uses and compared in YUV space. Going
  // the other way would have to invent a chroma upsampling filter and would
  // measure that filter instead of the renderer.
  const expected = goldenI420(golden, rendered.width, rendered.height);
  // The export's planes are padded; a VideoFrame given its layout reads them,
  // as the browser encoder does, and copies them out tightly packed.
  const frame = new VideoFrame(rendered.pixels, {
    format: "I420", codedWidth: rendered.width, codedHeight: rendered.height, timestamp: 0,
    layout: rendered.layout,
  });
  const wasm = new Uint8Array(frame.allocationSize());
  await frame.copyTo(wasm);
  frame.close();
  const native = await pixels("/native.png");
  done({
    adapter: JSON.parse(engine.diagnostics()).adapter,
    wasm: difference(wasm, expected),
    wasmPlanes: planeDifferences(wasm, expected, rendered.width, rendered.height),
    native: difference(native, golden),
  });
} catch (error) {
  done({ error: error?.stack ?? String(error) });
}
async function pixels(url) {
  const bitmap = await createImageBitmap(await fetch(url).then((response) => response.blob()));
  const canvas = new OffscreenCanvas(bitmap.width, bitmap.height);
  const context = canvas.getContext("2d");
  context.drawImage(bitmap, 0, 0);
  bitmap.close();
  return context.getImageData(0, 0, canvas.width, canvas.height).data;
}
function difference(actual, expected) {
  if (actual.length !== expected.length) {
    throw new Error(\`parity image sizes differ: \${actual.length} vs \${expected.length} bytes\`);
  }
  let max = 0;
  let sum = 0;
  let outliers = 0;
  for (let index = 0; index < actual.length; index += 1) {
    const delta = Math.abs(actual[index] - expected[index]);
    max = Math.max(max, delta);
    sum += delta;
    if (delta > PEAK_CODES) outliers += 1;
  }
  return { max, mean: sum / actual.length, outliers, samples: actual.length };
}
function planeDifferences(actual, expected, width, height) {
  const luma = width * height;
  const chroma = (width / 2) * (height / 2);
  return {
    y: difference(actual.subarray(0, luma), expected.subarray(0, luma)),
    u: difference(
      actual.subarray(luma, luma + chroma),
      expected.subarray(luma, luma + chroma),
    ),
    v: difference(actual.subarray(luma + chroma), expected.subarray(luma + chroma)),
  };
}
// Mirrors crates/alpha2-shaders/shaders/build/rgb-to-yuv420.wesl under the
// default BT.709 limited-range metadata: box-filtered 2x2 chroma, then unorm8
// quantization by the R8Unorm render targets.
function goldenI420(rgba, width, height) {
  if (width % 2 !== 0 || height % 2 !== 0) {
    throw new Error(\`parity golden must have even dimensions, got \${width}x\${height}\`);
  }
  const kr = 0.2126;
  const kg = 0.7152;
  const kb = 0.0722;
  const yOffset = 16 / 255;
  const yScale = 219 / 255;
  const chromaOffset = 128 / 255;
  const chromaScale = 224 / 255;
  const chromaWidth = width / 2;
  const chromaHeight = height / 2;
  const planes = new Uint8Array(width * height + chromaWidth * chromaHeight * 2);
  const code = (value) => Math.round(Math.min(Math.max(value, 0), 1) * 255);
  const straight = (x, y) => {
    const offset = (y * width + x) * 4;
    const alpha = rgba[offset + 3] / 255;
    if (alpha <= 0.000001) return [0, 0, 0];
    // getImageData already hands back straight alpha, so only the shader's
    // zero-alpha guard and its clamp have to be reproduced here.
    return [0, 1, 2].map((channel) => Math.min(Math.max(rgba[offset + channel] / 255, 0), 1));
  };
  const luma = ([r, g, b]) => r * kr + g * kg + b * kb;
  for (let y = 0; y < height; y += 1) {
    for (let x = 0; x < width; x += 1) {
      planes[y * width + x] = code(yOffset + yScale * luma(straight(x, y)));
    }
  }
  const uOffset = width * height;
  const vOffset = uOffset + chromaWidth * chromaHeight;
  for (let y = 0; y < chromaHeight; y += 1) {
    for (let x = 0; x < chromaWidth; x += 1) {
      // An H.264 export's chroma sits on the even luma columns, between the rows: columns 2x-1, 2x,
      // 2x+1 weigh 1:2:1, the edge repeated, and the two rows evenly.
      const samples = [];
      for (const row of [y * 2, y * 2 + 1]) {
        for (const [column, weight] of [[x * 2 - 1, 0.125], [x * 2, 0.25], [x * 2 + 1, 0.125]]) {
          samples.push([straight(Math.max(column, 0), row), weight]);
        }
      }
      const averaged = [0, 1, 2].map(
        (channel) => samples.reduce((total, [sample, weight]) => total + sample[channel] * weight, 0),
      );
      const value = luma(averaged);
      const index = y * chromaWidth + x;
      planes[uOffset + index] = code(
        chromaOffset + (chromaScale * (averaged[2] - value)) / (2 * (1 - kb)),
      );
      planes[vOffset + index] = code(
        chromaOffset + (chromaScale * (averaged[0] - value)) / (2 * (1 - kr)),
      );
    }
  }
  return planes;
}
</script></body></html>`;
}

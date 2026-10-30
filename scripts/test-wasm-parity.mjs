// Check packaged wasm against native fixtures; run `npm run build` first. Text layout and pixel
// hashes use tests/golden/text_raster.json. Waveform and loudness results use independent reference
// calculations; test:audio-determinism also compares their native and wasm hashes.

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

const root = resolve(import.meta.dirname, "..");
const packageRoot = join(root, "packages", "alpha2");
const wasm = await import(
  pathToFileURL(join(packageRoot, "wasm", "alpha2_wasm.js")).href
);
await wasm.default({
  module_or_path: await readFile(
    join(packageRoot, "wasm", "alpha2_wasm_bg.wasm"),
  ),
});
const { WasmTextRasterizer } = await import(
  pathToFileURL(join(packageRoot, "dist", "text.js")).href
);

const golden = JSON.parse(
  await readFile(join(root, "tests", "golden", "text_raster.json"), "utf8"),
);
const rasterizer = new WasmTextRasterizer(wasm);
for (const font of golden.fonts) {
  rasterizer.addFont(font.family, await readFile(join(root, font.path)));
}

/** FNV-1a over the pixels, as the native test computes it. */
function fnv1a(bytes) {
  let hash = 0xcbf29ce484222325n;
  for (const byte of bytes) {
    hash = ((hash ^ BigInt(byte)) * 0x100000001b3n) & 0xffffffffffffffffn;
  }
  return hash.toString(16).padStart(16, "0");
}

for (const fixture of golden.cases) {
  const raster = rasterizer.rasterize({
    ...fixture.request,
    contentVersion: BigInt(fixture.contentVersion),
  });
  const actual = {
    width: raster.texture.width,
    height: raster.texture.height,
    layoutX: raster.layoutX,
    layoutY: raster.layoutY,
    layoutWidth: raster.layoutWidth,
    layoutHeight: raster.layoutHeight,
    firstBaseline: raster.firstBaseline,
    ascent: raster.ascent,
    descent: raster.descent,
    lineCount: raster.lineCount,
    pixelsFnv1a: fnv1a(raster.rgba8),
    ...(raster.background ? { backgroundFnv1a: fnv1a(raster.background) } : {}),
  };
  assert.deepEqual(actual, fixture.expected, fixture.name);
  assert.equal(raster.texture.premultiplication, "premultiplied");
}
assert.equal(rasterizer.cacheSize, golden.cases.length);
rasterizer.close();
console.log(
  `verified ${golden.cases.length} text rasters are identical in wasm and natively`,
);

// A collection registered whole: the weight picks its Bold face, as registering that face alone
// draws it.
const pair = await readFile(
  join(root, "tests", "fonts", "Alpha2FixturePair.ttc"),
);
assert.equal(wasm.fontFaceCount(pair), 2);
const whole = new WasmTextRasterizer(wasm);
whole.addFontFile("Alpha2FixturePair", pair);
const boldOnly = new WasmTextRasterizer(wasm);
boldOnly.addFont("Alpha2FixturePair", pair, 1);
const bold = (host) =>
  fnv1a(
    host.rasterize({
      text: "HI",
      fontSize: 50,
      fill: [255, 255, 255, 255],
      fontWeight: 700,
      contentVersion: 1n,
    }).rgba8,
  );
assert.equal(bold(whole), bold(boldOnly));
whole.close();
boldOnly.close();
console.log("verified a font collection registers every face in wasm");

const { summarizeWaveform } = await import(
  pathToFileURL(join(packageRoot, "dist", "waveform.js")).href
);
const frames = 48_000 + 123;
const planes = [0, 1].map((channel) =>
  Float32Array.from({ length: frames }, (_, index) =>
    Math.fround(
      Math.sin((index / (37 + channel * 54)) * 2 * Math.PI) *
        (index % 11 === 0 ? -0.9 : 0.6),
    ),
  ),
);
const waveform = summarizeWaveform(wasm, planes, {
  bucketSamples: 480,
  levels: 3,
  blockFrames: 1_000,
});
assert.equal(waveform.frames, frames);
assert.deepEqual(
  waveform.levels.map((level) => level.bucketSamples),
  [480, 1_920, 7_680],
);
for (const level of waveform.levels) {
  planes.forEach((plane, channel) => {
    const buckets = level.channels[channel];
    assert.equal(buckets.length / 3, Math.ceil(frames / level.bucketSamples));
    for (let bucket = 0; bucket < buckets.length / 3; bucket += 1) {
      const run = plane.subarray(
        bucket * level.bucketSamples,
        (bucket + 1) * level.bucketSamples,
      );
      const squares = run.reduce((sum, sample) => sum + sample * sample, 0);
      assert.equal(buckets[bucket * 3], Math.min(...run));
      assert.equal(buckets[bucket * 3 + 1], Math.max(...run));
      assert.ok(
        Math.abs(buckets[bucket * 3 + 2] - Math.sqrt(squares / run.length)) <
          1e-6,
      );
    }
  });
}
console.log(
  `verified the wasm waveform summary over ${waveform.levels.length} levels`,
);

// Loudness: a -23 dBFS stereo 1 kHz sine is -23 LUFS (EBU Tech 3341 case 1).
const { measureLoudness, planLoudnessNormalization } = await import(
  pathToFileURL(join(packageRoot, "dist", "loudness.js")).href
);
const rate = 48_000;
const tone = Float32Array.from({ length: rate * 20 }, (_, index) =>
  Math.fround(
    10 ** (-23 / 20) * Math.sin((2 * Math.PI * 1_000 * index) / rate),
  ),
);
const report = measureLoudness(wasm, [tone, tone], rate, 4_096);
assert.ok(Math.abs(report.integratedLufs + 23) < 0.1, JSON.stringify(report));
assert.equal(report.frames, tone.length);
const plan = planLoudnessNormalization(wasm, report, -14, -1);
assert.ok(
  Math.abs(plan.integratedLufs + 14) < 1e-9 &&
    !plan.peakLimited &&
    !plan.limiterLimited,
);
// Under a 0.1 (-20 dBFS) master limiter the -23 dBFS sine can take 2.99 dB, not 9.
const limited = planLoudnessNormalization(
  wasm,
  { ...report, limiterThreshold: 0.1 },
  -14,
  -1,
);
assert.ok(
  limited.limiterLimited && report.samplePeakDbfs + limited.gainDb < -20,
);
assert.equal(
  planLoudnessNormalization(
    wasm,
    measureLoudness(wasm, [new Float32Array(rate)], rate),
    -14,
  ),
  null,
);
console.log(
  `verified the wasm loudness meter: ${report.integratedLufs.toFixed(3)} LUFS for EBU Tech 3341 case 1`,
);

// Captions: cues split onto rows where they overlap.
const { parseCaptions, captionTrack } = await import(
  pathToFileURL(join(packageRoot, "dist", "captions.js")).href
);
const imported = parseCaptions(
  wasm,
  "1\n00:00:01,000 --> 00:00:03,000\n<i>One</i>\n\n2\n00:00:02,000 --> 00:00:04,500\nTwo\n\nbroken\n",
  "srt",
);
// Italics become a run's class, as natively.
assert.deepEqual(imported, {
  rows: [
    [
      {
        timeline_range: { start: 120_000, end: 360_000 },
        text: [{ text: "One", class: ["i"] }],
      },
    ],
    [{ timeline_range: { start: 240_000, end: 540_000 }, text: "Two" }],
  ],
  skipped: 1,
  notes: {
    extraVoices: 0,
    droppedTimestamps: 0,
    droppedClasses: 0,
    flattened: 0,
  },
});
assert.throws(() => parseCaptions(wasm, "no header", "webvtt"), /WEBVTT/);
assert.equal(captionTrack("subs", "Subs", imported.rows[0]).kind, "caption");
console.log(
  "verified the wasm caption parser splits overlapping cues onto rows",
);

// Lint: wasm reports what the shared corpus pins natively.
const { lintProject } = await import(
  pathToFileURL(join(packageRoot, "dist", "project.js")).href
);
const lintCorpus = JSON.parse(
  await readFile(join(root, "tests", "cases", "lint-corpus.json"), "utf8"),
);
for (const fixture of lintCorpus) {
  assert.deepEqual(
    lintProject(wasm, fixture.project),
    fixture.issues,
    fixture.name,
  );
}
console.log(`verified ${lintCorpus.length} lint cases through the wasm engine`);

// Vector shapes: the browser module rasterizes and strokes the parity scenes to the same coverage
// and outline bits, and builds the same scenes, as the native tests pin.
const vectorGolden = JSON.parse(
  await readFile(join(root, "tests", "golden", "vector_coverage.json"), "utf8"),
);
assert.deepEqual(JSON.parse(wasm.vectorParityHashes()), vectorGolden);
console.log(
  `verified ${vectorGolden.coverage.length} coverage and ${vectorGolden.trees.length} scene hashes are identical in wasm and natively`,
);

// Shape geometry: the editor's queries answer in wasm what they answer natively, through the
// typed wrapper too.
const geometryGolden = JSON.parse(
  await readFile(
    join(root, "tests", "golden", "geometry_queries.json"),
    "utf8",
  ),
);
assert.deepEqual(JSON.parse(wasm.geometryParityAnswers()), geometryGolden);
const { pathLength, trimPath, hitTest } = await import(
  pathToFileURL(join(packageRoot, "dist", "geometry.js")).href
);
const curve = {
  subpaths: [
    {
      closed: false,
      vertices: [
        { point: { x: 0.1, y: 0.8 }, out_tangent: { x: 0.2, y: -0.6 } },
        { point: { x: 0.9, y: 0.2 }, in_tangent: { x: -0.3, y: 0.1 } },
      ],
    },
  ],
};
const size = { width: 320, height: 180 };
assert.deepEqual(
  pathLength(wasm, curve, size),
  JSON.parse(geometryGolden.pathLength),
);
assert.deepEqual(
  trimPath(wasm, curve, size, 0.25, 0.75, 0.1),
  JSON.parse(geometryGolden.trimPath),
);
const square = {
  subpaths: [
    {
      closed: true,
      vertices: [
        { point: { x: 0.25, y: 0.25 } },
        { point: { x: 0.75, y: 0.25 } },
        { point: { x: 0.75, y: 0.75 } },
        { point: { x: 0.25, y: 0.75 } },
      ],
    },
  ],
};
const filled = [
  { type: "path", path: { mode: "constant", value: square } },
  {
    type: "fill",
    paint: {
      mode: "constant",
      value: { type: "solid", color: { red: 1, green: 0, blue: 0, alpha: 1 } },
    },
  },
];
assert.equal(hitTest(wasm, filled, 0, size, { x: 0.5, y: 0.5 }), "items[1]");
assert.equal(hitTest(wasm, filled, 0, size, { x: 0.1, y: 0.5 }), undefined);
console.log(
  `verified ${Object.keys(geometryGolden).length} geometry queries answer the same in wasm and natively`,
);

// Layout: the browser module lays out the pinned layouts with the fixture font to the same bits
// as natively, through the typed wrapper too.
const layoutGolden = JSON.parse(
  await readFile(join(root, "tests", "golden", "layout_measure.json"), "utf8"),
);
const { measureLayout } = await import(
  pathToFileURL(join(packageRoot, "dist", "layout.js")).href
);
const measurer = new wasm.WasmTextMeasurer();
measurer.addFont(
  "Alpha2 Fixture",
  await readFile(join(root, layoutGolden.font)),
  0,
);
for (const layoutCase of layoutGolden.cases) {
  const picture = { width: layoutCase.width, height: layoutCase.height };
  // A case may give the project canvas its pixels are pixels of; the picture's own by default.
  const [canvasWidth, canvasHeight] = layoutCase.canvas ?? [];
  assert.equal(
    measurer.measureLayout(
      JSON.stringify(layoutCase.generator),
      picture.width,
      picture.height,
      layoutCase.time,
      canvasWidth,
      canvasHeight,
    ),
    layoutCase.expected,
    layoutCase.name,
  );
  const canvas = layoutCase.canvas
    ? { width: canvasWidth, height: canvasHeight }
    : undefined;
  assert.deepEqual(
    measureLayout(
      measurer,
      layoutCase.generator,
      picture,
      layoutCase.time,
      canvas,
    ),
    JSON.parse(layoutCase.expected),
    layoutCase.name,
  );
}
measurer.free();
console.log(
  `verified ${layoutGolden.cases.length} layouts measure the same in wasm and natively`,
);

// Text: the browser module measures the pinned text generators to the same bits as natively and
// as the Python package, which registers the font file by its stem.
const textGolden = JSON.parse(
  await readFile(join(root, "tests", "golden", "text_measure.json"), "utf8"),
);
const textMeasurer = new wasm.WasmTextMeasurer();
textMeasurer.addFontFile(
  "Alpha2Fixture",
  await readFile(join(root, textGolden.font)),
);
for (const textCase of textGolden.cases) {
  assert.equal(
    textMeasurer.measure(
      JSON.stringify(textCase.generator),
      textCase.width,
      textCase.height,
    ),
    textCase.expected,
    textCase.name,
  );
}
textMeasurer.free();
console.log(
  `verified ${textGolden.cases.length} texts measure the same in wasm and natively`,
);

// The browser half: each page's frames against the native ones, held to the bounds each project
// allows, and the browser sink's rate against the engine's.

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

import { PEAK_CODES, peakExcess } from "../parity-tolerance.mjs";
import { browserParity } from "./browser-server.mjs";
import { effectsProjectPath } from "./fixtures.mjs";

/** Runs every page in `browser` against the native frames `nativeFrames` wrote. */
export async function browserChecks(
  browser,
  {
    nativeFrame,
    complexFrames,
    shapesFrames,
    layoutFrames,
    effectsFrames,
    hdrFrames,
    orientationFrames,
    extensionFrames,
  },
) {
  const metrics = await browserParity(browser, nativeFrame);
  console.log(`cross-topology parity metrics: ${JSON.stringify(metrics)}`);
  // Measured in I420 code space: one code of unorm8 rounding on each side plus the renderer's error.
  assert.ok(
    metrics.wasm.max <= 4,
    `wasm max I420 code error ${metrics.wasm.max} exceeds 4`,
  );
  assert.ok(
    metrics.wasm.mean <= 0.5,
    `wasm mean I420 code error ${metrics.wasm.mean} exceeds 0.5`,
  );
  // The native frame is the MPEG-4 Part 2 export as the media CLI decodes it: the codec's quantizer
  // and its 4:2:0 chroma, sited on even luma columns and upsampled by nearest sample, move this
  // golden's one-pixel details by up to 117 codes. What is bounded is how many samples move more
  // than PEAK_CODES (measured: 2191 of 12,288 with the software encoder, which every machine
  // runs) and the mean, not the worst sample.
  assert.ok(
    metrics.native.outliers <= metrics.native.samples / 5,
    `native encoded frame: ${metrics.native.outliers} of ${metrics.native.samples} samples more than ${PEAK_CODES} codes from the golden; at most one in five allowed`,
  );
  assert.ok(
    metrics.native.mean <= 5,
    `native encoded mean channel error ${metrics.native.mean} exceeds 5`,
  );
  console.log(
    `verified wasm/native parity against one golden${on(metrics)}: ${JSON.stringify(metrics)}`,
  );
  const complex = await browserParity(browser, nativeFrame, "/complex", {
    "/native.y4m": complexFrames,
  });
  console.log(`complex project parity metrics: ${JSON.stringify(complex)}`);
  assert.equal(complex.frames, 36, "every frame of the complex project");
  // Cross-GPU float rounding and edge sampling. A discrete NVIDIA GPU against headless Chrome:
  // about 92% exact, nearly all others one code apart; bounds leave other GPUs headroom.
  const complexPeak = peakExcess({
    max: Math.max(complex.y.max, complex.u.max, complex.v.max),
    outliers: complex.outliers,
  });
  assert.equal(complexPeak, null, `complex project: ${complexPeak}`);
  for (const plane of ["y", "u", "v"]) {
    assert.ok(
      complex[plane].mean <= 0.25,
      `complex ${plane} mean code error ${complex[plane].mean} exceeds 0.25`,
    );
  }
  const samples = complex.histogram.reduce((sum, count) => sum + count, 0);
  assert.ok(
    complex.histogram[2] + complex.histogram[3] <= samples / 1_000,
    `more than 1 in 1000 samples are over a code apart: ${JSON.stringify(complex.histogram)}`,
  );
  console.log(
    `verified wasm/native parity on every frame of the complex project${on(complex)}`,
  );
  // Shapes: coverage is the same bits on both sides; paint and compositing are the GPU's, held to
  // the complex project's bounds.
  const shapes = await browserParity(browser, nativeFrame, "/shapes", {
    "/native.y4m": shapesFrames,
  });
  console.log(`shapes project parity metrics: ${JSON.stringify(shapes)}`);
  assert.equal(shapes.frames, 36, "every frame of the shapes project");
  const shapesPeak = peakExcess({
    max: Math.max(shapes.y.max, shapes.u.max, shapes.v.max),
    outliers: shapes.outliers,
  });
  assert.equal(shapesPeak, null, `shapes project: ${shapesPeak}`);
  for (const plane of ["y", "u", "v"]) {
    assert.ok(
      shapes[plane].mean <= 0.25,
      `shapes ${plane} mean code error ${shapes[plane].mean} exceeds 0.25`,
    );
  }
  const shapeSamples = shapes.histogram.reduce((sum, count) => sum + count, 0);
  assert.ok(
    shapes.histogram[2] + shapes.histogram[3] <= shapeSamples / 1_000,
    `shapes: more than 1 in 1000 samples are over a code apart: ${JSON.stringify(shapes.histogram)}`,
  );
  console.log(
    `verified wasm/native parity on every frame of the shapes project${on(shapes)}`,
  );
  // Layouts: the same layout from the same font on both sides; text, fills and composites held to
  // the shapes project's bounds.
  const layouts = await browserParity(browser, nativeFrame, "/layout", {
    "/native.y4m": layoutFrames,
  });
  console.log(`layout project parity metrics: ${JSON.stringify(layouts)}`);
  assert.equal(layouts.frames, 36, "every frame of the layout project");
  const layoutPeak = peakExcess({
    max: Math.max(layouts.y.max, layouts.u.max, layouts.v.max),
    outliers: layouts.outliers,
  });
  assert.equal(layoutPeak, null, `layout project: ${layoutPeak}`);
  for (const plane of ["y", "u", "v"]) {
    assert.ok(
      layouts[plane].mean <= 0.25,
      `layout ${plane} mean code error ${layouts[plane].mean} exceeds 0.25`,
    );
  }
  const layoutSamples = layouts.histogram.reduce(
    (sum, count) => sum + count,
    0,
  );
  assert.ok(
    layouts.histogram[2] + layouts.histogram[3] <= layoutSamples / 1_000,
    `layout: more than 1 in 1000 samples are over a code apart: ${JSON.stringify(layouts.histogram)}`,
  );
  console.log(
    `verified wasm/native parity on every frame of the layout project${on(layouts)}`,
  );
  const effects = await browserParity(browser, nativeFrame, "/effects", {
    "/native.y4m": effectsFrames,
  });
  console.log(`effects parity metrics: ${JSON.stringify(effects)}`);
  assert.deepEqual(
    effects.unused,
    [],
    "every built-in effect and transition is in the effects project (scripts/generate-effects-project.mjs)",
  );
  // Each body is held to the complex project's bounds on every frame it draws. Anti-aliasing uses
  // `fwidthFine`: plain `fwidth` differed by up to 41 codes across GPUs on edges.
  const effectsProject = JSON.parse(await readFile(effectsProjectPath, "utf8"));
  assert.equal(
    effects.frames,
    projectFrames(effectsProject),
    "every frame of the effects project",
  );
  const bodies = bodiesByFrame(effectsProject, effects.frames);
  const byBody = new Map();
  bodies.forEach((body, frame) => {
    const seen = byBody.get(body) ?? {
      max: 0,
      mean: 0,
      over: 0,
      outliers: 0,
    };
    seen.max = Math.max(seen.max, effects.worstFrames[frame]);
    seen.mean = Math.max(seen.mean, effects.frameMeans[frame]);
    seen.over += effects.frameOver[frame];
    seen.outliers += effects.frameOutliers[frame];
    byBody.set(body, seen);
  });
  let checkedSamples = 0;
  let checkedOver = 0;
  for (const [body, seen] of byBody) {
    const peak = peakExcess(seen);
    assert.equal(peak, null, `${body}: browser and native ${peak}`);
    assert.ok(
      seen.mean <= 0.25,
      `${body}: browser and native ${seen.mean} codes apart on average in a frame; allowed 0.25`,
    );
    checkedOver += seen.over;
    checkedSamples +=
      bodies.filter((other) => other === body).length * ((96 * 64 * 3) / 2);
  }
  assert.ok(
    checkedOver <= checkedSamples / 1_000,
    `more than 1 in 1000 samples are over a code apart: ${checkedOver} of ${checkedSamples}`,
  );
  console.log(
    `verified wasm/native parity on every built-in effect and transition${on(effects)}`,
  );
  const hdr = await browserParity(browser, nativeFrame, "/hdr", {
    "/native.y4m": hdrFrames,
  });
  console.log(`HDR project parity metrics: ${JSON.stringify(hdr)}`);
  assert.equal(hdr.frames, 12, "every frame of the HDR project");
  assert.deepEqual(
    hdr.transfers.slice(0, 3),
    ["pq", "hlg", "pq"],
    "the browser decoded the tagged sources as HDR",
  );
  assert.ok(
    !["pq", "hlg"].includes(hdr.transfers[3]),
    `the untagged source decoded as ${hdr.transfers[3]}, which only the project's declaration makes PQ`,
  );
  assert.deepEqual(
    hdr.declaredTransfers,
    ["pq", "hlg", "pq", null],
    "the containers declare the tagged sources HDR to an importer, and say nothing of the untagged one",
  );
  assert.deepEqual(
    hdr.sitings,
    ["left", "left", "top-left", "left"],
    "the streams' parameter sets say where their chroma sits",
  );
  // The ramp must survive as a ramp: a refused or blank source would agree with nothing.
  assert.ok(
    hdr.lumaSpread >= 150,
    `the tone-mapped ramp spans only ${hdr.lumaSpread} luma codes`,
  );
  for (const plane of ["y", "u", "v"]) {
    assert.ok(
      hdr[plane].max <= 4,
      `HDR ${plane} max code error ${hdr[plane].max} exceeds 4`,
    );
    assert.ok(
      hdr[plane].mean <= 0.25,
      `HDR ${plane} mean code error ${hdr[plane].mean} exceeds 0.25`,
    );
  }
  console.log(
    `verified wasm/native parity on PQ and HLG tone mapping, signalled chroma siting, and a declared color${on(hdr)}`,
  );
  const extensions = await browserParity(browser, nativeFrame, "/extensions", {
    "/native.y4m": extensionFrames,
  });
  console.log(
    `extension sources parity metrics: ${JSON.stringify(extensions)}`,
  );
  assert.equal(extensions.frames, 30, "every frame of the extensions project");
  for (const plane of ["y", "u", "v"]) {
    assert.ok(
      extensions[plane].max <= 4,
      `extensions ${plane} max code error ${extensions[plane].max} exceeds 4`,
    );
    assert.ok(
      extensions[plane].mean <= 0.25,
      `extensions ${plane} mean code error ${extensions[plane].mean} exceeds 0.25`,
    );
  }
  console.log(
    `verified wasm/native parity on every extension source${on(extensions)}`,
  );
  const orientation = await browserParity(
    browser,
    nativeFrame,
    "/orientation",
    {
      "/native.y4m": orientationFrames,
    },
  );
  console.log(`orientation parity metrics: ${JSON.stringify(orientation)}`);
  assert.equal(orientation.frames, 16, "every frame of the project");
  assert.deepEqual(
    orientation.displaySizes,
    {
      hflip: [128, 96],
      vflip: [128, 96],
      transverse: [128, 96],
      "transverse-mkv": [128, 96],
      pasp: [171, 96],
      "vui-sar-ts": [171, 96],
      // mediabunny declares no ratio for this MP4; the browser's decoder reads the VUI.
      "vui-sar-mp4": [171, 96],
    },
    "the browser shows 4:3 pixels 16:9",
  );
  assert.deepEqual(
    orientation.orientations,
    {
      hflip: { rotationDegrees: 0, mirror: true },
      vflip: { rotationDegrees: 180, mirror: true },
      transverse: { rotationDegrees: 90, mirror: true },
      "transverse-mkv": { rotationDegrees: 90, mirror: true },
      pasp: { rotationDegrees: 0, mirror: false },
      "vui-sar-ts": { rotationDegrees: 0, mirror: false },
      "vui-sar-mp4": { rotationDegrees: 0, mirror: false },
    },
    "the containers' orientations, mirrors included",
  );
  // A wrong turn or lost mirror moves the whole picture by tens of codes; measured at most 2 codes
  // apart (the browser's JPEG decoder rounds its own YCbCr conversion).
  for (const plane of ["y", "u", "v"]) {
    assert.ok(
      orientation[plane].max <= 4,
      `orientation ${plane} max code error ${orientation[plane].max} exceeds 4`,
    );
    assert.ok(
      orientation[plane].mean <= 0.25,
      `orientation ${plane} mean code error ${orientation[plane].mean} exceeds 0.25`,
    );
  }
  console.log(
    `verified wasm/native parity on stills' Exif orientation and colour, mirroring display matrices, and pixel aspect ratios${on(orientation)}`,
  );
  const sink = await browserParity(browser, nativeFrame, "/sink");
  for (const level of ["low", "high"]) {
    const { videoRateControl, applied } = sink[level];
    // The browser may take the quantizer or fall back to the bitrate, but the number must be the
    // engine's. The quantizer is in each frame's encode options, so those are compared.
    const { encodedQuantizers } = sink[level];
    if (applied?.bitrateMode === "quantizer") {
      assert.deepEqual(
        encodedQuantizers,
        [videoRateControl.quantizer],
        `every frame at ${level} must be encoded at the engine's quantizer: ${JSON.stringify(sink[level])}`,
      );
    } else {
      assert.equal(
        applied?.bitrate,
        videoRateControl.bitrate,
        `the browser encoder at ${level} must run the engine's rate: ${JSON.stringify(sink[level])}`,
      );
      assert.deepEqual(
        encodedQuantizers,
        [null],
        `a bitrate-mode encoder at ${level} is given no quantizer: ${JSON.stringify(sink[level])}`,
      );
    }
  }
  assert.ok(
    sink.low.bytes < sink.high.bytes,
    `the browser sink wrote ${sink.low.bytes} bytes at low and ${sink.high.bytes} at high`,
  );
  console.log(
    `verified the browser sink encodes at the engine's rate: ${JSON.stringify(sink)}`,
  );
}

/** ` on <backend> (<adapter>)`: what a page's engine ran on, for its verified line. */
function on(result) {
  const { backend = "", name = "" } = result.adapter ?? {};
  return ` on ${backend || "no backend"} (${name})`;
}

/** The frames of `project`'s timeline, to the end of its last clip. */
function projectFrames(project) {
  const ticksPerFrame =
    (120_000 * project.settings.frame_rate.den) /
    project.settings.frame_rate.num;
  const end = Math.max(
    ...project.timeline.tracks.flatMap((track) =>
      track.clips.map((clip) => clip.timeline_range.end),
    ),
  );
  return Math.ceil(end / ticksPerFrame);
}

/**
 * What each frame of the effects project draws: the effect of the clip it shows, the transition it
 * is in, or "plain".
 */
function bodiesByFrame(project, frames) {
  const ticksPerFrame =
    (120_000 * project.settings.frame_rate.den) /
    project.settings.frame_rate.num;
  const looks = new Map(
    project.compositions.map((composition) => [
      composition.id,
      composition.nodes.find((node) => node.op.type === "effect").op.effect.id,
    ]),
  );
  const within = (range, ticks) => ticks >= range.start && ticks < range.end;
  return Array.from({ length: frames }, (_, frame) => {
    const ticks = frame * ticksPerFrame;
    for (const track of project.timeline.tracks) {
      for (const transition of track.transitions ?? [])
        if (within(transition.timeline_range, ticks))
          return transition.effect.id;
      for (const clip of track.clips)
        if (clip.composition && within(clip.timeline_range, ticks))
          return looks.get(clip.composition.id);
    }
    return "plain";
  });
}

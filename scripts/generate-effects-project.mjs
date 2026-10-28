#!/usr/bin/env node
// Writes the host parity effects project, `tests/cases/host-parity-effects-project.json`: every
// built-in effect on a clip of its own, then every built-in transition between a video and a still.
//
//   node scripts/generate-effects-project.mjs [--check]
//
// Each effect is drawn with the look below, parameters away from its defaults where they show what
// it does. A built-in missing from the tables fails the browser parity check, which compares the
// project with the catalog. With `--check` it writes nothing and exits non-zero when the committed
// project is stale.

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const projectPath = join(root, "tests/cases/host-parity-effects-project.json");

/** How long each effect's clip, each clip of the chain and each transition lasts, in ticks. */
const EFFECT_TICKS = 10_000;
const CHAIN_TICKS = 35_000;
const TRANSITION_TICKS = 15_000;
/** Source starts the video clips cycle through. */
const VIDEO_STARTS = [0, 20_000, 40_000, 60_000, 80_000];

const float = (value) => ({ type: "float", value });
const vec2 = (value) => ({ type: "vec2", value });
const vec4 = (value) => ({ type: "vec4", value });
const color = (value) => ({ type: "color", value });
const choice = (value) => ({ type: "enum", value });
const bool = (value) => ({ type: "bool", value });

/**
 * Every built-in effect by its name after `alpha2.effect.`, in project order, with the values of
 * the parameters it sets, the resources it binds and the sources its named inputs show.
 */
const EFFECTS = [
  ["gaussian-blur", { params: { radius: float(0.03) } }],
  ["chroma-key", {}],
  ["lut-3d", { resources: { lut: "grade" } }],
  [
    "color-controls",
    {
      params: {
        exposure: float(0.4),
        contrast: float(1.3),
        saturation: float(0.6),
        temperature: float(0.3),
        tint: float(-0.2),
      },
    },
  ],
  ["invert", { params: { amount: float(0.8) } }],
  [
    "black-and-white",
    { params: { red: float(1), green: float(0), blue: float(0) } },
  ],
  ["sepia", {}],
  ["duotone", {}],
  [
    "gradient-map",
    {
      params: {
        shadows: color([0, 0, 0.5, 1]),
        midtones: color([0.9, 0.1, 0.2, 1]),
        highlights: color([1, 1, 0.2, 1]),
      },
    },
  ],
  ["posterize", { params: { levels: float(4) } }],
  ["threshold", { params: { softness: float(0.1) } }],
  [
    "levels",
    {
      params: {
        input_black: float(0.1),
        input_white: float(0.85),
        gamma: float(1.4),
      },
    },
  ],
  ["hue-saturation", { params: { hue: float(60), saturation: float(0.3) } }],
  ["vibrance", { params: { amount: float(0.7) } }],
  [
    "channel-mixer",
    { params: { red: vec4([0, 0, 1, 0]), blue: vec4([1, 0, 0, 0]) } },
  ],
  [
    "lift-gamma-gain",
    {
      params: { lift: vec4([0.05, 0, -0.05, 0]), gamma: vec4([0, 0, 0, 0.2]) },
    },
  ],
  ["color-isolate", { params: { hue: float(0), range: float(40) } }],
  ["solarize", {}],
  ["fill", { params: { color: color([1, 0.4, 0, 1]), amount: float(0.6) } }],
  ["vignette", {}],
  ["sharpen", { params: { amount: float(1.5) } }],
  ["pixelate", { params: { blocks: float(16) } }],
  ["glow", { params: { threshold: float(0.4), intensity: float(1.2) } }],
  ["film-grain", { params: { amount: float(0.5) } }],
  [
    "crop",
    {
      params: {
        top_left: vec2([0.1, 0.05]),
        bottom_right: vec2([0.15, 0.2]),
        feather: float(0.03),
      },
    },
  ],
  ["mirror", { params: { axis: choice(1) } }],
  ["luma-key", { params: { threshold: float(0.3), softness: float(0.2) } }],
  ["spill-suppress", { params: { amount: float(0.8) } }],
  [
    "alpha-adjust",
    { params: { black: float(0.1), white: float(0.8), gamma: float(2) } },
  ],
  ["matte-choker", { params: { choke: float(-0.6), radius: float(0.02) } }],
  ["set-matte", { params: { mode: choice(1) }, inputs: { matte: "still" } }],
  ["directional-blur", { params: { length: float(0.1), angle: float(30) } }],
  ["radial-blur", { params: { amount: float(0.3) } }],
  ["unsharp-mask", { params: { amount: float(2), radius: float(0.02) } }],
  ["tilt-shift", { params: { blur: float(0.1), focus: float(0.1) } }],
  ["lens-blur", { params: { radius: float(0.015), highlights: float(4) } }],
  ["surface-blur", { params: { radius: float(0.015), threshold: float(0.2) } }],
  ["rgb-split", { params: { amount: float(0.02), angle: float(30) } }],
  ["glitch", { params: { amount: float(0.8), color_split: float(0.02) } }],
  ["crt", { params: { curvature: float(0.3), scanlines: float(0.6) } }],
  ["vhs", { params: { amount: float(0.8), noise: float(0.3) } }],
  // Upright cells four pixels wide: no cell border crosses a pixel centre, where a rounding
  // error would move a dot to the next cell.
  [
    "halftone",
    { params: { mode: choice(1), cells: float(24), angle: float(0) } },
  ],
  ["emboss", { params: { distance: float(0.004), keep_color: bool(true) } }],
  ["edge-detect", { params: { width: float(0.003), colored: bool(true) } }],
  [
    "drop-shadow",
    {
      params: {
        color: color([0.1, 0.0, 0.3, 1.0]),
        distance: float(0.03),
        softness: float(0.02),
      },
    },
  ],
  [
    "outline",
    { params: { color: color([1.0, 0.9, 0.0, 1.0]), width: float(0.01) } },
  ],
  ["light-leak", { params: { intensity: float(1.2), speed: float(1) } }],
  ["letterbox", { params: { aspect: float(2.39) } }],
  ["oil-paint", { params: { radius: float(0.008) } }],
  ["swirl", { params: { angle: float(270) } }],
  ["bulge", { params: { strength: float(0.8) } }],
  [
    "wave-warp",
    {
      params: {
        amplitude: float(0.02),
        wavelength: float(0.15),
        speed: float(1),
      },
    },
  ],
  ["ripple", { params: { amplitude: float(0.02), wavelength: float(0.08) } }],
  ["lens-distortion", { params: { k1: float(0.4), k2: float(0.1) } }],
  ["polar-coordinates", { params: { edge: choice(1) } }],
  ["offset", { params: { shift: vec2([0.3, 0.2]) } }],
  [
    "corner-pin",
    {
      params: {
        top_left: vec2([0.1, 0.05]),
        top_right: vec2([0.85, 0.15]),
        bottom_right: vec2([0.95, 0.9]),
        bottom_left: vec2([0.05, 0.95]),
      },
    },
  ],
  ["shake", { params: { amplitude: float(0.05), rotation: float(5) } }],
  [
    "turbulent-displace",
    { params: { amount: float(0.04), evolution: float(1) } },
  ],
  [
    "displacement-map",
    {
      params: { amount_x: float(0.05), amount_y: float(-0.05) },
      inputs: { map: "still" },
    },
  ],
  ["kaleidoscope", { params: { segments: float(8), angle: float(15) } }],
  ["tile", { params: { tiles: vec2([3, 3]), mirror: bool(true) } }],
];

/**
 * Every built-in transition by its name after `alpha2.transition.`, in project order, with the values
 * of the parameters it sets.
 */
const TRANSITIONS = [
  ["fade"],
  ["directional"],
  ["circle-open"],
  ["angular"],
  ["pixelize"],
  ["directional-wipe"],
  ["circle"],
  ["cross-warp"],
  ["dreamy"],
  ["radial"],
  ["bounce"],
  ["directional-scaled"],
  ["slides"],
  ["fold"],
  ["directional-easing"],
  ["hsv-fade"],
  ["left-right"],
  ["top-bottom"],
  ["multiply-blend"],
  ["rolls"],
  ["split-slide-in-horizontal"],
  ["split-slide-in-vertical"],
  ["split-slide-out-horizontal"],
  ["split-slide-out-vertical"],
  ["split-slide-in-out-horizontal"],
  ["split-slide-in-out-vertical"],
  ["rotate"],
  ["x-axis-translation"],
  ["cover"],
  ["uncover"],
  ["wipe-up"],
  ["wipe-down"],
  ["wipe-left"],
  ["wipe-right"],
  ["window-blinds"],
  ["window-slice"],
  ["wind"],
  ["pinwheel"],
  ["heart"],
  ["color-phase"],
  ["fade-color"],
  ["fade-grayscale"],
  ["morph"],
  ["ripple"],
  ["squeeze"],
  ["simple-zoom"],
  ["circle-crop"],
  ["colour-distance"],
  ["polka-dots-curtain"],
  ["directional-warp"],
  ["horizontal-open"],
  ["horizontal-close"],
  ["vertical-open"],
  ["vertical-close"],
  ["screen-melt"],
  ["squares-wire"],
  ["static-wipe"],
  ["zoom-left-wipe"],
  ["zoom-right-wipe"],
  ["gradient-wipe"],
  ["box"],
  ["chessboard"],
  ["bow-tie"],
  ["bow-tie-horizontal"],
  ["bow-tie-vertical"],
  ["fragment"],
  ["polar-function"],
  ["rectangle"],
  ["rectangle-crop"],
  ["star-wipe"],
  ["shape-wipe"],
  ["cube"],
  ["doorway"],
  ["swap"],
  ["grid-flip"],
  ["tiles-wave"],
  ["book-flip"],
  ["simple-flip"],
  ["page-peel"],
  ["strip-datamosh-glitch"],
  ["drop-zone-flicker"],
  ["glitch-displace"],
  ["old-tv-lost-signal"],
  ["static-fade"],
  ["tv-static"],
  ["parametric-glitch"],
  ["glitch"],
  ["defocus-blur", { params: { blur_size: float(0.05) } }],
  ["dreamy-zoom"],
  ["revolve"],
  ["linear-blur", { params: { intensity: float(0.3) } }],
  ["mosaic-tiles"],
  ["rotate-scale-fade"],
  ["rotate-scale-vanish"],
  ["scale-in"],
  ["simple-zoom-out"],
  ["zoom-in-circles"],
  ["zoom-in-out"],
  ["zoom-blur"],
  ["whip-pan", { params: { direction: choice(2) } }],
  ["block-dissolve"],
  ["random-squares"],
  ["burn-noise"],
  ["perlin"],
  ["film-burn"],
  ["overexposure"],
  ["advanced-mosaic"],
  ["burn"],
  ["crosshatch"],
  ["butterfly-wave-scrawler"],
  ["crazy-parametric-fun"],
  ["edge"],
  ["fly-eye"],
  ["hexagonalize"],
  ["kaleidoscope"],
  ["luma-burn"],
  ["luminance-melt"],
  ["power-kaleido"],
  ["random-noise"],
  ["swirl"],
  ["undulating-burn-out"],
  ["water-drop"],
];

/** `{ name: value }` as constant animated values. */
function constants(values) {
  return Object.fromEntries(
    Object.entries(values).map(([name, value]) => [
      name,
      { mode: "constant", value },
    ]),
  );
}

/** The versioned effect `id` with `params` and `resources`, each only when set. */
function effect(id, { params, resources } = {}) {
  return {
    id,
    version: 1,
    ...(params ? { params: constants(params) } : {}),
    ...(resources ? { resources } : {}),
  };
}

/** The composition that draws one effect over the clip, its named inputs shown by media nodes. */
function look(name, settings) {
  const inputs = Object.entries(settings.inputs ?? {});
  return {
    id: `${name}-look`,
    nodes: [
      { id: "clip", op: { type: "clip" } },
      ...inputs.map(([input, source]) => ({
        id: input,
        op: { type: "media", source_id: source },
      })),
      {
        id: "effect",
        op: {
          type: "effect",
          input: "clip",
          ...(inputs.length > 0
            ? {
                inputs: Object.fromEntries(
                  inputs.map(([input]) => [input, input]),
                ),
              }
            : {}),
          effect: effect(`alpha2.effect.${name}`, settings),
        },
      },
    ],
    output: "effect",
  };
}

function project() {
  const effectClips = EFFECTS.map(([name], index) => ({
    id: `effect-${name}`,
    source_id: "video",
    timeline_range: {
      start: index * EFFECT_TICKS,
      end: (index + 1) * EFFECT_TICKS,
    },
    source_range: {
      start: VIDEO_STARTS[index % VIDEO_STARTS.length],
      end: VIDEO_STARTS[index % VIDEO_STARTS.length] + EFFECT_TICKS,
    },
    composition: { id: `${name}-look` },
  }));
  // A chain of clips, video and still in turn, each overlapping the next by one transition.
  const chainStart = EFFECTS.length * EFFECT_TICKS;
  const step = CHAIN_TICKS - TRANSITION_TICKS;
  const chainClips = Array.from(
    { length: TRANSITIONS.length + 1 },
    (_, index) => {
      const start = chainStart + index * step;
      const sourceStart = index % 2 === 0 ? VIDEO_STARTS[(index / 2) % 4] : 0;
      return {
        id: `chain-${index}`,
        source_id: index % 2 === 0 ? "video" : "still",
        timeline_range: { start, end: start + CHAIN_TICKS },
        source_range: { start: sourceStart, end: sourceStart + CHAIN_TICKS },
      };
    },
  );
  const transitions = TRANSITIONS.map(([name, settings], index) => {
    const start = chainStart + (index + 1) * step;
    return {
      from_clip: `chain-${index}`,
      to_clip: `chain-${index + 1}`,
      timeline_range: { start, end: start + TRANSITION_TICKS },
      easing: { type: "linear" },
      effect: effect(`alpha2.transition.${name}`, settings),
    };
  });
  return {
    schema_version: 1,
    id: "host-parity-effects",
    name: "Host parity: every built-in effect, and every built-in transition between a video and a still",
    settings: {
      width: 96,
      height: 64,
      frame_rate: { num: 24, den: 1 },
      sample_rate: 48000,
    },
    sources: [
      { id: "video", locator: "fixture://parity-rgb", kind: "video" },
      { id: "still", locator: "fixture://parity-image", kind: "image" },
    ],
    effect_resources: [
      { id: "grade", kind: "lut3d", locator: "fixture://parity-grade" },
    ],
    compositions: EFFECTS.map(([name, settings]) => look(name, settings)),
    timeline: {
      tracks: [
        {
          id: "base",
          name: "Base",
          kind: "video",
          clips: [...effectClips, ...chainClips],
          transitions,
        },
      ],
    },
  };
}

const text = JSON.stringify(project(), null, 2);
if (process.argv.includes("--check")) {
  if (readFileSync(projectPath, "utf8") !== text) {
    console.error(
      `${projectPath} is stale: run node scripts/generate-effects-project.mjs`,
    );
    process.exit(1);
  }
} else {
  writeFileSync(projectPath, text);
}

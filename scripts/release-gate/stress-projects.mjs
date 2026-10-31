#!/usr/bin/env node
// Writes the release gate's GPU-work stress inputs into a directory:
//   heavy-effects.json, max-effects.json      runtime effect requests (`alpha2-host export --effects`)
//   heavy-<W>x<H>.json, max-<W>x<H>.json      one two-frame clip running the heavy or the max
//                                             effect, at 64x36, 128x72, 1920x1080, 2560x1440 and
//                                             3840x2160
//   builtin-worst-<W>x<H>.json                the heaviest built-ins at their largest parameters,
//                                             two or three frames each (1920x1080 and up)
//   builtin-worst.segments.json               the tick ranges of each built-in segment
//   effects-<W>x<H>.json                      tests/cases/host-parity-effects-project.json scaled
// Every WGSL body is checked against the registry's limits, read at run time from the renderer's
// sources (registry.rs, loop_bound.rs, call_budget.rs), so the check cannot drift from the
// registry; the host stays the authority when it registers them.
//
// The projects read `video` (tests/corpus/parity-rgb.mov) and `still` (tests/corpus/parity-image.png).
//
// usage: stress-projects.mjs <output directory>

import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

const root = resolve(import.meta.dirname, "..", "..");
const renderSource = (name) =>
  readFileSync(join(root, "crates", "alpha2-render", "src", name), "utf8");

export const SIZES = [
  [64, 36],
  [128, 72],
  [1920, 1080],
  [2560, 1440],
  [3840, 2160],
];
const LARGE_SIZES = SIZES.filter(([width]) => width >= 1920);
const TICKS_PER_SECOND = 120_000;
// The effects project's rate: 24 fps, 5000 ticks a frame.
const FPS = 24;

function constant(source, name, file) {
  const match = new RegExp(`const ${name}: [^=]+= ([^;]+);`).exec(source);
  if (!match) throw new Error(`${file} has no const ${name}`);
  const value = match[1].trim();
  if (/^"[^"]*"$/.test(value)) return value.slice(1, -1);
  if (!/^[\d_ *]+$/.test(value))
    throw new Error(`${file}: ${name} = ${value} is not a product of integers`);
  return value
    .split("*")
    .map((factor) => Number(factor.trim().replaceAll("_", "")))
    .reduce((product, factor) => product * factor, 1);
}

/** The runtime effect limits as the renderer's sources declare them. */
export function registryLimits() {
  const registry = renderSource("registry.rs");
  const forbidden = /let forbidden = \[([^\]]+)\];/.exec(registry);
  const operators =
    /const fn is_operator\(byte: u8\) -> bool \{([\s\S]+?)\n\}/.exec(registry);
  if (!forbidden || !operators)
    throw new Error(
      "registry.rs: the forbidden token list or is_operator moved",
    );
  return {
    maxBodyBytes: constant(registry, "MAX_WGSL_BODY_BYTES", "registry.rs"),
    maxNesting: constant(registry, "MAX_WGSL_NESTING", "registry.rs"),
    maxStatementOperators: constant(
      registry,
      "MAX_WGSL_STATEMENT_OPERATORS",
      "registry.rs",
    ),
    reservedPrefix: constant(registry, "RESERVED_PREFIX", "registry.rs"),
    maxLoopIterations: constant(
      renderSource("loop_bound.rs"),
      "MAX_LOOP_ITERATIONS",
      "loop_bound.rs",
    ),
    maxInvocationCalls: constant(
      renderSource("call_budget.rs"),
      "MAX_INVOCATION_CALLS",
      "call_budget.rs",
    ),
    forbidden: [...forbidden[1].matchAll(/"([^"]+)"/g)].map(
      (match) => match[1],
    ),
    operators: new Set(
      [...operators[1].matchAll(/b'(.)'/g)].map((match) => match[1]),
    ),
  };
}

/** `body` with comments replaced by spaces (newlines kept), as the registry checks only code. */
export function blankComments(body) {
  let out = "";
  let depth = 0;
  let line = false;
  for (let i = 0; i < body.length;) {
    const pair = body.slice(i, i + 2);
    if (line) {
      line = body[i] !== "\n";
      out += body[i] === "\n" ? "\n" : " ";
      i += 1;
    } else if (pair === "/*") {
      depth += 1;
      out += "  ";
      i += 2;
    } else if (depth > 0 && pair === "*/") {
      depth -= 1;
      out += "  ";
      i += 2;
    } else if (depth > 0) {
      out += body[i] === "\n" ? "\n" : " ";
      i += 1;
    } else if (pair === "//") {
      line = true;
      out += "  ";
      i += 2;
    } else {
      out += body[i];
      i += 1;
    }
  }
  return out;
}

const identifierChar = (c) => c !== undefined && /[A-Za-z0-9_]/.test(c);

function findKeyword(code, start, keyword, end = code.length) {
  for (let i = start; ;) {
    const at = code.indexOf(keyword, i);
    if (at < 0 || at >= end) return -1;
    if (
      !identifierChar(code[at - 1]) &&
      !identifierChar(code[at + keyword.length])
    )
      return at;
    i = at + keyword.length;
  }
}

function matching(code, open) {
  let depth = 0;
  for (let k = open; k < code.length; k++) {
    if ("([{".includes(code[k])) depth += 1;
    else if (")]}".includes(code[k])) {
      depth -= 1;
      if (depth === 0) return k;
    }
  }
  throw new Error("unbalanced delimiters");
}

/** The `for` loops of `code` as [start, end) spans; throws on one the registry cannot bound. */
function forLoops(code) {
  const spans = [];
  for (let cursor = 0; ;) {
    const start = findKeyword(code, cursor, "for");
    if (start < 0) return spans;
    let k = start + 3;
    while (/\s/.test(code[k] ?? "")) k += 1;
    if (code[k] !== "(")
      throw new Error(
        "a for loop must be `for (init; condition; update) { ... }`",
      );
    const close = matching(code, k);
    let depth = 0;
    let parts = 1;
    for (const c of code.slice(k + 1, close)) {
      if ("([{".includes(c)) depth += 1;
      else if (")]}".includes(c)) depth = Math.max(0, depth - 1);
      else if (c === ";" && depth === 0) parts += 1;
    }
    if (parts !== 3) throw new Error("a for header must have three parts");
    let b = close + 1;
    while (/\s/.test(code[b] ?? "")) b += 1;
    if (code[b] !== "{") throw new Error("a for body must be a block");
    spans.push([start, matching(code, b) + 1]);
    cursor = b + 1;
  }
}

/** The bodies one invocation of the costliest function runs, as call_budget.rs counts them. */
function invocationCalls(code, maxLoopIterations) {
  const functions = [];
  let depth = 0;
  for (let i = 0; i < code.length; i++) {
    const c = code[i];
    if (c === "{") depth += 1;
    else if (c === "}") depth = Math.max(0, depth - 1);
    else if (
      depth === 0 &&
      code.startsWith("fn", i) &&
      !identifierChar(code[i - 1])
    ) {
      const match = /^fn\s+([A-Za-z_]\w*)/.exec(code.slice(i));
      if (!match) continue;
      const open = code.indexOf("{", i + match[0].length);
      const close = matching(code, open);
      functions.push({ name: match[1], start: open, end: close + 1 });
      i = close;
    }
  }
  const index = new Map(functions.map((f, at) => [f.name, at]));
  const calls = functions.map(({ start, end }) => {
    const loops = forLoops(code.slice(start, end)).map(([a, z]) => [
      a + start,
      z + start,
    ]);
    const found = [];
    for (const match of code.slice(start, end).matchAll(/[A-Za-z_]\w*/g)) {
      const at = start + match.index;
      if (
        index.has(match[0]) &&
        code
          .slice(at + match[0].length, end)
          .trimStart()
          .startsWith("(")
      ) {
        const inLoop = loops.some(([a, z]) => a <= at && at < z);
        found.push([index.get(match[0]), inLoop ? maxLoopIterations : 1]);
      }
    }
    return found;
  });
  const cost = new Array(functions.length);
  const visit = (f, stack) => {
    if (cost[f] !== undefined) return cost[f];
    if (stack.has(f))
      throw new Error(`WGSL function ${functions[f].name} calls itself`);
    let total = 1;
    for (const [callee, times] of calls[f])
      total += times * visit(callee, new Set([...stack, f]));
    cost[f] = total;
    return total;
  };
  let best = { calls: 0, name: null };
  functions.forEach((f, at) => {
    const value = visit(at, new Set());
    if (value > best.calls) best = { calls: value, name: f.name };
  });
  return best;
}

/** What the registry would say of a runtime effect body: its measures and any violated limit. */
export function checkBody(body, limits = registryLimits()) {
  const errors = [];
  const bytes = Buffer.byteLength(body, "utf8");
  if (bytes > limits.maxBodyBytes)
    errors.push(`body is ${bytes} bytes > ${limits.maxBodyBytes}`);
  if (/[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]/.test(body))
    errors.push("control characters");
  const code = blankComments(body);
  for (const word of code.match(/[A-Za-z_]\w*/g) ?? [])
    if (word.startsWith(limits.reservedPrefix))
      errors.push(`reserved identifier ${word}`);
  const statementOperators = Math.max(
    ...code
      .split(/[;{}]/)
      .map(
        (statement) =>
          [...statement].filter((c) => limits.operators.has(c)).length,
      ),
  );
  if (statementOperators > limits.maxStatementOperators)
    errors.push(`a statement has ${statementOperators} operators`);
  let nesting = 0;
  const stack = [];
  const pairs = { ")": "(", "]": "[", "}": "{" };
  for (const c of code) {
    if ("([{".includes(c)) {
      stack.push(c);
      nesting = Math.max(nesting, stack.length);
    } else if (")]}".includes(c) && stack.pop() !== pairs[c]) {
      errors.push("unbalanced delimiters");
      break;
    }
  }
  if (stack.length) errors.push("unbalanced delimiters");
  if (nesting > limits.maxNesting)
    errors.push(`nesting ${nesting} > ${limits.maxNesting}`);
  for (const token of code.split(/[^A-Za-z0-9_@<]/))
    if (limits.forbidden.includes(token))
      errors.push(`forbidden token ${token}`);
  let loops = [];
  let calls = null;
  try {
    loops = forLoops(code);
    const costliest = invocationCalls(code, limits.maxLoopIterations);
    calls = costliest.calls;
    if (calls > limits.maxInvocationCalls)
      errors.push(
        `${costliest.name} runs ${calls} bodies > ${limits.maxInvocationCalls}`,
      );
  } catch (error) {
    errors.push(error.message);
  }
  return {
    bytes,
    statementOperators,
    nesting,
    forLoops: loops.length,
    invocationCalls: calls,
    errors,
  };
}

/** A streak of 1024 taps, each weighted by eight dependent transcendental operations. */
export const HEAVY_BODY = `// Stress effect: a streak of 1024 taps (the whole per-invocation loop budget) across 0.72 x 0.31
// of the picture, each tap weighted by a chain of eight dependent transcendental operations
// seeded by the tap's colour. About 1 KiB of WGSL: compiles quickly on every compiler.
fn effect(uv: vec2f, src: vec4f) -> vec4f {
    var acc = vec4f(0.0);
    var total = 0.0;
    var w = 0.5;
    for (var i = 0u; i < 1024u; i++) {
        let s = sample_src(fract(uv + vec2f(f32(i) * 0.0007, f32(i) * 0.0003)));
        w = sin(w * 1.37 + s.r);
        w = cos(w * 1.11 + s.g);
        w = exp(-abs(w) * 0.7);
        w = sqrt(w + 0.01);
        w = sin(w * 2.03 + s.b);
        w = exp(-abs(w));
        w = cos(w * 0.91 + s.a);
        w = sqrt(abs(w) + 0.001);
        acc += s * w;
        total += w;
    }
    return acc / max(total, 0.000001);
}
`;

/**
 * The heaviest single body the registry admits: one 1024-iteration loop whose block repeats a
 * pixel-local tap and a dependent transcendental until the body is `slack` bytes under the limit.
 * No user-function calls: in-loop calls would multiply the work but also the code to compile.
 */
export function maxBody(limits = registryLimits(), slack = 1268) {
  const target = limits.maxBodyBytes - slack;
  const head =
    "// Stress effect: one loop spending the whole iteration budget, its block filled close to the\n" +
    "// body limit with coherent taps and dependent transcendentals. Small canvases only on\n" +
    "// laptop GPUs.\n" +
    "fn effect(uv: vec2f, src: vec4f) -> vec4f {\n" +
    "    let px = 1.0 / resolution();\n" +
    "    var w = 0.5;\n" +
    "    var acc = vec4f(0.0);\n" +
    "    for (var i = 0u; i < 1024u; i++) {\n" +
    "        let o = vec2f(f32(i % 32u), f32(i / 32u)) * px;\n" +
    "        var t = src;\n";
  const tail =
    "        acc += vec4f(w, w, w, 1.0);\n" +
    "    }\n" +
    "    return vec4f(acc.rgb / 1024.0, src.a);\n" +
    "}\n";
  const functions = ["sin", "cos", "exp", "sqrt"];
  let units = "";
  for (let k = 0; ; k++) {
    const dx = ((k * 7) % 13) - 6;
    const dy = ((k * 5) % 11) - 5;
    const a = (1.1 + (k % 9) * 0.07).toFixed(2);
    const f = functions[k % 4];
    const argument = {
      sin: `w * ${a} + t.r`,
      cos: `w * ${a} + t.g`,
      exp: `-abs(w * ${a} + t.b)`,
      sqrt: `abs(w * ${a} + t.a) + 0.01`,
    }[f];
    // Every unit reads its own tap and depends on the previous one through w, so no sample is a
    // dead store and nothing can be hoisted.
    const unit = `        t = sample_src(uv + o + vec2f(${dx}.0, ${dy}.0) * px); w = ${f}(${argument});\n`;
    if (Buffer.byteLength(head + units + unit + tail) > target)
      return head + units + tail;
    units += unit;
  }
}

const float = (value) => ({
  mode: "constant",
  value: { type: "float", value },
});
const choice = (value) => ({
  mode: "constant",
  value: { type: "enum", value },
});
const color = (r, g, b, a) => ({
  mode: "constant",
  value: { type: "color", value: [r, g, b, a] },
});
const flag = (value) => ({ mode: "constant", value: { type: "bool", value } });

// The heaviest built-ins at their largest parameters; the host refuses any value out of range.
const WORST_EFFECTS = [
  ["lens-blur", { radius: float(0.02), highlights: float(10.0) }],
  ["oil-paint", { radius: float(0.015) }],
  ["surface-blur", { radius: float(0.02), threshold: float(1.0) }],
  ["matte-choker", { choke: float(-1.0), radius: float(0.03) }],
  ["outline", { color: color(1, 1, 1, 1), width: float(0.03) }],
  ["radial-blur", { mode: choice(1), amount: float(1.0), angle: float(90.0) }],
  ["gaussian-blur", { radius: float(1.0) }],
  [
    "glow",
    { threshold: float(0.0), intensity: float(3.0), radius: float(0.06) },
  ],
  [
    "drop-shadow",
    {
      color: color(0, 0, 0, 1),
      opacity: float(1.0),
      distance: float(0.2),
      softness: float(0.2),
      shadow_only: flag(false),
    },
  ],
  ["tilt-shift", { blur: float(0.2), focus: float(0.0), falloff: float(1.0) }],
  ["unsharp-mask", { amount: float(5.0), radius: float(0.1) }],
];
const WORST_TRANSITIONS = [
  ["zoom-blur", { strength: float(1.0) }],
  ["defocus-blur", { blur_size: float(0.2) }],
  ["linear-blur", { intensity: float(1.0) }],
  [
    "revolve",
    {
      max_rotation: float(360.0),
      swirl: float(720.0),
      peak_zoom: float(5.0),
      barrel: float(2.0),
      motion_blur: float(3.0),
    },
  ],
  ["whip-pan", { direction: choice(0), blur: float(1.0) }],
  ["film-burn", {}],
  ["fragment", {}],
  ["power-kaleido", { scale: float(10.0), z: float(10.0), speed: float(20.0) }],
];

function base(effects, width, height, id, name) {
  return {
    schema_version: 1,
    id,
    name,
    settings: { ...effects.settings, width, height },
  };
}

function heavyProject(effects, width, height, effectId, id) {
  return {
    ...base(
      effects,
      width,
      height,
      id,
      `${effectId} at ${width}x${height} (2 frames)`,
    ),
    sources: [effects.sources.find((source) => source.id === "video")],
    compositions: [
      {
        id: "heavy",
        nodes: [
          { id: "clip", op: { type: "clip" } },
          {
            id: "effect",
            op: {
              type: "effect",
              input: "clip",
              effect: { id: effectId, version: 1, params: {} },
            },
          },
        ],
        output: "effect",
      },
    ],
    timeline: {
      tracks: [
        {
          id: "v1",
          name: "V1",
          kind: "video",
          clips: [
            {
              id: "c",
              source_id: "video",
              timeline_range: { start: 0, end: 10000 },
              source_range: { start: 0, end: 10000 },
              composition: { id: "heavy" },
            },
          ],
        },
      ],
    },
  };
}

function builtinWorstProject(effects, width, height) {
  const project = base(
    effects,
    width,
    height,
    "builtin-worst",
    `heaviest built-ins at their largest parameters, ${width}x${height}`,
  );
  project.sources = effects.sources.filter((source) =>
    ["video", "still"].includes(source.id),
  );
  const compositions = [];
  const clips = [];
  const transitions = [];
  const segments = [];
  let t = 0;
  WORST_EFFECTS.forEach(([name, params], i) => {
    const id = `alpha2.effect.${name}`;
    compositions.push({
      id: `${name}-look`,
      nodes: [
        { id: "clip", op: { type: "clip" } },
        {
          id: "effect",
          op: {
            type: "effect",
            input: "clip",
            effect: { id, version: 1, params },
          },
        },
      ],
      output: "effect",
    });
    const from = (i % 4) * 20000;
    clips.push({
      id: `effect-${name}`,
      source_id: "video",
      timeline_range: { start: t, end: t + 10000 },
      source_range: { start: from, end: from + 10000 },
      composition: { id: `${name}-look` },
    });
    segments.push({ name, kind: "effect", id, start: t, end: t + 10000 });
    t += 10000;
  });
  // Transitions: a chain of 35000-tick clips overlapping by 15000 (three frames), video and still
  // alternating, after a two-frame gap that keeps the effects and the chain apart.
  t += 10000;
  for (let i = 0; i <= WORST_TRANSITIONS.length; i++) {
    const start = t + i * 20000;
    const source = i % 2 === 0 ? "video" : "still";
    const from = source === "video" && i % 4 === 2 ? 20000 : 0;
    clips.push({
      id: `chain-${i}`,
      source_id: source,
      timeline_range: { start, end: start + 35000 },
      source_range: { start: from, end: from + 35000 },
    });
  }
  WORST_TRANSITIONS.forEach(([name, params], i) => {
    const id = `alpha2.transition.${name}`;
    const start = t + (i + 1) * 20000;
    transitions.push({
      from_clip: `chain-${i}`,
      to_clip: `chain-${i + 1}`,
      timeline_range: { start, end: start + 15000 },
      easing: { type: "linear" },
      effect: Object.keys(params).length
        ? { id, version: 1, params }
        : { id, version: 1 },
    });
    segments.push({ name, kind: "transition", id, start, end: start + 15000 });
  });
  project.compositions = compositions;
  project.timeline = {
    tracks: [{ id: "base", name: "Base", kind: "video", clips, transitions }],
  };
  return { project, segments };
}

/** Structural problems of a generated project that would make the host refuse it. */
export function checkProject(project, effectIds, sourceSeconds) {
  const errors = [];
  const { width, height } = project.settings;
  if (width % 2 || height % 2) errors.push("odd export size");
  const compositions = new Map(
    (project.compositions ?? []).map((c) => [c.id, c]),
  );
  const sources = new Set(project.sources.map((source) => source.id));
  for (const composition of compositions.values()) {
    const ids = new Set();
    for (const node of composition.nodes) {
      if (node.op.input !== undefined && !ids.has(node.op.input))
        errors.push(
          `${composition.id}: input ${node.op.input} is not an earlier node`,
        );
      if (node.op.type === "effect" && !effectIds.has(node.op.effect.id))
        errors.push(`unknown effect ${node.op.effect.id}`);
      ids.add(node.id);
    }
    if (!ids.has(composition.output))
      errors.push(`${composition.id}: output missing`);
  }
  for (const track of project.timeline.tracks) {
    const clips = new Map(track.clips.map((clip) => [clip.id, clip]));
    for (const clip of track.clips) {
      const { timeline_range: placed, source_range: read } = clip;
      if (!sources.has(clip.source_id))
        errors.push(`clip ${clip.id}: source missing`);
      if (
        placed.end - placed.start !== read.end - read.start ||
        placed.end <= placed.start
      )
        errors.push(`clip ${clip.id}: ranges`);
      const seconds = sourceSeconds[clip.source_id];
      if (seconds !== undefined && read.end > seconds * TICKS_PER_SECOND)
        errors.push(`clip ${clip.id} reads past its source`);
      if (clip.composition && !compositions.has(clip.composition.id))
        errors.push(`clip ${clip.id}: composition missing`);
    }
    for (const transition of track.transitions ?? []) {
      const from = clips.get(transition.from_clip);
      const to = clips.get(transition.to_clip);
      const range = transition.timeline_range;
      if (!from || !to) errors.push("transition clip missing");
      else if (!(
        to.timeline_range.start <= range.start &&
        range.end <= from.timeline_range.end
      ))
        errors.push(
          `transition ${transition.from_clip}->${transition.to_clip} outside the overlap`,
        );
      if (!effectIds.has(transition.effect.id))
        errors.push(`unknown transition ${transition.effect.id}`);
    }
  }
  return errors;
}

/** Every output file name -> document, and the checks of every body and project. */
export function generate() {
  const effects = JSON.parse(
    readFileSync(
      join(root, "tests", "cases", "host-parity-effects-project.json"),
      "utf8",
    ),
  );
  const limits = registryLimits();
  const bodies = { "stress.heavy": HEAVY_BODY, "stress.max": maxBody(limits) };
  const effectIds = new Set(Object.keys(bodies));
  for (const composition of effects.compositions)
    for (const node of composition.nodes)
      if (node.op.type === "effect") effectIds.add(node.op.effect.id);
  for (const track of effects.timeline.tracks)
    for (const transition of track.transitions ?? [])
      effectIds.add(transition.effect.id);
  const request = (id) => [
    { id, version: 1, kind: "effect", wgslBody: bodies[id] },
  ];
  const outputs = {
    "heavy-effects.json": request("stress.heavy"),
    "max-effects.json": request("stress.max"),
  };
  let segments;
  for (const [width, height] of SIZES) {
    outputs[`heavy-${width}x${height}.json`] = heavyProject(
      effects,
      width,
      height,
      "stress.heavy",
      "stress-heavy",
    );
    outputs[`max-${width}x${height}.json`] = heavyProject(
      effects,
      width,
      height,
      "stress.max",
      "stress-max",
    );
  }
  for (const [width, height] of LARGE_SIZES) {
    const worst = builtinWorstProject(effects, width, height);
    segments = worst.segments;
    outputs[`builtin-worst-${width}x${height}.json`] = worst.project;
    outputs[`effects-${width}x${height}.json`] = {
      ...effects,
      settings: { ...effects.settings, width, height },
    };
  }
  outputs["builtin-worst.segments.json"] = {
    fps: FPS,
    ticks_per_frame: TICKS_PER_SECOND / FPS,
    segments,
  };
  // tests/corpus/parity-rgb.mov is one second long; the still has no end.
  const sourceSeconds = { video: 1 };
  const checks = {
    bodies: Object.fromEntries(
      Object.entries(bodies).map(([id, body]) => [id, checkBody(body, limits)]),
    ),
    projects: Object.fromEntries(
      Object.entries(outputs)
        .filter(([, document]) => document.timeline)
        .map(([name, document]) => [
          name,
          checkProject(document, effectIds, sourceSeconds),
        ]),
    ),
  };
  return { outputs, checks, limits };
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  const [directory] = process.argv.slice(2);
  if (!directory) {
    console.log("usage: stress-projects.mjs <output directory>");
    process.exit(2);
  }
  const { outputs, checks } = generate();
  mkdirSync(directory, { recursive: true });
  for (const [name, document] of Object.entries(outputs))
    writeFileSync(join(directory, name), JSON.stringify(document) + "\n");
  writeFileSync(
    join(directory, "check-report.json"),
    JSON.stringify(checks, null, 1) + "\n",
  );
  let failed = false;
  for (const [id, result] of Object.entries(checks.bodies)) {
    failed ||= result.errors.length > 0;
    console.log(
      `${result.errors.length ? "FAIL" : "ok"} ${id}: ${result.bytes} bytes, nesting ${result.nesting}, ${result.invocationCalls} bodies per invocation${result.errors.length ? `: ${result.errors.join("; ")}` : ""}`,
    );
  }
  for (const [name, errors] of Object.entries(checks.projects))
    if (errors.length) {
      failed = true;
      console.log(`FAIL ${name}: ${errors.join("; ")}`);
    }
  console.log(`wrote ${Object.keys(outputs).length} files to ${directory}`);
  process.exitCode = failed ? 1 : 0;
}

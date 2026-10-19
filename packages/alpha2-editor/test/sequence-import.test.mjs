// How numbered files become one image sequence: numbers past 2^53 and spans too wide to walk are
// refused before any walk.
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import * as runtime from "../dist/browser/runtime/extension-import.js";

const corpus = join(
  resolve(import.meta.dirname, "..", "..", ".."),
  "tests/corpus/extensions",
);
const png = (name) =>
  new File([readFileSync(join(corpus, "sequence_0007.png"))], name, {
    type: "image/png",
  });

test("a sequence's numbering is read from names, without walking it", () => {
  const { sequenceNumbering } = runtime;
  assert.equal(typeof sequenceNumbering, "function");
  assert.deepEqual(
    sequenceNumbering(["a_0003.png", "a_0001.png", "a_0007.png"]),
    {
      ok: true,
      first: 1,
      last: 7,
      frames: 7,
      missing: 4,
      gaps: [2, 4, 5, 6],
    },
  );
  // Nanosecond timestamps: past what a number holds exactly, so two names would be one frame.
  assert.match(
    sequenceNumbering(["1403636579763555584.png"]).message,
    /larger than 9007199254740991/,
  );
  assert.equal(
    sequenceNumbering(["f_9007199254740992.png", "f_9007199254740993.png"])
      .code,
    "sequence_numbering",
  );
  // A day of millisecond timestamps: 86,400,001 numbers, refused before any walk.
  const wide = sequenceNumbering([
    "cap_1705312200000.png",
    "cap_1705398600000.png",
  ]);
  assert.equal(wide.code, "sequence_range");
  assert.match(wide.message, /86400001 frames; a sequence has at most 100000/);
  assert.equal(
    sequenceNumbering(["a_1.png", "b_1.png"]).code,
    "sequence_numbering",
  );
  assert.equal(sequenceNumbering(["a.png"]).code, "sequence_numbering");
  assert.equal(sequenceNumbering([]).code, "sequence_empty");
  assert.equal(sequenceNumbering(["a_1.png", "a_100000.png"]).frames, 100_000);
});

test("staging refuses what the numbering refuses, whatever it does with gaps", async () => {
  const stage = (names, missing) =>
    runtime.stageSequence(names.map(png), {
      frameRate: { num: 10, den: 1 },
      missing,
      id: "s",
    });
  await assert.rejects(stage(["a_1.png", "a_200000.png"], "hold"), {
    code: "sequence_range",
  });
  await assert.rejects(stage(["a_7.png", "a_9.png", "a_12.png"], "reject"), {
    code: "sequence_gap",
    message: /Frames 8, 10, 11 are missing/,
  });
});

test("an SVG is probed whatever prolog comes before its root", () => {
  const { mayBeExtension } = runtime;
  const head = (text) => new TextEncoder().encode(text).subarray(0, 4096);
  const svg = '<svg xmlns="http://www.w3.org/2000/svg"/>';
  // Illustrator's doctype declares entities before the root; a comment may run past the head.
  const entities = Array.from(
    { length: 20 },
    (_, index) => `<!ENTITY ns_${index} "http://ns.adobe.com/${index}/">`,
  ).join("\n");
  for (const text of [
    svg,
    `<?xml version="1.0"?>\n<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "svg11.dtd" [\n${entities}\n]>\n${svg}`,
    `\ufeff<?xml version="1.0"?><!-- ${"x".repeat(8_192)} -->${svg}`,
  ])
    assert.equal(mayBeExtension(head(text)), true, text.slice(0, 60));
  assert.equal(mayBeExtension(head("plain text, not XML")), false);
});

/**
 * An OpenEXR header: the magic number and version, then each attribute as its name, its type, its
 * size (int32, `sizeOf` overrides it) and its value; a zero byte ends the list.
 */
function exrHeader(attributes) {
  const parts = [new Uint8Array([0x76, 0x2f, 0x31, 0x01, 0x02, 0, 0, 0])];
  const ascii = (text) => new TextEncoder().encode(`${text}\0`);
  for (const { name, type, value, size = value.byteLength } of attributes) {
    const length = new Uint8Array(4);
    new DataView(length.buffer).setInt32(0, size, true);
    parts.push(ascii(name), ascii(type), length, value);
  }
  parts.push(new Uint8Array([0]));
  return new Blob(parts);
}
const box2i = (...values) => {
  const bytes = new Uint8Array(16);
  values.forEach((value, index) =>
    new DataView(bytes.buffer).setInt32(index * 4, value, true),
  );
  return bytes;
};

/**
 * Stages `files` as a sequence in a worker thread, so a walk that never ends fails the test instead
 * of holding it: resolves to the error code staging threw (or "staged"), or "hung" after `ms`.
 */
async function stageInWorker(files, ms = 5_000) {
  const { Worker } = await import("node:worker_threads");
  const moduleUrl = new URL(
    "../dist/browser/runtime/extension-import.js",
    import.meta.url,
  ).href;
  const worker = new Worker(
    `const { parentPort, workerData } = require("node:worker_threads");
     import(workerData.moduleUrl).then(async ({ stageSequence }) => {
       const files = workerData.files.map(({ name, bytes }) => new File([bytes], name));
       try {
         await stageSequence(files, { frameRate: { num: 24, den: 1 }, openExr: async () => ({}), id: "s" });
         parentPort.postMessage("staged");
       } catch (error) {
         parentPort.postMessage(error.code ?? String(error));
       }
     });`,
    {
      eval: true,
      workerData: {
        moduleUrl,
        files: await Promise.all(
          files.map(async ({ name, blob }) => ({
            name,
            bytes: new Uint8Array(await blob.arrayBuffer()),
          })),
        ),
      },
    },
  );
  try {
    return await new Promise((resolve, reject) => {
      const timer = setTimeout(() => resolve("hung"), ms);
      worker.once("message", (message) => {
        clearTimeout(timer);
        resolve(message);
      });
      worker.once("error", reject);
    });
  } finally {
    await worker.terminate();
  }
}

test("an OpenEXR header whose attribute size points back ends the walk instead of repeating it", async () => {
  const window = {
    name: "displayWindow",
    type: "box2i",
    value: box2i(0, 0, 1919, 1079),
  };
  // A size of -8 sends the walk from the end of "b"'s size back to the start of "a" forever.
  const back = exrHeader([
    { name: "a", type: "b", value: new Uint8Array(), size: -8 },
    window,
  ]);
  const negative = exrHeader([
    { name: "comments", type: "string", value: new Uint8Array(4), size: -1 },
    window,
  ]);
  for (const blob of [back, negative])
    assert.equal(
      await stageInWorker([{ name: "plate_0001.exr", blob }]),
      "unsupported_media",
    );
  // A well-formed header is still read past the attributes before its display window: the frame's
  // size is found, and only the missing provider module stops staging after it.
  const valid = exrHeader([
    { name: "comments", type: "string", value: new Uint8Array(5) },
    window,
  ]);
  assert.notEqual(
    await stageInWorker([{ name: "plate_0001.exr", blob: valid }]),
    "hung",
  );
  assert.notEqual(
    await stageInWorker([{ name: "plate_0001.exr", blob: valid }]),
    "unsupported_media",
  );
});

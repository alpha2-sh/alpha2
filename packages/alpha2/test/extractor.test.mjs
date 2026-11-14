import assert from "node:assert/strict";
import test from "node:test";

import { collectEmits } from "../dist/extractor.js";

test("emits the wire can carry are kept in the protocol's form", () => {
  const { emits, out } = collectEmits();
  out.sample("level", 4_000, [0.5, null]);
  out.word("words", 0, 4_000, "hello", "a");
  out.interval("scenes", 0, 8_000, "cut", 0.75);
  out.interval("scenes", 8_000, 9_000);
  assert.deepEqual(emits, [
    { output: "level", time: 4_000, values: [0.5, null] },
    { output: "words", start: 0, end: 4_000, text: "hello", speaker: "a" },
    { output: "scenes", start: 0, end: 8_000, label: "cut", score: 0.75 },
    { output: "scenes", start: 8_000, end: 9_000 },
  ]);
});

test("an emit the wire cannot carry throws, naming it, and is not kept", () => {
  const { emits, out } = collectEmits();
  const refusals = [
    [() => out.sample("level", 12n, [1]), /sample "level": time is a bigint/],
    [() => out.sample("level", 1.5, [1]), /time must be an integer tick count/],
    [
      () => out.sample("level", 0, [Number.NaN]),
      /values\[0\] must be a finite number/,
    ],
    [
      () => out.sample("level", 0, [1, Infinity]),
      /values\[1\] must be a finite number/,
    ],
    [() => out.sample(7, 0, [1]), /an emit's output must be a string/],
    [
      () => out.word("words", 0, 4_000, 5),
      /word "words": text must be a string/,
    ],
    [
      () => out.interval("scenes", 0, 1, "cut", -Infinity),
      /score must be a finite number/,
    ],
    [() => out.interval("scenes", 0, 1n), /end is a bigint/],
  ];
  for (const [emit, message] of refusals)
    assert.throws(
      emit,
      (error) => error instanceof TypeError && message.test(error.message),
    );
  assert.deepEqual(emits, []);
});

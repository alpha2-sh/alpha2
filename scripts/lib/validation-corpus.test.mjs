import test from "node:test";
import assert from "node:assert/strict";

import { expandRepeats } from "./validation-corpus.mjs";

test("a repeat writes out its item with the index in strings and counters", () => {
  assert.deepEqual(
    expandRepeats([
      { id: "first" },
      {
        $repeat: 3,
        item: {
          id: "c{i}",
          range: { start: { $index: { start: 10, step: 5 } }, end: 99 },
        },
      },
    ]),
    [
      { id: "first" },
      { id: "c0", range: { start: 10, end: 99 } },
      { id: "c1", range: { start: 15, end: 99 } },
      { id: "c2", range: { start: 20, end: 99 } },
    ],
  );
});

test("a nested repeat counts its own index", () => {
  assert.deepEqual(
    expandRepeats({
      tracks: [
        {
          $repeat: 2,
          item: {
            id: "t{i}",
            effects: [{ $repeat: 2, item: { id: "e{i}" } }],
          },
        },
      ],
    }),
    {
      tracks: [
        { id: "t0", effects: [{ id: "e0" }, { id: "e1" }] },
        { id: "t1", effects: [{ id: "e0" }, { id: "e1" }] },
      ],
    },
  );
});

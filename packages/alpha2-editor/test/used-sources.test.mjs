import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { shownSources } from "../dist/index.js";

const corpus = JSON.parse(
  await readFile(
    new URL("../../../tests/cases/used-sources-corpus.json", import.meta.url),
    "utf8",
  ),
);

// The engine's `Project::used_sources` runs the same corpus: an editor shows what a render reads.
for (const { name, project, used } of corpus)
  test(`shown sources: ${name}`, () => {
    assert.deepEqual([...shownSources(project)].sort(), used);
  });

test("the effect resources a render reads are those what it draws names", async () => {
  const { drawnEffectResources } = await import("../dist/index.js");
  const lut = (id) => ({
    id: "alpha2.effect.lut-3d",
    version: 1,
    resources: { lut: id },
  });
  const project = {
    ...corpus[0].project,
    sources: [{ id: "v", kind: "video", locator: "v.mp4" }],
    effect_resources: [
      "drawn",
      "bypassed",
      "elsewhere",
      "crossfade",
      "unused",
    ].map((id) => ({ id, kind: "lut3d", locator: `${id}.cube` })),
    compositions: [
      {
        id: "look",
        nodes: [
          { id: "clip", op: { type: "clip" } },
          {
            id: "graded",
            op: { type: "effect", input: "clip", effect: lut("drawn") },
          },
          {
            id: "off",
            bypass: true,
            op: { type: "effect", input: "graded", effect: lut("bypassed") },
          },
        ],
        output: "off",
      },
      // No clip shows it.
      {
        id: "spare",
        nodes: [
          { id: "clip", op: { type: "clip" } },
          {
            id: "graded",
            op: { type: "effect", input: "clip", effect: lut("elsewhere") },
          },
        ],
        output: "graded",
      },
    ],
    timeline: {
      tracks: [
        {
          id: "t1",
          name: "Video",
          kind: "video",
          clips: [
            {
              id: "c1",
              source_id: "v",
              timeline_range: { start: 0, end: 120000 },
              source_range: { start: 0, end: 120000 },
              composition: { id: "look" },
            },
          ],
          transitions: [{ effect: lut("crossfade") }],
        },
      ],
    },
  };
  assert.deepEqual([...drawnEffectResources(project)].sort(), [
    "crossfade",
    "drawn",
  ]);
});

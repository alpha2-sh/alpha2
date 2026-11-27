import assert from "node:assert/strict";
import test from "node:test";

import { runExtractor } from "../dist/extract.js";

const DESCRIPTOR = {
  id: "alpha2.test.scripted",
  version: 1,
  input: { media: "video", from: ["timeline"], format: "luma8" },
  params_schema: { type: "object" },
  outputs: [{ name: "level", kind: "scalars", channels: ["a"] }],
};
const REQUEST = {
  extractor: DESCRIPTOR.id,
  version: 1,
  input: { from: "timeline", range: { start: 0, end: 8_000 } },
  outputs: {},
};
const STEPS = [
  { step: "pass", pass: 0, direction: "forward" },
  { step: "video", time: 0, width: 1, height: 1, format: "luma8" },
  { step: "video", time: 4_000, width: 1, height: 1, format: "luma8" },
];

const report = (stage, completed) => ({
  stage,
  pass: 0,
  completed,
  total: 2,
  covered: stage === "end" ? 8_000 : Math.max(0, completed - 1) * 4_000,
  span: 8_000,
});
const heard = (stage, completed) => ({
  phase: "extract",
  ...report(stage, completed),
});

function scripted() {
  const asked = { progress: 0 };
  class ExtractSession {
    #taken = 0;
    #delivered = 0;
    #stage = "begin";
    context() {
      return "{}";
    }
    next() {
      const step = STEPS[this.#taken];
      this.#taken += 1;
      this.#stage = step === undefined ? "end" : "pass";
      if (step?.step === "video") this.#delivered += 1;
      return JSON.stringify(step ?? { step: "done" });
    }
    videoBytes() {
      return new Uint8Array(1);
    }
    emit() {}
    progress() {
      asked.progress += 1;
      return JSON.stringify(report(this.#stage, this.#delivered));
    }
    finish() {
      return "[]";
    }
    free() {}
  }
  return { binding: { ExtractSession }, asked };
}

function logging(log) {
  return {
    describe: () => DESCRIPTOR,
    begin: () => void log.push("begin"),
    pass: ({ pass }) => void log.push(`pass ${pass}`),
    video: (frame) => void log.push(`video ${frame.time}`),
    end: () => void log.push("end"),
  };
}

test("a run reports before begin, as a pass begins, after each delivery and before end", async () => {
  const { binding } = scripted();
  const log = [];
  await runExtractor(
    {
      binding,
      project: {},
      request: REQUEST,
      onProgress: (progress) => log.push(progress),
    },
    logging(log),
  );
  assert.deepEqual(log, [
    heard("begin", 0),
    "begin",
    heard("pass", 0),
    "pass 0",
    "video 0",
    heard("pass", 1),
    "video 4000",
    heard("pass", 2),
    heard("end", 2),
    "end",
  ]);
});

test("a run nobody listens to does not ask the session for its progress", async () => {
  const { binding, asked } = scripted();
  await runExtractor({ binding, project: {}, request: REQUEST }, logging([]));
  assert.equal(asked.progress, 0);
});

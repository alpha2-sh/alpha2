import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";
import test from "node:test";

import {
  checkTestOutput,
  checkValidationLog,
  DECODE_KNOWN,
  findValidationLayer,
  KNOWN,
  TEST_KNOWN,
} from "./vulkan-validation-log.mjs";

// The layer's own format (1.4.357): a header, then the message, the spec text and a blank line.
function message(id, body, level = "Error") {
  return `Validation ${level}: [ ${id} ] | MessageID = 0x4872eaa0\n${body}\nThe Vulkan spec states: ...\n\n`;
}

const views = (count) =>
  Array.from(
    { length: count },
    (_, index) => `VkImageView 0x2da0000000${index}`,
  ).join(", ");

function leak(count, objects = views(count)) {
  return message(
    "VUID-vkDestroyDevice-device-05137",
    `vkDestroyDevice(): VkDevice 0x5c7c73124bb0 has ${count} leaked objects that have not been destroyed.\n${objects}.`,
  );
}

const known =
  message(
    "VUID-StandaloneSpirv-None-10684",
    "vkCreateShaderModule(): pCreateInfo->pCode (spirv-val produced an error):",
  ) +
  message(
    "VUID-vkCmdDecodeVideoKHR-pDecodeInfo-07142",
    "vkCmdDecodeVideoKHR(): pDecodeInfo->dstPictureResource.imageViewBinding ...",
  );

const problems = (text) => checkValidationLog(text, KNOWN).problems;

test("a log of known messages passes", () => {
  const { problems: found, counts } = checkValidationLog(known, KNOWN);
  assert.deepEqual(found, []);
  assert.equal(counts.get("VUID-vkCmdDecodeVideoKHR-pDecodeInfo-07142"), 1);
});

test("a run that wrote no log fails: the layer did not load", () => {
  assert.match(problems(null)[0], /no log/);
});

test("an empty log passes: the layer loaded and reported nothing", () => {
  assert.deepEqual(problems(""), []);
});

test("an ID outside the VUID and SYNC-HAZARD prefixes fails", () => {
  assert.deepEqual(
    problems(
      known +
        message(
          "UNASSIGNED-CoreValidation-DrawState-InvalidImageLayout",
          "...",
        ),
    ),
    ["1 UNASSIGNED-CoreValidation-DrawState-InvalidImageLayout"],
  );
});

test("an unknown ID fails at any level", () => {
  assert.deepEqual(
    problems(
      message("VUID-vkQueueSubmit-pCommandBuffers-00070", "...", "Warning"),
    ),
    ["1 VUID-vkQueueSubmit-pCommandBuffers-00070"],
  );
  assert.deepEqual(problems(message("SYNC-HAZARD-WRITE-AFTER-WRITE", "...")), [
    "1 SYNC-HAZARD-WRITE-AFTER-WRITE",
  ]);
});

test("a header without an ID fails", () => {
  assert.match(
    problems("Validation Error: something new\n")[0],
    /without an ID/,
  );
});

test("any object alive at vkDestroyDevice fails, an export's or a decode's", () => {
  assert.deepEqual(problems(known + leak(6)), [
    "1 VUID-vkDestroyDevice-device-05137",
  ]);
  assert.deepEqual(problems(leak(1)), ["1 VUID-vkDestroyDevice-device-05137"]);
  assert.deepEqual(checkValidationLog(known + leak(6), DECODE_KNOWN).problems, [
    "1 VUID-vkDestroyDevice-device-05137",
  ]);
});

test("a run that only decodes does not know the encoder's checks", () => {
  const decodes = (text) => checkValidationLog(text, DECODE_KNOWN).problems;
  assert.deepEqual(decodes(known), []);
  assert.deepEqual(
    decodes(message("VUID-vkCmdEncodeVideoKHR-pEncodeInfo-08206", "...")),
    ["1 VUID-vkCmdEncodeVideoKHR-pEncodeInfo-08206"],
  );
});

// A test binary's stdout behind the layer: libtest's lines and the layer's messages interleaved, a
// message often starting on the `test <name> ... ` line of the test that caused it.
const enabled =
  "Validation Information: [ CURRENT-VALIDATION-ENABLED ] | MessageID = 0x2a3b1d8e\nvkCreateInstance(): Current Validaiton Enabled:\n  - Core Checks\n  - Synchronization\n\n";

test("a test binary's output with the layer's status and known messages passes", () => {
  const output = `running 2 tests\ntest a::first ... ${enabled}ok\ntest a::second ... ${message("VUID-StandaloneSpirv-None-10684", "vkCreateShaderModule(): ...")}ok\n\ntest result: ok. 2 passed\n`;
  const {
    problems: found,
    counts,
    layerLoaded,
  } = checkTestOutput(output, TEST_KNOWN);
  assert.deepEqual(found, []);
  assert.equal(layerLoaded, true);
  assert.equal(counts.get("VUID-StandaloneSpirv-None-10684"), 1);
});

test("an unknown message on a test's own line fails and names that test", () => {
  const hazard = message("SYNC-HAZARD-READ-AFTER-WRITE", "vkCmdDraw(): ...");
  const output = `${enabled}test a::first ... ok\ntest a::second ... ${hazard}ok\ntest a::third ... ${hazard}ok\n`;
  assert.deepEqual(checkTestOutput(output, TEST_KNOWN).problems, [
    "2 SYNC-HAZARD-READ-AFTER-WRITE (in a::second, a::third)",
  ]);
});

test("the export run's FFmpeg messages are not known to the test suites", () => {
  const output = `${enabled}test a::decode ... ${message("VUID-vkCmdDecodeVideoKHR-pDecodeInfo-07142", "...")}ok\n`;
  assert.deepEqual(checkTestOutput(output, TEST_KNOWN).problems, [
    "1 VUID-vkCmdDecodeVideoKHR-pDecodeInfo-07142 (in a::decode)",
  ]);
});

test("output without the layer's status report did not load the layer", () => {
  const { problems: found, layerLoaded } = checkTestOutput(
    "running 1 test\ntest a::first ... ok\n",
    TEST_KNOWN,
  );
  assert.deepEqual(found, []);
  assert.equal(layerLoaded, false);
});

test("a layer path without a 1.4 manifest is refused, with the reason", () => {
  const directory = mkdtempSync(join(tmpdir(), "alpha2-layer-"));
  try {
    assert.match(findValidationLayer(undefined).problem, /VK_LAYER_PATH/);
    assert.match(findValidationLayer(directory).problem, /VK_LAYER_PATH/);
    const manifest = join(directory, "VkLayer_khronos_validation.json");
    writeFileSync(
      manifest,
      JSON.stringify({ layer: { api_version: "1.3.204" } }),
    );
    assert.match(
      findValidationLayer(`/nowhere${delimiter}${directory}`).problem,
      /1\.3\.204; 1\.4/,
    );
    writeFileSync(
      manifest,
      JSON.stringify({ layer: { api_version: "1.4.357" } }),
    );
    assert.deepEqual(findValidationLayer(`/nowhere${delimiter}${directory}`), {
      manifest,
    });
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

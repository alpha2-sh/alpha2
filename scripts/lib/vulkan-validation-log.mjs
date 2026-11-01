// Judges a Khronos validation layer log against known messages; separate so it is testable without
// a GPU.

import { existsSync, readFileSync } from "node:fs";
import { delimiter, join } from "node:path";

const HEADER = /^Validation [A-Za-z ]+:/;
const HEADER_ID = /^Validation ([A-Za-z ]+): \[ (\S+) \]/;
const HEADER_IN_LINE =
  /(?<=[^\n])(?=Validation (?:Error|Warning|Performance Warning|Information|Verbose): \[ )/g;
const TEST_START = /^test (\S+) \.\.\. ?/;

/**
 * Layers older than 1.4 crash FFmpeg's video sessions.
 *
 * @param {string | undefined} layerPath
 * @returns {{ manifest: string, problem?: undefined } | { manifest?: undefined, problem: string }}
 */
export function findValidationLayer(layerPath) {
  const manifest = (layerPath ?? "")
    .split(delimiter)
    .filter(Boolean)
    .map((directory) => join(directory, "VkLayer_khronos_validation.json"))
    .find((path) => existsSync(path));
  if (manifest === undefined)
    return {
      problem:
        "no Khronos validation layer: set VK_LAYER_PATH to a 1.4 layer's explicit_layer.d (scripts/fetch-vulkan-validation-layer.sh prints one)",
    };
  const apiVersion = JSON.parse(readFileSync(manifest, "utf8")).layer
    ?.api_version;
  const [major, minor] = String(apiVersion).split(".").map(Number);
  if (!(major > 1 || (major === 1 && minor >= 4)))
    return {
      problem: `the validation layer at ${manifest} is ${apiVersion}; 1.4 or newer is required`,
    };
  return { manifest };
}

/**
 * @param {string} text
 * @returns {{ level: string, id: string | null, header: string, body: string }[]}
 */
export function parseValidationLog(text) {
  const messages = [];
  for (const line of text.split("\n")) {
    if (HEADER.test(line)) {
      const parsed = HEADER_ID.exec(line);
      messages.push({
        level: parsed?.[1] ?? line.slice(11, line.indexOf(":")),
        id: parsed?.[2] ?? null,
        header: line,
        body: "",
      });
    } else if (messages.length > 0) {
      messages[messages.length - 1].body += `${line}\n`;
    }
  }
  return messages;
}

/**
 * @typedef {object} KnownMessage
 * @property {string} source
 * @property {number} [max]
 * @property {(body: string) => string | null} [check] Null when this occurrence is the known one.
 */

/**
 * `text` is null when the run wrote no log, meaning the layer never loaded.
 *
 * @param {string | null} text
 * @param {Map<string, KnownMessage>} known
 * @returns {{ problems: string[], counts: Map<string, number> }}
 */
export function checkValidationLog(text, known) {
  if (text === null)
    return {
      problems: [
        "the layer wrote no log, so it did not load or did not log to its own file",
      ],
      counts: new Map(),
    };
  const problems = [];
  const counts = new Map();
  for (const message of parseValidationLog(text)) {
    if (message.id === null) {
      problems.push(`a message without an ID: ${message.header}`);
      continue;
    }
    counts.set(message.id, (counts.get(message.id) ?? 0) + 1);
    const entry = known.get(message.id);
    const reason = entry?.check?.(message.body);
    if (reason) problems.push(`${message.id}: ${reason}`);
  }
  for (const [id, count] of counts) {
    const entry = known.get(id);
    if (entry === undefined) problems.push(`${count} ${id}`);
    else if (entry.max !== undefined && count > entry.max)
      problems.push(`${count} ${id}, more than the ${entry.max} known`);
  }
  return { problems, counts };
}

// Known messages for FFmpeg 8.1.3 on NVIDIA driver 580; any other ID fails the run.
const NAGA_LAYOUT = "VUID-StandaloneSpirv-None-10684";

/** @type {Map<string, KnownMessage>} */
export const KNOWN = new Map([
  // FFmpeg creates its frames VIDEO_PROFILE_INDEPENDENT with a format the layer does not match
  // against the session's profile.
  [
    "VUID-VkVideoBeginCodingInfoKHR-pPictureResource-07240",
    { source: "FFmpeg" },
  ],
  ["VUID-vkCmdDecodeVideoKHR-pDecodeInfo-07142", { source: "FFmpeg" }],
  // FFmpeg 8.1 binds a VIDEO_DECODE_DST-only view as a reference slot in coincident-DPB mode
  // (fixed upstream in 25e187f84949, after 8.1.3).
  [
    "VUID-VkVideoBeginCodingInfoKHR-slotIndex-07245",
    { source: "FFmpeg (coincident DPB view usage)" },
  ],
  ["VUID-vkCmdEncodeVideoKHR-pEncodeInfo-08206", { source: "FFmpeg" }],
  ["VUID-VkImageCreateInfo-pNext-06811", { source: "FFmpeg (encode DPB)" }],
  // A view of a multi-planar NV12 image with STORAGE usage inherited from the image, which the
  // format does not support as a whole.
  ["VUID-VkImageViewCreateInfo-usage-02275", { source: "FFmpeg" }],
  // naga emits ArrayStride on Function-storage arrays and Offset on Private-storage structs.
  [NAGA_LAYOUT, { source: "naga" }],
]);

// The pinned SDK fixes the `VUID-vkDestroyDevice-device-05137` teardown leak, so no run allows it.
const ENCODE_ONLY = new Set(["VUID-vkCmdEncodeVideoKHR-pEncodeInfo-08206"]);

/** @type {Map<string, KnownMessage>} */
export const DECODE_KNOWN = new Map(
  [...KNOWN].filter(([id]) => !ENCODE_ONLY.has(id)),
);

// Printed at each vkCreateInstance with `info` flags; proves the layer loaded in every binary.
export const LAYER_ENABLED = "CURRENT-VALIDATION-ENABLED";

// Known messages for the render and engine suites on NVIDIA 580, RADV and lavapipe.
/** @type {Map<string, KnownMessage>} */
export const TEST_KNOWN = new Map([
  [NAGA_LAYOUT, KNOWN.get(NAGA_LAYOUT)],
  [LAYER_ENABLED, { source: "the layer, at instance creation" }],
]);

/**
 * A layer message can start on libtest's `test <name> ... ` line, before the result is printed.
 *
 * @param {string} text Stdout and stderr in write order.
 * @param {Map<string, KnownMessage>} known
 * @returns {{ problems: string[], counts: Map<string, number>, layerLoaded: boolean }}
 */
export function checkTestOutput(text, known) {
  const lines = text.replace(HEADER_IN_LINE, "\n").split("\n");
  const tests = new Map();
  let current = "(before the first test)";
  for (const line of lines) {
    const started = TEST_START.exec(line);
    if (started) current = started[1];
    const id = HEADER_ID.exec(line)?.[2];
    if (id === undefined) continue;
    if (!tests.has(id)) tests.set(id, new Set());
    tests.get(id).add(current);
  }
  const { problems, counts } = checkValidationLog(lines.join("\n"), known);
  const named = problems.map((problem) => {
    const id = /^(?:\d+ )?([^\s,:]+)/.exec(problem)?.[1];
    const where = tests.get(id);
    if (where === undefined) return problem;
    const list = [...where];
    return `${problem} (in ${list.slice(0, 3).join(", ")}${list.length > 3 ? `, and ${list.length - 3} more` : ""})`;
  });
  return { problems: named, counts, layerLoaded: counts.has(LAYER_ENABLED) };
}

// Judges a Khronos validation layer log against the messages a run is known to produce, so the
// decision can be tested without a GPU. A run passes only when it wrote a log, every header
// parses, every ID is known, and each known message stays within its entry's limits.

import { existsSync, readFileSync } from "node:fs";
import { delimiter, join } from "node:path";

const HEADER = /^Validation [A-Za-z ]+:/;
const HEADER_ID = /^Validation ([A-Za-z ]+): \[ (\S+) \]/;
const HEADER_IN_LINE =
  /(?<=[^\n])(?=Validation (?:Error|Warning|Performance Warning|Information|Verbose): \[ )/g;
// A libtest line that names the test it starts: `test <name> ... ` (the result follows later).
const TEST_START = /^test (\S+) \.\.\. ?/;

/**
 * The Khronos validation layer manifest in `layerPath` (VK_LAYER_PATH: `;`-separated on Windows,
 * `:` elsewhere), or why there is none a run can use. Layers older than 1.4 crash FFmpeg's video
 * sessions.
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
 * The messages in `text`, each with the level and ID from its header and the lines up to the next
 * header. A header without a bracketed ID gets the ID `null`.
 *
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
 * @property {string} source Who causes it.
 * @property {number} [max] How many times one log may contain it.
 * @property {(body: string) => string | null} [check] Why this occurrence is not the known one, or
 *   null when it is.
 */

/**
 * Judges a layer log. `text` is null when the run wrote no log, which means the layer never loaded.
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

// Known messages for FFmpeg 8.1.3 on NVIDIA driver 580. Any other message ID fails the run,
// whatever its prefix or level; remove entries when their causes are fixed.
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
  // FFmpeg 8.1 gives the current picture's view only VIDEO_DECODE_DST in coincident-DPB mode, then
  // binds it as a reference slot. Fixed upstream by 25e187f84949, which 8.1.3 does not have.
  [
    "VUID-VkVideoBeginCodingInfoKHR-slotIndex-07245",
    { source: "FFmpeg (coincident DPB view usage)" },
  ],
  ["VUID-vkCmdEncodeVideoKHR-pEncodeInfo-08206", { source: "FFmpeg" }],
  ["VUID-VkImageCreateInfo-pNext-06811", { source: "FFmpeg (encode DPB)" }],
  // A view of a multi-planar NV12 image with STORAGE usage inherited from the image, which the
  // format does not support as a whole.
  ["VUID-VkImageViewCreateInfo-usage-02275", { source: "FFmpeg" }],
  // naga emits explicit layout decorations outside the storage classes that allow them: ArrayStride
  // on a Function-storage array and Offset on a Private-storage struct.
  [NAGA_LAYOUT, { source: "naga" }],
]);

// A decode-only run (`probe-hwdecode`) does not know encoder messages. No run knows a teardown
// leak (`VUID-vkDestroyDevice-device-05137`): the pinned SDK carries the upstream fix.
const ENCODE_ONLY = new Set(["VUID-vkCmdEncodeVideoKHR-pEncodeInfo-08206"]);

/** @type {Map<string, KnownMessage>} */
export const DECODE_KNOWN = new Map(
  [...KNOWN].filter(([id]) => !ENCODE_ONLY.has(id)),
);

// Printed at each vkCreateInstance when the report flags include `info`; a GPU test run uses it
// to know the layer loaded in every test binary.
export const LAYER_ENABLED = "CURRENT-VALIDATION-ENABLED";

// Known messages for the render and engine test suites (`test-gpu-validation.mjs`), on every
// Vulkan driver here (NVIDIA 580, RADV and lavapipe). Any other message ID fails the run.
/** @type {Map<string, KnownMessage>} */
export const TEST_KNOWN = new Map([
  [NAGA_LAYOUT, KNOWN.get(NAGA_LAYOUT)],
  [LAYER_ENABLED, { source: "the layer, at instance creation" }],
]);

/**
 * Judges what a test binary printed while the layer logged to its stdout. libtest prints
 * `test <name> ... ` before a test runs and its result after, so a layer message can start on that
 * line; each problem names the tests whose messages it counts.
 *
 * @param {string} text The binary's stdout and stderr, in the order written.
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

// The wasm-browser case loop: runs each case's page and ends the run early when the browser itself
// is the failure, so a browser that cannot start costs one start timeout, not one per case.

import { BrowserStartError } from "../browser-errors.mjs";

/** What a page that never reported says when its time runs out. */
const NEVER_REPORTED = /browser page timed out at pending/;

/**
 * Runs `names` in order through `runPage(name)`, which resolves to the case's decoded result or
 * rejects. `report(name, result)` prints and counts each case; a rejection is reported as
 * `{ error }`.
 *
 * Stops after a `BrowserStartError`, and when two cases in a row time out before any page of the
 * run has reported (a browser that starts but never runs a page, as Chrome under Xvfb on a Mesa
 * Vulkan ICD does). Returns the lines to print for the stop, empty when every case ran.
 *
 * @param {string[]} names
 * @param {(name: string) => Promise<object>} runPage
 * @param {(name: string, result: object) => void} report
 * @returns {Promise<string[]>}
 */
export async function runCases(names, runPage, report) {
  let reported = false;
  let silentTimeouts = 0;
  for (const [index, name] of names.entries()) {
    const remaining = names.length - index - 1;
    let result;
    try {
      result = await runPage(name);
      reported = true;
      silentTimeouts = 0;
    } catch (error) {
      if (error instanceof BrowserStartError) {
        report(name, {
          error: `browser could not start: ${error.message}`,
        });
        return remaining > 0
          ? [
              `FAIL ${remaining} remaining case(s) not run: the browser could not start`,
            ]
          : [];
      }
      result = { error: error.message };
      silentTimeouts =
        !reported && NEVER_REPORTED.test(error.message)
          ? silentTimeouts + 1
          : 0;
    }
    report(name, result);
    if (silentTimeouts >= 2)
      return [
        "FAIL the browser never ran a page (two cases timed out before any reported); on Linux this is the Xvfb + Mesa Vulkan hang: use a DRI3 display",
        ...(remaining > 0
          ? [`FAIL ${remaining} remaining case(s) not run`]
          : []),
      ];
  }
  return [];
}

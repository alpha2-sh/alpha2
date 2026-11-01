// Chrome or Edge through DevTools, Safari or Firefox through WebDriver, with one page contract.

import {
  browserForCheck,
  edgeExecutable,
  runChromePage,
} from "./chrome-cdp.mjs";
import {
  runWebDriverPage,
  webDriverBrowser,
  WEBDRIVER_BROWSERS,
} from "./webdriver.mjs";

export const PAGE_BROWSERS = Object.freeze([
  "chrome",
  "edge",
  ...WEBDRIVER_BROWSERS,
]);

/**
 * `--browser <name>`, else ALPHA2_BROWSER, else Chrome. Null when Chrome is missing and not
 * required; a browser asked for by name always throws when missing.
 *
 * @returns {{ kind: "chrome", name: "chrome" | "edge", command: string } |
 *   { kind: "webdriver", name: string, webdriver: ReturnType<typeof webDriverBrowser> } | null}
 */
export function browserForRun({
  argv = process.argv,
  env = process.env,
  platform,
  exists,
  required = false,
} = {}) {
  const name = requestedBrowser(argv, env);
  if (name === "chrome") {
    const command = browserForCheck({
      env: required
        ? { ...env, ALPHA2_REQUIRE: `${env.ALPHA2_REQUIRE ?? ""},browser` }
        : env,
      platform,
      exists,
    });
    return command === null ? null : { kind: "chrome", name, command };
  }
  if (name === "edge") {
    const command = edgeExecutable({ env, platform, exists });
    if (command === null)
      throw new Error("no Edge found: set ALPHA2_EDGE to an Edge executable");
    return { kind: "chrome", name, command };
  }
  return {
    kind: "webdriver",
    name,
    webdriver: webDriverBrowser(name, { env, platform, exists }),
  };
}

function requestedBrowser(argv, env) {
  let name = env.ALPHA2_BROWSER || "chrome";
  for (const [index, arg] of argv.entries()) {
    if (arg === "--browser") name = argv[index + 1] ?? "";
    else if (arg.startsWith("--browser="))
      name = arg.slice("--browser=".length);
  }
  if (!PAGE_BROWSERS.includes(name))
    throw new Error(
      `unknown browser ${JSON.stringify(name)}: --browser takes ${PAGE_BROWSERS.join(", ")}`,
    );
  return name;
}

/**
 * Resolves to the page's title. `onPage` and `onGpuProcess` apply to Chrome only.
 *
 * @param {NonNullable<ReturnType<typeof browserForRun>>} browser
 * @param {string} url
 * @param {{ timeoutMs?: number, onBrowser?: (version: object) => void, onPage?: Function,
 *   onGpuProcess?: Function, domainBlocking3d?: boolean }} [options]
 * @returns {Promise<string>}
 */
export function runBrowserPage(
  browser,
  url,
  { timeoutMs, onBrowser, onPage, onGpuProcess, domainBlocking3d } = {},
) {
  return browser.kind === "chrome"
    ? runChromePage(browser.command, url, {
        timeoutMs,
        onBrowser,
        onPage,
        onGpuProcess,
        domainBlocking3d,
      })
    : runWebDriverPage(browser.webdriver, url, { timeoutMs, onBrowser });
}

// Which browser a page check opens its pages in: Chrome (the default) or Edge through DevTools, or
// Safari or Firefox through WebDriver. Both runners share one page contract.

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

/** The browsers `--browser` takes. */
export const PAGE_BROWSERS = Object.freeze([
  "chrome",
  "edge",
  ...WEBDRIVER_BROWSERS,
]);

/**
 * The browser a run asked for: `--browser <name>` in `argv`, else ALPHA2_BROWSER, else Chrome. Null
 * means this machine has no Chrome and the check skips its browser half, unless `required` or
 * ALPHA2_REQUIRE naming `browser` makes it throw. A missing Edge, or a missing Safari or Firefox
 * driver, always throws: a browser asked for by name must run.
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

/** The browser named in `argv` or the environment, checked against `PAGE_BROWSERS`. */
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
 * Opens `url` in `browser` and waits for the page to report; resolves to the URI-encoded JSON title
 * it set. `timeoutMs` is how long the page has.
 *
 * `onBrowser` receives the browser's version record: Chrome's `Browser.getVersion`, or the
 * WebDriver session's capabilities. `onPage` drives a Chrome page through DevTools while it runs
 * (see `runChromePage`), and `onGpuProcess` receives whether Chrome's GPU process restarted while
 * it ran; other browsers ignore both. Rejects with a `BrowserStartError` when the browser cannot
 * start.
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

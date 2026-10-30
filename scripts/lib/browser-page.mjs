// Which browser a page check opens its pages in: Chrome through DevTools (the default), or Safari
// or Firefox through WebDriver. Both runners share one page contract.

import { browserForCheck, runChromePage } from "./chrome-cdp.mjs";
import {
  runWebDriverPage,
  webDriverBrowser,
  WEBDRIVER_BROWSERS,
} from "./webdriver.mjs";

/** The browsers `--browser` takes. */
export const PAGE_BROWSERS = Object.freeze(["chrome", ...WEBDRIVER_BROWSERS]);

/**
 * The browser a run asked for: `--browser <name>` in `argv`, else ALPHA2_BROWSER, else Chrome. Null
 * means this machine has no Chrome and the check skips its browser half, unless `required` (or
 * `--require-browser`, ALPHA2_REQUIRE_BROWSER=1) makes it throw. A missing Safari or Firefox
 * driver always throws.
 *
 * @returns {{ kind: "chrome", name: "chrome", command: string } |
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
      argv: required ? [...argv, "--require-browser"] : argv,
      env,
      platform,
      exists,
    });
    return command === null ? null : { kind: "chrome", name, command };
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
 * WebDriver session's capabilities. Rejects with a `BrowserStartError` when the browser cannot
 * start.
 *
 * @param {NonNullable<ReturnType<typeof browserForRun>>} browser
 * @param {string} url
 * @param {{ timeoutMs?: number, onBrowser?: (version: object) => void }} [options]
 * @returns {Promise<string>}
 */
export function runBrowserPage(browser, url, { timeoutMs, onBrowser } = {}) {
  return browser.kind === "chrome"
    ? runChromePage(browser.command, url, { timeoutMs, onBrowser })
    : runWebDriverPage(browser.webdriver, url, { timeoutMs, onBrowser });
}

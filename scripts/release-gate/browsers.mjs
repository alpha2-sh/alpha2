#!/usr/bin/env node
// Prints the browser-run line (browser, version, flags, mode, adapter) of each named browser this
// machine has, for the release gate's env phase; a browser this machine lacks is listed as absent.
//
// usage: browsers.mjs <chrome|edge|firefox|safari>...

import { browserForRun } from "../lib/browser-page.mjs";
import { adapterLine, probeBrowser } from "../lib/browser-probe.mjs";

const names = process.argv.slice(2);
if (names.length === 0) {
  console.log("usage: browsers.mjs <chrome|edge|firefox|safari>...");
  process.exit(2);
}
for (const name of names) {
  let browser;
  try {
    browser = browserForRun({ argv: ["--browser", name] });
  } catch (error) {
    console.log(`${name}: absent (${error.message})`);
    continue;
  }
  if (browser === null) {
    console.log(`${name}: absent`);
    continue;
  }
  try {
    console.log(adapterLine(await probeBrowser(browser)));
  } catch (error) {
    console.log(`${name}: could not start (${error.message})`);
  }
}

// What a CI job ran on: runner image, toolchains, browsers, drivers and devices.

import { execFileSync } from "node:child_process";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import os from "node:os";
import { join, resolve } from "node:path";

import { chromeExecutable, edgeExecutable } from "./chrome-cdp.mjs";
import { firefoxExecutable } from "./webdriver.mjs";

const root = resolve(import.meta.dirname, "..", "..");

/**
 * @typedef {object} ProbeContext
 * @property {NodeJS.ProcessEnv} env
 * @property {NodeJS.Platform} platform
 * @property {(file: string, args: string[]) => string | null} run Its trimmed stdout, or null.
 * @property {(path: string) => string | null} read A file's text, or null.
 * @property {string} browserReports
 */

/** @param {string} file @param {string[]} args */
function runQuietly(file, args) {
  try {
    return execFileSync(file, args, {
      cwd: root,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
      timeout: 30_000,
    }).trim();
  } catch {
    return null;
  }
}

/** @param {string} path */
function readQuietly(path) {
  try {
    return readFileSync(path, "utf8");
  } catch {
    return null;
  }
}

/**
 * `key=value` lines of an Android `source.properties` file.
 *
 * @param {string | null} text
 * @returns {Record<string, string>}
 */
export function sourceProperties(text) {
  /** @type {Record<string, string>} */
  const properties = {};
  for (const line of (text ?? "").split(/\r?\n/)) {
    const match = /^\s*([\w.]+)\s*=\s*(.*?)\s*$/.exec(line);
    if (match) properties[match[1]] = match[2];
  }
  return properties;
}

/** @param {string} path @param {ProbeContext} context */
function executableVersion(path, { platform, run }) {
  if (platform === "win32")
    return run("powershell", [
      "-NoProfile",
      "-Command",
      `(Get-Item -LiteralPath '${path.replace(/'/g, "''")}').VersionInfo.ProductVersion`,
    ]);
  return run(path, ["--version"]);
}

/** @param {ProbeContext} context */
function browsers(context) {
  const { env, platform, run } = context;
  /** @type {Record<string, string>} */
  const found = {};
  const chrome = chromeExecutable({ env, platform });
  const edge = edgeExecutable({ env, platform });
  const firefox = firefoxExecutable({ env, platform, exists: existsSync });
  /** @type {[string, string | null][]} */
  const executables = [
    ["chrome", chrome],
    ["edge", edge],
    ["firefox", firefox],
  ];
  for (const [name, path] of executables) {
    if (!path) continue;
    const version = executableVersion(path, context);
    if (version) found[name] = `${version} (${path})`;
  }
  const geckodriver = run(env.ALPHA2_GECKODRIVER || "geckodriver", [
    "--version",
  ]);
  if (geckodriver) found.geckodriver = geckodriver.split(/\r?\n/)[0];
  return found;
}

/**
 * @param {ProbeContext} context
 * @returns {string[]}
 */
function browsersUsed({ browserReports, read }) {
  if (!existsSync(browserReports)) return [];
  return readdirSync(browserReports)
    .filter((name) => name.endsWith(".json"))
    .sort()
    .flatMap((name) => {
      try {
        const report = JSON.parse(read(join(browserReports, name)) ?? "");
        const adapter = report.webgpu?.adapter;
        const adapterName = adapter
          ? ` ${adapter.vendor ?? ""} ${adapter.architecture ?? ""} ${adapter.description ?? ""}`.trimEnd()
          : " no WebGPU adapter";
        return [
          `${name.replace(/\.json$/, "")}: ${report.browser?.name} ${report.browser?.version} gpu=${report.browser?.gpu}${adapterName}`,
        ];
      } catch {
        return [];
      }
    });
}

/** @param {ProbeContext} context */
function apple({ env, platform, run }) {
  if (platform !== "darwin") return {};
  /** @type {Record<string, string>} */
  const found = {};
  const macos = run("sw_vers", ["-productVersion"]);
  const build = run("sw_vers", ["-buildVersion"]);
  if (macos) found.macos = `${macos} (${build ?? "?"})`;
  const xcode = run("xcodebuild", ["-version"]);
  if (xcode) found.xcode = xcode.split(/\r?\n/).join(" ");
  if (env.SIMULATOR) {
    const listing = run("xcrun", ["simctl", "list", "devices", "-j"]);
    try {
      const devices = JSON.parse(listing ?? "").devices;
      for (const [runtime, list] of Object.entries(devices))
        for (const device of /** @type {{ udid: string, name: string, deviceTypeIdentifier?: string }[]} */ (
          list
        ))
          if (device.udid === env.SIMULATOR)
            found.simulator = `${device.name} ${device.deviceTypeIdentifier ?? ""} ${runtime}`;
    } catch {
      found.simulator = `${env.SIMULATOR} (not listed)`;
    }
  }
  return found;
}

/** @param {ProbeContext} context */
function android({ env, read }) {
  /** @type {Record<string, string>} */
  const found = {};
  const ndk = env.ANDROID_NDK_HOME;
  if (ndk) {
    const revision = sourceProperties(read(join(ndk, "source.properties")))[
      "Pkg.Revision"
    ];
    found.ndk = `${revision ?? "?"} (${ndk})`;
  }
  const sdk = env.ANDROID_HOME || env.ANDROID_SDK_ROOT;
  if (sdk && env.ALPHA2_ANDROID_API) {
    const emulator = sourceProperties(
      read(join(sdk, "emulator", "source.properties")),
    )["Pkg.Revision"];
    if (emulator) found.emulator = emulator;
    const image = join(
      sdk,
      "system-images",
      `android-${env.ALPHA2_ANDROID_API}`,
      "google_apis",
      "x86_64",
      "source.properties",
    );
    const properties = sourceProperties(read(image));
    if (properties["Pkg.Revision"])
      found["system-image"] =
        `android-${env.ALPHA2_ANDROID_API} google_apis x86_64 revision ${properties["Pkg.Revision"]}`;
  }
  return found;
}

/** @param {ProbeContext} context */
function linux({ platform, env, run, read }) {
  if (platform !== "linux") return {};
  /** @type {Record<string, string>} */
  const found = {};
  const release = /^PRETTY_NAME="?([^"\n]*)"?$/m.exec(
    read("/etc/os-release") ?? "",
  );
  if (release) found.os = `${release[1]} (kernel ${os.release()})`;
  const mesa = run("dpkg-query", [
    "-W",
    "-f=${Version}",
    "mesa-vulkan-drivers",
  ]);
  if (mesa) found.mesa = mesa;
  if (env.VK_ICD_FILENAMES) found["vulkan-icd"] = env.VK_ICD_FILENAMES;
  return found;
}

/**
 * @param {Partial<ProbeContext>} [overrides]
 * @returns {Record<string, string | string[]>}
 */
export function collectProvenance(overrides = {}) {
  /** @type {ProbeContext} */
  const context = {
    env: process.env,
    platform: process.platform,
    run: runQuietly,
    read: readQuietly,
    browserReports: join(root, "target", "browser-reports"),
    ...overrides,
  };
  const { env, platform, run } = context;
  /** @type {Record<string, string | string[]>} */
  const record = {
    image: `${env.ImageOS ?? "?"} ${env.ImageVersion ?? "?"}`,
    runner: `${env.RUNNER_OS ?? platform} ${env.RUNNER_ARCH ?? os.arch()}`,
    cpu: `${os.cpus().length} x ${os.cpus()[0]?.model ?? "?"}`,
    memory: `${Math.round(os.totalmem() / 2 ** 30)} GiB`,
    node: process.version,
  };
  if (platform === "win32") record.os = `Windows ${os.release()}`;
  const rustc = run("rustc", ["--version"]);
  if (rustc) record.rustc = rustc;
  Object.assign(
    record,
    linux(context),
    apple(context),
    android(context),
    browsers(context),
  );
  if (env.ALPHA2_DXC_PATH) record.dxc = env.ALPHA2_DXC_PATH;
  const used = browsersUsed(context);
  if (used.length > 0) record["browsers-used"] = used;
  return record;
}

// Backend selection, environments and device-line checks for scripts/test-gpu-validation.mjs.

import { basename, join } from "node:path";

/** @typedef {"vulkan" | "metal" | "dx12" | "gl"} Backend */

const PLATFORMS = {
  vulkan: ["linux", "win32"],
  metal: ["darwin"],
  dx12: ["win32"],
  gl: ["linux", "win32"],
};

/** wgpu's `Backend` debug names, as `Capabilities::summary` prints them. */
const BACKEND_NAMES = {
  vulkan: "Vulkan",
  metal: "Metal",
  dx12: "Dx12",
  gl: "Gl",
};

export const BACKENDS = Object.keys(PLATFORMS);

/**
 * @param {NodeJS.Platform} platform
 * @returns {Backend}
 */
export function defaultBackend(platform) {
  if (platform === "darwin") return "metal";
  if (platform === "win32") return "dx12";
  return "vulkan";
}

/**
 * @param {string} backend
 * @param {NodeJS.Platform} platform
 */
export function backendProblem(backend, platform) {
  if (!(backend in PLATFORMS))
    return `unknown backend ${backend}: one of ${BACKENDS.join(", ")}`;
  if (!PLATFORMS[backend].includes(platform))
    return `${backend} is not available on ${platform}`;
  return undefined;
}

/**
 * Adds the crates that share devices with video APIs on backends the native host uses for them.
 *
 * @param {Backend} backend
 */
export function defaultCrates(backend) {
  const shared = ["alpha2-render", "alpha2-engine"];
  if (backend === "metal" || backend === "dx12")
    return [...shared, "alpha2-native-gpu", "alpha2-native", "alpha2-host"];
  return shared;
}

/**
 * @param {Backend} backend
 * @param {{
 *   base: NodeJS.ProcessEnv,
 *   platform: NodeJS.Platform,
 *   report: string,
 *   shaderValidation?: boolean,
 *   gpuBasedValidation?: boolean,
 * }} options
 */
export function backendEnvironment(backend, options) {
  const environment = {
    ...options.base,
    WGPU_BACKEND: backend,
    WGPU_VALIDATION: "1",
    ALPHA2_VALIDATION_LOG: options.base.ALPHA2_VALIDATION_LOG || "report",
    ALPHA2_VALIDATION_REPORT: options.report,
  };
  if (backend === "vulkan") {
    Object.assign(environment, {
      // Routes the layer's messages to the collector.
      WGPU_DEBUG: "1",
      VK_INSTANCE_LAYERS: "VK_LAYER_KHRONOS_validation",
      VK_KHRONOS_VALIDATION_VALIDATE_SYNC: "true",
      VK_KHRONOS_VALIDATION_DEBUG_ACTION: "VK_DBG_LAYER_ACTION_LOG_MSG",
      VK_KHRONOS_VALIDATION_REPORT_FLAGS: "error,warn,perf,info",
      // Parallel test binaries race writing the layer's shader cache file.
      VK_KHRONOS_VALIDATION_CHECK_SHADERS_CACHING: "false",
    });
    // A log file named here would be truncated by each instance.
    delete environment.VK_KHRONOS_VALIDATION_LOG_FILENAME;
  } else if (backend === "metal") {
    Object.assign(environment, {
      MTL_DEBUG_LAYER: "1",
      MTL_DEBUG_LAYER_ERROR_MODE: "assert",
      MTL_DEBUG_LAYER_WARNING_MODE: "nslog",
    });
    if (options.shaderValidation) environment.MTL_SHADER_VALIDATION = "1";
  } else if (backend === "dx12") {
    if (options.gpuBasedValidation) environment.WGPU_GPU_BASED_VALIDATION = "1";
  } else if (backend === "gl") {
    environment.WGPU_DEBUG = "1";
    if (options.platform === "linux") environment.LIBGL_ALWAYS_SOFTWARE = "1";
  }
  return environment;
}

/**
 * `<backend>-<ICD manifest or adapter>`, so two drivers of one backend keep their logs apart.
 *
 * @param {string} root
 * @param {Backend} backend
 * @param {NodeJS.ProcessEnv} env
 */
export function logDirectory(root, backend, env) {
  const manifest = env.VK_ICD_FILENAMES?.split(/[:;]/)[0];
  const device =
    backend === "vulkan" && manifest
      ? basename(manifest, ".json")
      : env.ALPHA2_GPU_ADAPTER || env.WGPU_ADAPTER_NAME || "default";
  const slug =
    device
      .toLowerCase()
      .replace(/[^a-z0-9._-]+/g, "-")
      .replace(/^-|-$/g, "") || "default";
  return join(root, `${backend}-${slug}`);
}

// Followed by `Capabilities::summary`: `<adapter> (<backend>[, driver <driver>][, <compiler>])`.
const DEVICE_LINE =
  /(?:render|frame|gpu) tests on (.+) \((\w+)((?:, [^,]*)*)\)\s*$/;

/**
 * @param {string} output
 * @returns {{ adapter: string, backend: string, driver: string }[]}
 */
export function parseDevices(output) {
  const seen = new Map();
  for (const line of output.split("\n")) {
    const match = DEVICE_LINE.exec(line);
    if (match === null) continue;
    const [, adapter, backend, rest] = match;
    const driver =
      rest
        .split(", ")
        .find((part) => part.startsWith("driver "))
        ?.slice("driver ".length) ?? "";
    seen.set(`${adapter}\n${backend}\n${driver}`, { adapter, backend, driver });
  }
  return [...seen.values()];
}

const ICD_ADAPTERS = [
  ["lvp", /llvmpipe/i],
  ["radeon", /AMD|RADV/i],
  ["nvidia", /NVIDIA/i],
  ["intel", /Intel/i],
];

/**
 * `undefined` for an unknown manifest, which is printed but not checked.
 *
 * @param {string | undefined} manifest
 */
export function expectedAdapter(manifest) {
  if (!manifest) return undefined;
  const name = basename(manifest).toLowerCase();
  return ICD_ADAPTERS.find(([key]) => name.includes(key))?.[1];
}

/**
 * @param {{
 *   output: string,
 *   backend: Backend,
 *   opensDevice: boolean,
 *   manifest?: string,
 * }} run `opensDevice`: a crate's own test binary, which holds its GPU tests.
 */
export function judgeDevices(run) {
  const devices = parseDevices(run.output);
  const problems = [];
  if (devices.length === 0 && run.opensDevice)
    problems.push("no adapter line: the binary never opened a device");
  const wanted = BACKEND_NAMES[run.backend];
  const pattern =
    run.backend === "vulkan" ? expectedAdapter(run.manifest) : undefined;
  for (const device of devices) {
    if (device.backend !== wanted)
      problems.push(`${device.adapter} is on ${device.backend}, not ${wanted}`);
    else if (pattern !== undefined && !pattern.test(device.adapter))
      problems.push(
        `${device.adapter} is not the adapter of ${basename(run.manifest ?? "")}`,
      );
  }
  return { devices, problems };
}

/**
 * @param {string} output
 */
export function metalAssertions(output) {
  return output.split("\n").filter((line) => line.includes("failed assertion"));
}

/**
 * @param {string} text
 * @returns {{ errors: number, warnings: number, tests: string[] }}
 */
export function countReport(text) {
  let errors = 0;
  let warnings = 0;
  const tests = new Set();
  for (const line of text.split("\n")) {
    if (line.trim() === "") continue;
    const record = JSON.parse(line);
    if (record.level === "ERROR") errors += 1;
    else warnings += 1;
    tests.add(record.test);
  }
  return { errors, warnings, tests: [...tests] };
}

/**
 * @param {string} name
 * @param {{ adapter: string, backend: string, driver: string }[]} devices
 * @param {string[]} problems
 * @param {string} counts
 */
export function summaryLine(name, devices, problems, counts) {
  const on =
    devices.length === 0
      ? " (no device)"
      : ` on ${devices
          .map(
            ({ adapter, backend, driver }) =>
              `${adapter} (${[backend, driver].filter(Boolean).join(", ")})`,
          )
          .join("; ")}`;
  return problems.length === 0
    ? `ok ${name}${on} (${counts})`
    : `FAIL ${name}${on}: ${problems.join("; ")}`;
}

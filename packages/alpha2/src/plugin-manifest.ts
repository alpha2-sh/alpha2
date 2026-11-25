// Plugin manifests, checked as `alpha2_engine::extract::PluginManifest::from_json` checks them.

import type { ExtractorDescriptor } from "./extractor.js";
import {
  MAX_DATA_CHANNELS,
  MAX_DATA_LABEL_BYTES,
  MAX_SAMPLE_RATE,
  MIN_SAMPLE_RATE,
} from "./generated/project-schema.js";

/** The ABI this build implements. */
export const PLUGIN_ABI = "alpha2-plugin/1";

/** Mirrors `alpha2_engine::extract::MAX_MANIFEST_BYTES`: the longest manifest, in UTF-8 bytes. */
export const MAX_MANIFEST_BYTES = 1024 * 1024;
/** Mirrors `alpha2_engine::extract::MAX_PLUGIN_ARTIFACTS`. */
export const MAX_PLUGIN_ARTIFACTS = 64;
/** Mirrors `alpha2_engine::extract::MAX_PLUGIN_RESOURCES`. */
export const MAX_PLUGIN_RESOURCES = 64;
/** Mirrors `alpha2_engine::extract::MAX_PLUGIN_SLOTS`, counted within the resources. */
export const MAX_PLUGIN_SLOTS = 16;
/** Mirrors `alpha2_engine::extract::MAX_PLUGIN_ARTIFACT_BYTES`. */
export const MAX_PLUGIN_ARTIFACT_BYTES = 256 * 1024 * 1024;
/** Mirrors `alpha2_engine::extract::MAX_PLUGIN_RESOURCE_BYTES`, one pinned resource. */
export const MAX_PLUGIN_RESOURCE_BYTES = 2 * 1024 * 1024 * 1024;
/** Mirrors `alpha2_engine::extract::MAX_PLUGIN_RESOURCES_TOTAL_BYTES`, pinned resources only. */
export const MAX_PLUGIN_RESOURCES_TOTAL_BYTES = 4 * 1024 * 1024 * 1024;
/** Mirrors `alpha2_engine::extract::MAX_SLOT_NAME_BYTES`. */
export const MAX_SLOT_NAME_BYTES = 64;
/** Mirrors `alpha2_engine::extract::MAX_SLOT_ACCEPT`. */
export const MAX_SLOT_ACCEPT = 16;
/** Mirrors `alpha2_engine::extract::MAX_SLOT_ACCEPT_BYTES`. */
export const MAX_SLOT_ACCEPT_BYTES = 64;
/** Mirrors `alpha2_engine::extract::MAX_ABOUT_NAME_CHARS`, in Unicode code points. */
export const MAX_ABOUT_NAME_CHARS = 80;
/** Mirrors `alpha2_engine::extract::MAX_ABOUT_SUMMARY_CHARS`. */
export const MAX_ABOUT_SUMMARY_CHARS = 400;
/** Mirrors `alpha2_engine::extract::MAX_ABOUT_AUTHOR_CHARS`. */
export const MAX_ABOUT_AUTHOR_CHARS = 80;
/** Mirrors `alpha2_engine::extract::MAX_ABOUT_HOMEPAGE_CHARS`. */
export const MAX_ABOUT_HOMEPAGE_CHARS = 2048;
/** Mirrors `alpha2_engine::extract::MAX_ABOUT_LICENSE_CHARS`. */
export const MAX_ABOUT_LICENSE_CHARS = 256;
/** Mirrors `alpha2_engine::extract::MAX_OUTPUT_LABEL_CHARS`. */
export const MAX_OUTPUT_LABEL_CHARS = 80;
/** Mirrors `alpha2_engine::extract::MAX_OUTPUT_DESCRIPTION_CHARS`. */
export const MAX_OUTPUT_DESCRIPTION_CHARS = 400;
/** Mirrors `alpha2_engine::extract::MAX_EXTRACTOR_OUTPUTS`. */
export const MAX_EXTRACTOR_OUTPUTS = 32;

const DEFAULT_MAX_SIZE = 960;
const DEFAULT_BLOCK_FRAMES = 1024;
const BLOCK_FRAMES = { min: 64, max: 65_536 };
const MAX_U32 = 0xffff_ffff;
const SPDX_MAX_NESTING = 32;

/**
 * A plugin that was refused, did not load or failed, with the code every surface uses.
 * `details` is what a caller reads besides the code: `{ slot, reason }` for
 * `PLUGIN_RESOURCE_INVALID`.
 */
export class PluginError extends Error {
  readonly code: string;
  readonly details?: Readonly<Record<string, unknown>>;

  constructor(
    code: string,
    message: string,
    options?: ErrorOptions & {
      readonly details?: Readonly<Record<string, unknown>>;
    },
  ) {
    super(message, options);
    this.name = "PluginError";
    this.code = code;
    if (options?.details !== undefined) this.details = options.details;
  }
}

/** One build of a plugin. */
export interface PluginArtifact {
  /** `wasm32`, or the Rust target triple a native artifact is built for. */
  readonly target: string;
  readonly type: "wasm" | "cdylib" | "staticlib";
  /** A path under the manifest's own directory, or an absolute `https:` URL. */
  readonly url: string;
  /** 64 lowercase hexadecimal digits. */
  readonly sha256: string;
  readonly bytes: number;
}

/** A file the manifest pins. */
export interface PinnedPluginResource {
  readonly name: string;
  readonly url: string;
  readonly sha256: string;
  readonly bytes: number;
  readonly supplied?: undefined;
}

/** A slot the caller fills with a file or a directory when it loads the plugin. */
export interface SuppliedPluginResource {
  readonly name: string;
  readonly supplied: {
    readonly shape: "file" | "directory";
    /** `true` when left out. */
    readonly required?: boolean;
    /** The largest single file the plugin takes from the slot, in bytes. */
    readonly max_bytes?: number;
    /** File name suffixes a picker offers, such as `.onnx`. */
    readonly accept?: readonly string[];
  };
}

export type PluginResource = PinnedPluginResource | SuppliedPluginResource;

/** What an interface shows about a plugin: plain text, never markup. */
export interface PluginAbout {
  readonly name: string;
  readonly summary?: string;
  readonly author?: string;
  /** An `https:` URL. */
  readonly homepage?: string;
  /** An SPDX license expression. */
  readonly license?: string;
}

/** A plugin's manifest, as `schema/plugin-manifest.v1.json` describes it. */
export interface PluginManifest {
  readonly id: string;
  /** The package's semantic version. */
  readonly version: string;
  readonly abi: typeof PLUGIN_ABI;
  readonly kind: "extractor";
  readonly artifacts: readonly PluginArtifact[];
  readonly resources?: readonly PluginResource[];
  readonly about?: PluginAbout;
  /** As the engine reads it: `max_size`, `block_frames` and `params_schema` filled in. */
  readonly descriptor: ExtractorDescriptor;
}

type Json = Record<string, unknown>;

/** Why a manifest is refused, as `ManifestError::Invalid`. */
class Invalid extends Error {}

const fail = (why: string): never => {
  throw new Invalid(why);
};

function isRecord(value: unknown): value is Json {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function fields(
  value: unknown,
  path: string,
  allowed: readonly string[],
  required: readonly string[],
): Json {
  if (!isRecord(value)) return fail(`${path} is not an object`);
  for (const key of Object.keys(value))
    if (!allowed.includes(key)) fail(`${path} has unknown field ${key}`);
  for (const key of required)
    if (value[key] === undefined) fail(`${path} lacks ${key}`);
  return value;
}

/** A serde `Option`: absent and `null` are both none. */
function optional(value: unknown): unknown {
  return value === null ? undefined : value;
}

function text(value: unknown, path: string): string {
  return typeof value === "string" ? value : fail(`${path} is not a string`);
}

function integer(value: unknown, path: string, max: number): number {
  return typeof value === "number" &&
    Number.isSafeInteger(value) &&
    value >= 0 &&
    value <= max
    ? value
    : fail(`${path} is not an integer from 0 to ${max}`);
}

function oneOf<T extends string>(
  value: unknown,
  path: string,
  values: readonly T[],
): T {
  return values.includes(value as T)
    ? (value as T)
    : fail(`${path} is not one of ${values.join(", ")}`);
}

function list(value: unknown, path: string): unknown[] {
  return Array.isArray(value) ? value : fail(`${path} is not a list`);
}

const encoder = new TextEncoder();
const utf8Length = (value: string): number => encoder.encode(value).length;

const CONTROL = /\p{Cc}/u;

/** Mirrors `plain_text`: `min` to `max` code points, no control character. */
function plainText(
  value: unknown,
  path: string,
  min: number,
  max: number,
): string {
  const read = text(value, path);
  const length = [...read].length;
  if (length < min || length > max || CONTROL.test(read))
    fail(
      `${path} is not ${min} to ${max} characters without control characters`,
    );
  return read;
}

/** Mirrors `semver`. */
function semver(version: string): boolean {
  const plus = version.indexOf("+");
  const rest = plus < 0 ? version : version.slice(0, plus);
  const build = plus < 0 ? undefined : version.slice(plus + 1);
  const dash = rest.indexOf("-");
  const core = dash < 0 ? rest : rest.slice(0, dash);
  const pre = dash < 0 ? undefined : rest.slice(dash + 1);
  const identifiers = (part: string) =>
    part.split(".").every((item) => /^[A-Za-z0-9-]+$/.test(item));
  const numbers = core.split(".");
  return (
    numbers.length === 3 &&
    numbers.every((part) => /^(0|[1-9][0-9]*)$/.test(part)) &&
    (pre === undefined || identifiers(pre)) &&
    (build === undefined || identifiers(build))
  );
}

/** Mirrors `reference`: a path under the manifest's directory or an `https:` URL. */
function checkReference(url: string): void {
  if (url.length === 0) fail("a URL is empty");
  const scheme = /^([A-Za-z][A-Za-z0-9+.-]*):/.exec(url)?.[1];
  if (scheme === "https") return;
  if (scheme !== undefined)
    fail(
      scheme.length === 1
        ? `${url}: a drive-prefixed path is not under the manifest`
        : `${url}: only https: URLs and paths under the manifest are allowed`,
    );
  if (/[\\\0]/.test(url))
    fail(`${url}: a path takes neither backslashes nor NUL`);
  if (url.startsWith("/"))
    fail(`${url}: an absolute path is not under the manifest`);
  let depth = 0;
  for (const segment of url.split(/[?#]/)[0].split("/")) {
    if (segment === "" || segment === ".") continue;
    if (segment === "..") {
      depth -= 1;
      if (depth < 0) fail(`${url}: the path climbs above the manifest`);
    } else depth += 1;
  }
}

function checkFile(entry: Json, path: string): void {
  checkReference(text(entry.url, `${path}.url`));
  if (!/^[0-9a-f]{64}$/.test(text(entry.sha256, `${path}.sha256`)))
    fail(`${String(entry.url)}: sha256 is not 64 lowercase hexadecimal digits`);
}

/** Mirrors `is_slot_name`. */
function isSlotName(name: string): boolean {
  return (
    utf8Length(name) <= MAX_SLOT_NAME_BYTES &&
    /^[A-Za-z0-9_-][A-Za-z0-9._-]*$/.test(name)
  );
}

function spdxId(token: string | undefined): boolean {
  return (
    token !== undefined &&
    !["AND", "OR", "WITH", "(", ")"].includes(token) &&
    /^[A-Za-z0-9.:-]+$/.test(token)
  );
}

/** Mirrors `spdx_expression` (SPDX 2.3 annex D); the ids are not looked up. */
function spdxExpression(expression: string): boolean {
  const tokens = expression
    .replaceAll("(", " ( ")
    .replaceAll(")", " ) ")
    .split(/[\t\n\f\r ]+/)
    .filter((token) => token !== "");
  let at = 0;
  const term = (depth: number): boolean => {
    const token = tokens[at++];
    if (token === undefined) return false;
    if (token === "(") {
      if (depth >= SPDX_MAX_NESTING || !or(depth + 1) || tokens[at] !== ")")
        return false;
      at += 1;
      return true;
    }
    if (!spdxId(token.endsWith("+") ? token.slice(0, -1) : token)) return false;
    if (tokens[at] === "WITH") {
      at += 1;
      return spdxId(tokens[at++]);
    }
    return true;
  };
  const and = (depth: number): boolean => {
    if (!term(depth)) return false;
    while (tokens[at] === "AND") {
      at += 1;
      if (!term(depth)) return false;
    }
    return true;
  };
  const or = (depth: number): boolean => {
    if (!and(depth)) return false;
    while (tokens[at] === "OR") {
      at += 1;
      if (!and(depth)) return false;
    }
    return true;
  };
  return or(0) && at === tokens.length;
}

function readAbout(value: unknown): PluginAbout {
  const about = fields(
    value,
    "about",
    ["name", "summary", "author", "homepage", "license"],
    ["name"],
  );
  const read: Record<string, string> = {
    name: plainText(about.name, "about.name", 1, MAX_ABOUT_NAME_CHARS),
  };
  const summary = optional(about.summary);
  if (summary !== undefined)
    read.summary = plainText(
      summary,
      "about.summary",
      0,
      MAX_ABOUT_SUMMARY_CHARS,
    );
  const author = optional(about.author);
  if (author !== undefined)
    read.author = plainText(author, "about.author", 0, MAX_ABOUT_AUTHOR_CHARS);
  const homepage = optional(about.homepage);
  if (homepage !== undefined) {
    const url = text(homepage, "about.homepage");
    if (
      url.length > MAX_ABOUT_HOMEPAGE_CHARS ||
      !/^https:\/\/[!-~]+$/.test(url)
    )
      fail(
        `about.homepage is not an https: URL of printable ASCII, at most ${MAX_ABOUT_HOMEPAGE_CHARS} characters`,
      );
    read.homepage = url;
  }
  const license = optional(about.license);
  if (license !== undefined) {
    const expression = text(license, "about.license");
    if (
      expression.length > MAX_ABOUT_LICENSE_CHARS ||
      !/^[A-Za-z0-9.+:() -]+$/.test(expression) ||
      !spdxExpression(expression)
    )
      fail(
        `about.license ${JSON.stringify(expression)} is not an SPDX license expression of at most ${MAX_ABOUT_LICENSE_CHARS} characters`,
      );
    read.license = expression;
  }
  return read as unknown as PluginAbout;
}

type Output = ExtractorDescriptor["outputs"][number];

function readOutput(value: unknown, path: string): Output {
  const output = fields(
    value,
    path,
    [
      "name",
      "kind",
      "channels",
      "interpolation",
      "suggest",
      "label",
      "description",
    ],
    ["name", "kind"],
  );
  const read: Record<string, unknown> = {
    name: text(output.name, `${path}.name`),
    kind: oneOf(output.kind, `${path}.kind`, [
      "points",
      "scalars",
      "similarity",
      "words",
      "intervals",
    ]),
  };
  if (output.channels !== undefined) {
    const channels = list(output.channels, `${path}.channels`).map(
      (channel, index) => text(channel, `${path}.channels[${index}]`),
    );
    if (channels.length > 0) read.channels = channels;
  }
  const interpolation = optional(output.interpolation);
  if (interpolation !== undefined)
    read.interpolation = oneOf(interpolation, `${path}.interpolation`, [
      "hold",
      "linear",
    ]);
  const suggest = optional(output.suggest);
  if (suggest !== undefined)
    read.suggest = oneOf(suggest, `${path}.suggest`, [
      "bind",
      "remove_ranges",
      "split_at",
    ]);
  const label = optional(output.label);
  if (label !== undefined)
    read.label = plainText(label, `${path}.label`, 1, MAX_OUTPUT_LABEL_CHARS);
  const description = optional(output.description);
  if (description !== undefined)
    read.description = plainText(
      description,
      `${path}.description`,
      0,
      MAX_OUTPUT_DESCRIPTION_CHARS,
    );
  return read as unknown as Output;
}

function readInput(value: unknown): ExtractorDescriptor["input"] {
  const media = isRecord(value) ? value.media : undefined;
  const path = "descriptor.input";
  const from = (input: Json) =>
    list(input.from, `${path}.from`).map((item, index) =>
      oneOf(item, `${path}.from[${index}]`, ["source", "timeline"] as const),
    );
  if (media === "video") {
    const input = fields(
      value,
      path,
      ["media", "from", "format", "max_size"],
      ["media", "from", "format"],
    );
    return {
      media: "video",
      from: from(input),
      format: oneOf(input.format, `${path}.format`, ["rgba8", "luma8"]),
      max_size:
        input.max_size === undefined
          ? DEFAULT_MAX_SIZE
          : integer(input.max_size, `${path}.max_size`, MAX_U32),
    };
  }
  if (media === "audio") {
    const input = fields(
      value,
      path,
      ["media", "from", "format", "sample_rate", "channels", "block_frames"],
      ["media", "from", "format"],
    );
    const sampleRate = optional(input.sample_rate);
    const channels = optional(input.channels);
    return {
      media: "audio",
      from: from(input),
      format: oneOf(input.format, `${path}.format`, ["f32"]),
      ...(sampleRate === undefined
        ? {}
        : { sample_rate: integer(sampleRate, `${path}.sample_rate`, MAX_U32) }),
      ...(channels === undefined
        ? {}
        : {
            channels: oneOf(channels, `${path}.channels`, [
              "mono",
              "source",
            ] as const),
          }),
      block_frames:
        input.block_frames === undefined
          ? DEFAULT_BLOCK_FRAMES
          : integer(input.block_frames, `${path}.block_frames`, MAX_U32),
    };
  }
  return fail(`${path}.media is not video or audio`);
}

/** Mirrors `validate_descriptor`. */
function checkDescriptor(descriptor: ExtractorDescriptor): void {
  if (descriptor.id.length === 0) fail("empty id");
  const { outputs, input } = descriptor;
  if (outputs.length === 0 || outputs.length > MAX_EXTRACTOR_OUTPUTS)
    fail(`needs 1 to ${MAX_EXTRACTOR_OUTPUTS} outputs`);
  const names = new Set<string>();
  for (const output of outputs) {
    if (output.name.length === 0 || names.has(output.name))
      fail("an output name is empty or repeated");
    names.add(output.name);
    const channels = output.channels ?? [];
    const sampled = output.kind === "points" || output.kind === "scalars";
    if (sampled === (channels.length === 0))
      fail("points and scalars outputs need channels; others take none");
    if (channels.length > MAX_DATA_CHANNELS)
      fail("an output has too many channels");
    if (
      new Set(channels).size !== channels.length ||
      channels.some(
        (channel) =>
          channel.length === 0 || utf8Length(channel) > MAX_DATA_LABEL_BYTES,
      )
    )
      fail("a channel name is empty, too long or repeated");
    if (
      output.interpolation !== undefined &&
      (output.kind === "words" || output.kind === "intervals")
    )
      fail("only sampled outputs take an interpolation");
  }
  if (input.from.length === 0) fail("reads no input");
  if (input.media === "video" && input.max_size === 0) fail("max_size is zero");
  if (
    input.media === "audio" &&
    ((input.block_frames ?? DEFAULT_BLOCK_FRAMES) < BLOCK_FRAMES.min ||
      (input.block_frames ?? DEFAULT_BLOCK_FRAMES) > BLOCK_FRAMES.max ||
      (input.sample_rate !== undefined &&
        (input.sample_rate < MIN_SAMPLE_RATE ||
          input.sample_rate > MAX_SAMPLE_RATE)))
  )
    fail("block_frames or sample_rate out of range");
}

/** Reads a descriptor as the engine does, with the defaults it fills in. */
function readDescriptor(value: unknown): ExtractorDescriptor {
  const descriptor = fields(
    value,
    "descriptor",
    ["id", "version", "input", "params_schema", "outputs"],
    ["id", "version", "input", "outputs"],
  );
  const read: ExtractorDescriptor = {
    id: text(descriptor.id, "descriptor.id"),
    version: integer(descriptor.version, "descriptor.version", MAX_U32),
    input: readInput(descriptor.input),
    params_schema:
      descriptor.params_schema === undefined ? {} : descriptor.params_schema,
    outputs: list(descriptor.outputs, "descriptor.outputs").map(
      (output, index) => readOutput(output, `descriptor.outputs[${index}]`),
    ),
  };
  checkDescriptor(read);
  return read;
}

function readResource(value: unknown, index: number): PluginResource {
  const path = `resources[${index}]`;
  const entry = fields(
    value,
    path,
    ["name", "url", "sha256", "bytes", "supplied"],
    ["name"],
  );
  const name = text(entry.name, `${path}.name`);
  const pin = ["url", "sha256", "bytes"].map((key) => optional(entry[key]));
  const supplied = optional(entry.supplied);
  if (supplied !== undefined) {
    if (pin.some((part) => part !== undefined))
      fail(
        `resource ${JSON.stringify(name)} has supplied and a pin; a resource is one or the other`,
      );
    const slot = fields(
      supplied,
      `${path}.supplied`,
      ["shape", "required", "max_bytes", "accept"],
      ["shape"],
    );
    const read: Record<string, unknown> = {
      shape: oneOf(slot.shape, `${path}.supplied.shape`, ["file", "directory"]),
    };
    if (slot.required !== undefined) {
      if (typeof slot.required !== "boolean")
        fail(`${path}.supplied.required is not a boolean`);
      read.required = slot.required;
    }
    const maxBytes = optional(slot.max_bytes);
    if (maxBytes !== undefined)
      read.max_bytes = integer(
        maxBytes,
        `${path}.supplied.max_bytes`,
        Number.MAX_SAFE_INTEGER,
      );
    if (slot.accept !== undefined)
      read.accept = list(slot.accept, `${path}.supplied.accept`).map(
        (suffix, at) => text(suffix, `${path}.supplied.accept[${at}]`),
      );
    return { name, supplied: read } as unknown as SuppliedPluginResource;
  }
  if (pin.every((part) => part === undefined))
    fail(
      `resource ${JSON.stringify(name)} has neither url, sha256 and bytes nor supplied`,
    );
  if (pin.some((part) => part === undefined))
    fail(`resource ${JSON.stringify(name)} needs all of url, sha256 and bytes`);
  return {
    name,
    url: text(entry.url, `${path}.url`),
    sha256: text(entry.sha256, `${path}.sha256`),
    bytes: integer(entry.bytes, `${path}.bytes`, Number.MAX_SAFE_INTEGER),
  };
}

function checkSlot(resource: SuppliedPluginResource): void {
  const { name, supplied } = resource;
  if (!isSlotName(name))
    fail(
      `slot name ${JSON.stringify(name)} is not 1 to ${MAX_SLOT_NAME_BYTES} of A-Z, a-z, 0-9, '.', '_' and '-', not starting with '.'`,
    );
  if (supplied.max_bytes === 0) fail(`slot ${name}: max_bytes is 0`);
  const accept = supplied.accept ?? [];
  if (accept.length > MAX_SLOT_ACCEPT)
    fail(`slot ${name}: accept lists more than ${MAX_SLOT_ACCEPT} suffixes`);
  const bad = accept.find(
    (suffix) =>
      suffix.length === 0 || utf8Length(suffix) > MAX_SLOT_ACCEPT_BYTES,
  );
  if (bad !== undefined)
    fail(
      `slot ${name}: accept suffix ${JSON.stringify(bad)} is empty or longer than ${MAX_SLOT_ACCEPT_BYTES} bytes`,
    );
}

/** Mirrors `validate_resources`. */
function checkResources(resources: readonly PluginResource[]): void {
  const names = new Set<string>();
  const directories: string[] = [];
  let slots = 0;
  let total = 0;
  for (const resource of resources) {
    if (resource.name.length === 0 || names.has(resource.name))
      fail(
        `resource name ${JSON.stringify(resource.name)} is empty or repeated`,
      );
    names.add(resource.name);
    if (resource.supplied === undefined) {
      checkFile(resource as unknown as Json, `resource ${resource.name}`);
      if (resource.bytes > MAX_PLUGIN_RESOURCE_BYTES)
        fail(
          `resource ${resource.name} is longer than ${MAX_PLUGIN_RESOURCE_BYTES} bytes`,
        );
      total += resource.bytes;
    } else {
      slots += 1;
      checkSlot(resource);
      if (resource.supplied.shape === "directory")
        directories.push(`${resource.name}/`);
    }
  }
  if (slots > MAX_PLUGIN_SLOTS)
    fail(`declares more than ${MAX_PLUGIN_SLOTS} resource slots`);
  const under = resources.find(
    (resource) =>
      resource.supplied === undefined &&
      directories.some((prefix) => resource.name.startsWith(prefix)),
  );
  if (under !== undefined)
    fail(`pinned resource ${under.name} lies under a directory slot's names`);
  if (total > MAX_PLUGIN_RESOURCES_TOTAL_BYTES)
    fail(
      `the pinned resources are longer than ${MAX_PLUGIN_RESOURCES_TOTAL_BYTES} bytes together`,
    );
}

function readManifest(value: unknown): PluginManifest {
  const manifest = fields(
    value,
    "the manifest",
    [
      "id",
      "version",
      "abi",
      "kind",
      "artifacts",
      "resources",
      "about",
      "descriptor",
    ],
    ["id", "version", "abi", "kind", "artifacts", "descriptor"],
  );
  if (manifest.abi !== PLUGIN_ABI) fail(`abi is not ${PLUGIN_ABI}`);
  const id = text(manifest.id, "id");
  const version = text(manifest.version, "version");
  oneOf(manifest.kind, "kind", ["extractor"]);
  const artifacts = list(manifest.artifacts, "artifacts").map(
    (item, index): PluginArtifact => {
      const path = `artifacts[${index}]`;
      const artifact = fields(
        item,
        path,
        ["target", "type", "url", "sha256", "bytes"],
        ["target", "type", "url", "sha256", "bytes"],
      );
      return {
        target: text(artifact.target, `${path}.target`),
        type: oneOf(artifact.type, `${path}.type`, [
          "wasm",
          "cdylib",
          "staticlib",
        ]),
        url: text(artifact.url, `${path}.url`),
        sha256: text(artifact.sha256, `${path}.sha256`),
        bytes: integer(
          artifact.bytes,
          `${path}.bytes`,
          Number.MAX_SAFE_INTEGER,
        ),
      };
    },
  );
  const resources =
    manifest.resources === undefined
      ? []
      : list(manifest.resources, "resources").map(readResource);
  const aboutValue = optional(manifest.about);
  const about = aboutValue === undefined ? undefined : readAbout(aboutValue);
  const descriptor = readDescriptor(manifest.descriptor);
  if (id !== descriptor.id)
    fail(`id ${id} differs from the descriptor's ${descriptor.id}`);
  if (!semver(version)) fail(`version ${version} is not a semantic version`);
  if (artifacts.length === 0 || artifacts.length > MAX_PLUGIN_ARTIFACTS)
    fail(`needs 1 to ${MAX_PLUGIN_ARTIFACTS} artifacts`);
  if (resources.length > MAX_PLUGIN_RESOURCES)
    fail(`lists more than ${MAX_PLUGIN_RESOURCES} resources`);
  const targets = new Set<string>();
  for (const artifact of artifacts) {
    const key = `${artifact.type}\u0000${artifact.target}`;
    if (artifact.target.length === 0 || targets.has(key))
      fail(
        `artifact target ${JSON.stringify(artifact.target)} is empty or listed twice for one type`,
      );
    targets.add(key);
    if ((artifact.target === "wasm32") !== (artifact.type === "wasm"))
      fail(`artifact ${artifact.url}: only the wasm32 target is a wasm module`);
    checkFile(artifact as unknown as Json, `artifact ${artifact.url}`);
    if (artifact.bytes > MAX_PLUGIN_ARTIFACT_BYTES)
      fail(
        `artifact ${artifact.url} is longer than ${MAX_PLUGIN_ARTIFACT_BYTES} bytes`,
      );
  }
  checkResources(resources);
  return {
    id,
    version,
    abi: PLUGIN_ABI,
    kind: "extractor",
    artifacts,
    ...(resources.length === 0 ? {} : { resources }),
    ...(about === undefined ? {} : { about }),
    descriptor,
  };
}

/**
 * Reads and checks a manifest by the rules the engine's loaders use, fetching nothing. The
 * descriptor comes back as the engine reads it, with `max_size`, `block_frames` and
 * `params_schema` filled in and `null` options left out.
 *
 * Throws a {@link PluginError}: `PLUGIN_ABI_UNSUPPORTED` for another `abi`, checked first, and
 * `PLUGIN_LOAD_FAILED` for text past {@link MAX_MANIFEST_BYTES}, text that is not JSON, or a
 * manifest that breaks a rule of `docs/plugin-abi.md`.
 */
export function parsePluginManifest(text: string): PluginManifest {
  const invalid = (why: string, cause?: unknown) =>
    new PluginError("PLUGIN_LOAD_FAILED", `invalid plugin manifest: ${why}`, {
      cause,
    });
  if (
    text.length > MAX_MANIFEST_BYTES ||
    (text.length * 3 > MAX_MANIFEST_BYTES &&
      utf8Length(text) > MAX_MANIFEST_BYTES)
  )
    throw invalid(`longer than ${MAX_MANIFEST_BYTES} bytes`);
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch (cause) {
    throw invalid(String(cause), cause);
  }
  const abi = isRecord(value) ? value.abi : undefined;
  if (typeof abi === "string" && abi !== PLUGIN_ABI)
    throw new PluginError(
      "PLUGIN_ABI_UNSUPPORTED",
      `the plugin needs ABI ${abi}; this build implements ${PLUGIN_ABI}`,
    );
  try {
    return readManifest(value);
  } catch (cause) {
    if (cause instanceof Invalid) throw invalid(cause.message);
    throw cause;
  }
}

/** Reads a descriptor JSON as {@link parsePluginManifest} reads a manifest's. */
export function readPluginDescriptor(value: unknown): ExtractorDescriptor {
  try {
    return readDescriptor(value);
  } catch (cause) {
    if (cause instanceof Invalid)
      throw new PluginError(
        "PLUGIN_DESCRIPTOR_MISMATCH",
        `the artifact's descriptor is not one: ${cause.message}`,
      );
    throw cause;
  }
}

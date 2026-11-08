// Plugins in the browser: a manifest's wasm artifact, checked against its pinned hashes and run
// through the `alpha2-plugin/1` ABI as an extractor for `runExtractor`. Native artifacts never run
// here.

import type {
  ExtractAudio,
  ExtractContext,
  ExtractEmitter,
  ExtractFrame,
  Extractor,
  ExtractorDescriptor,
} from "./extract.js";

/** The ABI this loader implements. */
export const PLUGIN_ABI = "alpha2-plugin/1";

/** Linear memory a plugin may grow to when the caller sets no cap. */
export const DEFAULT_PLUGIN_MEMORY_BYTES = 1024 * 1024 * 1024;

/** A plugin that was refused, did not load or failed, with the code every surface uses. */
export class PluginError extends Error {
  readonly code: string;

  constructor(code: string, message: string, options?: ErrorOptions) {
    super(message, options);
    this.name = "PluginError";
    this.code = code;
  }
}

/** A manifest's file entry. */
interface PinnedFile {
  readonly url: string;
  readonly sha256: string;
  readonly bytes: number;
}

interface Manifest {
  readonly id: string;
  readonly version: string;
  readonly abi: string;
  readonly kind: string;
  readonly artifacts: readonly (PinnedFile & {
    readonly target: string;
    readonly type: string;
  })[];
  readonly resources?: readonly (PinnedFile & { readonly name: string })[];
  readonly descriptor: ExtractorDescriptor;
}

/** Where {@link loadPlugin} reads a plugin from. */
export interface LoadPluginOptions {
  /** The most linear memory the module may have, in bytes. */
  readonly memoryBytes?: number;
  /** Fetches the manifest's files; `globalThis.fetch` when left out. */
  readonly fetch?: typeof fetch;
  readonly signal?: AbortSignal;
}

/** A loaded plugin: its manifest's identity and an extractor per run. */
export interface LoadedPlugin {
  readonly id: string;
  readonly version: string;
  /** SHA-256 of the artifact that runs, as `made_by.plugin` records it. */
  readonly artifactSha256: string;
  readonly descriptor: ExtractorDescriptor;
  /** An extractor that runs the module with `params`; each run instantiates it anew. */
  extractor(params?: Readonly<Record<string, unknown>>): Extractor;
}

async function sha256Hex(bytes: BufferSource): Promise<string> {
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
  return [...digest].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

async function fetchBytes(
  fetcher: typeof fetch,
  url: URL,
  signal?: AbortSignal,
): Promise<Uint8Array<ArrayBuffer>> {
  let response: Response;
  try {
    response = await fetcher(url, { signal });
  } catch (cause) {
    throw new PluginError(
      "PLUGIN_FETCH_FAILED",
      `${url.href}: ${String(cause)}`,
      {
        cause,
      },
    );
  }
  if (!response.ok)
    throw new PluginError(
      "PLUGIN_FETCH_FAILED",
      `${url.href}: HTTP ${response.status}`,
    );
  return new Uint8Array(await response.arrayBuffer());
}

async function verified(
  fetcher: typeof fetch,
  base: URL,
  file: PinnedFile,
  signal?: AbortSignal,
): Promise<Uint8Array<ArrayBuffer>> {
  const url = new URL(file.url, base);
  const bytes = await fetchBytes(fetcher, url, signal);
  if (bytes.byteLength !== file.bytes)
    throw new PluginError(
      "PLUGIN_HASH_MISMATCH",
      `${file.url} is ${bytes.byteLength} bytes; the manifest pins ${file.bytes}`,
    );
  const found = await sha256Hex(bytes);
  if (found !== file.sha256)
    throw new PluginError(
      "PLUGIN_HASH_MISMATCH",
      `${file.url} has SHA-256 ${found}; the manifest pins ${file.sha256}`,
    );
  return bytes;
}

const HOST_FUNCTIONS = new Set([
  "emit",
  "resource_size",
  "resource_read",
  "fail",
  "log",
]);
const EXPORTS = [
  "memory",
  "alpha2_abi",
  "alpha2_alloc",
  "alpha2_free",
  "alpha2_describe",
  "alpha2_begin",
  "alpha2_pass",
  "alpha2_video",
  "alpha2_audio",
  "alpha2_end",
];

/** Refuses a module importing anything but the host functions, or sharing its memory. */
function checkModule(
  module: WebAssembly.Module,
  bytes: Uint8Array<ArrayBuffer>,
): void {
  for (const entry of WebAssembly.Module.imports(module)) {
    if (
      entry.module !== "alpha2" ||
      !HOST_FUNCTIONS.has(entry.name) ||
      entry.kind !== "function"
    )
      throw new PluginError(
        "PLUGIN_UNSUPPORTED_FEATURE",
        `the module imports ${entry.module}.${entry.name}; plugins import only the alpha2 host functions`,
      );
  }
  const exported = new Set(
    WebAssembly.Module.exports(module).map((entry) => entry.name),
  );
  const missing = EXPORTS.filter((name) => !exported.has(name));
  if (missing.length > 0)
    throw new PluginError(
      "PLUGIN_LOAD_FAILED",
      `the module does not export ${missing.join(", ")}`,
    );
  if (sharesMemory(bytes))
    throw new PluginError(
      "PLUGIN_UNSUPPORTED_FEATURE",
      "the module's memory is shared, which plugins may not use",
    );
}

/** Whether the module defines a shared memory: a memory section entry with the shared flag. */
function sharesMemory(bytes: Uint8Array): boolean {
  let at = 8;
  const leb = (): number => {
    let value = 0;
    let shift = 0;
    for (;;) {
      const byte = bytes[at++] ?? 0;
      value += (byte & 0x7f) * 2 ** shift;
      if ((byte & 0x80) === 0) return value;
      shift += 7;
    }
  };
  while (at < bytes.length) {
    const id = bytes[at++];
    const size = leb();
    const end = at + size;
    if (id === 5) {
      const count = leb();
      for (let index = 0; index < count; index += 1) {
        const flags = leb();
        if ((flags & 0x02) !== 0) return true;
        leb();
        if ((flags & 0x01) !== 0) leb();
      }
    }
    at = end;
  }
  return false;
}

/** One record of an `emit` batch, as `docs/plugin-abi.md` lays it out. */
type PluginRecord =
  | {
      readonly kind: "sample";
      readonly output: number;
      readonly time: number;
      readonly values: (number | null)[];
    }
  | {
      readonly kind: "word";
      readonly output: number;
      readonly start: number;
      readonly end: number;
      readonly text: string;
      readonly speaker?: string;
    }
  | {
      readonly kind: "interval";
      readonly output: number;
      readonly start: number;
      readonly end: number;
      readonly label?: string;
      readonly score?: number;
    };

const ABSENT = 0xffffffff;

/** Reads a whole batch; throws a reason for the first record that does not read. */
export function decodeRecords(batch: Uint8Array): PluginRecord[] {
  const view = new DataView(batch.buffer, batch.byteOffset, batch.byteLength);
  const text = new TextDecoder("utf-8", { fatal: true });
  const records: PluginRecord[] = [];
  let at = 0;
  while (at < batch.byteLength) {
    const index = records.length;
    const fail = (reason: string): never => {
      throw new PluginError("INVALID_EMIT", `record ${index}: ${reason}`);
    };
    if (at + 8 > batch.byteLength) fail("the header ends early");
    const tag = view.getUint8(at);
    if (view.getUint8(at + 1) !== 0) fail("the reserved header byte is not 0");
    const output = view.getUint16(at + 2, true);
    const size = view.getUint32(at + 4, true);
    const start = at + 8;
    const end = start + size;
    if (end > batch.byteLength) fail("the body is shorter than its length");
    let cursor = start;
    const need = (count: number): number => {
      if (cursor + count > end) fail("the record's fields end early");
      const from = cursor;
      cursor += count;
      return from;
    };
    const i64 = (): number => Number(view.getBigInt64(need(8), true));
    const u32 = (): number => view.getUint32(need(4), true);
    const f32 = (): number => view.getFloat32(need(4), true);
    const string = (length: number): string | undefined => {
      if (length === ABSENT) return undefined;
      const from = need(length);
      try {
        return text.decode(batch.subarray(from, from + length));
      } catch {
        return fail("a text is not UTF-8");
      }
    };
    if (tag === 1) {
      const time = i64();
      const count = u32();
      if (count > (end - cursor) / 4) fail("the sample's fields end early");
      const masks = Array.from({ length: Math.ceil(count / 32) }, u32);
      const values = Array.from({ length: count }, (_, value) => {
        const read = f32();
        return ((masks[Math.floor(value / 32)] ?? 0) >>> (value % 32)) & 1
          ? null
          : read;
      });
      records.push({ kind: "sample", output, time, values });
    } else if (tag === 2) {
      const startTick = i64();
      const endTick = i64();
      const textLength = u32();
      const speakerLength = u32();
      const word = string(textLength) ?? fail("a word has no text");
      const speaker = string(speakerLength);
      records.push({
        kind: "word",
        output,
        start: startTick,
        end: endTick,
        text: word,
        ...(speaker === undefined ? {} : { speaker }),
      });
    } else if (tag === 3) {
      const startTick = i64();
      const endTick = i64();
      const flags = u32();
      const score = f32();
      const label = string(u32());
      if (flags > 1) fail("unknown interval flags");
      records.push({
        kind: "interval",
        output,
        start: startTick,
        end: endTick,
        ...(label === undefined ? {} : { label }),
        ...(flags === 1 ? { score } : {}),
      });
    } else fail("unknown record tag");
    if (cursor !== end) fail("the body is longer than its fields");
    at = end;
  }
  return records;
}

interface Exports {
  readonly memory: WebAssembly.Memory;
  alpha2_abi(): number;
  alpha2_alloc(size: number, align: number): number;
  alpha2_free(ptr: number, size: number, align: number): void;
  alpha2_describe(): bigint;
  alpha2_begin(ptr: number, len: number): number;
  alpha2_pass(pass: number, direction: number): number;
  alpha2_video(
    time: bigint,
    width: number,
    height: number,
    format: number,
    ptr: number,
    len: number,
  ): number;
  alpha2_audio(
    start: bigint,
    startSample: bigint,
    frames: number,
    rate: number,
    channels: number,
    ptr: number,
    count: number,
  ): number;
  alpha2_end(): number;
}

/**
 * One run of the module: an instance, its host functions and what they collected. Instantiated
 * asynchronously, since browsers refuse to instantiate large modules synchronously on the main
 * thread.
 */
class Run {
  #exports!: Exports;
  readonly #descriptor: ExtractorDescriptor;
  readonly #memoryBytes: number;
  #records: PluginRecord[] = [];
  #undecodable: PluginError | undefined;
  #failure: string | undefined;

  private constructor(descriptor: ExtractorDescriptor, memoryBytes: number) {
    this.#descriptor = descriptor;
    this.#memoryBytes = memoryBytes;
  }

  static async start(
    module: WebAssembly.Module,
    descriptor: ExtractorDescriptor,
    resources: ReadonlyMap<string, Uint8Array>,
    memoryBytes: number,
  ): Promise<Run> {
    const run = new Run(descriptor, memoryBytes);
    const instance = await WebAssembly.instantiate(
      module,
      run.#imports(resources),
    );
    run.#exports = instance.exports as unknown as Exports;
    if (run.#exports.alpha2_abi() !== 1)
      throw new PluginError(
        "PLUGIN_ABI_UNSUPPORTED",
        `the module implements alpha2-plugin/${run.#exports.alpha2_abi()}`,
      );
    return run;
  }

  #imports(resources: ReadonlyMap<string, Uint8Array>): WebAssembly.Imports {
    const bytes = (ptr: number, len: number): Uint8Array =>
      new Uint8Array(this.#exports.memory.buffer, ptr, len);
    const utf8 = (ptr: number, len: number): string =>
      new TextDecoder().decode(bytes(ptr, len));
    return {
      alpha2: {
        emit: (ptr: number, len: number): number => {
          try {
            this.#records.push(...decodeRecords(bytes(ptr, len).slice()));
            return 0;
          } catch (error) {
            this.#undecodable =
              error instanceof PluginError
                ? error
                : new PluginError("INVALID_EMIT", String(error));
            return 1;
          }
        },
        resource_size: (name: number, nameLength: number): bigint => {
          const found = resources.get(utf8(name, nameLength));
          return found === undefined ? -1n : BigInt(found.byteLength);
        },
        resource_read: (
          name: number,
          nameLength: number,
          dst: number,
          dstLength: number,
        ): number => {
          const found = resources.get(utf8(name, nameLength));
          if (found === undefined || found.byteLength !== dstLength) return 1;
          bytes(dst, dstLength).set(found);
          return 0;
        },
        fail: (ptr: number, len: number): void => {
          this.#failure = utf8(ptr, len);
        },
        log: (_level: number, ptr: number, len: number): void => {
          console.warn(`plugin: ${utf8(ptr, len).slice(0, 2048)}`);
        },
      },
    };
  }

  /** The descriptor JSON the module itself returns. */
  describe(): string {
    const packed = BigInt.asUintN(64, this.#exports.alpha2_describe());
    const ptr = Number(packed & 0xffffffffn);
    const len = Number(packed >> 32n);
    return new TextDecoder().decode(
      new Uint8Array(this.#exports.memory.buffer, ptr, len),
    );
  }

  /** Runs a call, turning its status, a trap or the memory cap into the run's outcome. */
  call(run: () => number): PluginRecord[] {
    let status: number;
    try {
      status = run();
    } catch (cause) {
      throw new PluginError(
        "PLUGIN_TRAPPED",
        `the plugin trapped: ${String(cause)}`,
        { cause },
      );
    }
    if (this.#exports.memory.buffer.byteLength > this.#memoryBytes)
      throw new PluginError(
        "PLUGIN_LIMIT",
        `the plugin needed more than ${this.#memoryBytes} bytes of memory`,
      );
    const records = this.#records;
    this.#records = [];
    if (status === 0) return records;
    if (status === 1) {
      if (this.#undecodable !== undefined) throw this.#undecodable;
      throw new PluginError(
        "EXTRACTOR_FAILED",
        this.#failure ?? "the plugin failed without saying why",
      );
    }
    throw new PluginError(
      "PLUGIN_TRAPPED",
      status === 2
        ? "the plugin panicked; it caught the panic at its boundary"
        : `the plugin returned the unknown status ${status}`,
    );
  }

  /** Copies `data` into memory the module allocates, calls `run` with it, and frees it. */
  withBytes(
    data: Uint8Array,
    align: number,
    run: (ptr: number) => number,
  ): PluginRecord[] {
    const ptr = this.#exports.alpha2_alloc(data.byteLength, align);
    if (ptr === 0 && data.byteLength > 0)
      throw new PluginError(
        "PLUGIN_LIMIT",
        "the plugin could not allocate the call's input",
      );
    new Uint8Array(this.#exports.memory.buffer, ptr, data.byteLength).set(data);
    const records = this.call(() => run(ptr));
    this.#exports.alpha2_free(ptr, data.byteLength, align);
    return records;
  }

  get exports(): Exports {
    return this.#exports;
  }

  /** Hands `records` to `out` by output name. */
  deliver(records: readonly PluginRecord[], out: ExtractEmitter): void {
    for (const record of records) {
      const output = this.#descriptor.outputs[record.output]?.name;
      if (output === undefined)
        throw new PluginError(
          "INVALID_EMIT",
          `the plugin emitted to output ${record.output}, which it does not declare`,
        );
      if (record.kind === "sample")
        out.sample(output, record.time, record.values);
      else if (record.kind === "word")
        out.word(output, record.start, record.end, record.text, record.speaker);
      else
        out.interval(
          output,
          record.start,
          record.end,
          record.label,
          record.score,
        );
    }
  }
}

function nothingEmitted(records: readonly PluginRecord[], step: string): void {
  if (records.length > 0)
    throw new PluginError(
      "INVALID_EMIT",
      `the plugin emitted during ${step}, which takes no emits`,
    );
}

/** The run context as the ABI's `begin` takes it. */
function abiContext(
  context: ExtractContext,
  params: Readonly<Record<string, unknown>>,
): string {
  const raw = context as unknown as Record<string, unknown>;
  return JSON.stringify({
    range: context.range,
    passes: context.passes,
    from: context.from,
    video: context.video,
    audio:
      context.audio === null
        ? null
        : {
            rate: context.audio.rate,
            channels: context.audio.channels,
            block_frames: context.audio.blockFrames,
          },
    frame_rate: raw.frameRate ?? null,
    params,
  });
}

/**
 * Loads the plugin whose manifest is at `manifest` (a URL, absolute or relative to the page) and
 * whose bytes have SHA-256 `sha256`, with its wasm artifact and resources, each checked against
 * the hash and length the manifest pins. Rejects with a {@link PluginError}: `PLUGIN_FETCH_FAILED`,
 * `PLUGIN_HASH_MISMATCH`, `PLUGIN_ABI_UNSUPPORTED`, `PLUGIN_NO_ARTIFACT` (a manifest without a
 * wasm artifact), `PLUGIN_UNSUPPORTED_FEATURE`, `PLUGIN_LOAD_FAILED` or
 * `PLUGIN_DESCRIPTOR_MISMATCH`.
 */
export async function loadPlugin(
  manifest: string | URL,
  sha256: string,
  options: LoadPluginOptions = {},
): Promise<LoadedPlugin> {
  const fetcher = options.fetch ?? globalThis.fetch.bind(globalThis);
  const base = new URL(manifest, globalThis.location?.href);
  const text = await fetchBytes(fetcher, base, options.signal);
  const found = await sha256Hex(text);
  if (found !== sha256.toLowerCase())
    throw new PluginError(
      "PLUGIN_HASH_MISMATCH",
      `the manifest has SHA-256 ${found}; the caller pins ${sha256}`,
    );
  let parsed: Manifest;
  try {
    parsed = JSON.parse(new TextDecoder().decode(text)) as Manifest;
  } catch (cause) {
    throw new PluginError("PLUGIN_LOAD_FAILED", "the manifest is not JSON", {
      cause,
    });
  }
  if (parsed.abi !== PLUGIN_ABI)
    throw new PluginError(
      "PLUGIN_ABI_UNSUPPORTED",
      `the plugin needs ABI ${String(parsed.abi)}; this build implements ${PLUGIN_ABI}`,
    );
  if (parsed.kind !== "extractor" || parsed.id !== parsed.descriptor?.id)
    throw new PluginError(
      "PLUGIN_LOAD_FAILED",
      "the manifest is not an extractor whose id is its descriptor's",
    );
  const artifact = parsed.artifacts.find(
    (entry) => entry.target === "wasm32" && entry.type === "wasm",
  );
  if (artifact === undefined)
    throw new PluginError(
      "PLUGIN_NO_ARTIFACT",
      `${parsed.id} has no wasm artifact; it offers ${parsed.artifacts.map((entry) => `${entry.target} (${entry.type})`).join(", ")}`,
    );
  const bytes = await verified(fetcher, base, artifact, options.signal);
  const resources = new Map<string, Uint8Array<ArrayBuffer>>();
  for (const resource of parsed.resources ?? [])
    resources.set(
      resource.name,
      await verified(fetcher, base, resource, options.signal),
    );
  let module: WebAssembly.Module;
  try {
    module = await WebAssembly.compile(bytes);
  } catch (cause) {
    throw new PluginError(
      "PLUGIN_LOAD_FAILED",
      `the module is not valid: ${String(cause)}`,
      { cause },
    );
  }
  checkModule(module, bytes);
  const memoryBytes = options.memoryBytes ?? DEFAULT_PLUGIN_MEMORY_BYTES;
  const described = (
    await Run.start(module, parsed.descriptor, resources, memoryBytes)
  ).describe();
  if (!sameJson(JSON.parse(described), parsed.descriptor))
    throw new PluginError(
      "PLUGIN_DESCRIPTOR_MISMATCH",
      `the artifact ${artifact.url} describes another extractor than the manifest`,
    );
  const descriptor = parsed.descriptor;
  return {
    id: parsed.id,
    version: parsed.version,
    artifactSha256: artifact.sha256,
    descriptor,
    extractor(params = {}) {
      let run: Run | undefined;
      const current = (): Run => {
        if (run === undefined)
          throw new PluginError("PLUGIN_LOAD_FAILED", "the run has not begun");
        return run;
      };
      return {
        describe: () => descriptor,
        async begin(context: ExtractContext) {
          run = await Run.start(module, descriptor, resources, memoryBytes);
          const live = run;
          const json = new TextEncoder().encode(abiContext(context, params));
          const records = live.withBytes(json, 1, (ptr) =>
            live.exports.alpha2_begin(ptr, json.byteLength),
          );
          nothingEmitted(records, "begin");
        },
        pass({ pass, direction }) {
          const live = current();
          nothingEmitted(
            live.call(() =>
              live.exports.alpha2_pass(pass, direction === "backward" ? 1 : 0),
            ),
            "a pass start",
          );
        },
        video(frame: ExtractFrame, out: ExtractEmitter) {
          const live = current();
          live.deliver(
            live.withBytes(frame.data, 1, (ptr) =>
              live.exports.alpha2_video(
                BigInt(frame.time),
                frame.width,
                frame.height,
                frame.format === "luma8" ? 1 : 0,
                ptr,
                frame.data.byteLength,
              ),
            ),
            out,
          );
        },
        audio(block: ExtractAudio, out: ExtractEmitter) {
          const live = current();
          const data = new Uint8Array(
            block.samples.buffer,
            block.samples.byteOffset,
            block.samples.byteLength,
          );
          live.deliver(
            live.withBytes(data, 4, (ptr) =>
              live.exports.alpha2_audio(
                BigInt(block.start),
                BigInt(block.startSample),
                block.frames,
                block.rate,
                block.channels,
                ptr,
                block.samples.length,
              ),
            ),
            out,
          );
        },
        end(out: ExtractEmitter) {
          const live = current();
          live.deliver(
            live.call(() => live.exports.alpha2_end()),
            out,
          );
          run = undefined;
        },
      };
    },
  };
}

function sameJson(a: unknown, b: unknown): boolean {
  if (Array.isArray(a) && Array.isArray(b))
    return (
      a.length === b.length &&
      a.every((item, index) => sameJson(item, b[index]))
    );
  if (
    a !== null &&
    b !== null &&
    typeof a === "object" &&
    typeof b === "object"
  ) {
    const left = a as Record<string, unknown>;
    const right = b as Record<string, unknown>;
    const keys = Object.keys(left);
    return (
      keys.length === Object.keys(right).length &&
      keys.every((key) => key in right && sameJson(left[key], right[key]))
    );
  }
  return a === b;
}

/**
 * The compose compiler and its templates in the browser: a spec of intent (sequences, boxes,
 * groups, mattes, shapes, components) compiled to a project, and templates bound to their data.
 * Every function runs the engine's own code through the wasm binding, so a spec compiles to the
 * same bytes here as in Python or on a server.
 */

import { WasmModule, type WasmBindgenGlue } from "./wasm-module.js";

/** The binding functions compose uses: {@link loadCompose} returns them. */
export interface ComposeBinding {
  composeRequirements(spec: string): string;
  composeCompile(spec: string, facts: string, fonts: Uint8Array[]): string;
  composeBind(document: string, data: string, options: string): string;
  composeDataSchema(template: string): string;
  composeLint(template: string, data: string, options: string): string;
  composeTemplates(): string;
}

const composeModule = new WasmModule<ComposeBinding & WasmBindgenGlue>(
  () => import("alpha2/compose-wasm"),
);

/**
 * The compose module, a wasm module of its own (`alpha2/compose-wasm`) so pages that only play
 * and export projects never fetch it. `wasmUrl` locates its `.wasm` when a bundler moved it.
 *
 * @throws When the module cannot be fetched, compiled or instantiated.
 */
export function loadCompose(wasmUrl?: string | URL): Promise<ComposeBinding> {
  return composeModule.load(wasmUrl);
}

/** A compose spec or template: plain JSON data. */
export type ComposeDocument = Readonly<Record<string, unknown>>;

/** The files a spec names: media to gather facts about, and caption texts to read. */
export interface ComposeRequirements {
  readonly media: readonly {
    readonly path: string;
    readonly kind: "video" | "image" | "audio" | null;
  }[];
  readonly texts: readonly string[];
}

/** Facts about a spec's files, keyed by path, as `compose` reads them. */
export interface ComposeFacts {
  readonly media?: Readonly<
    Record<
      string,
      {
        readonly width?: number;
        readonly height?: number;
        readonly frame_rate?: { readonly num: number; readonly den: number };
        readonly duration_ticks?: number;
        readonly has_audio?: boolean;
      }
    >
  >;
  readonly texts?: Readonly<Record<string, string>>;
}

/** A compiled spec: the project, which file each media source reads, and notes. */
export interface CompiledCompose {
  readonly project: Readonly<Record<string, unknown>>;
  readonly assets: Readonly<Record<string, string>>;
  readonly loudness: unknown;
  readonly warnings: readonly string[];
}

/** A problem `lintTemplate` finds. */
export interface TemplateIssue {
  readonly path: string;
  readonly code: string;
  readonly message: string;
  readonly severity: "error" | "warning";
}

/** What binding takes besides the template and its data. */
export interface BindOptions {
  /** A theme for a video template, replacing the one it names. */
  readonly theme?: string | Readonly<Record<string, unknown>>;
  /** Node templates the document names, besides the built-in `alpha2/` ones. */
  readonly templates?: readonly ComposeDocument[];
}

/**
 * The files `spec` names, to gather facts about before compiling.
 *
 * @throws The binding's `INVALID_INPUT` error, its message `path: problem`, for a spec that
 * does not parse.
 */
export function composeRequirements(
  binding: ComposeBinding,
  spec: ComposeDocument,
): ComposeRequirements {
  return JSON.parse(
    binding.composeRequirements(JSON.stringify(spec)),
  ) as ComposeRequirements;
}

/**
 * Compiles `spec` with facts about its files and the fonts its text is measured with. Built-in
 * `alpha2/` template nodes expand; bind other templates with {@link bindTemplate} first.
 *
 * @throws The binding's `INVALID_INPUT` error, its message `path: problem`.
 */
export function compileCompose(
  binding: ComposeBinding,
  spec: ComposeDocument,
  options: {
    readonly facts?: ComposeFacts;
    readonly fonts?: Uint8Array[];
  } = {},
): CompiledCompose {
  return JSON.parse(
    binding.composeCompile(
      JSON.stringify(spec),
      JSON.stringify(options.facts ?? {}),
      options.fonts ?? [],
    ),
  ) as CompiledCompose;
}

/**
 * `template` bound to `data`: its parameters replaced and its template nodes expanded, giving a
 * plain spec (or, for a node template, a node). A spec without a `template` header only has
 * its template nodes expanded.
 *
 * @throws The binding's `INVALID_INPUT` error, naming where the template or data does not bind.
 */
export function bindTemplate(
  binding: ComposeBinding,
  template: ComposeDocument,
  data: Readonly<Record<string, unknown>> = {},
  options: BindOptions = {},
): Record<string, unknown> {
  return JSON.parse(
    binding.composeBind(
      JSON.stringify(template),
      JSON.stringify(data),
      JSON.stringify(options),
    ),
  ) as Record<string, unknown>;
}

/**
 * The JSON Schema of `template`'s data: what a form, an API or a model fills in.
 *
 * @throws The binding's `INVALID_INPUT` error for a template that does not parse.
 */
export function templateDataSchema(
  binding: ComposeBinding,
  template: ComposeDocument,
): Record<string, unknown> {
  return JSON.parse(
    binding.composeDataSchema(JSON.stringify(template)),
  ) as Record<string, unknown>;
}

/**
 * `template`'s problems: unknown or unused parameters, literal files, colors and words a
 * template takes from its data and theme, and whether it binds (to `data`, or to sample data)
 * into a spec that parses.
 */
export function lintTemplate(
  binding: ComposeBinding,
  template: ComposeDocument,
  data?: Readonly<Record<string, unknown>>,
  options: Pick<BindOptions, "templates"> = {},
): TemplateIssue[] {
  return JSON.parse(
    binding.composeLint(
      JSON.stringify(template),
      data === undefined ? "" : JSON.stringify(data),
      JSON.stringify(options),
    ),
  ) as TemplateIssue[];
}

/** The built-in `alpha2/` templates. */
export function builtinTemplates(
  binding: ComposeBinding,
): Record<string, unknown>[] {
  return JSON.parse(binding.composeTemplates()) as Record<string, unknown>[];
}

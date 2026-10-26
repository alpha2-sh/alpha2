#!/usr/bin/env node
// Generates the TypeScript project types from the published JSON Schema.
//
//   node scripts/generate-project-types.mjs [--check]
//
// Reads `schema/project.v1.schema.json` and `schema/project-limits.v1.json` (written by
// `cargo run -p alpha2-model --features schema --example emit-schema`) and writes
// `packages/alpha2/src/generated/project-schema.ts` and `project-shape.ts`, and the Python package's
// `python/alpha2/src/alpha2/types.py` (scripts/lib/python-project-types.mjs). With `--check` it
// writes nothing and exits non-zero when a committed file is stale.
//
// Unsupported schema constructs are rejected rather than emitted as incorrect types.

import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { format, resolveConfig } from "prettier";

import { pythonProjectTypes } from "./lib/python-project-types.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const schemaPath = join(root, "schema/project.v1.schema.json");
const limitsPath = join(root, "schema/project-limits.v1.json");
const outPath = join(root, "packages/alpha2/src/generated/project-schema.ts");
const shapePath = join(root, "packages/alpha2/src/generated/project-shape.ts");
const pythonPath = join(root, "python/alpha2/src/alpha2/types.py");

/**
 * Rust generic types. schemars names an instance `Name_Arg` (see the
 * `schemars(rename = "Name_{T}")` attributes in alpha2-model); each becomes one TypeScript generic
 * `Name<T>`.
 */
const GENERICS = new Set(["Animated", "Keyframe"]);

/** schemars' names for primitive generic arguments, by their `format`. */
const PRIMITIVE_ARGUMENTS = new Map([
  ["float", "number"],
  ["double", "number"],
  ["int32", "number"],
  ["int64", "number"],
  ["uint32", "number"],
  ["uint64", "number"],
  ["string", "string"],
]);

const schema = JSON.parse(readFileSync(schemaPath, "utf8"));
const limits = JSON.parse(readFileSync(limitsPath, "utf8"));
const defs = schema.$defs ?? {};

function refName(ref) {
  const prefix = "#/$defs/";
  if (!ref.startsWith(prefix)) throw new Error(`unsupported $ref ${ref}`);
  return ref.slice(prefix.length);
}

/** Splits `Animated_Vec2` into `["Animated", "Vec2"]` for a known generic. */
function genericInstance(name) {
  const split = name.indexOf("_");
  if (split < 0) return undefined;
  const base = name.slice(0, split);
  return GENERICS.has(base) ? [base, name.slice(split + 1)] : undefined;
}

function argumentType(argument) {
  if (argument in defs) return argument;
  const primitive = PRIMITIVE_ARGUMENTS.get(argument);
  if (primitive === undefined) {
    throw new Error(`unknown generic argument ${argument}`);
  }
  return primitive;
}

/** Rustdoc links and paths read as code spans in TSDoc. */
function docText(text) {
  return text
    .replace(/\[`([^`]+)`\](\([^)]*\))?/g, "`$1`")
    .replace(/`(?:crate|Self)::/g, "`")
    .trim();
}

function docComment(lines, indent = "") {
  const body = lines
    .filter((line) => line !== undefined)
    .join("\n\n")
    .split("\n");
  if (body.length === 0 || (body.length === 1 && body[0] === "")) return "";
  return (
    `${indent}/**\n` +
    body.map((line) => `${indent} *${line ? ` ${line}` : ""}`).join("\n") +
    `\n${indent} */\n`
  );
}

function schemaDoc(node) {
  const lines = [];
  if (typeof node.description === "string") {
    lines.push(docText(node.description));
  }
  if (node.default !== undefined) {
    const value = JSON.stringify(node.default);
    if (value.length <= 80) lines.push(`@defaultValue \`${value}\``);
  }
  return lines;
}

/**
 * A union member's description, moved onto its tag property: TypeScript has no place for a comment
 * on a union member that editors show.
 */
function documentedTag(variant) {
  if (typeof variant.description !== "string" || variant.type !== "object") {
    return variant;
  }
  const properties = { ...variant.properties };
  const tag = Object.keys(properties).find(
    (name) =>
      properties[name].const !== undefined &&
      properties[name].description === undefined,
  );
  if (tag === undefined) return variant;
  properties[tag] = { ...properties[tag], description: variant.description };
  return { ...variant, properties };
}

function isIdentifier(name) {
  return /^[A-Za-z_$][A-Za-z0-9_$]*$/.test(name);
}

/**
 * Renders one schema as a TypeScript type expression.
 *
 * `generic` is set while rendering a generic's body: `{ argument }` names the instance's argument,
 * which renders as `T`.
 */
function render(node, generic) {
  if (node === true) return "unknown";
  if (typeof node !== "object" || node === null) {
    throw new Error(`unsupported schema ${JSON.stringify(node)}`);
  }
  if (node.$ref !== undefined) {
    const name = refName(node.$ref);
    if (generic !== undefined && name === generic.argument) return "T";
    const instance = genericInstance(name);
    if (instance !== undefined) {
      const [base, argument] = instance;
      if (generic !== undefined && argument === generic.argument) {
        return `${base}<T>`;
      }
      return `${base}<${argumentType(argument)}>`;
    }
    if (!(name in defs)) throw new Error(`dangling $ref ${node.$ref}`);
    return name;
  }
  if (node.const !== undefined) return JSON.stringify(node.const);
  if (Array.isArray(node.enum)) {
    return node.enum.map((value) => JSON.stringify(value)).join(" | ");
  }
  const variants = node.oneOf ?? node.anyOf;
  if (Array.isArray(variants)) {
    return variants
      .map((variant) => `\n| ${render(documentedTag(variant), generic)}`)
      .join("");
  }
  if (Array.isArray(node.type)) {
    return node.type
      .map((type) => render({ ...node, type }, generic))
      .join(" | ");
  }
  if (
    generic !== undefined &&
    !(generic.argument in defs) &&
    (node.format === generic.argument ||
      (generic.argument === "string" &&
        node.type === "string" &&
        node.format === undefined))
  ) {
    return "T";
  }
  switch (node.type) {
    case "null":
      return "null";
    case "boolean":
      return "boolean";
    case "string":
      return "string";
    case "integer":
    case "number":
      return "number";
    case "array":
      return renderArray(node, generic);
    case "object":
      return renderObject(node, generic);
    default:
      throw new Error(`unsupported schema ${JSON.stringify(node)}`);
  }
}

function renderArray(node, generic) {
  if (Array.isArray(node.prefixItems)) {
    return `readonly [${node.prefixItems.map((item) => render(item, generic)).join(", ")}]`;
  }
  const item = render(node.items ?? true, generic);
  if (
    Number.isInteger(node.minItems) &&
    node.minItems === node.maxItems &&
    node.minItems <= 16
  ) {
    return `readonly [${Array(node.minItems).fill(item).join(", ")}]`;
  }
  return `readonly (${item})[]`;
}

function renderObject(node, generic) {
  const properties = node.properties ?? {};
  const required = new Set(node.required ?? []);
  const names = Object.keys(properties);
  if (names.length === 0) {
    const values = node.additionalProperties;
    if (typeof values === "object" && values !== null) {
      return `Readonly<Record<string, ${render(values, generic)}>>`;
    }
    return "Readonly<Record<string, never>>";
  }
  if (node.additionalProperties !== false) {
    throw new Error(
      `object with properties must reject unknown fields: ${names.join(", ")}`,
    );
  }
  const members = names.map((name) => {
    const property = properties[name];
    const key = isIdentifier(name) ? name : JSON.stringify(name);
    const optional = required.has(name) ? "" : "?";
    return `${docComment(schemaDoc(property), "  ")}  readonly ${key}${optional}: ${render(property, generic)};`;
  });
  return `{\n${members.join("\n")}\n}`;
}

function declaration(name, node, generic) {
  const doc = docComment(schemaDoc({ description: node.description }));
  const typeName = generic === undefined ? name : `${name}<T>`;
  if (node.type === "object" && node.properties !== undefined) {
    return `${doc}export interface ${typeName} ${renderObject(node, generic)}\n`;
  }
  return `${doc}export type ${typeName} = ${render(node, generic)};\n`;
}

const declarations = [];

// Each generic is declared once, from an instance whose argument is a named type; every other
// instance must render identically.
const instances = new Map();
for (const name of Object.keys(defs)) {
  const instance = genericInstance(name);
  if (instance === undefined) continue;
  const [base, argument] = instance;
  if (!instances.has(base)) instances.set(base, []);
  instances.get(base).push({ name, argument });
}
const genericDeclarations = new Map();
for (const [base, list] of instances) {
  const template = list.find(({ argument }) => argument in defs);
  if (template === undefined) {
    throw new Error(`generic ${base} has no instance with a named argument`);
  }
  const text = declaration(base, defs[template.name], {
    argument: template.argument,
  });
  for (const { name, argument } of list) {
    const other = declaration(base, defs[name], { argument });
    if (other !== text) {
      throw new Error(`${name} is not an instance of ${base}<T>:\n${other}`);
    }
  }
  genericDeclarations.set(base, text);
}

const rootDescription = schema.description;
declarations.push(
  declaration("Project", { ...schema, description: rootDescription }),
);
const ordered = [
  ...Object.keys(defs).filter((name) => genericInstance(name) === undefined),
  ...genericDeclarations.keys(),
].sort();
for (const name of ordered) {
  declarations.push(
    genericDeclarations.get(name) ?? declaration(name, defs[name]),
  );
}

const constants = Object.entries(limits).map(
  ([name, value]) =>
    `/** Mirrors \`alpha2_model::${name}\`. */\nexport const ${name} = ${JSON.stringify(value)};\n`,
);

/** The string values of an enumeration, or `undefined` for any other def. */
function enumValues(node) {
  if (
    Array.isArray(node.enum) &&
    node.enum.every((v) => typeof v === "string")
  ) {
    return node.enum;
  }
  const variants = node.oneOf ?? node.anyOf;
  if (
    Array.isArray(variants) &&
    variants.every((variant) => typeof variant.const === "string")
  ) {
    return variants.map((variant) => variant.const);
  }
  return undefined;
}

const upperSnake = (name) =>
  name.replace(/([a-z0-9])([A-Z])/g, "$1_$2").toUpperCase();

// Every string enumeration also exists at run time, in the model's order, so an editor fills its
// menus from the same source as the types.
const enumerations = Object.keys(defs)
  .sort()
  .flatMap((name) => {
    const values = enumValues(defs[name]);
    if (values === undefined) return [];
    return [
      `/** Every {@link ${name}}, in the model's order. */\n` +
        `export const ${upperSnake(name)}_VALUES: readonly ${name}[] = ${JSON.stringify(values)};\n`,
    ];
  });

const source = [
  "// Generated by scripts/generate-project-types.mjs from\n" +
    "// schema/project.v1.schema.json and schema/project-limits.v1.json.\n" +
    "// Do not edit; change the Rust model and run `npm run generate:schema`.\n",
  ...constants,
  ...enumerations,
  ...declarations,
].join("\n");

/** Schema keywords that decide whether a document has the right shape. */
const SHAPE_KEYWORDS = new Set([
  "$ref",
  "$defs",
  "type",
  "properties",
  "required",
  "additionalProperties",
  "enum",
  "const",
  "oneOf",
  "anyOf",
  "items",
  "prefixItems",
  "minItems",
  "maxItems",
  "minimum",
  "maximum",
  "minLength",
  "pattern",
]);

/** Number formats `validateProject` reads as the engine does (`INTEGER_FORMATS` and `float`). */
const SHAPE_FORMATS = new Set(["float", "uint32", "uint64", "int64"]);

/**
 * The schema without its prose: what `validateProject` walks at run time, so the package carries
 * the shape rules without the documentation.
 */
function shapeOnly(node, key) {
  if (Array.isArray(node)) return node.map((item) => shapeOnly(item));
  if (typeof node !== "object" || node === null) return node;
  const kept = {};
  for (const [name, value] of Object.entries(node)) {
    // Under `properties` and `$defs` the keys are names, not keywords.
    if (key === "properties" || key === "$defs") {
      kept[name] = shapeOnly(value);
    } else if (SHAPE_KEYWORDS.has(name)) {
      kept[name] = shapeOnly(value, name);
    } else if (name === "format") {
      // Read as the engine's Rust type: a float rounded to 32 bits, an integer within its range.
      if (!SHAPE_FORMATS.has(value)) {
        throw new Error(`unsupported schema format ${value}`);
      }
      kept[name] = value;
    } else if (
      name !== "description" &&
      name !== "default" &&
      name !== "title" &&
      name !== "$schema"
    ) {
      throw new Error(`unsupported schema keyword ${name}`);
    }
  }
  return kept;
}

const shapeSource =
  "// Generated by scripts/generate-project-types.mjs from\n" +
  "// schema/project.v1.schema.json. Do not edit.\n\n" +
  "/** The project schema's shape rules, without its documentation. */\n" +
  `export const PROJECT_SHAPE: unknown = ${JSON.stringify(shapeOnly(schema))};\n`;

const outputs = [
  [outPath, source],
  [shapePath, shapeSource],
  [pythonPath, pythonProjectTypes(schema, limits)],
];
let stale = false;
for (const [path, text] of outputs) {
  // Prettier formats the TypeScript; the Python is written formatted.
  const formatted = path.endsWith(".py")
    ? text
    : await format(text, {
        ...(await resolveConfig(path)),
        filepath: path,
      });
  if (process.argv.includes("--check")) {
    let committed = "";
    try {
      committed = readFileSync(path, "utf8");
    } catch {
      // Reported as stale below.
    }
    if (committed !== formatted) {
      console.error(
        `${path} is stale; run \`npm run generate:schema\` and commit the result`,
      );
      stale = true;
    }
  } else {
    mkdirSync(dirname(path), { recursive: true });
    writeFileSync(path, formatted);
  }
}
if (stale) process.exit(1);

// Renders the project schema as Python `TypedDict`s for the `alpha2` Python package
// (`python/alpha2/src/alpha2/types.py`). scripts/generate-project-types.mjs calls it beside the
// TypeScript output, so both read the schema the same way and `--check` covers both.
//
// A JSON object becomes a `TypedDict`, an optional field `NotRequired`, a union a `TypeAlias`, a
// string enumeration a `Literal` with a `*_VALUES` tuple. A union member written inline is a class
// of its own, named after its union and its tag (`Easing` + `cubic_bezier` is `EasingCubicBezier`).
// Unsupported schema constructs are rejected rather than emitted as incorrect types.

const PYTHON_KEYWORDS = new Set([
  "False",
  "None",
  "True",
  "and",
  "as",
  "assert",
  "async",
  "await",
  "break",
  "class",
  "continue",
  "def",
  "del",
  "elif",
  "else",
  "except",
  "finally",
  "for",
  "from",
  "global",
  "if",
  "import",
  "in",
  "is",
  "lambda",
  "nonlocal",
  "not",
  "or",
  "pass",
  "raise",
  "return",
  "try",
  "while",
  "with",
  "yield",
]);

/** The names the generated module imports from `typing`, which no type may take. */
const TYPING_NAMES = [
  "Generic",
  "Literal",
  "Never",
  "NotRequired",
  "TypeAlias",
  "TypeVar",
  "TypedDict",
];

/** schemars' names for primitive generic arguments. */
const PRIMITIVE_ARGUMENTS = new Map([
  ["float", "float"],
  ["double", "float"],
  ["int32", "int"],
  ["int64", "int"],
  ["uint32", "int"],
  ["uint64", "int"],
  ["string", "str"],
]);

/** Rust generic types, as `generate-project-types.mjs` names them. */
const GENERICS = new Set(["Animated", "Keyframe"]);

function pascal(text) {
  return text
    .split(/[^A-Za-z0-9]+/)
    .filter(Boolean)
    .map((word) => word[0].toUpperCase() + word.slice(1))
    .join("");
}

function docText(text) {
  return text
    .replace(/\[`([^`]+)`\](\([^)]*\))?/g, "`$1`")
    .replace(/`(?:crate|Self)::/g, "`")
    .trim();
}

/** A docstring at `indent`, or nothing for no text. */
function docstring(lines, indent) {
  const text = lines.filter((line) => line !== undefined).join("\n\n");
  if (text === "") return "";
  const escaped = text.replace(/\\/g, "\\\\").replace(/"""/g, '\\"\\"\\"');
  const body = escaped
    .split("\n")
    .map((line, index) => (index === 0 || line === "" ? line : indent + line))
    .join("\n");
  const closing =
    body.includes("\n") || body.endsWith('"') ? `\n${indent}` : "";
  return `${indent}"""${body}${closing}"""\n`;
}

function schemaDoc(node) {
  const lines = [];
  if (typeof node.description === "string")
    lines.push(docText(node.description));
  if (node.default !== undefined) {
    const value = JSON.stringify(node.default);
    if (value.length <= 80) lines.push(`Default: \`${value}\`.`);
  }
  return lines;
}

function refName(ref) {
  const prefix = "#/$defs/";
  if (!ref.startsWith(prefix)) throw new Error(`unsupported $ref ${ref}`);
  return ref.slice(prefix.length);
}

/**
 * The Python type declarations of `schema` and the constants of `limits`, as the text of a module.
 */
export function pythonProjectTypes(schema, limits) {
  const defs = schema.$defs ?? {};
  for (const name of Object.keys(defs)) {
    if (TYPING_NAMES.includes(name))
      throw new Error(`schema type ${name} shadows typing.${name}`);
  }

  const genericInstance = (name) => {
    const split = name.indexOf("_");
    if (split < 0) return undefined;
    const base = name.slice(0, split);
    return GENERICS.has(base) ? [base, name.slice(split + 1)] : undefined;
  };
  const argumentType = (argument) => {
    if (argument in defs) return argument;
    const primitive = PRIMITIVE_ARGUMENTS.get(argument);
    if (primitive === undefined)
      throw new Error(`unknown generic argument ${argument}`);
    return primitive;
  };

  /**
   * Renders declarations into `out`. `context` carries the name inline members are named after,
   * the generic argument being rendered as `T` (if any), and whether `T` was used.
   */
  function renderer(out, generic) {
    const state = { usesT: false };

    function type(node, owner) {
      if (node === true) return "object";
      if (typeof node !== "object" || node === null)
        throw new Error(`unsupported schema ${JSON.stringify(node)}`);
      if (node.$ref !== undefined) {
        const name = refName(node.$ref);
        if (generic !== undefined && name === generic.argument) {
          state.usesT = true;
          return "T";
        }
        const instance = genericInstance(name);
        if (instance !== undefined) {
          const [base, argument] = instance;
          if (generic !== undefined && argument === generic.argument) {
            state.usesT = true;
            return `${base}[T]`;
          }
          return `${base}[${argumentType(argument)}]`;
        }
        if (!(name in defs)) throw new Error(`dangling $ref ${node.$ref}`);
        return name;
      }
      if (node.const !== undefined)
        return `Literal[${JSON.stringify(node.const)}]`;
      if (Array.isArray(node.enum)) {
        if (!node.enum.every((value) => typeof value === "string"))
          throw new Error(`unsupported enum ${JSON.stringify(node.enum)}`);
        return `Literal[${node.enum.map((value) => JSON.stringify(value)).join(", ")}]`;
      }
      const variants = node.oneOf ?? node.anyOf;
      if (Array.isArray(variants)) {
        return union(
          variants.map((variant, index) => member(variant, owner, index)),
        );
      }
      if (Array.isArray(node.type)) {
        return union(
          node.type.map((kind) => type({ ...node, type: kind }, owner)),
        );
      }
      if (
        generic !== undefined &&
        !(generic.argument in defs) &&
        (node.format === generic.argument ||
          (generic.argument === "string" &&
            node.type === "string" &&
            node.format === undefined))
      ) {
        state.usesT = true;
        return "T";
      }
      switch (node.type) {
        case "null":
          return "None";
        case "boolean":
          return "bool";
        case "string":
          return "str";
        case "integer":
          return "int";
        case "number":
          return "float";
        case "array":
          if (Array.isArray(node.prefixItems))
            return `tuple[${node.prefixItems.map((item) => type(item, `${owner}Item`)).join(", ")}]`;
          return `list[${type(node.items ?? true, `${owner}Item`)}]`;
        case "object":
          return object(node, owner);
        default:
          throw new Error(`unsupported schema ${JSON.stringify(node)}`);
      }
    }

    /** A union member: inline objects become classes named after the union and their tag. */
    function member(variant, owner, index) {
      if (variant.type !== "object" || variant.properties === undefined)
        return type(variant, owner);
      const tag = Object.entries(variant.properties).find(
        ([, property]) => typeof property.const === "string",
      );
      const name = owner + (tag ? pascal(tag[1].const) : `Variant${index + 1}`);
      if (name in defs)
        throw new Error(
          `inline type ${name} collides with schema type ${name}`,
        );
      return declareClass(name, variant);
    }

    function object(node, owner) {
      const properties = node.properties ?? {};
      if (Object.keys(properties).length === 0) {
        const values = node.additionalProperties;
        if (typeof values === "object" && values !== null)
          return `dict[str, ${type(values, `${owner}Value`)}]`;
        return "dict[str, Never]";
      }
      if (owner in defs)
        throw new Error(
          `inline type ${owner} collides with schema type ${owner}`,
        );
      return declareClass(owner, node);
    }

    /** Declares `name` as a TypedDict of `node`'s properties; returns how to refer to it. */
    function declareClass(name, node) {
      const properties = node.properties ?? {};
      if (node.additionalProperties !== false) {
        throw new Error(
          `object with properties must reject unknown fields: ${Object.keys(properties).join(", ")}`,
        );
      }
      const required = new Set(node.required ?? []);
      const inner = renderer(out, generic);
      const fields = Object.entries(properties).map(([field, property]) => {
        const rendered = inner.type(property, name + pascal(field));
        return {
          field,
          annotation: required.has(field)
            ? rendered
            : `NotRequired[${rendered}]`,
          doc: schemaDoc(property),
        };
      });
      const usesT = inner.state.usesT;
      if (usesT) state.usesT = true;
      const head = usesT ? "TypedDict, Generic[T]" : "TypedDict";
      const doc = docstring(
        schemaDoc({ description: node.description }),
        "    ",
      );
      let text;
      if (
        fields.some(
          ({ field }) =>
            PYTHON_KEYWORDS.has(field) ||
            !/^[A-Za-z_][A-Za-z0-9_]*$/.test(field),
        )
      ) {
        if (usesT)
          throw new Error(
            `${name} needs the functional TypedDict syntax, which is not generic`,
          );
        // A field named as a Python keyword (`from`) cannot be written in a class body.
        const entries = fields
          .map(
            ({ field, annotation }) =>
              `        ${JSON.stringify(field)}: ${annotation},`,
          )
          .join("\n");
        text =
          `${name} = TypedDict(\n    ${JSON.stringify(name)},\n    {\n${entries}\n    },\n)\n` +
          (doc
            ? `${name}.__doc__ = ${JSON.stringify(docText(node.description ?? ""))}\n`
            : "");
      } else {
        const body = fields
          .map(
            ({ field, annotation, doc: lines }) =>
              `    ${field}: ${annotation}\n${docstring(lines, "    ")}`,
          )
          .join("");
        text = `class ${name}(${head}):\n${doc}${doc ? "\n" : ""}${body}`;
      }
      out.declare(name, text);
      return usesT ? `${name}[T]` : name;
    }

    function union(members) {
      const unique = [...new Set(members)];
      const literals = unique.filter((text) => text.startsWith("Literal["));
      const others = unique.filter((text) => !text.startsWith("Literal["));
      // Literal["a"] | Literal["b"] reads as Literal["a", "b"].
      const merged =
        literals.length > 1
          ? [`Literal[${literals.map((text) => text.slice(8, -1)).join(", ")}]`]
          : literals;
      return [...merged, ...others].join(" | ");
    }

    return { type, declareClass, state };
  }

  /** Declarations by name, with their text, in the order they are made. */
  function collector() {
    const declared = new Map();
    return {
      declared,
      declare(name, text) {
        const existing = declared.get(name);
        if (existing !== undefined && existing !== text)
          throw new Error(`two different types are both named ${name}`);
        declared.set(name, text);
      },
    };
  }

  /** Declares def `name` (the generic `base` for a generic instance) into `out`. */
  function declareDef(out, name, node, generic) {
    const r = renderer(out, generic);
    const doc = schemaDoc({ description: node.description });
    if (node.type === "object" && node.properties !== undefined) {
      r.declareClass(name, node);
      return;
    }
    const expression = r.type(node, name);
    const typeAlias = `${name}: TypeAlias = ${quoteAlias(expression)}\n${docstring(doc, "")}`;
    out.declare(name, typeAlias);
  }

  /**
   * An alias's expression: a string, so it may name types declared further down; a bare literal
   * or builtin needs no quotes.
   */
  function quoteAlias(expression) {
    if (/^(Literal\[[^\]]*\]|int|float|str|bool)$/.test(expression))
      return expression;
    return JSON.stringify(expression);
  }

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
  const genericTexts = new Map();
  for (const [base, list] of instances) {
    const template = list.find(({ argument }) => argument in defs);
    if (template === undefined)
      throw new Error(`generic ${base} has no instance with a named argument`);
    const render = ({ name, argument }) => {
      const out = collector();
      declareDef(out, base, defs[name], { argument });
      return out.declared;
    };
    const expected = render(template);
    for (const instance of list) {
      const other = render(instance);
      const same =
        other.size === expected.size &&
        [...other].every(([key, text]) => expected.get(key) === text);
      if (!same)
        throw new Error(`${instance.name} is not an instance of ${base}[T]`);
    }
    genericTexts.set(base, expected);
  }

  const out = collector();
  declareDef(out, "Project", { ...schema, $defs: undefined });
  const ordered = [
    ...Object.keys(defs).filter((name) => genericInstance(name) === undefined),
    ...genericTexts.keys(),
  ].sort();
  for (const name of ordered) {
    const generic = genericTexts.get(name);
    if (generic !== undefined) {
      for (const [key, text] of generic) out.declare(key, text);
    } else {
      declareDef(out, name, defs[name]);
    }
  }

  const constants = Object.entries(limits).map(
    ([name, value]) =>
      `${name} = ${JSON.stringify(value)}\n"""Mirrors \`alpha2_model::${name}\`."""\n`,
  );

  const upperSnake = (name) =>
    name.replace(/([a-z0-9])([A-Z])/g, "$1_$2").toUpperCase();
  const enumerations = Object.keys(defs)
    .sort()
    .flatMap((name) => {
      const node = defs[name];
      let values;
      if (
        Array.isArray(node.enum) &&
        node.enum.every((v) => typeof v === "string")
      )
        values = node.enum;
      const variants = node.oneOf ?? node.anyOf;
      if (
        Array.isArray(variants) &&
        variants.every((v) => typeof v.const === "string")
      )
        values = variants.map((v) => v.const);
      if (values === undefined) return [];
      return [
        `${upperSnake(name)}_VALUES: tuple[${name}, ...] = (${values.map((v) => JSON.stringify(v)).join(", ")}${values.length === 1 ? "," : ""})\n` +
          `"""Every \`${name}\`, in the model's order."""\n`,
      ];
    });

  const header =
    "# Generated by scripts/generate-project-types.mjs from\n" +
    "# schema/project.v1.schema.json and schema/project-limits.v1.json.\n" +
    "# Do not edit; change the Rust model and run `npm run generate:schema`.\n" +
    '"""The project document\'s types, as `TypedDict`s: a ``Project`` is a plain ``dict``."""\n\n' +
    "from __future__ import annotations\n\n" +
    `from typing import ${TYPING_NAMES.join(", ")}\n\n` +
    'T = TypeVar("T")\n';
  // Constants one blank line apart; classes and aliases two, as PEP 8 sets them.
  return [
    header,
    constants.join("\n"),
    enumerations.join("\n"),
    ...out.declared.values(),
  ].join("\n\n");
}

/**
 * Reads JSON text as the engine's parser (serde_json) does, keeping what
 * `JSON.parse` forgets.
 *
 * `JSON.parse` keeps the last of a repeated key, turns `1920.0`, `1.92e3`,
 * and `-0` into numbers like any other, and rounds an integer past 2^53. The
 * engine refuses a struct field given twice, reads those three spellings as
 * floating point, which no integer field accepts, and reads a 64-bit integer
 * exactly. {@link readJsonText} returns the same value as `JSON.parse`
 * together with where the text said any of these, so the shape check can
 * decide them field by field: a repeated key is allowed where the document
 * holds a map, a fractional spelling where it holds a float, and an integer
 * by its digits against the field's integer type.
 */

/** Where the text said something its parsed value does not show. */
export interface JsonSpelling {
  /**
   * Containers whose member, by key or index, is a number its parsed value does not spell: one
   * written as a float (with a fraction, an exponent, or as `-0`) or an integer past
   * `Number.MAX_SAFE_INTEGER`, with the member's text: every such text of a repeated key, which
   * the engine reads each of.
   */
  readonly numbers: WeakMap<object, Map<string | number, string[]>>;
  /** Objects whose text gives a key more than once, with those keys. */
  readonly repeated: WeakMap<object, Set<string>>;
}

/** The deepest nesting the engine's parser reads (serde_json's recursion limit). */
export const MAX_JSON_DEPTH = 127;

/**
 * The value of `json`, which `JSON.parse` has already accepted, with its
 * spelling; `undefined` when it nests deeper than {@link MAX_JSON_DEPTH}.
 */
export function readJsonText(
  json: string,
): { value: unknown; spelling: JsonSpelling } | undefined {
  const spelling: JsonSpelling = {
    numbers: new WeakMap(),
    repeated: new WeakMap(),
  };
  let at = 0;
  const skipSpace = () => {
    while (at < json.length && " \t\n\r".includes(json[at])) at += 1;
  };
  const note = <K>(table: WeakMap<object, Set<K>>, owner: object, key: K) => {
    const keys = table.get(owner);
    if (keys) keys.add(key);
    else table.set(owner, new Set([key]));
  };
  const noteNumber = (owner: object, key: string | number, text: string) => {
    const members = spelling.numbers.get(owner);
    if (!members) spelling.numbers.set(owner, new Map([[key, [text]]]));
    else if (members.has(key)) members.get(key)!.push(text);
    else members.set(key, [text]);
  };
  const readString = (): string => {
    const start = at;
    let escaped = false;
    at += 1;
    while (json[at] !== '"') {
      if (json[at] === "\\") {
        escaped = true;
        at += 2;
      } else at += 1;
    }
    at += 1;
    return escaped
      ? (JSON.parse(json.slice(start, at)) as string)
      : json.slice(start + 1, at - 1);
  };
  // Returns the member's value, and its text if it is a number its value does not spell (see
  // `JsonSpelling.numbers`).
  const readValue = (depth: number): [unknown, string | undefined] => {
    skipSpace();
    const char = json[at];
    if (char === "{" || char === "[") {
      if (depth >= MAX_JSON_DEPTH) throw new TooDeep();
      at += 1;
      skipSpace();
      if (char === "[") {
        const items: unknown[] = [];
        if (json[at] === "]") {
          at += 1;
          return [items, undefined];
        }
        for (;;) {
          const [item, text] = readValue(depth + 1);
          if (text !== undefined) noteNumber(items, items.length, text);
          items.push(item);
          skipSpace();
          if (json[at++] === "]") return [items, undefined];
        }
      }
      const object: Record<string, unknown> = {};
      if (json[at] === "}") {
        at += 1;
        return [object, undefined];
      }
      for (;;) {
        skipSpace();
        const key = readString();
        skipSpace();
        at += 1; // Skip the `:`.
        const [item, text] = readValue(depth + 1);
        if (Object.hasOwn(object, key)) note(spelling.repeated, object, key);
        // The engine reads every value of a repeated key, so each text counts.
        if (text !== undefined) noteNumber(object, key, text);
        // `__proto__` is an own property in parsed JSON, not the prototype.
        Object.defineProperty(object, key, {
          value: item,
          writable: true,
          enumerable: true,
          configurable: true,
        });
        skipSpace();
        if (json[at++] === "}") return [object, undefined];
      }
    }
    if (char === '"') return [readString(), undefined];
    const start = at;
    while (at < json.length && !",]} \t\n\r".includes(json[at])) at += 1;
    const token = json.slice(start, at);
    const value = JSON.parse(token) as unknown;
    const unspelled =
      typeof value === "number" &&
      (/[.eE]/.test(token) || token === "-0" || !Number.isSafeInteger(value));
    return [value, unspelled ? token : undefined];
  };
  try {
    return { value: readValue(0)[0], spelling };
  } catch (error) {
    if (error instanceof TooDeep) return undefined;
    throw error;
  }
}

class TooDeep extends Error {}

/**
 * Whether `text` has a UTF-16 surrogate without its pair, which UTF-8 (and the engine) cannot hold.
 */
export function hasLoneSurrogate(text: string): boolean {
  return /\p{Surrogate}/u.test(text);
}

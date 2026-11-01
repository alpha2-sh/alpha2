/**
 * Reads JSON text the way the engine's parser (`serde_json`) does.
 *
 * `JSON.parse` hides repeated keys, float spellings of integers (`1920.0`, `1.92e3`, `-0`) and
 * integers past 2^53, all of which the engine treats differently. {@link readJsonText} records
 * where they occur so the shape check can judge each field by its type.
 */

/** Source details the parsed value does not show. */
export interface JsonSpelling {
  /**
   * Per container and member key or index, the source text of numbers written as floats
   * (fraction, exponent or `-0`) or as integers past `Number.MAX_SAFE_INTEGER`. A repeated key
   * keeps every text.
   */
  readonly numbers: WeakMap<object, Map<string | number, string[]>>;
  /** Keys given more than once, per object. */
  readonly repeated: WeakMap<object, Set<string>>;
}

/** Deepest nesting the engine's parser accepts (the `serde_json` recursion limit). */
export const MAX_JSON_DEPTH = 127;

/**
 * Parses `json`, which `JSON.parse` must already accept, with its spelling details. Returns
 * `undefined` when it nests deeper than {@link MAX_JSON_DEPTH}.
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
  // Returns the value and, for numbers recorded in `JsonSpelling.numbers`, the source text.
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
        at += 1;
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

/** Whether `text` has an unpaired UTF-16 surrogate, which UTF-8 cannot encode. */
export function hasLoneSurrogate(text: string): boolean {
  return /\p{Surrogate}/u.test(text);
}

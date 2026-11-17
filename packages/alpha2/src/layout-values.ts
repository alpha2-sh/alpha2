/**
 * CSS value parsers for layout fields. Must match `alpha2_model::layout_values`, including error
 * messages.
 */

export type Parsed<T> =
  | { readonly ok: true; readonly value: T }
  | { readonly ok: false; readonly error: string };

const ok = <T>(value: T): Parsed<T> => ({ ok: true, value });
const refused = <T>(error: string): Parsed<T> => ({ ok: false, error });

/** `fraction` is a fraction of the canvas's shorter side. */
export type LengthUnit = "fraction" | "px" | "percent";

/** `value` is a 32-bit float. */
export interface Length {
  readonly unit: LengthUnit;
  readonly value: number;
}

const NUMBER = /^-?(\d+\.?\d*|\.\d+)$/;

/**
 * Parses a CSS number without an exponent, rounded through f64 to f32 as the engine does.
 * Returns `undefined` when invalid or not finite.
 */
export function parseNumber(text: string): number | undefined {
  if (!NUMBER.test(text)) return undefined;
  const value = Math.fround(Number(text));
  return Number.isFinite(value) ? value : undefined;
}

function tokens(text: string): string[] {
  return text.split(" ").filter((token) => token.length > 0);
}

/** Matches Rust's `trim_matches(' ')`: ASCII spaces only. */
function trimSpaces(text: string): string {
  let start = 0;
  let end = text.length;
  while (start < end && text.charCodeAt(start) === 32) start += 1;
  while (end > start && text.charCodeAt(end - 1) === 32) end -= 1;
  return text.slice(start, end);
}

function invalidLength(token: string, percent: boolean): string {
  const hint = token.includes(" ")
    ? ` (write ${token.replaceAll(" ", "")})`
    : token.endsWith("em")
      ? " (em and rem are not supported: write px or a number)"
      : "";
  const forms = percent
    ? 'a number, "<n>px", or "<n>%"'
    : 'a number or "<n>px"';
  return `invalid length "${token}"${hint}: write ${forms}`;
}

/** `percent` says whether the field accepts a percent. */
function parseSizeToken(token: string, percent: boolean): Parsed<Length> {
  if (token.endsWith("px")) {
    const value = parseNumber(token.slice(0, -2));
    return value === undefined
      ? refused(invalidLength(token, percent))
      : ok({ unit: "px", value });
  }
  if (
    !percent &&
    token.endsWith("%") &&
    parseNumber(token.slice(0, -1)) !== undefined
  ) {
    return refused(`percent is not allowed here: "${token}"`);
  }
  const value = parseNumber(token);
  return value === undefined
    ? refused(invalidLength(token, percent))
    : ok({ unit: "fraction", value });
}

/** Parses a `LayoutSize` (no percent). */
export function parseSize(text: string): Parsed<Length> {
  return parseSizeToken(trimSpaces(text), false);
}

/** Parses a `LayoutLength` (percent allowed). */
export function parseLength(text: string): Parsed<Length> {
  const trimmed = trimSpaces(text);
  if (trimmed.endsWith("%")) {
    const value = parseNumber(trimmed.slice(0, -1));
    return value === undefined
      ? refused(invalidLength(text, true))
      : ok({ unit: "percent", value });
  }
  const size = parseSizeToken(trimmed, true);
  return size.ok ? size : refused(invalidLength(text, true));
}

/** Expands a one-to-four value CSS shorthand, top first. */
function shorthand<T>(values: readonly T[]): Parsed<readonly [T, T, T, T]> {
  const [a, b, c, d] = values as [T, T, T, T];
  switch (values.length) {
    case 1:
      return ok([a, a, a, a]);
    case 2:
      return ok([a, b, a, b]);
    case 3:
      return ok([a, b, c, b]);
    case 4:
      return ok([a, b, c, d]);
    default:
      return refused(`expected 1 to 4 lengths, got ${values.length}`);
  }
}

/** Parses `Insets` from the shorthand string, top first. */
export function parseInsets(
  text: string,
): Parsed<readonly [Length, Length, Length, Length]> {
  const sides: Length[] = [];
  for (const token of tokens(text)) {
    const side = parseSizeToken(token, false);
    if (!side.ok) return side;
    sides.push(side.value);
  }
  return shorthand(sides);
}

/** Parses a `LayoutOffset` from a CSS `translate` string; y defaults to 0. */
export function parseOffset(
  text: string,
): Parsed<{ readonly x: Length; readonly y: Length }> {
  const values: Length[] = [];
  for (const token of tokens(text)) {
    const value = parseSizeToken(token, false);
    if (!value.ok) return value;
    values.push(value.value);
  }
  if (values.length === 1)
    return ok({ x: values[0], y: { unit: "fraction", value: 0 } });
  if (values.length === 2) return ok({ x: values[0], y: values[1] });
  return refused(`expected 1 or 2 lengths, got ${values.length}`);
}

export type TrackBreadth =
  | { readonly kind: "length"; readonly length: Length }
  | { readonly kind: "fr"; readonly share: number }
  | { readonly kind: "auto" | "min-content" | "max-content" };

export type TrackSize =
  | { readonly kind: "breadth"; readonly breadth: TrackBreadth }
  | {
      readonly kind: "minmax";
      readonly min: TrackBreadth;
      readonly max: TrackBreadth;
    }
  | { readonly kind: "fit-content"; readonly limit: Length };

export type RepeatCount = number | "auto-fill" | "auto-fit";

export type TrackComponent =
  | { readonly kind: "single"; readonly size: TrackSize }
  | {
      readonly kind: "repeat";
      readonly count: RepeatCount;
      readonly tracks: readonly TrackSize[];
    };

function unknownTrack(token: string): string {
  const hints: Record<string, string> = {
    min_content: "min-content",
    max_content: "max-content",
    auto_fill: "auto-fill",
    auto_fit: "auto-fit",
  };
  if (!(token in hints) && token.startsWith("[")) {
    return `named grid lines are not supported: "${token}"`;
  }
  const hint =
    hints[token] ?? (token.includes(" ") ? token.replaceAll(" ", "") : "");
  return hint
    ? `unknown track "${token}" (write ${hint})`
    : `unknown track "${token}"`;
}

function parseBreadth(token: string, max: boolean): Parsed<TrackBreadth> {
  if (token === "auto" || token === "min-content" || token === "max-content")
    return ok({ kind: token });
  if (token.endsWith("fr")) {
    const share = parseNumber(token.slice(0, -2));
    if (share !== undefined) {
      return max
        ? ok({ kind: "fr", share })
        : refused(`a minimum size cannot be fr: "${token}"`);
    }
  }
  const length = parseLength(token);
  return length.ok
    ? ok({ kind: "length", length: length.value })
    : refused(unknownTrack(token));
}

/** Mirrors the Rust `TrackParser`. */
class TrackParser {
  at = 0;
  constructor(readonly text: string) {}

  rest(): string {
    return this.text.slice(this.at);
  }

  /** Skips spaces and returns whether anything is left. */
  skipSpaces(): boolean {
    while (this.rest().startsWith(" ")) this.at += 1;
    return this.rest().length > 0;
  }

  /** Reads up to a space, comma or parenthesis. */
  token(): string {
    const rest = this.rest();
    const match = /[ ,()]/.exec(rest);
    const end = match ? match.index : rest.length;
    this.at += end;
    return rest.slice(0, end);
  }

  expect(expected: string): Parsed<null> {
    this.skipSpaces();
    if (this.rest().startsWith(expected)) {
      this.at += 1;
      return ok(null);
    }
    return refused(`expected "${expected}" in "${this.text}"`);
  }

  breadth(max: boolean): Parsed<TrackBreadth> {
    this.skipSpaces();
    const token = this.token();
    if (token.length === 0)
      return refused(`expected a track size in "${this.text}"`);
    return parseBreadth(token, max);
  }

  track(): Parsed<TrackSize> {
    const start = this.at;
    const token = this.token();
    if (
      (token === "minmax" || token === "fit-content") &&
      this.rest().startsWith("(")
    ) {
      this.at += 1;
      if (token === "fit-content") {
        const limit = this.breadth(false);
        if (!limit.ok) return limit;
        const close = this.expect(")");
        if (!close.ok) return close;
        return limit.value.kind === "length"
          ? ok({ kind: "fit-content", limit: limit.value.length })
          : refused(
              `fit-content takes a length: "${this.text.slice(start, this.at)}"`,
            );
      }
      const min = this.breadth(false);
      if (!min.ok) return min;
      const comma = this.expect(",");
      if (!comma.ok) return comma;
      const max = this.breadth(true);
      if (!max.ok) return max;
      const close = this.expect(")");
      if (!close.ok) return close;
      return ok({ kind: "minmax", min: min.value, max: max.value });
    }
    if (token === "") return refused(`expected a track in "${this.text}"`);
    if (token === "fit_content" || token === "min_max") {
      const hint = token.replaceAll("_", "-").replace("min-max", "minmax");
      return refused(`unknown track "${token}" (write ${hint})`);
    }
    // A unit standing apart from its number: "1 fr".
    const after = this.at;
    if (parseNumber(token) !== undefined && this.skipSpaces()) {
      const unit = this.token();
      if (unit === "fr" || unit === "px" || unit === "%") {
        return refused(unknownTrack(this.text.slice(start, this.at)));
      }
    }
    this.at = after;
    const breadth = parseBreadth(token, true);
    return breadth.ok
      ? ok({ kind: "breadth", breadth: breadth.value })
      : breadth;
  }

  repeat(): Parsed<TrackComponent> {
    this.skipSpaces();
    const token = this.token();
    let count: RepeatCount;
    if (token === "auto-fill" || token === "auto-fit") count = token;
    else if (token === "auto_fill" || token === "auto_fit") {
      return refused(
        `unknown repeat count "${token}" (write ${token.replace("_", "-")})`,
      );
    } else if (/^\d+$/.test(token) && Number(token) <= 65535) {
      count = Number(token);
    } else return refused(`unknown repeat count "${token}"`);
    const comma = this.expect(",");
    if (!comma.ok) return comma;
    const tracks: TrackSize[] = [];
    for (;;) {
      if (!this.skipSpaces()) return refused(`expected ")" in "${this.text}"`);
      if (this.rest().startsWith(")")) {
        this.at += 1;
        break;
      }
      if (this.rest().startsWith("repeat("))
        return refused("repeat() cannot hold repeat()");
      const track = this.track();
      if (!track.ok) return track;
      tracks.push(track.value);
    }
    if (tracks.length === 0)
      return refused("repeat() needs at least one track");
    return ok({ kind: "repeat", count, tracks });
  }
}

/** Parses a grid `TrackList`. */
export function parseTrackList(text: string): Parsed<TrackComponent[]> {
  const parser = new TrackParser(text);
  const components: TrackComponent[] = [];
  while (parser.skipSpaces()) {
    if (parser.rest().startsWith("repeat(")) {
      parser.at += "repeat(".length;
      const repeat = parser.repeat();
      if (!repeat.ok) return repeat;
      components.push(repeat.value);
    } else {
      const track = parser.track();
      if (!track.ok) return track;
      components.push({ kind: "single", size: track.value });
    }
  }
  return components.length === 0
    ? refused("expected at least one track")
    : ok(components);
}

export type GridEdge =
  | { readonly kind: "auto" }
  | { readonly kind: "line"; readonly line: number }
  | { readonly kind: "span"; readonly count: number };

function invalidLine(text: string): string {
  const trimmed = trimSpaces(text);
  const named = trimmed
    .split(/[ /]/)
    .filter((word) => word.length > 0)
    .some(
      (word) => word !== "span" && word !== "auto" && /^[A-Za-z]/.test(word),
    );
  return named
    ? `named grid lines are not supported: "${trimmed}"`
    : `invalid grid line "${trimmed}": write a line number, "span <n>", or "auto"`;
}

/** Parses an i16 written as a CSS integer. */
function gridInteger(word: string): number | undefined {
  if (!/^-?\d+$/.test(word)) return undefined;
  const value = Number(word);
  return value >= -32768 && value <= 32767 ? value : undefined;
}

function parseEdge(text: string): GridEdge | undefined {
  const words = tokens(text);
  if (words.length === 1 && words[0] === "auto") return { kind: "auto" };
  if (words.length === 2 && words[0] === "span") {
    const count = gridInteger(words[1]);
    return count !== undefined && count >= 0
      ? { kind: "span", count }
      : undefined;
  }
  if (words.length === 1) {
    const line = gridInteger(words[0]);
    return line === undefined ? undefined : { kind: "line", line };
  }
  return undefined;
}

/** Parses a `GridPlacement` from a line number or its CSS string. */
export function parsePlacement(
  value: number | string,
): Parsed<{ readonly start: GridEdge; readonly end: GridEdge }> {
  if (typeof value === "number") {
    return Number.isInteger(value) && value >= -32768 && value <= 32767
      ? ok({ start: { kind: "line", line: value }, end: { kind: "auto" } })
      : refused(invalidLine(String(value)));
  }
  const split = value.indexOf("/");
  const [start, end] =
    split < 0
      ? [value, undefined]
      : [value.slice(0, split), value.slice(split + 1)];
  if (end?.includes("/")) return refused(invalidLine(value));
  const first = parseEdge(start);
  if (first === undefined) return refused(invalidLine(start));
  if (end === undefined) return ok({ start: first, end: { kind: "auto" } });
  const second = parseEdge(end);
  return second === undefined
    ? refused(invalidLine(end))
    : ok({ start: first, end: second });
}

/** Straight (non-premultiplied) sRGB with alpha, components in `0..=1`. */
export interface Color {
  readonly red: number;
  readonly green: number;
  readonly blue: number;
  readonly alpha: number;
}

/** Parses `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa` into f32 components. */
export function parseHex(text: string): Parsed<Color> {
  const digits = text.startsWith("#") ? text.slice(1) : undefined;
  if (
    digits === undefined ||
    ![3, 4, 6, 8].includes(digits.length) ||
    !/^[0-9a-fA-F]*$/.test(digits)
  ) {
    return refused(
      `invalid color "${text}": write #rgb, #rgba, #rrggbb, or #rrggbbaa`,
    );
  }
  const short = digits.length <= 4;
  const channels = Array.from(
    { length: short ? digits.length : digits.length / 2 },
    (_, index) =>
      Math.fround(
        (short
          ? parseInt(digits[index], 16) * 17
          : parseInt(digits.slice(index * 2, index * 2 + 2), 16)) / 255,
      ),
  );
  return ok({
    red: channels[0],
    green: channels[1],
    blue: channels[2],
    alpha: channels[3] ?? 1,
  });
}

// Compares `cargo bench --workspace --all-features` output against a baseline and reports every
// time that grew by more than a threshold.
//
//   node scripts/lib/bench-compare.mjs <baseline.txt> <current.txt> [--threshold <percent>]
//
// Lines read `<name> <key>=<value> ...`. Keys with an `ns`, `us` or `ms` word are measurements;
// `ratio` and `realtime_pct` are skipped; other keys are parameters identifying the case. Each
// measurement compares the fastest value per file (load only adds time), and a change of one unit
// in the last printed digit is rounding and never counts.
//
// Exit status: 0 when nothing grew past the threshold (default 5 %), 1 when something did, 2 on a
// usage error or when the files share no measurement.
import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

const token = /^[A-Za-z0-9_]+=\S+$/;
const derived = new Set(["ratio", "realtime_pct"]);

/** Whether a key holds a time measurement. */
export function isTime(key) {
  return key.split("_").some((word) => ["ns", "us", "ms"].includes(word));
}

/**
 * The fastest value of every measurement in a benchmark output, keyed by
 * `<name> <parameters> <measurement key>`, with the unit of its last printed digit.
 *
 * @returns {Map<string, {value: number, step: number}>}
 */
export function measurements(text) {
  const fastest = new Map();
  for (const line of text.split("\n")) {
    const [name, ...pairs] = line.trim().split(/\s+/);
    if (!/^[a-z][a-z0-9_]*$/.test(name ?? "") || pairs.length === 0) continue;
    if (!pairs.every((pair) => token.test(pair))) continue;
    const parameters = [];
    const times = [];
    for (const pair of pairs) {
      const [key, value] = pair.split("=", 2);
      if (derived.has(key)) continue;
      if (isTime(key)) times.push([key, value]);
      else parameters.push(pair);
    }
    for (const [key, text] of times) {
      const value = Number(text);
      if (!Number.isFinite(value)) continue;
      const step = 10 ** -(text.split(".")[1]?.length ?? 0);
      const id = [name, ...parameters, key].join(" ");
      const seen = fastest.get(id);
      fastest.set(id, {
        value: Math.min(value, seen?.value ?? Infinity),
        step: Math.max(step, seen?.step ?? 0),
      });
    }
  }
  return fastest;
}

/**
 * Every measurement of `current` against `baseline`.
 *
 * @returns {{regressions: {id: string, baseline: number, current: number, change: number}[],
 *   improvements: number, unchanged: number, missing: string[], added: string[]}}
 */
export function compare(baseline, current, threshold = 5) {
  const regressions = [];
  let improvements = 0;
  let unchanged = 0;
  for (const [id, { value: before, step }] of baseline) {
    if (!current.has(id)) continue;
    const { value: after, step: currentStep } = current.get(id);
    // A zero baseline (a time under the printed precision) has no ratio; any time above it counts.
    const change =
      before > 0 ? ((after - before) / before) * 100 : after > 0 ? Infinity : 0;
    const digits = Math.round((after - before) / Math.max(step, currentStep));
    if (change > threshold && digits > 1)
      regressions.push({ id, baseline: before, current: after, change });
    else if (change < -threshold) improvements += 1;
    else unchanged += 1;
  }
  regressions.sort((a, b) => b.change - a.change);
  return {
    regressions,
    improvements,
    unchanged,
    missing: [...baseline.keys()].filter((id) => !current.has(id)),
    added: [...current.keys()].filter((id) => !baseline.has(id)),
  };
}

/** The report's lines; the first says whether anything regressed. */
export function report(result, threshold = 5) {
  const { regressions, improvements, unchanged, missing, added } = result;
  const compared = regressions.length + improvements + unchanged;
  const lines = [
    regressions.length
      ? `REGRESSION: ${regressions.length} of ${compared} benchmark times grew by more than ${threshold} %`
      : `ok: none of ${compared} benchmark times grew by more than ${threshold} %`,
  ];
  for (const { id, baseline, current, change } of regressions) {
    lines.push(`  +${change.toFixed(1)} %  ${id}: ${baseline} -> ${current}`);
  }
  lines.push(
    `faster by more than ${threshold} %: ${improvements}; within ${threshold} %: ${unchanged}`,
  );
  for (const id of missing) lines.push(`  not measured any more: ${id}`);
  for (const id of added) lines.push(`  new, no baseline: ${id}`);
  return lines;
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  const args = process.argv.slice(2);
  let threshold = 5;
  const at = args.indexOf("--threshold");
  if (at >= 0) {
    threshold = Number(args[at + 1]);
    args.splice(at, 2);
  }
  if (args.length !== 2 || !(threshold >= 0)) {
    console.error(
      "usage: bench-compare.mjs <baseline.txt> <current.txt> [--threshold <percent>]",
    );
    process.exit(2);
  }
  const [baseline, current] = args.map((file) =>
    measurements(readFileSync(file, "utf8")),
  );
  const result = compare(baseline, current, threshold);
  if (
    result.regressions.length + result.improvements + result.unchanged ===
    0
  ) {
    console.error(
      "the baseline and the current output share no benchmark measurement",
    );
    process.exit(2);
  }
  console.log(report(result, threshold).join("\n"));
  process.exit(result.regressions.length ? 1 : 0);
}

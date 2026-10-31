// `{ "$repeat": N, "item": T }` expands to N copies of T with "{i}" replaced by i and
// `{ "$index": { "start": a, "step": b } }` by a + b*i; nested repeats count their own i.
// Matches `crates/alpha2-model/src/tests/shared_corpora.rs`.

import { readFile } from "node:fs/promises";

export const validationCorpusUrl = new URL(
  "../../tests/cases/project-validation-corpus.json",
  import.meta.url,
);

export async function readValidationCorpus() {
  return expandRepeats(JSON.parse(await readFile(validationCorpusUrl, "utf8")));
}

export function expandRepeats(value) {
  if (Array.isArray(value)) {
    return value.flatMap((element) =>
      isRepeat(element)
        ? Array.from({ length: element.$repeat }, (_, index) =>
            expandRepeats(instantiate(element.item, index)),
          )
        : [expandRepeats(element)],
    );
  }
  if (isObject(value)) {
    return Object.fromEntries(
      Object.entries(value).map(([key, field]) => [key, expandRepeats(field)]),
    );
  }
  return value;
}

function instantiate(template, index) {
  if (typeof template === "string") {
    return template.replaceAll("{i}", String(index));
  }
  if (Array.isArray(template)) {
    return template.map((element) => instantiate(element, index));
  }
  if (isObject(template)) {
    if (isRepeat(template)) {
      return template;
    }
    const keys = Object.keys(template);
    if (keys.length === 1 && keys[0] === "$index") {
      return template.$index.start + template.$index.step * index;
    }
    return Object.fromEntries(
      keys.map((key) => [key, instantiate(template[key], index)]),
    );
  }
  return template;
}

const isObject = (value) => typeof value === "object" && value !== null;
const isRepeat = (value) =>
  isObject(value) && !Array.isArray(value) && "$repeat" in value;

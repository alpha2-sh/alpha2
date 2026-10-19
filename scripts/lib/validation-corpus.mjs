// The shared project validation corpus (`tests/cases/project-validation-corpus.json`), with its
// repeated runs written out. A case that needs hundreds of nearly equal elements (tracks, clips,
// effects, composition nodes) holds them as one array element `{ "$repeat": N, "item": T }`: N
// elements, T at i = 0..N-1, with "{i}" in T's strings replaced by i and
// `{ "$index": { "start": a, "step": b } }` by a + b*i. A `$repeat` inside T counts its own i.
// `crates/alpha2-model/src/tests/shared_corpora.rs` expands the corpus the same way.

import { readFile } from "node:fs/promises";

export const validationCorpusUrl = new URL(
  "../../tests/cases/project-validation-corpus.json",
  import.meta.url,
);

/** The corpus's cases, each with its repeats expanded. */
export async function readValidationCorpus() {
  return expandRepeats(JSON.parse(await readFile(validationCorpusUrl, "utf8")));
}

/** `value` with every `$repeat` element written out. */
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

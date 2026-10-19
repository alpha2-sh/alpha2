// Projects known to pass `validateProject`, so the editor does not walk the same project twice.
// Not part of the package's API: only the editor's own code says a project is valid.
import type { Project } from "alpha2/project";
import { freeze } from "./document.js";

/** Deep-frozen projects that validated: they cannot change, so they stay valid. */
const frozen = new WeakSet<Project>();
/** Projects an edit validated during the `executeCommand` running now (see `withValidated`). */
let current: WeakSet<Project> | null = null;

/**
 * Records that `project` passed `validateProject`. A deep-frozen project is remembered for good;
 * another only until the `withValidated` call running now returns, since its owner may change it
 * later.
 */
export function rememberValid(project: Project): void {
  if (Object.isFrozen(project)) frozen.add(project);
  else current?.add(project);
}

/** Whether `project` is known to pass `validateProject` (see `rememberValid`). */
export function knownValid(project: Project): boolean {
  return frozen.has(project) || (current?.has(project) ?? false);
}

/** Runs `run`, forgetting when it returns the unfrozen projects it called valid. */
export function withValidated<T>(run: () => T): T {
  const previous = current;
  current = new WeakSet();
  try {
    return run();
  } finally {
    current = previous;
  }
}

/**
 * Documents {@link frozenValid} froze: plain data the editor made or copied, frozen all through.
 */
const sealedDocuments = new WeakSet<object>();

/**
 * Deep-freezes `document`, which validated, and remembers its project as valid, so compiling it or
 * checking it again does not walk the project again.
 */
export function frozenValid<T extends { readonly project: Project }>(
  document: T,
): T {
  freeze(document);
  rememberValid(document.project);
  sealedDocuments.add(document);
  return document;
}

/**
 * `document` itself when {@link frozenValid} froze it, which nothing can change, or else a copy
 * for a session to own. Keeping the frozen document keeps what validating its parts found.
 */
export function owned<T extends object>(document: T): T {
  return sealedDocuments.has(document) ? document : structuredClone(document);
}

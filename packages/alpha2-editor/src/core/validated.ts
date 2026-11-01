// Projects known to pass `validateProject`, so the editor does not walk the same project twice.
// Internal: only the editor's own code may mark a project valid.
import type { Project } from "alpha2/project";
import { freeze } from "./document.js";

/** Deep-frozen projects that validated; they cannot change, so they stay valid. */
const frozen = new WeakSet<Project>();
/** Projects an edit validated during the `executeCommand` running now (see `withValidated`). */
let current: WeakSet<Project> | null = null;

/**
 * Records that `project` passed `validateProject`. A deep-frozen project is remembered for good;
 * others only until the current `withValidated` call returns, since their owner may change them.
 */
export function rememberValid(project: Project): void {
  if (Object.isFrozen(project)) frozen.add(project);
  else current?.add(project);
}

/** Whether `project` is known to pass `validateProject` (see `rememberValid`). */
export function knownValid(project: Project): boolean {
  return frozen.has(project) || (current?.has(project) ?? false);
}

/** Runs `run`, then forgets the unfrozen projects marked valid during it. */
export function withValidated<T>(run: () => T): T {
  const previous = current;
  current = new WeakSet();
  try {
    return run();
  } finally {
    current = previous;
  }
}

/** Documents {@link frozenValid} froze: editor-made plain data, deep-frozen. */
const sealedDocuments = new WeakSet<object>();

/**
 * Deep-freezes the validated `document` and remembers its project as valid, so later checks skip
 * walking it.
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
 * `document` itself when {@link frozenValid} froze it, else a copy for a session to own. Keeping
 * the frozen document keeps its cached validation.
 */
export function owned<T extends object>(document: T): T {
  return sealedDocuments.has(document) ? document : structuredClone(document);
}

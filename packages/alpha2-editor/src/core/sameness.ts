/** Keys `JSON.stringify` writes for `value`, in order. */
function serializedKeys(value: object): string[] {
  return Object.keys(value).filter((key) => {
    const field = (value as Record<string, unknown>)[key];
    return field !== undefined && typeof field !== "function";
  });
}

/**
 * Whether `a` and `b` serialize to the same JSON text: arrays item by item, objects key by key in
 * order. Shared parts compare by identity, so the cost is the unshared part, and the walk stops at
 * the first difference.
 */
export function sameJson(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (
    a === null ||
    b === null ||
    typeof a !== "object" ||
    typeof b !== "object"
  )
    return (a ?? null) === (b ?? null);
  if (Array.isArray(a)) {
    if (!Array.isArray(b) || a.length !== b.length) return false;
    for (let index = 0; index < a.length; index++)
      if (!sameJson(a[index] ?? null, (b[index] as unknown) ?? null))
        return false;
    return true;
  }
  if (Array.isArray(b)) return false;
  const keys = serializedKeys(a);
  const other = serializedKeys(b);
  if (keys.length !== other.length) return false;
  for (let index = 0; index < keys.length; index++) {
    const key = keys[index];
    if (
      key !== other[index] ||
      !sameJson(
        (a as Record<string, unknown>)[key],
        (b as Record<string, unknown>)[key],
      )
    )
      return false;
  }
  return true;
}

// The data hash of `tests/cases/surface-conformance.json`, for Node and the browser alike.

/** The 8 lowercase hex digits of `value`'s f32 bits, or `-` for null. */
function bits(value) {
  if (value === null || value === undefined) return "-";
  const view = new DataView(new ArrayBuffer(4));
  view.setFloat32(0, value);
  return view.getUint32(0).toString(16).padStart(8, "0");
}

function line(set) {
  let items;
  if (set.kind === "intervals")
    items = set.intervals.map(
      (item) =>
        `${item.start}:${item.end}:${item.label ?? ""}:${item.score === undefined ? "" : bits(item.score)}`,
    );
  else if (set.kind === "words")
    items = set.words.map(
      (item) => `${item.start}:${item.end}:${item.text}:${item.speaker ?? ""}`,
    );
  else {
    const width = set.values.length / Math.max(set.times.length, 1);
    items = set.times.map(
      (time, index) =>
        `${time}:${set.values
          .slice(index * width, (index + 1) * width)
          .map(bits)
          .join(",")}`,
    );
  }
  return `${set.id} ${set.kind}|${items.join(";")}`;
}

/** The table's `data_hash` of `sets`, as lowercase hex. */
export async function dataHash(sets) {
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(sets.map(line).join("\n")),
  );
  return [...new Uint8Array(digest)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}

/** 32-bit FNV-1a, as the digest fixture hashes deliveries on every surface. */
export function fnv1a(bytes) {
  let hash = 0x811c9dc5;
  for (const byte of bytes) hash = Math.imul(hash ^ byte, 0x01000193) >>> 0;
  return hash;
}

/** The digest fixture's two values for a delivery: the hash's halves over 65536. */
export function digestValues(bytes) {
  const hash = fnv1a(bytes);
  return [(hash >>> 16) / 65536, (hash & 0xffff) / 65536];
}

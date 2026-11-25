import { EditorError } from "../../core/document.js";
import { RangeFile } from "./range-file.js";

/**
 * @param {import("../types.js").MediaReference} reference
 * @param {AbortSignal} [signal]
 */
export async function openMediaReference(reference, signal) {
  const name = reference.name;
  const url = new URL(
    String(reference.url),
    globalThis.document?.baseURI ?? globalThis.location?.href,
  ).href;
  const refuse = (reason, cause) =>
    new EditorError(
      "unreadable_media",
      `Cannot read ${name}: ${reason}`,
      "import",
      undefined,
      true,
      cause === undefined ? undefined : { cause },
    );
  let response;
  try {
    response = await fetch(url, { headers: { range: "bytes=0-0" }, signal });
  } catch (error) {
    if (signal?.aborted) throw signal.reason;
    throw refuse(`${url} did not answer`, error);
  }
  await response.body?.cancel().catch(() => {});
  const total =
    response.status === 206
      ? /\/(\d+)$/.exec(response.headers.get("content-range") ?? "")?.[1]
      : undefined;
  if (total === undefined)
    throw refuse(
      response.ok
        ? `${url} does not answer range requests`
        : `${url} answered ${response.status}`,
    );
  const modified = Date.parse(response.headers.get("last-modified") ?? "");
  return new RangeFile({
    url,
    name,
    size: Number(total),
    type:
      reference.type ??
      response.headers.get("content-type")?.split(";")[0].trim() ??
      "",
    lastModified:
      reference.lastModified ??
      (Number.isFinite(modified) ? modified : undefined),
    ...(reference.path === undefined ? {} : { path: reference.path }),
  });
}

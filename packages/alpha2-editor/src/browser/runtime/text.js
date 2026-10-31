import { EditorError } from "../../core/document.js";

/**
 * The text of a user's file (a subtitle file, a `.cube` LUT) by the one rule every alpha2 runtime
 * reads such files by: UTF-8, with a byte order mark dropped, or UTF-16 when the file starts with a
 * little- or big-endian byte order mark (what Windows PowerShell 5.1 writes). Anything else, a
 * legacy code page above all, throws an `invalid_text` {@link EditorError} naming the file rather
 * than turning into replacement characters.
 *
 * @param {Blob & { name?: string }} file
 * @param {string} [operation] The operation the error names. Defaults to `import`.
 * @returns {Promise<string>}
 */
export async function readUserText(file, operation = "import") {
  const bytes = new Uint8Array(await file.arrayBuffer());
  const encoding =
    bytes[0] === 0xff && bytes[1] === 0xfe
      ? "utf-16le"
      : bytes[0] === 0xfe && bytes[1] === 0xff
        ? "utf-16be"
        : "utf-8";
  try {
    // Each decoder drops its own byte order mark.
    return new TextDecoder(encoding, { fatal: true }).decode(bytes);
  } catch {
    throw new EditorError(
      "invalid_text",
      `${file.name ?? "the file"} is not UTF-8 text; save it as UTF-8`,
      operation,
    );
  }
}

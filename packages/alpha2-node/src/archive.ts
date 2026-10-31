import { createReadStream } from "node:fs";
import { mkdir, open, stat, symlink } from "node:fs/promises";
import { basename, dirname, join, resolve, sep } from "node:path";
import type { Readable } from "node:stream";
import { createGunzip, createInflateRaw, crc32 } from "node:zlib";

/** Why an archive could not be unpacked; the message starts with the archive's name. */
class ArchiveError extends Error {}

/**
 * Unpacks a release archive (`.tar.gz` or `.zip`) into `destination`, an empty directory, without
 * running any program: names go to the file system as Unicode on every platform, and nothing the
 * archive records about owners is applied. Every entry must stay inside `destination`; symbolic
 * links must be relative and point inside their top-level directory. Files are written `0o755`
 * when the archive marks any execute bit, else `0o644`. Rejects on the first entry it cannot
 * unpack (leaving what it wrote for the caller to remove) or when `signal` aborts.
 */
export async function extractArchive(
  archive: string,
  destination: string,
  signal?: AbortSignal,
): Promise<void> {
  const writer = new Writer(archive, resolve(destination), signal);
  try {
    if (archive.endsWith(".tar.gz")) await extractTar(archive, writer);
    else if (archive.endsWith(".zip")) await extractZip(archive, writer);
    else writer.fail("is neither a .tar.gz nor a .zip");
    await writer.finish();
  } catch (error) {
    if (error instanceof ArchiveError || signal?.aborted) throw error;
    // A decompression or file system error, named with the archive it came from.
    throw new ArchiveError(
      `${basename(archive)} cannot be unpacked: ${(error as Error).message}`,
      { cause: error },
    );
  }
}

/** Creates the entries of one archive under its destination, links last. */
class Writer {
  private readonly links: { path: string; target: string; name: string }[] = [];

  constructor(
    private readonly archive: string,
    private readonly root: string,
    private readonly signal: AbortSignal | undefined,
  ) {}

  fail(message: string): never {
    throw new ArchiveError(`${basename(this.archive)} ${message}`);
  }

  checkAborted(): void {
    this.signal?.throwIfAborted();
  }

  /** The path `name` unpacks to, after refusing names that could land outside the root. */
  pathOf(name: string): { path: string; segments: string[] } {
    const trimmed = name.replace(/\/+$/, "");
    if (
      trimmed === "" ||
      trimmed.startsWith("/") ||
      /^[A-Za-z]:/.test(trimmed) ||
      trimmed.includes("\\") ||
      trimmed.includes("\0")
    )
      this.fail(`has an entry with an unsafe name: ${name}`);
    const segments = trimmed.split("/").filter((segment) => segment !== ".");
    if (segments.length === 0 || segments.some((s) => s === "" || s === ".."))
      this.fail(`has an entry with an unsafe name: ${name}`);
    const path = join(this.root, ...segments);
    if (!path.startsWith(this.root + sep))
      this.fail(`has an entry outside it: ${name}`);
    return { path, segments };
  }

  async directory(name: string): Promise<void> {
    await mkdir(this.pathOf(name).path, { recursive: true, mode: 0o755 });
  }

  /** Writes a regular file from `chunks`, refusing to replace anything already there. */
  async file(
    name: string,
    mode: number,
    chunks: AsyncIterable<Buffer>,
  ): Promise<void> {
    const { path } = this.pathOf(name);
    await mkdir(dirname(path), { recursive: true, mode: 0o755 });
    const handle = await open(path, "wx", mode & 0o111 ? 0o755 : 0o644);
    try {
      for await (const chunk of chunks) await handle.write(chunk);
    } finally {
      await handle.close();
    }
  }

  /**
   * Records a relative link, created by {@link finish} after every regular file, so no entry is
   * written through a link the archive made.
   */
  link(name: string, target: string): void {
    const { path, segments } = this.pathOf(name);
    const top = join(this.root, segments[0]);
    if (
      target === "" ||
      target.startsWith("/") ||
      /^[A-Za-z]:/.test(target) ||
      target.includes("\\")
    )
      this.fail(`has a link ${name} with a non-relative target ${target}`);
    const resolved = resolve(dirname(path), target);
    if (resolved !== top && !resolved.startsWith(top + sep))
      this.fail(`has a link ${name} that points outside it: ${target}`);
    this.links.push({ path, target, name });
  }

  async finish(): Promise<void> {
    const linked = new Set(this.links.map((link) => link.path));
    for (const link of this.links) {
      this.checkAborted();
      // A link beneath another link would be created wherever that link points.
      for (let parent = dirname(link.path); parent !== this.root;) {
        if (linked.has(parent))
          this.fail(`has a link ${link.name} inside another link`);
        parent = dirname(parent);
      }
      await mkdir(dirname(link.path), { recursive: true, mode: 0o755 });
      await symlink(link.target, link.path);
    }
  }
}

/** Reads exact byte counts from a stream of chunks. */
class ByteReader {
  private readonly iterator: AsyncIterator<Buffer>;
  private buffered: Buffer[] = [];
  private length = 0;
  private ended = false;

  constructor(stream: Readable) {
    this.iterator = (stream as AsyncIterable<Buffer>)[Symbol.asyncIterator]();
  }

  private async fill(size: number): Promise<void> {
    while (this.length < size && !this.ended) {
      const next = await this.iterator.next();
      if (next.done) this.ended = true;
      else {
        this.buffered.push(next.value);
        this.length += next.value.length;
      }
    }
  }

  /** Up to `size` bytes; fewer only at the end of the stream. */
  async read(size: number): Promise<Buffer> {
    await this.fill(size);
    const all = Buffer.concat(this.buffered);
    const taken = all.subarray(0, Math.min(size, all.length));
    this.buffered =
      all.length > taken.length ? [all.subarray(taken.length)] : [];
    this.length = all.length - taken.length;
    return taken;
  }

  /** Exactly `size` bytes as they arrive, without holding them all. */
  async *take(size: number, what: () => never): AsyncGenerator<Buffer> {
    let left = size;
    while (left > 0) {
      const chunk = await this.read(Math.min(left, 1 << 20));
      if (chunk.length === 0) what();
      left -= chunk.length;
      yield chunk;
    }
  }
}

const BLOCK = 512;

/** A NUL-terminated header field. */
function field(header: Buffer, start: number, length: number): string {
  const bytes = header.subarray(start, start + length);
  const end = bytes.indexOf(0);
  return bytes.subarray(0, end < 0 ? bytes.length : end).toString("utf8");
}

/** An octal header number, or GNU's base-256 form when the high bit of its first byte is set. */
function number(header: Buffer, start: number, length: number): number {
  const bytes = header.subarray(start, start + length);
  if (bytes[0] & 0x80) {
    let value = bytes[0] & 0x7f;
    for (const byte of bytes.subarray(1)) value = value * 256 + byte;
    return value;
  }
  const text = field(header, start, length).trim();
  return text === "" ? 0 : parseInt(text, 8);
}

/** The pax keys an entry's extended header overrides. */
interface PaxOverrides {
  path?: string;
  linkpath?: string;
  size?: number;
}

function parsePax(payload: Buffer, overrides: PaxOverrides): boolean {
  let offset = 0;
  while (offset < payload.length) {
    const space = payload.indexOf(0x20, offset);
    if (space < 0) return false;
    const length = parseInt(payload.subarray(offset, space).toString(), 10);
    if (!Number.isFinite(length) || length <= 0) return false;
    const record = payload.subarray(space + 1, offset + length - 1).toString();
    const equals = record.indexOf("=");
    if (equals < 0) return false;
    const key = record.slice(0, equals);
    const value = record.slice(equals + 1);
    if (key === "path") overrides.path = value;
    else if (key === "linkpath") overrides.linkpath = value;
    else if (key === "size") overrides.size = Number(value);
    offset += length;
  }
  return true;
}

async function extractTar(archive: string, writer: Writer): Promise<void> {
  const gunzip = createGunzip();
  const source = createReadStream(archive);
  source.on("error", (error) => gunzip.destroy(error));
  try {
    await readTar(new ByteReader(source.pipe(gunzip)), writer);
  } finally {
    source.destroy();
    gunzip.destroy();
  }
}

async function readTar(reader: ByteReader, writer: Writer): Promise<void> {
  const truncated = (): never => writer.fail("is truncated");
  let pax: PaxOverrides = {};
  let longName: string | undefined;
  let longLink: string | undefined;

  for (;;) {
    writer.checkAborted();
    const header = await reader.read(BLOCK);
    if (header.length < BLOCK) truncated();
    if (header.every((byte) => byte === 0)) {
      const second = await reader.read(BLOCK);
      if (second.length < BLOCK) truncated();
      if (!second.every((byte) => byte === 0))
        writer.fail("has a zero block inside it");
      return;
    }

    let sum = 0;
    for (let index = 0; index < BLOCK; index += 1)
      sum += index >= 148 && index < 156 ? 0x20 : header[index];
    if (sum !== number(header, 148, 8))
      writer.fail("has a header whose checksum does not match");

    const type = String.fromCharCode(header[156] || 0x30);
    const ustar = header.subarray(257, 263).toString("latin1") === "ustar\0";
    const prefix = ustar ? field(header, 345, 155) : "";
    const headerName = field(header, 0, 100);
    const name =
      pax.path ?? longName ?? (prefix ? `${prefix}/${headerName}` : headerName);
    const linkName = pax.linkpath ?? longLink ?? field(header, 157, 100);
    const size = pax.size ?? number(header, 124, 12);
    const mode = number(header, 100, 8);
    const padded = Math.ceil(size / BLOCK) * BLOCK;

    const payload = async (): Promise<Buffer> => {
      const data = await reader.read(padded);
      if (data.length < padded) truncated();
      return data.subarray(0, size);
    };
    const skip = async (): Promise<void> => {
      for await (const chunk of reader.take(padded, truncated)) void chunk;
    };

    if (type === "x") {
      if (!parsePax(await payload(), pax))
        writer.fail("has a malformed pax header");
      continue;
    }
    if (type === "g") {
      await skip();
      continue;
    }
    if (type === "L" || type === "K") {
      const data = await payload();
      const end = data.indexOf(0);
      const text = data.subarray(0, end < 0 ? data.length : end).toString();
      if (type === "L") longName = text;
      else longLink = text;
      continue;
    }
    pax = {};
    longName = undefined;
    longLink = undefined;

    if (type === "0") {
      await writer.file(name, mode, reader.take(size, truncated));
      const padding = padded - size;
      if (padding > 0 && (await reader.read(padding)).length < padding)
        truncated();
    } else if (type === "5") {
      await writer.directory(name);
      await skip();
    } else if (type === "2") {
      writer.link(name, linkName);
      await skip();
    } else {
      writer.fail(`has an unsupported entry ${name} (type ${type})`);
    }
  }
}

// Zip record signatures and the largest end-of-central-directory record (with a full comment).
const END_OF_DIRECTORY = 0x06054b50;
const CENTRAL_ENTRY = 0x02014b50;
const LOCAL_HEADER = 0x04034b50;
const END_SEARCH = 22 + 0xffff;

async function extractZip(archive: string, writer: Writer): Promise<void> {
  const handle = await open(archive, "r");
  try {
    const { size } = await stat(archive);
    const readAt = async (position: number, length: number) => {
      const buffer = Buffer.alloc(length);
      const { bytesRead } = await handle.read(buffer, 0, length, position);
      if (bytesRead < length) writer.fail("is truncated");
      return buffer;
    };

    const tailLength = Math.min(size, END_SEARCH);
    const tail = await readAt(size - tailLength, tailLength);
    let end = -1;
    for (let offset = tail.length - 22; offset >= 0; offset -= 1) {
      if (tail.readUInt32LE(offset) === END_OF_DIRECTORY) {
        end = offset;
        break;
      }
    }
    if (end < 0) writer.fail("is not a zip archive (no central directory)");
    const entries = tail.readUInt16LE(end + 10);
    const directorySize = tail.readUInt32LE(end + 12);
    const directoryOffset = tail.readUInt32LE(end + 16);
    if (
      entries === 0xffff ||
      directorySize === 0xffffffff ||
      directoryOffset === 0xffffffff
    )
      writer.fail("is a zip64 archive, which is not supported");
    const directory = await readAt(directoryOffset, directorySize);

    let offset = 0;
    for (let index = 0; index < entries; index += 1) {
      writer.checkAborted();
      if (
        offset + 46 > directory.length ||
        directory.readUInt32LE(offset) !== CENTRAL_ENTRY
      )
        writer.fail("has a damaged central directory");
      const madeBy = directory.readUInt16LE(offset + 4);
      const flags = directory.readUInt16LE(offset + 8);
      const method = directory.readUInt16LE(offset + 10);
      const crc = directory.readUInt32LE(offset + 16);
      const compressedSize = directory.readUInt32LE(offset + 20);
      const uncompressedSize = directory.readUInt32LE(offset + 24);
      const nameLength = directory.readUInt16LE(offset + 28);
      const extraLength = directory.readUInt16LE(offset + 30);
      const commentLength = directory.readUInt16LE(offset + 32);
      const attributes = directory.readUInt32LE(offset + 38);
      const localOffset = directory.readUInt32LE(offset + 42);
      const rawName = directory.subarray(offset + 46, offset + 46 + nameLength);
      offset += 46 + nameLength + extraLength + commentLength;

      let name: string;
      if (flags & 0x800) {
        try {
          name = new TextDecoder("utf-8", { fatal: true }).decode(rawName);
        } catch {
          writer.fail("has an entry whose UTF-8 name is malformed");
        }
      } else {
        if (rawName.some((byte) => byte > 0x7f))
          writer.fail(
            `has an entry whose name is neither UTF-8 nor ASCII: ${rawName.toString("latin1")}`,
          );
        name = rawName.toString("latin1");
      }
      if (flags & 0x1) writer.fail(`has an encrypted entry ${name}`);
      if (
        compressedSize === 0xffffffff ||
        uncompressedSize === 0xffffffff ||
        localOffset === 0xffffffff
      )
        writer.fail(`has a zip64 entry ${name}, which is not supported`);
      // Unix "made by" hosts keep the mode in the high half of the external attributes.
      const mode = madeBy >> 8 === 3 ? attributes >>> 16 : 0;
      if ((mode & 0o170000) === 0o120000)
        writer.fail(`has a link ${name}, which zip archives may not carry`);
      if (name.endsWith("/")) {
        await writer.directory(name);
        continue;
      }
      if (method !== 0 && method !== 8)
        writer.fail(`has an entry ${name} compressed with method ${method}`);

      const local = await readAt(localOffset, 30);
      if (local.readUInt32LE(0) !== LOCAL_HEADER)
        writer.fail(`has a damaged local header for ${name}`);
      const start =
        localOffset + 30 + local.readUInt16LE(26) + local.readUInt16LE(28);
      if (start + compressedSize > size) writer.fail("is truncated");

      let chunks: AsyncIterable<Buffer> | Iterable<Buffer> = [];
      const stored =
        compressedSize > 0
          ? createReadStream(archive, {
              start,
              end: start + compressedSize - 1,
            })
          : undefined;
      if (stored && method === 8) {
        const inflate = createInflateRaw();
        stored.on("error", (error) => inflate.destroy(error));
        chunks = stored.pipe(inflate);
      } else if (stored) chunks = stored;
      let checksum = 0;
      let written = 0;
      async function* verified(): AsyncGenerator<Buffer> {
        try {
          for await (const chunk of chunks) {
            checksum = crc32(chunk, checksum);
            written += chunk.length;
            yield chunk;
          }
        } catch (error) {
          writer.fail(
            `has an entry ${name} that does not decompress: ${(error as Error).message}`,
          );
        }
      }
      try {
        await writer.file(name, mode, verified());
      } finally {
        stored?.destroy();
      }
      if (written !== uncompressedSize || checksum >>> 0 !== crc)
        writer.fail(`has an entry ${name} whose CRC-32 or size does not match`);
    }
  } finally {
    await handle.close();
  }
}

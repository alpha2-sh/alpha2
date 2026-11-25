import { CustomSource, BlobSource } from "mediabunny";

const RANGE_FILE = "alpha2.range-file";

export class RangeFile {
  /**
   * @param {{ url: string, name: string, size: number, type?: string, lastModified?: number, path?: string, offset?: number }} fields
   */
  constructor({ url, name, size, type = "", lastModified, path, offset = 0 }) {
    this.format = RANGE_FILE;
    this.url = url;
    this.name = name;
    this.size = size;
    this.type = type;
    this.lastModified = lastModified ?? Date.now();
    if (path !== undefined) this.path = path;
    this.offset = offset;
  }

  async read(start, end) {
    const from = Math.max(0, Math.min(start, this.size));
    const to = Math.max(from, Math.min(end, this.size));
    if (from === to) return new Uint8Array(0);
    const response = await this.#fetch(from, to);
    const bytes = new Uint8Array(await response.arrayBuffer());
    if (bytes.length !== to - from)
      throw new Error(
        `${this.url} sent ${bytes.length} bytes for a range of ${to - from}`,
      );
    return bytes;
  }

  slice(start = 0, end = this.size, type = "") {
    const at = (value) =>
      value < 0 ? Math.max(0, this.size + value) : Math.min(value, this.size);
    const from = at(start);
    const to = Math.max(from, at(end));
    return new RangeFile({
      ...this,
      size: to - from,
      type,
      offset: this.offset + from,
    });
  }

  async arrayBuffer() {
    return (await this.read(0, this.size)).buffer;
  }

  bytes() {
    return this.read(0, this.size);
  }

  async text() {
    return new TextDecoder().decode(await this.read(0, this.size));
  }

  stream() {
    if (this.size === 0) return new Blob([]).stream();
    let body = null;
    return new ReadableStream({
      pull: async (controller) => {
        body ??= (await this.#fetch(0, this.size)).body.getReader();
        const { done, value } = await body.read();
        if (done) controller.close();
        else controller.enqueue(value);
      },
      cancel: (reason) => body?.cancel(reason),
    });
  }

  async blob() {
    return new Response(this.stream())
      .blob()
      .then((blob) => new Blob([blob], { type: this.type }));
  }

  async #fetch(from, to) {
    const first = this.offset + from;
    const last = this.offset + to - 1;
    const response = await fetch(this.url, {
      headers: { range: `bytes=${first}-${last}` },
    });
    const range = /^bytes (\d+)-(\d+)\//.exec(
      response.headers.get("content-range") ?? "",
    );
    if (
      response.status !== 206 ||
      Number(range?.[1]) !== first ||
      Number(range?.[2]) !== last
    ) {
      await response.body?.cancel().catch(() => {});
      throw new Error(
        `${this.url} answered bytes ${first}-${last} with ${response.status} ${response.headers.get("content-range") ?? ""}`.trim(),
      );
    }
    return response;
  }
}

export function isRangeFile(value) {
  return (
    typeof value === "object" && value !== null && value.format === RANGE_FILE
  );
}

export function reviveRangeFile(value) {
  return isRangeFile(value) && !(value instanceof RangeFile)
    ? new RangeFile(value)
    : value;
}

export function reviveAssetFiles(asset) {
  if (!isRangeFile(asset.file) && !asset.frameFiles) return asset;
  return {
    ...asset,
    ...(asset.file ? { file: reviveRangeFile(asset.file) } : {}),
    ...(asset.frameFiles
      ? {
          frameFiles: new Map(
            [...asset.frameFiles].map(([locator, file]) => [
              locator,
              reviveRangeFile(file),
            ]),
          ),
        }
      : {}),
  };
}

export function mediaSource(file) {
  const range = reviveRangeFile(file);
  return range instanceof RangeFile
    ? new CustomSource({
        getSize: () => range.size,
        read: (start, end) => range.read(start, end),
        prefetchProfile: "network",
      })
    : new BlobSource(file);
}

export function blobOf(file) {
  const range = reviveRangeFile(file);
  return range instanceof RangeFile ? range.blob() : Promise.resolve(file);
}

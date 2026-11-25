import {
  BlobSource,
  BufferSource,
  CustomSource,
  type Source,
} from "mediabunny";
import type { MediabunnyInput } from "./mediabunny.js";
import { isRangeReader } from "./media.js";

export function mediaSource(resource: MediabunnyInput): Source {
  if (resource instanceof Blob) return new BlobSource(resource);
  if (isRangeReader(resource))
    return new CustomSource({
      getSize: () => resource.size,
      read: (start, end) => resource.read(start, end),
      prefetchProfile: "network",
    });
  return new BufferSource(resource);
}

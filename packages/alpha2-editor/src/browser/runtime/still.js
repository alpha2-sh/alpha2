// Reads a still image by the same rules as native decoding, so preview and browser and native
// exports match: upright per Exif `Orientation` (`imageOrientation: "from-image"`), in stored
// sRGB code values with no ICC conversion (the engine treats SDR input as sRGB and native decoding
// has no colour management), and at its own size.

/** The longest side the engine decodes, renders, or composes (`MAX_FRAME_DIMENSION`). */
export const MAX_STILL_DIMENSION = 16_384;
/** The largest area the engine decodes, renders, or composes (`MAX_FRAME_PIXELS`). */
export const MAX_STILL_PIXELS = 67_108_864;

/**
 * Decodes an image upright without colour management, rejecting one larger than the engine takes.
 */
export async function decodeStill(file) {
  const bitmap = await createImageBitmap(file, {
    imageOrientation: "from-image",
    colorSpaceConversion: "none",
  });
  const { width, height } = bitmap;
  if (
    width > MAX_STILL_DIMENSION ||
    height > MAX_STILL_DIMENSION ||
    width * height > MAX_STILL_PIXELS
  ) {
    bitmap.close();
    throw new RangeError(
      `the picture is ${width}x${height}, larger than the ${MAX_STILL_DIMENSION} pixels a side or ${MAX_STILL_PIXELS} pixels alpha2 decodes`,
    );
  }
  return bitmap;
}

/** A decoded still's pixels at its own size, in straight alpha, as a raster frame. */
export function stillRaster(bitmap) {
  const { width, height } = bitmap;
  const canvas = new OffscreenCanvas(width, height);
  const context = canvas.getContext("2d", {
    colorSpace: "srgb",
    willReadFrequently: true,
  });
  context.drawImage(bitmap, 0, 0);
  const { data } = context.getImageData(0, 0, width, height, {
    colorSpace: "srgb",
  });
  return {
    kind: "raster-frame",
    width,
    height,
    pixels: new Uint8Array(data.buffer, data.byteOffset, data.byteLength),
    alpha: "straight",
    physicalTimeMicroseconds: 0,
  };
}

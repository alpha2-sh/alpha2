"""Writes the extension-provider corpus and, for each animation, the frames an
independent decoder (Pillow) composites from it: the reference the providers
must match exactly. Writes into the working directory (`generate.sh extensions`
runs it in tests/corpus/extensions) and needs Pillow 9+ built with WebP
animation support; the files are deterministic for a given Pillow/libwebp."""

import json
from PIL import Image, ImageDraw, ImageSequence

W, H = 48, 32


def frame(background, box, fill):
    image = Image.new("RGBA", (W, H), background)
    ImageDraw.Draw(image).rectangle(box, fill=fill)
    return image


# Four frames: a moving block over a changing background, with a transparent
# frame and uneven durations. The last duration is 0 ms, which a provider must
# normalize.
FRAMES = [
    frame((20, 40, 90, 255), (2, 2, 17, 17), (240, 200, 40, 255)),
    frame((20, 40, 90, 255), (14, 6, 29, 21), (240, 200, 40, 255)),
    frame((0, 0, 0, 0), (26, 10, 41, 25), (200, 60, 60, 255)),
    frame((90, 140, 60, 255), (8, 14, 23, 29), (255, 255, 255, 128)),
]
DURATIONS = [100, 50, 200, 0]


def reference(path, name):
    """Pillow's own composited frames, straight RGBA, and their durations."""
    image = Image.open(path)
    frames, durations = [], []
    for item in ImageSequence.Iterator(image):
        frames.append(item.convert("RGBA").tobytes())
        durations.append(item.info.get("duration", 0))
    with open(f"{name}.ref.rgba", "wb") as out:
        out.write(b"".join(frames))
    return {"width": image.width, "height": image.height, "frames": len(frames), "durations_ms": durations}


manifest = {}

# GIF: palette frames with transparency; disposal keep, restore-background, restore-previous.
gif_frames = [f.convert("RGBA") for f in FRAMES]
gif_frames[0].save(
    "animation.gif",
    save_all=True,
    append_images=gif_frames[1:],
    duration=DURATIONS,
    disposal=[1, 2, 3, 1],
    loop=0,
    transparency=0,
    optimize=False,
)
manifest["animation.gif"] = reference("animation.gif", "animation.gif")

# APNG: blend over and source, dispose none / background / previous.
FRAMES[0].save(
    "animation.png",
    save_all=True,
    append_images=FRAMES[1:],
    duration=DURATIONS,
    disposal=[0, 1, 2, 0],
    blend=[0, 1, 1, 0],
    loop=0,
)
manifest["animation.png"] = reference("animation.png", "animation.png")

# Animated WebP, lossless so the reference is exact.
FRAMES[0].save(
    "animation.webp",
    save_all=True,
    append_images=FRAMES[1:],
    duration=DURATIONS,
    loop=0,
    lossless=True,
    quality=100,
    method=6,
)
manifest["animation.webp"] = reference("animation.webp", "animation.webp")

# Animated WebP, lossy, assembled frame by frame so each frame's rectangle, alpha, blending and
# disposal are known; the reference is libwebp's own compositing of the whole file.
def webp_chunks(data, start, end):
    chunks, at = [], start
    while at + 8 <= end:
        kind, size = data[at:at + 4], int.from_bytes(data[at + 4:at + 8], "little")
        chunks.append((kind, data[at + 8:at + 8 + size]))
        at += 8 + size + (size & 1)
    return chunks


def chunk(kind, payload):
    return kind + len(payload).to_bytes(4, "little") + payload + (b"\0" if len(payload) % 2 else b"")


def lossy_picture(image):
    import io
    out = io.BytesIO()
    image.save(out, "WEBP", lossless=False, quality=90, method=6, exact=True)
    data = out.getvalue()
    return b"".join(chunk(kind, payload) for kind, payload in webp_chunks(data, 12, len(data))
                    if kind in (b"ALPH", b"VP8 "))


def lossy_frame(image, x, y, duration, blend_over, dispose_background):
    width, height = image.size
    header = (x // 2).to_bytes(3, "little") + (y // 2).to_bytes(3, "little")
    header += (width - 1).to_bytes(3, "little") + (height - 1).to_bytes(3, "little")
    header += duration.to_bytes(3, "little")
    header += bytes([(0 if blend_over else 2) | (1 if dispose_background else 0)])
    return chunk(b"ANMF", header + lossy_picture(image))


def patch(size, background, box, fill):
    image = Image.new("RGBA", size, background)
    ImageDraw.Draw(image).rectangle(box, fill=fill)
    return image


lossy_frames = [
    lossy_frame(patch((48, 32), (20, 40, 90, 230), (2, 2, 17, 17), (240, 200, 40, 255)), 0, 0, 100, True, False),
    lossy_frame(patch((24, 16), (200, 60, 60, 160), (4, 4, 11, 11), (255, 255, 255, 255)), 8, 8, 100, True, True),
    lossy_frame(patch((16, 16), (0, 0, 0, 0), (2, 2, 13, 13), (60, 200, 90, 200)), 24, 10, 100, True, False),
    lossy_frame(patch((20, 12), (250, 230, 30, 128), (0, 0, 0, 0), (250, 230, 30, 128)), 2, 2, 100, False, False),
]
vp8x = bytes([0x12, 0, 0, 0]) + (47).to_bytes(3, "little") + (31).to_bytes(3, "little")
body = b"WEBP" + chunk(b"VP8X", vp8x) + chunk(b"ANIM", bytes([0, 0, 0, 0, 0, 0])) + b"".join(lossy_frames)
with open("lossy.webp", "wb") as out:
    out.write(b"RIFF" + len(body).to_bytes(4, "little") + body)
manifest["lossy.webp"] = reference("lossy.webp", "lossy.webp")

# A numbered PNG sequence: frames 7 to 10.
for index, image in enumerate(FRAMES):
    image.save(f"sequence_{index + 7:04d}.png")

with open("MANIFEST.json", "w") as out:
    json.dump(manifest, out, indent=2, sort_keys=True)
    out.write("\n")

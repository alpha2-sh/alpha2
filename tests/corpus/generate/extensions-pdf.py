"""Writes the PDF fixtures of the extension-provider corpus into the working directory.

Standard library only, and deterministic: every file is ASCII but the encrypted ones, which use
the standard security handler (revision 2, RC4 40-bit) with a fixed document ID.
"""

import hashlib

# The padding string of PDF 32000-1:2008 7.6.3.3, Algorithm 2.
PAD = bytes.fromhex("28bf4e5e4e758a4164004e56fffa01082e2e00b6d0683e802f0ca9fe6453697a")
ID = bytes.fromhex("00112233445566778899aabbccddeeff")


def rc4(key, data):
    box = list(range(256))
    j = 0
    for i in range(256):
        j = (j + box[i] + key[i % len(key)]) % 256
        box[i], box[j] = box[j], box[i]
    out, i, j = bytearray(), 0, 0
    for byte in data:
        i = (i + 1) % 256
        j = (j + box[i]) % 256
        box[i], box[j] = box[j], box[i]
        out.append(byte ^ box[(box[i] + box[j]) % 256])
    return bytes(out)


def padded(password):
    return (password + PAD)[:32]


def security(user, owner, permissions=-4):
    """The /Encrypt dictionary and the file key for revision 2."""
    owner_key = hashlib.md5(padded(owner or user)).digest()[:5]
    o = rc4(owner_key, padded(user))
    key = hashlib.md5(padded(user) + o + permissions.to_bytes(4, "little", signed=True) + ID).digest()[:5]
    u = rc4(key, PAD)
    hexed = lambda value: b"<" + value.hex().encode() + b">"
    entry = b"<< /Filter /Standard /V 1 /R 2 /O " + hexed(o) + b" /U " + hexed(u) + b" /P " + str(permissions).encode() + b" >>"
    return entry, key


def object_key(key, number):
    return hashlib.md5(key + number.to_bytes(3, "little") + (0).to_bytes(2, "little")).digest()[:10]


def document(objects, key=None, encrypt=None):
    """`objects` are (dictionary, stream or None), numbered from 1; object 1 is the catalog."""
    out = bytearray(b"%PDF-1.4\n")
    offsets = []
    for number, (body, data) in enumerate(objects, 1):
        offsets.append(len(out))
        out += b"%d 0 obj\n" % number
        if data is None:
            out += body + b"\nendobj\n"
            continue
        if key is not None:
            data = rc4(object_key(key, number), data)
        out += body[:-2].rstrip() + b" /Length %d >>\nstream\n" % len(data) + data + b"\nendstream\nendobj\n"
    count = len(objects) + 1
    if encrypt is not None:
        offsets.append(len(out))
        out += b"%d 0 obj\n" % count + encrypt + b"\nendobj\n"
        count += 1
    xref = len(out)
    out += b"xref\n0 %d\n0000000000 65535 f \n" % count
    for offset in offsets:
        out += b"%010d 00000 n \n" % offset
    trailer = b"<< /Size %d /Root 1 0 R" % count
    if encrypt is not None:
        trailer += b" /Encrypt %d 0 R /ID [<%s> <%s>]" % (count - 1, ID.hex().encode(), ID.hex().encode())
    out += b"trailer\n" + trailer + b" >>\nstartxref\n%d\n%%%%EOF\n" % xref
    return bytes(out)


def pages(kids, extra=()):
    """A catalog, a page tree of `kids` (page dictionaries without /Parent), and `extra` objects."""
    first = 3
    objects = [(b"<< /Type /Catalog /Pages 2 0 R >>", None)]
    numbers = []
    contents = []
    for index, (page, content) in enumerate(kids):
        numbers.append(first + 2 * index)
        contents.append((page, content))
    objects.append((b"<< /Type /Pages /Kids [" + b" ".join(b"%d 0 R" % n for n in numbers) + b"] /Count %d >>" % len(kids), None))
    for number, (page, content) in zip(numbers, contents):
        objects.append((page[:-2].rstrip() + b" /Parent 2 0 R /Contents %d 0 R >>" % (number + 1), None))
        objects.append((b"<< >>", content))
    return objects + list(extra)


HELVETICA = b"<< /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >>"

# Two pages: A4 with a red left half and Helvetica text, then Letter turned 90 degrees with a blue
# square in the corner its content puts at the origin.
PAGES = pages(
    [
        (b"<< /Type /Page /MediaBox [0 0 595 842] /Resources " + HELVETICA + b" >>",
         b"1 0 0 rg 0 0 297.5 842 re f 0 0 0 rg BT /F1 48 Tf 320 400 Td (Ag) Tj ET"),
        (b"<< /Type /Page /MediaBox [0 0 612 792] /Rotate 90 >>", b"0 0 1 rg 0 0 100 100 re f"),
    ]
)

# Gradient, constant alpha and Multiply blending, drawn to a 192-point square.
SHADING = pages(
    [
        (b"<< /Type /Page /MediaBox [0 0 192 192] /Resources << "
         b"/Shading << /Sh1 << /ShadingType 2 /ColorSpace /DeviceRGB /Coords [0 0 192 192] /Extend [true true] "
         b"/Function << /FunctionType 2 /Domain [0 1] /C0 [0.1 0.2 0.6] /C1 [0.95 0.8 0.2] /N 1 >> >> >> "
         b"/ExtGState << /Half << /ca 0.5 /BM /Multiply >> >> >> >>",
         b"/Sh1 sh /Half gs 0.9 0.1 0.3 rg 40 40 112 112 re f 0.2 0.8 0.4 rg 96 20 76 152 re f"),
    ]
)

# A 4x2 RGB image, ASCII hex coded, scaled to the 96-point page without interpolation.
IMAGE_SAMPLES = "ff0000 00ff00 0000ff ffffff 000000 ffff00 00ffff ff00ff".replace(" ", "")
IMAGE = pages(
    [
        (b"<< /Type /Page /MediaBox [0 0 96 48] /Resources << /XObject << /Im1 5 0 R >> >> >>",
         b"q 96 0 0 48 0 0 cm /Im1 Do Q"),
    ],
    extra=[(b"<< /Type /XObject /Subtype /Image /Width 4 /Height 2 /ColorSpace /DeviceRGB "
            b"/BitsPerComponent 8 /Filter /ASCIIHexDecode >>", IMAGE_SAMPLES.encode() + b">")],
)

# One page with a red square, encrypted with an empty user password and with one.
SECRET = [(b"<< /Type /Page /MediaBox [0 0 64 64] >>", b"1 0 0 rg 16 16 32 32 re f")]

for name, objects in [("pages.pdf", PAGES), ("shading.pdf", SHADING), ("image.pdf", IMAGE)]:
    with open(name, "wb") as out:
        out.write(document(objects))
for name, user in [("owner-password.pdf", b""), ("user-password.pdf", b"secret")]:
    encrypt, key = security(user, b"owner")
    with open(name, "wb") as out:
        out.write(document(pages(SECRET), key, encrypt))

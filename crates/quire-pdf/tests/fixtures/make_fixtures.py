#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 The Quire contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Writes the small hand-made PDF fixtures and their gold text.

Run from this folder: python3 make_fixtures.py. `cid.pdf` is made by
Quire's own PDF export (Typst, CID fonts): quire-cli pdf <vault> cid.md.
"""

def pdf(objects, path):
    """objects: list of bytes (object bodies, numbered from 1)."""
    out = bytearray(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n")
    offsets = []
    for i, body in enumerate(objects, 1):
        offsets.append(len(out))
        out += b"%d 0 obj\n" % i + body + b"\nendobj\n"
    xref = len(out)
    out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(objects) + 1)
    for o in offsets:
        out += b"%010d 00000 n \n" % o
    out += b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (len(objects) + 1, xref)
    open(path, "wb").write(out)

def stream(content, extra=b""):
    return b"<< /Length %d %s>>\nstream\n" % (len(content), extra) + content + b"\nendstream"

def esc(s):
    return s.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)").encode("latin-1")

FONT = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"

def text_ops(lines, x, y, size=12, lead=16):
    ops = b"BT /F1 %d Tf %d %d Td\n" % (size, x, y)
    for i, l in enumerate(lines):
        if i:
            ops += b"0 -%d Td\n" % lead
        if isinstance(l, list):  # TJ with gaps instead of space characters
            parts = b"".join((b"(%s)" % esc(p)) if isinstance(p, str) else (b"%d" % p) for p in l)
            ops += b"[" + parts + b"] TJ\n"
        else:
            ops += b"(" + esc(l) + b") Tj\n"
    return ops + b"ET\n"

def gold_of(lines):
    return "\n".join(" ".join(p for p in l if isinstance(p, str)) if isinstance(l, list) else l for l in lines)

# ---- text.pdf: two pages, a web link and a link to page 2.
P1 = [
    "Ropes are balanced trees of strings.",
    "Edits touch only the path from the root to one leaf,",
    "so undo is cheap because subtrees are shared.",
    ["Words", -450, "spaced", -450, "by", -450, "gaps."],
    "See page two for the details.",
]
P2 = [
    "Page two: piece tables keep the original file intact.",
    "Ropes are balanced trees of strings, again.",
]
def text_pdf(path, p1, p2):
    c1 = text_ops(p1, 72, 720)
    c2 = text_ops(p2, 72, 720)
    objs = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 6 0 R /Annots [8 0 R 9 0 R] >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 7 0 R >>",
        FONT,
        stream(c1),
        stream(c2),
        b"<< /Type /Annot /Subtype /Link /Rect [72 650 240 664] /Border [0 0 0] /A << /S /URI /URI (https://en.wikipedia.org/wiki/Rope_\\(data_structure\\)) >> >>",
        b"<< /Type /Annot /Subtype /Link /Rect [72 652 300 664] /Border [0 0 0] /Dest [4 0 R /XYZ 0 792 0] >>",
    ]
    pdf(objs, path)
text_pdf("text.pdf", P1, P2)
open("text.txt", "w").write(gold_of(P1) + "\n\f\n" + gold_of(P2) + "\n")

# ---- text-v2.pdf: the same document re-saved with a new first line and
# the highlighted sentence moved down (for re-anchoring).
text_pdf("text-v2.pdf", ["Revised edition."] + P1, P2)

# ---- two-column.pdf: the left column's lines, then the right column's.
LEFT = [
    "In the left column, text",
    "flows down first. Readers",
    "finish this column before",
    "moving to the right one.",
]
RIGHT = [
    "The right column holds",
    "the rest of the article",
    "and ends the page here.",
    "Reading order matters.",
]
c = text_ops(LEFT, 56, 720, 11, 14) + text_ops(RIGHT, 320, 720, 11, 14)
pdf([
    b"<< /Type /Catalog /Pages 2 0 R >>",
    b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
    b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>",
    FONT,
    stream(c),
], "two-column.pdf")
open("two-column.txt", "w").write(gold_of(LEFT) + "\n" + gold_of(RIGHT) + "\n")

# ---- scanned.pdf: one page that is only a picture (no text).
W = H = 16
img = bytes((x * 16 + y) % 256 for y in range(H) for x in range(W))
pdf([
    b"<< /Type /Catalog /Pages 2 0 R >>",
    b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
    b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /XObject << /Im1 4 0 R >> >> /Contents 5 0 R >>",
    stream(img, b"/Type /XObject /Subtype /Image /Width 16 /Height 16 /ColorSpace /DeviceGray /BitsPerComponent 8 "),
    stream(b"q 560 0 0 740 26 26 cm /Im1 Do Q\n"),
], "scanned.pdf")

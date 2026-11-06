import json, struct, sys
src, dst = sys.argv[1], sys.argv[2]
d = json.load(open(src))
rings = []
for f in d["features"]:
    g = f["geometry"]
    polys = [g["coordinates"]] if g["type"] == "Polygon" else g["coordinates"]
    for p in polys:
        for ring in p:
            pts = [(round(x * 100), round(y * 100)) for x, y in ring]
            out = []
            for q in pts:
                if not out or out[-1] != q:
                    out.append(q)
            if len(out) >= 3:
                rings.append(out)
# Format: b"QNE1", u32 ring count, then per ring u32 point count, points as i16 lon*100, i16 lat*100 (LE).
b = bytearray(b"QNE1") + struct.pack("<I", len(rings))
n = 0
for r in rings:
    b += struct.pack("<I", len(r))
    for x, y in r:
        b += struct.pack("<hh", x, y); n += 1
open(dst, "wb").write(b)
print(len(rings), n, len(b))

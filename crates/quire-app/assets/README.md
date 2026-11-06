# Bundled data

`ne_110m_land.bin` is the land outline of the map view (Bases `type: map`
and `layout: map` in quire-views). It is Natural Earth's 1:110m land
polygons (`ne_110m_land`, from github.com/nvkelso/natural-earth-vector),
which is in the public domain: "No permission is needed to use Natural
Earth. Crediting the authors is unnecessary." Made with Natural Earth.

Format (little-endian): `QNE1`, a `u32` ring count, then per ring a `u32`
point count and the points as `i16` longitude × 100, `i16` latitude × 100.
128 rings, 5,129 points, 21 KB. `ne2bin.py` makes it from the GeoJSON:
`python3 ne2bin.py ne_110m_land.geojson ne_110m_land.bin`.

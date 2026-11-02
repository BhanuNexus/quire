# Canvas: `*.canvas` and ink SVGs

Owner: WS-G. Types: `quire_canvas`.

## `.canvas` (JSON Canvas 1.0)

Any JSON Canvas reader can open Quire's files. Quire writes one node or edge
per line, so line-based diff, merge, sync and proposals work per node:

```json
{"nodes":[
{"id":"a1","type":"text","x":0,"y":0,"width":240,"height":120,"text":"# Ropes"},
{"id":"f2","type":"file","x":300,"y":0,"width":400,"height":400,"file":"Rope Notes.md","subpath":"#Undo"}
],"edges":[
{"id":"e1","fromNode":"a1","fromSide":"right","toNode":"f2","toSide":"left","toEnd":"arrow"}
]}
```

* Nodes are in z-order. Types `text`, `file`, `link`, `group`; other types
  are kept. `x`, `y`, `width`, `height` are integers. Colours are `"1"`–`"6"`
  or `#RRGGBB`.
* Keys Quire doesn't know, on any object and at the top level, are kept and
  written back (top-level ones after `edges`).
* `model::to_canonical` writes this form; it is stable (writing what it read
  gives the same text).

## Ink: `attachments/<canvas>-ink-<n>.svg`

Each stroke is a `<path d=… fill=…>` outline any viewer draws, with
`data-quire-points="x,y,p;…"` (two decimals, pressure 0–1, optional when
reading) and `data-quire-tool="pen|marker|highlighter"`. Shapes are SVG
elements with `data-quire-shape`. Shown on a canvas through a `file` node.

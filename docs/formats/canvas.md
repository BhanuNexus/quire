# Canvas: `*.canvas` and ink SVGs

Owner: WS-G. Types: `quire_canvas` (`model`, `ops`, `ink`).

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

* Nodes are in z-order (the first is at the back). Types `text`, `file`,
  `link`, `group`; other types are kept and shown as "kept as it is".
* `x`, `y`, `width`, `height` are integers. A file with fractions still
  opens (they are rounded when Quire next writes it).
* Colours are `"1"`–`"6"` (red, orange, yellow, green, cyan, purple) or
  `#RRGGBB`.
* Edges: `fromSide`/`toSide` are `top`, `right`, `bottom`, `left` (absent:
  the facing sides); `fromEnd` defaults to `none` and `toEnd` to `arrow`.
* Keys Quire doesn't know, on any object and at the top level, are kept and
  written back (top-level ones after `edges`).
* `model::to_canonical` writes this form; it is stable (writing what it read
  gives the same text). Quire writes a file only after a change made in
  Quire, so an Obsidian file that is only opened stays byte for byte.
* A file Quire can't parse is shown with the reason and never written.
* A change made elsewhere while the canvas is open is taken in; if there
  are unsaved edits too, the two are merged line by line (node by node).
  When both changed the same node, Quire keeps its version and writes the
  other to `<stem> (changed elsewhere).canvas`.

### Agent operations (`canvas_apply`)

`quire_canvas::ops::apply` takes a list of operations, applied in order and
all or nothing:

| Op | Fields |
|---|---|
| `add_node` | `node`: `type` (`text`, `file`, `link`, `group`) and its content (`text`, `file`, `url`, `label`), `x`, `y` (default 0), `width`, `height` (defaults by type), `color`, `id` (optional; one is made) |
| `update_node` | `id`, `set`: keys to set; `null` removes a key; `id` and `type` can't change |
| `remove_node` | `id` (its edges go too) |
| `add_edge` | `edge`: `fromNode`, `toNode`, `fromSide`, `toSide`, `fromEnd`, `toEnd`, `color`, `label`, `id` (optional) |
| `update_edge` | `id`, `set` |
| `remove_edge` | `id` |

The result is written in the canonical form through `AgentStore::submit`,
so outside agent zones it is a proposal whose hunks are whole nodes.
`ops::diff` describes two versions node by node ("Add text node “X”").

## Ink: `attachments/<canvas>-ink-<n>.svg`

A sketch is an SVG file shown on a canvas through a `file` node (or embedded
in a note with `![[…svg]]` by "Insert sketch"). Any SVG viewer draws it.

```xml
<svg xmlns="http://www.w3.org/2000/svg" width="420" height="180" viewBox="0 0 420 180" data-quire-sketch="1">
<path d="M10.00 12.00 Q…Z" fill="#1e1e1e" data-quire-tool="pen" data-quire-size="3" data-quire-points="12.00,30.00,0.50;…"/>
<path d="…" fill="#ffd43b" fill-opacity="0.35" data-quire-tool="highlighter" data-quire-size="20" data-quire-points="…"/>
<rect x="…" y="…" width="…" height="…" fill="none" stroke="#e03131" stroke-width="2.5" … data-quire-shape="rect" data-quire-coords="x0,y0,x1,y1"/>
</svg>
```

* **Strokes** are filled `<path>` outlines (perfect-freehand style: width
  follows pressure, round ends) with the input points in
  `data-quire-points="x,y,p;…"` (two decimals; pressure 0–1, optional when
  reading, default 0.5), the tool in `data-quire-tool` (`pen`, `marker`,
  `highlighter`) and the width in `data-quire-size`. Quire draws strokes
  from the points (smoothed with Catmull–Rom); the outline is for other
  apps. Points are simplified with Ramer–Douglas–Peucker (half a screen
  pixel) when a stroke ends.
* **Shapes** (`rect`, `ellipse`, `line`, `arrow`) are plain SVG elements with
  `data-quire-shape` and their two corners or ends in `data-quire-coords`.
* Colours are `#rrggbb`. The default ink `#1e1e1e` is drawn light on a dark
  theme.
* **Elements another app wrote** are kept verbatim where they are; Quire
  doesn't draw them and says so.
* On a canvas the sketch's card hugs what is drawn: after each stroke the
  drawing is moved to start 12 units from its corner and the card is moved
  and sized to match (the SVG's `width`/`height`/`viewBox` follow).

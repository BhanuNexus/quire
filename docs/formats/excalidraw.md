# Excalidraw drawings: `.excalidraw` and `.excalidraw.md`

Quire (0.10) shows Excalidraw drawings as pictures, and (0.13) edits
them. It reads and writes two forms, and keeps each in its own form.
Design: [../design/0.13-excalidraw.md](../design/0.13-excalidraw.md).
Owner: `quire_canvas::drawing` (the file), `quire_canvas::excalidraw`
(the picture), `quire_app::drawings` (saving).

Sources: Excalidraw's own element types and `newElement`/`mutateElement`
(excalidraw 0.18), and the Obsidian Excalidraw plugin 2.x
(`ExcalidrawData.ts`, `excalidrawMarkdownParsing.ts`, `sceneDataUtils.ts`).

## `.excalidraw`: the scene as JSON

What Excalidraw's "Save to file" writes: `JSON.stringify(scene, null, 2)`.

```json
{
  "type": "excalidraw",
  "version": 2,
  "source": "https://excalidraw.com",
  "elements": [ … ],
  "appState": { "gridSize": 20, "viewBackgroundColor": "#ffffff", … },
  "files": { "<fileId>": { "mimeType": "image/png", "id": "<fileId>", "dataURL": "data:image/png;base64,…", "created": 1731400000000, "lastRetrieved": 1731400000000 } }
}
```

* `elements` are drawn in array order (later on top).
* `files` holds the pictures that image elements show, by `fileId`, as
  `data:` URLs.
* `appState` is the view and the current style; Quire reads
  `viewBackgroundColor` and keeps the rest as it is.

### Elements

Every element has:

| Field | Meaning |
|---|---|
| `id` | unique; Excalidraw makes 21 characters (nanoid) |
| `type` | `rectangle`, `ellipse`, `diamond`, `line`, `arrow`, `freedraw`, `text`, `image`, `frame`, `magicframe`, `embeddable`, `iframe`… |
| `x`, `y`, `width`, `height` | the box, in scene units, before rotation |
| `angle` | radians, clockwise, around the box's centre |
| `strokeColor`, `backgroundColor` | `#rrggbb` (or `#rgb`, `#rrggbbaa`) or `transparent` |
| `fillStyle` | `hachure`, `cross-hatch`, `solid`, `zigzag` |
| `strokeWidth` | 1 thin, 2 bold, 4 extra bold |
| `strokeStyle` | `solid`, `dashed`, `dotted` |
| `roughness` | 0 architect, 1 artist, 2 cartoonist (the hand-drawn wobble) |
| `opacity` | 0–100 |
| `groupIds` | groups, innermost first; clicking selects the outermost |
| `frameId` | the frame it belongs to, or `null` |
| `index` | fractional index (Excalidraw 0.17 on): base-62 keys that sort in array order (`a0`, `a1`, `a0V`…) |
| `roundness` | `null` (sharp) or `{"type": 2}` (proportional: lines, arrows, diamonds) / `{"type": 3}` (adaptive: rectangles) |
| `seed` | random integer (< 2³¹) for the hand-drawn look; stays the same when the element is edited |
| `version` | +1 on every edit |
| `versionNonce` | a new random integer on every edit |
| `updated` | time of the last edit, Unix ms |
| `isDeleted` | `true`: deleted (kept for collaboration and undo) |
| `boundElements` | `null` or `[{"type": "text" \| "arrow", "id": …}]`: text inside it, arrows bound to it |
| `link` | `null`, a web address, or (Obsidian) `[[Note]]` |
| `locked` | can't be selected or moved |
| `customData` | anything a host app keeps |

By type:

* **text**: `text` (as shown, with the line breaks wrapping added),
  `originalText` (without them), `fontSize`, `fontFamily` (1 Virgil, 2
  Helvetica, 3 Cascadia, 5 Excalifont, 6 Nunito, 7 Lilita One, 8 Comic
  Shanns), `textAlign` (`left`/`center`/`right`), `verticalAlign`
  (`top`/`middle`/`bottom`), `containerId` (the shape it's inside, or
  `null`), `lineHeight` (1.25), `autoResize`. In Obsidian also
  `rawText`: the text with its Markdown, `[[links]]` included.
* **line**, **arrow**: `points` (`[[x, y], …]` relative to `x`, `y`; the
  first is `[0, 0]`), `startBinding`/`endBinding` (`null` or
  `{"elementId", "focus", "gap"}`: the shape an end is attached to),
  `startArrowhead`/`endArrowhead` (`null`, `arrow`, `triangle`, `bar`,
  `dot`, `circle`…), `lastCommittedPoint`; arrows `elbowed`.
* **freedraw**: `points`, `pressures`, `simulatePressure`.
* **image**: `fileId`, `status`, `scale`, `crop`.
* **frame**: `name`.

## `.excalidraw.md`: the Obsidian plugin's drawing note

A Markdown note. The plugin recognises it by `excalidraw-plugin: parsed`
in its front matter.

````markdown
---

excalidraw-plugin: parsed
tags: [excalidraw]

---
==⚠  Switch to EXCALIDRAW VIEW in the MORE OPTIONS menu of this document. ⚠== …

(the note's own text, if any)

# Excalidraw Data

## Text Elements
Plan for [[Roadmap]] ^Kq3sXz1a

no links here ^pTq8nWm2

## Element Links
Kq3sXz1a: [[Roadmap]]

rectZZZZZZZZZZZZZZZZZ: [[Meeting notes#Decisions]]

## Embedded Files
3f2a9c: [[attachments/dot.png]]

%%
## Drawing
```compressed-json
N4KAkARALgngDgUwgLgAQQQDwMYEMA2AlgCYBOuA7hADTgQBuCpAzoQPYB2KqATLZMzYBXUtiRoIACyhQ4zZA…

…
```
%%
````

* **`# Excalidraw Data`** starts the plugin's part. Before it, the note's
  own text (front matter, a warning line, anything written there).
* **`## Text Elements`**: each text element as `<rawText> ^<id>` and a
  blank line. The plugin makes text ids 8 characters long, so the `^id`
  is a block reference. This section is what Obsidian (and Quire) index:
  `[[links]]` here are links of the note, with backlinks.
* **`## Element Links`**: `<id>: <link>` and a blank line, for each
  element with a link (text elements' links too).
* **`## Embedded Files`**: `<fileId>: [[path]]` (a picture in the vault;
  a colour map may follow), `<fileId>: https://…`, or `<fileId>: $$latex$$`.
* **`## Drawing`** (or `# Drawing`): a fenced block holding the scene
  (the same JSON as `.excalidraw`, written with a tab indent):
  * ```` ```json ````: the JSON as it is;
  * ```` ```compressed-json ````: LZ-string `compressToBase64` of the
    JSON (lz-string 1.5.0), in lines of 256 characters with a blank line
    between them. Line breaks are ignored when reading.
* `%%` lines make the data a comment in Obsidian's reading view. With
  the plugin's "text elements commented out" option, `%%` comes before
  `# Excalidraw Data` and there is none before `## Drawing`.
* In a note, the scene's `files` is usually `{}`: pictures are vault files
  listed under `## Embedded Files`.

## What Quire writes

* **Unchanged drawing**: nothing is written. Open, then save without an
  edit, gives the same bytes (tested on every fixture).
* **Fidelity**: elements are kept as ordered JSON: every field stays,
  in its place, unknown ones and unknown element types included. The JSON
  is written as `JSON.stringify` writes it (same indent, escapes and
  number forms; numbers not edited keep their text), so only edited
  elements' lines change.
* **The form is kept**: a `.excalidraw` stays JSON with its indent; a note
  stays compressed or plain. A note's text before `# Excalidraw Data`,
  its sections and the `%%` lines are kept. `## Text Elements` and
  `## Element Links` are written again only when a text or a link
  changed; `## Embedded Files` only gets new lines.
* **Edits, as Excalidraw makes them**: each changed element gets
  `version + 1`, a new `versionNonce` and `updated` = now; its `seed`
  stays. A new element gets the fields `newElement` writes, a random
  `seed`, `version` 1, and an `index` after the one below it (only when
  the drawing has indices). Moving elements in the stacking order gives
  them new `index` keys between their neighbours (as `syncMovedIndices`).
  A deleted element is kept with `isDeleted: true`.
* **Text**: `text` and `originalText` are the text as shown (`[[Note|alias]]`
  shows `alias`); in a note, `rawText` keeps the links, and the first
  `[[link]]` becomes the element's `link`. Width and height are measured
  with Quire's fonts.
* **Arrows** bound to shapes: `startBinding`/`endBinding` with `focus` 0
  and `gap` 4, and the arrow in the shape's `boundElements`. Moving the
  shape moves the arrow's end to the shape's outline.
* **Pictures** added in Quire: in a note, a vault link under
  `## Embedded Files`; in a `.excalidraw` file, a `data:` URL in `files`
  (40 hex characters as the id). Both as Excalidraw and the plugin do.
* **Clipboard**: Excalidraw's `{"type": "excalidraw/clipboard", "elements",
  "files"}`; pasting it from Excalidraw works too.
* New drawing notes have the plugin's front matter and a compressed
  block, named `Drawing YYYY-MM-DD HH.MM.SS.excalidraw.md` as the plugin
  names them. Embeds are written `![[Drawing ….excalidraw]]` (without
  `.md`), as the plugin writes them.

## Limits

A drawing over 32 MiB of text, 50,000 elements, 1,000,000 points, 1,000
pictures, or one that unpacks to more than 16 Mi UTF-16 units, is shown
as "too large" and not edited (0.10.1 security review #7).

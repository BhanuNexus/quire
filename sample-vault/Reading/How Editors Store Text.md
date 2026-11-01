# How Editors Store Text

Notes from reading about text buffers in editors.

Most modern editors pick one of three data structures: a gap buffer, a piece table, or a rope. The rope wins when documents are large and edits are scattered, because each edit rebuilds only a few tree nodes.

Undo is easier with persistent structures: keep the old root and you keep the old document.

Related: [[Rope Notes]] · #reading/cs #editors

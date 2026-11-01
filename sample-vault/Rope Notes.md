# Rope Notes

A **rope** is a balanced tree whose leaves hold chunks of text. Editors such as Zed, Helix and Xi use ropes because inserting or deleting text is `O(log n)` instead of `O(n)`.

## Why ropes beat strings
- Edits touch only the path from the root to one leaf
- Cloning is cheap: unchanged subtrees are shared, which makes undo snapshots nearly free
- Line indexes live in each node, so jumping to line 50,000 is instant

## Alternatives
- **Gap buffer** — what Emacs uses; great for local edits, slow for multiple cursors
- **Piece table** — VS Code and old Word; edits append to a buffer and the table points into it

Quire stores every document in a rope (`ropey`) and expresses edits as invertible transactions. See [[Quire Roadmap#Architecture]].

```rust
let mut rope = Rope::from_str("hello world");
rope.insert(5, ",");
```

#cs/data-structures #editors

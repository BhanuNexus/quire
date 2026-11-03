# Ropes for text editors

By Ada Lovelace

A rope is a balanced tree of string pieces. Editors use ropes because inserting or deleting text in the middle of a large document only touches a few nodes, instead of moving every byte after the edit point. This keeps typing fast even in files of many megabytes.

Undo becomes cheap as well: because subtrees are shared between versions, keeping an old version of the document costs only the nodes that changed. See the [original paper](http://SITE/papers/ropes.pdf) and [a longer explanation](https://example.org/ropes).

## How a rope splits

Splitting at an offset walks down from the root, choosing the left or right child by comparing the offset with the left subtree's weight.

```
fn split(node, at) {
    if at < node.weight { split(node.left, at) } else { split(node.right, at - node.weight) }
}
```

![[rope.png|A rope tree]]

A small rope.

The same picture again, from a CDN: ![[rope.png|Copy]]

A diagram: ![Diagram](http://SITE/blog/diagram.svg) and an inline dot: ![[image.png|dot]]

## Costs

| Operation | Rope     | Array |
| --------- | -------- | ----- |
| Insert    | O(log n) | O(n)  |
| Index     | O(log n) | O(1)  |

Ropes trade a little indexing speed for much faster edits, which is the right trade for an editor where most operations are local edits near the cursor.

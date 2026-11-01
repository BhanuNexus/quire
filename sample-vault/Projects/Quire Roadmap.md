---
title: Quire Roadmap
tags: [quire, project]
status: active
due: 2026-12-15
owner: Asha
---
# Quire Roadmap

## Architecture
Layers: text → markdown → vault → index → intel → app → UI. The UI is an adapter, so a GPUI frontend can replace egui later. Background: [[Rope Notes]].

## v1
- [x] Rope buffer with undo tree
- [x] Live Markdown styling
- [x] Wikilinks, backlinks, rename refactor
- [x] Full-text search (BM25) and fuzzy quick open
- [ ] Virtualized editor widget for 100 MB files
- [ ] Graph view

## v2 ideas
- [ ] WASM plugins
- [ ] Local embeddings for semantic search
- [ ] CRDT sync

Budget for the year:
designers = 2
designers * 85000 =
